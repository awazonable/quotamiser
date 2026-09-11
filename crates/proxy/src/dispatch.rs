//! Sending a reserved request upstream and supervising it to settlement.
//!
//! The work runs in a spawned supervisor, never in the request handler: axum
//! drops a handler within a millisecond of the client disconnecting
//! (ADR-0004), and the reservation's transitions must still complete.
//!
//! The order of operations is the safety argument.
//!
//! 1. `DISPATCHING` is made durable before the upstream request future is
//!    first polled; reqwest sends nothing before that poll (ADR-0005).
//! 2. A transport error after that point is an unknown outcome, because
//!    upstream may have received the whole request. The reservation is held.
//! 3. The response id is attached as soon as it is seen, and the reservation
//!    is settled from the terminal event's usage.
//! 4. If the client disconnects before the id is known, the supervisor keeps
//!    reading upstream for a bounded time to learn it. A background response
//!    keeps generating after its connection closes, so without the id there
//!    would be nothing to cancel and nothing to retrieve.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use quotamiser_ledger::{CompletedResponse, ReservationId, Settlement};
use quotamiser_protocol::responses::{Observation, StreamObserver};
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use reqwest::{Response, StatusCode};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::ledger_handle::LedgerHandle;
use crate::status_policy::{self, ReservationAction, StatusDecision};
use crate::upstream::Upstream;

/// The right to send exactly one request whose liability has been reserved.
/// Not `Clone`: it is consumed when dispatch begins.
pub struct DispatchPermit {
    reservation: ReservationId,
    body: Bytes,
    accounting_rev: i64,
}

impl DispatchPermit {
    /// Binds a reservation to the exact canonical body its liability was
    /// computed for.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "admission, which creates permits, is not written yet"
        )
    )]
    pub(crate) fn new(reservation: ReservationId, body: Bytes, accounting_rev: i64) -> Self {
        Self {
            reservation,
            body,
            accounting_rev,
        }
    }

    pub fn reservation(&self) -> ReservationId {
        self.reservation
    }
}

#[derive(Debug, Clone)]
pub struct DispatchPolicy {
    /// How long to keep reading upstream for the response id after the client
    /// has disconnected.
    pub id_wait_after_disconnect: Duration,
    pub max_error_body_bytes: usize,
    pub channel_capacity: usize,
}

/// What the request handler receives.
pub enum Head {
    /// Relay this body to the client.
    Stream {
        status: StatusCode,
        content_type: Option<HeaderValue>,
        body: mpsc::Receiver<Bytes>,
    },
    /// Upstream refused. The decision says what the router should do next.
    Refused {
        status: StatusCode,
        decision: StatusDecision,
        body: Bytes,
    },
    /// Nothing was sent; the reservation was released.
    NotSent(String),
    /// Sent, but the outcome is unknown; the reservation is held.
    OutcomeUnknown(String),
}

/// Where the reservation ended up when the supervisor finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    Settled(Settlement),
    /// The id is known and recorded; retrieval settles it.
    AwaitingRetrieval {
        response_id: String,
    },
    /// Sent with no id to recover; written off at its deadline.
    HeldAsUnknown,
    ReleasedUnsent,
    LedgerFailed(String),
}

pub struct Dispatched {
    pub head: Head,
    /// Dropping this does not stop the supervisor.
    pub supervisor: JoinHandle<Disposition>,
}

pub struct Dispatcher {
    upstream: Arc<Upstream>,
    ledger: LedgerHandle,
    policy: DispatchPolicy,
}

impl Dispatcher {
    pub fn new(upstream: Arc<Upstream>, ledger: LedgerHandle, policy: DispatchPolicy) -> Self {
        Self {
            upstream,
            ledger,
            policy,
        }
    }

    /// If the caller is dropped while waiting for the head, the supervisor
    /// treats the client as disconnected and carries on.
    pub async fn dispatch(&self, permit: DispatchPermit) -> Dispatched {
        let (head_tx, head_rx) = oneshot::channel();
        let supervisor = tokio::spawn(supervise(
            self.upstream.clone(),
            self.ledger.clone(),
            self.policy.clone(),
            permit,
            head_tx,
        ));
        let head = head_rx.await.unwrap_or_else(|_| {
            Head::OutcomeUnknown("the supervisor ended without reporting".into())
        });
        Dispatched { head, supervisor }
    }
}

async fn supervise(
    upstream: Arc<Upstream>,
    ledger: LedgerHandle,
    policy: DispatchPolicy,
    permit: DispatchPermit,
    head_tx: oneshot::Sender<Head>,
) -> Disposition {
    let DispatchPermit {
        reservation,
        body,
        accounting_rev,
    } = permit;
    let mut head_tx = Some(head_tx);

    if let Err(error) = ledger.with(move |l| l.begin_dispatch(reservation)).await {
        let released = ledger.with(move |l| l.release_unsent(reservation)).await;
        deliver(&mut head_tx, Head::NotSent(error.to_string()));
        return match released {
            Ok(()) => Disposition::ReleasedUnsent,
            Err(error) => Disposition::LedgerFailed(error.to_string()),
        };
    }

    // Created only now, after DISPATCHING is durable, and polled right here.
    let response = match upstream.create_response(body).await {
        Ok(response) => response,
        Err(error) => {
            let disposition = hold_as_unknown(&ledger, reservation).await;
            deliver(&mut head_tx, Head::OutcomeUnknown(error.to_string()));
            return disposition;
        }
    };

    let status = response.status();
    let decision = status_policy::decide(status.as_u16());
    if decision.reservation == ReservationAction::HoldAsUnknown {
        let body = read_capped(response, policy.max_error_body_bytes).await;
        let disposition = hold_as_unknown(&ledger, reservation).await;
        deliver(
            &mut head_tx,
            Head::Refused {
                status,
                decision,
                body,
            },
        );
        return disposition;
    }

    relay(
        &upstream,
        &ledger,
        &policy,
        reservation,
        accounting_rev,
        response,
        head_tx,
    )
    .await
}

async fn relay(
    upstream: &Upstream,
    ledger: &LedgerHandle,
    policy: &DispatchPolicy,
    reservation: ReservationId,
    accounting_rev: i64,
    mut response: Response,
    mut head_tx: Option<oneshot::Sender<Head>>,
) -> Disposition {
    let (body_tx, body_rx) = mpsc::channel(policy.channel_capacity);
    let head = Head::Stream {
        status: response.status(),
        content_type: response.headers().get(CONTENT_TYPE).cloned(),
        body: body_rx,
    };
    let mut downstream = deliver(&mut head_tx, head).then_some(body_tx);
    let mut deadline = downstream
        .is_none()
        .then(|| Instant::now() + policy.id_wait_after_disconnect);
    let mut observer = StreamObserver::new();
    let mut response_id: Option<String> = None;
    let mut settled: Option<Settlement> = None;

    loop {
        if downstream.is_none() && settled.is_none() {
            if let Some(id) = &response_id {
                // Stop generation. What it consumed is settled by retrieval.
                let _ = upstream.cancel(id).await;
                return Disposition::AwaitingRetrieval {
                    response_id: id.clone(),
                };
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return hold_as_unknown(ledger, reservation).await;
            }
        }

        tokio::select! {
            chunk = response.chunk() => {
                let bytes = match chunk {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) | Err(_) => break,
                };
                for observation in observer.push(&bytes) {
                    match observation {
                        Observation::ResponseId(id) => {
                            let attach = id.clone();
                            if let Err(error) = ledger.with(move |l| l.attach_response_id(reservation, &attach)).await {
                                return Disposition::LedgerFailed(error.to_string());
                            }
                            response_id = Some(id);
                        }
                        Observation::Terminal(summary) if settled.is_none() => {
                            let completed = CompletedResponse {
                                usage: summary.usage.map(|u| quotamiser_ledger::Usage {
                                    input_tokens: u.input_tokens,
                                    output_tokens: u.output_tokens,
                                }),
                                model: summary.model.unwrap_or_default(),
                                service_tier: summary.service_tier.unwrap_or_default(),
                            };
                            match ledger.with(move |l| l.settle(reservation, &completed, accounting_rev, unix_now())).await {
                                Ok(settlement) => settled = Some(settlement),
                                Err(error) => return Disposition::LedgerFailed(error.to_string()),
                            }
                        }
                        _ => {}
                    }
                }
                let delivered = match &downstream {
                    Some(tx) => tx.send(bytes).await.is_ok(),
                    None => true,
                };
                if !delivered {
                    downstream = None;
                    deadline.get_or_insert_with(|| Instant::now() + policy.id_wait_after_disconnect);
                }
            }
            () = closed(downstream.as_ref()) => {
                downstream = None;
                deadline.get_or_insert_with(|| Instant::now() + policy.id_wait_after_disconnect);
            }
            () = sleep_until(deadline), if downstream.is_none() && settled.is_none() => {}
        }
    }

    match (settled, response_id) {
        (Some(settlement), _) => Disposition::Settled(settlement),
        (None, Some(response_id)) => Disposition::AwaitingRetrieval { response_id },
        (None, None) => hold_as_unknown(ledger, reservation).await,
    }
}

async fn hold_as_unknown(ledger: &LedgerHandle, reservation: ReservationId) -> Disposition {
    match ledger.with(move |l| l.mark_id_unknown(reservation)).await {
        Ok(()) => Disposition::HeldAsUnknown,
        Err(error) => Disposition::LedgerFailed(error.to_string()),
    }
}

/// Whether the head reached the handler.
fn deliver(head_tx: &mut Option<oneshot::Sender<Head>>, head: Head) -> bool {
    head_tx.take().is_some_and(|tx| tx.send(head).is_ok())
}

async fn closed(tx: Option<&mpsc::Sender<Bytes>>) {
    match tx {
        Some(tx) => tx.closed().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
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

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::header::LOCATION;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use quotamiser_ledger::{
        Admission, Ledger, LedgerConfig, ReservationRequest, State as ReservationState,
    };
    use tokio_stream::wrappers::ReceiverStream;

    use super::*;
    use crate::status_policy::{ClientAction, ProviderAction};
    use crate::upstream::UpstreamConfig;

    const LIABILITY: u64 = 1_000;

    #[derive(Clone, Copy)]
    enum Mode {
        Completes,
        Redirects,
        RejectsAsBadRequest,
        /// Sends response.created after this delay, then trickles deltas and never completes.
        CreatedAfter(Duration),
        /// Trickles deltas and never names the response.
        NeverNamesTheResponse,
    }

    struct Mock {
        mode: Mode,
        cancels: AtomicUsize,
    }

    fn lifecycle(name: &str, status: &str, usage: serde_json::Value) -> Bytes {
        let data = serde_json::json!({
            "type": name,
            "response": {"id": "resp_1", "status": status, "model": "gpt-5.6-terra", "service_tier": "default", "usage": usage},
        });
        Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
    }

    fn delta() -> Bytes {
        Bytes::from_static(b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n")
    }

    async fn responses(State(mock): State<Arc<Mock>>) -> axum::response::Response {
        match mock.mode {
            Mode::Redirects => {
                return (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(LOCATION, "http://127.0.0.1:9/x")],
                )
                    .into_response();
            }
            Mode::RejectsAsBadRequest => {
                return (StatusCode::BAD_REQUEST, r#"{"error":{"message":"bad"}}"#).into_response();
            }
            _ => {}
        }
        let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(8);
        let mode = mock.mode;
        tokio::spawn(async move {
            match mode {
                Mode::Completes => {
                    let _ = tx
                        .send(Ok(lifecycle(
                            "response.created",
                            "in_progress",
                            serde_json::Value::Null,
                        )))
                        .await;
                    let _ = tx.send(Ok(delta())).await;
                    let usage = serde_json::json!({"input_tokens": 9, "output_tokens": 5});
                    let _ = tx
                        .send(Ok(lifecycle("response.completed", "completed", usage)))
                        .await;
                }
                Mode::CreatedAfter(delay) => {
                    tokio::time::sleep(delay).await;
                    let _ = tx
                        .send(Ok(lifecycle(
                            "response.created",
                            "in_progress",
                            serde_json::Value::Null,
                        )))
                        .await;
                    trickle(&tx).await;
                }
                Mode::NeverNamesTheResponse => trickle(&tx).await,
                Mode::Redirects | Mode::RejectsAsBadRequest => unreachable!(),
            }
        });
        (
            [(CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(ReceiverStream::new(rx)),
        )
            .into_response()
    }

    async fn trickle(tx: &mpsc::Sender<Result<Bytes, Infallible>>) {
        for _ in 0..50 {
            if tx.send(Ok(delta())).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn cancel(State(mock): State<Arc<Mock>>) -> StatusCode {
        mock.cancels.fetch_add(1, Ordering::SeqCst);
        StatusCode::OK
    }

    async fn mock_upstream(mode: Mode) -> (Arc<Mock>, u16) {
        let mock = Arc::new(Mock {
            mode,
            cancels: AtomicUsize::new(0),
        });
        let router = Router::new()
            .route("/v1/responses", post(responses))
            .route("/v1/responses/{id}/cancel", post(cancel))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (mock, port)
    }

    fn reserved_ledger(dir: &Path) -> (LedgerHandle, ReservationId) {
        let config = LedgerConfig {
            ledger_path: dir.join("ledger.db"),
            external_hwm_path: dir.join("external").join("hwm"),
            lock_dir: dir.join("locks"),
            organization: "org-test".into(),
            pools: vec!["openai:small".into()],
            allow_same_volume_external_record: true,
        };
        let (mut ledger, _) = Ledger::open(&config).unwrap();
        ledger
            .rollover(1, &[("openai:small".into(), 10_000)])
            .unwrap();
        ledger.adopt_recovered_consumption(&[]).unwrap();
        ledger
            .revalidate_safety_input("data_sharing", 0, i64::MAX / 2, 1_000_000)
            .unwrap();
        let request = ReservationRequest {
            pool_id: "openai:small".into(),
            liability: LIABILITY,
            request_digest: "digest".into(),
            model_snapshot: "gpt-5.6-terra".into(),
            service_tier: "default".into(),
            accounting_rev: 1,
            required_safety_inputs: vec!["data_sharing".into()],
        };
        let id = match ledger.reserve(&request, 1).unwrap() {
            Admission::Reserved(id) => id,
            other => panic!("expected a reservation, got {other:?}"),
        };
        (LedgerHandle::new(ledger), id)
    }

    fn dispatcher(port: u16, ledger: &LedgerHandle, id_wait: Duration) -> Dispatcher {
        let upstream = Upstream::new(UpstreamConfig {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: "test-key".into(),
            project: None,
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(5),
        })
        .unwrap();
        let policy = DispatchPolicy {
            id_wait_after_disconnect: id_wait,
            max_error_body_bytes: 4096,
            channel_capacity: 4,
        };
        Dispatcher::new(Arc::new(upstream), ledger.clone(), policy)
    }

    async fn state(ledger: &LedgerHandle, id: ReservationId) -> ReservationState {
        ledger.with(move |l| l.reservation_state(id)).await.unwrap()
    }

    async fn counters(ledger: &LedgerHandle) -> (i64, i64) {
        let c = ledger
            .with(|l| l.counters("openai:small", 1))
            .await
            .unwrap()
            .unwrap();
        (c.reserved, c.consumed)
    }

    fn permit(id: ReservationId) -> DispatchPermit {
        DispatchPermit::new(id, Bytes::from_static(b"{}"), 1)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_completed_stream_is_relayed_and_settled() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, id) = reserved_ledger(dir.path());
        let (_mock, port) = mock_upstream(Mode::Completes).await;

        let dispatched = dispatcher(port, &ledger, Duration::from_secs(2))
            .dispatch(permit(id))
            .await;
        let Head::Stream {
            status, mut body, ..
        } = dispatched.head
        else {
            panic!("expected a stream")
        };
        assert_eq!(status, StatusCode::OK);
        let mut relayed = BytesMut::new();
        while let Some(chunk) = body.recv().await {
            relayed.extend_from_slice(&chunk);
        }
        assert!(
            String::from_utf8_lossy(&relayed).contains("response.completed"),
            "every byte reaches the client"
        );

        assert_eq!(
            dispatched.supervisor.await.unwrap(),
            Disposition::Settled(Settlement::Settled {
                actual: 14,
                overrun: false
            })
        );
        assert_eq!(state(&ledger, id).await, ReservationState::Settled);
        assert_eq!(counters(&ledger).await, (0, 14));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_redirect_is_refused_and_the_reservation_held() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, id) = reserved_ledger(dir.path());
        let (_mock, port) = mock_upstream(Mode::Redirects).await;

        let dispatched = dispatcher(port, &ledger, Duration::from_secs(2))
            .dispatch(permit(id))
            .await;
        let Head::Refused {
            status, decision, ..
        } = dispatched.head
        else {
            panic!("expected a refusal")
        };
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(decision.provider, ProviderAction::CloseUntilRemediated);
        assert_eq!(
            dispatched.supervisor.await.unwrap(),
            Disposition::HeldAsUnknown
        );
        assert_eq!(
            state(&ledger, id).await,
            ReservationState::DispatchedIdUnknown
        );
        assert_eq!(counters(&ledger).await, (LIABILITY as i64, 0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_request_is_returned_and_the_reservation_held() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, id) = reserved_ledger(dir.path());
        let (_mock, port) = mock_upstream(Mode::RejectsAsBadRequest).await;

        let dispatched = dispatcher(port, &ledger, Duration::from_secs(2))
            .dispatch(permit(id))
            .await;
        let Head::Refused { decision, body, .. } = dispatched.head else {
            panic!("expected a refusal")
        };
        assert_eq!(decision.client, ClientAction::ReturnError);
        assert!(String::from_utf8_lossy(&body).contains("bad"));
        assert_eq!(
            dispatched.supervisor.await.unwrap(),
            Disposition::HeldAsUnknown
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_transport_failure_is_an_unknown_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, id) = reserved_ledger(dir.path());
        let unused_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };

        let dispatched = dispatcher(unused_port, &ledger, Duration::from_secs(2))
            .dispatch(permit(id))
            .await;
        assert!(matches!(dispatched.head, Head::OutcomeUnknown(_)));
        assert_eq!(
            dispatched.supervisor.await.unwrap(),
            Disposition::HeldAsUnknown
        );
        assert_eq!(
            state(&ledger, id).await,
            ReservationState::DispatchedIdUnknown
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_disconnect_after_the_id_cancels_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, id) = reserved_ledger(dir.path());
        let (mock, port) = mock_upstream(Mode::CreatedAfter(Duration::ZERO)).await;

        let dispatched = dispatcher(port, &ledger, Duration::from_secs(2))
            .dispatch(permit(id))
            .await;
        let Head::Stream { mut body, .. } = dispatched.head else {
            panic!("expected a stream")
        };
        body.recv()
            .await
            .expect("the created event is relayed first");
        drop(body);

        assert_eq!(
            dispatched.supervisor.await.unwrap(),
            Disposition::AwaitingRetrieval {
                response_id: "resp_1".into()
            }
        );
        assert_eq!(mock.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(state(&ledger, id).await, ReservationState::DispatchedWithId);
        assert_eq!(
            counters(&ledger).await,
            (LIABILITY as i64, 0),
            "nothing is released until retrieval settles it"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_disconnect_before_the_id_waits_for_it_and_then_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, id) = reserved_ledger(dir.path());
        let (mock, port) = mock_upstream(Mode::CreatedAfter(Duration::from_millis(300))).await;

        let dispatched = dispatcher(port, &ledger, Duration::from_secs(3))
            .dispatch(permit(id))
            .await;
        drop(dispatched.head);

        assert_eq!(
            dispatched.supervisor.await.unwrap(),
            Disposition::AwaitingRetrieval {
                response_id: "resp_1".into()
            }
        );
        assert_eq!(mock.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(state(&ledger, id).await, ReservationState::DispatchedWithId);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_disconnect_before_the_id_gives_up_after_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, id) = reserved_ledger(dir.path());
        let (mock, port) = mock_upstream(Mode::NeverNamesTheResponse).await;

        let dispatched = dispatcher(port, &ledger, Duration::from_millis(200))
            .dispatch(permit(id))
            .await;
        drop(dispatched.head);

        assert_eq!(
            dispatched.supervisor.await.unwrap(),
            Disposition::HeldAsUnknown
        );
        assert_eq!(
            mock.cancels.load(Ordering::SeqCst),
            0,
            "there is no id to cancel"
        );
        assert_eq!(
            state(&ledger, id).await,
            ReservationState::DispatchedIdUnknown
        );
        assert_eq!(counters(&ledger).await, (LIABILITY as i64, 0));
    }
}
