//! The three coordinator state machines, driven through their transition
//! tables.
//!
//! The three table-driven tests walk every row of every machine, so a
//! transition added to a table without a way to reach it fails here rather than
//! shipping unexercised.

use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_server::{
    Principal, QUESTION_TRANSITIONS, QuestionFacts, QuestionGuard, QuestionTransition,
    QuestionTrigger, RUN_RESULT_MAX_BYTES, RUN_TRANSITIONS, RunEffect, RunFacts, RunGuard,
    RunTransition, RunTrigger, StateMachines, TransitionOutcome, TransitionRejection,
    WORKTREE_TRANSITIONS, WorktreeFacts, WorktreeGuard, WorktreeTransition, WorktreeTrigger,
};
use agens_store::{
    AttemptOutcome, AttemptRow, ControlPlaneStore, ProviderRow, QuestionAuthor, QuestionKind,
    QuestionRow, QuestionState, QuotaState, RetryTrigger, RunRow, RunState, WorktreeStatus,
};

const NOW: i64 = 1_700_000_500;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory =
        std::env::temp_dir().join(format!("agens-server-fsm-{}-{suffix}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn run_in(state: RunState, worktree_status: Option<WorktreeStatus>) -> RunRow {
    RunRow {
        id: None,
        repo_id: "a1b2c3d4e5f60718".to_owned(),
        repo_root: "/home/dev/agens".to_owned(),
        remote_url: Some("git@example.com:dev/agens.git".to_owned()),
        external_ref: Some("agens/AGN-54".to_owned()),
        parent_run_id: None,
        task: "the three state machines".to_owned(),
        scope: "crates/agens-server/src/fsm".to_owned(),
        dod: "guards, effects and an event per transition".to_owned(),
        genesis_paths: None,
        state,
        priority: 5,
        dep_run_id: None,
        provider: "anthropic".to_owned(),
        budget_tokens: Some(200_000),
        worktree_path: Some("/data/worktrees/agens-a1b2c3d4/agn-54".to_owned()),
        worktree_status,
        created_at: 1_700_000_000,
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
        created_at: 1_700_000_100,
    }
}

fn event_types(machines: &StateMachines, run_id: i64) -> Vec<String> {
    machines
        .store()
        .events_for_run(run_id)
        .unwrap()
        .into_iter()
        .map(|event| event.event_type)
        .collect()
}

/// The facts that make one run transition's guard hold.
fn facts_for(store: &mut ControlPlaneStore, run_id: i64, transition: &RunTransition) -> RunFacts {
    let mut facts = RunFacts {
        now: NOW,
        ..RunFacts::default()
    };

    match transition.guard {
        RunGuard::None | RunGuard::ReportedByHarness => {}
        RunGuard::UserApproval => facts.principal = Principal::User,
        RunGuard::SchedulerAdmission => {
            facts.slot_available = true;
            facts.provider_serving = true;
            facts.worktree_ready = true;
        }
        RunGuard::AnsweredQuestion => {
            let question_id = store
                .insert_question(&question_in(
                    run_id,
                    QuestionKind::Question,
                    QuestionState::Answered,
                    None,
                ))
                .unwrap();
            facts.answered_question_id = Some(question_id);
        }
        RunGuard::AskOpensQuestion => {
            facts.opened_question = Some(question_in(
                run_id,
                QuestionKind::Question,
                QuestionState::Open,
                None,
            ));
        }
        RunGuard::QuotaResetElapsed => {
            store
                .record_provider(&ProviderRow {
                    provider: "anthropic".to_owned(),
                    quota_state: QuotaState::Capped,
                    reset_at: Some(NOW - 1),
                    updated_at: NOW - 60,
                })
                .unwrap();
        }
        RunGuard::RetryEligible => {
            facts.guidance = Some("keep the transition table as data".to_owned());
            facts.retry_trigger = Some(RetryTrigger::User);
            facts.retry_budget = 3;
        }
        RunGuard::BootReconciliation => facts.boot_reconciliation = true,
    }

    facts
}

fn worktree_facts_for(transition: &WorktreeTransition) -> WorktreeFacts {
    let mut facts = WorktreeFacts {
        now: NOW,
        ..WorktreeFacts::default()
    };

    match transition.guard {
        WorktreeGuard::MergeReDerived => facts.merge_re_derived = true,
        WorktreeGuard::WorktreeClean => facts.worktree_clean = true,
        WorktreeGuard::ConfirmedManualDisposition => facts.manual_disposition_confirmed = true,
    }

    facts
}

fn question_facts_for(transition: &QuestionTransition) -> QuestionFacts {
    match transition.guard {
        QuestionGuard::AuthorRecorded | QuestionGuard::UserAuthorizationInDate => QuestionFacts {
            now: NOW,
            answer: Some("yes".to_owned()),
            author: Some(QuestionAuthor::User),
            ..QuestionFacts::default()
        },
        QuestionGuard::NotExpired | QuestionGuard::Expired | QuestionGuard::None => QuestionFacts {
            now: NOW,
            ..QuestionFacts::default()
        },
    }
}

#[test]
fn every_run_transition_moves_the_run_and_journals_the_generic_event_with_its_domain_event() {
    for transition in RUN_TRANSITIONS {
        let directory = data_directory();
        let mut store = ControlPlaneStore::open(&directory).unwrap();
        let run_id = store
            .insert_run(&run_in(transition.from, Some(WorktreeStatus::Active)))
            .unwrap();
        let facts = facts_for(&mut store, run_id, transition);
        let mut machines = StateMachines::new(store);

        let outcome = machines
            .apply_run(run_id, transition.trigger, &facts)
            .unwrap_or_else(|error| {
                panic!(
                    "{} on {} was refused: {error}",
                    transition.trigger.as_str(),
                    transition.from.as_str()
                )
            });
        let applied = outcome.applied().expect("the transition should have moved");

        assert_eq!(applied.from, transition.from);
        assert_eq!(applied.to, transition.to);
        assert_eq!(applied.effects, transition.effects);
        assert_eq!(applied.domain_event, transition.domain_event);
        assert_ne!(applied.state_changed_event_id, applied.domain_event_id);
        assert_eq!(
            machines.store().load_run(run_id).unwrap().unwrap().state,
            transition.to
        );
        assert_eq!(
            event_types(&machines, run_id),
            vec![
                "run_state_changed".to_owned(),
                transition.domain_event.to_owned()
            ],
            "{} on {}",
            transition.trigger.as_str(),
            transition.from.as_str()
        );

        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn every_worktree_transition_moves_the_disposition_and_journals_both_events() {
    for transition in WORKTREE_TRANSITIONS {
        let directory = data_directory();
        let mut store = ControlPlaneStore::open(&directory).unwrap();
        let run_id = store
            .insert_run(&run_in(RunState::Running, Some(transition.from)))
            .unwrap();
        let mut machines = StateMachines::new(store);

        let outcome = machines
            .apply_worktree(run_id, transition.trigger, &worktree_facts_for(transition))
            .unwrap();
        let applied = outcome.applied().expect("the transition should have moved");

        assert_eq!(applied.from, transition.from);
        assert_eq!(applied.to, transition.to);
        assert_eq!(applied.effects, transition.effects);
        assert_eq!(
            machines
                .store()
                .load_run(run_id)
                .unwrap()
                .unwrap()
                .worktree_status,
            Some(transition.to)
        );
        assert_eq!(
            event_types(&machines, run_id),
            vec![
                "run_state_changed".to_owned(),
                transition.domain_event.to_owned()
            ],
            "{} on {}",
            transition.trigger.as_str(),
            transition.from.as_str()
        );

        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn every_question_transition_moves_the_question_and_journals_both_events() {
    for transition in QUESTION_TRANSITIONS {
        let directory = data_directory();
        let mut store = ControlPlaneStore::open(&directory).unwrap();
        let run_id = store
            .insert_run(&run_in(RunState::AwaitingInput, None))
            .unwrap();
        let expires_at = if transition.guard == QuestionGuard::Expired {
            Some(NOW - 1)
        } else {
            Some(NOW + 3600)
        };
        let question_id = store
            .insert_question(&question_in(
                run_id,
                transition.kind,
                transition.from,
                expires_at,
            ))
            .unwrap();
        let mut machines = StateMachines::new(store);

        let outcome = machines
            .apply_question(
                question_id,
                transition.trigger,
                &question_facts_for(transition),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{} on a {} in {} was refused: {error}",
                    transition.trigger.as_str(),
                    transition.kind.as_str(),
                    transition.from.as_str()
                )
            });
        let applied = outcome.applied().expect("the transition should have moved");

        assert_eq!(applied.from, transition.from);
        assert_eq!(applied.to, transition.to);
        assert_eq!(applied.effects, transition.effects);
        assert_eq!(
            machines
                .store()
                .load_question(question_id)
                .unwrap()
                .unwrap()
                .state,
            transition.to
        );
        assert_eq!(
            event_types(&machines, run_id),
            vec![
                "run_state_changed".to_owned(),
                transition.domain_event.to_owned()
            ],
            "{} on a {} in {}",
            transition.trigger.as_str(),
            transition.kind.as_str(),
            transition.from.as_str()
        );

        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn a_transition_the_table_does_not_have_is_refused_before_anything_is_written() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Queued, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let rejection = machines
        .apply_run(
            run_id,
            RunTrigger::Finished,
            &RunFacts {
                now: NOW,
                ..RunFacts::default()
            },
        )
        .unwrap_err();

    assert_eq!(
        rejection,
        TransitionRejection::NoSuchTransition {
            machine: "run",
            from: "queued",
            trigger: "finished",
        }
    );
    assert_eq!(
        machines.store().load_run(run_id).unwrap().unwrap().state,
        RunState::Queued
    );
    assert!(event_types(&machines, run_id).is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_failed_guard_leaves_the_run_and_the_journal_untouched() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Draft, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let rejection = machines
        .apply_run(
            run_id,
            RunTrigger::Approve,
            &RunFacts {
                now: NOW,
                principal: Principal::Praetor,
                ..RunFacts::default()
            },
        )
        .unwrap_err();

    assert!(
        matches!(
            rejection,
            TransitionRejection::GuardFailed {
                guard: "user_approval",
                ..
            }
        ),
        "{rejection}"
    );
    assert_eq!(
        machines.store().load_run(run_id).unwrap().unwrap().state,
        RunState::Draft
    );
    assert!(event_types(&machines, run_id).is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_client_cannot_report_a_runs_own_lifecycle_facts() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let rejection = machines
        .apply_run(
            run_id,
            RunTrigger::Finished,
            &RunFacts {
                now: NOW,
                principal: Principal::User,
                ..RunFacts::default()
            },
        )
        .unwrap_err();

    assert!(
        matches!(
            rejection,
            TransitionRejection::GuardFailed {
                guard: "reported_by_harness",
                ..
            }
        ),
        "{rejection}"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_run_whose_work_already_landed_is_not_retried() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Done, Some(WorktreeStatus::Reclaimable)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let rejection = machines
        .apply_run(
            run_id,
            RunTrigger::Retry,
            &RunFacts {
                now: NOW,
                guidance: Some("take the other branch".to_owned()),
                retry_budget: 3,
                ..RunFacts::default()
            },
        )
        .unwrap_err();

    assert!(
        matches!(
            rejection,
            TransitionRejection::GuardFailed {
                guard: "retry_eligible",
                ..
            }
        ),
        "{rejection}"
    );
    assert_eq!(
        machines.store().load_run(run_id).unwrap().unwrap().state,
        RunState::Done
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_retry_needs_guidance_and_budget_and_a_resumed_leg_spends_neither() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Failed, Some(WorktreeStatus::Active)))
        .unwrap();
    store
        .insert_attempt(&AttemptRow {
            id: None,
            run_id,
            n: 1,
            session_id: None,
            session_attempt_id: None,
            started_at: 1_700_000_200,
            ended_at: Some(1_700_000_300),
            outcome: Some(AttemptOutcome::Failed),
            retry_trigger: None,
            tokens: None,
            cost_micros: None,
        })
        .unwrap();
    store
        .insert_attempt(&AttemptRow {
            id: None,
            run_id,
            n: 2,
            session_id: None,
            session_attempt_id: None,
            started_at: 1_700_000_310,
            ended_at: Some(1_700_000_320),
            outcome: Some(AttemptOutcome::Interrupted),
            retry_trigger: None,
            tokens: None,
            cost_micros: None,
        })
        .unwrap();
    let mut machines = StateMachines::new(store);

    let without_guidance = machines
        .apply_run(
            run_id,
            RunTrigger::Retry,
            &RunFacts {
                now: NOW,
                retry_budget: 2,
                ..RunFacts::default()
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            without_guidance,
            TransitionRejection::GuardFailed {
                guard: "retry_eligible",
                ..
            }
        ),
        "{without_guidance}"
    );

    let over_budget = machines
        .apply_run(
            run_id,
            RunTrigger::Retry,
            &RunFacts {
                now: NOW,
                guidance: Some("try the other provider".to_owned()),
                retry_budget: 1,
                ..RunFacts::default()
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            over_budget,
            TransitionRejection::GuardFailed {
                guard: "retry_eligible",
                ..
            }
        ),
        "{over_budget}"
    );

    // Two attempt rows, one of them interrupted: only the failed one is
    // chargeable, so a budget of two still has room.
    let allowed = machines.apply_run(
        run_id,
        RunTrigger::Retry,
        &RunFacts {
            now: NOW,
            guidance: Some("try the other provider".to_owned()),
            retry_budget: 2,
            ..RunFacts::default()
        },
    );
    assert!(allowed.is_ok(), "{:?}", allowed.err());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn only_boot_reconciliation_interrupts_a_run() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let rejection = machines
        .apply_run(
            run_id,
            RunTrigger::Reconcile,
            &RunFacts {
                now: NOW,
                ..RunFacts::default()
            },
        )
        .unwrap_err();

    assert!(
        matches!(
            rejection,
            TransitionRejection::GuardFailed {
                guard: "boot_reconciliation",
                ..
            }
        ),
        "{rejection}"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn admission_without_a_slot_a_serving_provider_or_a_worktree_is_refused() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Queued, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let rejection = machines
        .apply_run(
            run_id,
            RunTrigger::Admit,
            &RunFacts {
                now: NOW,
                slot_available: true,
                provider_serving: false,
                worktree_ready: true,
                ..RunFacts::default()
            },
        )
        .unwrap_err();

    assert!(
        matches!(
            rejection,
            TransitionRejection::GuardFailed {
                guard: "scheduler_admission",
                ..
            }
        ),
        "{rejection}"
    );
    assert!(
        machines
            .store()
            .attempts_for_run(run_id)
            .unwrap()
            .is_empty()
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn admission_opens_the_attempt_and_parking_records_the_providers_reset() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Queued, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    machines
        .apply_run(
            run_id,
            RunTrigger::Admit,
            &RunFacts {
                now: NOW,
                slot_available: true,
                provider_serving: true,
                worktree_ready: true,
                retry_trigger: Some(RetryTrigger::User),
                session_attempt_id: None,
                ..RunFacts::default()
            },
        )
        .unwrap();

    let attempts = machines.store().attempts_for_run(run_id).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].n, 1);
    assert_eq!(attempts[0].retry_trigger, Some(RetryTrigger::User));
    assert_eq!(attempts[0].started_at, NOW);

    machines
        .apply_run(
            run_id,
            RunTrigger::QuotaReached,
            &RunFacts {
                now: NOW,
                quota_reset_at: Some(NOW + 3600),
                ..RunFacts::default()
            },
        )
        .unwrap();

    let provider = machines
        .store()
        .load_provider("anthropic")
        .unwrap()
        .unwrap();
    assert_eq!(provider.quota_state, QuotaState::Capped);
    assert_eq!(provider.reset_at, Some(NOW + 3600));

    let rejection = machines
        .apply_run(
            run_id,
            RunTrigger::QuotaReset,
            &RunFacts {
                now: NOW,
                ..RunFacts::default()
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            rejection,
            TransitionRejection::GuardFailed {
                guard: "quota_reset_elapsed",
                ..
            }
        ),
        "{rejection}"
    );

    machines
        .apply_run(
            run_id,
            RunTrigger::QuotaReset,
            &RunFacts {
                now: NOW + 3600,
                ..RunFacts::default()
            },
        )
        .unwrap();
    assert_eq!(
        machines
            .store()
            .load_provider("anthropic")
            .unwrap()
            .unwrap()
            .quota_state,
        QuotaState::Ok
    );

    fs::remove_dir_all(directory).unwrap();
}

/// A provider that refuses without naming a reset would otherwise strand every
/// run parked on it: the cap is only ever lifted by a run reaching that
/// provider, and no parked run is allowed to start.
#[test]
fn a_cap_that_named_no_reset_lifts_a_window_after_it_was_recorded() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    machines
        .apply_run(
            run_id,
            RunTrigger::QuotaReached,
            &RunFacts {
                now: NOW,
                principal: Principal::Coordinator,
                quota_reset_at: None,
                ..RunFacts::default()
            },
        )
        .unwrap();

    let provider = machines
        .store()
        .load_provider("anthropic")
        .unwrap()
        .unwrap();
    assert_eq!(provider.quota_state, QuotaState::Capped);
    assert_eq!(provider.reset_at, None);

    let rejection = machines
        .apply_run(
            run_id,
            RunTrigger::QuotaReset,
            &RunFacts {
                now: NOW + 900,
                ..RunFacts::default()
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            rejection,
            TransitionRejection::GuardFailed {
                guard: "quota_reset_elapsed",
                ..
            }
        ),
        "without a configured window such a cap waits for a fresh report: {rejection}"
    );

    let rejection = machines
        .apply_run(
            run_id,
            RunTrigger::QuotaReset,
            &RunFacts {
                now: NOW + 899,
                quota_window_seconds: Some(900),
                ..RunFacts::default()
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            rejection,
            TransitionRejection::GuardFailed {
                guard: "quota_reset_elapsed",
                ..
            }
        ),
        "the window is measured from when the cap was recorded: {rejection}"
    );

    machines
        .apply_run(
            run_id,
            RunTrigger::QuotaReset,
            &RunFacts {
                now: NOW + 900,
                quota_window_seconds: Some(900),
                ..RunFacts::default()
            },
        )
        .unwrap();
    assert_eq!(
        machines
            .store()
            .load_provider("anthropic")
            .unwrap()
            .unwrap()
            .quota_state,
        QuotaState::Ok
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn cancelling_a_cancelled_run_moves_nothing_and_journals_nothing() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Cancelled, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let outcome = machines
        .apply_run(
            run_id,
            RunTrigger::Cancel,
            &RunFacts {
                now: NOW,
                ..RunFacts::default()
            },
        )
        .unwrap();

    assert_eq!(outcome, TransitionOutcome::AlreadySettled);
    assert!(event_types(&machines, run_id).is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_active_worktree_is_only_discarded_by_a_confirmed_manual_disposition() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Done, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let rejection = machines
        .apply_worktree(
            run_id,
            WorktreeTrigger::ManualDisposition,
            &WorktreeFacts {
                now: NOW,
                manual_disposition_confirmed: false,
                ..WorktreeFacts::default()
            },
        )
        .unwrap_err();

    assert!(
        matches!(
            rejection,
            TransitionRejection::GuardFailed {
                guard: "confirmed_manual_disposition",
                ..
            }
        ),
        "{rejection}"
    );
    assert_eq!(
        machines
            .store()
            .load_run(run_id)
            .unwrap()
            .unwrap()
            .worktree_status,
        Some(WorktreeStatus::Active)
    );
    assert!(event_types(&machines, run_id).is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_worktree_is_only_reclaimable_when_the_merge_was_re_derived() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Done, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let rejection = machines
        .apply_worktree(
            run_id,
            WorktreeTrigger::MergeDetected,
            &WorktreeFacts {
                now: NOW,
                ..WorktreeFacts::default()
            },
        )
        .unwrap_err();

    assert!(
        matches!(
            rejection,
            TransitionRejection::GuardFailed {
                guard: "merge_re_derived",
                ..
            }
        ),
        "{rejection}"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_dirty_worktree_is_not_reclaimed() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Done, Some(WorktreeStatus::Reclaimable)))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let rejection = machines
        .apply_worktree(
            run_id,
            WorktreeTrigger::Reclaim,
            &WorktreeFacts {
                now: NOW,
                worktree_clean: false,
                ..WorktreeFacts::default()
            },
        )
        .unwrap_err();

    assert!(
        matches!(
            rejection,
            TransitionRejection::GuardFailed {
                guard: "worktree_clean",
                ..
            }
        ),
        "{rejection}"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_authorization_is_the_users_alone() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    let question_id = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Approval,
            QuestionState::Open,
            Some(NOW + 600),
        ))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let rejection = machines
        .apply_question(
            question_id,
            QuestionTrigger::Answer,
            &QuestionFacts {
                now: NOW,
                answer: Some("yes".to_owned()),
                author: Some(QuestionAuthor::Praetor),
                ..QuestionFacts::default()
            },
        )
        .unwrap_err();

    assert!(
        matches!(
            rejection,
            TransitionRejection::GuardFailed {
                guard: "user_authorization_in_date",
                ..
            }
        ),
        "{rejection}"
    );
    assert_eq!(
        machines
            .store()
            .load_question(question_id)
            .unwrap()
            .unwrap()
            .state,
        QuestionState::Open
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_expired_authorization_cannot_be_granted_or_handed_over() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    let open_id = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Approval,
            QuestionState::Open,
            Some(NOW - 1),
        ))
        .unwrap();
    let granted_id = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Approval,
            QuestionState::Answered,
            Some(NOW - 1),
        ))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let granting = machines
        .apply_question(
            open_id,
            QuestionTrigger::Answer,
            &QuestionFacts {
                now: NOW,
                answer: Some("yes".to_owned()),
                author: Some(QuestionAuthor::User),
                ..QuestionFacts::default()
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            granting,
            TransitionRejection::GuardFailed {
                guard: "user_authorization_in_date",
                ..
            }
        ),
        "{granting}"
    );

    let handing_over = machines
        .apply_question(
            granted_id,
            QuestionTrigger::Deliver,
            &QuestionFacts {
                now: NOW,
                ..QuestionFacts::default()
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            handing_over,
            TransitionRejection::GuardFailed {
                guard: "not_expired",
                ..
            }
        ),
        "{handing_over}"
    );

    assert!(event_types(&machines, run_id).is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_consumed_authorization_cannot_be_used_again() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    let question_id = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Approval,
            QuestionState::Open,
            Some(NOW + 600),
        ))
        .unwrap();
    let mut machines = StateMachines::new(store);

    machines
        .apply_question(
            question_id,
            QuestionTrigger::Answer,
            &QuestionFacts {
                now: NOW,
                answer: Some("yes".to_owned()),
                author: Some(QuestionAuthor::User),
                ..QuestionFacts::default()
            },
        )
        .unwrap();
    machines
        .apply_question(
            question_id,
            QuestionTrigger::Deliver,
            &QuestionFacts {
                now: NOW,
                ..QuestionFacts::default()
            },
        )
        .unwrap();

    let reuse = machines
        .apply_question(
            question_id,
            QuestionTrigger::Deliver,
            &QuestionFacts {
                now: NOW,
                ..QuestionFacts::default()
            },
        )
        .unwrap_err();

    assert_eq!(
        reuse,
        TransitionRejection::NoSuchTransition {
            machine: "question",
            from: "delivered",
            trigger: "deliver",
        }
    );
    assert_eq!(
        event_types(&machines, run_id),
        vec![
            "run_state_changed".to_owned(),
            "approval_granted".to_owned(),
            "run_state_changed".to_owned(),
            "approval_consumed".to_owned(),
        ]
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_answer_records_its_author_and_unblocks_the_run_that_asked() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(
            RunState::AwaitingInput,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let question_id = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Question,
            QuestionState::Open,
            None,
        ))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let anonymous = machines
        .apply_question(
            question_id,
            QuestionTrigger::Answer,
            &QuestionFacts {
                now: NOW,
                answer: Some("the server crate".to_owned()),
                author: None,
                ..QuestionFacts::default()
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            anonymous,
            TransitionRejection::GuardFailed {
                guard: "author_recorded",
                ..
            }
        ),
        "{anonymous}"
    );

    machines
        .apply_question(
            question_id,
            QuestionTrigger::Answer,
            &QuestionFacts {
                now: NOW,
                answer: Some("the server crate".to_owned()),
                author: Some(QuestionAuthor::Praetor),
                ..QuestionFacts::default()
            },
        )
        .unwrap();

    let question = machines
        .store()
        .load_question(question_id)
        .unwrap()
        .unwrap();
    assert_eq!(question.state, QuestionState::Answered);
    assert_eq!(question.author, Some(QuestionAuthor::Praetor));

    machines
        .apply_run(
            run_id,
            RunTrigger::Answered,
            &RunFacts {
                now: NOW,
                answered_question_id: Some(question_id),
                ..RunFacts::default()
            },
        )
        .unwrap();
    assert_eq!(
        machines.store().load_run(run_id).unwrap().unwrap().state,
        RunState::Queued
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_run_does_not_resume_on_a_question_that_is_still_open_or_belongs_elsewhere() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(
            RunState::AwaitingInput,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let other_run_id = store
        .insert_run(&run_in(
            RunState::AwaitingInput,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let open_id = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Question,
            QuestionState::Open,
            None,
        ))
        .unwrap();
    let elsewhere_id = store
        .insert_question(&question_in(
            other_run_id,
            QuestionKind::Question,
            QuestionState::Answered,
            None,
        ))
        .unwrap();
    let mut machines = StateMachines::new(store);

    for question_id in [open_id, elsewhere_id] {
        let rejection = machines
            .apply_run(
                run_id,
                RunTrigger::Answered,
                &RunFacts {
                    now: NOW,
                    answered_question_id: Some(question_id),
                    ..RunFacts::default()
                },
            )
            .unwrap_err();

        assert!(
            matches!(
                rejection,
                TransitionRejection::GuardFailed {
                    guard: "answered_question",
                    ..
                }
            ),
            "{rejection}"
        );
    }

    assert_eq!(
        machines.store().load_run(run_id).unwrap().unwrap().state,
        RunState::AwaitingInput
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn every_run_parked_on_one_provider_wakes_once_its_cap_is_lifted() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let first = store
        .insert_run(&run_in(
            RunState::AwaitingQuota,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let second = store
        .insert_run(&run_in(
            RunState::AwaitingQuota,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    store
        .record_provider(&ProviderRow {
            provider: "anthropic".to_owned(),
            quota_state: QuotaState::Capped,
            reset_at: Some(NOW),
            updated_at: NOW - 3600,
        })
        .unwrap();
    let mut machines = StateMachines::new(store);

    let facts = RunFacts {
        now: NOW,
        ..RunFacts::default()
    };

    machines
        .apply_run(first, RunTrigger::QuotaReset, &facts)
        .unwrap();

    assert_eq!(
        machines
            .store()
            .load_provider("anthropic")
            .unwrap()
            .unwrap()
            .reset_at,
        None,
        "the first run out lifts the cap, leaving no reset time behind it"
    );

    machines
        .apply_run(second, RunTrigger::QuotaReset, &facts)
        .unwrap();

    assert_eq!(
        machines.store().load_run(second).unwrap().unwrap().state,
        RunState::Queued
    );

    fs::remove_dir_all(directory).unwrap();
}

/// The open attempt of one run, for the assertions about how a leg ended.
fn open_attempt(run_id: i64, started_at: i64) -> AttemptRow {
    AttemptRow {
        id: None,
        run_id,
        n: 1,
        session_id: None,
        session_attempt_id: None,
        started_at,
        ended_at: None,
        outcome: None,
        retry_trigger: None,
        tokens: None,
        cost_micros: None,
    }
}

#[test]
fn every_transition_that_declares_it_closes_the_open_attempt_with_the_declared_outcome() {
    for transition in RUN_TRANSITIONS {
        let Some(declared) = transition.effects.iter().find_map(|effect| match effect {
            RunEffect::CloseAttempt(outcome) => Some(*outcome),
            _ => None,
        }) else {
            continue;
        };

        let directory = data_directory();
        let mut store = ControlPlaneStore::open(&directory).unwrap();
        let run_id = store
            .insert_run(&run_in(transition.from, Some(WorktreeStatus::Active)))
            .unwrap();
        store
            .insert_attempt(&open_attempt(run_id, NOW - 90))
            .unwrap();
        let facts = facts_for(&mut store, run_id, transition);
        let mut machines = StateMachines::new(store);

        machines
            .apply_run(run_id, transition.trigger, &facts)
            .unwrap_or_else(|error| {
                panic!(
                    "{} on {} was refused: {error}",
                    transition.trigger.as_str(),
                    transition.from.as_str()
                )
            });

        let attempt = machines
            .store()
            .attempts_for_run(run_id)
            .unwrap()
            .into_iter()
            .find(|attempt| attempt.n == 1)
            .expect("the attempt the leg ran as is still there");

        assert_eq!(
            attempt.outcome,
            Some(declared),
            "{} on {}",
            transition.trigger.as_str(),
            transition.from.as_str()
        );
        assert_eq!(attempt.ended_at, Some(NOW));

        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn a_run_that_only_ever_parked_keeps_its_whole_retry_budget() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    store
        .insert_attempt(&open_attempt(run_id, NOW - 90))
        .unwrap();
    let mut machines = StateMachines::new(store);

    // Three legs that end without the agent having failed: an `ask`, the
    // answer that brings it back, and a restart that cuts the next one off.
    machines
        .apply_run(
            run_id,
            RunTrigger::Ask,
            &RunFacts {
                now: NOW,
                opened_question: Some(question_in(
                    run_id,
                    QuestionKind::Question,
                    QuestionState::Open,
                    None,
                )),
                ..RunFacts::default()
            },
        )
        .unwrap();

    let question_id = machines
        .store()
        .questions_for_run(run_id)
        .unwrap()
        .first()
        .and_then(|question| question.id)
        .expect("the ask opened a question");

    machines
        .apply_question(
            question_id,
            agens_server::QuestionTrigger::Answer,
            &QuestionFacts {
                now: NOW,
                answer: Some("keep it".to_owned()),
                author: Some(QuestionAuthor::User),
                ..QuestionFacts::default()
            },
        )
        .unwrap();
    machines
        .apply_run(
            run_id,
            RunTrigger::Answered,
            &RunFacts {
                now: NOW,
                answered_question_id: Some(question_id),
                ..RunFacts::default()
            },
        )
        .unwrap();
    machines
        .apply_run(
            run_id,
            RunTrigger::Admit,
            &RunFacts {
                now: NOW,
                slot_available: true,
                provider_serving: true,
                worktree_ready: true,
                ..RunFacts::default()
            },
        )
        .unwrap();
    machines
        .apply_run(
            run_id,
            RunTrigger::Reconcile,
            &RunFacts {
                now: NOW,
                boot_reconciliation: true,
                ..RunFacts::default()
            },
        )
        .unwrap();

    let parked: Vec<Option<AttemptOutcome>> = machines
        .store()
        .attempts_for_run(run_id)
        .unwrap()
        .into_iter()
        .map(|attempt| attempt.outcome)
        .collect();
    assert_eq!(
        parked,
        vec![
            Some(AttemptOutcome::Interrupted),
            Some(AttemptOutcome::Interrupted)
        ]
    );

    machines
        .apply_run(
            run_id,
            RunTrigger::Resume,
            &RunFacts {
                now: NOW,
                ..RunFacts::default()
            },
        )
        .unwrap();
    machines
        .apply_run(
            run_id,
            RunTrigger::Admit,
            &RunFacts {
                now: NOW,
                slot_available: true,
                provider_serving: true,
                worktree_ready: true,
                ..RunFacts::default()
            },
        )
        .unwrap();
    machines
        .apply_run(
            run_id,
            RunTrigger::AttemptFailed,
            &RunFacts {
                now: NOW,
                ..RunFacts::default()
            },
        )
        .unwrap();

    // Four attempt rows, and only the last one failed. A budget of two has
    // room for exactly one more, which is what a run that parked three times
    // and failed once should have: before the legs were closed, all four
    // counted and the retry was refused with the budget untouched.
    let retried = machines.apply_run(
        run_id,
        RunTrigger::Retry,
        &RunFacts {
            now: NOW,
            guidance: Some("read the failing test first".to_owned()),
            retry_budget: 2,
            ..RunFacts::default()
        },
    );
    assert!(retried.is_ok(), "{:?}", retried.err());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn finishing_records_the_turns_last_message_as_the_runs_result() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    store
        .insert_attempt(&open_attempt(run_id, NOW - 90))
        .unwrap();
    let mut machines = StateMachines::new(store);

    machines
        .apply_run(
            run_id,
            RunTrigger::Finished,
            &RunFacts {
                now: NOW,
                result: Some("the definition of done is met".to_owned()),
                ..RunFacts::default()
            },
        )
        .unwrap();

    assert_eq!(
        machines
            .store()
            .load_run(run_id)
            .unwrap()
            .unwrap()
            .result
            .as_deref(),
        Some("the definition of done is met")
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_failing_turn_records_no_result_even_when_one_travels_with_the_facts() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    store
        .insert_attempt(&open_attempt(run_id, NOW - 90))
        .unwrap();
    let mut machines = StateMachines::new(store);

    machines
        .apply_run(
            run_id,
            RunTrigger::AttemptFailed,
            &RunFacts {
                now: NOW,
                result: Some("a claim the failed leg does not get to make".to_owned()),
                ..RunFacts::default()
            },
        )
        .unwrap();

    assert_eq!(
        machines.store().load_run(run_id).unwrap().unwrap().result,
        None
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_oversized_result_is_cut_at_the_bound_on_a_character_boundary() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    store
        .insert_attempt(&open_attempt(run_id, NOW - 90))
        .unwrap();
    let mut machines = StateMachines::new(store);

    // Two-byte characters, so the byte bound falls inside one of them and the
    // cut has to step back to the boundary rather than split it.
    let oversized = "é".repeat(RUN_RESULT_MAX_BYTES / 2 + 8);
    machines
        .apply_run(
            run_id,
            RunTrigger::Finished,
            &RunFacts {
                now: NOW,
                result: Some(oversized.clone()),
                ..RunFacts::default()
            },
        )
        .unwrap();

    let result = machines
        .store()
        .load_run(run_id)
        .unwrap()
        .unwrap()
        .result
        .expect("the finish recorded a result");

    assert!(result.len() <= RUN_RESULT_MAX_BYTES);
    assert!(oversized.starts_with(&result));
    assert!(result.len() > RUN_RESULT_MAX_BYTES - 2);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn the_ask_domain_event_names_the_question_it_opened() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    store
        .insert_attempt(&open_attempt(run_id, NOW - 90))
        .unwrap();
    let mut machines = StateMachines::new(store);

    let outcome = machines
        .apply_run(
            run_id,
            RunTrigger::Ask,
            &RunFacts {
                now: NOW,
                opened_question: Some(question_in(
                    run_id,
                    QuestionKind::Question,
                    QuestionState::Open,
                    None,
                )),
                ..RunFacts::default()
            },
        )
        .unwrap();
    let opened = outcome
        .applied()
        .and_then(|applied| applied.opened_question_id)
        .expect("the ask opened a question");

    let event = machines
        .store()
        .events_for_run(run_id)
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == "run_awaiting_input")
        .expect("the ask journaled its domain event");
    let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();

    assert_eq!(
        payload.get("opened_question_id").and_then(|id| id.as_i64()),
        Some(opened)
    );
    assert_eq!(
        payload.get("question_id"),
        Some(&serde_json::Value::Null),
        "the answered-question field stays what it was: nothing was answered here"
    );

    fs::remove_dir_all(directory).unwrap();
}
