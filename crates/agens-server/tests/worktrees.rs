use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_server::SessionWorktrees;

static NEXT_REPOSITORY: AtomicUsize = AtomicUsize::new(0);

struct Repository {
    root: PathBuf,
    checkout: PathBuf,
    data_directory: PathBuf,
}

impl Repository {
    fn new() -> Self {
        let suffix = NEXT_REPOSITORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "agens-server-worktrees-{}-{suffix}",
            std::process::id()
        ));
        let checkout = root.join("repository");
        let data_directory = root.join("data");

        std::fs::create_dir_all(&checkout).expect("create repository directory");
        git(&checkout, &["init", "--quiet", "--initial-branch=main"]);
        git(&checkout, &["config", "user.name", "Agens Test"]);
        git(&checkout, &["config", "user.email", "agens-test@localhost"]);
        std::fs::write(checkout.join("tracked.txt"), "initial\n").expect("write tracked file");
        git(&checkout, &["add", "tracked.txt"]);
        git(&checkout, &["commit", "--quiet", "-m", "initial"]);

        Self {
            root,
            checkout,
            data_directory,
        }
    }

    fn worktrees(&self) -> SessionWorktrees {
        SessionWorktrees::new(&self.data_directory)
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn git(directory: &Path, arguments: &[&str]) -> String {
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

    String::from_utf8(output.stdout).expect("git output is UTF-8")
}

#[test]
fn create_places_the_session_worktree_under_the_repository_id() {
    let repository = Repository::new();
    let worktrees = repository.worktrees();

    let path = worktrees
        .create(
            &repository.checkout,
            "repo-a1b2c3d4",
            "session-one",
            "feature/session-one",
            "main",
        )
        .expect("create worktree");

    assert_eq!(
        path,
        repository
            .data_directory
            .join("worktrees/repo-a1b2c3d4/session-one")
    );
    assert_eq!(
        git(&path, &["rev-parse", "--show-toplevel"]).trim(),
        path.display().to_string()
    );
    assert_eq!(
        git(&path, &["branch", "--show-current"]).trim(),
        "feature/session-one"
    );
}

#[test]
fn status_rederives_whether_the_worktree_head_is_merged() {
    let repository = Repository::new();
    let worktrees = repository.worktrees();
    let path = worktrees
        .create(
            &repository.checkout,
            "repo-a1b2c3d4",
            "session-two",
            "feature/session-two",
            "main",
        )
        .expect("create worktree");

    std::fs::write(path.join("tracked.txt"), "worker change\n").expect("write worker change");
    git(&path, &["add", "tracked.txt"]);
    git(&path, &["commit", "--quiet", "-m", "worker change"]);

    let before = worktrees
        .status("repo-a1b2c3d4", "session-two", "main")
        .expect("read worktree status");
    assert!(!before.merged);

    git(
        &repository.checkout,
        &[
            "merge",
            "--quiet",
            "--no-edit",
            "--no-ff",
            "feature/session-two",
        ],
    );

    let after = worktrees
        .status("repo-a1b2c3d4", "session-two", "main")
        .expect("read worktree status");
    assert!(after.merged);
}

#[test]
fn status_reports_tracked_and_untracked_changes_as_dirty() {
    let repository = Repository::new();
    let worktrees = repository.worktrees();
    let path = worktrees
        .create(
            &repository.checkout,
            "repo-a1b2c3d4",
            "session-three",
            "feature/session-three",
            "main",
        )
        .expect("create worktree");

    let clean = worktrees
        .status("repo-a1b2c3d4", "session-three", "main")
        .expect("read clean status");
    assert!(!clean.dirty);

    std::fs::write(path.join("tracked.txt"), "modified\n").expect("modify tracked file");
    let tracked = worktrees
        .status("repo-a1b2c3d4", "session-three", "main")
        .expect("read tracked status");
    assert!(tracked.dirty);

    git(&path, &["restore", "tracked.txt"]);
    std::fs::write(path.join("untracked.txt"), "new\n").expect("write untracked file");
    let untracked = worktrees
        .status("repo-a1b2c3d4", "session-three", "main")
        .expect("read untracked status");
    assert!(untracked.dirty);
}

#[test]
fn remove_unregisters_and_deletes_the_session_worktree() {
    let repository = Repository::new();
    let worktrees = repository.worktrees();
    let path = worktrees
        .create(
            &repository.checkout,
            "repo-a1b2c3d4",
            "session-four",
            "feature/session-four",
            "main",
        )
        .expect("create worktree");

    worktrees
        .remove(&repository.checkout, "repo-a1b2c3d4", "session-four")
        .expect("remove worktree");

    assert!(!path.exists());
    assert!(
        !git(&repository.checkout, &["worktree", "list", "--porcelain"])
            .lines()
            .any(|line| line == format!("worktree {}", path.display()))
    );
}
