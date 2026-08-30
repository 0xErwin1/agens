//! The daemon's lifecycle as an operator drives it: start it and get the
//! terminal back, stop it, ask what it is doing.
//!
//! Starting detached re-executes this binary as `serve --foreground` rather
//! than forking. The process reaching this point already has a signal thread
//! and whatever else the runtime started, and a `fork` of a multithreaded
//! process inherits every lock those threads happened to hold without the
//! threads that would release them. A fresh image has none of that history,
//! and the child that comes back is exactly the process a supervisor would
//! have started with `--foreground` itself.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use agens_bootstrap::Bootstrap;
use agens_error::CliError;
use agens_store::{ControlPlaneStore, RunState};

/// How long a start waits for the daemon to accept on its socket.
///
/// Generous on purpose: what happens before the first accept is boot
/// reconciliation over the control plane, and a machine with a long journal
/// takes longer than an empty one. A start that gave up early would report a
/// failure for a daemon that then comes up anyway.
const START_PATIENCE: Duration = Duration::from_secs(30);

/// How long a stop waits for the daemon to be gone.
///
/// The daemon's own shutdown is bounded — it stops its sessions within ten
/// seconds and names the ones that outlived the wait — so this only has to
/// outlast that bound and the writes that follow it.
const STOP_PATIENCE: Duration = Duration::from_secs(30);

/// How often a wait looks again.
const POLL: Duration = Duration::from_millis(25);
#[derive(Clone, Copy)]
pub(crate) enum DaemonStartupRequest {
    ExplicitAttached,
}

/// The states a run is in while the daemon is carrying it.
///
/// A draft is not one of them: it is a run that was created and is waiting for
/// a person to approve its plan, not for this process.
pub(super) const ACTIVE_STATES: [RunState; 4] = [
    RunState::Queued,
    RunState::Running,
    RunState::AwaitingInput,
    RunState::AwaitingQuota,
];

/// Starts the daemon only when this machine has none, and reports whether it started one.
pub(crate) fn ensure_running(
    bootstrap: &Bootstrap,
    _request: DaemonStartupRequest,
) -> Result<bool, CliError> {
    let data_directory = bootstrap.data_directory();
    if running_pid(&agens_server::pid_path(data_directory)).is_some() {
        return Ok(false);
    }

    if agens_server::slot_is_held(data_directory) {
        await_serving(data_directory, None).map_err(CliError::unavailable)?;
        return Ok(false);
    }

    start_detached(bootstrap)?;
    Ok(true)
}

/// Starts the daemon detached and returns once it is serving.
///
/// The return is the contract: an operator who has their prompt back has a
/// daemon whose socket, coordinator and control plane are all up, not one that
/// may still fail to compose. Waiting on the socket alone would not give them
/// that — a unix socket answers `connect` from the moment it is bound, which is
/// before the coordinator behind it exists — so what this waits for is the pid
/// the daemon publishes when it starts serving.
pub(crate) fn start_detached(bootstrap: &Bootstrap) -> Result<String, CliError> {
    let data_directory = bootstrap.data_directory().to_path_buf();
    let socket = agens_server::socket_path(&data_directory);

    if let Some(pid) = running_pid(&agens_server::pid_path(&data_directory)) {
        return Ok(format!(
            "a daemon is already running for this machine at pid {pid} on {}\n",
            socket.display()
        ));
    }

    let log = open_log(&data_directory)?;
    let mut child = spawn_detached(&log)?;

    match await_serving(&data_directory, Some(&mut child)) {
        Ok(()) => Ok(format!("the daemon is listening on {}\n", socket.display())),
        Err(reason) => Err(CliError::unavailable(format!(
            "{reason}; its output is in {}",
            agens_server::log_path(&data_directory).display()
        ))),
    }
}

/// Asks the running daemon to stop, and waits for it to be gone.
///
/// `SIGTERM` and nothing harder: the daemon bounds its own shutdown and reports
/// what it left behind, and a `SIGKILL` from here would take that report away
/// along with the sessions' chance to end.
pub(crate) fn stop(bootstrap: &Bootstrap) -> Result<String, CliError> {
    let data_directory = bootstrap.data_directory();
    let pid_path = agens_server::pid_path(data_directory);

    let Some(pid) = running_pid(&pid_path) else {
        // A daemon that holds the slot without having published a pid is one
        // that is still composing itself. Refused rather than reported as
        // absent: the caller asked for it to be gone, and it is about to be up.
        if agens_server::slot_is_held(data_directory) {
            return Err(CliError::unavailable(
                "a daemon is starting for this machine and has not published its pid yet; \
                 stop it once it is running",
            ));
        }

        // Idempotent: a stop whose subject is already gone did what it was
        // asked. What it also does is clear a pid a crashed daemon left, so the
        // next `status` does not report a process that is not there.
        let _ = std::fs::remove_file(&pid_path);

        return Ok("no daemon is running for this machine\n".to_owned());
    };

    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(CliError::unavailable(format!(
            "the daemon at pid {pid} could not be signalled: {}",
            io::Error::last_os_error()
        )));
    }

    let deadline = Instant::now() + STOP_PATIENCE;
    while alive(pid) {
        if Instant::now() >= deadline {
            return Err(CliError::unavailable(format!(
                "the daemon at pid {pid} was asked to stop and is still running"
            )));
        }

        std::thread::sleep(POLL);
    }

    // The daemon removes its own pid file on the way out. This only covers a
    // process that died without unwinding, which is the case that would
    // otherwise leave a pid pointing at nothing.
    let _ = std::fs::remove_file(&pid_path);

    Ok(format!("the daemon at pid {pid} stopped\n"))
}

/// Reports what the machine's daemon is doing, without attaching to it.
///
/// Read out of the control plane rather than asked of the daemon: a status that
/// needs the facade to answer says nothing at exactly the moment the facade is
/// the thing that went wrong.
pub(crate) fn status(bootstrap: &Bootstrap) -> Result<String, CliError> {
    let data_directory = bootstrap.data_directory();
    let pid_path = agens_server::pid_path(data_directory);
    let socket = agens_server::socket_path(data_directory);

    let Some(pid) = running_pid(&pid_path) else {
        // Told apart rather than collapsed into "stopped": a daemon holding the
        // slot with no pid published is one whose coordinator is still being
        // built, and an operator watching a slow start needs to see that it is
        // happening rather than that nothing is.
        let state = if agens_server::slot_is_held(data_directory) {
            "starting"
        } else {
            "stopped"
        };

        return Ok(format!("Agens serve status\nStatus:  {state}\n"));
    };

    let listening = if accepts(&socket) {
        "accepting"
    } else {
        "not accepting"
    };

    let mut report = String::from("Agens serve status\nStatus:  running\n");
    report.push_str(&format!("Pid:     {pid}\n"));
    report.push_str(&format!("Socket:  {} ({listening})\n", socket.display()));

    if let Some(uptime) = uptime(&pid_path) {
        report.push_str(&format!("Uptime:  {}\n", render_uptime(uptime)));
    }

    report.push_str(&format!("Runs:    {}\n", active_runs(data_directory)));

    Ok(report)
}

/// The pid of the daemon this data directory has, if it has one.
///
/// A pid file whose process is gone is not a daemon: the file outlives a
/// process that died without unwinding, and reporting it as running would send
/// the next `stop` at a pid the kernel may since have handed to something else.
///
/// Zero and negatives are refused before anything is asked of them. To `kill`
/// they are not process ids at all but process groups — `-1` is every process
/// the caller may signal — so a corrupt pid file must never reach it.
fn running_pid(pid_path: &Path) -> Option<libc::pid_t> {
    let pid = std::fs::read_to_string(pid_path)
        .ok()?
        .trim()
        .parse::<libc::pid_t>()
        .ok()
        .filter(|pid| *pid > 0)?;

    alive(pid).then_some(pid)
}

/// Signal zero: the existence and permission checks run, and nothing is sent.
fn alive(pid: libc::pid_t) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Whether something is listening on the socket right now.
fn accepts(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

/// The daemon's log, opened before the daemon so a process that dies before it
/// can say anything still has somewhere its words went.
fn open_log(data_directory: &Path) -> Result<File, CliError> {
    std::fs::create_dir_all(data_directory).map_err(|error| {
        CliError::unavailable(format!("the data directory is unavailable: {error}"))
    })?;

    let log_path = agens_server::log_path(data_directory);

    // Appended rather than truncated: why one start failed is worth more than a
    // clean file, and this is the only record of it.
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|error| {
            CliError::unavailable(format!(
                "the daemon log at {} is unavailable: {error}",
                log_path.display()
            ))
        })
}

/// Re-executes this binary as the daemon, detached from the terminal.
///
/// `setsid` leaves the child in its own session with no controlling terminal,
/// so the terminal that started it can close without a `SIGHUP` reaching it.
/// The child never opens a tty afterwards — its three descriptors are the log
/// and `/dev/null` — which is what a second fork would otherwise be there to
/// guarantee.
///
/// The working directory is inherited on purpose. Configuration resolves from
/// it, so a daemon started somewhere else would be a daemon reading a different
/// configuration than the command that just started it.
fn spawn_detached(log: &File) -> Result<Child, CliError> {
    let executable = std::env::current_exe().map_err(|error| {
        CliError::unavailable(format!("this executable is unreachable: {error}"))
    })?;

    let output = log.try_clone().map_err(|error| {
        CliError::unavailable(format!("the daemon log is unavailable: {error}"))
    })?;
    let errors = log.try_clone().map_err(|error| {
        CliError::unavailable(format!("the daemon log is unavailable: {error}"))
    })?;

    let mut command = Command::new(executable);
    command
        .arg("serve")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(errors));

    // Only `setsid`, which is async-signal-safe. Anything that allocates here
    // would run between the fork and the exec, in a child with one thread and
    // every lock the others were holding.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }

            Ok(())
        });
    }

    command
        .spawn()
        .map_err(|error| CliError::unavailable(format!("the daemon could not be started: {error}")))
}

/// Waits for the daemon to be serving, or for it to die trying.
///
/// Both endings are watched, because only one of them ever arrives: a daemon
/// that refused to start never publishes a pid, and a wait that only looked for
/// one would spend its whole patience on a process that is already gone.
fn await_serving(data_directory: &Path, mut child: Option<&mut Child>) -> Result<(), String> {
    let pid_path = agens_server::pid_path(data_directory);
    let deadline = Instant::now() + START_PATIENCE;

    loop {
        if running_pid(&pid_path).is_some() {
            return Ok(());
        }

        let mut lost_race = false;
        if let Some(child) = child.as_deref_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if running_pid(&pid_path).is_some() {
                        return Ok(());
                    }

                    // A held slot with the child gone means the child lost the
                    // machine's slot to a daemon that is on its way up, which
                    // happens when two launches spawn at the same time. The
                    // wait continues on the winner's pid rather than reporting
                    // a failure for a daemon that then comes up.
                    if !agens_server::slot_is_held(data_directory) {
                        return Err(format!(
                            "the daemon stopped before it started serving ({status})"
                        ));
                    }

                    lost_race = true;
                }
                Ok(None) => {}
                Err(error) => return Err(format!("the daemon could not be waited on: {error}")),
            }
        }

        if lost_race {
            child = None;
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "the daemon did not start serving on {} within {} seconds",
                agens_server::socket_path(data_directory).display(),
                START_PATIENCE.as_secs()
            ));
        }

        std::thread::sleep(POLL);
    }
}

/// How long the daemon has been up, taken from when it published its pid.
fn uptime(pid_path: &Path) -> Option<Duration> {
    SystemTime::now()
        .duration_since(std::fs::metadata(pid_path).ok()?.modified().ok()?)
        .ok()
}

fn render_uptime(uptime: Duration) -> String {
    let seconds = uptime.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);

    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// The runs the daemon is carrying, by state.
///
/// A control plane it cannot read is reported as that rather than as no runs: a
/// daemon with an unreadable journal is something an operator has to know
/// about, and a confident `0 active` would hide it. A journal that does not
/// exist yet is the opposite case and reads as what it is — no runs — because a
/// machine that has never had one has nothing to report, not a fault.
///
/// Opening is conditional on the file for the same reason: a report must not be
/// what creates the database it is reporting on.
fn active_runs(data_directory: &Path) -> String {
    if !agens_store::unified_database_path(data_directory).exists() {
        return "0 active".to_owned();
    }

    let Ok(store) = ControlPlaneStore::open(data_directory) else {
        return "unreadable".to_owned();
    };

    let mut total = 0;
    let mut detail = Vec::new();

    for state in ACTIVE_STATES {
        let Ok(runs) = store.runs_in_state(state) else {
            return "unreadable".to_owned();
        };

        if !runs.is_empty() {
            total += runs.len();
            detail.push(format!("{} {}", runs.len(), state.as_str()));
        }
    }

    if detail.is_empty() {
        return "0 active".to_owned();
    }

    format!("{total} active ({})", detail.join(", "))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    fn scratch() -> PathBuf {
        let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "agens-serve-lifecycle-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create the scratch directory");

        directory
    }

    #[test]
    fn uptime_reads_as_the_coarsest_unit_that_is_not_zero() {
        assert_eq!(render_uptime(Duration::from_secs(9)), "9s");
        assert_eq!(render_uptime(Duration::from_secs(63)), "1m 3s");
        assert_eq!(render_uptime(Duration::from_secs(3723)), "1h 2m 3s");
    }

    /// `-1` is every process the caller may signal, and `0` is its own process
    /// group. Neither may ever be read back as the daemon.
    #[test]
    fn a_pid_that_is_not_a_process_is_refused_before_it_is_signalled() {
        let directory = scratch();
        let pid_path = directory.join("serve.pid");

        for written in ["-1", "0"] {
            std::fs::write(&pid_path, format!("{written}\n")).expect("write the pid file");

            assert_eq!(running_pid(&pid_path), None, "{written} is not a daemon");
        }

        std::fs::remove_dir_all(directory).expect("remove the scratch directory");
    }

    #[test]
    fn an_absent_pid_file_is_not_a_running_daemon() {
        let directory = scratch();

        assert_eq!(running_pid(&directory.join("serve.pid")), None);

        std::fs::remove_dir_all(directory).expect("remove the scratch directory");
    }

    #[test]
    fn a_pid_file_that_is_not_a_number_is_not_a_running_daemon() {
        let directory = scratch();
        let pid_path = directory.join("serve.pid");
        std::fs::write(&pid_path, "not a pid\n").expect("write the pid file");

        assert_eq!(running_pid(&pid_path), None);

        std::fs::remove_dir_all(directory).expect("remove the scratch directory");
    }

    #[test]
    fn a_pid_file_naming_this_process_reads_as_running() {
        let directory = scratch();
        let pid_path = directory.join("serve.pid");
        std::fs::write(&pid_path, format!("{}\n", std::process::id())).expect("write the pid file");

        assert_eq!(
            running_pid(&pid_path),
            Some(libc::pid_t::try_from(std::process::id()).expect("this process has a pid"))
        );

        std::fs::remove_dir_all(directory).expect("remove the scratch directory");
    }

    /// The launcher whose child lost the slot race is looking at a daemon on
    /// its way up, not at a failed start: the wait continues on the winner's
    /// pid instead of reporting a failure for a daemon that then comes up.
    #[test]
    fn a_child_that_lost_the_slot_race_defers_to_the_winners_pid() {
        use std::os::fd::AsRawFd;

        let directory = scratch();

        // Hold the machine's slot the way the winning daemon does.
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("serve.lock"))
            .expect("open the slot lock");
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "the test holds the slot"
        );

        // The loser: a child that already exited without publishing a pid.
        let mut child = Command::new("true").spawn().expect("spawn a done child");
        child.wait().expect("the child ends");

        // The winner publishes its pid while the loser's launcher is waiting.
        let pid_path = directory.join("serve.pid");
        let publisher = std::thread::spawn({
            let pid_path = pid_path.clone();
            move || {
                std::thread::sleep(Duration::from_millis(200));
                std::fs::write(&pid_path, format!("{}\n", std::process::id()))
                    .expect("publish the winner's pid");
            }
        });

        let waited = await_serving(&directory, Some(&mut child));

        publisher.join().expect("the publisher thread ends");
        assert_eq!(waited, Ok(()), "the wait ends on the winner's pid");

        drop(lock);
        std::fs::remove_dir_all(directory).expect("remove the scratch directory");
    }

    /// Without a slot holder there is no daemon on its way up, so a child that
    /// died without publishing a pid is a failed start and is reported as one
    /// now, not after the whole patience.
    #[test]
    fn a_child_that_died_with_no_slot_holder_is_a_failed_start() {
        let directory = scratch();

        let mut child = Command::new("false").spawn().expect("spawn a done child");
        child.wait().expect("the child ends");

        let started = Instant::now();
        let waited = await_serving(&directory, Some(&mut child));

        assert!(
            waited
                .expect_err("the start failed")
                .contains("stopped before it started serving"),
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the failure is reported without waiting out the patience"
        );

        std::fs::remove_dir_all(directory).expect("remove the scratch directory");
    }

    #[test]
    fn a_socket_nothing_is_listening_on_does_not_accept() {
        let directory = scratch();

        assert!(!accepts(&directory.join("serve.sock")));

        std::fs::remove_dir_all(directory).expect("remove the scratch directory");
    }
}
