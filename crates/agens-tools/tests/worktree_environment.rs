//! One test in its own binary, because it sets a process-wide environment
//! variable: a `GIT_DIR` set for the whole test process would decide the
//! outcome of every other test that reaches git.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use agens_tools::SessionWorktrees;

#[test]
fn an_inherited_git_dir_does_not_redirect_the_worktree_at_another_repository() {
    let root = std::env::temp_dir().join(format!(
        "agens-tools-worktree-environment-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);

    let session = initialize_repository(&root.join("session"), "session");
    let other = initialize_repository(&root.join("other"), "other");
    let data_directory = root.join("data");

    // What the hardening has to survive: an environment that names another
    // repository. Without `GIT_DIR` removed, `worktree add` registers the new
    // worktree in `other` and checks its content out instead.
    unsafe {
        std::env::set_var("GIT_DIR", other.join(".git"));
    }

    let created = SessionWorktrees::new(&data_directory)
        .create(&session, "repo-a1b2c3d4", "session-one", "topic", "HEAD")
        .expect("create worktree");

    let registered = git(&session, &["worktree", "list", "--porcelain"]);
    assert!(
        registered
            .lines()
            .any(|line| line == format!("worktree {}", created.display())),
        "the worktree was not registered in the session repository: {registered}"
    );
    assert!(
        created.join("session.txt").exists(),
        "the worktree holds another repository's content"
    );

    unsafe {
        std::env::remove_var("GIT_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn initialize_repository(path: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(path).expect("create repository directory");
    git(path, &["init", "--quiet", "--initial-branch=main"]);
    git(path, &["config", "user.name", "Agens Test"]);
    git(path, &["config", "user.email", "agens-test@localhost"]);
    std::fs::write(path.join(format!("{name}.txt")), "initial\n").expect("write tracked file");
    git(path, &["add", "."]);
    git(path, &["commit", "--quiet", "-m", "initial"]);

    path.to_path_buf()
}

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env_remove("GIT_DIR")
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
