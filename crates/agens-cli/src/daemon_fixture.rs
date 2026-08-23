//! One composed daemon with a scripted model behind it, which is what every
//! end-to-end run in this crate needs before it can assert anything.
//!
//! Standing it up is sixty lines that say nothing about the journey under test:
//! a checkout with one commit, a configuration pointing the provider at the
//! scripted endpoint, the production bootstrap over it, and the repository
//! policy the daemon serves. Three tests had their own copy, comments included.
//! What differs between them is the script the model plays and the settings the
//! coordinator runs under, so those are what a caller passes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agens_bootstrap::Bootstrap;
use agens_core::HeadlessTurnCancellation;
use agens_fixtures::{Script, ScriptedDialect, ScriptedProvider};
use agens_server::grpc::proto::{self, feed_client::FeedClient};
use agens_server::{CoordinatorSettings, SchedulerLimits, SessionShutdown};
use tonic::transport::{Channel, Endpoint, Uri};

use crate::CliDependencies;
use crate::deps::bootstrap;
use crate::worker::run_worker;

/// How long an assertion waits for loops that tick on a heartbeat and for a
/// turn that talks to a socket.
pub(crate) const PATIENCE: Duration = Duration::from_secs(60);

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn scratch() -> PathBuf {
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

/// What every end-to-end run in this crate asks of the coordinator: one slot,
/// one worktree, and a heartbeat fast enough for a test to wait out.
pub(crate) fn daemon_settings() -> CoordinatorSettings {
    CoordinatorSettings {
        heartbeat: Duration::from_millis(25),
        scheduler: SchedulerLimits {
            max_concurrent: 1,
            available_worktrees: 1,
            provider_capacity: BTreeMap::new(),
            default_provider_capacity: 1,
        },
        ..CoordinatorSettings::default()
    }
}

/// A daemon composed the way `agens serve` composes one, with a scripted model
/// where the provider would be.
pub(crate) struct DaemonFixture {
    /// Everything this run wrote, removed by the test that owns it.
    pub(crate) root: PathBuf,
    /// The checkout runs are created against.
    pub(crate) checkout: PathBuf,
    pub(crate) data_directory: PathBuf,
    /// The model's side of the journey, kept so a test can read the requests
    /// back and assert the script was consumed.
    pub(crate) provider: ScriptedProvider,
    pub(crate) socket: PathBuf,
    pub(crate) shutdown: HeadlessTurnCancellation,
    bootstrap: Bootstrap,
    settings: CoordinatorSettings,
}

impl DaemonFixture {
    pub(crate) fn start(script: Script, settings: CoordinatorSettings) -> Self {
        let mut settings = settings;
        let root = scratch();
        let checkout = checkout(&root);
        let config_home = root.join("config");
        let data_directory = root.join("data");
        std::fs::create_dir_all(&config_home).expect("create the config directory");
        std::fs::create_dir_all(&data_directory).expect("create the data directory");

        let provider = ScriptedProvider::start(ScriptedDialect::Responses, script);
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
        // else: a repository nobody named is a repository whose hooks it would
        // be executing on a caller's say-so. The fixture's own checkout is the
        // only one any of these runs names, so the fixture is what declares it
        // rather than every caller repeating it.
        settings.policy.project_roots = vec![checkout.clone()];

        let socket = agens_server::socket_path(&data_directory);

        Self {
            root,
            checkout,
            data_directory,
            provider,
            socket,
            shutdown: HeadlessTurnCancellation::new(),
            bootstrap,
            settings,
        }
    }

    /// The repository path a `CreateRun` request names.
    pub(crate) fn repo_root(&self) -> String {
        self.checkout.display().to_string()
    }

    /// Stops the daemon however the client thread ends.
    pub(crate) fn stopper(&self) -> Stopper {
        Stopper(self.shutdown.clone())
    }

    /// Serves on this thread until the shutdown is asked for, which is what the
    /// client thread does by dropping its [`Stopper`].
    pub(crate) fn serve(&self) -> SessionShutdown {
        agens_server::serve_until_shutdown(
            &self.data_directory,
            &self.settings,
            run_worker(&self.bootstrap),
            &self.shutdown,
        )
        .expect("the daemon serves")
    }
}

pub(crate) async fn connect(socket: PathBuf) -> Channel {
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
pub(crate) struct Stopper(pub(crate) HeadlessTurnCancellation);

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

pub(crate) async fn await_reported_state(
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
pub(crate) async fn journal_of(client: &mut FeedClient<Channel>, run_id: i64) -> Vec<String> {
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
