use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_store::{
    AttemptOutcome, AttemptRow, CausalDisposition, ControlPlaneStore, EventClass, EventRow,
    EvidenceClass, FindingRow, ProviderRow, QuestionAnswer, QuestionAuthor, QuestionChange,
    QuestionKind, QuestionRow, QuestionState, QuotaState, RetryTrigger, RunHealthRow, RunRow,
    RunState, StateChange, TransitionWrite, WorktreeStatus,
};
use rusqlite::Connection;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-store-control-plane-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn draft_run() -> RunRow {
    RunRow {
        id: None,
        repo_id: "a1b2c3d4e5f60718".to_owned(),
        repo_root: "/home/dev/agens".to_owned(),
        remote_url: Some("git@example.com:dev/agens.git".to_owned()),
        external_ref: Some("agens/AGN-53".to_owned()),
        parent_run_id: None,
        task: "control plane schema".to_owned(),
        scope: "crates/agens-store".to_owned(),
        dod: "migrations applied and green".to_owned(),
        genesis_paths: None,
        state: RunState::Draft,
        priority: 3,
        dep_run_id: None,
        provider: "anthropic".to_owned(),
        budget_tokens: Some(200_000),
        worktree_path: None,
        worktree_status: None,
        created_at: 1_700_000_000,
        result: None,
    }
}

fn table_names(database_path: &std::path::Path) -> Vec<String> {
    let connection = Connection::open(database_path).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table'
               AND name IN ('runs', 'attempts', 'events', 'questions', 'findings',
                            'directives', 'providers', 'run_health')
             ORDER BY name",
        )
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn the_migration_creates_every_control_plane_table() {
    let directory = data_directory();

    let store = ControlPlaneStore::open(&directory).unwrap();

    assert_eq!(
        table_names(&store.database_path()),
        vec![
            "attempts",
            "directives",
            "events",
            "findings",
            "providers",
            "questions",
            "run_health",
            "runs",
        ]
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_run_survives_a_write_and_a_read_with_every_column_intact() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();

    let mut run = draft_run();
    run.genesis_paths = Some("[\"crates/agens-store/src/database.rs\"]".to_owned());
    run.state = RunState::Running;
    run.worktree_path = Some("/home/dev/.local/share/agens/worktrees/agens-a1b2/agn-53".to_owned());
    run.worktree_status = Some(WorktreeStatus::Active);
    let id = store.insert_run(&run).unwrap();

    let loaded = store.load_run(id).unwrap().unwrap();

    assert_eq!(
        loaded,
        RunRow {
            id: Some(id),
            ..run
        }
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn runs_are_grouped_by_repository_fingerprint() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();

    let first = store.insert_run(&draft_run()).unwrap();
    let second = store.insert_run(&draft_run()).unwrap();
    let other_repository = RunRow {
        repo_id: "ffffffffffffffff".to_owned(),
        ..draft_run()
    };
    store.insert_run(&other_repository).unwrap();

    let grouped = store.runs_for_repo("a1b2c3d4e5f60718").unwrap();

    assert_eq!(
        grouped.iter().map(|run| run.id).collect::<Vec<_>>(),
        vec![Some(first), Some(second)]
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_attempt_carries_its_own_cost_duration_and_correlation() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();

    let first = AttemptRow {
        id: None,
        run_id,
        n: 1,
        session_id: None,
        session_attempt_id: None,
        started_at: 1_700_000_100,
        ended_at: Some(1_700_000_400),
        outcome: Some(AttemptOutcome::Failed),
        retry_trigger: None,
        tokens: Some(12_000),
        cost_micros: Some(43_210),
    };
    let second = AttemptRow {
        n: 2,
        started_at: 1_700_000_500,
        ended_at: None,
        outcome: None,
        retry_trigger: Some(RetryTrigger::User),
        tokens: None,
        cost_micros: None,
        ..first.clone()
    };
    let first_id = store.insert_attempt(&first).unwrap();
    let second_id = store.insert_attempt(&second).unwrap();

    let attempts = store.attempts_for_run(run_id).unwrap();

    assert_eq!(
        attempts,
        vec![
            AttemptRow {
                id: Some(first_id),
                ..first
            },
            AttemptRow {
                id: Some(second_id),
                ..second
            },
        ]
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn two_attempts_cannot_share_a_number_within_one_run() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();
    let attempt = AttemptRow {
        id: None,
        run_id,
        n: 1,
        session_id: None,
        session_attempt_id: None,
        started_at: 1_700_000_100,
        ended_at: None,
        outcome: None,
        retry_trigger: None,
        tokens: None,
        cost_micros: None,
    };
    store.insert_attempt(&attempt).unwrap();

    assert!(store.insert_attempt(&attempt).is_err());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_ended_attempt_must_say_how_it_ended() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();

    let ended_without_outcome = AttemptRow {
        id: None,
        run_id,
        n: 1,
        session_id: None,
        session_attempt_id: None,
        started_at: 1_700_000_100,
        ended_at: Some(1_700_000_200),
        outcome: None,
        retry_trigger: None,
        tokens: None,
        cost_micros: None,
    };

    assert!(store.insert_attempt(&ended_without_outcome).is_err());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn the_journal_orders_events_by_the_id_it_assigns() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();
    let event = EventRow {
        id: None,
        run_id: Some(run_id),
        event_type: "run_state_changed".to_owned(),
        class: EventClass::Infra,
        payload: "{\"to\":\"running\"}".to_owned(),
        ts: 1_700_000_100,
    };

    let first = store.append_event(&event).unwrap();
    let second = store
        .append_event(&EventRow {
            event_type: "checkpoint".to_owned(),
            class: EventClass::Agent,
            ts: 1_700_000_200,
            ..event.clone()
        })
        .unwrap();

    assert!(second > first);
    assert_eq!(
        store
            .events_for_run(run_id)
            .unwrap()
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec![Some(first), Some(second)]
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_event_that_belongs_to_no_run_is_still_journaled() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();

    let quota_reset = EventRow {
        id: None,
        run_id: None,
        event_type: "quota_reset".to_owned(),
        class: EventClass::Infra,
        payload: "{\"provider\":\"anthropic\"}".to_owned(),
        ts: 1_700_000_900,
    };

    assert!(store.append_event(&quota_reset).is_ok());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_approval_freezes_the_receipt_the_merge_gate_re_derives() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();

    let approval = QuestionRow {
        id: None,
        run_id,
        kind: QuestionKind::Approval,
        blocked_decision: "merge run 1 into main".to_owned(),
        options: "[\"authorize\",\"decline\"]".to_owned(),
        recommendation: None,
        answer: None,
        author: None,
        expires_at: Some(1_700_003_600),
        tree_hash: Some("4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_owned()),
        paths_digest: Some("sha256:0f1e2d".to_owned()),
        state: QuestionState::Open,
        created_at: 1_700_000_300,
    };
    let id = store.insert_question(&approval).unwrap();

    let loaded = store.load_question(id).unwrap().unwrap();

    assert_eq!(
        loaded,
        QuestionRow {
            id: Some(id),
            ..approval
        }
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_approval_without_a_receipt_is_rejected() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();

    let receiptless = QuestionRow {
        id: None,
        run_id,
        kind: QuestionKind::Approval,
        blocked_decision: "merge run 1 into main".to_owned(),
        options: "[]".to_owned(),
        recommendation: None,
        answer: None,
        author: None,
        expires_at: None,
        tree_hash: None,
        paths_digest: None,
        state: QuestionState::Open,
        created_at: 1_700_000_300,
    };

    assert!(store.insert_question(&receiptless).is_err());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_plain_question_carries_no_receipt_and_records_its_author() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();

    let answered = QuestionRow {
        id: None,
        run_id,
        kind: QuestionKind::Question,
        blocked_decision: "which store owns the queue".to_owned(),
        options: "[\"reuse\",\"new table\"]".to_owned(),
        recommendation: Some("reuse".to_owned()),
        answer: Some("reuse".to_owned()),
        author: Some(QuestionAuthor::Praetor),
        expires_at: None,
        tree_hash: None,
        paths_digest: None,
        state: QuestionState::Answered,
        created_at: 1_700_000_300,
    };
    let id = store.insert_question(&answered).unwrap();

    let with_receipt = QuestionRow {
        tree_hash: Some("4b825dc".to_owned()),
        paths_digest: Some("sha256:0f".to_owned()),
        ..answered.clone()
    };

    assert_eq!(
        store.load_question(id).unwrap().unwrap(),
        QuestionRow {
            id: Some(id),
            ..answered
        }
    );
    assert!(store.insert_question(&with_receipt).is_err());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_answered_question_cannot_lose_its_author() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();

    let authorless = QuestionRow {
        id: None,
        run_id,
        kind: QuestionKind::Question,
        blocked_decision: "which store owns the queue".to_owned(),
        options: "[]".to_owned(),
        recommendation: None,
        answer: Some("reuse".to_owned()),
        author: None,
        expires_at: None,
        tree_hash: None,
        paths_digest: None,
        state: QuestionState::Answered,
        created_at: 1_700_000_300,
    };

    assert!(store.insert_question(&authorless).is_err());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_finding_points_at_the_checkpoint_event_it_came_from() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();
    let checkpoint_id = store
        .append_event(&EventRow {
            id: None,
            run_id: Some(run_id),
            event_type: "checkpoint".to_owned(),
            class: EventClass::Agent,
            payload: "{}".to_owned(),
            ts: 1_700_000_200,
        })
        .unwrap();

    let finding = FindingRow {
        id: None,
        run_id,
        checkpoint_id: Some(checkpoint_id),
        description: "the migration test is green".to_owned(),
        evidence_class: EvidenceClass::Deterministic,
        proof_refs: "[\"cargo test -p agens-store\"]".to_owned(),
        causal_disposition: CausalDisposition::CandidateCaused,
        created_at: 1_700_000_250,
    };
    let id = store.insert_finding(&finding).unwrap();

    assert_eq!(
        store.findings_for_run(run_id).unwrap(),
        vec![FindingRow {
            id: Some(id),
            ..finding
        }]
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_provider_quota_state_is_replaced_rather_than_appended() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();

    store
        .record_provider(&ProviderRow {
            provider: "anthropic".to_owned(),
            quota_state: QuotaState::Capped,
            reset_at: Some(1_700_007_200),
            updated_at: 1_700_000_400,
        })
        .unwrap();
    let lifted = ProviderRow {
        provider: "anthropic".to_owned(),
        quota_state: QuotaState::Ok,
        reset_at: None,
        updated_at: 1_700_007_300,
    };
    store.record_provider(&lifted).unwrap();

    assert_eq!(store.load_provider("anthropic").unwrap(), Some(lifted));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn health_signals_are_one_replaceable_snapshot_per_run() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();

    store
        .record_run_health(&RunHealthRow {
            run_id,
            last_progress_turn: Some(4),
            noop_turns: 0,
            failing_test_signature: None,
            tokens_since_progress: 0,
            updated_at: 1_700_000_400,
        })
        .unwrap();
    let stalled = RunHealthRow {
        run_id,
        last_progress_turn: Some(4),
        noop_turns: 5,
        failing_test_signature: Some("store::control_plane".to_owned()),
        tokens_since_progress: 31_000,
        updated_at: 1_700_000_900,
    };
    store.record_run_health(&stalled).unwrap();

    assert_eq!(store.load_run_health(run_id).unwrap(), Some(stalled));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn deleting_a_run_takes_its_journal_and_health_with_it() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();
    store
        .append_event(&EventRow {
            id: None,
            run_id: Some(run_id),
            event_type: "run_state_changed".to_owned(),
            class: EventClass::Infra,
            payload: "{}".to_owned(),
            ts: 1_700_000_100,
        })
        .unwrap();
    store
        .record_run_health(&RunHealthRow {
            run_id,
            last_progress_turn: None,
            noop_turns: 0,
            failing_test_signature: None,
            tokens_since_progress: 0,
            updated_at: 1_700_000_100,
        })
        .unwrap();

    let connection = Connection::open(store.database_path()).unwrap();
    connection.execute("PRAGMA foreign_keys = ON", []).unwrap();
    connection
        .execute("DELETE FROM runs WHERE id = ?1", [run_id])
        .unwrap();
    drop(connection);

    assert!(store.events_for_run(run_id).unwrap().is_empty());
    assert_eq!(store.load_run_health(run_id).unwrap(), None);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_applied_transition_writes_the_state_change_and_every_event_together() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();

    let outcome = store
        .apply_transition(&TransitionWrite {
            run_id,
            run_state: Some(StateChange {
                expected: RunState::Draft,
                next: RunState::Queued,
            }),
            worktree_status: None,
            question: None,
            attempt: None,
            provider: None,
            events: &[
                event(run_id, "run_state_changed"),
                event(run_id, "run_approved"),
            ],
        })
        .unwrap();

    assert_eq!(outcome.event_ids.len(), 2);
    assert_eq!(outcome.attempt_id, None);
    assert_eq!(
        store.load_run(run_id).unwrap().unwrap().state,
        RunState::Queued
    );
    assert_eq!(
        event_types(&store, run_id),
        vec!["run_state_changed".to_owned(), "run_approved".to_owned()]
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_transition_against_a_state_the_run_already_left_writes_nothing() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();

    let error = store
        .apply_transition(&TransitionWrite {
            run_id,
            run_state: Some(StateChange {
                expected: RunState::Running,
                next: RunState::Done,
            }),
            worktree_status: None,
            question: None,
            attempt: None,
            provider: None,
            events: &[event(run_id, "run_state_changed")],
        })
        .unwrap_err();

    assert!(error.is_conflict(), "{error}");
    assert_eq!(
        store.load_run(run_id).unwrap().unwrap().state,
        RunState::Draft
    );
    assert!(store.events_for_run(run_id).unwrap().is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_transition_carries_its_attempt_question_and_provider_writes() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&RunRow {
            state: RunState::AwaitingInput,
            worktree_path: Some("/data/worktrees/agens-a1b2c3d4/run-1".to_owned()),
            worktree_status: Some(WorktreeStatus::Active),
            ..draft_run()
        })
        .unwrap();
    let question_id = store
        .insert_question(&QuestionRow {
            id: None,
            run_id,
            kind: QuestionKind::Question,
            blocked_decision: "which crate owns the deriver".to_owned(),
            options: "[\"store\",\"server\"]".to_owned(),
            recommendation: None,
            answer: None,
            author: None,
            expires_at: None,
            tree_hash: None,
            paths_digest: None,
            state: QuestionState::Open,
            created_at: 1_700_000_050,
        })
        .unwrap();

    let outcome = store
        .apply_transition(&TransitionWrite {
            run_id,
            run_state: Some(StateChange {
                expected: RunState::AwaitingInput,
                next: RunState::Queued,
            }),
            worktree_status: Some(StateChange {
                expected: WorktreeStatus::Active,
                next: WorktreeStatus::Reclaimable,
            }),
            question: Some(QuestionChange {
                question_id,
                expected: QuestionState::Open,
                next: QuestionState::Answered,
                answer: Some(QuestionAnswer {
                    answer: "server".to_owned(),
                    author: QuestionAuthor::User,
                }),
            }),
            attempt: Some(&AttemptRow {
                id: None,
                run_id,
                n: 1,
                session_id: None,
                session_attempt_id: None,
                started_at: 1_700_000_100,
                ended_at: None,
                outcome: None,
                retry_trigger: Some(RetryTrigger::User),
                tokens: None,
                cost_micros: None,
            }),
            provider: Some(&ProviderRow {
                provider: "anthropic".to_owned(),
                quota_state: QuotaState::Ok,
                reset_at: None,
                updated_at: 1_700_000_100,
            }),
            events: &[event(run_id, "run_state_changed")],
        })
        .unwrap();

    assert!(outcome.attempt_id.is_some());
    let question = store.load_question(question_id).unwrap().unwrap();
    assert_eq!(question.state, QuestionState::Answered);
    assert_eq!(question.answer.as_deref(), Some("server"));
    assert_eq!(question.author, Some(QuestionAuthor::User));
    assert_eq!(
        store.load_run(run_id).unwrap().unwrap().worktree_status,
        Some(WorktreeStatus::Reclaimable)
    );
    assert_eq!(store.attempts_for_run(run_id).unwrap().len(), 1);
    assert_eq!(
        store
            .load_provider("anthropic")
            .unwrap()
            .unwrap()
            .quota_state,
        QuotaState::Ok
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_question_write_that_loses_its_race_rolls_back_the_run_state_with_it() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&RunRow {
            state: RunState::AwaitingInput,
            ..draft_run()
        })
        .unwrap();
    let question_id = store
        .insert_question(&QuestionRow {
            id: None,
            run_id,
            kind: QuestionKind::Question,
            blocked_decision: "which crate owns the deriver".to_owned(),
            options: "[\"store\",\"server\"]".to_owned(),
            recommendation: None,
            answer: None,
            author: None,
            expires_at: None,
            tree_hash: None,
            paths_digest: None,
            state: QuestionState::Open,
            created_at: 1_700_000_050,
        })
        .unwrap();

    let error = store
        .apply_transition(&TransitionWrite {
            run_id,
            run_state: Some(StateChange {
                expected: RunState::AwaitingInput,
                next: RunState::Queued,
            }),
            worktree_status: None,
            question: Some(QuestionChange {
                question_id,
                expected: QuestionState::Answered,
                next: QuestionState::Delivered,
                answer: None,
            }),
            attempt: None,
            provider: None,
            events: &[event(run_id, "run_state_changed")],
        })
        .unwrap_err();

    assert!(error.is_conflict(), "{error}");
    assert_eq!(
        store.load_run(run_id).unwrap().unwrap().state,
        RunState::AwaitingInput
    );
    assert_eq!(
        store.load_question(question_id).unwrap().unwrap().state,
        QuestionState::Open
    );
    assert!(store.events_for_run(run_id).unwrap().is_empty());

    fs::remove_dir_all(directory).unwrap();
}

fn event(run_id: i64, event_type: &str) -> EventRow {
    EventRow {
        id: None,
        run_id: Some(run_id),
        event_type: event_type.to_owned(),
        class: EventClass::Infra,
        payload: "{}".to_owned(),
        ts: 1_700_000_100,
    }
}

fn event_types(store: &ControlPlaneStore, run_id: i64) -> Vec<String> {
    store
        .events_for_run(run_id)
        .unwrap()
        .into_iter()
        .map(|event| event.event_type)
        .collect()
}

#[test]
fn only_capped_providers_whose_reset_has_arrived_are_due() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();

    store
        .record_provider(&ProviderRow {
            provider: "anthropic".to_owned(),
            quota_state: QuotaState::Capped,
            reset_at: Some(1_700_000_100),
            updated_at: 1_700_000_000,
        })
        .unwrap();
    store
        .record_provider(&ProviderRow {
            provider: "openai".to_owned(),
            quota_state: QuotaState::Capped,
            reset_at: Some(1_700_000_900),
            updated_at: 1_700_000_000,
        })
        .unwrap();
    store
        .record_provider(&ProviderRow {
            provider: "google".to_owned(),
            quota_state: QuotaState::Capped,
            reset_at: None,
            updated_at: 1_700_000_000,
        })
        .unwrap();
    store
        .record_provider(&ProviderRow {
            provider: "moonshot".to_owned(),
            quota_state: QuotaState::Ok,
            reset_at: Some(1_700_000_100),
            updated_at: 1_700_000_000,
        })
        .unwrap();

    let due: Vec<String> = store
        .providers_due(1_700_000_100)
        .unwrap()
        .into_iter()
        .map(|provider| provider.provider)
        .collect();

    assert_eq!(due, vec!["anthropic".to_owned()]);
}

#[test]
fn runs_are_listed_by_state_across_every_repository() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();

    let mut parked = draft_run();
    parked.state = RunState::AwaitingQuota;
    let parked_id = store.insert_run(&parked).unwrap();

    let mut elsewhere = draft_run();
    elsewhere.repo_id = "ffffffffffffffff".to_owned();
    elsewhere.state = RunState::AwaitingQuota;
    let elsewhere_id = store.insert_run(&elsewhere).unwrap();

    let mut running = draft_run();
    running.state = RunState::Running;
    store.insert_run(&running).unwrap();

    let parked_ids: Vec<Option<i64>> = store
        .runs_in_state(RunState::AwaitingQuota)
        .unwrap()
        .into_iter()
        .map(|run| run.id)
        .collect();

    assert_eq!(parked_ids, vec![Some(parked_id), Some(elsewhere_id)]);
}

#[test]
fn questions_past_their_expiry_exclude_settled_and_undated_ones() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();

    let open_and_overdue = store
        .insert_question(&question_row(
            run_id,
            QuestionKind::Approval,
            QuestionState::Open,
            Some(1_700_000_100),
        ))
        .unwrap();
    let answered_and_overdue = store
        .insert_question(&question_row(
            run_id,
            QuestionKind::Approval,
            QuestionState::Answered,
            Some(1_700_000_050),
        ))
        .unwrap();
    store
        .insert_question(&question_row(
            run_id,
            QuestionKind::Question,
            QuestionState::Open,
            Some(1_700_000_900),
        ))
        .unwrap();
    store
        .insert_question(&question_row(
            run_id,
            QuestionKind::Question,
            QuestionState::Open,
            None,
        ))
        .unwrap();
    store
        .insert_question(&question_row(
            run_id,
            QuestionKind::Approval,
            QuestionState::Delivered,
            Some(1_700_000_050),
        ))
        .unwrap();

    let overdue: Vec<Option<i64>> = store
        .questions_past_expiry(1_700_000_100)
        .unwrap()
        .into_iter()
        .map(|question| question.id)
        .collect();

    assert_eq!(
        overdue,
        vec![Some(open_and_overdue), Some(answered_and_overdue)]
    );
}

#[test]
fn one_type_of_event_can_be_read_back_on_its_own() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&draft_run()).unwrap();

    store.append_event(&event(run_id, "checkpoint")).unwrap();
    store.append_event(&event(run_id, "turn_ended")).unwrap();
    let last_checkpoint = store.append_event(&event(run_id, "checkpoint")).unwrap();

    let checkpoints = store.events_of_type_for_run(run_id, "checkpoint").unwrap();

    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints.last().unwrap().id, Some(last_checkpoint));
}

fn question_row(
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
        created_at: 1_700_000_000,
    }
}
