//! The composed daemon, driven the way a client drives it.
//!
//! One run, one path: a proposed execution is approved over the gRPC facade,
//! the scheduler admits it, a peer session executes it, the journal reaches a
//! subscriber over the wire, the worker's `ask` parks the run on
//! `awaiting_input`, and `AnswerQuestion` resumes it — after which the run is
//! admitted a second time and the answer is waiting in the run's own mailbox.
//!
//! Nothing here reaches into the daemon. Every control-plane move goes through
//! a connected client, and every assertion about what happened is read back
//! through the Feed plane, because a composition root that only works when its
//! own test calls its internals is not composed.
//!
//! The one piece supplied by the test is the worker factory, which is the seam
//! the composition root leaves open: what a run's session is made of belongs to
//! the surface that knows about models, and nothing fills it in yet.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agens_core::HeadlessTurnCancellation;
use agens_core::run_introspection::{Ask, AskOption, RunIntrospectionPort};
use agens_server::grpc::proto::{self, feed_client::FeedClient, team_client::TeamClient};
use agens_server::{
    CoordinatorSettings, LaunchError, RunIntrospection, RunLaunch, RunSession, RunWorkerFactory,
    SessionAdmission, SessionBudget, SessionId, SessionOutcome, SessionProvider, SessionRuntime,
};
use agens_store::{
    ControlPlaneStore, DirectiveGrain, DirectiveStore, DirectiveTarget, RunRow, RunState,
    WorktreeStatus,
};
use tonic::transport::{Endpoint, Uri};

use common::{REPO, REPO_ROOT, now, run_in, scratch_directory, worktree_in};

const ANSWER: &str = "keep";

/// How long an assertion waits for a loop that ticks on a heartbeat.
const PATIENCE: Duration = Duration::from_secs(20);

/// A proposed run, which is where a client's approval finds one.
///
/// It carries the worktree `CreateRun` provisions, because admission reads that
/// column rather than assuming it: a run whose directory is not `active` is one
/// no session can be started in, and this test writes the row directly instead
/// of going through the call that would have provisioned it.
fn proposed_run(worktree: &Path) -> RunRow {
    RunRow {
        external_ref: Some("agens/AGN-180".to_owned()),
        task: "wire the daemon to the core".to_owned(),
        scope: "crates/agens-server".to_owned(),
        dod: "serve runs the composed daemon".to_owned(),
        ..run_in(RunState::Draft, worktree)
    }
}

/// The client one session speaks through. The daemon guarantees that no two
/// sessions share one, and this test's worker needs nothing more of it.
struct ScriptedClient;

impl SessionProvider for ScriptedClient {
    fn model(&self) -> &str {
        "scripted/none"
    }
}

/// What the worker did, read back by the test.
#[derive(Default)]
struct Script {
    /// Everything the resumed session found waiting in the run's mailbox.
    delivered: Mutex<Vec<String>>,
}

/// Opens the session row a launch runs as.
///
/// A session is durable before it executes anything: the attempt the admission
/// transition writes carries the session that ran it, and the column is a
/// foreign key. The real worker opens this row through `agens-session`; the
/// test writes it directly, which is the same thing the ingest suite does for
/// the rows only a live session creates.
fn open_session_row(data_directory: &Path, run_id: i64) -> Result<SessionId, LaunchError> {
    let connection = rusqlite::Connection::open(data_directory.join("agens.db"))
        .map_err(|error| LaunchError(error.to_string()))?;

    connection
        .execute(
            "INSERT INTO sessions (project, title, active_agent, created_at, updated_at)
             VALUES (?1, ?2, 'primary', ?3, ?3)",
            rusqlite::params![REPO_ROOT, format!("run {run_id}"), now()],
        )
        .map_err(|error| LaunchError(error.to_string()))?;

    Ok(SessionId::new(connection.last_insert_rowid()))
}

/// A worker that asks a question on its first attempt and reads its mailbox on
/// the second.
///
/// It is the whole of what the composition root leaves to its caller, and it
/// uses only the surfaces a real worker uses: the run's introspection port for
/// `ask`, and the run's own mailbox for what was delivered to it.
fn scripted_worker(script: Arc<Script>) -> RunWorkerFactory {
    Arc::new(move |launch: &RunLaunch<'_>| {
        let session = open_session_row(&launch.data_directory, launch.run_id)?;
        let core = Arc::clone(&launch.core);
        let run_id = launch.run_id;
        let resumed = launch.resumed;
        let mailbox = launch.mailbox.clone();
        let data_directory = launch.data_directory.clone();
        let script = Arc::clone(&script);

        let work = Box::new(move |_runtime: SessionRuntime| {
            // The launch happens before the admission transition, so the run is
            // still queued for as long as the tick that started this session
            // holds the core. A worker with anything to report waits for the
            // row it is executing to say it is executing.
            if !await_state(&core, run_id, RunState::Running) {
                return SessionOutcome::Failed;
            }

            if resumed {
                let delivered = drain_mailbox(&data_directory, &mailbox);
                script
                    .delivered
                    .lock()
                    .expect("the script is readable")
                    .extend(delivered);

                return SessionOutcome::Completed;
            }

            let mut introspection = RunIntrospection::new(Arc::clone(&core), run_id, Arc::new(now))
                .for_attempt(Some(session.value()), None);

            let asked = introspection.ask(
                &Ask::new(
                    "keep the options as JSON or split them into a table".to_owned(),
                    vec![
                        AskOption::new(
                            ANSWER,
                            "keep the JSON array",
                            Some("no migration".to_owned()),
                        ),
                        AskOption::new("split", "split it into its own table", None),
                    ],
                    Some(ANSWER.to_owned()),
                )
                .expect("the question is valid"),
            );

            match asked {
                Ok(_) => SessionOutcome::Completed,
                Err(_) => SessionOutcome::Failed,
            }
        });

        Ok(RunSession {
            admission: SessionAdmission::new(
                session,
                Box::new(ScriptedClient),
                SessionBudget::unlimited(),
            ),
            work,
            session_attempt_id: None,
        })
    }) as RunWorkerFactory
}

/// Waits for the run to reach one state, reading it through the core the way
/// the worker's own writes go through it.
fn await_state(core: &Arc<Mutex<agens_server::ApiCore>>, run_id: i64, wanted: RunState) -> bool {
    let deadline = Instant::now() + PATIENCE;

    while Instant::now() < deadline {
        let state = core.lock().ok().and_then(|core| {
            core.machines()
                .store()
                .load_run(run_id)
                .ok()
                .flatten()
                .map(|run| run.state)
        });

        if state == Some(wanted) {
            return true;
        }

        std::thread::sleep(Duration::from_millis(20));
    }

    false
}

/// Everything queued for this run at a tool-call edge, which is where an answer
/// to a question lands.
fn drain_mailbox(data_directory: &Path, mailbox: &str) -> Vec<String> {
    let Ok(mut store) = DirectiveStore::open(data_directory) else {
        return Vec::new();
    };

    store
        .drain(
            &DirectiveTarget::Child(mailbox.to_owned()),
            DirectiveGrain::ToolCall,
        )
        .map(|drained| drained.into_iter().map(|input| input.text).collect())
        .unwrap_or_default()
}

async fn connect(socket: PathBuf) -> tonic::transport::Channel {
    for _ in 0..400 {
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            let path = socket.clone();

            return Endpoint::try_from("http://localhost")
                .unwrap()
                .connect_with_connector(tower::service_fn(move |_: Uri| {
                    let path = path.clone();

                    async move {
                        let stream = tokio::net::UnixStream::connect(path).await?;

                        Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                    }
                }))
                .await
                .unwrap();
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("the daemon never accepted on its socket");
}

/// Cancels the daemon however the thread holding it ends.
struct Stopper(HeadlessTurnCancellation);

impl Drop for Stopper {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// One run's journal, for an assertion that has to say what happened instead.
async fn journal_of(
    client: &mut FeedClient<tonic::transport::Channel>,
    run_id: i64,
) -> Vec<String> {
    client
        .run_detail(proto::RunDetailRequest { run_id })
        .await
        .map(|view| {
            view.into_inner()
                .events
                .into_iter()
                .map(|event| format!("{}: {}", event.r#type, event.payload))
                .collect()
        })
        .unwrap_or_default()
}

/// One run's state as the Feed plane reports it.
async fn run_state(client: &mut FeedClient<tonic::transport::Channel>, run_id: i64) -> String {
    client
        .run_detail(proto::RunDetailRequest { run_id })
        .await
        .expect("the run is readable")
        .into_inner()
        .run
        .expect("a run view carries its run")
        .state
}

/// Waits for the Feed plane to report one state, which is the only place this
/// test looks for what the daemon did.
async fn await_reported_state(
    client: &mut FeedClient<tonic::transport::Channel>,
    run_id: i64,
    wanted: &str,
) -> String {
    let deadline = Instant::now() + PATIENCE;

    loop {
        let state = run_state(client, run_id).await;

        if state == wanted || Instant::now() >= deadline {
            return state;
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[test]
fn the_daemon_runs_a_run_from_approval_to_a_question_and_back() {
    let directory = scratch_directory("daemon", "lifecycle");
    let worktree = worktree_in(&directory, "agn-180");
    fs::create_dir_all(&worktree).expect("provision the run's worktree");

    let run_id = {
        let mut store = ControlPlaneStore::open(&directory).expect("open the control plane");

        store
            .insert_run(&proposed_run(&worktree))
            .expect("insert the run")
    };

    let script = Arc::new(Script::default());
    let shutdown = HeadlessTurnCancellation::new();
    let socket = agens_server::socket_path(&directory);

    // The daemon is stopped however the client thread ends: a client that
    // panicked would otherwise leave it serving and the test hanging on a join
    // that never comes.
    let stopper = Stopper(shutdown.clone());
    let client_script = Arc::clone(&script);

    // The daemon takes its own runtime with it, so the client drives another.
    let asking = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let stopper = stopper;

        runtime.block_on(async move {
            let channel = connect(socket).await;
            let mut team = TeamClient::new(channel.clone());
            let mut feed = FeedClient::new(channel);

            // Subscribed before anything moves, so what crosses the stream is
            // the run's own history rather than a replay of it.
            let mut events = feed
                .subscribe(proto::EventFilter {
                    repo_id: Some(REPO.to_owned()),
                    run_id: Some(run_id),
                    classes: Vec::new(),
                })
                .await
                .expect("the feed accepts a subscriber")
                .into_inner();

            team.approve_plan(proto::ApprovePlanRequest { run_id })
                .await
                .expect("the user may approve a proposed run");

            let parked = await_reported_state(&mut feed, run_id, "awaiting_input").await;

            let inbox = feed
                .inbox(proto::InboxRequest {
                    repo_id: REPO.to_owned(),
                })
                .await
                .expect("the inbox is readable")
                .into_inner();

            let question_id = inbox.items.first().map(|item| item.question_id);

            if let Some(question_id) = question_id {
                team.answer_question(proto::AnswerQuestionRequest {
                    question_id,
                    answer: ANSWER.to_owned(),
                })
                .await
                .expect("the user may answer a question");
            }

            let resumed = await_reported_state(&mut feed, run_id, "running").await;

            // The second session drains what was delivered to the run, so the
            // answer arriving is asserted on the worker's side rather than on
            // the queue's.
            let deadline = Instant::now() + PATIENCE;
            while client_script
                .delivered
                .lock()
                .expect("the script is readable")
                .is_empty()
                && Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }

            let streamed = collect_streamed(&mut events).await;
            let journal = journal_of(&mut feed, run_id).await;

            drop(stopper);

            (parked, resumed, streamed, journal)
        })
    });

    let report = agens_server::serve_until_shutdown(
        &directory,
        &CoordinatorSettings {
            heartbeat: Duration::from_millis(25),
            ..CoordinatorSettings::default()
        },
        scripted_worker(Arc::clone(&script)),
        common::refusing_chat(),
        common::refusing_chat_history(),
        &shutdown,
    )
    .expect("the daemon serves");

    let (parked, resumed, streamed, journal) = asking.join().expect("the client thread finishes");

    assert!(report.is_clean(), "every session ended: {report:?}");
    assert_eq!(
        parked, "awaiting_input",
        "the worker's ask parks the run on a person, journal: {journal:?}"
    );
    assert_eq!(
        resumed, "running",
        "the answer requeues the run and the scheduler admits it again, journal: {journal:?}"
    );
    assert_eq!(
        script
            .delivered
            .lock()
            .expect("the script is readable")
            .as_slice(),
        [ANSWER.to_owned()],
        "the resumed session finds the answer in the run's mailbox"
    );
    // The generic move is what a subscriber follows without knowing every
    // domain event by name, and the domain events are the run's own history:
    // approved, started, parked, answered, resumed, started again.
    assert!(
        streamed.iter().any(|event| event == "run_state_changed"),
        "the journal reaches a subscriber over the wire: {streamed:?}"
    );
    assert_eq!(
        streamed
            .iter()
            .filter(|event| *event != "run_state_changed")
            .cloned()
            .collect::<Vec<_>>(),
        [
            "run_approved",
            "run_started",
            "run_awaiting_input",
            "question_answered",
            "run_resumed",
            "run_started",
        ],
        "the subscriber sees the whole of what the daemon did"
    );

    fs::remove_dir_all(directory).unwrap();
}

/// Everything already on the stream, without waiting for one more.
///
/// The daemon is about to be stopped, so a read that waits on the next entry
/// would wait for a publisher that is shutting down.
async fn collect_streamed(events: &mut tonic::Streaming<proto::Event>) -> Vec<String> {
    let mut seen = Vec::new();

    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(200), events.message())
        .await
        .unwrap_or(Ok(None))
    {
        seen.push(event.r#type);
    }

    seen
}

/// Work the last process left behind is already back in the queue by the time
/// a client can ask about it.
///
/// The reconciliation is finished before the facade answers anything, so the
/// first thing a client can read is a reconciled control plane rather than a
/// row describing a session that no longer exists. Nothing about it reaches a
/// subscriber: a subscription is live from the moment it is registered, and no
/// client is attached while a daemon is still booting, which is why the run's
/// own detail is where a watcher finds it.
#[test]
fn the_first_answer_a_client_gets_is_from_a_reconciled_control_plane() {
    let directory = scratch_directory("daemon", "reconciled");
    let worktree = directory.join("worktrees").join(REPO).join("agn-192");
    fs::create_dir_all(&worktree).expect("provision the run's worktree");

    let run_id = {
        let mut store = ControlPlaneStore::open(&directory).expect("open the control plane");
        let run_id = store
            .insert_run(&proposed_run(&worktree))
            .expect("insert the run");

        // What a killed daemon leaves: a row that says a session is executing
        // this run, and no session anywhere executing it.
        rusqlite::Connection::open(store.database_path())
            .expect("open the control plane directly")
            .execute("UPDATE runs SET state = 'running' WHERE id = ?1", [run_id])
            .expect("leave the run where a killed daemon leaves it");

        run_id
    };

    let shutdown = HeadlessTurnCancellation::new();
    let socket = agens_server::socket_path(&directory);
    let stopper = Stopper(shutdown.clone());

    let asking = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let stopper = stopper;

        runtime.block_on(async move {
            let mut feed = FeedClient::new(connect(socket).await);
            let first = run_state(&mut feed, run_id).await;

            drop(stopper);

            first
        })
    });

    let report = agens_server::serve_until_shutdown(
        &directory,
        &CoordinatorSettings {
            heartbeat: Duration::from_millis(25),
            ..CoordinatorSettings::default()
        },
        // Refusing every launch keeps the resumed run where reconciliation put
        // it, so what the client reads is the boot pass rather than a session.
        std::sync::Arc::new(|_launch: &RunLaunch<'_>| {
            Err(LaunchError("this test starts no sessions".to_owned()))
        }) as RunWorkerFactory,
        common::refusing_chat(),
        common::refusing_chat_history(),
        &shutdown,
    )
    .expect("the daemon serves");

    let first = asking.join().expect("the client thread finishes");

    assert!(report.is_clean(), "every session ended: {report:?}");
    assert_eq!(
        first, "queued",
        "a run the last process left running is interrupted and requeued before \
         the facade answers for it"
    );

    fs::remove_dir_all(directory).unwrap();
}

/// A daemon that cannot compose says what stopped it.
///
/// The whole of the operator's evidence is this one line: the refusal happens
/// before the journal, the facade and the diagnostics file exist, so a fixed
/// phrase would leave them with a daemon that will not start and nothing at all
/// about why.
#[test]
fn a_coordinator_that_cannot_open_its_store_carries_the_cause_out() {
    let directory = scratch_directory("daemon", "unopenable");

    // A directory where the control plane's file belongs: the store cannot open
    // it, and the failure is the store's own rather than one this test wrote.
    fs::create_dir_all(directory.join("agens.db")).unwrap();

    let shutdown = HeadlessTurnCancellation::new();
    let refused = agens_server::serve_until_shutdown(
        &directory,
        &CoordinatorSettings::default(),
        std::sync::Arc::new(|_launch: &RunLaunch<'_>| {
            Err(LaunchError("this test starts no sessions".to_owned()))
        }) as RunWorkerFactory,
        common::refusing_chat(),
        common::refusing_chat_history(),
        &shutdown,
    )
    .expect_err("the control plane cannot be opened");

    let cause = match &refused {
        agens_server::ServerError::Unavailable(cause) => cause.clone(),
        other => panic!("{other:?}"),
    };

    assert!(
        cause.contains("the control plane"),
        "the component that failed travels: {cause}"
    );
    assert_eq!(cause, refused.to_string());

    fs::remove_dir_all(directory).unwrap();
}

/// The other end of a run's life, over the same composed daemon: a finished run
/// whose branch landed has its directory reclaimed and disposed of, with nobody
/// calling a gate by hand.
///
/// Nothing here reaches into the coordinator either. The repository is real, the
/// run is written the way `CreateRun` would leave it, and what the sweep did is
/// read back through the Feed plane and off the filesystem.
#[test]
fn the_daemon_reclaims_the_worktree_of_a_finished_run_whose_branch_landed() {
    let directory = scratch_directory("daemon", "reclaim");
    let checkout = directory.join("repository");
    fs::create_dir_all(&checkout).unwrap();

    git(&checkout, &["init", "--quiet", "--initial-branch=main"]);
    git(&checkout, &["config", "user.name", "Agens Test"]);
    git(&checkout, &["config", "user.email", "agens-test@localhost"]);
    fs::write(checkout.join("tracked.txt"), "initial\n").unwrap();
    git(&checkout, &["add", "."]);
    git(&checkout, &["commit", "--quiet", "-m", "initial"]);

    let worktree = agens_tools::SessionWorktrees::new(&directory)
        .create(&checkout, REPO, "agn-191", "feature/agn-191", "main")
        .expect("provision the session worktree");

    fs::write(worktree.join("feature.txt"), "work\n").unwrap();
    git(&worktree, &["add", "."]);
    git(&worktree, &["commit", "--quiet", "-m", "feature"]);
    git(&checkout, &["merge", "--quiet", "feature/agn-191"]);

    let run_id = {
        let mut store = ControlPlaneStore::open(&directory).expect("open the control plane");

        store
            .insert_run(&RunRow {
                repo_root: checkout.display().to_string(),
                state: RunState::Done,
                ..proposed_run(&worktree)
            })
            .expect("insert the run")
    };

    let shutdown = HeadlessTurnCancellation::new();
    let socket = agens_server::socket_path(&directory);
    let stopper = Stopper(shutdown.clone());

    let watching = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let stopper = stopper;

        runtime.block_on(async move {
            let channel = connect(socket).await;
            let mut feed = FeedClient::new(channel);

            let deadline = Instant::now() + PATIENCE;
            let mut journal = Vec::new();

            while Instant::now() < deadline {
                journal = journal_of(&mut feed, run_id).await;

                if journal
                    .iter()
                    .any(|entry| entry.starts_with("worktree_cleaned"))
                {
                    break;
                }

                tokio::time::sleep(Duration::from_millis(25)).await;
            }

            drop(stopper);

            journal
        })
    });

    let report = agens_server::serve_until_shutdown(
        &directory,
        &CoordinatorSettings {
            heartbeat: Duration::from_millis(25),
            gates_sweep: Duration::from_millis(50),
            ..CoordinatorSettings::default()
        },
        std::sync::Arc::new(|_launch: &RunLaunch<'_>| {
            Err(LaunchError("this test starts no sessions".to_owned()))
        }) as RunWorkerFactory,
        common::refusing_chat(),
        common::refusing_chat_history(),
        &shutdown,
    )
    .expect("the daemon serves");

    let journal = watching.join().expect("the client thread finishes");

    assert!(report.is_clean(), "every session ended: {report:?}");

    let store = ControlPlaneStore::open(&directory).expect("reopen the control plane");
    let status = store
        .load_run(run_id)
        .expect("load the run")
        .expect("the run exists")
        .worktree_status;

    assert_eq!(
        status,
        Some(WorktreeStatus::Cleaned),
        "the sweep releases and then disposes; a row that stopped at reclaimable \
         would hold its place in the worktree ceiling for good, journal: {journal:?}"
    );
    assert!(
        journal
            .iter()
            .any(|entry| entry.starts_with("worktree_reclaimable")),
        "the release is announced before the disposal: {journal:?}"
    );
    assert!(
        !worktree.is_dir(),
        "the directory is gone, not only the row"
    );

    fs::remove_dir_all(directory).unwrap();
}

fn git(directory: &Path, arguments: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("git runs");

    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
