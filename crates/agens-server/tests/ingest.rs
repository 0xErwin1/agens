//! The ingest channel and the health signals derived from what comes through
//! it: one rule of the derivation per test, plus the two detectors and the
//! recomputability the derived row is claimed to have.

use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_core::{EditMagnitude, FactPath, ToolOutcome, ToolResultFacts, WriteMagnitude};
use agens_server::{
    AcceptedFact, CheckpointClaim, HealthSignal, HealthThresholds, Ingest, IngestFact,
    IngestRejection, LostReason, ReportedCheckpoint, ReportedFact, ingest_channel,
};
use agens_store::{AttemptRow, ControlPlaneStore, EvidenceClass, RunRow, RunState, ToolFactStore};
use rusqlite::Connection;

const SESSION_ATTEMPT: i64 = 1;
const OTHER_SESSION_ATTEMPT: i64 = 2;
const NOW: i64 = 1_700_000_500;

#[test]
fn a_tool_call_that_touched_a_path_resets_the_stall_counters() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();

    harness.stall(run_id, 3);
    let before = harness.ingest.recompute(run_id, NOW).unwrap();
    let accepted = harness.accept(run_id, 4, IngestFact::ToolResult(wrote("src/one.rs")));

    assert_eq!(before.noop_turns, 3);
    assert!(before.tokens_since_progress > 0);
    assert_eq!(accepted.health.noop_turns, 0);
    assert_eq!(accepted.health.tokens_since_progress, 0);
    assert_eq!(accepted.health.last_progress_turn, Some(4));

    harness.finish();
}

#[test]
fn an_edit_with_no_diff_delta_and_a_read_move_nothing() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();

    harness.accept(run_id, 1, IngestFact::TurnStarted);
    harness.accept(
        run_id,
        1,
        IngestFact::ToolResult(ToolResultFacts::Edit {
            path: FactPath::new("src/one.rs"),
            outcome: ToolOutcome::Succeeded,
            changed: Some(EditMagnitude {
                lines_added: 0,
                lines_removed: 0,
            }),
        }),
    );
    harness.accept(
        run_id,
        1,
        IngestFact::ToolResult(ToolResultFacts::Read {
            path: FactPath::new("src/one.rs"),
            outcome: ToolOutcome::Succeeded,
        }),
    );
    let accepted = harness.accept(run_id, 1, IngestFact::TurnEnded { tokens: 900 });

    assert_eq!(accepted.health.noop_turns, 1);
    assert_eq!(accepted.health.tokens_since_progress, 900);
    assert_eq!(accepted.health.last_progress_turn, None);

    harness.finish();
}

#[test]
fn a_turn_parked_by_an_exhausted_context_is_not_counted_as_a_noop_turn() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();

    harness.accept(run_id, 1, IngestFact::TurnStarted);
    harness.accept(run_id, 1, IngestFact::ContextExhausted);
    let accepted = harness.accept(run_id, 1, IngestFact::TurnEnded { tokens: 4_000 });

    assert_eq!(accepted.health.noop_turns, 0);
    assert_eq!(accepted.health.tokens_since_progress, 0);

    harness.finish();
}

#[test]
fn the_same_exit_code_twice_becomes_a_failing_signature_and_a_pass_clears_it() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();

    let first = harness.accept(run_id, 1, IngestFact::ToolResult(exited(1)));
    let second = harness.accept(run_id, 1, IngestFact::ToolResult(exited(1)));
    let other_code = harness.accept(run_id, 2, IngestFact::ToolResult(exited(101)));
    let passing = harness.accept(run_id, 3, IngestFact::ToolResult(exited(0)));

    assert_eq!(first.health.failing_test_signature, None);
    assert_eq!(
        second.health.failing_test_signature.as_deref(),
        Some("bash:exit=1")
    );
    assert_eq!(
        other_code.health.failing_test_signature.as_deref(),
        Some("bash:exit=1"),
        "a different code starts its own run of repeats and does not replace the standing one"
    );
    assert_eq!(passing.health.failing_test_signature, None);

    harness.finish();
}

#[test]
fn only_a_deterministic_claim_of_progress_credits_progress() {
    for (evidence_class, credited) in [
        (EvidenceClass::Deterministic, true),
        (EvidenceClass::Inferential, false),
        (EvidenceClass::Insufficient, false),
    ] {
        let mut harness = Harness::open();
        let run_id = harness.seed_run();
        harness.stall(run_id, 2);

        let accepted = harness.accept(
            run_id,
            3,
            IngestFact::Checkpoint(ReportedCheckpoint::new(evidence_class, true)),
        );

        assert_eq!(
            accepted.health.noop_turns == 0,
            credited,
            "{evidence_class:?} credited progress: {credited}"
        );
        assert!(
            harness
                .event_types(run_id)
                .contains(&"checkpoint_recorded".to_owned()),
            "every claim is recorded, credited or not"
        );

        harness.finish();
    }
}

/// The limit of what a claim buys, pinned so it is a decision rather than an
/// oversight.
///
/// A `deterministic` claim credits progress on the worker's word alone.
/// `proof_refs` names something re-runnable and nothing re-runs it, and nothing
/// matches it against the passive layer either: the ledger here is empty and
/// the run has been stalled for three turns, and the claim still clears every
/// counter. What the claim cannot do is freeze anything — the genesis paths
/// come from the ledger, so a claim with no facts behind it leaves the run
/// with no declared paths at all.
#[test]
fn a_deterministic_claim_credits_progress_with_nothing_in_the_passive_layer_behind_it() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();
    harness.stall(run_id, 3);

    let accepted = harness.accept(
        run_id,
        4,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Deterministic, true)),
    );

    assert_eq!(
        accepted.health.noop_turns, 0,
        "the worker's own word clears the stall counter"
    );
    assert_eq!(
        accepted.health.tokens_since_progress, 0,
        "and the tokens the stall spent with it"
    );
    assert_eq!(
        accepted.frozen_genesis_paths, None,
        "a claim the ledger has nothing behind freezes no genesis paths"
    );
    assert!(
        accepted.signals.is_empty(),
        "and nothing contradicts it: {:?}",
        accepted.signals
    );

    harness.finish();
}

#[test]
fn a_checkpoint_that_claims_nothing_credits_nothing_even_with_deterministic_evidence() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();
    harness.stall(run_id, 2);

    let accepted = harness.accept(
        run_id,
        3,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Deterministic, false)),
    );

    assert_eq!(accepted.health.noop_turns, 2);

    harness.finish();
}

#[test]
fn the_first_checkpoint_with_a_diff_freezes_the_genesis_paths_from_the_ledger() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();

    let before_any_diff = harness.accept(
        run_id,
        1,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Inferential, false)),
    );
    harness.record_ledger_write(2, "src/b.rs");
    harness.record_ledger_write(3, "src/a.rs");
    let freezing = harness.accept(
        run_id,
        2,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Deterministic, true)),
    );
    harness.record_ledger_write(4, "src/late.rs");
    let after = harness.accept(
        run_id,
        3,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Deterministic, true)),
    );

    assert_eq!(before_any_diff.frozen_genesis_paths, None);
    assert_eq!(
        freezing.frozen_genesis_paths,
        Some(vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()])
    );
    assert_eq!(after.frozen_genesis_paths, None);
    assert_eq!(
        harness.genesis_paths(run_id),
        Some(r#"["src/a.rs","src/b.rs"]"#.to_owned())
    );

    harness.finish();
}

#[test]
fn a_path_outside_the_frozen_genesis_paths_raises_divergence_and_one_inside_does_not() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();

    let before_freeze = harness.accept(run_id, 1, IngestFact::ToolResult(wrote("src/wild.rs")));
    harness.record_ledger_write(2, "src/a.rs");
    harness.accept(
        run_id,
        2,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Deterministic, true)),
    );
    let inside = harness.accept(run_id, 3, IngestFact::ToolResult(wrote("src/a.rs")));
    let outside = harness.accept(run_id, 4, IngestFact::ToolResult(wrote("src/other.rs")));

    assert!(
        before_freeze.signals.is_empty(),
        "planning paths are tentative, so nothing is comparable before the freeze"
    );
    assert!(inside.signals.is_empty());
    assert_eq!(
        outside.signals,
        vec![HealthSignal::Divergence {
            path: "src/other.rs".to_owned()
        }]
    );
    assert!(
        harness
            .event_types(run_id)
            .contains(&"divergence_detected".to_owned())
    );

    harness.finish();
}

#[test]
fn a_mutation_whose_path_cannot_be_represented_is_escalated_once_the_set_is_frozen() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();

    harness.record_ledger_write(2, "src/a.rs");
    harness.accept(
        run_id,
        1,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Deterministic, true)),
    );
    let accepted = harness.accept(run_id, 2, IngestFact::ToolResult(wrote("/etc/passwd")));

    assert_eq!(
        accepted.signals,
        vec![HealthSignal::UnrepresentableMutation]
    );

    harness.finish();
}

#[test]
fn a_checkpoint_claiming_progress_over_a_stall_reports_a_lost_worker_once() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();
    harness.stall(run_id, 5);

    let claiming = harness.accept(
        run_id,
        6,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Inferential, true)),
    );
    let again = harness.accept(
        run_id,
        7,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Inferential, true)),
    );

    assert_eq!(
        claiming.signals,
        vec![HealthSignal::WorkerLost {
            reason: LostReason::ProgressClaimedWhileStalled,
            noop_turns: 5,
        }]
    );
    assert!(again.signals.is_empty(), "the standing stall signals once");

    harness.finish();
}

/// The fold is memoized, and a memo is evicted by another run's arrival or by
/// the daemon restarting.
///
/// Everything else the fold holds is rebuilt from the journal, and the standing
/// lost-worker signal has to be too: a run whose fold was evicted would
/// otherwise raise `worker_lost` again for the same stall, once per eviction,
/// and Praetor would be activated for a condition already reported.
#[test]
fn a_lost_worker_the_journal_already_records_is_not_raised_again_by_a_replay() {
    let directory = data_directory();
    let mut harness = Harness::in_directory(&directory);
    let run_id = harness.seed_run();
    harness.stall(run_id, 5);

    let claiming = harness.accept(
        run_id,
        6,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Inferential, true)),
    );
    assert_eq!(
        claiming.signals,
        vec![HealthSignal::WorkerLost {
            reason: LostReason::ProgressClaimedWhileStalled,
            noop_turns: 5,
        }]
    );
    drop(harness);

    let mut replayed = Harness::in_directory(&directory);
    let again = replayed.accept(
        run_id,
        7,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Inferential, true)),
    );

    assert!(
        again.signals.is_empty(),
        "the signal the journal already carries is not raised a second time"
    );
    assert_eq!(
        replayed
            .event_types(run_id)
            .iter()
            .filter(|event| *event == "worker_lost")
            .count(),
        1,
        "one standing stall is one entry"
    );

    replayed.finish();
}

#[test]
fn a_stall_under_the_threshold_and_a_run_with_no_checkpoint_report_nothing() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();

    harness.stall(run_id, 4);
    let under_threshold = harness.accept(
        run_id,
        5,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Inferential, true)),
    );

    let quiet_run_id = harness.seed_run();
    harness.stall(quiet_run_id, 9);
    let never_checkpointed = harness.accept(quiet_run_id, 10, IngestFact::TurnStarted);

    assert!(under_threshold.signals.is_empty());
    assert!(
        never_checkpointed.signals.is_empty(),
        "with no claim to contradict there is nothing to detect"
    );

    harness.finish();
}

#[test]
fn an_expired_checkpoint_reports_a_lost_worker_whatever_the_counters_say() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();

    let accepted = harness.accept(run_id, 1, IngestFact::CheckpointExpired);

    assert_eq!(
        accepted.signals,
        vec![HealthSignal::WorkerLost {
            reason: LostReason::CheckpointExpired,
            noop_turns: 0,
        }]
    );

    harness.finish();
}

/// The first checkpoint's deadline exists for the worker that died during
/// provisioning: it journaled `run_started`, never correlated, and never
/// reported anything. A fact refused for want of a physical execution left the
/// detector unreached and the slot held for the life of the daemon.
#[test]
fn an_expired_checkpoint_reaches_the_detector_for_an_attempt_that_never_correlated() {
    let mut harness = Harness::open();
    let run_id = harness.seed_uncorrelated_run();

    let accepted = harness
        .ingest
        .accept(&ReportedFact {
            run_id,
            attempt_id: None,
            turn: 1,
            now: NOW,
            fact: IngestFact::CheckpointExpired,
        })
        .expect("an attempt with no physical execution still owns its facts");

    assert_eq!(
        accepted.signals,
        vec![HealthSignal::WorkerLost {
            reason: LostReason::CheckpointExpired,
            noop_turns: 0,
        }]
    );

    harness.finish();
}

/// A worker naming its own execution against a run whose live attempt has not
/// been correlated is still a straggler: the two do not describe the same
/// physical execution.
#[test]
fn a_correlated_fact_against_an_uncorrelated_attempt_is_refused() {
    let mut harness = Harness::open();
    let run_id = harness.seed_uncorrelated_run();

    let refused = harness.ingest.accept(&ReportedFact {
        run_id,
        attempt_id: Some(SESSION_ATTEMPT),
        turn: 1,
        now: NOW,
        fact: IngestFact::TurnStarted,
    });

    assert_eq!(
        refused.expect_err("the live attempt names no execution"),
        IngestRejection::StaleAttempt {
            run_id,
            reported: Some(SESSION_ATTEMPT),
            live: None,
        }
    );

    harness.finish();
}

#[test]
fn a_fact_from_an_attempt_the_run_has_left_is_refused_and_writes_nothing() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();
    harness.stall(run_id, 2);
    harness.open_second_attempt(run_id);

    let straggler = harness.ingest.accept(&ReportedFact {
        run_id,
        attempt_id: Some(SESSION_ATTEMPT),
        turn: 3,
        now: NOW,
        fact: IngestFact::ToolResult(wrote("src/one.rs")),
    });

    assert_eq!(
        straggler,
        Err(IngestRejection::StaleAttempt {
            run_id,
            reported: Some(SESSION_ATTEMPT),
            live: Some(OTHER_SESSION_ATTEMPT),
        })
    );
    assert_eq!(
        harness
            .ingest
            .store()
            .load_run_health(run_id)
            .unwrap()
            .unwrap()
            .noop_turns,
        2,
        "a refused fact leaves the live attempt's signals where they were"
    );

    harness.finish();
}

#[test]
fn a_fact_for_an_unknown_run_or_a_negative_turn_is_refused() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();

    let unknown = harness.ingest.accept(&ReportedFact {
        run_id: run_id + 404,
        attempt_id: Some(SESSION_ATTEMPT),
        turn: 1,
        now: NOW,
        fact: IngestFact::TurnStarted,
    });
    let negative_turn = harness.ingest.accept(&ReportedFact {
        run_id,
        attempt_id: Some(SESSION_ATTEMPT),
        turn: -1,
        now: NOW,
        fact: IngestFact::TurnStarted,
    });

    assert_eq!(unknown, Err(IngestRejection::NoSuchRun(run_id + 404)));
    assert!(matches!(negative_turn, Err(IngestRejection::Malformed(_))));

    harness.finish();
}

#[test]
fn the_health_row_is_recomputable_from_the_journal_alone() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();

    harness.record_ledger_write(2, "src/a.rs");
    harness.accept(run_id, 1, IngestFact::TurnStarted);
    harness.accept(run_id, 1, IngestFact::ToolResult(wrote("src/a.rs")));
    harness.accept(run_id, 1, IngestFact::TurnEnded { tokens: 500 });
    harness.accept(
        run_id,
        2,
        IngestFact::Checkpoint(ReportedCheckpoint::new(EvidenceClass::Deterministic, true)),
    );
    harness.accept(run_id, 3, IngestFact::TurnStarted);
    harness.accept(run_id, 3, IngestFact::ToolResult(exited(2)));
    harness.accept(run_id, 3, IngestFact::ToolResult(exited(2)));
    harness.accept(run_id, 3, IngestFact::TurnEnded { tokens: 1_100 });
    let live = harness.accept(run_id, 4, IngestFact::TurnStarted);

    let stored = harness
        .ingest
        .store()
        .load_run_health(run_id)
        .unwrap()
        .unwrap();
    let recomputed = harness.ingest.recompute(run_id, NOW).unwrap();

    assert_eq!(recomputed, stored);
    assert_eq!(recomputed, live.health);
    assert_eq!(recomputed.noop_turns, 1);
    assert_eq!(recomputed.tokens_since_progress, 1_100);
    assert_eq!(
        recomputed.failing_test_signature.as_deref(),
        Some("bash:exit=2")
    );

    harness.finish();
}

#[test]
fn a_writer_that_never_saw_the_run_rebuilds_its_state_from_the_journal() {
    let directory = data_directory();
    let run_id = {
        let mut harness = Harness::in_directory(&directory);
        let run_id = harness.seed_run();
        harness.stall(run_id, 3);
        run_id
    };

    let mut restarted = Harness::in_directory(&directory);
    let accepted = restarted.accept(run_id, 4, IngestFact::TurnStarted);
    let after_noop = restarted.accept(run_id, 4, IngestFact::TurnEnded { tokens: 100 });

    assert_eq!(accepted.health.noop_turns, 3);
    assert_eq!(after_noop.health.noop_turns, 4);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn the_channel_hands_every_queued_fact_to_the_single_writer_in_order() {
    let mut harness = Harness::open();
    let run_id = harness.seed_run();
    let (sender, receiver) = ingest_channel();

    for turn in 1..=3 {
        sender
            .report(ReportedFact {
                run_id,
                attempt_id: Some(SESSION_ATTEMPT),
                turn,
                now: NOW,
                fact: IngestFact::TurnEnded { tokens: 10 },
            })
            .unwrap();
    }
    sender
        .report(ReportedFact {
            run_id,
            attempt_id: Some(OTHER_SESSION_ATTEMPT),
            turn: 4,
            now: NOW,
            fact: IngestFact::TurnEnded { tokens: 10 },
        })
        .unwrap();

    let drained = harness.ingest.drain_available(&receiver);

    assert_eq!(drained.len(), 4);
    assert_eq!(
        drained
            .iter()
            .filter_map(|entry| entry.outcome.as_ref().ok())
            .map(|accepted| accepted.health.noop_turns)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(
        drained[3].outcome.is_err(),
        "a fact from another attempt is surfaced, not dropped"
    );

    harness.finish();
}

#[test]
fn a_stored_checkpoint_row_reaches_ingest_through_the_claim_interface() {
    struct StoredCheckpoint {
        evidence_class: EvidenceClass,
        claims_progress: bool,
    }

    impl CheckpointClaim for StoredCheckpoint {
        fn evidence_class(&self) -> EvidenceClass {
            self.evidence_class
        }

        fn claims_progress(&self) -> bool {
            self.claims_progress
        }
    }

    let mut harness = Harness::open();
    let run_id = harness.seed_run();
    harness.stall(run_id, 1);

    let reported = ReportedCheckpoint::from_claim(&StoredCheckpoint {
        evidence_class: EvidenceClass::Deterministic,
        claims_progress: true,
    });
    let accepted = harness.accept(run_id, 2, IngestFact::Checkpoint(reported));

    assert!(reported.credits_progress());
    assert_eq!(accepted.health.noop_turns, 0);

    harness.finish();
}

struct Harness {
    directory: std::path::PathBuf,
    ingest: Ingest,
    facts: ToolFactStore,
    connection: Connection,
}

impl Harness {
    fn open() -> Self {
        Self::in_directory(&data_directory())
    }

    fn in_directory(directory: &std::path::Path) -> Self {
        let store = ControlPlaneStore::open(directory).unwrap();
        let connection = Connection::open(store.database_path()).unwrap();

        Self {
            directory: directory.to_path_buf(),
            ingest: Ingest::with_thresholds(store, HealthThresholds::default()),
            facts: ToolFactStore::open(directory).unwrap(),
            connection,
        }
    }

    /// A running run with its live attempt, and the physical session attempts
    /// the evidence ledger hangs off. A session holds one running attempt, so
    /// each physical attempt gets its own.
    fn seed_run(&mut self) -> i64 {
        let run_id = self
            .ingest_store_mut()
            .insert_run(&RunRow {
                id: None,
                repo_id: "a1b2c3d4e5f60718".to_owned(),
                repo_root: "/home/dev/agens".to_owned(),
                remote_url: None,
                external_ref: None,
                parent_run_id: None,
                task: "ingest and health signals".to_owned(),
                scope: "crates/agens-server".to_owned(),
                dod: "green gate".to_owned(),
                genesis_paths: None,
                state: RunState::Running,
                priority: 3,
                dep_run_id: None,
                provider: "anthropic".to_owned(),
                budget_tokens: None,
                worktree_path: None,
                worktree_status: None,
                created_at: 1_700_000_000,
                result: None,
            })
            .unwrap();

        for id in [SESSION_ATTEMPT, OTHER_SESSION_ATTEMPT] {
            self.seed_session_attempt(id);
        }
        self.open_attempt(run_id, 1, SESSION_ATTEMPT);

        run_id
    }

    /// A running run whose live attempt was opened and never correlated with a
    /// physical execution, which is where a worker that died during
    /// provisioning leaves it.
    fn seed_uncorrelated_run(&mut self) -> i64 {
        let run_id = self.seed_run();
        self.connection
            .execute(
                "UPDATE attempts SET session_attempt_id = NULL WHERE run_id = ?1",
                [run_id],
            )
            .unwrap();

        run_id
    }

    fn open_second_attempt(&mut self, run_id: i64) {
        self.open_attempt(run_id, 2, OTHER_SESSION_ATTEMPT);
    }

    fn open_attempt(&mut self, run_id: i64, n: i64, session_attempt_id: i64) {
        self.ingest_store_mut()
            .insert_attempt(&AttemptRow {
                id: None,
                run_id,
                n,
                session_id: Some(session_attempt_id),
                session_attempt_id: Some(session_attempt_id),
                started_at: 1_700_000_000,
                ended_at: None,
                outcome: None,
                retry_trigger: None,
                tokens: None,
                cost_micros: None,
            })
            .unwrap();
    }

    fn seed_session_attempt(&self, id: i64) {
        self.connection
            .execute(
                "INSERT OR IGNORE INTO sessions (
                     id, project, title, active_agent, created_at, updated_at
                 ) VALUES (?1, 'project', 'title', 'build', 0, 0)",
                [id],
            )
            .unwrap();
        self.connection
            .execute(
                "INSERT OR IGNORE INTO session_attempts (
                     id, session_id, sequence, status, retry_prompt, started_at
                 ) VALUES (?1, ?1, 1, 'running', 'retry', 0)",
                [id],
            )
            .unwrap();
    }

    /// A `ControlPlaneStore` handle for the seeding this harness does directly.
    /// Ingest holds the writer, so the seeding goes through a second handle to
    /// the same file rather than a second copy of the state.
    fn ingest_store_mut(&mut self) -> ControlPlaneStore {
        ControlPlaneStore::open(&self.directory).unwrap()
    }

    fn record_ledger_write(&mut self, sequence: u64, path: &str) {
        self.facts
            .record(
                SESSION_ATTEMPT,
                SESSION_ATTEMPT,
                sequence,
                &format!("call-{sequence}"),
                &wrote(path),
            )
            .unwrap();
    }

    fn accept(&mut self, run_id: i64, turn: i64, fact: IngestFact) -> AcceptedFact {
        self.ingest
            .accept(&ReportedFact {
                run_id,
                attempt_id: Some(SESSION_ATTEMPT),
                turn,
                now: NOW,
                fact,
            })
            .unwrap()
    }

    /// Runs `turns` turns that end without observable progress.
    fn stall(&mut self, run_id: i64, turns: i64) {
        for turn in 1..=turns {
            self.accept(run_id, turn, IngestFact::TurnStarted);
            self.accept(run_id, turn, IngestFact::TurnEnded { tokens: 100 });
        }
    }

    fn event_types(&self, run_id: i64) -> Vec<String> {
        self.ingest
            .store()
            .events_for_run(run_id)
            .unwrap()
            .into_iter()
            .map(|event| event.event_type)
            .collect()
    }

    fn genesis_paths(&self, run_id: i64) -> Option<String> {
        self.ingest
            .store()
            .load_run(run_id)
            .unwrap()
            .unwrap()
            .genesis_paths
    }

    fn finish(self) {
        let directory = self.directory.clone();
        drop(self);
        fs::remove_dir_all(directory).unwrap();
    }
}

fn wrote(path: &str) -> ToolResultFacts {
    ToolResultFacts::Write {
        path: FactPath::new(path),
        outcome: ToolOutcome::Succeeded,
        written: Some(WriteMagnitude {
            is_new_file: false,
            bytes_written: 64,
            lines_written: 4,
        }),
    }
}

fn exited(code: i32) -> ToolResultFacts {
    ToolResultFacts::Bash {
        outcome: if code == 0 {
            ToolOutcome::Succeeded
        } else {
            ToolOutcome::Failed
        },
        exit_code: Some(code),
    }
}

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-server-ingest-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}
