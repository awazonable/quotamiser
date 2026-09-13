//! Sending to OpenRouter, where the allowance is counted in requests.
//!
//! The shape of this path is deliberately unlike the OpenAI one. There, a
//! liability is reserved, the dispatch is made durable before a byte moves,
//! and the reservation is settled or released from what came back. Here there
//! is nothing to settle: the slot is taken before sending and stays taken
//! whatever happens, because a request that fails spends one of the fifty just
//! the same (ADR-0009).
//!
//! Two gates come before the slot, and neither spends one. The reactive rate
//! limit gate, which a 429 closes, and the short window inside the ledger's
//! own transaction. OpenRouter returns no rate-limit headers on success, so
//! the window is the one that does the work; the gate is the second line.

use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use quotamiser_ledger::{RequestSlot, RequestWindow};
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use reqwest::{Response, StatusCode};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::ledger_handle::LedgerHandle;
use crate::openrouter::OpenRouter;
use crate::rate_limit::RateLimitGate;

/// What happened to one attempt at the OpenRouter route.
pub enum Sent {
    /// Relay this body to the client.
    Stream {
        status: StatusCode,
        content_type: Option<HeaderValue>,
        body: mpsc::Receiver<Bytes>,
    },
    /// OpenRouter refused the request itself. The slot is spent.
    Refused { status: StatusCode, body: Bytes },
    /// The day's allowance is gone. Nothing was sent.
    Exhausted { used: i64, limit: i64 },
    /// A rate limit holds the route back. Nothing was sent, nothing spent.
    HeldBack { retry_after: Duration },
    /// Sent, and the outcome is unknown. The slot is spent.
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct OpenRouterPolicy {
    pub pool_id: String,
    pub daily_request_limit: u64,
    pub window: RequestWindow,
    pub max_error_body_bytes: usize,
    pub channel_capacity: usize,
}

pub struct OpenRouterDispatcher {
    openrouter: Arc<OpenRouter>,
    ledger: LedgerHandle,
    gate: Arc<RateLimitGate>,
    policy: OpenRouterPolicy,
}

impl OpenRouterDispatcher {
    pub fn new(
        openrouter: Arc<OpenRouter>,
        ledger: LedgerHandle,
        policy: OpenRouterPolicy,
    ) -> Self {
        Self {
            openrouter,
            ledger,
            gate: Arc::new(RateLimitGate::new()),
            policy,
        }
    }

    /// Takes a slot and sends. `now_unix` must be trusted time: it decides
    /// both which day is being spent and where the short window sits.
    pub async fn send(&self, body: Bytes, epoch: i64, now_unix: i64) -> Sent {
        if let Some(retry_after) = self.gate.closed_for(Instant::now()) {
            return Sent::HeldBack { retry_after };
        }

        let pool_id = self.policy.pool_id.clone();
        let limit = self.policy.daily_request_limit;
        let window = self.policy.window;
        let slot = self
            .ledger
            .with(move |ledger| ledger.take_request_slot(&pool_id, epoch, limit, window, now_unix))
            .await;
        match slot {
            Ok(RequestSlot::Granted { .. }) => {}
            Ok(RequestSlot::Exhausted { used, limit }) => {
                return Sent::Exhausted { used, limit };
            }
            Ok(RequestSlot::WindowFull {
                retry_after_seconds,
            }) => {
                return Sent::HeldBack {
                    retry_after: Duration::from_secs(retry_after_seconds.max(1) as u64),
                };
            }
            Err(error) => return Sent::Failed(error.to_string()),
        }

        // From here the slot is spent, whatever comes back.
        let response = match self.openrouter.create_response(body).await {
            Ok(response) => response,
            Err(error) => return Sent::Failed(error.to_string()),
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            self.gate
                .record_rate_limited(response.headers(), Instant::now());
        }
        if !status.is_success() {
            let body = read_capped(response, self.policy.max_error_body_bytes).await;
            return Sent::Refused { status, body };
        }

        let content_type = response.headers().get(CONTENT_TYPE).cloned();
        let (tx, rx) = mpsc::channel(self.policy.channel_capacity);
        tokio::spawn(relay(response, tx));
        Sent::Stream {
            status,
            content_type,
            body: rx,
        }
    }

    /// For the router, which may want to know before choosing this route.
    pub fn rate_limit_gate(&self) -> &RateLimitGate {
        &self.gate
    }
}

/// Copies the stream through. There is no observer: nothing here needs the
/// response id, and there is no reservation to settle.
async fn relay(mut response: Response, tx: mpsc::Sender<Bytes>) {
    while let Ok(Some(chunk)) = response.chunk().await {
        if tx.send(chunk).await.is_err() {
            // The client is gone. Nothing to cancel: the slot is already spent
            // and no token inventory depends on how this ends.
            return;
        }
    }
}

async fn read_capped(mut response: Response, cap: usize) -> Bytes {
    let mut body = BytesMut::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = cap.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= cap {
            break;
        }
    }
    body.freeze()
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use quotamiser_ledger::{Ledger, LedgerConfig};
    use tokio_stream::wrappers::ReceiverStream;

    use super::*;
    use crate::config::{OpenRouterModel, ResolvedOpenRouter};

    const POOL: &str = "openrouter:free";
    const EPOCH: i64 = 20_708;

    #[derive(Clone, Copy)]
    enum Mode {
        Streams,
        Rejects,
    }

    struct Mock {
        mode: Mode,
        requests: AtomicUsize,
    }

    async fn mock(mode: Mode) -> (Arc<Mock>, String) {
        let state = Arc::new(Mock {
            mode,
            requests: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route(
                "/api/v1/responses",
                post(|State(mock): State<Arc<Mock>>, _body: Bytes| async move {
                    mock.requests.fetch_add(1, Ordering::SeqCst);
                    match mock.mode {
                        Mode::Rejects => (
                            StatusCode::BAD_REQUEST,
                            r#"{"error":{"message":"expected function"}}"#,
                        )
                            .into_response(),
                        Mode::Streams => {
                            let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(4);
                            tokio::spawn(async move {
                                let _ = tx
                                    .send(Ok(Bytes::from_static(
                                        b"event: response.created\ndata: {}\n\n",
                                    )))
                                    .await;
                                let _ = tx
                                    .send(Ok(Bytes::from_static(
                                        b"event: response.completed\ndata: {}\n\n",
                                    )))
                                    .await;
                            });
                            (
                                [(CONTENT_TYPE, "text/event-stream")],
                                Body::from_stream(ReceiverStream::new(rx)),
                            )
                                .into_response()
                        }
                    }
                }),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (state, format!("http://127.0.0.1:{port}/api/v1"))
    }

    fn ledger(dir: &Path) -> LedgerHandle {
        let config = LedgerConfig {
            ledger_path: dir.join("ledger.db"),
            external_hwm_path: dir.join("external").join("hwm"),
            lock_dir: dir.join("locks"),
            organization: "org-test".into(),
            pools: vec!["openai:small".into()],
            allow_same_volume_external_record: true,
        };
        LedgerHandle::new(Ledger::open(&config).unwrap().0)
    }

    fn dispatcher(
        base_url: String,
        ledger: &LedgerHandle,
        daily_request_limit: u64,
        max_in_window: u32,
    ) -> OpenRouterDispatcher {
        let resolved = ResolvedOpenRouter {
            base_url,
            api_key: "sk-or-test".into(),
            management_key: None,
            pool_id: POOL.into(),
            daily_request_limit,
            window: RequestWindow {
                max_in_window,
                window_seconds: 60,
            },
            account_ttl: 900,
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(5),
            models: vec![OpenRouterModel {
                id: "a/b:free".into(),
                function_tools: true,
                custom_tools: false,
                namespace_tools: true,
                structured_outputs: false,
            }],
        };
        OpenRouterDispatcher::new(
            Arc::new(OpenRouter::new(&resolved).unwrap()),
            ledger.clone(),
            OpenRouterPolicy {
                pool_id: POOL.into(),
                daily_request_limit,
                window: resolved.window,
                max_error_body_bytes: 4096,
                channel_capacity: 8,
            },
        )
    }

    async fn used(ledger: &LedgerHandle) -> i64 {
        ledger
            .with(|l| l.request_counts(POOL, EPOCH))
            .await
            .unwrap()
            .map_or(0, |(used, _)| used)
    }

    fn body() -> Bytes {
        Bytes::from_static(br#"{"model":"a/b:free","stream":true,"store":false}"#)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_granted_slot_sends_and_relays() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(dir.path());
        let (mock, base) = mock(Mode::Streams).await;
        let dispatcher = dispatcher(base, &ledger, 50, 20);

        let Sent::Stream {
            status, mut body, ..
        } = dispatcher.send(body(), EPOCH, 1_000).await
        else {
            panic!("expected a stream")
        };
        assert_eq!(status, StatusCode::OK);
        let mut relayed = BytesMut::new();
        while let Some(chunk) = body.recv().await {
            relayed.extend_from_slice(&chunk);
        }
        assert!(String::from_utf8_lossy(&relayed).contains("response.completed"));
        assert_eq!(mock.requests.load(Ordering::SeqCst), 1);
        assert_eq!(used(&ledger).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_still_spends_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(dir.path());
        let (_mock, base) = mock(Mode::Rejects).await;
        let dispatcher = dispatcher(base, &ledger, 50, 20);

        let Sent::Refused {
            status,
            body: refusal,
        } = dispatcher.send(body(), EPOCH, 1_000).await
        else {
            panic!("expected a refusal")
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&refusal).contains("expected function"));
        assert_eq!(
            used(&ledger).await,
            1,
            "a failed request spends one of the fifty too"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_day_runs_out_without_sending() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(dir.path());
        let (mock, base) = mock(Mode::Streams).await;
        let dispatcher = dispatcher(base, &ledger, 2, 20);

        for at in [1_000, 1_001] {
            assert!(matches!(
                dispatcher.send(body(), EPOCH, at).await,
                Sent::Stream { .. }
            ));
        }
        let Sent::Exhausted { used: spent, limit } = dispatcher.send(body(), EPOCH, 1_002).await
        else {
            panic!("expected the day to be spent")
        };
        assert_eq!((spent, limit), (2, 2));
        assert_eq!(
            mock.requests.load(Ordering::SeqCst),
            2,
            "nothing is sent once the allowance is gone"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_short_window_holds_back_without_spending() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(dir.path());
        let (mock, base) = mock(Mode::Streams).await;
        let dispatcher = dispatcher(base, &ledger, 50, 2);

        for _ in 0..2 {
            assert!(matches!(
                dispatcher.send(body(), EPOCH, 1_000).await,
                Sent::Stream { .. }
            ));
        }
        let Sent::HeldBack { retry_after } = dispatcher.send(body(), EPOCH, 1_010).await else {
            panic!("expected to be held back")
        };
        assert_eq!(retry_after, Duration::from_secs(50));
        assert_eq!(mock.requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            used(&ledger).await,
            2,
            "being held back by the window spends nothing"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_transport_failure_spends_the_slot_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(dir.path());
        let unused_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let dispatcher = dispatcher(
            format!("http://127.0.0.1:{unused_port}/api/v1"),
            &ledger,
            50,
            20,
        );

        assert!(matches!(
            dispatcher.send(body(), EPOCH, 1_000).await,
            Sent::Failed(_)
        ));
        assert_eq!(
            used(&ledger).await,
            1,
            "the request may have reached the provider; the slot is not returned"
        );
    }
}
