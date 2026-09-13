//! Settling reservations that their stream could not settle.
//!
//! A dispatch that ended with a known response id — the client disconnected
//! and upstream was cancelled, or the stream ended without a terminal event —
//! is settled here by retrieving the response. A dispatch with no id has
//! nothing to retrieve and is written off after a short hold.
//!
//! Retrieval never releases a reservation on weak evidence. A response still
//! in progress is retried later. "Not found" is trusted only once the
//! response is old enough that eventual consistency no longer explains it.
//! Anything else — transport errors, unreadable bodies, other statuses — is
//! retried with backoff, and a reservation left unsettled too long is flagged
//! for attention rather than written off.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use quotamiser_ledger::{CompletedResponse, OpenDispatch, ReservationId, Settlement, State};
use quotamiser_protocol::responses::ResponseSummary;
use tokio::time::Instant;

use crate::ledger_handle::{LedgerAccessError, LedgerHandle};
use crate::upstream::Upstream;

#[derive(Debug, Clone)]
pub struct RetrievalPolicy {
    /// The first retry delay, doubling up to `max_backoff`.
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// A dispatch with no response id is written off once this old.
    pub write_off_unknown_after: Duration,
    /// Upstream's "not found" is believed only once the dispatch is this old.
    pub trust_not_found_after: Duration,
    /// Past this age an unsettled reservation is flagged. It is not written off.
    pub escalate_after: Duration,
    pub accounting_rev: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOffReason {
    NoResponseId,
    NotFoundUpstream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Settled(Settlement),
    StillRunning,
    WrittenOff(WriteOffReason),
    RetryLater(String),
    /// Backing off, or too young to act on.
    Waiting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepEntry {
    pub reservation: ReservationId,
    pub outcome: Outcome,
    pub escalate: bool,
}

struct Backoff {
    next_attempt: Instant,
    delay: Duration,
}

pub struct RetrievalWorker {
    upstream: Arc<Upstream>,
    ledger: LedgerHandle,
    policy: RetrievalPolicy,
    backoff: HashMap<ReservationId, Backoff>,
}

impl RetrievalWorker {
    pub fn new(upstream: Arc<Upstream>, ledger: LedgerHandle, policy: RetrievalPolicy) -> Self {
        Self {
            upstream,
            ledger,
            policy,
            backoff: HashMap::new(),
        }
    }

    /// One pass over every open dispatch. Ages use `now_unix`; backoff uses
    /// the monotonic clock.
    pub async fn sweep(&mut self, now_unix: i64) -> Result<Vec<SweepEntry>, LedgerAccessError> {
        let open = self.ledger.with(|l| l.open_dispatches()).await?;
        let live: HashSet<ReservationId> = open.iter().map(|d| d.id).collect();
        self.backoff.retain(|id, _| live.contains(id));

        let mut entries = Vec::with_capacity(open.len());
        for dispatch in open {
            let age =
                Duration::from_secs(u64::try_from(now_unix - dispatch.created_at).unwrap_or(0));
            let outcome = self.handle(&dispatch, age, now_unix).await?;
            entries.push(SweepEntry {
                reservation: dispatch.id,
                outcome,
                escalate: age >= self.policy.escalate_after,
            });
        }
        Ok(entries)
    }

    async fn handle(
        &mut self,
        dispatch: &OpenDispatch,
        age: Duration,
        now_unix: i64,
    ) -> Result<Outcome, LedgerAccessError> {
        match (dispatch.state, &dispatch.response_id) {
            (State::DispatchedIdUnknown, _) if age >= self.policy.write_off_unknown_after => {
                self.write_off(dispatch.id, WriteOffReason::NoResponseId)
                    .await
            }
            (State::DispatchedWithId, Some(response_id)) if self.due(dispatch.id) => {
                self.retrieve(dispatch.id, response_id, age, now_unix).await
            }
            _ => Ok(Outcome::Waiting),
        }
    }

    async fn retrieve(
        &mut self,
        id: ReservationId,
        response_id: &str,
        age: Duration,
        now_unix: i64,
    ) -> Result<Outcome, LedgerAccessError> {
        let response = match self.upstream.retrieve(response_id).await {
            Ok(response) => response,
            Err(error) => return Ok(self.retry(id, error.to_string())),
        };
        let status = response.status().as_u16();
        if status == 404 && age >= self.policy.trust_not_found_after {
            return self.write_off(id, WriteOffReason::NotFoundUpstream).await;
        }
        if status != 200 {
            return Ok(self.retry(id, format!("upstream returned {status}")));
        }
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => return Ok(self.retry(id, error.to_string())),
        };
        let summary = serde_json::from_slice(&body)
            .ok()
            .and_then(|v| ResponseSummary::from_object(&v));
        let Some(summary) = summary.filter(|s| s.id == response_id) else {
            return Ok(self.retry(id, "the retrieved response could not be read".into()));
        };
        if !summary.is_terminal() {
            self.schedule(id);
            return Ok(Outcome::StillRunning);
        }

        let completed = CompletedResponse {
            usage: summary.usage.map(|u| quotamiser_ledger::Usage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
            }),
            model: summary.model.unwrap_or_default(),
            service_tier: summary.service_tier.unwrap_or_default(),
        };
        let rev = self.policy.accounting_rev;
        let settlement = self
            .ledger
            .with(move |l| l.settle(id, &completed, rev, now_unix))
            .await?;
        self.backoff.remove(&id);
        Ok(Outcome::Settled(settlement))
    }

    async fn write_off(
        &mut self,
        id: ReservationId,
        reason: WriteOffReason,
    ) -> Result<Outcome, LedgerAccessError> {
        self.ledger.with(move |l| l.write_off(id)).await?;
        self.backoff.remove(&id);
        Ok(Outcome::WrittenOff(reason))
    }

    fn due(&self, id: ReservationId) -> bool {
        self.backoff
            .get(&id)
            .is_none_or(|b| Instant::now() >= b.next_attempt)
    }

    fn schedule(&mut self, id: ReservationId) {
        let delay = match self.backoff.get(&id) {
            Some(previous) => (previous.delay * 2).min(self.policy.max_backoff),
            None => self.policy.initial_backoff,
        };
        self.backoff.insert(
            id,
            Backoff {
                next_attempt: Instant::now() + delay,
                delay,
            },
        );
    }

    fn retry(&mut self, id: ReservationId, why: String) -> Outcome {
        self.schedule(id);
        Outcome::RetryLater(why)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::Router;
    use axum::extract::Path as UrlPath;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use quotamiser_ledger::{Admission, Ledger, LedgerConfig, ReservationRequest};

    use super::*;
    use crate::upstream::UpstreamConfig;

    const NOW: i64 = 2_000;

    async fn retrieve(UrlPath(id): UrlPath<String>) -> axum::response::Response {
        let object = |status: &str, usage: serde_json::Value| {
            serde_json::json!({"id": id, "status": status, "model": "gpt-5.6-terra", "service_tier": "default", "usage": usage})
                .to_string()
        };
        match id.as_str() {
            "resp_done" => object(
                "completed",
                serde_json::json!({"input_tokens": 9, "output_tokens": 5}),
            )
            .into_response(),
            "resp_running" => object("in_progress", serde_json::Value::Null).into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }

    async fn worker(ledger: &LedgerHandle) -> RetrievalWorker {
        let router = Router::new().route("/v1/responses/{id}", get(retrieve));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let upstream = Upstream::new(UpstreamConfig {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: "test-key".into(),
            project: None,
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(5),
        })
        .unwrap();
        let policy = RetrievalPolicy {
            initial_backoff: Duration::from_secs(60),
            max_backoff: Duration::from_secs(600),
            write_off_unknown_after: Duration::from_secs(300),
            trust_not_found_after: Duration::from_secs(120),
            escalate_after: Duration::from_secs(900),
            accounting_rev: 1,
        };
        RetrievalWorker::new(Arc::new(upstream), ledger.clone(), policy)
    }

    fn open_ledger(dir: &Path) -> Ledger {
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
            .rollover(1, &[("openai:small".into(), 100_000)])
            .unwrap();
        ledger.adopt_recovered_consumption(&[]).unwrap();
        ledger
            .revalidate_safety_input("data_sharing", 0, i64::MAX / 2, 1_000_000)
            .unwrap();
        ledger
    }

    /// Reserves at `created_at`, dispatches, and records `response_id` or,
    /// with `None`, an unknown id.
    fn dispatched(
        ledger: &mut Ledger,
        created_at: i64,
        response_id: Option<&str>,
    ) -> ReservationId {
        let request = ReservationRequest {
            pool_id: "openai:small".into(),
            liability: 1_000,
            request_digest: "digest".into(),
            model_snapshot: "gpt-5.6-terra".into(),
            service_tier: "default".into(),
            accounting_rev: 1,
            required_safety_inputs: vec!["data_sharing".into()],
        };
        let Admission::Reserved(id) = ledger.reserve(&request, created_at).unwrap() else {
            panic!("not reserved")
        };
        ledger.begin_dispatch(id).unwrap();
        match response_id {
            Some(response_id) => ledger.attach_response_id(id, response_id).unwrap(),
            None => ledger.mark_id_unknown(id).unwrap(),
        }
        id
    }

    fn outcome_of(entries: &[SweepEntry], id: ReservationId) -> &SweepEntry {
        entries
            .iter()
            .find(|e| e.reservation == id)
            .expect("every open dispatch is reported")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sweep_settles_retries_and_writes_off_on_the_right_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(dir.path());
        let done = dispatched(&mut ledger, NOW - 60, Some("resp_done"));
        let running = dispatched(&mut ledger, NOW - 60, Some("resp_running"));
        let gone_young = dispatched(&mut ledger, NOW - 10, Some("resp_gone_young"));
        let gone_old = dispatched(&mut ledger, NOW - 1_000, Some("resp_gone_old"));
        let unknown_old = dispatched(&mut ledger, NOW - 1_000, None);
        let unknown_young = dispatched(&mut ledger, NOW - 10, None);
        let ledger = LedgerHandle::new(ledger);
        let mut worker = worker(&ledger).await;

        let entries = worker.sweep(NOW).await.unwrap();
        assert_eq!(entries.len(), 6);
        assert_eq!(
            outcome_of(&entries, done).outcome,
            Outcome::Settled(Settlement::Settled {
                actual: 14,
                overrun: false
            })
        );
        assert_eq!(outcome_of(&entries, running).outcome, Outcome::StillRunning);
        assert!(
            matches!(
                outcome_of(&entries, gone_young).outcome,
                Outcome::RetryLater(_)
            ),
            "a young not-found is not believed"
        );
        assert_eq!(
            outcome_of(&entries, gone_old).outcome,
            Outcome::WrittenOff(WriteOffReason::NotFoundUpstream)
        );
        assert_eq!(
            outcome_of(&entries, unknown_old).outcome,
            Outcome::WrittenOff(WriteOffReason::NoResponseId)
        );
        assert_eq!(
            outcome_of(&entries, unknown_young).outcome,
            Outcome::Waiting
        );
        assert!(outcome_of(&entries, gone_old).escalate);
        assert!(!outcome_of(&entries, done).escalate);

        let counters = ledger
            .with(|l| l.counters("openai:small", 1))
            .await
            .unwrap()
            .unwrap();
        // Settled for 14; two written off at 1,000; three still held at 1,000.
        assert_eq!((counters.consumed, counters.reserved), (14 + 2_000, 3_000));

        let again = worker.sweep(NOW).await.unwrap();
        assert_eq!(
            again.len(),
            3,
            "settled and written-off dispatches are gone"
        );
        assert_eq!(
            outcome_of(&again, running).outcome,
            Outcome::Waiting,
            "backing off"
        );
        assert_eq!(
            outcome_of(&again, gone_young).outcome,
            Outcome::Waiting,
            "backing off"
        );
    }
}
