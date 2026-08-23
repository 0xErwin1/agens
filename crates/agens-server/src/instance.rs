//! Single-instance ownership of the machine's daemon runtime.
//!
//! The server is one daemon per machine serving N projects, not one process per
//! repository (AGN-80), so its runtime paths derive from the data directory and
//! never from the working directory or a project root. A second `serve` must not
//! start: two daemons over one `agens.db` would break the single-writer property
//! the coordinator's state machines, scheduler and timers all rest on.

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
};

const LOCK_FILE: &str = "serve.lock";
const SOCKET_FILE: &str = "serve.sock";
const PID_FILE: &str = "serve.pid";
const LOG_FILE: &str = "serve.log";

/// Where a client attaches to the daemon of one data directory.
///
/// Derived rather than reported, so a client that wants to reach a daemon does
/// not need the daemon to tell it where it is listening.
#[must_use]
pub fn socket_path(data_directory: &Path) -> PathBuf {
    data_directory.join(SOCKET_FILE)
}

/// Where the running daemon publishes its process id.
///
/// Derived like the socket, and for the same reason: `serve stop` and
/// `serve status` have to find a daemon nobody told them about.
#[must_use]
pub fn pid_path(data_directory: &Path) -> PathBuf {
    data_directory.join(PID_FILE)
}

/// Where a detached daemon's output goes.
///
/// A daemon that returns the terminal has nowhere else to write: this is the
/// one place an operator can read why it refused to start.
#[must_use]
pub fn log_path(data_directory: &Path) -> PathBuf {
    data_directory.join(LOG_FILE)
}

#[derive(Debug)]
pub enum ServeInstanceError {
    /// Another daemon holds the lock. Reported as its own variant because the
    /// caller must attach rather than start a second process.
    AlreadyRunning,
    Unavailable(String),
}

impl ServeInstanceError {
    fn unavailable(action: &str, error: impl std::fmt::Display) -> Self {
        Self::Unavailable(format!("{action}: {error}"))
    }
}

/// Exclusive ownership of the daemon runtime, held for the life of the process.
/// Dropping it releases the advisory lock and removes the socket.
///
/// Taking the slot and being ready to serve are two different moments, and the
/// pid file marks the second one. Everything between them — binding the socket,
/// composing the coordinator, opening the control plane — is a daemon that owns
/// the machine's slot and cannot answer for it yet.
#[derive(Debug)]
pub struct ServeInstance {
    lock: File,
    socket_path: PathBuf,
    pid_path: PathBuf,
}

impl ServeInstance {
    /// Takes the lock before touching anything else, so a leftover socket is only
    /// ever removed by the process that just proved no daemon is running.
    pub fn acquire(data_directory: &Path) -> Result<Self, ServeInstanceError> {
        fs::create_dir_all(data_directory)
            .map_err(|error| ServeInstanceError::unavailable("create data directory", error))?;
        restrict(data_directory, 0o700)?;

        let lock_path = data_directory.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| ServeInstanceError::unavailable("open runtime lock", error))?;
        restrict(&lock_path, 0o600)?;

        take_exclusive_lock(&lock)?;

        let socket_path = socket_path(data_directory);
        remove_stale_socket(&socket_path)?;

        // A pid left by a crashed daemon is stale by construction here, and it
        // must not be readable as this one until this one is actually serving.
        let pid_path = pid_path(data_directory);
        remove_stale_pid(&pid_path)?;

        Ok(Self {
            lock,
            socket_path,
            pid_path,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn pid_path(&self) -> &Path {
        &self.pid_path
    }

    /// Publishes this process's id, which is what says the daemon is serving.
    ///
    /// Called by the daemon once everything it serves with is built, so whoever
    /// finds a pid finds a daemon whose socket, coordinator and control plane
    /// are all up. That is what makes it the signal a start can wait on: a
    /// socket answers `connect` from the moment it is bound, long before
    /// anything behind it can answer a request.
    ///
    /// Only reached with the lock held, which is what makes the write safe: the
    /// id and the lock name the same process for as long as the file exists,
    /// and the drop below is what ends that.
    pub fn publish_pid(&self) -> Result<(), ServeInstanceError> {
        fs::write(&self.pid_path, format!("{}\n", std::process::id()))
            .map_err(|error| ServeInstanceError::unavailable("publish the runtime pid", error))?;

        restrict(&self.pid_path, 0o600)
    }
}

/// Whether a daemon holds this data directory's slot right now.
///
/// Asked of the lock rather than of the pid file, so it also answers for a
/// daemon that has taken the machine's slot and has not finished starting. A
/// caller that finds no pid and a held slot is looking at a daemon on its way
/// up, not at an absent one.
///
/// Read-only: the lock file is never created here, and a lock this probe
/// happens to take is released before it returns.
#[must_use]
pub fn slot_is_held(data_directory: &Path) -> bool {
    let Ok(lock) = File::open(data_directory.join(LOCK_FILE)) else {
        return false;
    };

    let taken = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if taken == 0 {
        unsafe {
            libc::flock(lock.as_raw_fd(), libc::LOCK_UN);
        }

        return false;
    }

    io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
}

impl Drop for ServeInstance {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_file(&self.pid_path);
        let _ = &self.lock;
    }
}

/// `flock` with `LOCK_NB`: the lock lives on the open file description, so it is
/// released by the kernel even if the daemon dies without unwinding.
fn take_exclusive_lock(lock: &File) -> Result<(), ServeInstanceError> {
    let taken = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if taken == 0 {
        return Ok(());
    }

    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Err(ServeInstanceError::AlreadyRunning),
        _ => Err(ServeInstanceError::unavailable("lock runtime", error)),
    }
}

/// Only reached with the lock held, which is what makes the removal safe: a
/// socket file left by a crashed daemon is stale by construction here.
fn remove_stale_socket(socket_path: &Path) -> Result<(), ServeInstanceError> {
    remove_stale(socket_path, "remove stale runtime socket")
}

/// The same, for the pid: whoever holds the lock is the only daemon there is,
/// so any id still on disk belongs to one that is gone.
fn remove_stale_pid(pid_path: &Path) -> Result<(), ServeInstanceError> {
    remove_stale(pid_path, "remove stale runtime pid")
}

fn remove_stale(path: &Path, action: &str) -> Result<(), ServeInstanceError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServeInstanceError::unavailable(action, error)),
    }
}

fn restrict(path: &Path, maximum_mode: u32) -> Result<(), ServeInstanceError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::metadata(path)
        .map_err(|error| ServeInstanceError::unavailable("inspect runtime permissions", error))?;
    let current_mode = metadata.mode() & 0o777;
    let restricted_mode = current_mode & maximum_mode;

    if restricted_mode != current_mode {
        fs::set_permissions(path, fs::Permissions::from_mode(restricted_mode)).map_err(
            |error| ServeInstanceError::unavailable("restrict runtime permissions", error),
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    fn data_directory() -> PathBuf {
        let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("agens-serve-{}-{suffix}", std::process::id()))
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt;

        fs::metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn acquires_a_fresh_runtime_with_private_permissions() {
        let directory = data_directory();

        let instance = ServeInstance::acquire(&directory).unwrap();

        assert_eq!(mode(&directory), 0o700);
        assert_eq!(mode(&directory.join(LOCK_FILE)), 0o600);
        assert_eq!(instance.socket_path(), directory.join(SOCKET_FILE));

        instance.publish_pid().unwrap();
        assert_eq!(mode(&directory.join(PID_FILE)), 0o600);

        drop(instance);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn every_runtime_path_lives_under_the_data_directory() {
        let directory = data_directory();

        let instance = ServeInstance::acquire(&directory).unwrap();

        assert!(instance.socket_path().starts_with(&directory));
        assert!(instance.pid_path().starts_with(&directory));
        assert!(directory.join(LOCK_FILE).starts_with(&directory));

        drop(instance);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_second_acquisition_reports_a_running_daemon_instead_of_starting_one() {
        let directory = data_directory();
        let first = ServeInstance::acquire(&directory).unwrap();

        assert!(matches!(
            ServeInstance::acquire(&directory),
            Err(ServeInstanceError::AlreadyRunning)
        ));

        drop(first);
        assert!(ServeInstance::acquire(&directory).is_ok());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_socket_left_by_a_crashed_daemon_is_reclaimed() {
        let directory = data_directory();
        fs::create_dir_all(&directory).unwrap();
        let socket_path = directory.join(SOCKET_FILE);
        fs::write(&socket_path, "stale").unwrap();

        let instance = ServeInstance::acquire(&directory).unwrap();

        assert!(!socket_path.exists());

        drop(instance);
        fs::remove_dir_all(directory).unwrap();
    }

    /// The dangerous inverse of reclaiming a stale socket: a losing process must
    /// never delete the live daemon's socket on its way out.
    #[test]
    fn a_rejected_acquisition_leaves_the_running_daemon_socket_alone() {
        let directory = data_directory();
        let first = ServeInstance::acquire(&directory).unwrap();
        let socket_path = directory.join(SOCKET_FILE);
        fs::write(&socket_path, "live").unwrap();

        assert!(ServeInstance::acquire(&directory).is_err());

        assert!(socket_path.exists());

        drop(first);
        fs::remove_dir_all(directory).unwrap();
    }

    /// Holding the slot is not serving. Until the daemon says it is serving,
    /// nothing may read a pid for it — that is what makes the pid the signal a
    /// start waits on.
    #[test]
    fn taking_the_slot_publishes_no_pid() {
        let directory = data_directory();

        let instance = ServeInstance::acquire(&directory).unwrap();

        assert!(!instance.pid_path().exists());
        assert!(slot_is_held(&directory));

        drop(instance);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_serving_daemon_publishes_its_own_pid() {
        let directory = data_directory();

        let instance = ServeInstance::acquire(&directory).unwrap();
        instance.publish_pid().unwrap();

        let published = fs::read_to_string(instance.pid_path()).unwrap();
        assert_eq!(published.trim().parse::<u32>().unwrap(), std::process::id());

        drop(instance);
        fs::remove_dir_all(directory).unwrap();
    }

    /// A pid left by a crashed daemon must not outlive it: whoever takes the
    /// lock next is the process `serve stop` has to reach, and until that one
    /// serves there is no pid to read at all.
    #[test]
    fn a_pid_left_by_a_crashed_daemon_is_cleared_and_then_replaced() {
        let directory = data_directory();
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(PID_FILE), "999999\n").unwrap();

        let instance = ServeInstance::acquire(&directory).unwrap();

        assert!(!instance.pid_path().exists());

        instance.publish_pid().unwrap();
        let published = fs::read_to_string(instance.pid_path()).unwrap();
        assert_eq!(published.trim().parse::<u32>().unwrap(), std::process::id());

        drop(instance);
        fs::remove_dir_all(directory).unwrap();
    }

    /// The lock is what says a daemon owns this data directory, and reading it
    /// must leave it exactly as it was found.
    #[test]
    fn an_unheld_slot_reads_as_unheld_and_stays_takeable() {
        let directory = data_directory();

        assert!(!slot_is_held(&directory), "no lock file yet");

        let instance = ServeInstance::acquire(&directory).unwrap();
        assert!(slot_is_held(&directory));
        drop(instance);

        assert!(!slot_is_held(&directory), "the lock file outlives the lock");
        assert!(
            ServeInstance::acquire(&directory).is_ok(),
            "probing the slot did not take it"
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn releasing_the_runtime_removes_its_pid() {
        let directory = data_directory();
        let instance = ServeInstance::acquire(&directory).unwrap();
        instance.publish_pid().unwrap();
        let pid_path = instance.pid_path().to_path_buf();

        drop(instance);

        assert!(!pid_path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn releasing_the_runtime_removes_its_socket() {
        let directory = data_directory();
        let instance = ServeInstance::acquire(&directory).unwrap();
        let socket_path = instance.socket_path().to_path_buf();
        fs::write(&socket_path, "live").unwrap();

        drop(instance);

        assert!(!socket_path.exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
