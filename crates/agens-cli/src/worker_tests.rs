//! One run of the composed daemon, end to end, with a real agent loop in it.
//!
//! Everything is production except the model. The daemon is the composed one,
//! the client speaks to it over its own socket, the worker is the one `agens
//! serve` supplies, and the turn it takes is a real headless turn against a
//! real streaming endpoint — the endpoint is just scripted, so what the model
//! does is decided by the test instead of by a provider.
//!
//! What it asserts is the whole path the harness exists for: a run is created
//! and its worktree provisioned, the user approves it, the scheduler admits it,
//! the worker's turn reports a checkpoint and then asks a question, the run
//! parks on a person, the answer requeues it, and the resumed session finishes
//! the run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agens_core::HeadlessTurnCancellation;
use agens_fixtures::{Script, ScriptedDialect, ScriptedProvider, ScriptedTurn};
use agens_server::grpc::proto::{self, feed_client::FeedClient, team_client::TeamClient};
use agens_server::{CoordinatorSettings, SchedulerLimits};
use tonic::transport::{Channel, Endpoint, Uri};

use crate::CliDependencies;
use crate::deps::bootstrap;
use crate::worker::run_worker;

/// How long an assertion waits for loops that tick on a heartbeat and for a
/// turn that talks to a socket.
const PATIENCE: Duration = Duration::from_secs(60);

const ANSWER: &str = "split";

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn scratch() -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory =
        std::env::temp_dir().join(format!("agens-cli-worker-{}-{suffix}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("create the scratch directory");

    directory
}

fn git(directory: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");

    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A checkout with one commit, which is all a worktree needs to be created
/// from.
fn checkout(root: &Path) -> PathBuf {
    let checkout = root.join("repository");
    std::fs::create_dir_all(&checkout).expect("create the checkout");

    git(&checkout, &["init", "--quiet", "--initial-branch=main"]);
    git(&checkout, &["config", "user.name", "Agens Test"]);
    git(&checkout, &["config", "user.email", "agens-test@localhost"]);
    std::fs::write(checkout.join("tracked.txt"), "initial\n").expect("write the tracked file");
    git(&checkout, &["add", "tracked.txt"]);
    git(&checkout, &["commit", "--quiet", "-m", "initial"]);

    checkout
}

/// The model's side of both sessions.
///
/// The first is the whole shape of a worker's turn: report the milestone, then
/// raise the one decision it cannot make, then stop — because the run parks on
/// the question and there is nothing else for the turn to do. The second is the
/// resumed session, which answers and ends, and ending is what reports the run
/// finished.
fn script() -> Script {
    Script::new([
        ScriptedTurn::tool_call(
            "call-checkpoint",
            "checkpoint",
            r#"{"next_goal":"split the options into their own table","evidence":[{"description":"cargo test -p agens-store passes","evidence_class":"deterministic","proof_refs":["cargo test -p agens-store"],"disposition":"candidate_caused"}],"touched_paths":["crates/agens-store/src/control_plane.rs"]}"#,
        ),
        ScriptedTurn::tool_call(
            "call-ask",
            "ask",
            r#"{"blocked_decision":"keep the options as JSON or split them into a table","options":[{"id":"keep","label":"keep the JSON array"},{"id":"split","label":"split it into its own table"}],"recommendation":"split"}"#,
        ),
        ScriptedTurn::text("parked on the question"),
        ScriptedTurn::text("the options now live in their own table"),
    ])
}

async fn connect(socket: PathBuf) -> Channel {
    for _ in 0..600 {
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

/// Stops the daemon however the client thread ends, so a panicking client does
/// not leave the test hanging on a join that never comes.
struct Stopper(HeadlessTurnCancellation);

impl Drop for Stopper {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn run_state(client: &mut FeedClient<Channel>, run_id: i64) -> String {
    client
        .run_detail(proto::RunDetailRequest { run_id })
        .await
        .expect("the run is readable")
        .into_inner()
        .run
        .expect("a run view carries its run")
        .state
}

async fn await_reported_state(
    client: &mut FeedClient<Channel>,
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

/// One run's journal, for an assertion that has to say what happened instead of
/// only that it did not.
async fn journal_of(client: &mut FeedClient<Channel>, run_id: i64) -> Vec<String> {
    client
        .run_detail(proto::RunDetailRequest { run_id })
        .await
        .map(|view| {
            view.into_inner()
                .events
                .into_iter()
                .map(|event| event.r#type)
                .collect()
        })
        .unwrap_or_default()
}

/// What the run recorded as evidence, which is what a checkpoint that reached
/// the control plane looks like from outside.
async fn findings_of(client: &mut FeedClient<Channel>, run_id: i64) -> Vec<String> {
    client
        .run_detail(proto::RunDetailRequest { run_id })
        .await
        .map(|view| {
            view.into_inner()
                .findings
                .into_iter()
                .map(|finding| finding.evidence_class)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_daemon_executes_a_run_through_a_real_turn_that_checkpoints_asks_and_finishes() {
    let root = scratch();
    let checkout = checkout(&root);
    let config_home = root.join("config");
    let data_directory = root.join("data");
    std::fs::create_dir_all(&config_home).expect("create the config directory");
    std::fs::create_dir_all(&data_directory).expect("create the data directory");

    let provider = ScriptedProvider::start(ScriptedDialect::Responses, script());
    let base_url = provider.base_url();

    let dependencies = CliDependencies::for_test(
        checkout.clone(),
        Some(root.join("home")),
        BTreeMap::from([
            (
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            ),
            ("OPENAI_API_KEY".to_owned(), "test-key".to_owned()),
        ]),
        BTreeMap::from([
            (
                config_home.join("config.toml"),
                format!(
                    "[provider]\nmodel = \"openai-api/gpt-4.1\"\nbase_url = \"{base_url}\"\n\n\
                     [options]\ndata_dir = \"{}\"\n",
                    data_directory.display()
                ),
            ),
            (
                config_home.join("auth.json"),
                r#"{"openai-api": {"api_key": "fixture"}}"#.to_owned(),
            ),
        ]),
    );
    let bootstrap = bootstrap(&dependencies).expect("the production bootstrap is valid");

    // The daemon serves the checkouts its operator wrote down, and nothing
    // else: a repository nobody named is a repository whose hooks it would be
    // executing on a caller's say-so.
    std::fs::write(
        data_directory.join("worktree-policy.toml"),
        format!("project_roots = [\"{}\"]\n", checkout.display()),
    )
    .expect("write the daemon's repository policy");

    let shutdown = HeadlessTurnCancellation::new();
    let socket = agens_server::socket_path(&data_directory);
    let stopper = Stopper(shutdown.clone());
    let repo_root = checkout.display().to_string();

    // The daemon takes its own runtime with it, so the client drives another.
    let client = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let stopper = stopper;

        runtime.block_on(async move {
            let channel = connect(socket).await;
            let mut team = TeamClient::new(channel.clone());
            let mut feed = FeedClient::new(channel);

            let created = team
                .create_run(proto::CreateRunRequest {
                    repo_root,
                    task: "move the question options into their own table".to_owned(),
                    scope: "crates/agens-store".to_owned(),
                    dod: "the options are a table and the tests pass".to_owned(),
                    external_ref: Some("agens/AGN-181".to_owned()),
                    parent_run_id: None,
                    dep_run_id: None,
                    provider: "openai-api".to_owned(),
                    priority: 5,
                    budget_tokens: None,
                    start_point: String::new(),
                })
                .await
                .expect("a client may propose an execution")
                .into_inner();

            team.approve_plan(proto::ApprovePlanRequest {
                run_id: created.run_id,
            })
            .await
            .expect("the user may approve a proposed run");

            let parked = await_reported_state(&mut feed, created.run_id, "awaiting_input").await;
            let findings = findings_of(&mut feed, created.run_id).await;

            let inbox = feed
                .inbox(proto::InboxRequest {
                    repo_id: created.repo_id.clone(),
                })
                .await
                .expect("the inbox is readable")
                .into_inner();
            let question = inbox.items.first().map(|item| item.question_id);

            if let Some(question_id) = question {
                team.answer_question(proto::AnswerQuestionRequest {
                    question_id,
                    answer: ANSWER.to_owned(),
                })
                .await
                .expect("the user may answer a question");
            }

            let finished = await_reported_state(&mut feed, created.run_id, "done").await;
            let journal = journal_of(&mut feed, created.run_id).await;

            drop(stopper);

            (created, parked, findings, question, finished, journal)
        })
    });

    let report = agens_server::serve_until_shutdown(
        &data_directory,
        &CoordinatorSettings {
            heartbeat: Duration::from_millis(25),
            scheduler: SchedulerLimits {
                max_concurrent: 1,
                available_worktrees: 1,
                provider_capacity: BTreeMap::new(),
                default_provider_capacity: 1,
            },
            ..CoordinatorSettings::default()
        },
        run_worker(&bootstrap),
        None,
        &shutdown,
    )
    .expect("the daemon serves");

    let (created, parked, findings, question, finished, journal) =
        client.join().expect("the client thread finishes");

    assert!(report.is_clean(), "every session ended: {report:?}");
    assert!(
        Path::new(&created.worktree_path).is_dir(),
        "creating a run provisions the worktree it works in: {created:?}"
    );
    assert_eq!(
        parked, "awaiting_input",
        "the worker's own `ask` parks the run on a person, journal: {journal:?}"
    );
    assert_eq!(
        findings,
        vec!["deterministic".to_owned()],
        "the worker's checkpoint reached the control plane as evidence, journal: {journal:?}"
    );
    assert!(
        question.is_some(),
        "the question the run parked on is in the inbox, journal: {journal:?}"
    );
    assert_eq!(
        finished, "done",
        "the answer requeues the run and the resumed session finishes it, journal: {journal:?}"
    );
    assert_eq!(
        journal
            .iter()
            .filter(|event| *event != "run_state_changed")
            .cloned()
            .collect::<Vec<_>>(),
        [
            "run_created",
            "run_approved",
            "run_started",
            "checkpoint",
            "run_awaiting_input",
            "question_answered",
            "run_resumed",
            "run_started",
            "run_finished",
        ],
        "the journal is the whole of what the run did"
    );

    provider.assert_script_consumed();

    let _ = std::fs::remove_dir_all(&root);
}
