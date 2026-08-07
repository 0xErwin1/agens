//! What a snapshot has to survive: a turn that rewrote a file, a turn that
//! created one, and a project that is not a repository at all.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use agens_snapshot::WorkspaceSnapshots;

struct Project {
    worktree: PathBuf,
    data: PathBuf,
}

impl Project {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "agens-snapshot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the fixture clock is after the epoch")
                .as_nanos()
        ));
        let worktree = root.join("project");
        let data = root.join("data");
        std::fs::create_dir_all(&worktree).expect("project directory");
        std::fs::create_dir_all(&data).expect("data directory");

        Self { worktree, data }
    }

    fn into_repository(self) -> Self {
        git(&self.worktree, &["init", "--quiet"]);
        git(&self.worktree, &["config", "user.name", "test"]);
        git(&self.worktree, &["config", "user.email", "test@localhost"]);
        self.write("tracked.txt", "original\n");
        git(&self.worktree, &["add", "."]);
        git(&self.worktree, &["commit", "--quiet", "-m", "initial"]);
        self
    }

    fn write(&self, path: &str, content: &str) {
        let target = self.worktree.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("parent directory");
        }
        std::fs::write(target, content).expect("write fixture file");
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.worktree.join(path)).ok()
    }

    fn snapshots(&self) -> WorkspaceSnapshots {
        WorkspaceSnapshots::open(&self.data, &self.worktree)
            .expect("snapshot repository opens")
            .expect("project is a git worktree")
    }
}

fn git(directory: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {arguments:?} failed");
}

/// The ordinary undo: a file the agent rewrote comes back byte for byte.
#[test]
fn a_rewritten_file_is_restored_to_its_captured_content() {
    let project = Project::new().into_repository();
    let snapshots = project.snapshots();

    let before = snapshots.capture().expect("capture succeeds");
    project.write("tracked.txt", "rewritten by the agent\n");

    let changed = snapshots.changed_since(&before).expect("diff succeeds");
    assert_eq!(changed, vec!["tracked.txt".to_owned()]);

    let report = snapshots
        .restore(&before, &changed)
        .expect("restore succeeds");
    assert_eq!(report.restored, vec!["tracked.txt".to_owned()]);
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(project.read("tracked.txt").as_deref(), Some("original\n"));
}

/// A file that did not exist in the snapshot is not a file to restore — it is a
/// file to take away, or the undo leaves the agent's work behind.
#[test]
fn a_file_the_agent_created_is_removed_rather_than_restored() {
    let project = Project::new().into_repository();
    let snapshots = project.snapshots();

    let before = snapshots.capture().expect("capture succeeds");
    project.write("src/created.rs", "fn main() {}\n");

    let changed = snapshots.changed_since(&before).expect("diff succeeds");
    assert_eq!(changed, vec!["src/created.rs".to_owned()]);

    let report = snapshots
        .restore(&before, &changed)
        .expect("restore succeeds");
    assert_eq!(report.removed, vec!["src/created.rs".to_owned()]);
    assert_eq!(project.read("src/created.rs"), None);
}

/// Restoring is per-path on purpose: a caller undoing one turn must not roll
/// back a file that turn never touched.
#[test]
fn restore_leaves_paths_outside_the_list_alone() {
    let project = Project::new().into_repository();
    let snapshots = project.snapshots();

    let before = snapshots.capture().expect("capture succeeds");
    project.write("tracked.txt", "agent wrote this\n");
    project.write("untouched.txt", "the reader wrote this\n");

    let report = snapshots
        .restore(&before, &["tracked.txt".to_owned()])
        .expect("restore succeeds");
    assert_eq!(report.restored, vec!["tracked.txt".to_owned()]);
    assert_eq!(project.read("tracked.txt").as_deref(), Some("original\n"));
    assert_eq!(
        project.read("untouched.txt").as_deref(),
        Some("the reader wrote this\n"),
        "a path nobody asked to restore is not the snapshot's business"
    );
}

/// The redo path: capturing the state an undo is about to discard is what makes
/// putting it back exact rather than a re-run.
#[test]
fn a_capture_taken_before_restoring_puts_the_discarded_state_back() {
    let project = Project::new().into_repository();
    let snapshots = project.snapshots();

    let before = snapshots.capture().expect("capture succeeds");
    project.write("tracked.txt", "agent output\n");
    let after = snapshots.capture().expect("capture succeeds");

    snapshots
        .restore(&before, &["tracked.txt".to_owned()])
        .expect("undo succeeds");
    assert_eq!(project.read("tracked.txt").as_deref(), Some("original\n"));

    snapshots
        .restore(&after, &["tracked.txt".to_owned()])
        .expect("redo succeeds");
    assert_eq!(
        project.read("tracked.txt").as_deref(),
        Some("agent output\n"),
        "redo returns the exact bytes the undo discarded"
    );
}

/// Ignored files are the project's own decision, and a snapshot that captured
/// them would restore build output over a reader's work.
#[test]
fn ignored_files_are_never_captured() {
    let project = Project::new().into_repository();
    project.write(".gitignore", "ignored/\n");
    git(&project.worktree, &["add", ".gitignore"]);
    git(&project.worktree, &["commit", "--quiet", "-m", "ignore"]);

    let snapshots = project.snapshots();
    let before = snapshots.capture().expect("capture succeeds");
    project.write("ignored/artifact.bin", "build output\n");

    let changed = snapshots.changed_since(&before).expect("diff succeeds");
    assert!(
        changed.is_empty(),
        "an ignored path is not a change to undo: {changed:?}"
    );
}

/// Not every project is a repository, and that is an answer rather than a
/// failure: the caller says so and disables the command.
#[test]
fn a_project_without_git_reports_that_it_cannot_be_snapshotted() {
    let project = Project::new();

    let opened = WorkspaceSnapshots::open(&project.data, &project.worktree)
        .expect("probing a plain directory is not an error");
    assert!(opened.is_none());
}

/// Two projects must never share a snapshot repository, or one undo would
/// restore the other's files.
#[test]
fn each_worktree_restores_only_its_own_files() {
    let first = Project::new().into_repository();
    let second = Project::new().into_repository();

    let shared_data = first.data.clone();
    let one = WorkspaceSnapshots::open(&shared_data, &first.worktree)
        .expect("opens")
        .expect("is a worktree");
    let two = WorkspaceSnapshots::open(&shared_data, &second.worktree)
        .expect("opens")
        .expect("is a worktree");

    let first_before = one.capture().expect("capture succeeds");
    let second_before = two.capture().expect("capture succeeds");
    first.write("tracked.txt", "first project\n");
    second.write("tracked.txt", "second project\n");

    one.restore(&first_before, &["tracked.txt".to_owned()])
        .expect("restore succeeds");
    assert_eq!(first.read("tracked.txt").as_deref(), Some("original\n"));
    assert_eq!(
        second.read("tracked.txt").as_deref(),
        Some("second project\n"),
        "restoring one project must leave the other where it was"
    );

    two.restore(&second_before, &["tracked.txt".to_owned()])
        .expect("restore succeeds");
    assert_eq!(second.read("tracked.txt").as_deref(), Some("original\n"));
}
