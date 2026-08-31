//! Praetor's facade, driven against a real control plane.
//!
//! What is asserted here is what the facade decides rather than what the core
//! does: which principal every request arrives as, and which repository it is
//! allowed to be about. Both are held by the facade and named by nobody, so a
//! manager cannot claim the user's authority or reach another project's runs.

use std::{
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use agens_core::coordination::{
    AnswerRequest, CancelRequest, CoordinationError, CoordinationPort, DirectRequest,
    EscalateRequest, MergeRequest, ReclaimRequest, ReportRequest,
};
use agens_core::run_introspection::{Ask, AskOption};
use agens_server::{
    AdmissionControl, ApiCore, Delivery, DeliveryPayload, DeliveryQueue, EventFeed, EventFilter,
    HookTrust, PendingHookTrust, PortError, Ports, ProvisionedWorktree, RepositoryIdentity,
    RepositoryPolicy, SessionControl, StateMachines, StopScope, Subscription, TakeoverHandle,
    TeamBinding, TeamCoordination, WorktreeDerivation, WorktreeGate, WorktreeRequest,
};
use agens_store::{
    ControlPlaneStore, QuestionKind, QuestionRow, QuestionState, RunRow, RunState, WorktreeStatus,
};

const NOW: i64 = 1_700_000_500;
const REPO: &str = "a1b2c3d4e5f60718";
const OTHER_REPO: &str = "ffffffffffffffff";

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-server-coordination-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();

    directory
}

/// Every port the facade's operations reach, with the two answers a test
/// varies: what git says about a worktree, and what was queued for delivery.
struct Doubles {
    derivation: WorktreeDerivation,
    queued: Mutex<Vec<Delivery>>,
    removed: Mutex<Vec<i64>>,
}

impl Doubles {
    fn new(branch_merged: bool, worktree_clean: bool) -> Self {
        Self {
            derivation: WorktreeDerivation {
                branch_merged,
                worktree_clean,
                tree_hash: "c0ffee".repeat(6),
                paths_digest: "d1ge57".repeat(6),
            },
            queued: Mutex::new(Vec::new()),
            removed: Mutex::new(Vec::new()),
        }
    }
}

impl AdmissionControl for Doubles {
    fn admissions_paused(&self) -> bool {
        false
    }

    fn set_admissions_paused(&self, _paused: bool) -> Result<bool, PortError> {
        Err(PortError::new("scheduler", "this test pauses nothing"))
    }

    fn queue_changed(&self, _run_id: i64) {}
}

impl WorktreeGate for Doubles {
    fn derive(&self, _run: &RunRow) -> Result<WorktreeDerivation, PortError> {
        Ok(self.derivation.clone())
    }

    fn remove(&self, run: &RunRow) -> Result<(), PortError> {
        self.removed.lock().unwrap().push(run.id.unwrap());
        Ok(())
    }

    fn identify(&self, _repository: &std::path::Path) -> Result<RepositoryIdentity, PortError> {
        Ok(RepositoryIdentity {
            repo_id: REPO.to_owned(),
            remote_url: None,
        })
    }

    fn provision(&self, _request: &WorktreeRequest<'_>) -> Result<ProvisionedWorktree, PortError> {
        Err(PortError::new("worktrees", "this test provisions nothing"))
    }
}

impl DeliveryQueue for Doubles {
    fn enqueue(&self, delivery: &Delivery) -> Result<(), PortError> {
        self.queued.lock().unwrap().push(delivery.clone());
        Ok(())
    }
}

impl SessionControl for Doubles {
    fn cancel(&self, _run_id: i64) -> Result<(), PortError> {
        Ok(())
    }

    fn suspend(&self, _run_id: i64) -> Result<(), PortError> {
        Ok(())
    }

    fn take_over(&self, _run_id: i64) -> Result<TakeoverHandle, PortError> {
        Err(PortError::new("sessions", "this test takes nothing over"))
    }

    fn stop(&self, _scope: &StopScope) -> Result<(), PortError> {
        Ok(())
    }
}

impl EventFeed for Doubles {
    fn subscribe(&self, _filter: &EventFilter) -> Result<Subscription, PortError> {
        let (_sender, receiver) = std::sync::mpsc::channel();
        Ok(receiver)
    }
}

impl RepositoryPolicy for Doubles {
    fn hook_trust(&self, _repo_id: &str) -> HookTrust {
        HookTrust::Granted
    }

    fn hook_exports(&self) -> Vec<String> {
        Vec::new()
    }

    fn record_pending(&self, _pending: &PendingHookTrust) -> Result<(), PortError> {
        Ok(())
    }

    fn is_pending(&self, _question_id: i64) -> bool {
        false
    }

    fn resolve_pending(&self, _question_id: i64, _granted: bool) -> Result<bool, PortError> {
        Ok(false)
    }
}

fn run_in(repo_id: &str, state: RunState, worktree_status: Option<WorktreeStatus>) -> RunRow {
    RunRow {
        id: None,
        repo_id: repo_id.to_owned(),
        repo_root: "/home/dev/agens".to_owned(),
        remote_url: None,
        external_ref: None,
        parent_run_id: None,
        task: "port the importer".to_owned(),
        scope: "crates/agens-server".to_owned(),
        dod: "the suite is green".to_owned(),
        genesis_paths: None,
        state,
        priority: 5,
        dep_run_id: None,
        provider: "anthropic".to_owned(),
        budget_tokens: None,
        worktree_path: Some("/data/worktrees/agens/agn-66".to_owned()),
        worktree_status,
        created_at: 1_700_000_000,
        result: None,
    }
}

fn question_in(run_id: i64, kind: QuestionKind) -> QuestionRow {
    QuestionRow {
        id: None,
        run_id,
        kind,
        blocked_decision: "which serializer".to_owned(),
        options: r#"[{"id":"serde_json","label":"use serde_json"}]"#.to_owned(),
        recommendation: None,
        answer: None,
        author: None,
        expires_at: None,
        tree_hash: (kind == QuestionKind::Approval).then(|| "c0ffee".repeat(6)),
        paths_digest: (kind == QuestionKind::Approval).then(|| "d1ge57".repeat(6)),
        state: QuestionState::Open,
        created_at: 1_700_000_100,
    }
}

/// One facade over one control plane, with the doubles kept reachable.
struct Facade {
    coordination: TeamCoordination,
    doubles: Arc<Doubles>,
    core: Arc<Mutex<ApiCore>>,
}

impl Facade {
    fn over(store: ControlPlaneStore, doubles: Doubles) -> Self {
        let doubles = Arc::new(doubles);
        let ports = Ports {
            scheduler: doubles.clone(),
            worktrees: doubles.clone(),
            delivery: doubles.clone(),
            sessions: doubles.clone(),
            feed: doubles.clone(),
        };
        let core = Arc::new(Mutex::new(ApiCore::new(
            StateMachines::new(store),
            ports,
            doubles.clone(),
        )));

        Self {
            coordination: TeamCoordination::new(
                Arc::clone(&core),
                TeamBinding {
                    repository: std::path::PathBuf::from("/home/dev/agens"),
                    repo_id: REPO.to_owned(),
                    provider: "anthropic".to_owned(),
                    start_point: "HEAD".to_owned(),
                    retry_budget: 3,
                },
                Arc::new(|| NOW),
            ),
            doubles,
            core,
        }
    }

    fn question_state(&self, question_id: i64) -> QuestionState {
        self.core
            .lock()
            .unwrap()
            .machines()
            .store()
            .load_question(question_id)
            .unwrap()
            .unwrap()
            .state
    }
}

fn store() -> ControlPlaneStore {
    ControlPlaneStore::open(data_directory()).unwrap()
}

/// One daemon serves N projects. A manager bound to one of them must not see
/// another's runs, and the repository it is scoped to is never in a request.
#[test]
fn the_facade_only_ever_reports_the_repository_it_is_bound_to() {
    let mut store = store();
    let mine = store
        .insert_run(&run_in(
            REPO,
            RunState::Running,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    store
        .insert_run(&run_in(
            OTHER_REPO,
            RunState::Running,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let mut facade = Facade::over(store, Doubles::new(false, true));

    let status = facade.coordination.status().unwrap();

    assert_eq!(status.repo_id, REPO);
    assert_eq!(status.runs.len(), 1);
    assert_eq!(status.runs[0].run_id, mine);
    assert_eq!(status.runs[0].state, "running");
}

/// The principal is pinned to Praetor and read from nothing, so the one thing
/// the table keeps for the user is out of the facade's reach even though the
/// facade is the party asking.
#[test]
fn the_facade_cannot_answer_an_authorization() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(
            REPO,
            RunState::Running,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let question_id = store
        .insert_question(&question_in(run_id, QuestionKind::Approval))
        .unwrap();
    let mut facade = Facade::over(store, Doubles::new(true, true));

    let error = facade
        .coordination
        .answer(&AnswerRequest::new(question_id, "merge".to_owned()).unwrap())
        .unwrap_err();

    assert!(
        matches!(error, CoordinationError::Unauthorized(_)),
        "{error:?}"
    );
    assert_eq!(facade.question_state(question_id), QuestionState::Open);
}

#[test]
fn the_facade_answers_a_detail_question_a_worker_asked() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(
            REPO,
            RunState::AwaitingInput,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let question_id = store
        .insert_question(&question_in(run_id, QuestionKind::Question))
        .unwrap();
    let mut facade = Facade::over(store, Doubles::new(false, true));

    let receipt = facade
        .coordination
        .answer(&AnswerRequest::new(question_id, "serde_json".to_owned()).unwrap())
        .unwrap();

    assert_eq!(receipt.run_id, run_id);
    assert!(receipt.run_resumed, "the run was parked on this question");
    assert_eq!(facade.question_state(question_id), QuestionState::Answered);
}

#[test]
fn a_directive_reaches_the_delivery_queue() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(
            REPO,
            RunState::Running,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let mut facade = Facade::over(store, Doubles::new(false, true));

    facade
        .coordination
        .direct(&DirectRequest::new(run_id, "narrow the scope".to_owned()).unwrap())
        .unwrap();

    let queued = facade.doubles.queued.lock().unwrap();

    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0].payload,
        DeliveryPayload::Directive("narrow the scope".to_owned())
    );
}

#[test]
fn an_escalation_lands_in_the_inbox_the_person_reads() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(
            REPO,
            RunState::Running,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let mut facade = Facade::over(store, Doubles::new(false, true));

    let question = Ask::new(
        "which database the importer writes to".to_owned(),
        vec![
            AskOption::new("postgres".to_owned(), "write to postgres".to_owned(), None),
            AskOption::new("sqlite".to_owned(), "write to sqlite".to_owned(), None),
        ],
        Some("postgres".to_owned()),
    )
    .unwrap();

    let receipt = facade
        .coordination
        .escalate(&EscalateRequest::new(run_id, question).unwrap())
        .unwrap();

    let status = facade.coordination.status().unwrap();
    let open = status
        .open_questions
        .iter()
        .find(|item| item.question_id == receipt.question_id)
        .expect("the escalation is open");

    assert_eq!(open.kind, "question");
    assert_eq!(open.options.len(), 2);
    assert_eq!(open.options[0].id(), "postgres");
    assert_eq!(open.recommendation.as_deref(), Some("postgres"));
}

/// A merge request opens the decision and never closes it: the approval stays
/// open, frozen over the bytes git reported.
#[test]
fn a_merge_request_opens_an_approval_the_facade_cannot_grant() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(REPO, RunState::Done, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut facade = Facade::over(store, Doubles::new(true, true));

    let receipt = facade
        .coordination
        .request_merge(&MergeRequest::new(run_id, "the dod is met".to_owned()).unwrap())
        .unwrap();

    assert_eq!(receipt.tree_hash, "c0ffee".repeat(6));
    assert_eq!(receipt.paths_digest, "d1ge57".repeat(6));
    assert_eq!(
        facade.question_state(receipt.question_id),
        QuestionState::Open
    );

    // The only way the facade could close it is by answering it, and an
    // approval is never a detail question.
    let error = facade
        .coordination
        .answer(&AnswerRequest::new(receipt.question_id, "merge".to_owned()).unwrap())
        .unwrap_err();

    assert!(
        matches!(error, CoordinationError::Unauthorized(_)),
        "{error:?}"
    );
}

/// A reclaim is a request, and the machine decides whether it may be carried
/// out. A worktree that was never re-derived as merged is still `active`, and
/// nothing releases one from there.
#[test]
fn a_reclaim_of_a_worktree_nobody_showed_merged_is_refused() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(REPO, RunState::Done, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut facade = Facade::over(store, Doubles::new(false, true));

    let error = facade
        .coordination
        .request_reclaim(&ReclaimRequest::new(run_id).unwrap())
        .unwrap_err();

    assert!(matches!(error, CoordinationError::Refused(_)), "{error:?}");
    assert!(facade.doubles.removed.lock().unwrap().is_empty());
}

/// The facade never confirms a disposal on anybody's behalf, so a worktree
/// still holding uncommitted work is refused rather than thrown away.
#[test]
fn a_reclaim_of_a_worktree_holding_uncommitted_work_is_refused() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(
            REPO,
            RunState::Done,
            Some(WorktreeStatus::Reclaimable),
        ))
        .unwrap();
    let mut facade = Facade::over(store, Doubles::new(true, false));

    let error = facade
        .coordination
        .request_reclaim(&ReclaimRequest::new(run_id).unwrap())
        .unwrap_err();

    assert!(matches!(error, CoordinationError::Refused(_)), "{error:?}");
    assert!(facade.doubles.removed.lock().unwrap().is_empty());
}

#[test]
fn a_reclaim_of_a_released_worktree_cleans_it() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(
            REPO,
            RunState::Done,
            Some(WorktreeStatus::Reclaimable),
        ))
        .unwrap();
    let mut facade = Facade::over(store, Doubles::new(true, true));

    let receipt = facade
        .coordination
        .request_reclaim(&ReclaimRequest::new(run_id).unwrap())
        .unwrap();

    assert!(receipt.moved);
    assert_eq!(receipt.worktree_status, "cleaned");
    assert_eq!(*facade.doubles.removed.lock().unwrap(), vec![run_id]);
}

/// Cancelling twice moves nothing the second time, and the receipt says so
/// rather than reporting a move that did not happen.
#[test]
fn cancelling_a_cancelled_run_reports_the_state_without_moving_it() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(
            REPO,
            RunState::Running,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let mut facade = Facade::over(store, Doubles::new(false, true));

    let first = facade
        .coordination
        .cancel(&CancelRequest::new(run_id, "withdrawn".to_owned()).unwrap())
        .unwrap();

    assert!(first.moved);
    assert_eq!(first.state, "cancelled");

    let second = facade
        .coordination
        .cancel(&CancelRequest::new(run_id, "withdrawn".to_owned()).unwrap())
        .unwrap();

    assert!(!second.moved);
    assert_eq!(second.state, "cancelled");
}

/// The report is everything the control plane recorded and nothing about where
/// the work lives: no worktree path crosses this seam.
#[test]
fn a_report_carries_the_run_without_its_worktree() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(
            REPO,
            RunState::Running,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let mut facade = Facade::over(store, Doubles::new(false, true));

    let report = facade
        .coordination
        .report(&ReportRequest::new(run_id).unwrap())
        .unwrap();

    assert_eq!(report.run.run_id, run_id);
    assert_eq!(report.scope, "crates/agens-server");
    assert_eq!(report.dod, "the suite is green");
    assert_eq!(report.provider, "anthropic");
}

#[test]
fn a_run_that_does_not_exist_is_reported_as_missing() {
    let mut facade = Facade::over(store(), Doubles::new(false, true));

    let error = facade
        .coordination
        .report(&ReportRequest::new(404).unwrap())
        .unwrap_err();

    assert!(matches!(error, CoordinationError::NotFound(_)), "{error:?}");
}
