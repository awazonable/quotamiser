//! LAN access is checked before any body extraction or provider operation.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use hyper::body::{Body as HttpBody, Frame, SizeHint};
use hyper_util::rt::{TokioIo, TokioTimer};
use ipnet::IpNet;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tower::ServiceExt;

use crate::config::{ConfigError, Server};
use crate::server::MAX_BODY_BYTES;

const LIMIT: usize = 32;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Deliberately has no Debug implementation: the token must never be logged.
pub struct Access {
    token: Vec<u8>,
    cidrs: Vec<IpNet>,
    requests: Arc<Semaphore>,
    receive_timeout: Duration,
}

impl Access {
    pub(crate) fn matches_token(&self, token: &[u8]) -> bool {
        bool::from(self.token.ct_eq(token))
    }

    fn allows(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        ip.is_loopback() || self.cidrs.iter().any(|cidr| cidr.contains(&ip))
    }
}

fn private(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => ip.is_private(),
        IpAddr::V6(ip) => ip.is_unique_local(),
    }
}

pub(crate) fn resolve(
    server: &Server,
    bind: SocketAddr,
) -> Result<Option<Arc<Access>>, ConfigError> {
    if !server.allow_lan {
        if server.auth_token_env.is_some() || !server.allowed_client_cidrs.is_empty() {
            return Err(ConfigError::Lan("LAN controls require allow_lan = true"));
        }
        return Ok(None);
    }
    let ip = bind.ip().to_canonical();
    if !(ip.is_loopback() || ip.is_unspecified() || private(ip)) {
        return Err(ConfigError::Lan(
            "bind must be private, loopback, or wildcard",
        ));
    }
    let variable = server
        .auth_token_env
        .as_ref()
        .ok_or(ConfigError::Lan("auth_token_env is required"))?;
    let token =
        std::env::var(variable).map_err(|_| ConfigError::MissingCredential(variable.clone()))?;
    build_access(server, token).map(Some)
}

pub(crate) fn build_access(server: &Server, token: String) -> Result<Arc<Access>, ConfigError> {
    if token.len() < 64
        || !token.len().is_multiple_of(2)
        || !token.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(ConfigError::Lan(
            "token must be an even number of hexadecimal digits, at least 64",
        ));
    }
    if server.allowed_client_cidrs.is_empty() {
        return Err(ConfigError::Lan("allowed_client_cidrs must not be empty"));
    }
    let mut cidrs = Vec::new();
    for value in &server.allowed_client_cidrs {
        let cidr: IpNet = value
            .parse()
            .map_err(|_| ConfigError::Lan("invalid client CIDR"))?;
        // Checking both extremes rejects ranges spanning private and public addresses.
        if !private(cidr.network()) || !private(cidr.broadcast()) {
            return Err(ConfigError::Lan(
                "client CIDRs must be contained within private IPv4 or IPv6 ULA",
            ));
        }
        // Mapped IPv6 CIDRs would otherwise have different matching semantics.
        if matches!(cidr, IpNet::V6(net) if net.network().to_ipv4_mapped().is_some()) {
            return Err(ConfigError::Lan("use IPv4 CIDRs for IPv4-mapped addresses"));
        }
        cidrs.push(cidr);
    }
    Ok(Arc::new(Access {
        token: token.into_bytes(),
        cidrs,
        requests: Arc::new(Semaphore::new(LIMIT)),
        receive_timeout: RECEIVE_TIMEOUT,
    }))
}

pub fn protect(router: Router, access: Arc<Access>) -> Router {
    router.layer(middleware::from_fn_with_state(access, guard))
}

fn refusal(status: StatusCode, kind: &str, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({"error": {
            "type": kind, "message": message, "param": null, "code": null
        }})),
    )
        .into_response()
}

async fn guard(State(access): State<Arc<Access>>, mut request: Request, next: Next) -> Response {
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>();
    if !peer.is_some_and(|peer| access.allows(peer.0.ip())) {
        return refusal(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Client address is not allowed.",
        );
    }
    if request.headers().contains_key(header::ORIGIN) {
        return refusal(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Browser requests are not supported.",
        );
    }
    let mut values = request.headers().get_all(header::AUTHORIZATION).iter();
    let valid = values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            (scheme.eq_ignore_ascii_case("Bearer") && !token.is_empty()).then_some(token)
        })
        .is_some_and(|token| access.matches_token(token.as_bytes()))
        && values.next().is_none();
    if !valid {
        let mut response = refusal(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "A valid LAN Bearer token is required.",
        );
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, "Bearer".parse().unwrap());
        return response;
    }
    request.headers_mut().remove(header::AUTHORIZATION);
    let Ok(permit) = access.requests.clone().try_acquire_owned() else {
        return refusal(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "Too many active requests.",
        );
    };
    let (parts, body) = request.into_parts();
    let body =
        match tokio::time::timeout(access.receive_timeout, to_bytes(body, MAX_BODY_BYTES)).await {
            Err(_) => {
                return refusal(
                    StatusCode::REQUEST_TIMEOUT,
                    "invalid_request_error",
                    "Request body receive deadline exceeded.",
                );
            }
            Ok(Err(error)) => {
                let too_large = error.into_inner().is::<http_body_util::LengthLimitError>();
                return refusal(
                    if too_large {
                        StatusCode::PAYLOAD_TOO_LARGE
                    } else {
                        StatusCode::BAD_REQUEST
                    },
                    "invalid_request_error",
                    if too_large {
                        "Request body exceeds the size limit."
                    } else {
                        "Request body could not be read."
                    },
                );
            }
            Ok(Ok(body)) => body,
        };
    let response = next.run(Request::from_parts(parts, Body::from(body))).await;
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(LeasedBody {
            body,
            permit: Some(permit),
        }),
    )
}

// The permit lives through streaming, including a handler returning before SSE ends.
struct LeasedBody {
    body: Body,
    permit: Option<OwnedSemaphorePermit>,
}

impl HttpBody for LeasedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let result = Pin::new(&mut self.body).poll_frame(cx);
        if matches!(result, Poll::Ready(None | Some(Err(_)))) {
            self.permit.take();
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// LAN transport deliberately serves HTTP/1.1, matching the existing axum surface.
/// Header timeout also covers idle keep-alive connections, without timing out SSE.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()>,
) -> std::io::Result<()> {
    serve_with_timeout(listener, router, shutdown, RECEIVE_TIMEOUT).await
}

async fn serve_with_timeout(
    listener: TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()>,
    header_timeout: Duration,
) -> std::io::Result<()> {
    let connections = Arc::new(Semaphore::new(LIMIT));
    let (stop, _) = watch::channel(false);
    let mut tasks = JoinSet::new();
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            _ = &mut shutdown => break Ok(()),
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = listener.accept() => {
                let (socket, peer) = match accepted { Ok(value) => value, Err(error) => break Err(error) };
                let Ok(permit) = connections.clone().try_acquire_owned() else { continue; };
                let router = router.clone();
                let mut stopped = stop.subscribe();
                tasks.spawn(async move {
                    let _permit = permit;
                    let service = hyper::service::service_fn(move |mut request: hyper::Request<hyper::body::Incoming>| {
                        let router = router.clone();
                        request.extensions_mut().insert(ConnectInfo(peer));
                        async move { router.oneshot(request.map(Body::new)).await }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(header_timeout);
                    let connection = builder.serve_connection(TokioIo::new(socket), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {},
                        _ = stopped.changed() => {
                            connection.as_mut().graceful_shutdown();
                            let _ = connection.await;
                        }
                    }
                });
            }
        }
    };
    let _ = stop.send(true);
    while tasks.join_next().await.is_some() {}
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use axum::routing::get;
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn access(limit: usize) -> Arc<Access> {
        Arc::new(Access {
            token: TOKEN.as_bytes().to_vec(),
            cidrs: vec![
                "192.168.1.0/24".parse().unwrap(),
                "fd12:3456::/32".parse().unwrap(),
            ],
            requests: Arc::new(Semaphore::new(limit)),
            receive_timeout: Duration::from_millis(30),
        })
    }

    fn request(peer: &str, token: Option<&str>, body: Body) -> Request {
        let mut request = Request::builder().uri("/v1/responses").body(body).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        if let Some(token) = token {
            request.headers_mut().insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
            );
        }
        request
    }

    fn app(access: Arc<Access>, calls: Arc<AtomicUsize>) -> Router {
        let handler = move |headers: axum::http::HeaderMap, _: Bytes| {
            let calls = calls.clone();
            async move {
                assert!(!headers.contains_key(header::AUTHORIZATION));
                calls.fetch_add(1, Ordering::SeqCst);
                "ok"
            }
        };
        protect(
            Router::new()
                .route("/v1/responses", get(handler.clone()).post(handler.clone()))
                .route("/v1/models", get(handler)),
            access,
        )
    }

    #[test]
    fn configuration_fails_closed_and_private_ranges_do_not_span_public_addresses() {
        let mut server = Server {
            bind: "127.0.0.1:0".into(),
            allow_lan: false,
            auth_token_env: None,
            allowed_client_cidrs: vec![],
        };
        let bind = server.bind.parse().unwrap();
        assert!(resolve(&server, bind).unwrap().is_none());
        server.allowed_client_cidrs = vec!["192.168.1.0/24".into()];
        assert!(resolve(&server, bind).is_err());
        server.allow_lan = true;
        assert!(resolve(&server, bind).is_err());
        assert!(resolve(&server, "8.8.8.8:80".parse().unwrap()).is_err());
        for token in ["", "abc", &"a".repeat(65), &"g".repeat(64)] {
            assert!(build_access(&server, token.into()).is_err());
        }
        for cidr in [
            "0.0.0.0/0",
            "10.0.0.0/7",
            "172.16.0.0/11",
            "192.168.0.0/15",
            "::/0",
            "fc00::/6",
            "::ffff:192.168.0.0/112",
            "bad",
        ] {
            server.allowed_client_cidrs = vec![cidr.into()];
            assert!(build_access(&server, TOKEN.into()).is_err(), "{cidr}");
        }
        for cidr in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"] {
            server.allowed_client_cidrs = vec![cidr.into()];
            let access = build_access(&server, TOKEN.into()).unwrap();
            assert!(access.matches_token(TOKEN.as_bytes()));
            assert!(!access.matches_token(b"wrong"));
        }
        server.allowed_client_cidrs.clear();
        assert!(build_access(&server, TOKEN.into()).is_err());
    }

    #[tokio::test]
    async fn oversized_authenticated_body_never_reaches_handler() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(access(1), calls.clone());
        let response = app
            .oneshot(request(
                "127.0.0.1:1",
                Some(TOKEN),
                Body::from(vec![0; MAX_BODY_BYTES + 1]),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn broken_body_is_a_bad_request_without_leaking_transport_details() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(access(1), calls.clone());
        let body = Body::from_stream(tokio_stream::iter([Err::<Bytes, _>(
            std::io::Error::other("private transport detail"),
        )]));
        let response = app
            .oneshot(request("127.0.0.1:1", Some(TOKEN), body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("private transport detail"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn transport_connection_limit_and_long_sse_preserve_bounds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let access = access(32);
        let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(2);
        let receiver = Arc::new(tokio::sync::Mutex::new(Some(receiver)));
        let app = protect(
            Router::new()
                .route(
                    "/v1/responses",
                    get(move || {
                        let receiver = receiver.clone();
                        async move {
                            Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(
                                receiver.lock().await.take().unwrap(),
                            ))
                        }
                    }),
                )
                .route("/v1/models", get(|| async { "ok" })),
            access,
        );
        let serving = tokio::spawn(serve_with_timeout(
            listener,
            app,
            async {
                let _ = stopped.await;
            },
            Duration::from_millis(150),
        ));
        let mut sse = TcpStream::connect(address).await.unwrap();
        sse.write_all(format!("GET /v1/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").as_bytes()).await.unwrap();
        sender
            .send(Ok(Bytes::from_static(b"data: first\n\n")))
            .await
            .unwrap();
        let mut chunk = [0; 4096];
        let received = tokio::time::timeout(Duration::from_secs(2), sse.read(&mut chunk))
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&chunk[..received]).starts_with("HTTP/1.1 200"));
        // Each confirmed response leaves a distinct keep-alive connection open.
        let mut sockets = Vec::new();
        for _ in 1..LIMIT {
            let mut socket = TcpStream::connect(address).await.unwrap();
            socket.write_all(format!("GET /v1/models HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").as_bytes()).await.unwrap();
            let received = socket.read(&mut chunk).await.unwrap();
            assert!(String::from_utf8_lossy(&chunk[..received]).starts_with("HTTP/1.1 200"));
            sockets.push(socket);
        }
        let mut extra = TcpStream::connect(address).await.unwrap();
        let extra_read = tokio::time::timeout(Duration::from_secs(2), extra.read(&mut chunk))
            .await
            .unwrap();
        assert!(matches!(extra_read, Ok(0) | Err(_)));
        // After the header deadline expires, active SSE must still deliver.
        tokio::time::sleep(Duration::from_millis(200)).await;
        sender
            .send(Ok(Bytes::from_static(b"data: second\n\n")))
            .await
            .unwrap();
        let received = tokio::time::timeout(Duration::from_secs(2), sse.read(&mut chunk))
            .await
            .unwrap()
            .unwrap();
        assert!(received > 0, "SSE was closed by an idle/header deadline");
        drop(sender);
        drop(sse);
        drop(sockets);
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn denials_never_read_the_body_or_reach_a_handler() {
        let calls = Arc::new(AtomicUsize::new(0));
        let access = access(32);
        let app = app(access.clone(), calls.clone());
        for (peer, token, expected) in [
            ("192.168.1.5:1", None, StatusCode::UNAUTHORIZED),
            ("192.168.1.5:1", Some("wrong"), StatusCode::UNAUTHORIZED),
            ("127.0.0.1:1", None, StatusCode::UNAUTHORIZED),
            ("192.168.2.5:1", Some(TOKEN), StatusCode::FORBIDDEN),
            ("8.8.8.8:1", Some(TOKEN), StatusCode::FORBIDDEN),
        ] {
            let body = Body::from_stream(tokio_stream::pending::<Result<Bytes, std::io::Error>>());
            let mut request = request(peer, token, body);
            request
                .headers_mut()
                .insert("x-forwarded-for", "192.168.1.5".parse().unwrap());
            let response =
                tokio::time::timeout(Duration::from_secs(1), app.clone().oneshot(request))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(response.status(), expected);
            if expected == StatusCode::UNAUTHORIZED {
                assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
            }
            let text =
                String::from_utf8(to_bytes(response.into_body(), 4096).await.unwrap().to_vec())
                    .unwrap();
            assert!(!text.contains(TOKEN));
        }
        let mut duplicate = request("127.0.0.1:1", Some(TOKEN), Body::empty());
        duplicate
            .headers_mut()
            .append(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert_eq!(
            app.clone().oneshot(duplicate).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let mut origin = request("127.0.0.1:1", Some(TOKEN), Body::empty());
        origin
            .headers_mut()
            .insert(header::ORIGIN, "http://example.com".parse().unwrap());
        assert_eq!(
            app.clone().oneshot(origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let mut missing_peer = request("127.0.0.1:1", Some(TOKEN), Body::empty());
        missing_peer
            .extensions_mut()
            .remove::<ConnectInfo<SocketAddr>>();
        assert_eq!(
            app.oneshot(missing_peer).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(access.requests.available_permits(), 32);
    }

    #[tokio::test]
    async fn authorized_models_and_responses_accept_ipv4_ipv6_and_mapped_peers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(access(32), calls.clone());
        for peer in [
            "192.168.1.2:1",
            "[fd12:3456::1]:1",
            "[::ffff:192.168.1.2]:1",
            "[::1]:1",
        ] {
            for path in ["/v1/models", "/v1/responses"] {
                let mut request = request(peer, Some(TOKEN), Body::empty());
                *request.uri_mut() = path.parse().unwrap();
                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(to_bytes(response.into_body(), 16).await.unwrap(), "ok");
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 8);
    }

    #[tokio::test]
    async fn slow_authenticated_body_times_out_before_handler() {
        let calls = Arc::new(AtomicUsize::new(0));
        let access = access(1);
        let app = app(access.clone(), calls.clone());
        let body = Body::from_stream(tokio_stream::pending::<Result<Bytes, std::io::Error>>());
        let response = app
            .oneshot(request("127.0.0.1:1", Some(TOKEN), body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(access.requests.available_permits(), 1);
    }

    #[tokio::test]
    async fn streaming_holds_request_permit_until_end_or_disconnect() {
        let access = access(1);
        let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(2);
        let receiver = Arc::new(tokio::sync::Mutex::new(Some(receiver)));
        let app = protect(
            Router::new().route(
                "/v1/responses",
                get(move || {
                    let receiver = receiver.clone();
                    async move {
                        Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(
                            receiver.lock().await.take().unwrap(),
                        ))
                    }
                }),
            ),
            access.clone(),
        );
        let first = app
            .clone()
            .oneshot(request("127.0.0.1:1", Some(TOKEN), Body::empty()))
            .await
            .unwrap();
        assert_eq!(access.requests.available_permits(), 0);
        let second = app
            .clone()
            .oneshot(request("127.0.0.1:1", Some(TOKEN), Body::empty()))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        let mut body = first.into_body();
        sender
            .send(Ok(Bytes::from_static(b"data: ok\n\n")))
            .await
            .unwrap();
        assert!(body.frame().await.unwrap().is_ok());
        assert_eq!(access.requests.available_permits(), 0);
        drop(sender);
        assert!(body.frame().await.is_none());
        assert_eq!(access.requests.available_permits(), 1);
        let permit = access.requests.clone().try_acquire_owned().unwrap();
        let body = Body::new(LeasedBody {
            body: Body::empty(),
            permit: Some(permit),
        });
        drop(body);
        assert_eq!(access.requests.available_permits(), 1);
    }

    #[tokio::test]
    async fn transport_rejects_slow_headers_and_http2_and_stops_cleanly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let app = app(access(32), Arc::new(AtomicUsize::new(0)));
        let serving = tokio::spawn(serve_with_timeout(
            listener,
            app,
            async {
                let _ = stopped.await;
            },
            Duration::from_millis(50),
        ));
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket
            .write_all(b"GET /v1/models HTTP/1.1\r\nHost:")
            .await
            .unwrap();
        let mut buffer = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        let mut h2 = TcpStream::connect(address).await.unwrap();
        h2.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            .await
            .unwrap();
        buffer.clear();
        tokio::time::timeout(Duration::from_secs(2), h2.read_to_end(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert!(!buffer.starts_with(b"HTTP/1.1 200"));
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
