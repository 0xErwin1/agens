//! `agens serve` as an operator drives it, against the real binary.
//!
//! Nothing here can be proved in-process: what is under test is that a command
//! returns while a *different* process keeps running, and that a signal reaches
//! it. So every one of these runs the built binary with its own configuration
//! and its own data directory, and asserts on the runtime files and the socket
//! rather than on anything the daemon says about itself.
//!
//! No test sleeps for a fixed time. A start is waited out on the socket, a stop
//! on the process being gone, and both waits are bounded so a hang fails the
//! test instead of the suite.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// How long any of these waits before calling it a failure.
const PATIENCE: Duration = Duration::from_secs(60);

/// How often a wait looks again.
const POLL: Duration = Duration::from_millis(25);

static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

/// One isolated operator: a configuration home, a data directory, and no reach
/// into the machine's own.
struct Operator {
    root: PathBuf,
    data_directory: PathBuf,
}

impl Operator {
    fn prepare() -> Self {
        let suffix = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "agens-serve-lifecycle-{}-{suffix}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let configuration = root.join("config");
        let data_directory = root.join("data");
        std::fs::create_dir_all(&configuration).expect("an isolated configuration home");
        std::fs::create_dir_all(root.join("home")).expect("an isolated home");

        // A provider that is configured and never reached: nothing in this file
        // creates a run, so the daemon boots, listens, and talks to no model.
        std::fs::write(
            configuration.join("config.toml"),
            format!(
                "[provider]\nmodel = \"openai-api/gpt-4.1\"\nbase_url = \"http://127.0.0.1:9/v1\"\n\
                 \n[options]\ndata_dir = \"{}\"\n",
                data_directory.display()
            ),
        )
        .expect("write the configuration");
        std::fs::write(
            configuration.join("auth.json"),
            r#"{"openai-api": {"api_key": "fixture"}}"#,
        )
        .expect("write the credentials");

        Self {
            root,
            data_directory,
        }
    }

    /// The binary with the operator's roots and nothing else of the machine's.
    ///
    /// The environment is cleared rather than extended: an inherited
    /// `XDG_DATA_HOME` or `AGENS_CONFIG_HOME` would point this daemon at the
    /// data directory of whoever is running the suite.
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agens"));
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("AGENS_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("XDG_DATA_HOME", self.root.join("xdg-data"))
            .current_dir(&self.root);

        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command()
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .expect("the command runs")
    }

    fn socket(&self) -> PathBuf {
        self.data_directory.join("serve.sock")
    }

    fn pid_path(&self) -> PathBuf {
        self.data_directory.join("serve.pid")
    }

    fn published_pid(&self) -> i32 {
        std::fs::read_to_string(self.pid_path())
            .expect("the daemon published its pid")
            .trim()
            .parse()
            .expect("the pid file holds a number")
    }
}

impl Drop for Operator {
    fn drop(&mut self) {
        // Whatever the test proved or failed to prove, no daemon outlives it.
        let _ = self
            .command()
            .args(["serve", "stop"])
            .stdin(Stdio::null())
            .output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn accepts(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;

    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(POLL);
    }
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn assert_succeeded(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_bare_serve_returns_the_terminal_with_the_daemon_still_listening() {
    let operator = Operator::prepare();

    let started = operator.run(&["serve"]);

    assert_succeeded(&started, "serve");
    assert!(
        stdout_of(&started).contains(&operator.socket().display().to_string()),
        "the start names the socket a client attaches to: {}",
        stdout_of(&started)
    );

    // The command returned. The point of the whole change is that what it left
    // behind is a daemon a client can already reach.
    assert!(
        accepts(&operator.socket()),
        "the socket accepts as soon as the start returns"
    );
    assert!(
        alive(operator.published_pid()),
        "the published pid names a live process"
    );
}

#[test]
fn a_second_serve_reports_the_running_daemon_and_does_not_start_another() {
    let operator = Operator::prepare();
    assert_succeeded(&operator.run(&["serve"]), "the first serve");
    let first = operator.published_pid();

    let second = operator.run(&["serve"]);

    assert_succeeded(&second, "the second serve");
    assert!(
        stdout_of(&second).contains("already running"),
        "the second start says why it did nothing: {}",
        stdout_of(&second)
    );
    assert_eq!(
        operator.published_pid(),
        first,
        "the daemon that was already running is the one still running"
    );
}

#[test]
fn status_reports_the_daemon_before_it_stops_and_its_absence_after() {
    let operator = Operator::prepare();
    assert_succeeded(&operator.run(&["serve"]), "serve");
    let pid = operator.published_pid();

    let running = operator.run(&["serve", "status"]);

    assert_succeeded(&running, "serve status");
    let report = stdout_of(&running);
    assert!(report.contains("Status:  running"), "{report}");
    assert!(report.contains(&format!("Pid:     {pid}")), "{report}");
    assert!(
        report.contains(&operator.socket().display().to_string()),
        "{report}"
    );
    assert!(report.contains("accepting"), "{report}");
    assert!(report.contains("Runs:    0 active"), "{report}");

    assert_succeeded(&operator.run(&["serve", "stop"]), "serve stop");

    let stopped = operator.run(&["serve", "status"]);

    assert_succeeded(&stopped, "serve status after stop");
    assert!(stdout_of(&stopped).contains("Status:  stopped"), "{report}");
}

/// The regression this file exists for. `serve` used to return as soon as the
/// socket answered `connect`, which a unix socket does from the moment it is
/// bound — before the coordinator behind it has opened the control plane. A
/// `status` at that instant could not read the journal and said so, which is a
/// daemon reporting a fault about itself while it is merely still starting.
///
/// Now the start waits for the daemon to say it is serving, so every reader
/// after it finds a control plane that is already open.
#[test]
fn a_start_that_returned_leaves_a_journal_its_status_can_read() {
    let operator = Operator::prepare();

    assert_succeeded(&operator.run(&["serve"]), "serve");

    assert!(
        operator.data_directory.join("agens.db").exists(),
        "the control plane is open by the time the start returns"
    );

    let report = stdout_of(&operator.run(&["serve", "status"]));

    assert!(
        report.contains("Runs:    0 active"),
        "the journal is readable, not reported as a fault: {report}"
    );
}

/// A data directory nothing has ever served is not a fault either, and asking
/// about it must not be what creates its database.
#[test]
fn status_without_a_daemon_reports_nothing_and_creates_nothing() {
    let operator = Operator::prepare();

    let report = stdout_of(&operator.run(&["serve", "status"]));

    assert!(report.contains("Status:  stopped"), "{report}");
    assert!(
        !operator.data_directory.join("agens.db").exists(),
        "a status report did not create a control plane"
    );
}

#[test]
fn stop_ends_the_daemon_and_clears_what_it_left_behind() {
    let operator = Operator::prepare();
    assert_succeeded(&operator.run(&["serve"]), "serve");
    let pid = operator.published_pid();

    let stopped = operator.run(&["serve", "stop"]);

    assert_succeeded(&stopped, "serve stop");
    assert!(!alive(pid), "the daemon is gone by the time stop returns");
    assert!(
        !operator.pid_path().exists(),
        "the pid file goes with the process it named"
    );
    assert!(
        !operator.socket().exists(),
        "the socket goes with the daemon that bound it"
    );
}

/// A stop with nothing to stop is not a failure. It is the state the caller
/// asked for, so scripts that stop before starting do not have to guess.
#[test]
fn stop_without_a_daemon_reports_it_and_succeeds() {
    let operator = Operator::prepare();

    let stopped = operator.run(&["serve", "stop"]);

    assert_succeeded(&stopped, "serve stop");
    assert!(
        stdout_of(&stopped).contains("no daemon is running"),
        "{}",
        stdout_of(&stopped)
    );
}

#[test]
fn foreground_keeps_the_terminal_until_it_is_signalled() {
    let operator = Operator::prepare();

    let mut child = operator
        .command()
        .args(["serve", "--foreground"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the foreground daemon starts");

    // Waited out on the pid rather than on the socket, for the same reason the
    // start waits on it: the socket answers `connect` from the moment it is
    // bound, and the pid is what says the daemon is serving.
    wait_until("the foreground daemon to serve", || {
        operator.pid_path().exists()
    });

    assert!(accepts(&operator.socket()), "a serving daemon is reachable");
    assert!(
        child.try_wait().expect("the child is waitable").is_none(),
        "--foreground has not returned while the daemon is serving"
    );
    assert_eq!(
        operator.published_pid(),
        i32::try_from(child.id()).expect("a pid fits"),
        "the process holding the terminal is the daemon itself, not a child of it"
    );

    // The bounded shutdown, reached the way `serve stop` and a process
    // supervisor both reach it.
    assert_eq!(
        unsafe {
            libc::kill(
                i32::try_from(child.id()).expect("a pid fits"),
                libc::SIGTERM,
            )
        },
        0,
        "the daemon is signalled"
    );

    let status = child.wait().expect("the daemon ends");

    assert!(
        status.success(),
        "a daemon asked to stop stops cleanly: {status}"
    );
    assert!(!operator.socket().exists(), "the socket is released");
    assert!(!operator.pid_path().exists(), "the pid file is released");
}

/// `--foreground` says how the daemon runs. Next to a verb that runs no daemon
/// it is a typo, and a typo that is silently ignored is one nobody fixes.
#[test]
fn foreground_next_to_a_subcommand_is_refused() {
    let operator = Operator::prepare();

    let refused = operator.run(&["serve", "--foreground", "status"]);

    assert!(!refused.status.success(), "the command is refused");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("takes no subcommand"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

/// The start reports a daemon that could not start rather than waiting out its
/// whole patience on a socket that will never appear.
#[test]
fn a_daemon_that_cannot_start_is_reported_with_its_log() {
    let operator = Operator::prepare();

    // A data directory that cannot be a directory: the daemon fails to take its
    // slot, and the failure is the one an operator has to be told about.
    std::fs::create_dir_all(operator.root.join("data-parent")).expect("a parent for the blocker");
    let blocked = operator.root.join("data-parent").join("blocked");
    std::fs::write(&blocked, "not a directory").expect("write the blocker");
    std::fs::write(
        operator.root.join("config").join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/gpt-4.1\"\nbase_url = \"http://127.0.0.1:9/v1\"\n\
             \n[options]\ndata_dir = \"{}\"\n",
            blocked.display()
        ),
    )
    .expect("rewrite the configuration");

    let started = operator.run(&["serve"]);

    assert!(!started.status.success(), "the start reports the failure");
    let reported = String::from_utf8_lossy(&started.stderr);
    assert!(
        reported.contains("data directory") || reported.contains("daemon"),
        "the failure names what went wrong: {reported}"
    );
}

/// The runtime files are the contract `serve stop`, `serve status` and a
/// process supervisor all read. They live under the data directory and nowhere
/// else.
#[test]
fn the_runtime_files_live_under_the_data_directory() {
    let operator = Operator::prepare();
    assert_succeeded(&operator.run(&["serve"]), "serve");

    for name in ["serve.pid", "serve.log", "serve.sock", "serve.lock"] {
        let path = operator.data_directory.join(name);

        assert!(
            std::fs::metadata(&path).is_ok(),
            "the daemon has a {name} under its data directory"
        );
    }

    assert!(
        matches!(
            std::fs::metadata(operator.root.join("serve.pid")).map_err(|error| error.kind()),
            Err(ErrorKind::NotFound)
        ),
        "and writes nothing beside it"
    );
}
