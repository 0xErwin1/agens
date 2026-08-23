//! What the coordinator's test binaries all need before they can assert
//! anything: a scratch directory of their own, epoch seconds, a control-plane
//! run to drive, and a launcher that produces no session.
//!
//! Each of them had its own copy, byte-identical but for the words that make
//! the test's own point, so a change to the shape of a `RunRow` was a change to
//! seven files. What varies per test stays at the call site through struct
//! update syntax; what never varied lives here.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agens_server::{
    ChatError, ChatSessionFactory, ChatSessionRequest, ChatSessions, LaunchError, RunLaunch,
    RunWorkerFactory, SessionSupervisor,
};
use agens_store::{RunRow, RunState, WorktreeStatus};

/// The repository every coordinator test drives a run in.
pub(crate) const REPO: &str = "a1b2c3d4e5f60718";

/// The checkout a run names. Never read from, because no test in this set
/// reaches git.
pub(crate) const REPO_ROOT: &str = "/home/dev/agens";

/// What a run says it is working on.
pub(crate) const SCOPE: &str = "crates/agens-server/src/coordinator";

/// The backend a run names when no test asserts anything about the provider.
pub(crate) const PROVIDER: &str = "scripted";

/// What a launcher that starts nothing says.
pub(crate) const REFUSAL: &str = "this test starts no sessions";

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

/// A directory this test alone writes in.
///
/// The counter and the process id together are what keep two tests in one
/// binary, and two binaries running at once, off each other's data.
pub(crate) fn scratch_directory(area: &str, kind: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-server-{area}-{kind}-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();

    directory
}

/// Epoch seconds, which is what every control-plane timestamp is written in.
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

/// The worktree directory `CreateRun` would have provisioned, created.
///
/// Admission reads the worktree column rather than assuming it: a run whose
/// directory is not `active` is held as ineligible rather than offered a slot,
/// and these tests write the row directly instead of going through the call
/// that would have provisioned it.
pub(crate) fn worktree_in(directory: &Path, name: &str) -> PathBuf {
    let worktree = directory.join("worktrees").join(REPO).join(name);
    fs::create_dir_all(&worktree).unwrap();

    worktree
}

/// A run in the given state, with the worktree it works in already active.
///
/// The task, scope, definition of done and external reference are what one test
/// differs from another in, so a caller states its own over this base rather
/// than restating the twenty fields it does not care about.
pub(crate) fn run_in(state: RunState, worktree: &Path) -> RunRow {
    RunRow {
        id: None,
        repo_id: REPO.to_owned(),
        repo_root: REPO_ROOT.to_owned(),
        remote_url: None,
        external_ref: None,
        parent_run_id: None,
        task: "give the coordinator a run to move".to_owned(),
        scope: SCOPE.to_owned(),
        dod: "the run reaches the state the test asserts".to_owned(),
        genesis_paths: None,
        state,
        priority: 5,
        dep_run_id: None,
        provider: PROVIDER.to_owned(),
        budget_tokens: None,
        worktree_path: Some(worktree.display().to_string()),
        worktree_status: Some(WorktreeStatus::Active),
        created_at: now(),
        result: None,
    }
}

/// A launcher that never produces a session.
///
/// A launch that refuses leaves the run where the assertion can read it instead
/// of racing the admission loop for the row.
pub(crate) fn refusing_worker() -> RunWorkerFactory {
    Arc::new(|_launch: &RunLaunch<'_>| Err(LaunchError(REFUSAL.to_owned()))) as RunWorkerFactory
}

/// A chat factory for the tests that compose a daemon without opening a chat.
///
/// It refuses rather than building anything: reaching it would mean a test
/// opened a chat, and what a hosted chat does is asserted where one is opened.
pub(crate) fn refusing_chat() -> ChatSessionFactory {
    Arc::new(|_: &ChatSessionRequest| Err(ChatError::Unavailable(REFUSAL.to_owned())))
        as ChatSessionFactory
}

/// The same, already assembled into the registry a facade is served with.
///
/// It takes the supervisor rather than building one, so a daemon's chats are
/// held by the same registry its runs are, which is what the production
/// composition does.
pub(crate) fn no_chats(supervisor: SessionSupervisor) -> Arc<ChatSessions> {
    Arc::new(ChatSessions::new(supervisor, refusing_chat()))
}
