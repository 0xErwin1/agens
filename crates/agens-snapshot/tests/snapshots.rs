//! What a snapshot has to survive: a turn that rewrote a file, a turn that
//! created one, a project that moved on underneath the snapshot, and a project
//! that is not a repository at all.
//!
//! The cases that matter most are the ones where a snapshot could end up
//! deleting or overwriting something no agent wrote, so most of what follows
//! asserts what was *not* touched.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use agens_snapshot::{SnapshotId, WorkspaceSnapshots};

const OVER_SIZE_CAP: usize = 3 * 1024 * 1024;

struct Project {
    root: PathBuf,
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

        Self {
            root,
            worktree,
            data,
        }
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

    fn commit(&self, message: &str) {
        git(&self.worktree, &["add", "--all"]);
        git(&self.worktree, &["commit", "--quiet", "-m", message]);
    }

    fn snapshots(&self) -> WorkspaceSnapshots {
        WorkspaceSnapshots::open(&self.data, &self.worktree)
            .expect("snapshot repository opens")
            .expect("project is a git worktree")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
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

/// Undoing a created file has to leave a redo able to find it again. The undo
/// deletes it, and a path that exists nowhere is reported by nothing — so the
/// snapshot has to stop describing it too, or the redo silently does nothing.
#[test]
fn a_file_the_agent_created_can_be_put_back_after_being_undone() {
    let project = Project::new().into_repository();
    let snapshots = project.snapshots();

    let before = snapshots.capture().expect("capture succeeds");
    project.write("src/created.rs", "fn main() {}\n");
    let after = snapshots.capture().expect("capture succeeds");

    let undoing = snapshots.changed_since(&before).expect("diff succeeds");
    snapshots.restore(&before, &undoing).expect("undo succeeds");
    assert_eq!(project.read("src/created.rs"), None);

    let redoing = snapshots.changed_since(&after).expect("diff succeeds");
    assert_eq!(
        redoing,
        vec!["src/created.rs".to_owned()],
        "the file the undo deleted is exactly what a redo has to put back"
    );
    let report = snapshots.restore(&after, &redoing).expect("redo succeeds");
    assert_eq!(report.restored, vec!["src/created.rs".to_owned()]);
    assert_eq!(
        project.read("src/created.rs").as_deref(),
        Some("fn main() {}\n")
    );
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

/// A turn that commits its own work leaves nothing dirty against the project's
/// history, which is not the same as leaving nothing to undo.
#[test]
fn work_committed_after_the_capture_is_still_seen_as_changed() {
    let project = Project::new().into_repository();
    let snapshots = project.snapshots();

    let before = snapshots.capture().expect("capture succeeds");
    project.write("tracked.txt", "agent output\n");
    project.commit("the agent committed its own work");

    let changed = snapshots.changed_since(&before).expect("diff succeeds");
    assert_eq!(
        changed,
        vec!["tracked.txt".to_owned()],
        "a clean worktree at a new commit is not an unchanged worktree"
    );

    snapshots
        .restore(&before, &changed)
        .expect("restore succeeds");
    assert_eq!(project.read("tracked.txt").as_deref(), Some("original\n"));
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

/// A turn that un-ignores a file makes a file the reader already had appear in
/// the snapshot for the first time. It was never created by the turn, so undoing
/// the turn must not delete it.
#[test]
fn a_file_that_only_became_visible_is_not_deleted_by_an_undo() {
    let project = Project::new().into_repository();
    project.write(".gitignore", "local.env\n");
    project.write("local.env", "the reader's secret\n");
    project.commit("ignore the local environment");

    let snapshots = project.snapshots();
    let before = snapshots.capture().expect("capture succeeds");
    assert!(
        snapshots
            .uncovered(&before)
            .expect("the uncovered set is readable")
            .contains(&"local.env".to_owned()),
        "a file the snapshot could not hold has to be reported as such"
    );

    project.write(".gitignore", "\n");
    let changed = snapshots.changed_since(&before).expect("diff succeeds");
    assert!(changed.contains(&"local.env".to_owned()));

    let report = snapshots
        .restore(&before, &changed)
        .expect("restore succeeds");
    assert_eq!(
        project.read("local.env").as_deref(),
        Some("the reader's secret\n"),
        "the turn changed which files are visible, not which files exist"
    );
    assert_eq!(report.uncovered, vec!["local.env".to_owned()]);
    assert!(report.removed.is_empty(), "{report:?}");
}

/// A file too large to snapshot is outside the feature in both directions. It
/// must not come back from the snapshot at all, because the only content the
/// snapshot could offer for it is content it never captured.
#[test]
fn a_file_past_the_size_cap_is_never_restored_from_a_snapshot() {
    let project = Project::new().into_repository();
    project.write("big.txt", "committed\n");
    project.commit("track the large file while it is small");

    let snapshots = project.snapshots();
    project.write("big.txt", &"x".repeat(OVER_SIZE_CAP));

    let before = snapshots.capture().expect("capture succeeds");
    assert!(
        snapshots
            .uncovered(&before)
            .expect("the uncovered set is readable")
            .contains(&"big.txt".to_owned())
    );

    project.write("big.txt", "the agent shrank it\n");
    let changed = snapshots.changed_since(&before).expect("diff succeeds");
    assert!(changed.contains(&"big.txt".to_owned()));

    let report = snapshots
        .restore(&before, &changed)
        .expect("restore succeeds");
    assert_eq!(
        project.read("big.txt").as_deref(),
        Some("the agent shrank it\n"),
        "restoring committed content over an uncaptured edit would be a silent loss"
    );
    assert_eq!(report.uncovered, vec!["big.txt".to_owned()]);
    assert!(report.removed.is_empty(), "{report:?}");
}

/// The snapshot starts from the project's index. A project without one still
/// has files, and a capture that quietly described it as empty would turn the
/// next undo into a deletion of everything the turn touched.
#[test]
fn a_project_whose_index_is_missing_is_still_described_by_its_history() {
    let project = Project::new().into_repository();
    std::fs::remove_file(project.worktree.join(".git").join("index"))
        .expect("the fixture index exists");

    let snapshots = project.snapshots();
    let before = snapshots.capture().expect("capture succeeds");
    project.write("tracked.txt", "agent output\n");

    let changed = snapshots.changed_since(&before).expect("diff succeeds");
    assert_eq!(changed, vec!["tracked.txt".to_owned()]);

    let report = snapshots
        .restore(&before, &changed)
        .expect("restore succeeds");
    assert_eq!(report.restored, vec!["tracked.txt".to_owned()]);
    assert_eq!(
        project.read("tracked.txt").as_deref(),
        Some("original\n"),
        "a file the project tracks is restored, not deleted"
    );
}

/// The empty tree is a snapshot in which nothing exists. Handing one back for a
/// project full of files would make the next undo delete them, so a capture
/// that arrives at it says so instead.
#[test]
fn a_capture_that_would_describe_an_empty_project_is_refused() {
    let project = Project::new().into_repository();
    let snapshots = project.snapshots();

    for repository in std::fs::read_dir(project.data.join("snapshots")).expect("snapshot storage") {
        let repository = repository.expect("snapshot repository entry").path();
        for entry in std::fs::read_dir(&repository).expect("snapshot repository") {
            let entry = entry.expect("repository entry").path();
            let is_index = entry
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("index"));
            if is_index {
                std::fs::remove_file(&entry).expect("the session index is removable");
            }
        }
    }

    assert!(
        snapshots.capture().is_err(),
        "a snapshot that knows about no file is not a snapshot of this project"
    );
    assert_eq!(project.read("tracked.txt").as_deref(), Some("original\n"));
}

/// A linked worktree has its own index and its own checkout. Seeding from the
/// main repository would describe a different set of files entirely.
#[test]
fn a_linked_worktree_is_snapshotted_from_its_own_checkout() {
    let project = Project::new().into_repository();
    git(&project.worktree, &["checkout", "--quiet", "-b", "other"]);
    project.write("tracked.txt", "the other branch\n");
    project.commit("diverge");
    git(&project.worktree, &["checkout", "--quiet", "-"]);

    let linked = project.root.join("linked");
    git(
        &project.worktree,
        &[
            "worktree",
            "add",
            "--quiet",
            linked.to_str().expect("a printable fixture path"),
            "other",
        ],
    );

    let snapshots = WorkspaceSnapshots::open(&project.data, &linked)
        .expect("snapshot repository opens")
        .expect("a linked worktree is a git worktree");
    let before = snapshots.capture().expect("capture succeeds");

    std::fs::write(linked.join("tracked.txt"), "agent output\n").expect("write into the worktree");
    let changed = snapshots.changed_since(&before).expect("diff succeeds");
    assert_eq!(changed, vec!["tracked.txt".to_owned()]);

    snapshots
        .restore(&before, &changed)
        .expect("restore succeeds");
    assert_eq!(
        std::fs::read_to_string(linked.join("tracked.txt"))
            .ok()
            .as_deref(),
        Some("the other branch\n"),
        "the linked worktree is restored to its own content, not the main one's"
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

/// Two sessions on one project share a repository but must not share the state
/// a capture is built from, or each would write the other's tree.
#[test]
fn two_sessions_on_one_worktree_capture_independently() {
    let project = Project::new().into_repository();
    let data = project.data.clone();
    let worktree = project.worktree.clone();

    let (one, two) = std::thread::scope(|scope| {
        let opener = |directory: PathBuf, root: PathBuf| {
            scope.spawn(move || {
                WorkspaceSnapshots::open(&directory, &root)
                    .expect("opens")
                    .expect("is a worktree")
            })
        };
        let first = opener(data.clone(), worktree.clone());
        let second = opener(data, worktree);
        (
            first.join().expect("the first session opens"),
            second.join().expect("the second session opens"),
        )
    });

    let session_indexes = std::fs::read_dir(project.data.join("snapshots"))
        .expect("snapshot storage")
        .filter_map(|repository| repository.ok())
        .flat_map(|repository| std::fs::read_dir(repository.path()).expect("snapshot repository"))
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("index"))
        .count();
    assert_eq!(
        session_indexes, 2,
        "each session stages through state of its own"
    );

    project.write("one.txt", "session one\n");
    let first_state = one.capture().expect("capture succeeds");
    project.write("two.txt", "session two\n");
    let second_state = two.capture().expect("capture succeeds");

    let changed = one
        .changed_since(&first_state)
        .expect("the first session still sees its own snapshot");
    assert_eq!(changed, vec!["two.txt".to_owned()]);
    assert!(
        two.changed_since(&second_state)
            .expect("the second session sees its own snapshot")
            .is_empty()
    );

    let report = one
        .restore(&first_state, &changed)
        .expect("restore succeeds");
    assert_eq!(report.removed, vec!["two.txt".to_owned()]);
    assert_eq!(
        project.read("one.txt").as_deref(),
        Some("session one\n"),
        "the other session's capture is not the other session's undo"
    );
}

/// A hash from somewhere else reaches git in argument position, so it is
/// checked before it is used rather than after.
#[test]
fn an_id_that_is_not_an_object_hash_is_refused() {
    let project = Project::new().into_repository();
    let snapshots = project.snapshots();

    let forged = SnapshotId::from_hash("--output=/tmp/escape".to_owned());
    assert!(snapshots.changed_since(&forged).is_err());
    assert!(
        snapshots
            .restore(&forged, &["tracked.txt".to_owned()])
            .is_err()
    );
    assert_eq!(project.read("tracked.txt").as_deref(), Some("original\n"));
}

/// "Not a repository" and "this repository could not be read" are different
/// answers. Reporting the second as the first sends the reader looking for a
/// git project they already have.
#[test]
fn a_repository_git_refuses_to_read_is_a_failure_not_an_answer() {
    let project = Project::new().into_repository();
    std::fs::write(
        project.worktree.join(".git").join("config"),
        "this is not = valid [config\n",
    )
    .expect("the fixture configuration is writable");

    let outcome = WorkspaceSnapshots::open(&project.data, &project.worktree);
    assert!(
        outcome.is_err(),
        "an unreadable repository is not an answer"
    );
}

/// Error text is read by people. It names what was attempted, not where the
/// session keeps its data.
#[test]
fn a_failure_names_the_operation_that_failed() {
    let project = Project::new().into_repository();
    let snapshots = project.snapshots();

    let missing = SnapshotId::from_hash("0".repeat(40));
    let failure = snapshots
        .changed_since(&missing)
        .expect_err("an unknown tree cannot be diffed");
    assert!(
        failure.to_string().starts_with("snapshot diff failed:"),
        "{failure}"
    );
    assert!(
        !failure
            .to_string()
            .contains(project.data.to_str().expect("a printable fixture path")),
        "{failure}"
    );
}

/// The snapshot repository holds copies of the project's files, so it is no
/// more readable than the project's own runtime state.
#[cfg(unix)]
#[test]
fn snapshot_storage_is_private_to_its_owner() {
    use std::os::unix::fs::PermissionsExt;

    let project = Project::new().into_repository();
    let snapshots = project.snapshots();
    snapshots.capture().expect("capture succeeds");

    for repository in std::fs::read_dir(project.data.join("snapshots")).expect("snapshot storage") {
        let repository = repository.expect("snapshot repository entry").path();
        let mode = std::fs::metadata(&repository)
            .expect("the repository is readable")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{repository:?} is readable by others");
    }
}

/// Two routes to one working tree are one working tree, and must not become two
/// separately seeded repositories.
#[test]
fn a_symlinked_route_to_a_project_finds_the_same_repository() {
    let project = Project::new().into_repository();
    let direct = project.snapshots();
    let before = direct.capture().expect("capture succeeds");

    let alias = project.root.join("alias");
    std::os::unix::fs::symlink(&project.worktree, &alias).expect("the fixture links");
    let through_link = WorkspaceSnapshots::open(&project.data, &alias)
        .expect("opens")
        .expect("is a worktree");

    project.write("tracked.txt", "agent output\n");
    let changed = through_link
        .changed_since(&before)
        .expect("the snapshot taken by the other route resolves");
    assert_eq!(changed, vec!["tracked.txt".to_owned()]);
    assert_eq!(
        std::fs::read_dir(project.data.join("snapshots"))
            .expect("snapshot storage")
            .count(),
        1,
        "one working tree keeps one snapshot repository"
    );
}

/// git writes its listings into a pipe that holds a page at a time, so a tree
/// large enough to fill it has to be drained while git is still running.
#[test]
fn a_listing_too_large_for_a_pipe_does_not_stall() {
    let project = Project::new().into_repository();
    for index in 0..3000 {
        project.write(
            &format!("generated/a-fairly-long-untracked-name-{index:06}.txt"),
            "x",
        );
    }

    let snapshots = project.snapshots();
    let started = std::time::Instant::now();
    let before = snapshots.capture().expect("capture succeeds");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "capturing a large tree waited for the timeout instead of reading"
    );

    for index in 0..300 {
        project.write(&format!("generated/created-{index:04}.txt"), "agent output");
    }
    let changed = snapshots.changed_since(&before).expect("diff succeeds");
    assert_eq!(changed.len(), 300);

    let report = snapshots
        .restore(&before, &changed)
        .expect("restore succeeds");
    assert_eq!(report.removed.len(), 300, "{:?}", report.failed);
    assert_eq!(project.read("generated/created-0000.txt"), None);
}

/// Snapshot repositories hold copies of project files, so one whose project is
/// gone is content nobody can reach and nobody asked to keep.
#[test]
fn a_snapshot_repository_outliving_its_project_is_pruned() {
    let live = Project::new().into_repository();
    let doomed = Project::new().into_repository();

    let data = live.data.clone();
    let kept = WorkspaceSnapshots::open(&data, &live.worktree)
        .expect("opens")
        .expect("is a worktree");
    let orphan = WorkspaceSnapshots::open(&data, &doomed.worktree)
        .expect("opens")
        .expect("is a worktree");
    kept.capture().expect("capture succeeds");
    orphan.capture().expect("capture succeeds");
    drop(orphan);
    std::fs::remove_dir_all(&doomed.worktree).expect("the fixture project is removable");

    let removed = WorkspaceSnapshots::prune_orphans(&data).expect("pruning succeeds");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert!(
        kept.capture().is_ok(),
        "pruning must not disturb a project that is still there"
    );
}
