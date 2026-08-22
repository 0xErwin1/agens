//! Where a session's tools are working right now.
//!
//! The directory a tool call resolves a relative path against is session
//! state, not process state: the tools move it, and every other reader only
//! observes it. Keeping it here rather than in the process's own current
//! directory is what lets two sessions in one process sit in different trees,
//! and what keeps a surface's report of the location honest without the
//! surface having to ask the tools.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Notified when the session's working directory moves, so an audit log or a
/// surface learns about the move as it happens rather than by polling.
pub type WorkingDirectoryObserver = Arc<dyn Fn(&Path) + Send + Sync>;

/// The directory a session's tools resolve relative paths against.
///
/// Cloning shares the same directory: the clone the tools hold is the clone a
/// footer reads, which is what lets the location on screen follow a tool call
/// that moved it.
#[derive(Clone)]
pub struct WorkingDirectory {
    current: Arc<Mutex<PathBuf>>,
    observer: Option<WorkingDirectoryObserver>,
}

impl WorkingDirectory {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            current: Arc::new(Mutex::new(directory.into())),
            observer: None,
        }
    }

    /// Attaches the observer notified on every later move.
    pub fn with_observer(mut self, observer: WorkingDirectoryObserver) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Where the session's tools are working.
    ///
    /// A poisoned lock still answers: the value behind it is one owned
    /// `PathBuf` that no panic can leave half-written, and a surface that
    /// cannot report a location is worse than one reporting the last known
    /// one.
    pub fn current(&self) -> PathBuf {
        match self.current.lock() {
            Ok(current) => current.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Records a completed move.
    ///
    /// Called by the tools that perform one, and by the runtime that opens
    /// them when the directory it was told to reopen in is gone: leaving the
    /// recorded location pointing at a directory nothing can read would make
    /// every later reader of this handle report a place the session is not.
    pub fn moved_to(&self, directory: &Path) {
        match self.current.lock() {
            Ok(mut current) => *current = directory.to_path_buf(),
            Err(poisoned) => *poisoned.into_inner() = directory.to_path_buf(),
        }

        if let Some(observer) = self.observer.as_ref() {
            observer(directory);
        }
    }
}

impl fmt::Debug for WorkingDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkingDirectory")
            .field("current", &self.current())
            .field("observed", &self.observer.is_some())
            .finish()
    }
}
