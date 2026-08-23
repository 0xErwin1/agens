//! The team-mode journey, end to end, against a real daemon and a real
//! provider.
//!
//! Every other test of the coordinator supplies its own worker factory and a
//! scripted provider, so what they prove is that the control plane moves when
//! something moves it. This one proves the composition: `agens serve` as the
//! operator starts it, the worker factory the CLI actually installs, a real
//! model, and a real repository. The path it walks is the whole one — create,
//! approve, admit, checkpoint, ask, answer, done, approval, merge, reclaim —
//! and every transition is read back through the Feed plane rather than out of
//! the daemon's own memory.
//!
//! It is `#[ignore]` and gated on an environment variable naming a credentials
//! file, because it spends real provider budget. Both guards are deliberate:
//! `--ignored` alone is one keystroke away from a suite run, and a test that
//! bills an account should take more than that.
//!
//! Nothing here reaches the operator's own configuration. The daemon runs with
//! its own `HOME`, `XDG_*` and `AGENS_CONFIG_HOME`, so its data directory, its
//! worktrees, its database and its diagnostics are all inside the temporary
//! root this test removes on the way out. The one thing that crosses over is
//! the credentials file, and only the entry for the provider under test.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agens_server::grpc::proto::{self, feed_client::FeedClient, team_client::TeamClient};
use tonic::transport::{Channel, Endpoint, Uri};

/// The credentials file the daemon authenticates with. Absent, the test does
/// nothing: it is the switch that says a real provider may be billed.
const CREDENTIALS_VARIABLE: &str = "AGENS_TEAM_JOURNEY_CREDENTIALS";

/// The qualified model the worker runs as, when the operator wants another.
const MODEL_VARIABLE: &str = "AGENS_TEAM_JOURNEY_MODEL";

/// The checkout the run's repository is cloned from, when it is not this one.
const REPOSITORY_VARIABLE: &str = "AGENS_TEAM_JOURNEY_REPOSITORY";

const DEFAULT_MODEL: &str = "openai-chatgpt/gpt-5.6-sol";

/// The provider whose credentials travel into the isolated configuration.
///
/// One entry is copied rather than the file, so an operator with several
/// providers authenticated lends this run exactly the one it needs.
const PROVIDER: &str = "openai-chatgpt";

/// How long a model-driven stage is given. A turn against a real provider is
/// minutes, not milliseconds, and a run that asks twice takes three of them.
const MODEL_PATIENCE: Duration = Duration::from_secs(900);

/// How long a stage the daemon drives on its own is given. The gates sweep is
/// the slowest of them and runs every fifteen seconds.
const DAEMON_PATIENCE: Duration = Duration::from_secs(120);

/// How often a wait looks again.
const POLL: Duration = Duration::from_millis(500);

/// The work the run is given.
///
/// Small enough to finish in one turn, verifiable without judgement — the test
/// re-runs nothing, it reads the commit — and deliberately silent about the
/// name of the test function, which is what gives the worker something it
/// cannot decide alone and has to `ask` about.
const TASK: &str = "Add one unit test to the agens-models crate that pins the parse of the \
                    qualified model identifier openai-chatgpt/gpt-5.6-sol: QualifiedModel::parse \
                    must resolve it to the openai-chatgpt provider and to the gpt-5.6-sol model \
                    identifier. Put the test in the existing test module of \
                    crates/agens-models/src/lib.rs, next to the tests already covering \
                    QualifiedModel::parse. Commit the change on this worktree's current branch.";

const SCOPE: &str = "crates/agens-models/src/lib.rs";

const DEFINITION_OF_DONE: &str = concat!(
    "cargo test -p agens-models passes in this worktree, the new test is committed on the ",
    "run's branch with a Conventional Commits message, and git status reports a clean ",
    "working tree. The name of the new test function is not fixed here and is not yours to ",
    "pick alone: decide it with the operator before you write the test."
);

#[test]
#[ignore = "bills a real provider; needs AGENS_TEAM_JOURNEY_CREDENTIALS"]
fn a_run_reaches_a_merge_through_the_whole_journey() {
    let Some(credentials) = std::env::var_os(CREDENTIALS_VARIABLE).map(PathBuf::from) else {
        eprintln!("{CREDENTIALS_VARIABLE} is unset: the journey needs real credentials");
        return;
    };

    let probe = Probe::prepare(&credentials);
    let mut daemon = probe.start_daemon();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the client");

    let outcome = runtime.block_on(probe.walk_the_journey());

    daemon.stop();

    outcome.assert_merged(&probe);
}

/// Everything the journey needs on disk, and where it is.
struct Probe {
    root: PathBuf,
    data_directory: PathBuf,
    configuration_home: PathBuf,
    home: PathBuf,
    xdg_config_home: PathBuf,
    xdg_data_home: PathBuf,
    clone: PathBuf,
}

impl Probe {
    /// Builds the isolated roots, the clone, and the configuration the daemon
    /// reads, in that order: the configuration names the clone, so the clone
    /// has to exist before it is written.
    fn prepare(credentials: &Path) -> Self {
        let root = std::env::temp_dir().join(format!("agens-team-journey-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let home = root.join("home");
        let xdg_config_home = root.join("xdg-config");
        let xdg_data_home = root.join("xdg-data");
        let configuration_home = root.join("agens-config");

        for directory in [&home, &xdg_config_home, &xdg_data_home, &configuration_home] {
            fs::create_dir_all(directory).expect("an isolated root");
        }

        let clone = root.join("repository");
        clone_repository(&repository_under_test(), &clone);

        write_git_identity(&home);
        copy_provider_credentials(credentials, &configuration_home.join("auth.json"));
        write_configuration(&configuration_home.join("config.toml"), &clone);

        Self {
            data_directory: xdg_data_home.join("agens"),
            root,
            configuration_home,
            home,
            xdg_config_home,
            xdg_data_home,
            clone,
        }
    }

    /// The binary with the four roots replaced and everything else inherited:
    /// the worker builds with the repository's own toolchain, which is on the
    /// caller's `PATH` and nowhere else.
    fn agens(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agens"));
        command
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.xdg_config_home)
            .env("XDG_DATA_HOME", &self.xdg_data_home)
            .env("AGENS_CONFIG_HOME", &self.configuration_home)
            .stdin(Stdio::null());

        command
    }

    /// Starts `agens serve` the way an operator does: bare, detached, and
    /// returning once the daemon is answering. Its own log lives under the data
    /// directory now, which is where `serve` puts it.
    fn start_daemon(&self) -> Daemon {
        let started = self
            .agens()
            .arg("serve")
            .output()
            .expect("the daemon starts");

        assert!(
            started.status.success(),
            "the daemon did not start: {}",
            String::from_utf8_lossy(&started.stderr)
        );

        let mut stopper = self.agens();
        stopper.args(["serve", "stop"]);

        Daemon { stopper }
    }

    async fn walk_the_journey(&self) -> Journey {
        let channel = connect(agens_server::socket_path(&self.data_directory)).await;
        let mut team = TeamClient::new(channel.clone());
        let mut feed = FeedClient::new(channel);

        let created = team
            .create_run(proto::CreateRunRequest {
                repo_root: self.clone.display().to_string(),
                task: TASK.to_owned(),
                scope: SCOPE.to_owned(),
                dod: DEFINITION_OF_DONE.to_owned(),
                provider: PROVIDER.to_owned(),
                ..proto::CreateRunRequest::default()
            })
            .await
            .expect("the run is created")
            .into_inner();

        let run_id = created.run_id;

        assert_eq!(
            state_of(&mut feed, run_id).await,
            "draft",
            "a created run waits for its plan to be approved"
        );

        let approval = team
            .approve_plan(proto::ApprovePlanRequest { run_id })
            .await
            .expect("the plan is approved")
            .into_inner();

        let transition = approval.transition.expect("the approval moves the run");
        assert_eq!(
            (transition.from.as_str(), transition.to.as_str()),
            ("draft", "queued")
        );

        let answered = answer_until_finished(&mut team, &mut feed, run_id).await;

        assert_eq!(
            state_of(&mut feed, run_id).await,
            "done",
            "the run finished on its own"
        );
        assert!(
            answered > 0,
            "the definition of done withheld the test's name, so the worker had to ask"
        );

        let merge = team
            .authorize_merge(proto::AuthorizeMergeRequest {
                subject: Some(proto::authorize_merge_request::Subject::RunId(run_id)),
                answer: "authorized by the journey test".to_owned(),
                expires_at: None,
            })
            .await
            .expect("the merge is authorized")
            .into_inner();

        wait_for(
            &mut feed,
            run_id,
            DAEMON_PATIENCE,
            "the worktree is reclaimed",
            |view| worktree_status_of(view) == "cleaned",
        )
        .await;

        Journey {
            run_id,
            approval_id: merge.question_id,
            questions_answered: answered,
            journal: journal_of(&mut feed, run_id).await,
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// What the journey produced, as the assertions read it.
struct Journey {
    run_id: i64,
    approval_id: i64,
    questions_answered: usize,
    journal: Vec<String>,
}

impl Journey {
    /// Asserts the journey against the two records that outlive it: the
    /// coordinator's journal, and the repository's own history.
    fn assert_merged(&self, probe: &Probe) {
        for event in [
            "run_created",
            "run_approved",
            "run_started",
            "checkpoint",
            "run_awaiting_input",
            "run_resumed",
            "run_finished",
            "approval_requested",
            "approval_granted",
            "merged",
            "worktree_reclaimable",
            "worktree_cleaned",
        ] {
            assert!(
                self.journal.iter().any(|entry| entry == event),
                "run {} never journaled {event}; it journaled {:?}",
                self.run_id,
                self.journal
            );
        }

        assert!(self.approval_id > 0, "the merge opened an approval");
        assert!(self.questions_answered > 0, "the run parked on a question");

        let merged = git(&probe.clone, &["log", "--oneline", "-1", "main"]);
        assert!(
            merged.contains("Merge branch 'agens/"),
            "main does not carry the run's merge: {merged}"
        );

        let touched = git(
            &probe.clone,
            &["show", "--name-only", "--format=", "HEAD^2"],
        );
        assert_eq!(
            touched.trim(),
            SCOPE,
            "the merged commit left the declared scope"
        );
    }
}

/// The daemon process, stopped by the test rather than by the harness.
///
/// A daemon left running holds its data directory's lock, and the next run of
/// this test would be refused by a process nobody remembers starting. It is
/// stopped through `serve stop` rather than killed: the daemon this test starts
/// is not its child any more, and a signal is what it answers to either way.
struct Daemon {
    stopper: Command,
}

impl Daemon {
    fn stop(&mut self) {
        let _ = self.stopper.output();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Answers every question the run parks on until it stops parking, and reports
/// how many it answered.
///
/// The answer is the worker's own recommendation when it made one, because
/// what this test proves is that the loop closes, not that a particular option
/// is the right one.
async fn answer_until_finished(
    team: &mut TeamClient<Channel>,
    feed: &mut FeedClient<Channel>,
    run_id: i64,
) -> usize {
    let mut answered = 0;

    loop {
        let view = wait_for(
            feed,
            run_id,
            MODEL_PATIENCE,
            "the run parks or finishes",
            |view| {
                matches!(
                    state_of_view(view).as_str(),
                    "awaiting_input" | "done" | "failed"
                )
            },
        )
        .await;

        match state_of_view(&view).as_str() {
            "done" => return answered,
            "failed" => panic!("the run failed: {:?}", view.run),
            _ => {}
        }

        let question = view
            .questions
            .iter()
            .find(|question| question.state == "open")
            .expect("a parked run has an open question");

        team.answer_question(proto::AnswerQuestionRequest {
            question_id: question.question_id,
            answer: chosen_answer(question),
        })
        .await
        .expect("the question is answered");

        answered += 1;
    }
}

/// The option an answer picks: the worker's recommendation, or the first one
/// it offered when it recommended none.
fn chosen_answer(question: &proto::Question) -> String {
    if let Some(recommendation) = question
        .recommendation
        .as_deref()
        .filter(|recommendation| !recommendation.is_empty())
    {
        return recommendation.to_owned();
    }

    serde_json::from_str::<Vec<serde_json::Value>>(&question.options)
        .ok()
        .and_then(|options| {
            options
                .first()
                .and_then(|option| option.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "proceed".to_owned())
}

/// Polls the read plane until the run satisfies `settled`, and returns the view
/// that did.
async fn wait_for(
    feed: &mut FeedClient<Channel>,
    run_id: i64,
    patience: Duration,
    expectation: &str,
    settled: impl Fn(&proto::RunView) -> bool,
) -> proto::RunView {
    let deadline = Instant::now() + patience;

    loop {
        let view = run_detail(feed, run_id).await;
        if settled(&view) {
            return view;
        }

        assert!(
            Instant::now() < deadline,
            "waited {patience:?} for {expectation}; run {run_id} is {} with worktree {}",
            state_of_view(&view),
            worktree_status_of(&view)
        );

        tokio::time::sleep(POLL).await;
    }
}

async fn run_detail(feed: &mut FeedClient<Channel>, run_id: i64) -> proto::RunView {
    feed.run_detail(proto::RunDetailRequest { run_id })
        .await
        .expect("the run is readable")
        .into_inner()
}

async fn state_of(feed: &mut FeedClient<Channel>, run_id: i64) -> String {
    state_of_view(&run_detail(feed, run_id).await)
}

fn state_of_view(view: &proto::RunView) -> String {
    view.run
        .as_ref()
        .map(|run| run.state.clone())
        .unwrap_or_default()
}

fn worktree_status_of(view: &proto::RunView) -> String {
    view.run
        .as_ref()
        .and_then(|run| run.worktree_status.clone())
        .unwrap_or_default()
}

async fn journal_of(feed: &mut FeedClient<Channel>, run_id: i64) -> Vec<String> {
    run_detail(feed, run_id)
        .await
        .events
        .into_iter()
        .map(|event| event.r#type)
        .collect()
}

/// Dials the daemon's unix socket, waiting for it to appear.
async fn connect(socket: PathBuf) -> Channel {
    let deadline = Instant::now() + DAEMON_PATIENCE;

    loop {
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            let path = socket.clone();

            return Endpoint::try_from("http://localhost")
                .expect("a placeholder authority")
                .connect_with_connector(tower::service_fn(move |_: Uri| {
                    let path = path.clone();

                    async move {
                        let stream = tokio::net::UnixStream::connect(path).await?;

                        Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                    }
                }))
                .await
                .expect("the facade accepts");
        }

        assert!(
            Instant::now() < deadline,
            "the daemon never accepted on {}",
            socket.display()
        );

        tokio::time::sleep(POLL).await;
    }
}

/// The checkout the run's repository is cloned from.
///
/// This workspace by default, so the journey has a crate whose tests it can be
/// asked for and a history it can measure a merge against.
fn repository_under_test() -> PathBuf {
    std::env::var_os(REPOSITORY_VARIABLE).map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(Path::parent)
                .expect("the workspace root")
                .to_path_buf()
        },
        PathBuf::from,
    )
}

fn clone_repository(source: &Path, destination: &Path) {
    let status = Command::new("git")
        .args(["clone", "--quiet"])
        .arg(source)
        .arg(destination)
        .status()
        .expect("git runs");

    assert!(status.success(), "cloning {} failed", source.display());
}

/// The identity the worker commits with.
///
/// Written before the daemon starts, because without one the run parks on a
/// question about git configuration instead of on the one this journey is
/// about.
fn write_git_identity(home: &Path) {
    fs::write(
        home.join(".gitconfig"),
        "[user]\n\tname = agens journey\n\temail = journey@localhost\n",
    )
    .expect("a git identity");
}

/// Copies one provider's entry out of the operator's credentials file.
///
/// Never the file: an operator with several providers authenticated is lending
/// this run the one it needs and nothing else.
fn copy_provider_credentials(source: &Path, destination: &Path) {
    let contents = fs::read_to_string(source).expect("the credentials file is readable");
    let credentials: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&contents).expect("the credentials file is JSON");
    let entry = credentials
        .get(PROVIDER)
        .unwrap_or_else(|| panic!("{} holds no {PROVIDER} entry", source.display()));

    let mut file = fs::File::create(destination).expect("a credentials file");
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .expect("the credentials stay private");
    file.write_all(
        serde_json::to_string(&BTreeMap::from([(PROVIDER, entry)]))
            .expect("the entry serializes")
            .as_bytes(),
    )
    .expect("the credentials are written");
}

fn write_configuration(destination: &Path, clone: &Path) {
    let model = std::env::var(MODEL_VARIABLE).unwrap_or_else(|_| DEFAULT_MODEL.to_owned());

    fs::write(
        destination,
        format!(
            "[options]\ndebug = true\n\n\
             [provider]\nmodel = \"{model}\"\n\n\
             [team]\nproject_roots = [\"{}\"]\n",
            clone.display()
        ),
    )
    .expect("a configuration file");
}

fn git(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .output()
        .expect("git runs");

    assert!(
        output.status.success(),
        "git {arguments:?} failed in {}",
        repository.display()
    );

    String::from_utf8_lossy(&output.stdout).into_owned()
}
