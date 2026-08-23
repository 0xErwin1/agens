//! Admission: eligibility, order, the three ceilings, and what the queue says
//! when it cannot give a slot out.

use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use agens_core::SessionMetadata;
use agens_server::{
    Admission, AdmissionFailure, Deferral, Ineligible, LaunchError, LaunchedSession, PendingRun,
    Principal, QuestionFacts, QuestionTrigger, QueueReport, RunFacts, RunLauncher, RunSession,
    RunTrigger, Scheduler, SchedulerLimits, SchedulerLoad, SessionAdmission, SessionBudget,
    SessionId, SessionOutcome, SessionProvider, SessionRuntime, SessionSupervisor, StateMachines,
    SupervisorLauncher,
};
use agens_store::{
    ControlPlaneStore, ProviderRow, QuestionAuthor, QuestionKind, QuestionRow, QuestionState,
    QuotaState, RunRow, RunState, SessionStore, StateChange, TransitionWrite, WorktreeStatus,
};

const NOW: i64 = 1_700_001_000;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-server-scheduler-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

/// One control plane, with a writer for the test's own setup alongside the
/// machines under test.
///
/// The state machines own their store because they are the only thing allowed
/// to move the state they govern. A test still has to put rows there to begin
/// with, so it opens its own handle to the same database rather than being
/// handed a way around that ownership.
struct Harness {
    directory: PathBuf,
    setup: ControlPlaneStore,
    sessions: SessionStore,
    machines: StateMachines,
}

impl Harness {
    fn new() -> Self {
        let directory = data_directory();

        Self {
            setup: ControlPlaneStore::open(&directory).unwrap(),
            sessions: SessionStore::open(&directory).unwrap(),
            machines: StateMachines::new(ControlPlaneStore::open(&directory).unwrap()),
            directory,
        }
    }

    /// Opens a real session and physical attempt.
    ///
    /// The control plane's `attempts` row points at both, so a correlation to
    /// ids that do not exist is refused by the schema rather than recorded. A
    /// launcher that reports a session has, by then, started one.
    fn open_session(&mut self) -> (i64, i64) {
        open_session_in(&mut self.sessions)
    }

    fn insert(&mut self, draft: &Draft) -> i64 {
        self.setup.insert_run(&draft.row()).unwrap()
    }

    fn cap(&mut self, provider: &str, reset_at: Option<i64>) {
        self.setup
            .record_provider(&ProviderRow {
                provider: provider.to_owned(),
                quota_state: QuotaState::Capped,
                reset_at,
                updated_at: NOW,
            })
            .unwrap();
    }

    fn reclaim(&mut self, run_id: i64) {
        self.setup
            .apply_transition(&TransitionWrite {
                run_id,
                run_state: None,
                worktree_status: Some(StateChange {
                    expected: WorktreeStatus::Active,
                    next: WorktreeStatus::Reclaimable,
                }),
                question: None,
                new_question: None,
                attempt: None,
                close_attempt: None,
                provider: None,
                events: &[],
            })
            .unwrap();
    }

    fn state_of(&self, run_id: i64) -> RunState {
        self.machines
            .store()
            .load_run(run_id)
            .unwrap()
            .unwrap()
            .state
    }

    fn tick(&mut self, limits: SchedulerLimits) -> (QueueReport, RecordingLauncher) {
        let launcher = RecordingLauncher::new(&self.directory);
        let report = Scheduler::new(limits)
            .tick(&mut self.machines, &launcher, &load())
            .unwrap();

        (report, launcher)
    }

    /// Walks a running run out to `queued` the way a restart does, so its
    /// journal carries the move resumed priority is read from.
    fn interrupt_and_requeue(&mut self, run_id: i64) {
        let reconcile = RunFacts {
            boot_reconciliation: true,
            ..coordinator_facts()
        };
        self.machines
            .apply_run(run_id, RunTrigger::Reconcile, &reconcile)
            .unwrap();
        self.machines
            .apply_run(run_id, RunTrigger::Resume, &coordinator_facts())
            .unwrap();
    }

    /// Parks a running run on a question and answers it: the other way a run
    /// reaches the queue as resumed.
    ///
    /// The question is opened by the `ask` transition itself rather than
    /// inserted beside it, because that is the only way a question comes into
    /// being: a run cannot park on `awaiting_input` with nothing to answer.
    fn park_on_question_and_answer(&mut self, run_id: i64) {
        let parked = RunFacts {
            opened_question: Some(QuestionRow {
                id: None,
                run_id,
                kind: QuestionKind::Question,
                blocked_decision: "which branch".to_owned(),
                options: "[\"main\",\"next\"]".to_owned(),
                recommendation: None,
                answer: None,
                author: None,
                expires_at: None,
                tree_hash: None,
                paths_digest: None,
                state: QuestionState::Open,
                created_at: NOW,
            }),
            ..coordinator_facts()
        };
        let question_id = self
            .machines
            .apply_run(run_id, RunTrigger::Ask, &parked)
            .unwrap()
            .applied()
            .and_then(|transition| transition.opened_question_id)
            .expect("the ask opened the question it parked on");

        self.machines
            .apply_question(
                question_id,
                QuestionTrigger::Answer,
                &QuestionFacts {
                    now: NOW,
                    answer: Some("main".to_owned()),
                    author: Some(QuestionAuthor::User),
                    ..QuestionFacts::default()
                },
            )
            .unwrap();

        let answered = RunFacts {
            answered_question_id: Some(question_id),
            ..coordinator_facts()
        };
        self.machines
            .apply_run(run_id, RunTrigger::Answered, &answered)
            .unwrap();
    }
}

/// Opens a real session and physical attempt.
///
/// The control plane's `attempts` row points at both, so a correlation to ids
/// that do not exist is refused by the schema rather than recorded. A launcher
/// that reports a session has, by the time it reports it, started one.
fn open_session_in(sessions: &mut SessionStore) -> (i64, i64) {
    let metadata = SessionMetadata {
        id: 0,
        project: "/home/dev/agens".to_owned(),
        title: "a scheduled run".to_owned(),
        active_agent: "build".to_owned(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: NOW,
        updated_at: NOW,
        completed_turn_count: 0,
        resumable: false,
        parent_session_id: None,
        fork_message_count: None,
    };

    let summary = sessions
        .begin_session_attempt(&metadata, "start the run".to_owned())
        .unwrap();

    (summary.key().session_id(), summary.key().attempt_id())
}

/// A run as the store holds it, with the fields the scheduler reads spelled out
/// and the rest fixed.
struct Draft {
    state: RunState,
    provider: &'static str,
    priority: i64,
    created_at: i64,
    dep_run_id: Option<i64>,
    worktree_status: Option<WorktreeStatus>,
}

impl Draft {
    fn in_state(state: RunState, provider: &'static str) -> Self {
        Self {
            state,
            provider,
            priority: 5,
            created_at: 1_700_000_000,
            dep_run_id: None,
            worktree_status: Some(WorktreeStatus::Active),
        }
    }

    fn queued(provider: &'static str) -> Self {
        Self::in_state(RunState::Queued, provider)
    }

    fn running(provider: &'static str) -> Self {
        Self::in_state(RunState::Running, provider)
    }

    fn priority(mut self, priority: i64) -> Self {
        self.priority = priority;
        self
    }

    fn created_at(mut self, created_at: i64) -> Self {
        self.created_at = created_at;
        self
    }

    fn depending_on(mut self, dep_run_id: i64) -> Self {
        self.dep_run_id = Some(dep_run_id);
        self
    }

    fn worktree(mut self, worktree_status: Option<WorktreeStatus>) -> Self {
        self.worktree_status = worktree_status;
        self
    }

    fn row(&self) -> RunRow {
        RunRow {
            id: None,
            repo_id: "a1b2c3d4e5f60718".to_owned(),
            repo_root: "/home/dev/agens".to_owned(),
            remote_url: None,
            external_ref: None,
            parent_run_id: None,
            task: "a queued run".to_owned(),
            scope: "crates/agens-server/src/scheduler".to_owned(),
            dod: "slots, order and ceilings".to_owned(),
            genesis_paths: None,
            state: self.state,
            priority: self.priority,
            dep_run_id: self.dep_run_id,
            provider: self.provider.to_owned(),
            budget_tokens: None,
            worktree_path: Some("/data/worktrees/agens-a1b2c3d4/agn-56".to_owned()),
            worktree_status: self.worktree_status,
            created_at: self.created_at,
            result: None,
        }
    }
}

fn coordinator_facts() -> RunFacts {
    RunFacts {
        now: NOW,
        principal: Principal::Coordinator,
        ..RunFacts::default()
    }
}

fn limits(max_concurrent: usize, worktrees: usize, provider_capacity: usize) -> SchedulerLimits {
    SchedulerLimits {
        max_concurrent,
        available_worktrees: worktrees,
        provider_capacity: BTreeMap::new(),
        default_provider_capacity: provider_capacity,
    }
}

fn load() -> SchedulerLoad {
    SchedulerLoad {
        now: NOW,
        subagents: BTreeMap::new(),
    }
}

fn load_with(subagents: &[(&str, usize)]) -> SchedulerLoad {
    SchedulerLoad {
        now: NOW,
        subagents: subagents
            .iter()
            .map(|(provider, count)| ((*provider).to_owned(), *count))
            .collect(),
    }
}

/// What the scheduler asked to be launched, and what it was told back.
struct RecordingLauncher {
    /// Its own handle, because a launcher opens the session it reports.
    sessions: Mutex<SessionStore>,
    launched: Mutex<Vec<(i64, LaunchedSession)>>,
    abandoned: Mutex<Vec<SessionId>>,
    refuse: Option<&'static str>,
    /// A run to cancel from a second handle at launch time, reaching the path
    /// where the transition is refused after the session already started.
    cancel_during_launch: Option<i64>,
    directory: PathBuf,
}

impl RecordingLauncher {
    fn new(directory: &std::path::Path) -> Self {
        Self {
            sessions: Mutex::new(SessionStore::open(directory).unwrap()),
            launched: Mutex::new(Vec::new()),
            abandoned: Mutex::new(Vec::new()),
            refuse: None,
            cancel_during_launch: None,
            directory: directory.to_path_buf(),
        }
    }

    fn refusing(directory: &std::path::Path, reason: &'static str) -> Self {
        Self {
            refuse: Some(reason),
            ..Self::new(directory)
        }
    }

    fn cancelling_during_launch(directory: &std::path::Path, run_id: i64) -> Self {
        Self {
            cancel_during_launch: Some(run_id),
            ..Self::new(directory)
        }
    }

    fn launched(&self) -> Vec<i64> {
        self.launched
            .lock()
            .unwrap()
            .iter()
            .map(|(run_id, _)| *run_id)
            .collect()
    }

    fn session_for(&self, run_id: i64) -> LaunchedSession {
        self.launched
            .lock()
            .unwrap()
            .iter()
            .find_map(|(id, launched)| (*id == run_id).then_some(*launched))
            .unwrap_or_else(|| panic!("run {run_id} was never launched"))
    }

    fn abandoned(&self) -> Vec<SessionId> {
        self.abandoned.lock().unwrap().clone()
    }
}

impl RunLauncher for RecordingLauncher {
    fn launch(&self, pending: &PendingRun<'_>) -> Result<LaunchedSession, LaunchError> {
        if let Some(reason) = self.refuse {
            return Err(LaunchError(reason.to_owned()));
        }

        if let Some(run_id) = self.cancel_during_launch {
            let mut store = ControlPlaneStore::open(&self.directory).unwrap();
            store
                .apply_transition(&TransitionWrite {
                    run_id,
                    run_state: Some(StateChange {
                        expected: RunState::Queued,
                        next: RunState::Cancelled,
                    }),
                    worktree_status: None,
                    question: None,
                    new_question: None,
                    attempt: None,
                    close_attempt: None,
                    provider: None,
                    events: &[],
                })
                .unwrap();
        }

        let (session_id, session_attempt_id) = open_session_in(&mut self.sessions.lock().unwrap());
        let launched = LaunchedSession {
            session: SessionId::new(session_id),
            session_attempt_id: Some(session_attempt_id),
        };

        self.launched
            .lock()
            .unwrap()
            .push((pending.run_id, launched));

        Ok(launched)
    }

    fn abandon(&self, session: SessionId) {
        self.abandoned.lock().unwrap().push(session);
    }
}

fn admitted_runs(report: &QueueReport) -> Vec<i64> {
    report
        .admitted
        .iter()
        .map(|admission| admission.run_id)
        .collect()
}

fn deferral_for(report: &QueueReport, run_id: i64) -> &Deferral {
    report
        .deferred
        .iter()
        .find_map(|(id, deferral)| (*id == run_id).then_some(deferral))
        .unwrap_or_else(|| panic!("run {run_id} was not deferred"))
}

#[test]
fn admits_a_queued_run_and_moves_it_to_running() {
    let mut harness = Harness::new();
    let run = harness.insert(&Draft::queued("anthropic"));

    let (report, launcher) = harness.tick(limits(4, 4, 4));

    assert_eq!(admitted_runs(&report), vec![run]);
    assert_eq!(launcher.launched(), vec![run]);
    assert_eq!(report.depth, 1);
    assert_eq!(report.running_before, 0);
    assert!(!report.is_saturated());
    assert_eq!(harness.state_of(run), RunState::Running);

    let launched = launcher.session_for(run);
    let attempts = harness.machines.store().attempts_for_run(run).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].n, 1);
    assert_eq!(attempts[0].session_id, Some(launched.session.value()));
    assert_eq!(attempts[0].session_attempt_id, launched.session_attempt_id);
}

#[test]
fn a_capped_provider_makes_its_runs_ineligible() {
    let mut harness = Harness::new();
    let capped = harness.insert(&Draft::queued("anthropic"));
    let serving = harness.insert(&Draft::queued("openai"));
    harness.cap("anthropic", Some(NOW + 3_600));

    let (report, _) = harness.tick(limits(4, 4, 4));

    assert_eq!(admitted_runs(&report), vec![serving]);
    assert_eq!(
        deferral_for(&report, capped),
        &Deferral::Ineligible(Ineligible::ProviderCapped {
            provider: "anthropic".to_owned()
        })
    );
    // A capped provider is not backpressure: more slots would not release it.
    assert!(!report.is_saturated());
}

#[test]
fn a_provider_with_no_recorded_row_is_serving() {
    let mut harness = Harness::new();
    let run = harness.insert(&Draft::queued("a-provider-never-seen"));

    let (report, _) = harness.tick(limits(4, 4, 4));

    assert_eq!(admitted_runs(&report), vec![run]);
}

/// A run parked on a question keeps its place in the queue while its worktree
/// is reclaimed underneath it. Admitting it would start a session over a
/// directory that is being taken apart, so the queue holds it until a worktree
/// is provisioned again.
#[test]
fn a_run_whose_worktree_is_not_active_is_ineligible() {
    let mut harness = Harness::new();
    let reclaimed = harness.insert(
        &Draft::queued("anthropic")
            .created_at(10)
            .worktree(Some(WorktreeStatus::Reclaimable)),
    );
    let never_provisioned =
        harness.insert(&Draft::queued("anthropic").created_at(20).worktree(None));
    let ready = harness.insert(&Draft::queued("anthropic").created_at(30));

    let (report, launcher) = harness.tick(limits(4, 4, 4));

    assert_eq!(admitted_runs(&report), vec![ready]);
    assert_eq!(launcher.launched().len(), 1);
    assert_eq!(harness.state_of(reclaimed), RunState::Queued);
    assert_eq!(
        deferral_for(&report, reclaimed),
        &Deferral::Ineligible(Ineligible::WorktreeNotReady {
            worktree_status: Some(WorktreeStatus::Reclaimable),
        })
    );
    assert_eq!(
        deferral_for(&report, never_provisioned),
        &Deferral::Ineligible(Ineligible::WorktreeNotReady {
            worktree_status: None,
        })
    );
}

#[test]
fn a_dependency_holds_a_run_until_its_worktree_is_reclaimable() {
    let mut harness = Harness::new();
    let dependency = harness.insert(&Draft::running("anthropic"));
    let dependent = harness.insert(&Draft::queued("anthropic").depending_on(dependency));

    let (blocked, _) = harness.tick(limits(4, 4, 4));

    assert!(admitted_runs(&blocked).is_empty());
    assert_eq!(
        deferral_for(&blocked, dependent),
        &Deferral::Ineligible(Ineligible::DependencyPending {
            dep_run_id: dependency,
            worktree_status: Some(WorktreeStatus::Active),
        })
    );

    harness.reclaim(dependency);
    let (released, _) = harness.tick(limits(4, 4, 4));

    assert_eq!(admitted_runs(&released), vec![dependent]);
}

#[test]
fn resumed_runs_are_admitted_before_higher_priority_fresh_ones() {
    let mut harness = Harness::new();

    let urgent = harness.insert(&Draft::queued("anthropic").priority(100));
    let interrupted = harness.insert(&Draft::running("anthropic").priority(1).created_at(10));
    let parked = harness.insert(&Draft::running("anthropic").priority(1).created_at(20));

    harness.interrupt_and_requeue(interrupted);
    harness.park_on_question_and_answer(parked);

    let (report, _) = harness.tick(limits(2, 4, 4));

    assert_eq!(admitted_runs(&report), vec![interrupted, parked]);
    assert!(report.is_saturated());
    assert_eq!(
        deferral_for(&report, urgent),
        &Deferral::MaxConcurrent {
            running: 2,
            limit: 2
        }
    );
}

#[test]
fn among_fresh_runs_priority_wins_and_then_first_queued_wins() {
    let mut harness = Harness::new();

    let late_high = harness.insert(&Draft::queued("anthropic").priority(9).created_at(300));
    let early_high = harness.insert(&Draft::queued("anthropic").priority(9).created_at(100));
    let low = harness.insert(&Draft::queued("anthropic").priority(1).created_at(50));

    let (report, _) = harness.tick(limits(3, 3, 3));

    assert_eq!(admitted_runs(&report), vec![early_high, late_high, low]);
}

#[test]
fn max_concurrent_bounds_admission_and_counts_what_is_already_running() {
    let mut harness = Harness::new();
    harness.insert(&Draft::running("anthropic"));
    let waiting = harness.insert(&Draft::queued("anthropic"));

    let (report, launcher) = harness.tick(limits(1, 4, 4));

    assert!(admitted_runs(&report).is_empty());
    assert!(launcher.launched().is_empty());
    assert_eq!(report.running_before, 1);
    assert_eq!(
        deferral_for(&report, waiting),
        &Deferral::MaxConcurrent {
            running: 1,
            limit: 1
        }
    );
}

#[test]
fn the_worktree_ceiling_bounds_admission_on_its_own() {
    let mut harness = Harness::new();
    let first = harness.insert(&Draft::queued("anthropic").created_at(10));
    let second = harness.insert(&Draft::queued("anthropic").created_at(20));

    let (report, _) = harness.tick(limits(8, 1, 8));

    assert_eq!(admitted_runs(&report), vec![first]);
    assert_eq!(
        deferral_for(&report, second),
        &Deferral::WorktreeCeiling { held: 1, limit: 1 }
    );
}

/// The worktree ceiling counts worktrees, and a run parked on a question holds
/// one without occupying a slot. Counted as slots instead, the parked run costs
/// nothing and the ceiling can only refuse what `max_concurrent` already
/// refused.
#[test]
fn a_parked_run_holds_its_worktree_against_the_ceiling() {
    let mut harness = Harness::new();
    let parked = harness.insert(&Draft::in_state(RunState::AwaitingInput, "anthropic"));
    let first = harness.insert(&Draft::queued("anthropic").created_at(10));
    let second = harness.insert(&Draft::queued("anthropic").created_at(20));

    let (report, _) = harness.tick(limits(8, 2, 8));

    assert_eq!(harness.state_of(parked), RunState::AwaitingInput);
    assert_eq!(admitted_runs(&report), vec![first]);
    assert_eq!(
        deferral_for(&report, second),
        &Deferral::WorktreeCeiling { held: 2, limit: 2 }
    );
    assert!(report.is_saturated());
}

/// A worktree that was cleaned costs the machine nothing, so the run that used
/// to hold it is not charged for it.
#[test]
fn a_cleaned_worktree_is_not_held_against_the_ceiling() {
    let mut harness = Harness::new();
    harness.insert(
        &Draft::in_state(RunState::AwaitingInput, "anthropic")
            .worktree(Some(WorktreeStatus::Cleaned)),
    );
    let queued = harness.insert(&Draft::queued("anthropic"));

    let (report, _) = harness.tick(limits(8, 1, 8));

    assert_eq!(admitted_runs(&report), vec![queued]);
}

/// Cancellation moves the run and never touches its directory, so a cancelled
/// run is still holding one. Boot reconciliation reported that directory as an
/// orphan while the ceiling counted it; both now read the same list.
#[test]
fn a_cancelled_run_still_holds_its_worktree_against_the_ceiling() {
    let mut harness = Harness::new();
    harness.insert(&Draft::in_state(RunState::Cancelled, "anthropic"));
    let queued = harness.insert(&Draft::queued("anthropic"));

    let (report, _) = harness.tick(limits(8, 1, 8));

    assert!(admitted_runs(&report).is_empty());
    assert_eq!(
        deferral_for(&report, queued),
        &Deferral::WorktreeCeiling { held: 1, limit: 1 }
    );
}

/// A finished run holds its directory until the reclaim sweep disposes of it,
/// so `done` and `failed` are charged for theirs too.
#[test]
fn a_finished_run_holds_its_worktree_until_it_is_cleaned() {
    let mut harness = Harness::new();
    harness.insert(&Draft::in_state(RunState::Done, "anthropic"));
    harness.insert(
        &Draft::in_state(RunState::Failed, "anthropic").worktree(Some(WorktreeStatus::Reclaimable)),
    );
    let queued = harness.insert(&Draft::queued("anthropic"));

    let (report, _) = harness.tick(limits(8, 2, 8));

    assert!(admitted_runs(&report).is_empty());
    assert_eq!(
        deferral_for(&report, queued),
        &Deferral::WorktreeCeiling { held: 2, limit: 2 },
        "a released worktree is still a directory on disk"
    );
}

#[test]
fn provider_headroom_bounds_one_provider_without_holding_up_the_others() {
    let mut harness = Harness::new();

    let mut limits = limits(8, 8, 8);
    limits.provider_capacity.insert("anthropic".to_owned(), 1);

    let first_anthropic = harness.insert(&Draft::queued("anthropic").created_at(10));
    let second_anthropic = harness.insert(&Draft::queued("anthropic").created_at(20));
    let openai = harness.insert(&Draft::queued("openai").created_at(30));

    let launcher = RecordingLauncher::new(&harness.directory);
    let report = Scheduler::new(limits)
        .tick(&mut harness.machines, &launcher, &load())
        .unwrap();

    assert_eq!(admitted_runs(&report), vec![first_anthropic, openai]);
    assert_eq!(
        deferral_for(&report, second_anthropic),
        &Deferral::ProviderHeadroom {
            provider: "anthropic".to_owned(),
            running: 1,
            headroom: 1,
        }
    );
}

#[test]
fn sub_agents_lower_provider_headroom_and_never_max_concurrent() {
    let mut harness = Harness::new();

    let mut limits = limits(4, 4, 4);
    limits.provider_capacity.insert("anthropic".to_owned(), 2);

    let first = harness.insert(&Draft::queued("anthropic").created_at(10));
    let second = harness.insert(&Draft::queued("anthropic").created_at(20));

    let launcher = RecordingLauncher::new(&harness.directory);
    let report = Scheduler::new(limits)
        .tick(
            &mut harness.machines,
            &launcher,
            &load_with(&[("anthropic", 1)]),
        )
        .unwrap();

    // `max_concurrent` had room for four. The sub-agent took one of the
    // provider's two, so only one run went through, and the sub-agent itself
    // never asked the scheduler for anything.
    assert_eq!(admitted_runs(&report), vec![first]);
    assert_eq!(
        deferral_for(&report, second),
        &Deferral::ProviderHeadroom {
            provider: "anthropic".to_owned(),
            running: 1,
            headroom: 1,
        }
    );
}

#[test]
fn more_sub_agents_than_capacity_closes_the_provider_rather_than_going_negative() {
    let mut harness = Harness::new();

    let mut limits = limits(4, 4, 4);
    limits.provider_capacity.insert("anthropic".to_owned(), 2);

    let run = harness.insert(&Draft::queued("anthropic"));

    let launcher = RecordingLauncher::new(&harness.directory);
    let report = Scheduler::new(limits)
        .tick(
            &mut harness.machines,
            &launcher,
            &load_with(&[("anthropic", 5)]),
        )
        .unwrap();

    assert!(admitted_runs(&report).is_empty());
    assert_eq!(
        deferral_for(&report, run),
        &Deferral::ProviderHeadroom {
            provider: "anthropic".to_owned(),
            running: 0,
            headroom: 0,
        }
    );
}

/// A parked run holds a worktree and no slot, so the machine is given worktree
/// headroom for the two parked runs and a single slot to compete for.
#[test]
fn a_parked_run_holds_no_slot() {
    let mut harness = Harness::new();

    harness.insert(&Draft::in_state(RunState::AwaitingInput, "anthropic"));
    harness.insert(&Draft::in_state(RunState::AwaitingQuota, "anthropic"));
    let waiting = harness.insert(&Draft::queued("anthropic"));

    let (report, _) = harness.tick(limits(1, 3, 1));

    assert_eq!(report.running_before, 0);
    assert_eq!(admitted_runs(&report), vec![waiting]);
}

#[test]
fn every_run_that_did_not_start_is_reported_with_a_reason() {
    let mut harness = Harness::new();

    let capped = harness.insert(&Draft::queued("openai"));
    harness.cap("openai", None);
    let admitted = harness.insert(&Draft::queued("anthropic").created_at(10));
    let over_ceiling = harness.insert(&Draft::queued("anthropic").created_at(20));

    let (report, _) = harness.tick(limits(1, 4, 4));

    assert_eq!(admitted_runs(&report), vec![admitted]);
    assert_eq!(report.depth, 3);
    assert_eq!(report.deferred_count(), 2);
    assert!(report.is_saturated());
    assert!(matches!(
        deferral_for(&report, capped),
        Deferral::Ineligible(Ineligible::ProviderCapped { .. })
    ));
    assert!(matches!(
        deferral_for(&report, over_ceiling),
        Deferral::MaxConcurrent { .. }
    ));
}

#[test]
fn a_launch_that_fails_leaves_the_run_queued_and_charges_it_nothing() {
    let mut harness = Harness::new();
    let run = harness.insert(&Draft::queued("anthropic"));

    let launcher = RecordingLauncher::refusing(&harness.directory, "no worktree could be made");
    let report = Scheduler::new(limits(4, 4, 4))
        .tick(&mut harness.machines, &launcher, &load())
        .unwrap();

    assert!(report.admitted.is_empty());
    assert_eq!(
        report.failures,
        vec![(
            run,
            AdmissionFailure::Launch(LaunchError("no worktree could be made".to_owned()))
        )]
    );
    assert_eq!(harness.state_of(run), RunState::Queued);
    assert!(
        harness
            .machines
            .store()
            .attempts_for_run(run)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_transition_refused_after_the_launch_abandons_the_session() {
    let mut harness = Harness::new();
    let run = harness.insert(&Draft::queued("anthropic"));

    let launcher = RecordingLauncher::cancelling_during_launch(&harness.directory, run);
    let report = Scheduler::new(limits(4, 4, 4))
        .tick(&mut harness.machines, &launcher, &load())
        .unwrap();

    assert!(report.admitted.is_empty());
    assert_eq!(
        launcher.abandoned(),
        vec![launcher.session_for(run).session]
    );
    assert_eq!(report.failures.len(), 1);
    assert!(matches!(
        report.failures[0],
        (id, AdmissionFailure::Refused(_)) if id == run
    ));
    assert_eq!(harness.state_of(run), RunState::Cancelled);
}

struct StubProvider;

impl SessionProvider for StubProvider {
    fn model(&self) -> &str {
        "claude-opus-5"
    }
}

#[test]
fn the_supervisor_launcher_starts_an_admitted_run_as_a_session() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .unwrap();
    let supervisor = SessionSupervisor::new(runtime.handle().clone());

    let mut harness = Harness::new();
    let run = harness.insert(&Draft::queued("anthropic"));
    let (session_id, _) = harness.open_session();

    let (sender, ran) = mpsc::channel();
    let sender = Arc::new(Mutex::new(sender));

    let launcher = SupervisorLauncher::new(supervisor, move |pending: &PendingRun<'_>| {
        let sender = Arc::clone(&sender);
        let run_id = pending.run_id;

        Ok(RunSession {
            admission: SessionAdmission::new(
                SessionId::new(session_id),
                Box::new(StubProvider),
                SessionBudget::unlimited(),
            ),
            work: Box::new(move |_: SessionRuntime| {
                sender.lock().unwrap().send(run_id).unwrap();
                SessionOutcome::Completed
            }),
            session_attempt_id: None,
        })
    });

    let report = Scheduler::new(limits(4, 4, 4))
        .tick(&mut harness.machines, &launcher, &load())
        .unwrap();

    assert_eq!(
        report.admitted,
        vec![Admission {
            run_id: run,
            session: SessionId::new(session_id),
            resumed: false,
        }]
    );
    assert_eq!(ran.recv_timeout(Duration::from_secs(5)).unwrap(), run);
    assert_eq!(harness.state_of(run), RunState::Running);

    runtime.shutdown_timeout(Duration::from_secs(1));
}
