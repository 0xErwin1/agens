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

/// Where a client attaches to the daemon of one data directory.
///
/// Derived rather than reported, so a client that wants to reach a daemon does
/// not need the daemon to tell it where it is listening.
#[must_use]
pub fn socket_path(data_directory: &Path) -> PathBuf {
    data_directory.join(SOCKET_FILE)
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
#[derive(Debug)]
pub struct ServeInstance {
    lock: File,
    socket_path: PathBuf,
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

        Ok(Self { lock, socket_path })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for ServeInstance {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
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
    match fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServeInstanceError::unavailable(
            "remove stale runtime socket",
            error,
        )),
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

        drop(instance);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn every_runtime_path_lives_under_the_data_directory() {
        let directory = data_directory();

        let instance = ServeInstance::acquire(&directory).unwrap();

        assert!(instance.socket_path().starts_with(&directory));
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
