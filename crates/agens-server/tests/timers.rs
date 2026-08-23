//! The coordinator's timer wheel.
//!
//! Every test drives an injected clock and asserts against the database, never
//! against anything the wheel remembers: the wheel is required to hold no state
//! of its own, so a deadline that survives only in memory is the bug these
//! tests exist to catch.

use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_server::{
    CHECKPOINT_EVENT, CHECKPOINT_OVERDUE_EVENT, QuestionEffect, RunEffect, StateMachines,
    TIMER_STAGE_REJECTED_EVENT, TimerSettings, TimerStage, TimerWheel,
};
use agens_store::{
    ControlPlaneStore, EventClass, EventRow, ProviderRow, QuestionAuthor, QuestionKind,
    QuestionRow, QuestionState, QuotaState, RunRow, RunState, WorktreeStatus,
};

const START: i64 = 1_700_000_000;

/// The promised span every fixture checkpoint declares. With the default grace
/// its deadline lands 900 seconds after the checkpoint.
const PROMISED_SPAN: i64 = 600;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-server-timers-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn store() -> ControlPlaneStore {
    ControlPlaneStore::open(data_directory()).unwrap()
}

/// The three stages of a tick answer to different deadlines and are refused
/// independently.
///
/// Chained through `?`, one refused stage suspended the other two on every
/// tick: a run that left `awaiting_quota` between the read and the apply was
/// enough to stop every question from expiring and every overdue checkpoint
/// from being raised, for as long as the row disagreed. Here that refusal is
/// forced with a trigger that aborts exactly the write the quota stage makes.
#[test]
fn a_refused_stage_is_journaled_and_the_other_two_still_run() {
    let (directory, mut store) = store_in_its_own_directory();

    let working = working_run(&mut store);
    let question = store
        .insert_question(&question_in(
            working,
            QuestionKind::Approval,
            QuestionState::Open,
            Some(START + 600),
        ))
        .unwrap();
    let parked = store
        .insert_run(&run_in(RunState::AwaitingQuota, "anthropic"))
        .unwrap();
    store
        .record_provider(&capped("anthropic", Some(START + 600)))
        .unwrap();

    refuse_leaving_awaiting_quota(&directory);

    let mut machines = StateMachines::new(store);
    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    clock.set(START + 900);

    let tick = wheel.tick(&mut machines);

    assert!(tick.quota_resets.is_empty());
    assert_eq!(state_of(&machines, parked), RunState::AwaitingQuota);
    assert_eq!(tick.rejections.len(), 1);

    let refused = tick.rejections.first().expect("the stage was refused");
    assert_eq!(refused.stage, TimerStage::QuotaResets);
    assert!(
        refused.event_id.is_some(),
        "the refusal reached the journal"
    );

    // The two stages behind it ran all the same.
    assert_eq!(tick.expired_questions.len(), 1);
    assert_eq!(question_state(&machines, question), QuestionState::Expired);
    assert_eq!(tick.overdue_checkpoints.len(), 1);
    assert_eq!(overdue_signals(&machines, working), 1);

    let journaled = machines
        .store()
        .events_after(0, 256)
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == TIMER_STAGE_REJECTED_EVENT)
        .expect("the refusal is in the journal");
    let payload: serde_json::Value = serde_json::from_str(&journaled.payload).unwrap();

    assert_eq!(payload["stage"], "quota_resets");
    assert!(
        payload["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty())
    );
}

/// The wheel ticks four times a second, and a condition that refuses a stage
/// refuses it on every one of those ticks.
///
/// Journaling each occurrence buries the journal under the same sentence, which
/// is the rule the queue journal already holds itself to: what an operator
/// needs is the condition and the moment it started. The refusal is still
/// carried back on every tick, because the wheel's caller decides what to do
/// about a stage that is not running.
#[test]
fn a_refusal_that_stands_is_journaled_once_rather_than_on_every_tick() {
    let (directory, mut store) = store_in_its_own_directory();

    let parked = store
        .insert_run(&run_in(RunState::AwaitingQuota, "anthropic"))
        .unwrap();
    store
        .record_provider(&capped("anthropic", Some(START + 600)))
        .unwrap();

    refuse_leaving_awaiting_quota(&directory);

    let mut machines = StateMachines::new(store);
    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    clock.set(START + 900);

    let first = wheel.tick(&mut machines);
    let second = wheel.tick(&mut machines);
    let third = wheel.tick(&mut machines);

    assert_eq!(first.rejections.len(), 1);
    assert_eq!(
        second.rejections.len(),
        1,
        "the caller hears about the stage on every tick it did not run"
    );
    assert_eq!(
        [
            first.rejections[0].event_id,
            second.rejections[0].event_id,
            third.rejections[0].event_id
        ],
        [first.rejections[0].event_id; 3],
        "every tick points at the entry that already stands"
    );
    assert_eq!(
        rejection_entries(&machines).len(),
        1,
        "one standing condition is one entry"
    );

    // The condition passes, and comes back. What comes back is a new condition:
    // an operator reading the journal sees when it started, both times.
    allow_leaving_awaiting_quota(&directory);
    let recovered = wheel.tick(&mut machines);
    assert!(recovered.rejections.is_empty());
    assert_eq!(state_of(&machines, parked), RunState::Queued);

    park_again(&directory, parked);
    refuse_leaving_awaiting_quota(&directory);
    let again = wheel.tick(&mut machines);

    assert_eq!(again.rejections.len(), 1);
    assert_eq!(
        rejection_entries(&machines).len(),
        2,
        "a condition that ended and started again is a second entry"
    );
}

fn rejection_entries(machines: &StateMachines) -> Vec<agens_store::EventRow> {
    machines
        .store()
        .events_after(0, 256)
        .unwrap()
        .into_iter()
        .filter(|event| event.event_type == TIMER_STAGE_REJECTED_EVENT)
        .collect()
}

fn allow_leaving_awaiting_quota(directory: &std::path::Path) {
    rusqlite::Connection::open(directory.join("agens.db"))
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_quota_reset;")
        .unwrap();
}

/// Puts the run and its provider back where the wheel finds them, without a
/// transition: what this test needs is the same refusal a second time, not a
/// lifecycle.
fn park_again(directory: &std::path::Path, run_id: i64) {
    ControlPlaneStore::open(directory)
        .unwrap()
        .record_provider(&capped("anthropic", Some(START + 600)))
        .unwrap();

    rusqlite::Connection::open(directory.join("agens.db"))
        .unwrap()
        .execute(
            "UPDATE runs SET state = 'awaiting_quota' WHERE id = ?1",
            [run_id],
        )
        .unwrap();
}

/// Aborts the one write the quota stage makes, which is what a run leaving
/// `awaiting_quota` between the wheel's read and its apply does to it.
fn refuse_leaving_awaiting_quota(directory: &std::path::Path) {
    rusqlite::Connection::open(directory.join("agens.db"))
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_quota_reset BEFORE UPDATE ON runs
             WHEN old.state = 'awaiting_quota'
             BEGIN SELECT RAISE(ABORT, 'the run already left awaiting_quota'); END;",
        )
        .unwrap();
}

/// A store whose directory the test keeps, for the one test that reaches the
/// database file itself.
fn store_in_its_own_directory() -> (std::path::PathBuf, ControlPlaneStore) {
    let directory = data_directory();
    let store = ControlPlaneStore::open(&directory).unwrap();

    (directory, store)
}

fn run_in(state: RunState, provider: &str) -> RunRow {
    RunRow {
        id: None,
        repo_id: "a1b2c3d4e5f60718".to_owned(),
        repo_root: "/home/dev/agens".to_owned(),
        remote_url: Some("git@example.com:dev/agens.git".to_owned()),
        external_ref: Some("agens/AGN-57".to_owned()),
        parent_run_id: None,
        task: "the timer wheel".to_owned(),
        scope: "crates/agens-server/src/timers".to_owned(),
        dod: "every deadline recomputed from the database".to_owned(),
        genesis_paths: None,
        state,
        priority: 5,
        dep_run_id: None,
        provider: provider.to_owned(),
        budget_tokens: Some(200_000),
        worktree_path: Some("/data/worktrees/agens-a1b2c3d4/agn-57".to_owned()),
        worktree_status: Some(WorktreeStatus::Active),
        created_at: START,
        result: None,
    }
}

fn question_in(
    run_id: i64,
    kind: QuestionKind,
    state: QuestionState,
    expires_at: Option<i64>,
) -> QuestionRow {
    let answered = matches!(state, QuestionState::Answered | QuestionState::Delivered);

    QuestionRow {
        id: None,
        run_id,
        kind,
        blocked_decision: "merge the branch".to_owned(),
        options: "[\"yes\",\"no\"]".to_owned(),
        recommendation: None,
        answer: answered.then(|| "yes".to_owned()),
        author: answered.then_some(QuestionAuthor::User),
        expires_at,
        tree_hash: (kind == QuestionKind::Approval).then(|| "c0ffee".repeat(6)),
        paths_digest: (kind == QuestionKind::Approval).then(|| "d1ge57".repeat(6)),
        state,
        created_at: START,
    }
}

/// A checkpoint as the harness journals it: an event whose payload carries the
/// moment the worker promised the next one by.
fn checkpoint(run_id: i64, ts: i64, promised_at: Option<i64>) -> EventRow {
    let payload = match promised_at {
        Some(promised_at) => {
            format!("{{\"goal\":\"land the wheel\",\"promised_at\":{promised_at}}}")
        }
        None => "{\"goal\":\"land the wheel\"}".to_owned(),
    };

    EventRow {
        id: None,
        run_id: Some(run_id),
        event_type: CHECKPOINT_EVENT.to_owned(),
        class: EventClass::Agent,
        payload,
        ts,
    }
}

fn capped(provider: &str, reset_at: Option<i64>) -> ProviderRow {
    ProviderRow {
        provider: provider.to_owned(),
        quota_state: QuotaState::Capped,
        reset_at,
        updated_at: START,
    }
}

fn state_of(machines: &StateMachines, run_id: i64) -> RunState {
    machines.store().load_run(run_id).unwrap().unwrap().state
}

fn question_state(machines: &StateMachines, question_id: i64) -> QuestionState {
    machines
        .store()
        .load_question(question_id)
        .unwrap()
        .unwrap()
        .state
}

fn overdue_signals(machines: &StateMachines, run_id: i64) -> usize {
    machines
        .store()
        .events_of_type_for_run(run_id, CHECKPOINT_OVERDUE_EVENT)
        .unwrap()
        .len()
}

/// A running run with one checkpoint already journaled.
fn working_run(store: &mut ControlPlaneStore) -> i64 {
    let run_id = store
        .insert_run(&run_in(RunState::Running, "anthropic"))
        .unwrap();
    store
        .append_event(&checkpoint(run_id, START, Some(START + PROMISED_SPAN)))
        .unwrap();

    run_id
}

/// A running run that was admitted and has never checkpointed.
fn silent_run(store: &mut ControlPlaneStore) -> i64 {
    let run_id = store
        .insert_run(&run_in(RunState::Running, "anthropic"))
        .unwrap();
    store.append_event(&run_started(run_id, START)).unwrap();

    run_id
}

fn run_started(run_id: i64, ts: i64) -> EventRow {
    EventRow {
        id: None,
        run_id: Some(run_id),
        event_type: "run_started".to_owned(),
        class: EventClass::Infra,
        payload: "{\"machine\":\"run\",\"to\":\"running\"}".to_owned(),
        ts,
    }
}

/// A worker that never checkpoints declares no deadline of its own, so nothing
/// measured it: it held a slot and a worktree for as long as the daemon lived.
/// The admission is what the wheel measures it from instead.
#[test]
fn a_run_that_never_checkpoints_is_overdue_once_the_first_checkpoint_span_passes() {
    let mut store = store();
    let run_id = silent_run(&mut store);
    let mut machines = StateMachines::new(store);

    let settings = TimerSettings {
        first_checkpoint_seconds: 1_800,
        ..TimerSettings::default()
    };
    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(settings, START);

    clock.set(START + 1_799);
    assert!(
        wheel.tick(&mut machines).overdue_checkpoints.is_empty(),
        "everything before the first checkpoint is setup the worker cannot report on"
    );

    clock.set(START + 1_800);
    let tick = wheel.tick(&mut machines);

    let overdue = tick.overdue_checkpoints.first().unwrap();
    assert_eq!(overdue.run_id, run_id);
    assert_eq!(
        overdue.promised_at, None,
        "a worker that has said nothing has promised nothing"
    );
    assert_eq!(overdue.deadline, START + 1_800);
    assert_eq!(overdue_signals(&machines, run_id), 1);
}

/// The signal is deduplicated on the entry it was raised for, and the entry
/// itself is what carries that across a restart.
#[test]
fn a_missed_first_checkpoint_is_signalled_once_however_many_ticks_pass() {
    let mut store = store();
    let run_id = silent_run(&mut store);
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);

    clock.set(START + 100_000);
    assert_eq!(wheel.tick(&mut machines).overdue_checkpoints.len(), 1);

    clock.set(START + 200_000);
    assert!(
        wheel.tick(&mut machines).overdue_checkpoints.is_empty(),
        "a wheel that raised it every tick would bury the feed it draws attention in"
    );
    assert_eq!(overdue_signals(&machines, run_id), 1);
}

/// A run that has not executed has nothing to be late for.
#[test]
fn a_run_that_never_started_has_no_first_checkpoint_deadline() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Running, "anthropic"))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    clock.set(START + 1_000_000);

    assert!(wheel.tick(&mut machines).overdue_checkpoints.is_empty());
    assert_eq!(overdue_signals(&machines, run_id), 0);
}

/// The first checkpoint replaces the admission as what the run is measured
/// against: it carries a promise, and the promise is the more specific claim.
#[test]
fn the_first_checkpoint_replaces_the_admission_as_the_deadline() {
    let mut store = store();
    let run_id = silent_run(&mut store);
    store
        .append_event(&checkpoint(
            run_id,
            START + 60,
            Some(START + 60 + PROMISED_SPAN),
        ))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);

    clock.set(START + 959);
    assert!(wheel.tick(&mut machines).overdue_checkpoints.is_empty());

    clock.set(START + 960);
    let tick = wheel.tick(&mut machines);

    assert_eq!(
        tick.overdue_checkpoints
            .first()
            .and_then(|overdue| overdue.promised_at),
        Some(START + 60 + PROMISED_SPAN)
    );
}

#[test]
fn a_checkpoint_is_overdue_only_once_the_promised_span_plus_its_grace_has_passed() {
    let mut store = store();
    let run_id = working_run(&mut store);
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);

    clock.set(START + 899);
    assert!(wheel.tick(&mut machines).overdue_checkpoints.is_empty());

    clock.set(START + 900);
    let tick = wheel.tick(&mut machines);

    let overdue = tick.overdue_checkpoints.first().unwrap();
    assert_eq!(overdue.run_id, run_id);
    assert_eq!(overdue.promised_at, Some(START + PROMISED_SPAN));
    assert_eq!(overdue.deadline, START + 900);
    assert_eq!(overdue_signals(&machines, run_id), 1);
}

#[test]
fn a_configured_grace_replaces_the_default_one_and_a_half_promised_spans() {
    let mut store = store();
    working_run(&mut store);
    let mut machines = StateMachines::new(store);

    let settings = TimerSettings {
        checkpoint_grace_percent: 300,
        ..TimerSettings::default()
    };
    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(settings, START);

    clock.set(START + 900);
    assert!(wheel.tick(&mut machines).overdue_checkpoints.is_empty());

    clock.set(START + 1_800);
    let tick = wheel.tick(&mut machines);

    assert_eq!(
        tick.overdue_checkpoints.first().unwrap().deadline,
        START + 1_800
    );
}

#[test]
fn an_overdue_checkpoint_is_signalled_once_however_many_ticks_pass() {
    let mut store = store();
    let run_id = working_run(&mut store);
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);

    clock.set(START + 1_000);
    assert_eq!(wheel.tick(&mut machines).overdue_checkpoints.len(), 1);

    clock.set(START + 2_000);
    assert!(wheel.tick(&mut machines).overdue_checkpoints.is_empty());

    clock.set(START + 3_000);
    assert!(wheel.tick(&mut machines).overdue_checkpoints.is_empty());

    assert_eq!(overdue_signals(&machines, run_id), 1);
}

#[test]
fn a_fresh_checkpoint_earns_its_own_deadline_and_its_own_signal() {
    let mut store = store();
    let run_id = working_run(&mut store);
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);

    clock.set(START + 1_000);
    wheel.tick(&mut machines);

    machines
        .journal(&[checkpoint(run_id, START + 1_100, Some(START + 1_700))])
        .unwrap();

    clock.set(START + 1_500);
    assert!(wheel.tick(&mut machines).overdue_checkpoints.is_empty());

    clock.set(START + 2_000);
    assert_eq!(wheel.tick(&mut machines).overdue_checkpoints.len(), 1);
    assert_eq!(overdue_signals(&machines, run_id), 2);
}

#[test]
fn a_restarted_wheel_still_owes_the_checkpoint_its_deadline_and_never_signals_twice() {
    let mut store = store();
    let run_id = working_run(&mut store);
    let mut machines = StateMachines::new(store);

    let (first, first_clock) =
        TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    first_clock.set(START + 100);
    assert!(first.tick(&mut machines).overdue_checkpoints.is_empty());
    drop(first);

    let (second, second_clock) =
        TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    second_clock.set(START + 1_000);
    assert_eq!(second.tick(&mut machines).overdue_checkpoints.len(), 1);
    drop(second);

    let (third, third_clock) =
        TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    third_clock.set(START + 2_000);
    assert!(third.tick(&mut machines).overdue_checkpoints.is_empty());

    assert_eq!(overdue_signals(&machines, run_id), 1);
}

#[test]
fn a_checkpoint_that_promised_nothing_measurable_has_no_deadline() {
    let mut store = store();
    let silent = store
        .insert_run(&run_in(RunState::Running, "anthropic"))
        .unwrap();
    let backwards = store
        .insert_run(&run_in(RunState::Running, "anthropic"))
        .unwrap();
    store
        .append_event(&checkpoint(silent, START, None))
        .unwrap();
    store
        .append_event(&checkpoint(backwards, START, Some(START - 60)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    clock.set(START + 100_000);

    let tick = wheel.tick(&mut machines);

    assert!(tick.overdue_checkpoints.is_empty());
    assert_eq!(overdue_signals(&machines, silent), 0);
    assert_eq!(overdue_signals(&machines, backwards), 0);
}

#[test]
fn only_a_running_run_owes_a_checkpoint() {
    let mut store = store();
    let parked = store
        .insert_run(&run_in(RunState::AwaitingInput, "anthropic"))
        .unwrap();
    store
        .append_event(&checkpoint(parked, START, Some(START + PROMISED_SPAN)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    clock.set(START + 100_000);

    assert!(wheel.tick(&mut machines).overdue_checkpoints.is_empty());
    assert_eq!(overdue_signals(&machines, parked), 0);
}

#[test]
fn an_elapsed_reset_requeues_every_run_parked_on_that_provider_and_lifts_the_cap() {
    let mut store = store();
    let first = store
        .insert_run(&run_in(RunState::AwaitingQuota, "anthropic"))
        .unwrap();
    let second = store
        .insert_run(&run_in(RunState::AwaitingQuota, "anthropic"))
        .unwrap();
    let elsewhere = store
        .insert_run(&run_in(RunState::AwaitingQuota, "openai"))
        .unwrap();
    store
        .record_provider(&capped("anthropic", Some(START + 600)))
        .unwrap();
    store
        .record_provider(&capped("openai", Some(START + 5_000)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    clock.set(START + 600);

    let tick = wheel.tick(&mut machines);

    let requeued: Vec<i64> = tick.quota_resets.iter().map(|reset| reset.run_id).collect();
    assert_eq!(requeued, vec![first, second]);
    assert_eq!(state_of(&machines, first), RunState::Queued);
    assert_eq!(state_of(&machines, second), RunState::Queued);
    assert_eq!(state_of(&machines, elsewhere), RunState::AwaitingQuota);

    let lifted = machines
        .store()
        .load_provider("anthropic")
        .unwrap()
        .unwrap();
    assert_eq!(lifted.quota_state, QuotaState::Ok);
    assert_eq!(lifted.reset_at, None);

    assert!(tick.quota_resets.iter().all(|reset| {
        reset
            .transition
            .effects
            .contains(&RunEffect::ResumePriority)
    }));
}

#[test]
fn a_reset_that_has_not_arrived_wakes_nothing() {
    let mut store = store();
    let parked = store
        .insert_run(&run_in(RunState::AwaitingQuota, "anthropic"))
        .unwrap();
    store
        .record_provider(&capped("anthropic", Some(START + 600)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    clock.set(START + 599);

    assert!(wheel.tick(&mut machines).quota_resets.is_empty());
    assert_eq!(state_of(&machines, parked), RunState::AwaitingQuota);
}

/// Nothing re-derives a cap the provider named no reset for: it is lifted by a
/// run reaching that provider, and every run parked on it is barred from
/// starting. The configured window is what breaks that circle, and it is
/// measured from the moment the cap was recorded.
#[test]
fn a_cap_with_no_reset_time_waits_out_the_configured_window() {
    let mut store = store();
    let parked = store
        .insert_run(&run_in(RunState::AwaitingQuota, "anthropic"))
        .unwrap();
    store.record_provider(&capped("anthropic", None)).unwrap();
    let mut machines = StateMachines::new(store);

    let settings = TimerSettings {
        quota_window_seconds: 900,
        ..TimerSettings::default()
    };
    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(settings, START);

    clock.set(START + 899);
    assert!(wheel.tick(&mut machines).quota_resets.is_empty());
    assert_eq!(state_of(&machines, parked), RunState::AwaitingQuota);

    clock.set(START + 900);
    assert_eq!(wheel.tick(&mut machines).quota_resets.len(), 1);
    assert_eq!(state_of(&machines, parked), RunState::Queued);
}

#[test]
fn an_expired_approval_is_voided_rather_than_granted() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Running, "anthropic"))
        .unwrap();
    let approval = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Approval,
            QuestionState::Open,
            Some(START + 600),
        ))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);

    clock.set(START + 599);
    assert!(wheel.tick(&mut machines).expired_questions.is_empty());

    clock.set(START + 600);
    let tick = wheel.tick(&mut machines);

    let expired = tick.expired_questions.first().unwrap();
    assert_eq!(expired.question_id, approval);
    assert!(
        expired
            .transition
            .effects
            .contains(&QuestionEffect::InvalidateAuthorization)
    );
    assert_eq!(question_state(&machines, approval), QuestionState::Expired);
    assert_eq!(
        machines
            .store()
            .load_question(approval)
            .unwrap()
            .unwrap()
            .answer,
        None
    );
}

#[test]
fn an_authorization_already_granted_still_expires_unconsumed() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Running, "anthropic"))
        .unwrap();
    let approval = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Approval,
            QuestionState::Answered,
            Some(START + 600),
        ))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    clock.set(START + 601);

    assert_eq!(wheel.tick(&mut machines).expired_questions.len(), 1);
    assert_eq!(question_state(&machines, approval), QuestionState::Expired);
}

#[test]
fn a_plain_question_past_its_expiry_expires_and_an_undated_one_waits() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Running, "anthropic"))
        .unwrap();
    let dated = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Question,
            QuestionState::Open,
            Some(START + 600),
        ))
        .unwrap();
    let undated = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Question,
            QuestionState::Open,
            None,
        ))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    clock.set(START + 100_000);

    let tick = wheel.tick(&mut machines);

    assert_eq!(tick.expired_questions.len(), 1);
    assert_eq!(question_state(&machines, dated), QuestionState::Expired);
    assert_eq!(question_state(&machines, undated), QuestionState::Open);
}

#[test]
fn an_answered_question_the_table_cannot_expire_is_left_where_it_is() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Running, "anthropic"))
        .unwrap();
    let answered = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Question,
            QuestionState::Answered,
            Some(START + 600),
        ))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    clock.set(START + 100_000);

    let tick = wheel.tick(&mut machines);

    assert!(tick.expired_questions.is_empty());
    assert_eq!(question_state(&machines, answered), QuestionState::Answered);
}

#[test]
fn one_tick_carries_all_three_kinds_of_expiry() {
    let mut store = store();
    let working = working_run(&mut store);
    let parked = store
        .insert_run(&run_in(RunState::AwaitingQuota, "openai"))
        .unwrap();
    store
        .record_provider(&capped("openai", Some(START + 300)))
        .unwrap();
    let approval = store
        .insert_question(&question_in(
            working,
            QuestionKind::Approval,
            QuestionState::Open,
            Some(START + 200),
        ))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);
    clock.set(START + 1_000);

    let tick = wheel.tick(&mut machines);

    assert_eq!(tick.now, START + 1_000);
    assert_eq!(tick.overdue_checkpoints.len(), 1);
    assert_eq!(tick.quota_resets.len(), 1);
    assert_eq!(tick.expired_questions.len(), 1);

    assert_eq!(state_of(&machines, parked), RunState::Queued);
    assert_eq!(question_state(&machines, approval), QuestionState::Expired);
    assert_eq!(overdue_signals(&machines, working), 1);
}

#[test]
fn nothing_at_all_is_due_before_the_clock_moves() {
    let mut store = store();
    let working = working_run(&mut store);
    store
        .insert_question(&question_in(
            working,
            QuestionKind::Approval,
            QuestionState::Open,
            Some(START + 200),
        ))
        .unwrap();
    store
        .insert_run(&run_in(RunState::AwaitingQuota, "openai"))
        .unwrap();
    store
        .record_provider(&capped("openai", Some(START + 300)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let (wheel, _clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), START);

    let tick = wheel.tick(&mut machines);

    assert_eq!(tick.now, START);
    assert!(tick.overdue_checkpoints.is_empty());
    assert!(tick.quota_resets.is_empty());
    assert!(tick.expired_questions.is_empty());
}
