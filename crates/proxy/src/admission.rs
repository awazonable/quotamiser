//! Turning a normalized request into a reservation, or into a refusal.
//!
//! The order is the safety argument. The model must be in the catalog, so its
//! pool and maximum output are known. The epoch window must be open, so the
//! day being spent is the day upstream will charge. The liability must be an
//! upper bound: the structural bound when the request qualifies for it, and
//! otherwise the provider's counts, summed over every body the request needs
//! counted (ADR-0008). Only then does the ledger compare and reserve, and only
//! a reservation produces a permit to send.
//!
//! Nothing here can send a request. The permit it returns is the only way to,
//! and the dispatcher consumes it once.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use quotamiser_admission::epoch::{ClosedReason, Window};
use quotamiser_admission::liability::{
    self, EstimateError, EstimatorPolicy, InputShape, Liability,
};
use quotamiser_ledger::{Admission, Refusal, ReservationRequest};
use quotamiser_protocol::request::{CreateRequest, UPSTREAM_SERVICE_TIER};
use serde_json::Value;

use crate::config::CatalogEntry;
use crate::dispatch::DispatchPermit;
use crate::ledger_handle::LedgerHandle;
use crate::upstream::Upstream;

#[derive(Debug, Clone)]
pub struct AdmissionPolicy {
    pub estimator: EstimatorPolicy,
    /// Bumped when the meaning of recorded accounting changes; settlement
    /// refuses a reservation from another revision.
    pub accounting_rev: i64,
    /// Safety inputs every admission depends on, such as `data_sharing`.
    pub required_safety_inputs: Vec<String>,
}

/// Why a request was not admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Not a configured model, so neither its pool nor its output ceiling is
    /// known. Routing to a substitute is the router's job, not admission's.
    UnknownModel(String),
    /// The day's pool may not be spent at all right now.
    WindowClosed(ClosedReason),
    /// The ledger is still on an earlier epoch. The caller rolls it over and
    /// tries again.
    RolloverDue {
        epoch: i64,
    },
    /// The ledger refused: quota, a latch, or a safety input.
    Ledger(Refusal),
    /// The input could not be counted, so no upper bound exists.
    InputNotCounted(String),
    Estimate(EstimateError),
    LedgerUnavailable(String),
}

pub enum Decision {
    Admitted(DispatchPermit),
    Refused(Refused),
}

pub struct Admitter {
    upstream: Arc<Upstream>,
    ledger: LedgerHandle,
    catalog: HashMap<String, CatalogEntry>,
    policy: AdmissionPolicy,
}

impl Admitter {
    pub fn new(
        upstream: Arc<Upstream>,
        ledger: LedgerHandle,
        catalog: HashMap<String, CatalogEntry>,
        policy: AdmissionPolicy,
    ) -> Self {
        Self {
            upstream,
            ledger,
            catalog,
            policy,
        }
    }

    /// `now_unix` must be trusted time, and `window` the decision made from
    /// it. Both come from the caller so that one reading governs the whole
    /// admission.
    pub async fn admit(&self, request: &CreateRequest, now_unix: i64, window: Window) -> Decision {
        let Some(entry) = self.catalog.get(request.model()) else {
            return Decision::Refused(Refused::UnknownModel(request.model().to_owned()));
        };
        match window {
            Window::Open => {}
            Window::Closed(reason) => return Decision::Refused(Refused::WindowClosed(reason)),
            Window::RolloverDue { epoch } => {
                return Decision::Refused(Refused::RolloverDue { epoch });
            }
        }

        let shape = match request.flat_text() {
            Some(flat) => InputShape::FlatText {
                instructions: flat.instructions,
                input: flat.input,
            },
            None => InputShape::Structured,
        };
        let liability = match liability::estimate(
            &self.policy.estimator,
            &entry.spec,
            shape,
            request.max_output_tokens(),
        ) {
            Ok(Liability::Known(liability)) => liability,
            Ok(Liability::NeedsExactInput { output_bound }) => {
                let mut input_bound: u64 = 0;
                for body in request.input_count_bodies() {
                    match self.count_input(body).await {
                        Ok(count) => match input_bound.checked_add(count) {
                            Some(sum) => input_bound = sum,
                            None => {
                                return Decision::Refused(Refused::Estimate(
                                    EstimateError::Overflow,
                                ));
                            }
                        },
                        Err(error) => return Decision::Refused(Refused::InputNotCounted(error)),
                    }
                }
                match liability::with_exact_input(input_bound, output_bound) {
                    Ok(liability) => liability,
                    Err(error) => return Decision::Refused(Refused::Estimate(error)),
                }
            }
            Err(error) => return Decision::Refused(Refused::Estimate(error)),
        };

        let body = Bytes::from(request.openai_body());
        let reservation = ReservationRequest {
            pool_id: entry.pool_id.clone(),
            liability,
            request_digest: digest(&body),
            model_snapshot: request.model().to_owned(),
            service_tier: UPSTREAM_SERVICE_TIER.to_owned(),
            accounting_rev: self.policy.accounting_rev,
            required_safety_inputs: self.policy.required_safety_inputs.clone(),
        };
        match self
            .ledger
            .with(move |ledger| ledger.reserve(&reservation, now_unix))
            .await
        {
            Ok(Admission::Reserved(id)) => {
                Decision::Admitted(DispatchPermit::new(id, body, self.policy.accounting_rev))
            }
            Ok(Admission::Refused(refusal)) => Decision::Refused(Refused::Ledger(refusal)),
            Err(error) => Decision::Refused(Refused::LedgerUnavailable(error.to_string())),
        }
    }

    /// One call to the provider's counter. Measured to cost nothing and to
    /// consume no quota.
    async fn count_input(&self, body: Value) -> Result<u64, String> {
        let bytes = Bytes::from(serde_json::to_vec(&body).map_err(|error| error.to_string())?);
        let response = self
            .upstream
            .count_input_tokens(bytes)
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let text = response.text().await.map_err(|error| error.to_string())?;
        if !status.is_success() {
            let detail: String = text.chars().take(300).collect();
            return Err(format!("the counter answered {status}: {detail}"));
        }
        serde_json::from_str::<Value>(&text)
            .ok()
            .as_ref()
            .and_then(|parsed| parsed.get("input_tokens"))
            .and_then(Value::as_u64)
            .ok_or_else(|| "the counter's answer carried no input_tokens".to_owned())
    }
}

/// Binds the reservation to the bytes its liability was computed for. Not a
/// cryptographic commitment: it catches a permit paired with the wrong body,
/// which is a programming error, not an attack.
fn digest(body: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in body {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}:{}", body.len())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::Router;
    use axum::extract::State;
    use axum::routing::post;
    use quotamiser_admission::liability::ModelSpec;
    use quotamiser_ledger::{Ledger, LedgerConfig, ReservationId, State as ReservationState};
    use serde_json::json;

    use super::*;
    use crate::upstream::UpstreamConfig;

    const POOL: &str = "openai:small";
    const GRANT: u64 = 300_000;
    const COUNT: u64 = 1_000;

    struct Counter {
        calls: AtomicUsize,
        fails: bool,
    }

    async fn mock_counter(fails: bool) -> (Arc<Counter>, u16) {
        let counter = Arc::new(Counter {
            calls: AtomicUsize::new(0),
            fails,
        });
        let app = Router::new()
            .route(
                "/v1/responses/input_tokens",
                post(
                    |State(counter): State<Arc<Counter>>, _body: String| async move {
                        counter.calls.fetch_add(1, Ordering::SeqCst);
                        if counter.fails {
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(json!({"error": {"message": "no"}})),
                            );
                        }
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(json!({"input_tokens": COUNT})),
                        )
                    },
                ),
            )
            .with_state(counter.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (counter, port)
    }

    fn ledger(dir: &Path, grant: u64) -> LedgerHandle {
        let config = LedgerConfig {
            ledger_path: dir.join("ledger.db"),
            external_hwm_path: dir.join("external").join("hwm"),
            lock_dir: dir.join("locks"),
            organization: "org-test".into(),
            pools: vec![POOL.into()],
            allow_same_volume_external_record: true,
        };
        let (mut ledger, _) = Ledger::open(&config).unwrap();
        ledger.rollover(1, &[(POOL.into(), grant)]).unwrap();
        ledger.adopt_recovered_consumption(&[]).unwrap();
        ledger
            .revalidate_safety_input("data_sharing", 0, i64::MAX / 2, GRANT)
            .unwrap();
        LedgerHandle::new(ledger)
    }

    fn admitter(port: u16, ledger: &LedgerHandle, byte_level_encoding_known: bool) -> Admitter {
        let upstream = Upstream::new(UpstreamConfig {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: "test-key".into(),
            project: None,
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(5),
        })
        .unwrap();
        let catalog = HashMap::from([(
            "gpt-5.6-terra".to_string(),
            CatalogEntry {
                pool_id: POOL.into(),
                spec: ModelSpec {
                    max_output_tokens: 128_000,
                    byte_level_encoding_known,
                },
            },
        )]);
        Admitter::new(
            Arc::new(upstream),
            ledger.clone(),
            catalog,
            AdmissionPolicy {
                estimator: EstimatorPolicy::default(),
                accounting_rev: 1,
                required_safety_inputs: vec!["data_sharing".into()],
            },
        )
    }

    fn parse(value: Value) -> CreateRequest {
        CreateRequest::parse(&serde_json::to_vec(&value).unwrap()).unwrap()
    }

    fn flat(max_output: u64) -> CreateRequest {
        parse(json!({
            "model": "gpt-5.6-terra",
            "input": "hello",
            "stream": true,
            "max_output_tokens": max_output,
        }))
    }

    /// Two bodies to count: the request, and its additional_tools item.
    fn lite() -> CreateRequest {
        parse(json!({
            "model": "gpt-5.6-terra",
            "instructions": "",
            "stream": true,
            "max_output_tokens": 2_000,
            "input": [
                {"type": "additional_tools", "role": "developer", "tools": [
                    {"type": "function", "name": "exec", "parameters": {"type": "object"}}
                ]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "run ls"}]}
            ]
        }))
    }

    async fn reserved_liability(ledger: &LedgerHandle) -> i64 {
        ledger
            .with(|l| l.counters(POOL, 1))
            .await
            .unwrap()
            .unwrap()
            .reserved
    }

    async fn state(ledger: &LedgerHandle, id: ReservationId) -> ReservationState {
        ledger.with(move |l| l.reservation_state(id)).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_flat_request_is_reserved_from_the_structural_bound_without_counting() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(dir.path(), GRANT);
        let (counter, port) = mock_counter(false).await;
        let admitter = admitter(port, &ledger, true);

        let Decision::Admitted(permit) = admitter.admit(&flat(2_000), 10, Window::Open).await
        else {
            panic!("expected an admission")
        };
        assert_eq!(
            counter.calls.load(Ordering::SeqCst),
            0,
            "the structural bound needs no call"
        );
        // "hello" is five bytes, with one node's framing allowance, plus the cap.
        assert_eq!(reserved_liability(&ledger).await, 5 + 64 + 2_000);
        assert_eq!(
            state(&ledger, permit.reservation()).await,
            ReservationState::Reserved
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_structured_request_reserves_the_sum_of_every_counted_body() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(dir.path(), GRANT);
        let (counter, port) = mock_counter(false).await;
        let admitter = admitter(port, &ledger, false);

        let request = lite();
        assert_eq!(request.input_count_bodies().len(), 2);
        let Decision::Admitted(_) = admitter.admit(&request, 10, Window::Open).await else {
            panic!("expected an admission")
        };
        assert_eq!(counter.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            reserved_liability(&ledger).await,
            i64::try_from(COUNT * 2 + 2_000).unwrap(),
            "both counts are reserved, not just the request's own"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_model_is_refused_before_anything_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(dir.path(), GRANT);
        let (counter, port) = mock_counter(false).await;
        let admitter = admitter(port, &ledger, false);

        let request = parse(json!({"model": "gpt-5.6-luna", "input": "hi", "stream": true}));
        let Decision::Refused(refused) = admitter.admit(&request, 10, Window::Open).await else {
            panic!("expected a refusal")
        };
        assert_eq!(refused, Refused::UnknownModel("gpt-5.6-luna".into()));
        assert_eq!(counter.calls.load(Ordering::SeqCst), 0);
        assert_eq!(reserved_liability(&ledger).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_closed_window_reserves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(dir.path(), GRANT);
        let (counter, port) = mock_counter(false).await;
        let admitter = admitter(port, &ledger, true);

        for (window, expected) in [
            (
                Window::Closed(ClosedReason::NoTrustedTime),
                Refused::WindowClosed(ClosedReason::NoTrustedTime),
            ),
            (
                Window::Closed(ClosedReason::NearBoundary),
                Refused::WindowClosed(ClosedReason::NearBoundary),
            ),
            (
                Window::RolloverDue { epoch: 2 },
                Refused::RolloverDue { epoch: 2 },
            ),
        ] {
            let Decision::Refused(refused) = admitter.admit(&flat(10), 10, window).await else {
                panic!("expected a refusal")
            };
            assert_eq!(refused, expected);
        }
        assert_eq!(counter.calls.load(Ordering::SeqCst), 0);
        assert_eq!(reserved_liability(&ledger).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_liability_over_the_remaining_grant_is_refused_by_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(dir.path(), 1_500);
        let (_counter, port) = mock_counter(false).await;
        let admitter = admitter(port, &ledger, true);

        let Decision::Refused(Refused::Ledger(Refusal::InsufficientQuota {
            remaining,
            liability,
        })) = admitter.admit(&flat(2_000), 10, Window::Open).await
        else {
            panic!("expected the ledger to refuse")
        };
        assert_eq!(remaining, 1_500);
        assert_eq!(liability, 5 + 64 + 2_000);
        assert_eq!(reserved_liability(&ledger).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_uncountable_input_is_refused_rather_than_estimated() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(dir.path(), GRANT);
        let (counter, port) = mock_counter(true).await;
        let admitter = admitter(port, &ledger, false);

        let Decision::Refused(Refused::InputNotCounted(detail)) =
            admitter.admit(&lite(), 10, Window::Open).await
        else {
            panic!("expected a refusal")
        };
        assert!(detail.contains("500"), "{detail}");
        assert_eq!(
            counter.calls.load(Ordering::SeqCst),
            1,
            "it stops at the first failure"
        );
        assert_eq!(reserved_liability(&ledger).await, 0);
    }

    #[test]
    fn the_digest_binds_the_body() {
        let a = digest(b"{\"model\":\"m\"}");
        assert_eq!(a, digest(b"{\"model\":\"m\"}"));
        assert_ne!(a, digest(b"{\"model\":\"n\"}"));
        assert!(a.ends_with(":13"), "{a}");
    }
}
