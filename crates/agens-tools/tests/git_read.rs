use std::{
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agens_core::ToolAccess;
use agens_tools::{
    GitReadInput, GitReadOperation, NativeToolCatalog, NativeTools, ToolExecutionContext,
};
use serde_json::json;

static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

/// A temporary project directory that removes itself when the test ends.
///
/// The name deliberately does not rest on the process id alone: pids are a
/// finite, recycled key, so a leaked root from an earlier run can be handed
/// straight back to a later one. A root that already holds a repository makes
/// `repository()` replay `git init` on it and fail on an empty commit, which is
/// why a leak of this kind poisons every future run until `/tmp` is cleared.
struct ProjectRoot {
    path: PathBuf,
}

impl ProjectRoot {
    fn new() -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let nanoseconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agens-git-read-{}-{nanoseconds}-{sequence}",
            std::process::id()
        ));

        fs::create_dir_all(&path).expect("temporary project root");
        assert!(
            fs::read_dir(&path)
                .expect("read the temporary project root")
                .next()
                .is_none(),
            "the project root {} already exists and is not empty; \
             a leaked root from an earlier run would silently poison this test",
            path.display()
        );

        Self { path }
    }
}

impl Deref for ProjectRoot {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for ProjectRoot {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProjectRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn project_root() -> ProjectRoot {
    ProjectRoot::new()
}

fn git(root: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "agens")
        .env("GIT_AUTHOR_EMAIL", "agens@example.invalid")
        .env("GIT_COMMITTER_NAME", "agens")
        .env("GIT_COMMITTER_EMAIL", "agens@example.invalid")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

/// A repository with one commit on `main` containing `tracked.txt`.
fn repository() -> ProjectRoot {
    let root = project_root();
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    fs::write(root.join("tracked.txt"), "base\n").unwrap();
    git(&root, &["add", "tracked.txt"]);
    git(&root, &["commit", "--quiet", "-m", "base commit"]);
    root
}

fn run(root: &std::path::Path, input: GitReadInput) -> agens_tools::ToolOutput {
    NativeTools::open(root).unwrap().git_read(input).unwrap()
}

fn executable_script(
    root: &std::path::Path,
    name: &str,
    sentinel: &std::path::Path,
) -> std::path::PathBuf {
    let script = root.join(name);
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch {}\n", sentinel.display()),
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    script
}

#[test]
fn status_reports_untracked_and_modified_entries() {
    let root = repository();
    fs::write(root.join("tracked.txt"), "changed\n").unwrap();
    fs::write(root.join("fresh.txt"), "new\n").unwrap();

    let output = run(&root, GitReadInput::new(GitReadOperation::Status));

    assert!(!output.is_error, "{output:?}");
    assert!(output.content.contains("tracked.txt"), "{output:?}");
    assert!(output.content.contains("fresh.txt"), "{output:?}");
}

#[test]
fn diff_reports_the_worktree_patch_and_staged_selects_the_index() {
    let root = repository();
    fs::write(root.join("tracked.txt"), "changed\n").unwrap();

    let worktree = run(&root, GitReadInput::new(GitReadOperation::Diff));
    assert!(!worktree.is_error, "{worktree:?}");
    assert!(worktree.content.contains("-base"), "{worktree:?}");
    assert!(worktree.content.contains("+changed"), "{worktree:?}");

    let staged = run(
        &root,
        GitReadInput::new(GitReadOperation::Diff).with_staged(true),
    );
    assert!(!staged.is_error, "{staged:?}");
    assert!(!staged.content.contains("+changed"), "{staged:?}");
}

#[test]
fn log_is_bounded_by_the_requested_limit() {
    let root = repository();
    fs::write(root.join("tracked.txt"), "second\n").unwrap();
    git(&root, &["commit", "--quiet", "-am", "second commit"]);

    let output = run(
        &root,
        GitReadInput::new(GitReadOperation::Log).with_limit(1),
    );

    assert!(!output.is_error, "{output:?}");
    assert!(output.content.contains("second commit"), "{output:?}");
    assert!(!output.content.contains("base commit"), "{output:?}");
}

#[test]
fn branch_merged_lists_only_branches_already_contained_in_the_base() {
    let root = repository();
    git(&root, &["branch", "landed"]);
    git(&root, &["checkout", "--quiet", "-b", "pending"]);
    fs::write(root.join("tracked.txt"), "pending\n").unwrap();
    git(&root, &["commit", "--quiet", "-am", "pending commit"]);
    git(&root, &["checkout", "--quiet", "main"]);

    let output = run(
        &root,
        GitReadInput::new(GitReadOperation::BranchMerged).with_base("main"),
    );

    assert!(!output.is_error, "{output:?}");
    assert!(output.content.contains("landed"), "{output:?}");
    assert!(!output.content.contains("pending"), "{output:?}");
}

#[test]
fn merge_base_resolves_the_common_ancestor_of_two_refs() {
    let root = repository();
    let ancestor = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&root)
        .output()
        .unwrap();
    let ancestor = String::from_utf8(ancestor.stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(&root, &["checkout", "--quiet", "-b", "topic"]);
    fs::write(root.join("tracked.txt"), "topic\n").unwrap();
    git(&root, &["commit", "--quiet", "-am", "topic commit"]);

    let output = run(
        &root,
        GitReadInput::new(GitReadOperation::MergeBase)
            .with_base("main")
            .with_head("topic"),
    );

    assert!(!output.is_error, "{output:?}");
    assert!(output.content.contains(&ancestor), "{output:?}");
}

#[test]
fn rejects_a_revision_that_would_smuggle_a_flag_into_the_argv() {
    let root = repository();
    let escape = root.join("written-by-git.txt");

    let output = run(
        &root,
        GitReadInput::new(GitReadOperation::Log)
            .with_base(format!("--output={}", escape.display())),
    );

    assert!(output.is_error, "{output:?}");
    assert!(!escape.exists(), "git wrote a file through a smuggled flag");
}

#[test]
fn rejects_revisions_carrying_range_or_shell_metacharacters() {
    let root = repository();

    for revision in [
        "main..topic",
        "main;touch owned",
        "main topic",
        "HEAD@{0}",
        "refs/heads/main:refs/heads/other",
        "",
    ] {
        let output = run(
            &root,
            GitReadInput::new(GitReadOperation::BranchMerged).with_base(revision),
        );
        assert!(output.is_error, "accepted revision {revision:?}");
    }
}

#[test]
fn repository_configuration_cannot_make_a_diff_execute_a_program() {
    let root = repository();
    let sentinel = root.join("external-diff-ran.txt");
    let script = executable_script(&root, "external-diff.sh", &sentinel);
    git(
        &root,
        &["config", "diff.external", script.to_str().unwrap()],
    );
    fs::write(root.join("tracked.txt"), "changed\n").unwrap();

    let output = run(&root, GitReadInput::new(GitReadOperation::Diff));

    assert!(!output.is_error, "{output:?}");
    assert!(
        !sentinel.exists(),
        "repository configuration executed a program during a read"
    );
}

/// A content change does not make git refresh stat information, so the mutation
/// that removes `--no-optional-locks` survives it. Touching the file without
/// changing it does, which is what makes this a real guard.
#[test]
fn status_does_not_write_the_repository_index() {
    let root = repository();
    let index = root.join(".git").join("index");
    assert!(!run(&root, GitReadInput::new(GitReadOperation::Status)).is_error);

    std::thread::sleep(std::time::Duration::from_millis(1100));
    fs::write(root.join("tracked.txt"), "base\n").unwrap();
    let before = fs::read(&index).unwrap();

    assert!(!run(&root, GitReadInput::new(GitReadOperation::Status)).is_error);

    assert_eq!(before, fs::read(&index).unwrap(), "status wrote the index");
}

#[test]
fn repository_configuration_cannot_make_status_execute_a_program() {
    let root = repository();
    let sentinel = root.join("fsmonitor-ran.txt");
    let script = executable_script(&root, "fsmonitor.sh", &sentinel);
    git(
        &root,
        &["config", "core.fsmonitor", script.to_str().unwrap()],
    );

    let output = run(&root, GitReadInput::new(GitReadOperation::Status));

    assert!(!output.is_error, "{output:?}");
    assert!(
        !sentinel.exists(),
        "repository configuration executed a program during a read"
    );
}

#[test]
fn branch_merged_and_merge_base_require_their_revisions() {
    let root = repository();

    assert!(run(&root, GitReadInput::new(GitReadOperation::BranchMerged)).is_error);
    assert!(
        run(
            &root,
            GitReadInput::new(GitReadOperation::MergeBase).with_base("main")
        )
        .is_error
    );
}

#[test]
fn reports_a_failure_when_the_root_is_not_a_repository() {
    let root = project_root();

    let output = run(&root, GitReadInput::new(GitReadOperation::Status));

    assert!(output.is_error, "{output:?}");
}

#[test]
fn catalog_exposes_git_read_as_a_read_only_tool_with_an_enumerated_operation() {
    let metadata = NativeToolCatalog::metadata();

    let git_read = metadata
        .iter()
        .find(|tool| tool.qualified_name == "native::git_read")
        .expect("git_read metadata");

    assert_eq!(git_read.access, ToolAccess::ReadOnly);
    assert_eq!(git_read.input_schema["required"], json!(["operation"]));
    assert_eq!(
        git_read.input_schema["properties"]["operation"]["enum"],
        json!(["status", "diff", "log", "branch_merged", "merge_base"])
    );
    assert_eq!(git_read.input_schema["additionalProperties"], json!(false));
}

#[test]
fn catalog_dispatches_git_read_and_rejects_an_unknown_operation() {
    let root = repository();
    let mut catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());
    let context = ToolExecutionContext::with_timeout(Duration::from_secs(5));

    let status = catalog
        .execute("native::git_read", json!({"operation": "status"}), &context)
        .unwrap();
    assert!(!status.is_error, "{status:?}");

    let unknown = catalog
        .execute("native::git_read", json!({"operation": "push"}), &context)
        .unwrap();
    assert!(unknown.is_error, "{unknown:?}");
}
