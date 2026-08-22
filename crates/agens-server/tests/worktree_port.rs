//! The daemon's own worktree seam, over a real checkout.
//!
//! What is worth proving here is not that git creates a directory. It is the
//! two decisions the daemon makes about repository code: whether a declared
//! hook runs at all, which the core settles and this port only obeys, and what
//! a hook that ran is allowed to say to the rest of the system.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use agens_server::{GitWorktreeGate, HookPolicy, WorktreeGate, WorktreeRequest};
use agens_tools::SessionWorktrees;

static NEXT_REPOSITORY: AtomicUsize = AtomicUsize::new(0);

const REPO_ID: &str = "repo-a1b2c3d4";
const NAME: &str = "session-one";
const BRANCH: &str = "agens/session-one";

/// A checkout with a contract, and the data directory its worktrees live under.
struct Fixture {
    root: PathBuf,
    checkout: PathBuf,
    data_directory: PathBuf,
}

impl Fixture {
    fn new(contract: &str) -> Self {
        let suffix = NEXT_REPOSITORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "agens-server-worktree-port-{}-{suffix}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let checkout = root.join("repository");
        std::fs::create_dir_all(&checkout).expect("create the checkout");

        run_git(&checkout, &["init", "--quiet", "--initial-branch=main"]);
        run_git(&checkout, &["config", "user.name", "Agens Test"]);
        run_git(&checkout, &["config", "user.email", "agens-test@localhost"]);
        std::fs::write(checkout.join("tracked.txt"), "initial\n").expect("write a tracked file");
        std::fs::create_dir_all(checkout.join(".agens")).expect("create .agens");
        std::fs::write(checkout.join(".agens/worktree.toml"), contract)
            .expect("write the contract");
        run_git(&checkout, &["add", "."]);
        run_git(&checkout, &["commit", "--quiet", "-m", "initial"]);

        let data_directory = root.join("data");

        Self {
            root,
            checkout,
            data_directory,
        }
    }

    fn provision(&self, hooks: HookPolicy) -> agens_server::ProvisionedWorktree {
        let gate = GitWorktreeGate::new(
            SessionWorktrees::new(&self.data_directory),
            "main",
            Vec::new(),
        );

        gate.provision(&WorktreeRequest {
            repository: &self.checkout,
            repo_id: REPO_ID,
            name: NAME,
            branch: BRANCH,
            start_point: "HEAD",
            hooks,
        })
        .expect("the worktree is created and its contract applied")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run_git(directory: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .status()
        .expect("git runs");

    assert!(status.success(), "git {arguments:?}");
}

/// A hook that prints a provider key and a host path, and fails so that what it
/// printed travels.
const LEAKY_CONTRACT: &str = r#"
[[hooks]]
name = "devshell"
command = ["/bin/sh", "-c", "printf 'OPENAI_API_KEY=sk-live0123456789abcdef under /home/someone/private\n'; exit 3"]
"#;

#[test]
fn a_failing_hooks_output_reaches_the_caller_with_its_credentials_and_paths_removed() {
    let fixture = Fixture::new(LEAKY_CONTRACT);

    let provisioned = fixture.provision(HookPolicy::Allow);

    let failure = provisioned
        .hook_failures
        .first()
        .expect("a hook that exited three failed");

    assert!(
        failure.starts_with("devshell: "),
        "the record names the hook that left the environment half-built: {failure}"
    );
    assert!(
        !failure.contains("sk-live0123456789abcdef"),
        "a hook's own output is repository code's output, and it printed a key: {failure}"
    );
    assert!(
        !failure.contains("/home/someone/private"),
        "the daemon's host paths do not travel with it either: {failure}"
    );
}

#[test]
fn a_hook_the_core_denied_does_not_run_and_is_still_reported_as_declared() {
    let fixture = Fixture::new(LEAKY_CONTRACT);

    let provisioned = fixture.provision(HookPolicy::Deny);

    assert!(
        !provisioned.hooks_ran,
        "a denied contract executes nothing the repository declared"
    );
    assert!(provisioned.hook_failures.is_empty());
    assert_eq!(
        provisioned.declared_hooks,
        vec!["devshell".to_owned()],
        "what was on offer is what the operator is eventually asked about"
    );
}

#[test]
fn asking_runs_nothing_either_because_nobody_has_answered_yet() {
    let fixture = Fixture::new(LEAKY_CONTRACT);

    let provisioned = fixture.provision(HookPolicy::Ask);

    assert!(!provisioned.hooks_ran);
    assert_eq!(provisioned.declared_hooks, vec!["devshell".to_owned()]);
}

#[test]
fn a_repository_declaring_no_hooks_reports_none_whatever_the_policy_says() {
    let fixture = Fixture::new("include = \"\"\n");

    let provisioned = fixture.provision(HookPolicy::Allow);

    assert!(provisioned.declared_hooks.is_empty());
    assert!(
        !provisioned.hooks_ran,
        "there was nothing to run, which is not the same as having run something"
    );
}
