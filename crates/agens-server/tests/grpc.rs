//! The gRPC facade, driven by real clients over real sockets.
//!
//! Nothing here calls the service core directly. Every assertion goes through a
//! connected client, because what the facade is worth proving is that a request
//! crossing a socket reaches the core with the authority the core decided and
//! not the authority the request claimed.
//!
//! Two principals are served on purpose. The daemon's facade is the user's, but
//! the same facade built for Praetor has to be refused everything the
//! authorization table keeps for a person — over the wire, not in a unit test —
//! because that is the property a new transport could otherwise quietly widen.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agens_core::HeadlessTurnCancellation;
use agens_server::grpc::proto::{self, feed_client::FeedClient, team_client::TeamClient};
use agens_server::{
    ApiCore, BlockingBoundary, CoreHandle, Delivery, DeliveryQueue, EventFeed, EventFilter,
    FacadeBinding, FeedFacade, PortError, Ports, Principal, SchedulerPort, SessionControl,
    StateMachines, StopScope, Subscription, TakeoverHandle, TeamFacade, WorktreeDerivation,
    WorktreeGate,
};
use agens_store::{
    ControlPlaneStore, EventClass, EventRow, QuestionKind, QuestionRow, QuestionState, RunRow,
    RunState, WorktreeStatus,
};
use tonic::Code;
use tonic::transport::{Endpoint, Server, Uri};

const REPO: &str = "a1b2c3d4e5f60718";
const OTHER_REPO: &str = "0000111122223333";

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn scratch_directory(kind: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-server-grpc-{kind}-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

// The doubles. They record nothing the wire tests need to read back: what is
// under test is the crossing, and the core's own suite already proves which
// port an applied transition reaches.

#[derive(Default)]
struct StubScheduler {
    paused: AtomicBool,
}

impl SchedulerPort for StubScheduler {
    fn admissions_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    fn set_admissions_paused(&self, paused: bool) -> Result<bool, PortError> {
        Ok(self.paused.swap(paused, Ordering::Relaxed))
    }

    fn queue_changed(&self, _run_id: i64) {}
}

/// A worktree whose branch git says is merged and whose tree is clean, so the
/// cleaning transition has facts it can act on.
struct StubWorktrees;

impl WorktreeGate for StubWorktrees {
    fn derive(&self, _run: &RunRow) -> Result<WorktreeDerivation, PortError> {
        Ok(WorktreeDerivation {
            branch_merged: true,
            worktree_clean: true,
            tree_hash: "c0ffee".repeat(6),
            paths_digest: "d1ge57".repeat(6),
        })
    }

    fn remove(&self, _run: &RunRow) -> Result<(), PortError> {
        Ok(())
    }
}

struct StubDelivery;

impl DeliveryQueue for StubDelivery {
    fn enqueue(&self, _delivery: &Delivery) -> Result<(), PortError> {
        Ok(())
    }
}

struct StubSessions;

impl SessionControl for StubSessions {
    fn cancel(&self, _run_id: i64) -> Result<(), PortError> {
        Ok(())
    }

    fn take_over(&self, run_id: i64) -> Result<TakeoverHandle, PortError> {
        Ok(TakeoverHandle {
            run_id,
            session_id: 42,
        })
    }

    fn stop(&self, _scope: &StopScope) -> Result<(), PortError> {
        Ok(())
    }
}

/// A fan-out that hands the subscriber a channel the test keeps the sending end
/// of, so `Subscribe` can be driven with a journal entry that really crosses the
/// stream.
#[derive(Default)]
struct StubFeed {
    senders: Mutex<Vec<std::sync::mpsc::Sender<EventRow>>>,
}

impl StubFeed {
    fn publish(&self, event: &EventRow) {
        for sender in self.senders.lock().unwrap().iter() {
            let _ = sender.send(event.clone());
        }
    }

    fn subscribers(&self) -> usize {
        self.senders.lock().unwrap().len()
    }
}

impl EventFeed for StubFeed {
    fn subscribe(&self, _filter: &EventFilter) -> Result<Subscription, PortError> {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.senders.lock().unwrap().push(sender);

        Ok(receiver)
    }
}

fn run_in(repo_id: &str, state: RunState, worktree_status: Option<WorktreeStatus>) -> RunRow {
    RunRow {
        id: None,
        repo_id: repo_id.to_owned(),
        repo_root: "/home/dev/agens".to_owned(),
        remote_url: Some("git@example.com:dev/agens.git".to_owned()),
        external_ref: Some("agens/AGN-64".to_owned()),
        parent_run_id: None,
        task: "the grpc facade".to_owned(),
        scope: "crates/agens-server/src/grpc".to_owned(),
        dod: "two planes over one core".to_owned(),
        genesis_paths: None,
        state,
        priority: 5,
        dep_run_id: None,
        provider: "anthropic".to_owned(),
        budget_tokens: Some(200_000),
        worktree_path: Some("/data/worktrees/agens-a1b2c3d4/agn-64".to_owned()),
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

/// Every row the wire tests drive, in one store.
///
/// Each operation gets its own run, because they move state: sharing one run
/// would make the suite order-dependent in exactly the way the state machines
/// are designed to catch.
struct Fixture {
    draft: i64,
    parked: i64,
    parked_question: i64,
    merge_approval: i64,
    running: i64,
    failed: i64,
    merged: i64,
    taken_over: i64,
}

fn seeded_store(directory: &Path) -> (ControlPlaneStore, Fixture) {
    let mut store = ControlPlaneStore::open(directory).unwrap();

    let draft = store
        .insert_run(&run_in(REPO, RunState::Draft, None))
        .unwrap();

    let parked = store
        .insert_run(&run_in(
            REPO,
            RunState::AwaitingInput,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();
    let parked_question = store
        .insert_question(&question_in(
            parked,
            QuestionKind::Question,
            r#"["serde_json","prost"]"#,
        ))
        .unwrap();

    let awaiting_merge = store
        .insert_run(&run_in(REPO, RunState::Done, Some(WorktreeStatus::Active)))
        .unwrap();
    let merge_approval = store
        .insert_question(&question_in(
            awaiting_merge,
            QuestionKind::Approval,
            r#"["merge"]"#,
        ))
        .unwrap();

    let running = store
        .insert_run(&run_in(
            REPO,
            RunState::Running,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();

    let failed = store
        .insert_run(&run_in(
            REPO,
            RunState::Failed,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();

    // Already released for reclaim: the worktree machine only reaches `cleaned`
    // from there, because releasing is the step that re-derived the merge.
    let merged = store
        .insert_run(&run_in(
            REPO,
            RunState::Done,
            Some(WorktreeStatus::Reclaimable),
        ))
        .unwrap();

    let taken_over = store
        .insert_run(&run_in(
            REPO,
            RunState::Running,
            Some(WorktreeStatus::Active),
        ))
        .unwrap();

    // One run of another project, so a repository-scoped listing has something
    // to leave out rather than only something to include.
    store
        .insert_run(&run_in(OTHER_REPO, RunState::Running, None))
        .unwrap();

    (
        store,
        Fixture {
            draft,
            parked,
            parked_question,
            merge_approval,
            running,
            failed,
            merged,
            taken_over,
        },
    )
}

/// A facade serving one principal on a unix socket, with clients connected to
/// it.
struct Wire {
    team: TeamClient<tonic::transport::Channel>,
    feed: FeedClient<tonic::transport::Channel>,
    events: Arc<StubFeed>,
    fixture: Fixture,
    shutdown: HeadlessTurnCancellation,
}

impl Drop for Wire {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn socket_in(directory: &Path) -> PathBuf {
    directory.join("facade.sock")
}

async fn connect_unix(path: PathBuf) -> tonic::transport::Channel {
    // The authority is never used: the connector below hands back a unix stream
    // whatever the URI says, and gRPC still wants a syntactically valid one.
    Endpoint::try_from("http://localhost")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = path.clone();

            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;

                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .unwrap()
}

async fn wire_for(principal: Principal) -> Wire {
    let directory = scratch_directory(principal.as_str());
    let (store, fixture) = seeded_store(&directory);

    let events = Arc::new(StubFeed::default());
    let ports = Ports {
        scheduler: Arc::new(StubScheduler::default()),
        worktrees: Arc::new(StubWorktrees),
        delivery: Arc::new(StubDelivery),
        sessions: Arc::new(StubSessions),
        feed: Arc::clone(&events) as Arc<dyn EventFeed>,
    };

    let core = Arc::new(Mutex::new(ApiCore::new(StateMachines::new(store), ports)));
    let blocking = BlockingBoundary::new(tokio::runtime::Handle::current());
    let handle = CoreHandle::new(core, blocking, principal);

    let socket = socket_in(&directory);
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let shutdown = HeadlessTurnCancellation::new();
    let parked = shutdown.clone();

    tokio::spawn(async move {
        Server::builder()
            .add_service(proto::team_server::TeamServer::new(TeamFacade::new(
                handle.clone(),
            )))
            .add_service(proto::feed_server::FeedServer::new(FeedFacade::new(handle)))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::UnixListenerStream::new(listener),
                async move {
                    while !parked.is_cancelled() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                },
            )
            .await
    });

    let channel = connect_unix(socket).await;

    Wire {
        team: TeamClient::new(channel.clone()),
        feed: FeedClient::new(channel),
        events,
        fixture,
        shutdown,
    }
}

fn code(error: &tonic::Status) -> Code {
    error.code()
}

#[tokio::test]
async fn every_team_rpc_reaches_the_core_over_a_unix_socket() {
    let mut wire = wire_for(Principal::User).await;
    let fixture = &wire.fixture;

    let approved = wire
        .team
        .approve_plan(proto::ApprovePlanRequest {
            run_id: fixture.draft,
        })
        .await
        .unwrap()
        .into_inner();
    let approved = approved.transition.unwrap();
    assert!(approved.applied);
    assert_eq!(
        (approved.from.as_str(), approved.to.as_str()),
        ("draft", "queued")
    );

    let answered = wire
        .team
        .answer_question(proto::AnswerQuestionRequest {
            question_id: fixture.parked_question,
            answer: "serde_json".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(answered.run_id, fixture.parked);
    assert_eq!(answered.question.unwrap().to, "answered");
    assert_eq!(
        answered.run.expect("a parked run resumes").to,
        "queued",
        "answering the question a run is parked on puts it back in the queue"
    );

    let granted = wire
        .team
        .authorize_merge(proto::AuthorizeMergeRequest {
            question_id: fixture.merge_approval,
            answer: "merge".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(granted.transition.unwrap().to, "answered");

    let cancelled = wire
        .team
        .cancel_run(proto::CancelRunRequest {
            run_id: fixture.running,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cancelled.transition.unwrap().to, "cancelled");

    let retried = wire
        .team
        .retry(proto::RetryRequest {
            run_id: fixture.failed,
            guidance: "the parser needs the escaped case".to_owned(),
            retry_budget: 3,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(retried.transition.unwrap().to, "queued");

    let cleaned = wire
        .team
        .cleaning(proto::CleaningRequest {
            run_id: fixture.merged,
            disposition: "reclaim".to_owned(),
            confirmed: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cleaned.transition.unwrap().to, "cleaned");

    let taken = wire
        .team
        .takeover(proto::TakeoverRequest {
            run_id: fixture.taken_over,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(taken.run_id, fixture.taken_over);
    assert_eq!(taken.session_id, 42);

    let paused = wire
        .team
        .pause_admissions(proto::PauseAdmissionsRequest { paused: true })
        .await
        .unwrap()
        .into_inner();
    assert!(paused.paused && paused.changed && !paused.previously_paused);

    let stopped = wire
        .team
        .stop(proto::StopRequest {
            scope: Some(proto::stop_request::Scope::Machine(true)),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        stopped.paused && !stopped.changed,
        "a stop pauses admission before it stops anything, and this one was already paused"
    );
}

#[tokio::test]
async fn every_feed_rpc_reaches_the_core_over_a_unix_socket() {
    let mut wire = wire_for(Principal::User).await;
    let run_id = wire.fixture.parked;

    let tree = wire
        .feed
        .tree(proto::TreeRequest {
            repo_id: REPO.to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(tree.repo_id, REPO);
    assert_eq!(
        tree.runs.len(),
        7,
        "one daemon serves N projects, and the tree carries one"
    );
    assert!(tree.runs.iter().all(|run| run.task == "the grpc facade"));

    let detail = wire
        .feed
        .run_detail(proto::RunDetailRequest { run_id })
        .await
        .unwrap()
        .into_inner();
    let run = detail.run.expect("a run detail carries its run");
    assert_eq!(run.run_id, run_id);
    assert_eq!(run.state, "awaiting_input");
    assert_eq!(run.external_ref.as_deref(), Some("agens/AGN-64"));
    assert_eq!(detail.questions.len(), 1);
    assert!(detail.health.is_none(), "nothing has derived health yet");

    let inbox = wire
        .feed
        .inbox(proto::InboxRequest {
            repo_id: REPO.to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(inbox.repo_id, REPO);
    let waiting: Vec<_> = inbox.items.iter().map(|item| item.kind.as_str()).collect();
    assert_eq!(waiting, ["question", "approval"]);

    let mut stream = wire
        .feed
        .subscribe(proto::EventFilter {
            repo_id: Some(REPO.to_owned()),
            run_id: Some(run_id),
            classes: vec!["agent".to_owned()],
        })
        .await
        .unwrap()
        .into_inner();

    // The subscription is registered inside the blocking call, so the publish
    // waits for it rather than racing it.
    while wire.events.subscribers() == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    wire.events.publish(&EventRow {
        id: Some(7),
        run_id: Some(run_id),
        event_type: "checkpoint".to_owned(),
        class: EventClass::Agent,
        payload: r#"{"claim":"the parser handles escapes"}"#.to_owned(),
        ts: 1_700_000_400,
    });

    let event = stream.message().await.unwrap().expect("the entry crosses");
    assert_eq!(event.id, 7);
    assert_eq!(event.r#type, "checkpoint");
    assert_eq!(event.class, "agent");
    assert_eq!(event.run_id, Some(run_id));
}

#[tokio::test]
async fn a_repository_scoped_listing_leaves_out_every_other_project() {
    let mut wire = wire_for(Principal::User).await;

    let tree = wire
        .feed
        .tree(proto::TreeRequest {
            repo_id: OTHER_REPO.to_owned(),
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(tree.runs.len(), 1);
    assert_eq!(tree.repo_id, OTHER_REPO);
}

/// The point of the file.
///
/// The facade the daemon serves is the user's, but nothing about being a facade
/// grants anything: built for Praetor and served over the same socket, it is
/// refused every operation the table keeps for a person, and allowed exactly
/// the ones a manager already has.
#[tokio::test]
async fn a_praetor_facade_is_refused_what_the_table_keeps_for_the_user() {
    let mut wire = wire_for(Principal::Praetor).await;
    let fixture = &wire.fixture;

    let refused = wire
        .team
        .approve_plan(proto::ApprovePlanRequest {
            run_id: fixture.draft,
        })
        .await
        .expect_err("approving an execution freezes a scope");
    assert_eq!(code(&refused), Code::PermissionDenied);
    assert!(refused.message().contains("praetor may not approve_plan"));

    let refused = wire
        .team
        .authorize_merge(proto::AuthorizeMergeRequest {
            question_id: fixture.merge_approval,
            answer: "merge".to_owned(),
        })
        .await
        .expect_err("the user approves bytes");
    assert_eq!(code(&refused), Code::PermissionDenied);

    let refused = wire
        .team
        .cleaning(proto::CleaningRequest {
            run_id: fixture.merged,
            disposition: "reclaim".to_owned(),
            confirmed: false,
        })
        .await
        .expect_err("discarding a worktree needs a person");
    assert_eq!(code(&refused), Code::PermissionDenied);

    let refused = wire
        .team
        .takeover(proto::TakeoverRequest {
            run_id: fixture.taken_over,
        })
        .await
        .expect_err("a takeover hands a session's authority to whoever holds it");
    assert_eq!(code(&refused), Code::PermissionDenied);

    let refused = wire
        .team
        .pause_admissions(proto::PauseAdmissionsRequest { paused: true })
        .await
        .expect_err("admission is the operator's control");
    assert_eq!(code(&refused), Code::PermissionDenied);

    let refused = wire
        .team
        .stop(proto::StopRequest {
            scope: Some(proto::stop_request::Scope::Machine(true)),
        })
        .await
        .expect_err("stopping the team is the operator's control");
    assert_eq!(code(&refused), Code::PermissionDenied);

    // And what a manager does have, it still has: the facade narrowed the
    // principal, it did not disable the plane.
    let cancelled = wire
        .team
        .cancel_run(proto::CancelRunRequest {
            run_id: fixture.running,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cancelled.transition.unwrap().to, "cancelled");

    let retried = wire
        .team
        .retry(proto::RetryRequest {
            run_id: fixture.failed,
            guidance: "the parser needs the escaped case".to_owned(),
            retry_budget: 3,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(retried.transition.unwrap().to, "queued");

    let tree = wire
        .feed
        .tree(proto::TreeRequest {
            repo_id: REPO.to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(tree.repo_id, REPO);
}

/// An authorization is the user's whatever it is worded as, so the plain
/// question path cannot be used to grant one either.
#[tokio::test]
async fn praetor_answering_an_approval_is_refused_on_both_paths() {
    let mut wire = wire_for(Principal::Praetor).await;
    let question_id = wire.fixture.merge_approval;

    let refused = wire
        .team
        .answer_question(proto::AnswerQuestionRequest {
            question_id,
            answer: "merge".to_owned(),
        })
        .await
        .expect_err("an approval is granted, not answered");
    assert_eq!(code(&refused), Code::PermissionDenied);
}

/// Praetor answers detail questions, and the policy is the core's: an answer
/// outside the options the question offered is the open decision that escalates
/// to a person.
#[tokio::test]
async fn praetor_answers_within_the_options_and_no_further() {
    let mut wire = wire_for(Principal::Praetor).await;
    let question_id = wire.fixture.parked_question;

    let refused = wire
        .team
        .answer_question(proto::AnswerQuestionRequest {
            question_id,
            answer: "something else entirely".to_owned(),
        })
        .await
        .expect_err("that is a decision, not a detail");
    assert_eq!(code(&refused), Code::PermissionDenied);

    let answered = wire
        .team
        .answer_question(proto::AnswerQuestionRequest {
            question_id,
            answer: "prost".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(answered.question.unwrap().to, "answered");
}

#[tokio::test]
async fn a_transition_the_machine_has_no_path_for_is_a_failed_precondition() {
    let mut wire = wire_for(Principal::User).await;

    let refused = wire
        .team
        .approve_plan(proto::ApprovePlanRequest {
            run_id: wire.fixture.running,
        })
        .await
        .expect_err("a running run was approved once already");

    assert_eq!(code(&refused), Code::FailedPrecondition);
}

#[tokio::test]
async fn a_run_that_does_not_exist_is_not_found() {
    let mut wire = wire_for(Principal::User).await;

    let missing = wire
        .feed
        .run_detail(proto::RunDetailRequest { run_id: 9_999 })
        .await
        .expect_err("no such run");

    assert_eq!(code(&missing), Code::NotFound);
}

#[tokio::test]
async fn a_request_missing_what_scopes_it_is_refused_rather_than_widened() {
    let mut wire = wire_for(Principal::User).await;

    let unscoped = wire
        .feed
        .tree(proto::TreeRequest {
            repo_id: String::new(),
        })
        .await
        .expect_err("an unset repository is not every repository");
    assert_eq!(code(&unscoped), Code::InvalidArgument);

    let unscoped = wire
        .feed
        .inbox(proto::InboxRequest {
            repo_id: String::new(),
        })
        .await
        .expect_err("an unset repository is not every repository");
    assert_eq!(code(&unscoped), Code::InvalidArgument);

    let unscoped = wire
        .team
        .stop(proto::StopRequest { scope: None })
        .await
        .expect_err("a stop with no scope would default to the widest one");
    assert_eq!(code(&unscoped), Code::InvalidArgument);

    let unscoped = wire
        .team
        .stop(proto::StopRequest {
            scope: Some(proto::stop_request::Scope::Machine(false)),
        })
        .await
        .expect_err("a machine scope set to false names nothing");
    assert_eq!(code(&unscoped), Code::InvalidArgument);

    let unknown = wire
        .team
        .cleaning(proto::CleaningRequest {
            run_id: wire.fixture.merged,
            disposition: "throw it away".to_owned(),
            confirmed: true,
        })
        .await
        .expect_err("an unrecognized disposition lands on neither");
    assert_eq!(code(&unknown), Code::InvalidArgument);

    let unknown = wire
        .feed
        .subscribe(proto::EventFilter {
            repo_id: Some(REPO.to_owned()),
            run_id: None,
            classes: vec!["gossip".to_owned()],
        })
        .await
        .expect_err("an unknown class is neither dropped nor widened");
    assert_eq!(code(&unknown), Code::InvalidArgument);
}

#[test]
fn the_facade_refuses_an_address_it_cannot_keep_local() {
    let listener = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();

    let refused = FacadeBinding::none()
        .on_localhost(listener)
        .expect_err("the facade authenticates nobody");

    assert!(refused.to_string().contains("is not loopback"));
}

#[test]
fn serving_no_address_is_refused_rather_than_treated_as_serving_nothing() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let directory = scratch_directory("no-address");
    let (store, _) = seeded_store(&directory);

    let core = Arc::new(Mutex::new(ApiCore::new(
        StateMachines::new(store),
        Ports {
            scheduler: Arc::new(StubScheduler::default()),
            worktrees: Arc::new(StubWorktrees),
            delivery: Arc::new(StubDelivery),
            sessions: Arc::new(StubSessions),
            feed: Arc::new(StubFeed::default()),
        },
    )));
    let blocking = BlockingBoundary::new(runtime.handle().clone());
    let shutdown = HeadlessTurnCancellation::new();
    shutdown.cancel();

    let refused = runtime.block_on(agens_server::grpc::serve_until_shutdown(
        core,
        blocking,
        FacadeBinding::none(),
        &shutdown,
    ));

    assert!(refused.is_err());
}

/// The daemon's own path, end to end: it takes the machine's slot, serves the
/// facade on the socket it owns and on loopback, and both answer.
#[test]
fn the_daemon_serves_the_facade_on_its_socket_and_on_loopback() {
    let directory = scratch_directory("daemon");
    let (store, _) = seeded_store(&directory);

    let core = ApiCore::new(
        StateMachines::new(store),
        Ports {
            scheduler: Arc::new(StubScheduler::default()),
            worktrees: Arc::new(StubWorktrees),
            delivery: Arc::new(StubDelivery),
            sessions: Arc::new(StubSessions),
            feed: Arc::new(StubFeed::default()),
        },
    );

    let daemon = agens_server::Daemon::start(&directory).unwrap();
    let socket = daemon.socket_path().to_path_buf();
    let shutdown = HeadlessTurnCancellation::new();
    let stopper = shutdown.clone();

    // The daemon takes the runtime with it, so the client drives its own.
    let client_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let address = reserve_loopback_port();

    let asking = std::thread::spawn(move || {
        client_runtime.block_on(async move {
            let over_unix = await_unix(&socket).await;
            let over_loopback = await_loopback(address).await;

            let by_socket = FeedClient::new(over_unix)
                .tree(proto::TreeRequest {
                    repo_id: REPO.to_owned(),
                })
                .await
                .unwrap()
                .into_inner();

            let by_loopback = FeedClient::new(over_loopback)
                .tree(proto::TreeRequest {
                    repo_id: REPO.to_owned(),
                })
                .await
                .unwrap()
                .into_inner();

            stopper.cancel();

            (by_socket.runs.len(), by_loopback.runs.len())
        })
    });

    let report = daemon
        .serve_until_shutdown(Arc::new(Mutex::new(core)), Some(address.port()), &shutdown)
        .unwrap();

    assert!(report.is_clean());
    assert_eq!(asking.join().unwrap(), (7, 7));
}

/// A port the operating system just handed out and nothing is listening on, so
/// the daemon binds a free one rather than a guessed one.
fn reserve_loopback_port() -> SocketAddr {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);

    address
}

/// The daemon binds after this thread starts asking, so connecting retries
/// until it is up rather than betting on a sleep.
async fn await_unix(socket: &Path) -> tonic::transport::Channel {
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return connect_unix(socket.to_path_buf()).await;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("the daemon never accepted on its socket");
}

async fn await_loopback(address: SocketAddr) -> tonic::transport::Channel {
    for _ in 0..200 {
        if let Ok(channel) = Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
        {
            return channel;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("the daemon never accepted on loopback");
}
