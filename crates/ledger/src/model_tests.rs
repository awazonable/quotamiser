//! Model-based tests of the ledger against real SQLite.
//!
//! The oracle is a ghost model of every request's maximum upstream
//! consumption, per pool and epoch. The ledger's counters are checked against
//! the ghost, and the ghost is checked against the grant. A test that only
//! inspected the stored `consumed + reserved <= granted` would have passed the
//! unsound design ADR-0003 replaced.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use proptest::prelude::*;

use crate::{
    Admission, Assessment, CompletedResponse, ExternalHwm, HighWaterMark, Ledger, LedgerConfig,
    LedgerError, Refusal, ReservationId, ReservationRequest, RolloverOutcome, Settlement, State,
    TrustVerdict, UnknownReason, Usage,
};

const POOLS: [&str; 2] = ["openai:large", "openai:small"];
const GRANTS: [u64; 2] = [1_000, 5_000];
const MODELS: [&str; 2] = ["sol-snapshot", "terra-snapshot"];
const TIER: &str = "default";
const REV: i64 = 1;
const SAFETY: &str = "data_sharing";
const TTL: i64 = 1_000_000_000;
const BUDGET: u64 = 3_000;

fn config(dir: &Path) -> LedgerConfig {
    LedgerConfig {
        ledger_path: dir.join("ledger.db"),
        external_hwm_path: dir.join("external").join("hwm"),
        lock_dir: dir.join("locks"),
        organization: "org-test".into(),
        pools: POOLS.iter().map(|p| p.to_string()).collect(),
        // Both records share one temp directory, which production refuses.
        allow_same_volume_external_record: true,
    }
}

#[test]
fn an_external_record_on_the_ledger_volume_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = LedgerConfig {
        allow_same_volume_external_record: false,
        ..config(dir.path())
    };
    assert!(matches!(
        Ledger::open(&config),
        Err(LedgerError::StorageContract(_))
    ));
}

fn grants() -> Vec<(String, u64)> {
    POOLS
        .iter()
        .zip(GRANTS)
        .map(|(p, g)| (p.to_string(), g))
        .collect()
}

/// A first run: uninitialized, then today's epoch, then an empty recovery.
fn bootstrap(dir: &Path, budget: u64) -> Ledger {
    let (mut ledger, assessment) = Ledger::open(&config(dir)).unwrap();
    assert_eq!(
        assessment.verdict,
        TrustVerdict::Unknown(UnknownReason::Uninitialized)
    );
    ledger.rollover(1, &grants()).unwrap();
    ledger.adopt_recovered_consumption(&[]).unwrap();
    ledger
        .revalidate_safety_input(SAFETY, 0, TTL, budget)
        .unwrap();
    ledger
}

fn request(pool: usize, liability: u64) -> ReservationRequest {
    ReservationRequest {
        pool_id: POOLS[pool].into(),
        liability,
        request_digest: "digest".into(),
        model_snapshot: MODELS[pool].into(),
        service_tier: TIER.into(),
        accounting_rev: REV,
        required_safety_inputs: vec![SAFETY.into()],
    }
}

fn response(pool: usize, usage: Option<(i64, i64)>) -> CompletedResponse {
    CompletedResponse {
        usage: usage.map(|(input_tokens, output_tokens)| Usage {
            input_tokens,
            output_tokens,
        }),
        model: MODELS[pool].into(),
        service_tier: TIER.into(),
    }
}

fn reserved(admission: Admission) -> ReservationId {
    match admission {
        Admission::Reserved(id) => id,
        other => panic!("expected a reservation, got {other:?}"),
    }
}

fn reopen(dir: &Path, ledger: Ledger, clean: bool) -> (Ledger, Assessment) {
    if clean {
        ledger.close().unwrap();
    } else {
        drop(ledger);
    }
    Ledger::open(&config(dir)).unwrap()
}

#[test]
fn an_untrusted_ledger_admits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ledger, _) = Ledger::open(&config(dir.path())).unwrap();
    ledger.rollover(1, &grants()).unwrap();
    assert!(matches!(
        ledger.reserve(&request(1, 1), 1).unwrap(),
        Admission::Refused(Refusal::Untrusted(_))
    ));
}

#[test]
fn a_second_ledger_for_the_same_pools_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let _first = bootstrap(dir.path(), BUDGET);
    assert!(matches!(
        Ledger::open(&config(dir.path())),
        Err(LedgerError::Locked)
    ));
}

#[test]
fn the_liability_not_an_estimate_controls_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = bootstrap(dir.path(), 1_000_000);
    reserved(ledger.reserve(&request(0, 400), 1).unwrap());
    reserved(ledger.reserve(&request(0, 400), 2).unwrap());
    assert_eq!(
        ledger.reserve(&request(0, 400), 3).unwrap(),
        Admission::Refused(Refusal::InsufficientQuota {
            remaining: 200,
            liability: 400
        })
    );
}

#[test]
fn rollover_reserves_the_carried_liability_in_the_new_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = bootstrap(dir.path(), 1_000_000);
    let id = reserved(ledger.reserve(&request(0, 600), 1).unwrap());
    ledger.begin_dispatch(id).unwrap();

    assert_eq!(
        ledger.rollover(2, &grants()).unwrap(),
        RolloverOutcome::Advanced { carried: 1 }
    );
    assert_eq!(ledger.counters(POOLS[0], 2).unwrap().unwrap().reserved, 600);
    assert!(matches!(
        ledger.reserve(&request(0, 500), 2).unwrap(),
        Admission::Refused(Refusal::InsufficientQuota { remaining: 400, .. })
    ));

    assert_eq!(
        ledger.rollover(2, &grants()).unwrap(),
        RolloverOutcome::AlreadyCurrent
    );
    assert_eq!(ledger.counters(POOLS[0], 2).unwrap().unwrap().reserved, 600);

    ledger.attach_response_id(id, "resp_1").unwrap();
    ledger
        .settle(id, &response(0, Some((60, 40))), REV, 3)
        .unwrap();
    for epoch in [1, 2] {
        let counters = ledger.counters(POOLS[0], epoch).unwrap().unwrap();
        assert_eq!(
            (counters.reserved, counters.consumed),
            (0, 100),
            "epoch {epoch}"
        );
    }
}

#[test]
fn untrustworthy_usage_is_written_off_and_latches_the_pool() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = bootstrap(dir.path(), 1_000_000);
    let id = reserved(ledger.reserve(&request(1, 300), 1).unwrap());
    ledger.begin_dispatch(id).unwrap();
    assert_eq!(
        ledger.settle(id, &response(1, None), REV, 2).unwrap(),
        Settlement::WrittenOff(crate::UsageDefect::Missing)
    );
    assert_eq!(ledger.counters(POOLS[1], 1).unwrap().unwrap().consumed, 300);
    assert!(matches!(
        ledger.reserve(&request(1, 1), 3).unwrap(),
        Admission::Refused(Refusal::Latched { .. })
    ));
}

#[test]
fn an_overrun_is_recorded_as_reported_and_latches_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = bootstrap(dir.path(), 1_000_000);
    let id = reserved(ledger.reserve(&request(0, 100), 1).unwrap());
    ledger.begin_dispatch(id).unwrap();
    assert_eq!(
        ledger
            .settle(id, &response(0, Some((80, 40))), REV, 2)
            .unwrap(),
        Settlement::Settled {
            actual: 120,
            overrun: true
        }
    );
    assert_eq!(ledger.counters(POOLS[0], 1).unwrap().unwrap().consumed, 120);
    match ledger.reserve(&request(0, 1), 3).unwrap() {
        Admission::Refused(Refusal::Latched { scope, .. }) => {
            assert_eq!(scope, "model:sol-snapshot")
        }
        other => panic!("expected a model latch, got {other:?}"),
    }
}

#[test]
fn the_safety_budget_is_debited_not_merely_checked() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = bootstrap(dir.path(), 10);
    reserved(ledger.reserve(&request(1, 9), 1).unwrap());
    assert_eq!(
        ledger.reserve(&request(1, 9), 2).unwrap(),
        Admission::Refused(Refusal::SafetyBudgetExhausted {
            key: SAFETY.into(),
            remaining: 1
        })
    );
}

#[test]
fn an_expired_or_backdated_safety_input_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = bootstrap(dir.path(), 1_000);
    ledger
        .revalidate_safety_input(SAFETY, 100, 50, 1_000)
        .unwrap();
    for now in [150, 99] {
        assert!(matches!(
            ledger.reserve(&request(1, 1), now).unwrap(),
            Admission::Refused(Refusal::SafetyInputExpired(_))
        ));
    }
}

#[test]
fn a_restored_snapshot_is_detected_as_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let settle_one = |ledger: &mut Ledger, now: i64| {
        let id = reserved(ledger.reserve(&request(1, 50), now).unwrap());
        ledger.begin_dispatch(id).unwrap();
        ledger.attach_response_id(id, "resp").unwrap();
        ledger
            .settle(id, &response(1, Some((10, 10))), REV, now)
            .unwrap();
    };
    let sidecars = |base: &Path| [base.to_path_buf(), base.with_extension("db-wal")];

    let mut ledger = bootstrap(dir.path(), 1_000_000);
    settle_one(&mut ledger, 1);
    ledger.close().unwrap();
    let snapshot = dir.path().join("snapshot.db");
    for (from, to) in sidecars(&cfg.ledger_path).iter().zip(sidecars(&snapshot)) {
        if from.exists() {
            std::fs::copy(from, to).unwrap();
        }
    }

    let (mut ledger, assessment) = Ledger::open(&cfg).unwrap();
    assert_eq!(assessment.verdict, TrustVerdict::Trusted);
    settle_one(&mut ledger, 2);
    ledger.close().unwrap();

    for (live, saved) in sidecars(&cfg.ledger_path).iter().zip(sidecars(&snapshot)) {
        let _ = std::fs::remove_file(live);
        if saved.exists() {
            std::fs::copy(saved, live).unwrap();
        }
    }
    let _ = std::fs::remove_file(cfg.ledger_path.with_extension("db-shm"));
    let (_ledger, assessment) = Ledger::open(&cfg).unwrap();
    assert_eq!(
        assessment.verdict,
        TrustVerdict::Unknown(UnknownReason::LedgerRolledBack)
    );
}

// ---------------------------------------------------------------------------
// Model-based property test
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Cmd {
    Reserve {
        pool: usize,
        liability: u64,
    },
    BeginDispatch(usize),
    AttachId(usize),
    MarkIdUnknown(usize),
    Settle {
        pick: usize,
        permille: u16,
        overrun: bool,
    },
    SettleInvalid(usize),
    WriteOff(usize),
    ReleaseUnsent(usize),
    RejectBeforeProcessing(usize),
    Rollover,
    Revalidate,
    CrashAfterExternalWrite(usize),
    Restart {
        clean: bool,
    },
}

fn cmd() -> impl Strategy<Value = Cmd> {
    prop_oneof![
        5 => (0..2usize, 1..=700u64).prop_map(|(pool, liability)| Cmd::Reserve { pool, liability }),
        4 => any::<usize>().prop_map(Cmd::BeginDispatch),
        2 => any::<usize>().prop_map(Cmd::AttachId),
        1 => any::<usize>().prop_map(Cmd::MarkIdUnknown),
        3 => (any::<usize>(), 0..=1000u16, prop::bool::weighted(0.05))
            .prop_map(|(pick, permille, overrun)| Cmd::Settle { pick, permille, overrun }),
        1 => any::<usize>().prop_map(Cmd::SettleInvalid),
        1 => any::<usize>().prop_map(Cmd::WriteOff),
        1 => any::<usize>().prop_map(Cmd::ReleaseUnsent),
        1 => any::<usize>().prop_map(Cmd::RejectBeforeProcessing),
        1 => Just(Cmd::Rollover),
        1 => Just(Cmd::Revalidate),
        1 => any::<usize>().prop_map(Cmd::CrashAfterExternalWrite),
        1 => any::<bool>().prop_map(|clean| Cmd::Restart { clean }),
    ]
}

#[derive(Debug, Clone)]
struct Ghost {
    id: ReservationId,
    pool: usize,
    liability: i64,
    epochs: Vec<i64>,
    state: State,
    settled: Option<i64>,
}

#[derive(Debug, Default)]
struct Model {
    requests: Vec<Ghost>,
    epoch: i64,
    budget: i64,
    latched_pools: BTreeSet<usize>,
    latched_models: BTreeSet<usize>,
    overrun_excess: BTreeMap<(usize, i64), i64>,
}

impl Model {
    fn charged(&self, pool: usize, epoch: i64) -> impl Iterator<Item = &Ghost> {
        self.requests
            .iter()
            .filter(move |r| r.pool == pool && r.epochs.contains(&epoch))
    }
    fn reserved(&self, pool: usize, epoch: i64) -> i64 {
        self.charged(pool, epoch)
            .filter(|r| r.state.is_active())
            .map(|r| r.liability)
            .sum()
    }
    fn consumed(&self, pool: usize, epoch: i64) -> i64 {
        self.charged(pool, epoch)
            .map(|r| match r.state {
                State::Settled => r.settled.unwrap(),
                State::ConsumedUnrecoverable => r.liability,
                _ => 0,
            })
            .sum()
    }
    fn pick(&self, n: usize) -> Option<usize> {
        (!self.requests.is_empty()).then(|| n % self.requests.len())
    }
    fn expected_refusal(&self, pool: usize, liability: i64) -> Option<&'static str> {
        if self.latched_pools.contains(&pool) || self.latched_models.contains(&pool) {
            return Some("latched");
        }
        if self.budget < liability {
            return Some("budget");
        }
        let headroom =
            GRANTS[pool] as i64 - self.reserved(pool, self.epoch) - self.consumed(pool, self.epoch);
        (headroom < liability).then_some("quota")
    }
    fn normalize_after_restart(&mut self, promoted: Option<ReservationId>) {
        for r in &mut self.requests {
            if Some(r.id) == promoted {
                assert_eq!(r.state, State::Reserved);
                r.state = State::DispatchedIdUnknown;
            } else if r.state == State::Dispatching {
                r.state = State::DispatchedIdUnknown;
            } else if r.state == State::Reserved {
                r.state = State::ReleasedUnsent;
            }
        }
    }
}

fn refusal_kind(refusal: &Refusal) -> &'static str {
    match refusal {
        Refusal::Untrusted(_) => "untrusted",
        Refusal::Latched { .. } => "latched",
        Refusal::SafetyInputMissing(_) => "safety-missing",
        Refusal::SafetyInputExpired(_) => "safety-expired",
        Refusal::SafetyBudgetExhausted { .. } => "budget",
        Refusal::PoolNotOpen { .. } => "pool-not-open",
        Refusal::InsufficientQuota { .. } => "quota",
    }
}

fn expect_legal<T>(result: crate::Result<T>, legal: bool) -> Option<T> {
    match (result, legal) {
        (Ok(value), true) => Some(value),
        (Err(LedgerError::IllegalTransition { .. }), false) => None,
        (Ok(_), false) => panic!("the ledger allowed a transition the model forbids"),
        (Err(err), _) => panic!("unexpected ledger error (legal = {legal}): {err}"),
    }
}

fn check(ledger: &Ledger, model: &Model) {
    ledger.check_integrity().unwrap();
    for pool in 0..POOLS.len() {
        for epoch in 1..=model.epoch {
            let counters = ledger.counters(POOLS[pool], epoch).unwrap().unwrap();
            let (ghost_reserved, ghost_consumed) =
                (model.reserved(pool, epoch), model.consumed(pool, epoch));
            assert_eq!(
                counters.reserved, ghost_reserved,
                "reserved, pool {pool} epoch {epoch}"
            );
            assert_eq!(
                counters.consumed, ghost_consumed,
                "consumed, pool {pool} epoch {epoch}"
            );
            // The property that costs money: everything that may reach
            // upstream fits the grant, except usage the provider reported
            // beyond its own limit, which latches.
            let excess = model
                .overrun_excess
                .get(&(pool, epoch))
                .copied()
                .unwrap_or(0);
            assert!(
                ghost_reserved + ghost_consumed <= GRANTS[pool] as i64 + excess,
                "pool {pool} epoch {epoch}: {ghost_reserved} + {ghost_consumed} exceeds the grant"
            );
        }
    }
}

fn run(cmds: Vec<Cmd>) {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = bootstrap(dir.path(), BUDGET);
    let mut model = Model {
        epoch: 1,
        budget: BUDGET as i64,
        ..Model::default()
    };

    for (step, cmd) in cmds.into_iter().enumerate() {
        let now = step as i64 + 1;
        match cmd {
            Cmd::Reserve { pool, liability } => {
                let expected = model.expected_refusal(pool, liability as i64);
                match ledger.reserve(&request(pool, liability), now).unwrap() {
                    Admission::Reserved(id) => {
                        assert_eq!(expected, None, "the ledger admitted what the model refuses");
                        model.budget -= liability as i64;
                        model.requests.push(Ghost {
                            id,
                            pool,
                            liability: liability as i64,
                            epochs: vec![model.epoch],
                            state: State::Reserved,
                            settled: None,
                        });
                    }
                    Admission::Refused(refusal) => {
                        assert_eq!(Some(refusal_kind(&refusal)), expected, "{refusal:?}")
                    }
                }
            }
            Cmd::BeginDispatch(n) => {
                if let Some(i) = model.pick(n) {
                    let legal = model.requests[i].state == State::Reserved;
                    if expect_legal(ledger.begin_dispatch(model.requests[i].id), legal).is_some() {
                        model.requests[i].state = State::Dispatching;
                    }
                }
            }
            Cmd::AttachId(n) => {
                if let Some(i) = model.pick(n) {
                    let legal = model.requests[i].state == State::Dispatching;
                    let id = model.requests[i].id;
                    if expect_legal(
                        ledger.attach_response_id(id, &format!("resp_{}", id.0)),
                        legal,
                    )
                    .is_some()
                    {
                        model.requests[i].state = State::DispatchedWithId;
                    }
                }
            }
            Cmd::MarkIdUnknown(n) => {
                if let Some(i) = model.pick(n) {
                    let legal = model.requests[i].state == State::Dispatching;
                    if expect_legal(ledger.mark_id_unknown(model.requests[i].id), legal).is_some() {
                        model.requests[i].state = State::DispatchedIdUnknown;
                    }
                }
            }
            Cmd::Settle {
                pick,
                permille,
                overrun,
            } => {
                if let Some(i) = model.pick(pick) {
                    let r = model.requests[i].clone();
                    let legal = matches!(r.state, State::Dispatching | State::DispatchedWithId);
                    let actual = if overrun {
                        r.liability + 1 + i64::from(permille)
                    } else {
                        r.liability * i64::from(permille) / 1000
                    };
                    let usage = Some((actual / 2, actual - actual / 2));
                    if let Some(outcome) = expect_legal(
                        ledger.settle(r.id, &response(r.pool, usage), REV, now),
                        legal,
                    ) {
                        assert_eq!(
                            outcome,
                            Settlement::Settled {
                                actual,
                                overrun: actual > r.liability
                            }
                        );
                        model.requests[i].state = State::Settled;
                        model.requests[i].settled = Some(actual);
                        if actual > r.liability {
                            model.latched_models.insert(r.pool);
                            for epoch in &r.epochs {
                                *model.overrun_excess.entry((r.pool, *epoch)).or_default() +=
                                    actual - r.liability;
                            }
                        }
                    }
                }
            }
            Cmd::SettleInvalid(n) => {
                if let Some(i) = model.pick(n) {
                    let r = model.requests[i].clone();
                    let legal = matches!(r.state, State::Dispatching | State::DispatchedWithId);
                    if expect_legal(
                        ledger.settle(r.id, &response(r.pool, None), REV, now),
                        legal,
                    )
                    .is_some()
                    {
                        model.requests[i].state = State::ConsumedUnrecoverable;
                        model.latched_pools.insert(r.pool);
                    }
                }
            }
            Cmd::WriteOff(n) => {
                if let Some(i) = model.pick(n) {
                    let legal = model.requests[i].state.possibly_sent();
                    if expect_legal(ledger.write_off(model.requests[i].id), legal).is_some() {
                        model.requests[i].state = State::ConsumedUnrecoverable;
                    }
                }
            }
            Cmd::ReleaseUnsent(n) => {
                if let Some(i) = model.pick(n) {
                    let legal = model.requests[i].state == State::Reserved;
                    if expect_legal(ledger.release_unsent(model.requests[i].id), legal).is_some() {
                        model.requests[i].state = State::ReleasedUnsent;
                    }
                }
            }
            Cmd::RejectBeforeProcessing(n) => {
                if let Some(i) = model.pick(n) {
                    let legal = model.requests[i].state == State::Dispatching;
                    if expect_legal(ledger.release_rejected(model.requests[i].id), legal).is_some()
                    {
                        model.requests[i].state = State::RejectedBeforeProcessing;
                    }
                }
            }
            Cmd::Rollover => {
                model.epoch += 1;
                let active = model
                    .requests
                    .iter()
                    .filter(|r| r.state.is_active())
                    .count();
                assert_eq!(
                    ledger.rollover(model.epoch, &grants()).unwrap(),
                    RolloverOutcome::Advanced { carried: active }
                );
                let epoch = model.epoch;
                for r in model.requests.iter_mut().filter(|r| r.state.is_active()) {
                    r.epochs.push(epoch);
                }
            }
            Cmd::Revalidate => {
                ledger
                    .revalidate_safety_input(SAFETY, now, TTL, BUDGET)
                    .unwrap();
                model.budget = BUDGET as i64;
            }
            Cmd::CrashAfterExternalWrite(n) => {
                if let Some(i) = model.pick(n) {
                    let r = model.requests[i].clone();
                    if r.state == State::Reserved {
                        // The external record advanced; the ledger commit never happened.
                        let meta = ledger.meta().unwrap();
                        ExternalHwm::new(config(dir.path()).external_hwm_path)
                            .write(HighWaterMark {
                                generation: meta.generation + 1,
                                cumulative_liability: meta.cumulative_liability + r.liability,
                                last_reservation: r.id.0,
                            })
                            .unwrap();
                        let (reopened, assessment) = reopen(dir.path(), ledger, false);
                        ledger = reopened;
                        assert_eq!(assessment.promote_to_id_unknown, Some(r.id.0));
                        assert_eq!(
                            assessment.verdict,
                            TrustVerdict::Unknown(UnknownReason::UncleanShutdown)
                        );
                        model.normalize_after_restart(Some(r.id));
                        ledger.adopt_recovered_consumption(&[]).unwrap();
                    }
                }
            }
            Cmd::Restart { clean } => {
                let (reopened, assessment) = reopen(dir.path(), ledger, clean);
                ledger = reopened;
                assert_eq!(assessment.promote_to_id_unknown, None);
                model.normalize_after_restart(None);
                if clean {
                    assert_eq!(assessment.verdict, TrustVerdict::Trusted);
                } else {
                    assert_eq!(
                        assessment.verdict,
                        TrustVerdict::Unknown(UnknownReason::UncleanShutdown)
                    );
                    ledger.adopt_recovered_consumption(&[]).unwrap();
                }
            }
        }
        check(&ledger, &model);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    #[test]
    fn upstream_liability_never_exceeds_the_grant(cmds in prop::collection::vec(cmd(), 1..48)) {
        run(cmds);
    }
}
