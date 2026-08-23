//! The coordinator's service core, driven through both planes.
//!
//! The authorization tests are the point of the file. They exercise the core
//! directly, with no facade in sight, because that is the property the design
//! is buying: a transport picks a principal and gets exactly what the table
//! below gives it.

use std::{
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use agens_server::{
    AdmissionControl, AnswerQuestion, ApiCore, ApiError, ApprovePlan, AuthorizeMerge,
    CleaningAction, CleaningDisposition, CreateRun, Delivery, DeliveryGrain, DeliveryPayload,
    DeliveryQueue, DetailQuestionRefusal, EventFeed, EventFilter, HookPolicy, HookTrust,
    MergeAuthorization, OPERATION_AUTHORIZATION, Operation, PendingHookTrust, PortError, Ports,
    Principal, ProvisionedWorktree, RepositoryIdentity, RepositoryPolicy, RetryRequest, RunFacts,
    RunRef, RunTrigger, SessionControl, StateMachines, StopRequest, StopScope, Subscription,
    TakeoverHandle, TransitionRejection, WorktreeDerivation, WorktreeGate, WorktreeRequest,
    praetor_may_answer,
};
use agens_store::{
    ControlPlaneStore, QuestionAuthor, QuestionKind, QuestionRow, QuestionState, RetryTrigger,
    RunRow, RunState, WorktreeStatus,
};

const NOW: i64 = 1_700_000_500;
const REPO: &str = "a1b2c3d4e5f60718";

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory =
        std::env::temp_dir().join(format!("agens-server-api-{}-{suffix}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    directory
}

// The doubles. Each records what the core asked it to do, so a test can assert
// that an applied transition reached its port exactly once.

#[derive(Default)]
struct RecordingScheduler {
    paused: AtomicBool,
    queue_changed: Mutex<Vec<i64>>,
}

impl AdmissionControl for RecordingScheduler {
    fn admissions_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    fn set_admissions_paused(&self, paused: bool) -> Result<bool, PortError> {
        Ok(self.paused.swap(paused, Ordering::Relaxed))
    }

    fn queue_changed(&self, run_id: i64) {
        self.queue_changed.lock().unwrap().push(run_id);
    }
}

struct RecordingWorktrees {
    derivation: WorktreeDerivation,
    removed: Mutex<Vec<i64>>,
    /// Every worktree the core asked for, by the name it asked under.
    provisioned: Mutex<Vec<String>>,
    /// The hook policy the core decided, as each provisioning received it.
    hook_policies: Mutex<Vec<HookPolicy>>,
    /// The hooks the repository declares, reported back the way a real
    /// contract with hooks would.
    declared_hooks: Vec<String>,
}

impl RecordingWorktrees {
    fn new(branch_merged: bool, worktree_clean: bool) -> Self {
        Self {
            derivation: WorktreeDerivation {
                branch_merged,
                worktree_clean,
                tree_hash: "c0ffee".repeat(6),
                paths_digest: "d1ge57".repeat(6),
            },
            removed: Mutex::new(Vec::new()),
            provisioned: Mutex::new(Vec::new()),
            hook_policies: Mutex::new(Vec::new()),
            declared_hooks: Vec::new(),
        }
    }

    fn declaring_hooks(mut self, hooks: &[&str]) -> Self {
        self.declared_hooks = hooks.iter().map(|hook| (*hook).to_owned()).collect();
        self
    }
}

impl WorktreeGate for RecordingWorktrees {
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
            remote_url: Some("git@github.com:agens/agens.git".to_owned()),
        })
    }

    fn provision(&self, request: &WorktreeRequest<'_>) -> Result<ProvisionedWorktree, PortError> {
        self.provisioned
            .lock()
            .unwrap()
            .push(request.name.to_owned());
        self.hook_policies.lock().unwrap().push(request.hooks);

        Ok(ProvisionedWorktree {
            path: std::path::PathBuf::from("/worktrees")
                .join(request.repo_id)
                .join(request.name),
            hook_failures: Vec::new(),
            declared_hooks: self.declared_hooks.clone(),
            hooks_ran: request.hooks == HookPolicy::Allow && !self.declared_hooks.is_empty(),
        })
    }
}

/// The operator's decisions, as a test sets them.
struct RecordingPolicy {
    roots: Mutex<Vec<std::path::PathBuf>>,
    trust: Mutex<HookTrust>,
    pending: Mutex<Vec<PendingHookTrust>>,
    /// Every decision an answer applied, as `(repo_id, granted)`.
    decided: Mutex<Vec<(String, bool)>>,
}

impl RecordingPolicy {
    fn serving(root: &std::path::Path) -> Self {
        Self {
            roots: Mutex::new(vec![root.to_path_buf()]),
            trust: Mutex::new(HookTrust::Unknown),
            pending: Mutex::new(Vec::new()),
            decided: Mutex::new(Vec::new()),
        }
    }

    fn trusting(self, trust: HookTrust) -> Self {
        *self.trust.lock().unwrap() = trust;
        self
    }
}

impl RepositoryPolicy for RecordingPolicy {
    fn admits(&self, repository: &std::path::Path) -> bool {
        self.roots
            .lock()
            .unwrap()
            .iter()
            .any(|root| repository.starts_with(root))
    }

    fn admission_remedy(&self) -> String {
        "name the checkout in the daemon's policy".to_owned()
    }

    fn hook_trust(&self, _repo_id: &str) -> HookTrust {
        *self.trust.lock().unwrap()
    }

    fn hook_exports(&self) -> Vec<String> {
        Vec::new()
    }

    fn record_pending(&self, pending: &PendingHookTrust) -> Result<(), PortError> {
        self.pending.lock().unwrap().push(pending.clone());
        Ok(())
    }

    fn is_pending(&self, question_id: i64) -> bool {
        self.pending
            .lock()
            .unwrap()
            .iter()
            .any(|pending| pending.question_id == question_id)
    }

    fn resolve_pending(&self, question_id: i64, granted: bool) -> Result<bool, PortError> {
        let mut pending = self.pending.lock().unwrap();
        let Some(position) = pending
            .iter()
            .position(|entry| entry.question_id == question_id)
        else {
            return Ok(false);
        };

        let entry = pending.remove(position);
        self.decided.lock().unwrap().push((entry.repo_id, granted));

        Ok(true)
    }
}

#[derive(Default)]
struct RecordingDelivery {
    queued: Mutex<Vec<Delivery>>,
}

impl DeliveryQueue for RecordingDelivery {
    fn enqueue(&self, delivery: &Delivery) -> Result<(), PortError> {
        self.queued.lock().unwrap().push(delivery.clone());
        Ok(())
    }
}

#[derive(Default)]
struct RecordingSessions {
    cancelled: Mutex<Vec<i64>>,
    taken_over: Mutex<Vec<i64>>,
    stopped: Mutex<Vec<StopScope>>,
}

impl SessionControl for RecordingSessions {
    fn cancel(&self, run_id: i64) -> Result<(), PortError> {
        self.cancelled.lock().unwrap().push(run_id);
        Ok(())
    }

    fn take_over(&self, run_id: i64) -> Result<TakeoverHandle, PortError> {
        self.taken_over.lock().unwrap().push(run_id);

        Ok(TakeoverHandle {
            run_id,
            session_id: 42,
        })
    }

    fn stop(&self, scope: &StopScope) -> Result<(), PortError> {
        self.stopped.lock().unwrap().push(scope.clone());
        Ok(())
    }
}

#[derive(Default)]
struct RecordingFeed {
    filters: Mutex<Vec<EventFilter>>,
}

impl EventFeed for RecordingFeed {
    fn subscribe(&self, filter: &EventFilter) -> Result<Subscription, PortError> {
        self.filters.lock().unwrap().push(filter.clone());

        let (_sender, receiver) = std::sync::mpsc::channel();
        Ok(receiver)
    }
}

/// The doubles behind one core, kept reachable so a test can read them back.
struct Harness {
    core: ApiCore,
    scheduler: Arc<RecordingScheduler>,
    worktrees: Arc<RecordingWorktrees>,
    delivery: Arc<RecordingDelivery>,
    sessions: Arc<RecordingSessions>,
    feed: Arc<RecordingFeed>,
    policy: Arc<RecordingPolicy>,
    /// A checkout that exists on disk, because the core canonicalizes what it
    /// was handed before deciding whether the daemon serves it.
    repository: std::path::PathBuf,
}

impl Harness {
    fn build(store: ControlPlaneStore, worktrees: RecordingWorktrees) -> Self {
        let repository = checkout();
        let policy = Arc::new(RecordingPolicy::serving(&repository));

        Self::with_policy(store, worktrees, policy, repository)
    }

    fn with_policy(
        store: ControlPlaneStore,
        worktrees: RecordingWorktrees,
        policy: Arc<RecordingPolicy>,
        repository: std::path::PathBuf,
    ) -> Self {
        let scheduler = Arc::new(RecordingScheduler::default());
        let worktrees = Arc::new(worktrees);
        let delivery = Arc::new(RecordingDelivery::default());
        let sessions = Arc::new(RecordingSessions::default());
        let feed = Arc::new(RecordingFeed::default());

        let ports = Ports {
            scheduler: scheduler.clone(),
            worktrees: worktrees.clone(),
            delivery: delivery.clone(),
            sessions: sessions.clone(),
            feed: feed.clone(),
        };

        Self {
            core: ApiCore::new(StateMachines::new(store), ports, policy.clone()),
            scheduler,
            worktrees,
            delivery,
            sessions,
            feed,
            policy,
            repository,
        }
    }

    /// The creation request this harness's own checkout is named in.
    fn creation(&self) -> CreateRun {
        create_run(&self.repository)
    }

    fn run_state(&self, run_id: i64) -> RunState {
        self.core
            .machines()
            .store()
            .load_run(run_id)
            .unwrap()
            .unwrap()
            .state
    }

    fn event_types(&self, run_id: i64) -> Vec<String> {
        self.core
            .machines()
            .store()
            .events_for_run(run_id)
            .unwrap()
            .into_iter()
            .map(|event| event.event_type)
            .collect()
    }
}

fn store() -> ControlPlaneStore {
    ControlPlaneStore::open(data_directory()).unwrap()
}

fn run_in(state: RunState, worktree_status: Option<WorktreeStatus>) -> RunRow {
    RunRow {
        id: None,
        repo_id: REPO.to_owned(),
        repo_root: "/home/dev/agens".to_owned(),
        remote_url: Some("git@example.com:dev/agens.git".to_owned()),
        external_ref: Some("agens/AGN-63".to_owned()),
        parent_run_id: None,
        task: "the api core".to_owned(),
        scope: "crates/agens-server/src/api".to_owned(),
        dod: "one core, one authorization table".to_owned(),
        genesis_paths: None,
        state,
        priority: 5,
        dep_run_id: None,
        provider: "anthropic".to_owned(),
        budget_tokens: Some(200_000),
        worktree_path: Some("/data/worktrees/agens-a1b2c3d4/agn-63".to_owned()),
        worktree_status,
        created_at: 1_700_000_000,
        result: None,
    }
}

fn question_in(run_id: i64, kind: QuestionKind, options: &str) -> QuestionRow {
    QuestionRow {
        id: None,
        run_id,
        kind,
        blocked_decision: "which serializer".to_owned(),
        options: options.to_owned(),
        recommendation: Some("serde_json".to_owned()),
        answer: None,
        author: None,
        expires_at: None,
        tree_hash: (kind == QuestionKind::Approval).then(|| "c0ffee".repeat(6)),
        paths_digest: (kind == QuestionKind::Approval).then(|| "d1ge57".repeat(6)),
        state: QuestionState::Open,
        created_at: 1_700_000_100,
    }
}

/// A run parked on one open question of the given shape.
fn parked_on(kind: QuestionKind, options: &str) -> (Harness, i64, i64) {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(
            RunState::AwaitingInput,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let question_id = store
        .insert_question(&question_in(run_id, kind, options))
        .unwrap();

    (
        Harness::build(store, RecordingWorktrees::new(false, true)),
        run_id,
        question_id,
    )
}

fn unauthorized(error: &ApiError) -> (Operation, Principal, bool) {
    match error {
        ApiError::Unauthorized {
            operation,
            principal,
            journaled,
            ..
        } => (*operation, *principal, *journaled),
        other => panic!("expected a refusal, got {other}"),
    }
}

const EVERY_OPERATION: &[Operation] = &[
    Operation::CreateRun,
    Operation::ApprovePlan,
    Operation::AnswerQuestion,
    Operation::AuthorizeMerge,
    Operation::CancelRun,
    Operation::Retry,
    Operation::Cleaning,
    Operation::Takeover,
    Operation::PauseAdmissions,
    Operation::Stop,
    Operation::Tree,
    Operation::RunDetail,
    Operation::Inbox,
    Operation::Subscribe,
];

#[test]
fn every_operation_has_exactly_one_authorization_row() {
    for operation in EVERY_OPERATION {
        let rows = OPERATION_AUTHORIZATION
            .iter()
            .filter(|entry| entry.operation == *operation)
            .count();

        assert_eq!(rows, 1, "{} has {rows} rows", operation.as_str());
        assert!(
            !operation.principals().is_empty(),
            "{} reaches nobody",
            operation.as_str()
        );
    }

    assert_eq!(OPERATION_AUTHORIZATION.len(), EVERY_OPERATION.len());
}

#[test]
fn the_table_grants_exactly_what_the_design_says_it_grants() {
    // Spelled out rather than derived, so widening anybody's authority has to
    // be written down twice: once in the table and once here.
    const EXPECTED: &[(Operation, &[Principal])] = &[
        (Operation::CreateRun, &[Principal::User, Principal::Praetor]),
        (Operation::ApprovePlan, &[Principal::User]),
        (
            Operation::AnswerQuestion,
            &[Principal::User, Principal::Praetor],
        ),
        (Operation::AuthorizeMerge, &[Principal::User]),
        (Operation::CancelRun, &[Principal::User, Principal::Praetor]),
        (Operation::Retry, &[Principal::User, Principal::Praetor]),
        (Operation::Cleaning, &[Principal::User]),
        (Operation::Takeover, &[Principal::User]),
        (Operation::PauseAdmissions, &[Principal::User]),
        (Operation::Stop, &[Principal::User]),
        (Operation::Tree, &[Principal::User, Principal::Praetor]),
        (Operation::RunDetail, &[Principal::User, Principal::Praetor]),
        (Operation::Inbox, &[Principal::User, Principal::Praetor]),
        (Operation::Subscribe, &[Principal::User, Principal::Praetor]),
    ];

    for (operation, principals) in EXPECTED {
        assert_eq!(
            operation.principals(),
            *principals,
            "{} grants something else",
            operation.as_str()
        );
    }

    assert_eq!(EXPECTED.len(), EVERY_OPERATION.len());
}

#[test]
fn the_coordinator_reaches_no_team_operation() {
    const TEAM: &[Operation] = &[
        Operation::ApprovePlan,
        Operation::AnswerQuestion,
        Operation::AuthorizeMerge,
        Operation::CancelRun,
        Operation::Retry,
        Operation::Cleaning,
        Operation::Takeover,
        Operation::PauseAdmissions,
        Operation::Stop,
    ];

    for operation in TEAM {
        assert!(
            !operation.admits(Principal::Coordinator),
            "the coordinator reaches {}",
            operation.as_str()
        );
    }
}

#[test]
fn praetor_can_never_approve_a_plan() {
    let mut store = store();
    let run_id = store.insert_run(&run_in(RunState::Draft, None)).unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    let error = harness
        .core
        .approve_plan(Principal::Praetor, &ApprovePlan { run_id, now: NOW })
        .unwrap_err();

    assert_eq!(
        unauthorized(&error),
        (Operation::ApprovePlan, Principal::Praetor, true)
    );
    assert_eq!(harness.run_state(run_id), RunState::Draft);
    assert!(harness.scheduler.queue_changed.lock().unwrap().is_empty());
}

#[test]
fn praetor_can_never_authorize_a_merge() {
    let (mut harness, run_id, question_id) = parked_on(QuestionKind::Approval, "[\"merge\"]");

    let error = harness
        .core
        .authorize_merge(
            Principal::Praetor,
            &AuthorizeMerge {
                subject: MergeAuthorization::Existing(question_id),
                answer: "merge".to_owned(),
                now: NOW,
            },
        )
        .unwrap_err();

    assert_eq!(
        unauthorized(&error),
        (Operation::AuthorizeMerge, Principal::Praetor, true)
    );
    assert_eq!(
        harness
            .core
            .machines()
            .store()
            .load_question(question_id)
            .unwrap()
            .unwrap()
            .state,
        QuestionState::Open
    );
    assert_eq!(harness.run_state(run_id), RunState::AwaitingInput);
}

#[test]
fn praetor_cannot_route_an_approval_through_answer_question() {
    let (mut harness, _run_id, question_id) = parked_on(QuestionKind::Approval, "[\"merge\"]");

    let error = harness
        .core
        .answer_question(
            Principal::Praetor,
            &AnswerQuestion {
                question_id,
                answer: "merge".to_owned(),
                now: NOW,
            },
        )
        .unwrap_err();

    assert_eq!(
        unauthorized(&error),
        (Operation::AnswerQuestion, Principal::Praetor, true)
    );
}

#[test]
fn the_user_cannot_route_an_approval_through_answer_question_either() {
    // The split is the operation, not the principal: an authorization is
    // granted where the receipt is checked, and answer_question does not check
    // it.
    let (mut harness, _run_id, question_id) = parked_on(QuestionKind::Approval, "[\"merge\"]");

    let error = harness
        .core
        .answer_question(
            Principal::User,
            &AnswerQuestion {
                question_id,
                answer: "merge".to_owned(),
                now: NOW,
            },
        )
        .unwrap_err();

    assert_eq!(
        unauthorized(&error),
        (Operation::AnswerQuestion, Principal::User, true)
    );
}

#[test]
fn a_refusal_is_journaled_against_the_run() {
    let (mut harness, run_id, question_id) = parked_on(QuestionKind::Approval, "[\"merge\"]");

    harness
        .core
        .authorize_merge(
            Principal::Praetor,
            &AuthorizeMerge {
                subject: MergeAuthorization::Existing(question_id),
                answer: "merge".to_owned(),
                now: NOW,
            },
        )
        .unwrap_err();

    let events = harness
        .core
        .machines()
        .store()
        .events_for_run(run_id)
        .unwrap();
    let denial = events
        .iter()
        .find(|event| event.event_type == "authorization_denied")
        .expect("the refusal is not in the journal");

    let payload: serde_json::Value = serde_json::from_str(&denial.payload).unwrap();
    assert_eq!(payload["operation"], "authorize_merge");
    assert_eq!(payload["principal"], "praetor");
    assert_eq!(denial.ts, NOW);
}

#[test]
fn a_refusal_with_no_run_behind_it_is_journaled_all_the_same() {
    let mut harness = Harness::build(store(), RecordingWorktrees::new(false, true));

    let error = harness
        .core
        .pause_admissions(Principal::Praetor, true, NOW)
        .unwrap_err();

    assert_eq!(
        unauthorized(&error),
        (Operation::PauseAdmissions, Principal::Praetor, true)
    );

    let denial = harness
        .core
        .machines()
        .store()
        .events_for_run(1)
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == "authorization_denied");

    assert!(
        denial.is_none(),
        "a machine-wide refusal was filed under a run"
    );
}

#[test]
fn the_user_approves_a_plan_and_the_scheduler_hears_about_it() {
    let mut store = store();
    let run_id = store.insert_run(&run_in(RunState::Draft, None)).unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    let outcome = harness
        .core
        .approve_plan(Principal::User, &ApprovePlan { run_id, now: NOW })
        .unwrap();

    assert_eq!(outcome.applied().unwrap().to, RunState::Queued);
    assert_eq!(harness.run_state(run_id), RunState::Queued);
    assert_eq!(
        *harness.scheduler.queue_changed.lock().unwrap(),
        vec![run_id]
    );
    assert_eq!(
        harness.event_types(run_id),
        vec!["run_state_changed", "run_approved"]
    );
}

#[test]
fn the_user_authorizes_a_merge_with_a_receipt() {
    let (mut harness, _run_id, question_id) = parked_on(QuestionKind::Approval, "[\"merge\"]");

    let outcome = harness
        .core
        .authorize_merge(
            Principal::User,
            &AuthorizeMerge {
                subject: MergeAuthorization::Existing(question_id),
                answer: "merge".to_owned(),
                now: NOW,
            },
        )
        .unwrap();

    assert_eq!(outcome.grant.applied().unwrap().to, QuestionState::Answered);
    assert_eq!(outcome.question_id, question_id);

    let question = harness
        .core
        .machines()
        .store()
        .load_question(question_id)
        .unwrap()
        .unwrap();
    assert_eq!(question.author, Some(QuestionAuthor::User));
}

#[test]
fn authorizing_a_merge_for_a_run_opens_the_approval_and_freezes_its_receipt() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Done, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    let authorized = harness
        .core
        .authorize_merge(
            Principal::User,
            &AuthorizeMerge {
                subject: MergeAuthorization::ForRun {
                    run_id,
                    expires_at: Some(NOW + 600),
                },
                answer: "merge".to_owned(),
                now: NOW,
            },
        )
        .unwrap();

    assert_eq!(authorized.run_id, run_id);
    assert_eq!(
        authorized.grant.applied().unwrap().to,
        QuestionState::Answered
    );

    let approval = harness
        .core
        .machines()
        .store()
        .load_question(authorized.question_id)
        .unwrap()
        .unwrap();
    assert_eq!(approval.kind, QuestionKind::Approval);
    assert_eq!(approval.author, Some(QuestionAuthor::User));
    assert_eq!(approval.expires_at, Some(NOW + 600));
    assert_eq!(
        (approval.tree_hash, approval.paths_digest),
        (
            Some(harness.worktrees.derivation.tree_hash.clone()),
            Some(harness.worktrees.derivation.paths_digest.clone())
        ),
        "the receipt is derived from the worktree, never taken from the request"
    );
    assert_eq!(
        harness.event_types(run_id),
        [
            "approval_requested",
            "run_state_changed",
            "approval_granted"
        ],
        "the approval is announced when it is opened, not only when it is granted"
    );
}

#[test]
fn no_approval_is_frozen_over_a_worktree_with_uncommitted_work() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Done, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, false));

    let error = harness
        .core
        .authorize_merge(
            Principal::User,
            &AuthorizeMerge {
                subject: MergeAuthorization::ForRun {
                    run_id,
                    expires_at: None,
                },
                answer: "merge".to_owned(),
                now: NOW,
            },
        )
        .unwrap_err();

    assert_eq!(
        unauthorized(&error),
        (Operation::AuthorizeMerge, Principal::User, true)
    );
    assert!(
        harness
            .core
            .machines()
            .store()
            .questions_for_run(run_id)
            .unwrap()
            .is_empty(),
        "a refused authorization leaves no approval behind"
    );
}

#[test]
fn praetor_asking_for_an_approval_opens_nothing() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Done, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    let error = harness
        .core
        .authorize_merge(
            Principal::Praetor,
            &AuthorizeMerge {
                subject: MergeAuthorization::ForRun {
                    run_id,
                    expires_at: None,
                },
                answer: "merge".to_owned(),
                now: NOW,
            },
        )
        .unwrap_err();

    assert_eq!(
        unauthorized(&error),
        (Operation::AuthorizeMerge, Principal::Praetor, true)
    );
    assert!(
        harness
            .core
            .machines()
            .store()
            .questions_for_run(run_id)
            .unwrap()
            .is_empty(),
        "the table is checked before anything is opened"
    );
}

#[test]
fn an_approval_without_a_receipt_never_reaches_the_core() {
    // The core checks the receipt before granting, but it never gets the
    // chance: the schema refuses to store an approval without one, so there is
    // no receiptless approval for anyone to present.
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(
            RunState::AwaitingInput,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();

    let mut approval = question_in(run_id, QuestionKind::Approval, "[\"merge\"]");
    approval.tree_hash = None;
    approval.paths_digest = None;

    assert!(store.insert_question(&approval).is_err());
}

#[test]
fn authorize_merge_refuses_a_plain_question() {
    let (mut harness, _run_id, question_id) =
        parked_on(QuestionKind::Question, "[\"serde_json\",\"simd_json\"]");

    let error = harness
        .core
        .authorize_merge(
            Principal::User,
            &AuthorizeMerge {
                subject: MergeAuthorization::Existing(question_id),
                answer: "serde_json".to_owned(),
                now: NOW,
            },
        )
        .unwrap_err();

    assert_eq!(
        unauthorized(&error),
        (Operation::AuthorizeMerge, Principal::User, true)
    );
}

#[test]
fn praetor_answers_a_detail_question_and_the_run_resumes() {
    let (mut harness, run_id, question_id) =
        parked_on(QuestionKind::Question, "[\"serde_json\",\"simd_json\"]");

    let answered = harness
        .core
        .answer_question(
            Principal::Praetor,
            &AnswerQuestion {
                question_id,
                answer: "serde_json".to_owned(),
                now: NOW,
            },
        )
        .unwrap();

    assert_eq!(
        answered.question.applied().unwrap().to,
        QuestionState::Answered
    );
    assert_eq!(
        answered.run.as_ref().unwrap().applied().unwrap().to,
        RunState::Queued
    );
    assert_eq!(harness.run_state(run_id), RunState::Queued);

    let question = harness
        .core
        .machines()
        .store()
        .load_question(question_id)
        .unwrap()
        .unwrap();
    assert_eq!(question.author, Some(QuestionAuthor::Praetor));
}

#[test]
fn an_answer_is_enqueued_once_and_at_the_tool_call_edge() {
    let (mut harness, run_id, question_id) =
        parked_on(QuestionKind::Question, "[\"serde_json\",\"simd_json\"]");

    harness
        .core
        .answer_question(
            Principal::User,
            &AnswerQuestion {
                question_id,
                answer: "simd_json".to_owned(),
                now: NOW,
            },
        )
        .unwrap();

    let queued = harness.delivery.queued.lock().unwrap();
    assert_eq!(queued.len(), 1, "the answer reached the worker twice");
    assert_eq!(
        queued[0],
        Delivery {
            run_id,
            payload: DeliveryPayload::Answer {
                question_id,
                text: "simd_json".to_owned(),
            },
            grain: DeliveryGrain::ToolCall,
        }
    );
}

#[test]
fn praetor_cannot_answer_an_open_ended_question() {
    let (mut harness, run_id, question_id) = parked_on(QuestionKind::Question, "[]");

    let error = harness
        .core
        .answer_question(
            Principal::Praetor,
            &AnswerQuestion {
                question_id,
                answer: "rewrite it in another crate".to_owned(),
                now: NOW,
            },
        )
        .unwrap_err();

    assert_eq!(
        unauthorized(&error),
        (Operation::AnswerQuestion, Principal::Praetor, true)
    );
    assert_eq!(harness.run_state(run_id), RunState::AwaitingInput);
    assert!(harness.delivery.queued.lock().unwrap().is_empty());
}

#[test]
fn praetor_cannot_answer_outside_the_options_the_question_offered() {
    let (mut harness, _run_id, question_id) =
        parked_on(QuestionKind::Question, "[\"serde_json\",\"simd_json\"]");

    let error = harness
        .core
        .answer_question(
            Principal::Praetor,
            &AnswerQuestion {
                question_id,
                answer: "write our own".to_owned(),
                now: NOW,
            },
        )
        .unwrap_err();

    assert_eq!(
        unauthorized(&error),
        (Operation::AnswerQuestion, Principal::Praetor, true)
    );
}

#[test]
fn the_user_answers_the_open_ended_question_praetor_could_not() {
    let (mut harness, run_id, question_id) = parked_on(QuestionKind::Question, "[]");

    let answered = harness
        .core
        .answer_question(
            Principal::User,
            &AnswerQuestion {
                question_id,
                answer: "rewrite it in another crate".to_owned(),
                now: NOW,
            },
        )
        .unwrap();

    assert_eq!(
        answered.question.applied().unwrap().to,
        QuestionState::Answered
    );
    assert_eq!(harness.run_state(run_id), RunState::Queued);
}

#[test]
fn the_detail_question_policy_fails_closed_on_every_unknown() {
    let approval = question_in(1, QuestionKind::Approval, "[\"merge\"]");
    assert_eq!(
        praetor_may_answer(&approval, "merge"),
        Err(DetailQuestionRefusal::IsAuthorization)
    );

    let open_ended = question_in(1, QuestionKind::Question, "[]");
    assert_eq!(
        praetor_may_answer(&open_ended, "anything"),
        Err(DetailQuestionRefusal::OpenEnded)
    );

    let closed = question_in(1, QuestionKind::Question, "[\"a\",\"b\"]");
    assert_eq!(praetor_may_answer(&closed, "a"), Ok(()));
    assert_eq!(
        praetor_may_answer(&closed, "c"),
        Err(DetailQuestionRefusal::OutsideOptions)
    );

    let unreadable = question_in(1, QuestionKind::Question, "not json");
    assert_eq!(
        praetor_may_answer(&unreadable, "a"),
        Err(DetailQuestionRefusal::UnreadableOptions)
    );

    let not_an_array = question_in(1, QuestionKind::Question, "{\"a\":1}");
    assert_eq!(
        praetor_may_answer(&not_an_array, "a"),
        Err(DetailQuestionRefusal::UnreadableOptions)
    );
}

#[test]
fn answering_a_question_the_run_is_not_parked_on_moves_no_run() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    let question_id = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Question,
            "[\"yes\",\"no\"]",
        ))
        .unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    let answered = harness
        .core
        .answer_question(
            Principal::Praetor,
            &AnswerQuestion {
                question_id,
                answer: "yes".to_owned(),
                now: NOW,
            },
        )
        .unwrap();

    assert!(answered.run.is_none());
    assert_eq!(harness.run_state(run_id), RunState::Running);
    assert_eq!(harness.delivery.queued.lock().unwrap().len(), 1);
}

#[test]
fn the_harness_lifecycle_operation_pins_the_principal_it_reports_as() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    // A caller naming somebody else is not a caller the guard sees: the
    // operation overwrites the principal, so a client that reached this could
    // still not claim a run's lifecycle for a party that is not executing it.
    harness
        .core
        .report_run_lifecycle(
            run_id,
            RunTrigger::Finished,
            &RunFacts {
                now: NOW,
                principal: Principal::User,
                ..RunFacts::default()
            },
        )
        .unwrap();

    assert_eq!(harness.run_state(run_id), RunState::Done);
}

#[test]
fn praetor_cancels_a_run_and_the_session_is_signalled_once() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    harness
        .core
        .cancel_run(Principal::Praetor, &RunRef { run_id, now: NOW })
        .unwrap();
    assert_eq!(harness.run_state(run_id), RunState::Cancelled);
    assert_eq!(*harness.sessions.cancelled.lock().unwrap(), vec![run_id]);

    let repeated = harness
        .core
        .cancel_run(Principal::User, &RunRef { run_id, now: NOW })
        .unwrap();

    assert!(repeated.applied().is_none());
    assert_eq!(
        *harness.sessions.cancelled.lock().unwrap(),
        vec![run_id],
        "an idempotent cancel signalled the session twice"
    );
}

#[test]
fn a_retry_records_who_asked_for_it() {
    for (principal, expected) in [
        (Principal::User, RetryTrigger::User),
        (Principal::Praetor, RetryTrigger::Praetor),
    ] {
        let mut store = store();
        let run_id = store
            .insert_run(&run_in(RunState::Failed, Some(WorktreeStatus::Active)))
            .unwrap();
        let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

        harness
            .core
            .retry(
                principal,
                &RetryRequest {
                    run_id,
                    guidance: "keep the transaction, drop the second writer".to_owned(),
                    retry_budget: 3,
                    now: NOW,
                },
            )
            .unwrap();

        assert_eq!(harness.run_state(run_id), RunState::Queued);
        assert_eq!(
            *harness.scheduler.queue_changed.lock().unwrap(),
            vec![run_id]
        );

        let events = harness
            .core
            .machines()
            .store()
            .events_for_run(run_id)
            .unwrap();
        let retried = events
            .iter()
            .find(|event| event.event_type == "run_retried")
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&retried.payload).unwrap();

        assert_eq!(payload["retry_trigger"], expected.as_str());
    }
}

#[test]
fn a_retry_without_guidance_is_refused_by_the_machine_not_by_the_table() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Failed, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    let error = harness
        .core
        .retry(
            Principal::Praetor,
            &RetryRequest {
                run_id,
                guidance: String::new(),
                retry_budget: 3,
                now: NOW,
            },
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ApiError::Rejected(TransitionRejection::GuardFailed { .. })
    ));
    assert_eq!(harness.run_state(run_id), RunState::Failed);
}

#[test]
fn praetor_cannot_clean_a_worktree() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Done, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    let error = harness
        .core
        .cleaning(
            Principal::Praetor,
            &CleaningAction {
                run_id,
                disposition: CleaningDisposition::Dispose,
                confirmed: true,
                now: NOW,
            },
        )
        .unwrap_err();

    assert_eq!(
        unauthorized(&error),
        (Operation::Cleaning, Principal::Praetor, true)
    );
    assert!(harness.worktrees.removed.lock().unwrap().is_empty());
}

#[test]
fn disposing_an_active_worktree_needs_a_confirmation() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Done, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    let error = harness
        .core
        .cleaning(
            Principal::User,
            &CleaningAction {
                run_id,
                disposition: CleaningDisposition::Dispose,
                confirmed: false,
                now: NOW,
            },
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ApiError::Rejected(TransitionRejection::GuardFailed { .. })
    ));
    assert!(harness.worktrees.removed.lock().unwrap().is_empty());

    let disposed = harness
        .core
        .cleaning(
            Principal::User,
            &CleaningAction {
                run_id,
                disposition: CleaningDisposition::Dispose,
                confirmed: true,
                now: NOW,
            },
        )
        .unwrap();

    assert_eq!(disposed.applied().unwrap().to, WorktreeStatus::Cleaned);
    assert_eq!(*harness.worktrees.removed.lock().unwrap(), vec![run_id]);
}

#[test]
fn reclaiming_takes_its_facts_from_the_live_derivation() {
    let mut dirty = store();
    let run_id = dirty
        .insert_run(&run_in(RunState::Done, Some(WorktreeStatus::Reclaimable)))
        .unwrap();
    let mut harness = Harness::build(dirty, RecordingWorktrees::new(true, false));

    let error = harness
        .core
        .cleaning(
            Principal::User,
            &CleaningAction {
                run_id,
                disposition: CleaningDisposition::Reclaim,
                confirmed: false,
                now: NOW,
            },
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ApiError::Rejected(TransitionRejection::GuardFailed { .. })
    ));
    assert!(harness.worktrees.removed.lock().unwrap().is_empty());

    let mut clean = store();
    let clean_run = clean
        .insert_run(&run_in(RunState::Done, Some(WorktreeStatus::Reclaimable)))
        .unwrap();
    let mut harness = Harness::build(clean, RecordingWorktrees::new(true, true));

    let reclaimed = harness
        .core
        .cleaning(
            Principal::User,
            &CleaningAction {
                run_id: clean_run,
                disposition: CleaningDisposition::Reclaim,
                confirmed: false,
                now: NOW,
            },
        )
        .unwrap();

    assert_eq!(reclaimed.applied().unwrap().to, WorktreeStatus::Cleaned);
    assert_eq!(*harness.worktrees.removed.lock().unwrap(), vec![clean_run]);
}

#[test]
fn takeover_pause_and_stop_are_the_users_alone() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    let takeover = harness
        .core
        .takeover(Principal::Praetor, &RunRef { run_id, now: NOW })
        .unwrap_err();
    assert_eq!(
        unauthorized(&takeover),
        (Operation::Takeover, Principal::Praetor, true)
    );

    let paused = harness
        .core
        .pause_admissions(Principal::Praetor, true, NOW)
        .unwrap_err();
    assert_eq!(
        unauthorized(&paused),
        (Operation::PauseAdmissions, Principal::Praetor, true)
    );

    let stopped = harness
        .core
        .stop(
            Principal::Praetor,
            &StopRequest {
                scope: StopScope::Machine,
                now: NOW,
            },
        )
        .unwrap_err();
    assert_eq!(
        unauthorized(&stopped),
        (Operation::Stop, Principal::Praetor, true)
    );

    assert!(harness.sessions.taken_over.lock().unwrap().is_empty());
    assert!(harness.sessions.stopped.lock().unwrap().is_empty());
    assert!(!harness.scheduler.admissions_paused());
}

#[test]
fn the_user_takes_over_a_run() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(RunState::Running, Some(WorktreeStatus::Active)))
        .unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    let handle = harness
        .core
        .takeover(Principal::User, &RunRef { run_id, now: NOW })
        .unwrap();

    assert_eq!(handle.run_id, run_id);
    assert_eq!(*harness.sessions.taken_over.lock().unwrap(), vec![run_id]);
}

#[test]
fn pausing_admission_reports_what_it_was_before() {
    let mut harness = Harness::build(store(), RecordingWorktrees::new(false, true));

    let first = harness
        .core
        .pause_admissions(Principal::User, true, NOW)
        .unwrap();
    assert!(first.changed());
    assert!(harness.scheduler.admissions_paused());

    let repeated = harness
        .core
        .pause_admissions(Principal::User, true, NOW)
        .unwrap();
    assert!(!repeated.changed());
}

#[test]
fn stopping_pauses_admission_before_it_stops_anything() {
    let mut harness = Harness::build(store(), RecordingWorktrees::new(false, true));

    let stopped = harness
        .core
        .stop(
            Principal::User,
            &StopRequest {
                scope: StopScope::Repo(REPO.to_owned()),
                now: NOW,
            },
        )
        .unwrap();

    assert!(stopped.paused);
    assert!(harness.scheduler.admissions_paused());
    assert_eq!(
        *harness.sessions.stopped.lock().unwrap(),
        vec![StopScope::Repo(REPO.to_owned())]
    );
}

#[test]
fn the_tree_is_scoped_to_one_repository() {
    let mut store = store();
    let mine = store.insert_run(&run_in(RunState::Queued, None)).unwrap();

    let mut other = run_in(RunState::Queued, None);
    other.repo_id = "ffffffffffffffff".to_owned();
    store.insert_run(&other).unwrap();

    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    let tree = harness.core.tree(Principal::Praetor, REPO, NOW).unwrap();

    assert_eq!(tree.repo_id, REPO);
    assert_eq!(
        tree.runs.iter().map(|run| run.run_id).collect::<Vec<_>>(),
        vec![mine]
    );
}

#[test]
fn the_inbox_lists_only_what_is_still_open() {
    let mut store = store();
    let run_id = store
        .insert_run(&run_in(
            RunState::AwaitingInput,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();

    let open = store
        .insert_question(&question_in(
            run_id,
            QuestionKind::Question,
            "[\"a\",\"b\"]",
        ))
        .unwrap();

    let mut expired = question_in(run_id, QuestionKind::Approval, "[\"merge\"]");
    expired.state = QuestionState::Expired;
    store.insert_question(&expired).unwrap();

    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    let inbox = harness.core.inbox(Principal::User, REPO, NOW).unwrap();

    assert_eq!(
        inbox
            .items
            .iter()
            .map(|item| item.question_id)
            .collect::<Vec<_>>(),
        vec![open]
    );
    assert_eq!(inbox.items[0].recommendation.as_deref(), Some("serde_json"));
}

#[test]
fn the_run_detail_carries_everything_recorded_about_the_run() {
    let mut store = store();
    let run_id = store.insert_run(&run_in(RunState::Draft, None)).unwrap();
    let question_id = store
        .insert_question(&question_in(run_id, QuestionKind::Question, "[\"a\"]"))
        .unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    harness
        .core
        .approve_plan(Principal::User, &ApprovePlan { run_id, now: NOW })
        .unwrap();

    let detail = harness
        .core
        .run_detail(Principal::User, run_id, NOW)
        .unwrap();

    assert_eq!(detail.run.state, RunState::Queued);
    assert_eq!(
        detail
            .questions
            .iter()
            .map(|question| question.id.unwrap())
            .collect::<Vec<_>>(),
        vec![question_id]
    );
    assert_eq!(
        detail
            .events
            .iter()
            .map(|event| event.event_type.clone())
            .collect::<Vec<_>>(),
        vec!["run_state_changed", "run_approved"]
    );
    assert!(detail.attempts.is_empty());
    assert!(detail.health.is_none());
}

#[test]
fn a_subscription_carries_the_filter_through_to_the_feed() {
    let mut harness = Harness::build(store(), RecordingWorktrees::new(false, true));

    let filter = EventFilter {
        repo_id: Some(REPO.to_owned()),
        run_id: None,
        classes: vec![agens_store::EventClass::Agent],
    };

    harness
        .core
        .subscribe(Principal::Praetor, &filter, NOW)
        .unwrap();

    assert_eq!(*harness.feed.filters.lock().unwrap(), vec![filter]);
}

#[test]
fn the_read_plane_never_writes() {
    let mut store = store();
    let run_id = store.insert_run(&run_in(RunState::Queued, None)).unwrap();
    let mut harness = Harness::build(store, RecordingWorktrees::new(false, true));

    harness.core.tree(Principal::Praetor, REPO, NOW).unwrap();
    harness
        .core
        .run_detail(Principal::Praetor, run_id, NOW)
        .unwrap();
    harness.core.inbox(Principal::Praetor, REPO, NOW).unwrap();

    assert_eq!(harness.run_state(run_id), RunState::Queued);
    assert!(harness.event_types(run_id).is_empty());
}

/// A checkout on disk, canonical, and outside every other test's.
fn checkout() -> std::path::PathBuf {
    let directory = data_directory().join("checkout");
    fs::create_dir_all(&directory).unwrap();

    directory.canonicalize().unwrap()
}

fn create_run(repository: &std::path::Path) -> CreateRun {
    CreateRun {
        repo_root: repository.to_path_buf(),
        task: "the worker harness".to_owned(),
        scope: "crates/agens-cli/src/worker".to_owned(),
        dod: "a run executes against the scripted provider".to_owned(),
        external_ref: Some("agens/AGN-181".to_owned()),
        parent_run_id: None,
        dep_run_id: None,
        provider: "openai-api".to_owned(),
        priority: 5,
        budget_tokens: None,
        start_point: "HEAD".to_owned(),
        now: NOW,
    }
}

#[test]
fn a_created_run_is_a_proposal_with_a_worktree_of_its_own() {
    let mut harness = Harness::build(store(), RecordingWorktrees::new(false, true));
    let creation = harness.creation();

    let created = harness.core.create_run(Principal::User, &creation).unwrap();

    let run = harness
        .core
        .machines()
        .store()
        .load_run(created.run_id)
        .unwrap()
        .unwrap();

    assert_eq!(
        run.state,
        RunState::Draft,
        "creating proposes an execution; only the user's approval queues it"
    );
    assert_eq!(run.repo_id, REPO, "the repository's identity is derived");
    assert_eq!(
        run.worktree_status,
        Some(WorktreeStatus::Active),
        "the run has a worktree the scheduler can admit it into"
    );
    assert_eq!(
        run.worktree_path.as_deref(),
        Some(created.worktree_path.display().to_string().as_str())
    );
    assert_eq!(
        harness.worktrees.provisioned.lock().unwrap().len(),
        1,
        "exactly one worktree is provisioned for one run"
    );
    assert_eq!(
        harness.event_types(created.run_id),
        vec!["run_created".to_owned()],
        "a run that exists says so in the journal"
    );
    assert!(
        harness.scheduler.queue_changed.lock().unwrap().is_empty(),
        "a draft is not queued, so admission has no occasion to look"
    );
}

#[test]
fn a_created_run_reaches_the_queue_only_through_approval() {
    let mut harness = Harness::build(store(), RecordingWorktrees::new(false, true));
    let creation = harness.creation();

    let created = harness
        .core
        .create_run(Principal::Praetor, &creation)
        .unwrap();

    assert_eq!(harness.run_state(created.run_id), RunState::Draft);

    harness
        .core
        .approve_plan(
            Principal::User,
            &ApprovePlan {
                run_id: created.run_id,
                now: NOW,
            },
        )
        .unwrap();

    assert_eq!(harness.run_state(created.run_id), RunState::Queued);
    assert_eq!(
        *harness.scheduler.queue_changed.lock().unwrap(),
        vec![created.run_id]
    );
}

#[test]
fn a_run_with_no_scope_or_definition_of_done_is_refused_before_any_worktree_exists() {
    let mut harness = Harness::build(store(), RecordingWorktrees::new(false, true));

    let error = harness
        .core
        .create_run(
            Principal::User,
            &CreateRun {
                scope: "   ".to_owned(),
                dod: String::new(),
                ..harness.creation()
            },
        )
        .unwrap_err();

    match error {
        ApiError::Unauthorized { reason, .. } => assert_eq!(
            reason, "a run needs a scope, a definition of done",
            "the refusal names every field that was missing"
        ),
        other => panic!("a run nothing could measure is refused: {other:?}"),
    }

    assert!(
        harness.worktrees.provisioned.lock().unwrap().is_empty(),
        "nothing is created on disk for a run that was never accepted"
    );
}

// The repository policy: which checkouts a daemon serves, and whose hooks it
// is willing to execute. Both are the operator's, and neither is derivable
// from a request that arrives over a socket authenticating nobody.

#[test]
fn a_checkout_outside_every_configured_root_is_refused_before_any_worktree_exists() {
    let served = checkout();
    let elsewhere = checkout();
    let harness = Harness::with_policy(
        store(),
        RecordingWorktrees::new(false, true),
        Arc::new(RecordingPolicy::serving(&served)),
        elsewhere.clone(),
    );
    let mut harness = harness;

    let error = harness
        .core
        .create_run(Principal::User, &create_run(&elsewhere))
        .unwrap_err();

    match error {
        ApiError::Unauthorized { reason, .. } => assert!(
            reason.contains("does not serve"),
            "the refusal names the checkout and what would admit it: {reason}"
        ),
        other => panic!("a checkout the daemon does not serve is refused: {other:?}"),
    }

    assert!(
        harness.worktrees.provisioned.lock().unwrap().is_empty(),
        "nothing is created on disk for a repository the daemon does not serve"
    );
}

#[test]
fn a_checkout_named_through_a_traversal_is_admitted_as_the_path_it_resolves_to() {
    let mut harness = Harness::build(store(), RecordingWorktrees::new(false, true));
    let indirect = harness.repository.join("..").join(
        harness
            .repository
            .file_name()
            .map(std::ffi::OsStr::to_owned)
            .unwrap(),
    );

    let created = harness
        .core
        .create_run(
            Principal::User,
            &CreateRun {
                repo_root: indirect,
                ..harness.creation()
            },
        )
        .unwrap();

    let run = harness
        .core
        .machines()
        .store()
        .load_run(created.run_id)
        .unwrap()
        .unwrap();

    assert_eq!(
        run.repo_root,
        harness.repository.display().to_string(),
        "the run records the checkout the daemon resolved, not the one it was handed"
    );
}

#[test]
fn a_repository_whose_hooks_the_operator_authorized_runs_them() {
    let repository = checkout();
    let mut harness = Harness::with_policy(
        store(),
        RecordingWorktrees::new(false, true).declaring_hooks(&["devshell"]),
        Arc::new(RecordingPolicy::serving(&repository).trusting(HookTrust::Granted)),
        repository.clone(),
    );

    let created = harness
        .core
        .create_run(Principal::User, &create_run(&repository))
        .unwrap();

    assert_eq!(
        *harness.worktrees.hook_policies.lock().unwrap(),
        vec![HookPolicy::Allow]
    );
    assert!(created.hooks_ran, "an authorized repository's hooks run");
    assert_eq!(
        created.hook_authorization_question, None,
        "nothing is asked about a repository already decided on"
    );
}

#[test]
fn a_repository_nobody_has_decided_on_does_not_run_its_hooks_and_asks() {
    let repository = checkout();
    let mut harness = Harness::with_policy(
        store(),
        RecordingWorktrees::new(false, true).declaring_hooks(&["devshell", "fixtures"]),
        Arc::new(RecordingPolicy::serving(&repository)),
        repository.clone(),
    );

    let created = harness
        .core
        .create_run(Principal::User, &create_run(&repository))
        .unwrap();

    assert_eq!(
        *harness.worktrees.hook_policies.lock().unwrap(),
        vec![HookPolicy::Ask]
    );
    assert!(
        !created.hooks_ran,
        "an undecided repository's hooks do not run"
    );

    let question_id = created
        .hook_authorization_question
        .expect("an undecided repository is asked about");
    let question = harness
        .core
        .machines()
        .store()
        .load_question(question_id)
        .unwrap()
        .unwrap();

    assert_eq!(question.run_id, created.run_id);
    assert_eq!(question.state, QuestionState::Open);
    assert_eq!(question.kind, QuestionKind::Question);
    assert!(
        question
            .recommendation
            .as_deref()
            .is_some_and(|shown| shown.contains("devshell")
                && shown.contains("fixtures")
                && shown.contains("credentials")),
        "the operator is shown what would run and what it inherits: {question:?}"
    );
    assert_eq!(
        harness.policy.pending.lock().unwrap().len(),
        1,
        "what answering that question grants is recorded where the answer will look"
    );
}

#[test]
fn a_repository_declaring_no_hooks_is_asked_nothing() {
    let mut harness = Harness::build(store(), RecordingWorktrees::new(false, true));
    let creation = harness.creation();

    let created = harness.core.create_run(Principal::User, &creation).unwrap();

    assert_eq!(
        created.hook_authorization_question, None,
        "a repository that declares no hooks has nothing to authorize"
    );
    assert!(harness.policy.pending.lock().unwrap().is_empty());
}

#[test]
fn a_repository_whose_hooks_the_operator_refused_is_not_asked_again() {
    let repository = checkout();
    let mut harness = Harness::with_policy(
        store(),
        RecordingWorktrees::new(false, true).declaring_hooks(&["devshell"]),
        Arc::new(RecordingPolicy::serving(&repository).trusting(HookTrust::Refused)),
        repository.clone(),
    );

    let created = harness
        .core
        .create_run(Principal::User, &create_run(&repository))
        .unwrap();

    assert_eq!(
        *harness.worktrees.hook_policies.lock().unwrap(),
        vec![HookPolicy::Deny]
    );
    assert_eq!(created.hook_authorization_question, None);
}

#[test]
fn a_run_praetor_proposed_never_runs_a_repositorys_hooks() {
    let repository = checkout();
    let mut harness = Harness::with_policy(
        store(),
        RecordingWorktrees::new(false, true).declaring_hooks(&["devshell"]),
        Arc::new(RecordingPolicy::serving(&repository).trusting(HookTrust::Granted)),
        repository.clone(),
    );

    let created = harness
        .core
        .create_run(Principal::Praetor, &create_run(&repository))
        .unwrap();

    assert_eq!(
        *harness.worktrees.hook_policies.lock().unwrap(),
        vec![HookPolicy::Deny],
        "the manager does not reach repository code, whatever the operator trusts"
    );
    assert!(!created.hooks_ran);
    assert_eq!(created.hook_authorization_question, None);
}

#[test]
fn answering_the_hook_question_records_the_operators_decision() {
    let repository = checkout();
    let mut harness = Harness::with_policy(
        store(),
        RecordingWorktrees::new(false, true).declaring_hooks(&["devshell"]),
        Arc::new(RecordingPolicy::serving(&repository)),
        repository.clone(),
    );

    let created = harness
        .core
        .create_run(Principal::User, &create_run(&repository))
        .unwrap();
    let question_id = created.hook_authorization_question.unwrap();

    harness
        .core
        .answer_question(
            Principal::User,
            &AnswerQuestion {
                question_id,
                answer: "trust".to_owned(),
                now: NOW,
            },
        )
        .unwrap();

    assert_eq!(
        *harness.policy.decided.lock().unwrap(),
        vec![(REPO.to_owned(), true)],
        "the answer is what grants the repository's hooks, durably"
    );
}

#[test]
fn an_answer_that_is_not_the_grant_refuses_the_repositorys_hooks() {
    let repository = checkout();
    let mut harness = Harness::with_policy(
        store(),
        RecordingWorktrees::new(false, true).declaring_hooks(&["devshell"]),
        Arc::new(RecordingPolicy::serving(&repository)),
        repository.clone(),
    );

    let created = harness
        .core
        .create_run(Principal::User, &create_run(&repository))
        .unwrap();
    let question_id = created.hook_authorization_question.unwrap();

    harness
        .core
        .answer_question(
            Principal::User,
            &AnswerQuestion {
                question_id,
                answer: "refuse".to_owned(),
                now: NOW,
            },
        )
        .unwrap();

    assert_eq!(
        *harness.policy.decided.lock().unwrap(),
        vec![(REPO.to_owned(), false)]
    );
}

#[test]
fn praetor_may_not_authorize_a_repositorys_hooks() {
    let repository = checkout();
    let mut harness = Harness::with_policy(
        store(),
        RecordingWorktrees::new(false, true).declaring_hooks(&["devshell"]),
        Arc::new(RecordingPolicy::serving(&repository)),
        repository.clone(),
    );

    let created = harness
        .core
        .create_run(Principal::User, &create_run(&repository))
        .unwrap();
    let question_id = created.hook_authorization_question.unwrap();

    let error = harness
        .core
        .answer_question(
            Principal::Praetor,
            &AnswerQuestion {
                question_id,
                answer: "trust".to_owned(),
                now: NOW,
            },
        )
        .unwrap_err();

    match error {
        ApiError::Unauthorized { reason, .. } => assert!(
            reason.contains("user's alone"),
            "the refusal says whose decision it is: {reason}"
        ),
        other => panic!("the manager does not grant hook authorization: {other:?}"),
    }

    assert!(
        harness.policy.decided.lock().unwrap().is_empty(),
        "a refused answer grants nothing"
    );
}
