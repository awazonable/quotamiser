//! The only holder of an upstream HTTP client and credential.
//!
//! Every operation is narrowly typed. There is no general "send this request
//! with the credential" function, because one would be an inference path
//! that bypasses the reservation. The operation that starts generation is
//! crate-private and reserved for the dispatcher; the public ones — cancel,
//! retrieve, count input tokens — cannot start generation.
//!
//! The client never resends on its own (ADR-0005): its retry layer and
//! redirect following are disabled, and it ignores system proxy settings so
//! the credential goes only to the configured host.

use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, Response, Url, redirect};

#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    /// Including the version prefix, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub api_key: String,
    /// Sent as `OpenAI-Project` when set. Pinned by configuration; a client
    /// cannot choose it.
    pub project: Option<String>,
    pub connect_timeout: Duration,
    /// Idle time allowed between reads, including between stream chunks.
    pub read_timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("the upstream base URL is invalid: {0}")]
    InvalidBaseUrl(String),
    #[error("the upstream base URL must use https unless it is loopback: {0}")]
    InsecureBaseUrl(String),
    #[error("not a valid response id: {0:?}")]
    InvalidResponseId(String),
    #[error("could not build the upstream client: {0}")]
    Build(#[source] reqwest::Error),
    #[error("upstream transport failed: {0}")]
    Transport(#[source] reqwest::Error),
}

pub struct Upstream {
    client: Client,
    base_url: String,
    api_key: String,
    project: Option<String>,
}

impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upstream")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Upstream {
    pub fn new(config: UpstreamConfig) -> Result<Self, UpstreamError> {
        let base = config.base_url.trim_end_matches('/').to_string();
        let url = Url::parse(&base).map_err(|_| UpstreamError::InvalidBaseUrl(base.clone()))?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err(UpstreamError::InsecureBaseUrl(base));
        }
        let client = Client::builder()
            .retry(reqwest::retry::never())
            .redirect(redirect::Policy::none())
            .no_proxy()
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .build()
            .map_err(UpstreamError::Build)?;
        Ok(Self {
            client,
            base_url: base,
            api_key: config.api_key,
            project: config.project,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self
            .client
            .request(method, format!("{}{path}", self.base_url))
            .bearer_auth(&self.api_key);
        match &self.project {
            Some(project) => builder.header("OpenAI-Project", project),
            None => builder,
        }
    }

    /// Starts generation. Nothing leaves until the returned future is first
    /// polled, and the dispatcher must have made `DISPATCHING` durable before
    /// that poll.
    pub(crate) fn create_response(
        &self,
        canonical_body: Bytes,
    ) -> impl Future<Output = reqwest::Result<Response>> {
        self.request(reqwest::Method::POST, "/responses")
            .header(CONTENT_TYPE, "application/json")
            .body(canonical_body)
            .send()
    }

    pub async fn cancel(&self, response_id: &str) -> Result<Response, UpstreamError> {
        let path = format!("/responses/{}/cancel", checked_id(response_id)?);
        self.request(reqwest::Method::POST, &path)
            .send()
            .await
            .map_err(UpstreamError::Transport)
    }

    pub async fn retrieve(&self, response_id: &str) -> Result<Response, UpstreamError> {
        let path = format!("/responses/{}", checked_id(response_id)?);
        self.request(reqwest::Method::GET, &path)
            .send()
            .await
            .map_err(UpstreamError::Transport)
    }

    /// Counts input tokens exactly. Measured to incur no charge and consume
    /// no free quota.
    pub async fn count_input_tokens(
        &self,
        canonical_body: Bytes,
    ) -> Result<Response, UpstreamError> {
        self.request(reqwest::Method::POST, "/responses/input_tokens")
            .header(CONTENT_TYPE, "application/json")
            .body(canonical_body)
            .send()
            .await
            .map_err(UpstreamError::Transport)
    }
}

/// Response ids come from upstream output, but they still become a URL path
/// segment and must not be able to change the path.
fn checked_id(response_id: &str) -> Result<&str, UpstreamError> {
    let valid = !response_id.is_empty()
        && response_id.len() <= 128
        && response_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(response_id)
    } else {
        Err(UpstreamError::InvalidResponseId(response_id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::routing::{any, post};

    use super::*;

    async fn serve(router: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        addr
    }

    fn upstream(addr: SocketAddr) -> Upstream {
        Upstream::new(UpstreamConfig {
            base_url: format!("http://127.0.0.1:{}/v1", addr.port()),
            api_key: "test-key".into(),
            project: Some("proj_pinned".into()),
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(5),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn a_redirect_is_returned_not_followed() {
        let target_hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = target_hits.clone();
        let location = format!("http://127.0.0.1:{}/elsewhere", addr.port());
        let router = Router::new()
            .route(
                "/v1/responses",
                post(move || {
                    let location = location.clone();
                    async move {
                        (
                            StatusCode::TEMPORARY_REDIRECT,
                            [(header::LOCATION, location)],
                        )
                    }
                }),
            )
            .route(
                "/elsewhere",
                any(move || {
                    hits.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::OK }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let response = upstream(addr)
            .create_response(Bytes::from_static(b"{}"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            target_hits.load(Ordering::SeqCst),
            0,
            "the redirect target must never be contacted"
        );
    }

    #[tokio::test]
    async fn only_the_configured_credential_and_project_are_sent() {
        let router = Router::new().route(
            "/v1/responses",
            post(|headers: HeaderMap| async move {
                let pick = |name: &str| {
                    headers
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string()
                };
                format!(
                    "{}|{}|{}",
                    pick("authorization"),
                    pick("openai-project"),
                    pick("openai-organization")
                )
            }),
        );
        let addr = serve(router).await;
        let response = upstream(addr)
            .create_response(Bytes::from_static(b"{}"))
            .await
            .unwrap();
        assert_eq!(
            response.text().await.unwrap(),
            "Bearer test-key|proj_pinned|"
        );
    }

    #[test]
    fn a_non_loopback_http_base_url_is_refused() {
        let config = |base_url: &str| UpstreamConfig {
            base_url: base_url.into(),
            api_key: "k".into(),
            project: None,
            connect_timeout: Duration::from_secs(1),
            read_timeout: Duration::from_secs(1),
        };
        assert!(matches!(
            Upstream::new(config("http://api.openai.com/v1")),
            Err(UpstreamError::InsecureBaseUrl(_))
        ));
        assert!(Upstream::new(config("https://api.openai.com/v1")).is_ok());
        assert!(Upstream::new(config("http://127.0.0.1:8080/v1")).is_ok());
    }

    #[test]
    fn a_response_id_cannot_change_the_path() {
        for bad in ["", "../models", "resp_1/cancel", "resp 1", "resp%2F1"] {
            assert!(checked_id(bad).is_err(), "{bad:?}");
        }
        assert!(checked_id("resp_0fca4faa0801454d006aa14bc55b3887d0b8156d57e1f3ca7d").is_ok());
    }
}
