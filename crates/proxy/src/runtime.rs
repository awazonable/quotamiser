//! Starting the proxy, and the loops that keep admission honest while it runs.
//!
//! Startup order matters. Trusted time comes first, because the epoch being
//! spent must be the epoch upstream will charge. The ledger is rolled over to
//! that epoch. Only then, and only if the ledger cannot be trusted, is the
//! day's consumption recovered from the provider's usage reporting — a ledger
//! recovered that way carries no fail-closed guarantee, and says so in the
//! log. Finally the data-sharing safety input is checked, without which
//! admission refuses everything.
//!
//! Three loops run afterwards: one re-reads the provider's clock and rolls the
//! ledger over at the boundary, one re-checks data sharing before its
//! verification expires, and one settles dispatches their streams could not.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use quotamiser_admission::epoch::{ClockPolicy, Window, epoch_of, window};
use quotamiser_admission::liability::EstimatorPolicy;
use quotamiser_ledger::{Ledger, RolloverOutcome, TrustVerdict};
use tokio::task::JoinHandle;

use crate::admission::{AdmissionPolicy, Admitter};
use crate::clock::{ClockError, TrustedClock};
use crate::config::{CatalogEntry, Resolved};
use crate::dispatch::{DispatchPolicy, Dispatcher};
use crate::failure_breaker::BreakerPolicy;
use crate::ledger_handle::{LedgerAccessError, LedgerHandle};
use crate::retrieval::{RetrievalPolicy, RetrievalWorker};
use crate::upstream::{Upstream, UpstreamError};
use crate::usage_api::{UsageApi, UsageApiError};

/// The safety input every admission depends on.
pub const DATA_SHARING: &str = "data_sharing";

/// Bumped when the meaning of recorded accounting changes.
const ACCOUNTING_REV: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("the upstream client could not be built: {0}")]
    Upstream(#[from] UpstreamError),
    #[error("the usage client could not be built: {0}")]
    Usage(#[from] UsageApiError),
    #[error("the ledger could not be opened: {0}")]
    Ledger(#[from] quotamiser_ledger::LedgerError),
    #[error("the ledger is not reachable: {0}")]
    LedgerAccess(#[from] LedgerAccessError),
    #[error(
        "no trusted time: the provider's clock could not be read, and the local clock is not trusted to choose the day ({0})"
    )]
    NoTrustedTime(#[from] ClockError),
    #[error("the ledger could not be recovered from usage reporting: {0}")]
    Recovery(String),
}

pub struct Runtime {
    ledger: LedgerHandle,
    clock: Arc<TrustedClock>,
    usage: Arc<UsageApi>,
    admitter: Admitter,
    dispatcher: Dispatcher,
    clock_policy: ClockPolicy,
    grants: Vec<(String, u64)>,
    catalog: HashMap<String, CatalogEntry>,
    data_sharing_ttl: i64,
    safety_budget: u64,
    tasks: Vec<JoinHandle<()>>,
}

impl Runtime {
    pub async fn start(resolved: Resolved) -> Result<Self, StartupError> {
        let upstream = Arc::new(Upstream::new(resolved.upstream)?);
        let usage = Arc::new(UsageApi::new(&resolved.usage_base_url, resolved.admin_key)?);
        let clock = Arc::new(TrustedClock::new(upstream.clone()));

        let (opened, assessment) = Ledger::open(&resolved.ledger)?;
        let verdict = opened.trust();
        let ledger = LedgerHandle::new(opened);
        log(&format!(
            "ledger opened: trust {verdict:?}{}",
            match assessment.promote_to_id_unknown {
                Some(id) => format!(", reservation {id} promoted to sent with an unknown id"),
                None => String::new(),
            }
        ));

        // Trusted time before anything is rolled over or admitted.
        let reading = clock.refresh().await?;
        let epoch = epoch_of(reading.upstream_unix);
        let outcome = roll_over(&ledger, epoch, resolved.grants.clone()).await?;
        log(&format!("epoch {epoch}: {outcome:?}"));

        if verdict != TrustVerdict::Trusted {
            log(
                "the ledger cannot be trusted; recovering today's consumption from usage reporting. Fail-closed is not claimed for a ledger recovered this way.",
            );
            recover(&ledger, &usage, epoch, &resolved.catalog).await?;
        }

        let smallest_grant = resolved
            .grants
            .iter()
            .map(|(_, granted)| *granted)
            .min()
            .unwrap_or(0);
        let safety_budget = smallest_grant / resolved.safety_budget_divisor;
        let verified = refresh_data_sharing(
            &ledger,
            &usage,
            epoch,
            reading.upstream_unix,
            resolved.data_sharing_ttl,
            safety_budget,
        )
        .await?;
        if !verified {
            log("data sharing is not verified; admission will refuse until it is.");
        }

        let admitter = Admitter::new(
            upstream.clone(),
            ledger.clone(),
            resolved.catalog.clone(),
            AdmissionPolicy {
                estimator: EstimatorPolicy::default(),
                accounting_rev: ACCOUNTING_REV,
                required_safety_inputs: vec![DATA_SHARING.to_string()],
            },
        );
        let dispatcher = Dispatcher::new(
            upstream.clone(),
            ledger.clone(),
            DispatchPolicy {
                id_wait_after_disconnect: Duration::from_secs(30),
                max_error_body_bytes: 64 * 1024,
                channel_capacity: 32,
                breaker: BreakerPolicy::default(),
            },
        );

        let mut runtime = Self {
            ledger,
            clock,
            usage,
            admitter,
            dispatcher,
            clock_policy: resolved.clock_policy,
            grants: resolved.grants,
            catalog: resolved.catalog,
            data_sharing_ttl: resolved.data_sharing_ttl,
            safety_budget,
            tasks: Vec::new(),
        };
        runtime.spawn_loops(upstream, resolved.clock_refresh);
        Ok(runtime)
    }

    fn spawn_loops(&mut self, upstream: Arc<Upstream>, clock_refresh: Duration) {
        let ledger = self.ledger.clone();
        let clock = self.clock.clone();
        let policy = self.clock_policy;
        let grants = self.grants.clone();
        self.tasks.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(clock_refresh).await;
                if let Err(error) = clock.refresh().await {
                    log(&format!("the provider's clock could not be read: {error}"));
                    continue;
                }
                match current_window(&ledger, &clock, &policy).await {
                    Ok(Window::RolloverDue { epoch }) => {
                        match roll_over(&ledger, epoch, grants.clone()).await {
                            Ok(outcome) => log(&format!("epoch {epoch}: {outcome:?}")),
                            Err(error) => log(&format!("rollover failed: {error}")),
                        }
                    }
                    Ok(_) => {}
                    Err(error) => log(&format!("the epoch could not be read: {error}")),
                }
            }
        }));

        let ledger = self.ledger.clone();
        let clock = self.clock.clone();
        let usage = self.usage.clone();
        let ttl = self.data_sharing_ttl;
        let budget = self.safety_budget;
        // Re-check well before the verification expires.
        let every = Duration::from_secs((ttl / 3).clamp(60, 3_600) as u64);
        self.tasks.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(reading) = clock.last_reading() else {
                    continue;
                };
                let now = reading.upstream_unix + (clock.monotonic_now() - reading.local_unix);
                if let Err(error) =
                    refresh_data_sharing(&ledger, &usage, epoch_of(now), now, ttl, budget).await
                {
                    log(&format!(
                        "the data sharing check could not be recorded: {error}"
                    ));
                }
            }
        }));

        let ledger = self.ledger.clone();
        let clock = self.clock.clone();
        let mut worker = RetrievalWorker::new(
            upstream,
            ledger,
            RetrievalPolicy {
                initial_backoff: Duration::from_secs(5),
                max_backoff: Duration::from_secs(60),
                write_off_unknown_after: Duration::from_secs(300),
                trust_not_found_after: Duration::from_secs(120),
                escalate_after: Duration::from_secs(900),
                accounting_rev: ACCOUNTING_REV,
            },
        );
        self.tasks.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let Some(reading) = clock.last_reading() else {
                    continue;
                };
                let now = reading.upstream_unix + (clock.monotonic_now() - reading.local_unix);
                match worker.sweep(now).await {
                    Ok(entries) => {
                        for entry in entries.iter().filter(|entry| entry.escalate) {
                            log(&format!(
                                "reservation {:?} is still unsettled: {:?}",
                                entry.reservation, entry.outcome
                            ));
                        }
                    }
                    Err(error) => log(&format!("the retrieval sweep failed: {error}")),
                }
            }
        }));
    }

    pub fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }

    pub fn admitter(&self) -> &Admitter {
        &self.admitter
    }

    pub fn catalog(&self) -> &HashMap<String, CatalogEntry> {
        &self.catalog
    }

    pub fn ledger(&self) -> &LedgerHandle {
        &self.ledger
    }

    /// Trusted time now: the last reading advanced by monotonic elapsed time.
    pub fn trusted_now(&self) -> Option<i64> {
        let reading = self.clock.last_reading()?;
        Some(reading.upstream_unix + (self.clock.monotonic_now() - reading.local_unix))
    }

    pub async fn window(&self) -> Result<Window, LedgerAccessError> {
        current_window(&self.ledger, &self.clock, &self.clock_policy).await
    }

    /// Opens `epoch` when the window says it is due, so a request arriving
    /// between two runs of the loop need not wait for the next one.
    pub async fn ensure_epoch(&self, epoch: i64) -> Result<RolloverOutcome, LedgerAccessError> {
        roll_over(&self.ledger, epoch, self.grants.clone()).await
    }

    /// Stops the loops and marks the ledger clean, so the next start is
    /// trusted and does not have to recover the day from usage reporting.
    ///
    /// The pool locks are held until the `Runtime` itself is dropped: nothing
    /// else may open this ledger before that. The binary exits instead.
    pub async fn shutdown(&self) {
        for task in &self.tasks {
            task.abort();
        }
        match self
            .ledger
            .with(|ledger| ledger.mark_clean_shutdown())
            .await
        {
            Ok(()) => log("ledger marked clean"),
            Err(error) => log(&format!("the ledger was not marked clean: {error}")),
        }
    }
}

/// Rolls the ledger over to `epoch`. Monotonic: an epoch at or before the
/// current one changes nothing.
pub async fn roll_over(
    ledger: &LedgerHandle,
    epoch: i64,
    grants: Vec<(String, u64)>,
) -> Result<RolloverOutcome, LedgerAccessError> {
    ledger
        .with(move |ledger| ledger.rollover(epoch, &grants))
        .await
}

/// Whether the current epoch's pool may be spent.
pub async fn current_window(
    ledger: &LedgerHandle,
    clock: &TrustedClock,
    policy: &ClockPolicy,
) -> Result<Window, LedgerAccessError> {
    let current = ledger.with(|ledger| ledger.current_epoch()).await?;
    Ok(window(
        policy,
        current,
        clock.monotonic_now(),
        clock.last_reading(),
    ))
}

/// Recovers a day's consumption for a ledger that cannot be trusted. Tokens
/// reported for a model outside the catalog are charged to every pool: which
/// pool paid for them is unknown, and over-counting is the safe direction.
pub async fn recover(
    ledger: &LedgerHandle,
    usage: &UsageApi,
    epoch: i64,
    catalog: &HashMap<String, CatalogEntry>,
) -> Result<(), StartupError> {
    let day = usage
        .day(epoch)
        .await
        .map_err(|error| StartupError::Recovery(error.to_string()))?;
    let mut by_pool: HashMap<String, u64> = HashMap::new();
    for entry in catalog.values() {
        by_pool.entry(entry.pool_id.clone()).or_default();
    }
    for (model, tokens) in &day.tokens_by_model {
        match catalog.get(model) {
            Some(entry) => *by_pool.entry(entry.pool_id.clone()).or_default() += tokens,
            None => {
                for total in by_pool.values_mut() {
                    *total += tokens;
                }
            }
        }
    }
    let adoptions: Vec<(String, i64, u64)> = by_pool
        .into_iter()
        .map(|(pool, tokens)| (pool, epoch, tokens))
        .collect();
    log(&format!("recovered consumption: {adoptions:?}"));
    ledger
        .with(move |ledger| ledger.adopt_recovered_consumption(&adoptions))
        .await?;
    Ok(())
}

/// Re-checks that this organization's traffic is still served from the grant,
/// and refreshes or withdraws the safety input accordingly. Returns whether
/// the verification now stands.
pub async fn refresh_data_sharing(
    ledger: &LedgerHandle,
    usage: &UsageApi,
    epoch: i64,
    now_unix: i64,
    ttl: i64,
    budget: u64,
) -> Result<bool, LedgerAccessError> {
    match usage.day(epoch).await {
        Ok(day) if day.requests_outside_the_grant == 0 => {
            if day.requests_of_unknown_tier > 0 {
                log(&format!(
                    "{} requests today were reported with no service tier",
                    day.requests_of_unknown_tier
                ));
            }
            ledger
                .with(move |ledger| {
                    ledger.revalidate_safety_input(DATA_SHARING, now_unix, ttl, budget)
                })
                .await?;
            Ok(true)
        }
        Ok(day) => {
            log(&format!(
                "data sharing check failed: {} requests today were served outside the grant; closing admission",
                day.requests_outside_the_grant
            ));
            ledger
                .with(|ledger| ledger.invalidate_safety_input(DATA_SHARING))
                .await?;
            Ok(false)
        }
        Err(error) => {
            log(&format!(
                "data sharing could not be checked ({error}); the previous verification is left to expire"
            ));
            Ok(false)
        }
    }
}

fn log(message: &str) {
    println!("[quotamiser] {message}");
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::SystemTime;

    use axum::Router;
    use axum::extract::State;
    use axum::http::header::DATE;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use quotamiser_admission::liability::ModelSpec;
    use quotamiser_ledger::{LedgerConfig, TrustVerdict};
    use serde_json::json;

    use super::*;
    use crate::config::Resolved;
    use crate::upstream::UpstreamConfig;

    const MODEL: &str = "gpt-5.6-terra";
    const POOL: &str = "openai:small";

    /// How many tokens the usage reporting will claim for today. The test sets
    /// it between starts.
    static REPORTED_TOKENS: Mutex<u64> = Mutex::new(0);

    async fn mock_provider() -> String {
        let app = Router::new()
            .route(
                "/v1/models",
                get(|| async {
                    (
                        [(DATE, httpdate::fmt_http_date(SystemTime::now()))],
                        axum::Json(json!({"object": "list", "data": []})),
                    )
                        .into_response()
                }),
            )
            .route(
                "/v1/organization/usage/completions",
                get(|State(()): State<()>| async {
                    let tokens = *REPORTED_TOKENS
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    axum::Json(json!({
                        "data": [{"start_time": 1, "end_time": 2, "results": [
                            {"model": MODEL, "service_tier": "incentivized-tier",
                             "num_model_requests": 1, "input_tokens": tokens, "output_tokens": 0}
                        ]}],
                        "has_more": false,
                        "next_page": null
                    }))
                }),
            )
            .with_state(());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}/v1")
    }

    fn ledger_config(dir: &Path) -> LedgerConfig {
        LedgerConfig {
            ledger_path: dir.join("ledger.db"),
            external_hwm_path: dir.join("external").join("hwm"),
            lock_dir: dir.join("locks"),
            organization: "org-test".into(),
            pools: vec![POOL.into()],
            allow_same_volume_external_record: true,
        }
    }

    fn resolved(base: &str, dir: &Path) -> Resolved {
        Resolved {
            bind: "127.0.0.1:0".parse().unwrap(),
            upstream: UpstreamConfig {
                base_url: base.to_string(),
                api_key: "sk-test".into(),
                project: None,
                connect_timeout: Duration::from_secs(2),
                read_timeout: Duration::from_secs(5),
            },
            admin_key: "sk-admin-test".into(),
            usage_base_url: base.to_string(),
            ledger: ledger_config(dir),
            clock_policy: ClockPolicy::new(5, 300, 900).unwrap(),
            // Long enough that no background loop runs during a test.
            clock_refresh: Duration::from_secs(3_600),
            data_sharing_ttl: 900,
            safety_budget_divisor: 4,
            grants: vec![(POOL.to_string(), 2_500_000)],
            catalog: HashMap::from([(
                MODEL.to_string(),
                CatalogEntry {
                    pool_id: POOL.into(),
                    spec: ModelSpec {
                        max_output_tokens: 128_000,
                        byte_level_encoding_known: false,
                    },
                },
            )]),
        }
    }

    fn set_reported(tokens: u64) {
        *REPORTED_TOKENS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = tokens;
    }

    async fn consumed(runtime: &Runtime, epoch: i64) -> i64 {
        runtime
            .ledger()
            .with(move |ledger| ledger.counters(POOL, epoch))
            .await
            .unwrap()
            .expect("the pool is open")
            .consumed
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fresh_ledger_is_recovered_from_usage_reporting_and_closed_clean() {
        let dir = tempfile::tempdir().unwrap();
        let base = mock_provider().await;
        set_reported(5_000);

        let runtime = Runtime::start(resolved(&base, dir.path())).await.unwrap();
        let now = runtime.trusted_now().expect("trusted time was read");
        let epoch = epoch_of(now);
        assert_eq!(
            runtime
                .ledger()
                .with(|ledger| ledger.current_epoch())
                .await
                .unwrap(),
            epoch,
            "the ledger is rolled over to the epoch trusted time names"
        );
        assert_eq!(
            consumed(&runtime, epoch).await,
            5_000,
            "an untrusted ledger adopts the day's reported consumption"
        );

        runtime.shutdown().await;
        // The pool lock outlives shutdown and is released on drop.
        drop(runtime);
        let (reopened, _) = Ledger::open(&ledger_config(dir.path())).unwrap();
        assert_eq!(
            reopened.trust(),
            TrustVerdict::Trusted,
            "a clean shutdown leaves the ledger trusted"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_trusted_ledger_is_not_re_adopted_from_usage_reporting() {
        let dir = tempfile::tempdir().unwrap();
        let base = mock_provider().await;
        set_reported(5_000);

        let runtime = Runtime::start(resolved(&base, dir.path())).await.unwrap();
        let epoch = epoch_of(runtime.trusted_now().unwrap());
        assert_eq!(consumed(&runtime, epoch).await, 5_000);
        runtime.shutdown().await;
        drop(runtime);

        // Usage reporting now claims more. A trusted ledger must not take it:
        // its own record is the inventory, and reporting never reopens capacity.
        set_reported(9_000);
        let runtime = Runtime::start(resolved(&base, dir.path())).await.unwrap();
        assert_eq!(
            consumed(&runtime, epoch).await,
            5_000,
            "a trusted ledger keeps its own consumption"
        );
        runtime.shutdown().await;
    }
}
