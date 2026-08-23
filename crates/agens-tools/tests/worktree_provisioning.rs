//! Provisioning of a freshly created session worktree from the contract the
//! repository itself declares.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agens_tools::{
    HookAuthorization, HookAuthorizationRequest, HookFailure, HookFailureResponse,
    ProvisioningDecisions, ProvisioningError, ProvisioningOutcome, ProvisioningRequest,
    SessionWorktrees, WorktreeProvisioner,
};

static NEXT_REPOSITORY: AtomicUsize = AtomicUsize::new(0);

const REPOSITORY_ID: &str = "repo-a1b2c3d4";
const WORKTREE_NAME: &str = "session-one";
const BRANCH: &str = "agens/session-one";

struct Repository {
    root: PathBuf,
    checkout: PathBuf,
    data_directory: PathBuf,
    /// The environment names a hook of this repository may export.
    exports: Vec<String>,
}

impl Repository {
    fn new() -> Self {
        let suffix = NEXT_REPOSITORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "agens-tools-provisioning-{}-{suffix}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let checkout = root.join("repository");
        let data_directory = root.join("data");

        std::fs::create_dir_all(&checkout).expect("create repository directory");
        git(&checkout, &["init", "--quiet", "--initial-branch=main"]);
        git(&checkout, &["config", "user.name", "Agens Test"]);
        git(&checkout, &["config", "user.email", "agens-test@localhost"]);
        std::fs::write(checkout.join(".gitignore"), "/ignored\n/.env\n/build/\n")
            .expect("write gitignore");
        std::fs::write(checkout.join("tracked.txt"), "initial\n").expect("write tracked file");
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "--quiet", "-m", "initial"]);

        Self {
            root,
            checkout,
            data_directory,
            exports: Vec::new(),
        }
    }

    /// Grants these exported names, the way an operator's policy does.
    fn exporting(mut self, names: &[&str]) -> Self {
        self.exports = names.iter().map(|name| (*name).to_owned()).collect();
        self
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.checkout.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(path, contents).expect("write repository file");
    }

    fn declare(&self, contract: &str) {
        std::fs::create_dir_all(self.checkout.join(".agens")).expect("create .agens");
        std::fs::write(self.checkout.join(".agens/worktree.toml"), contract)
            .expect("write contract");
    }

    fn worktrees(&self) -> SessionWorktrees {
        SessionWorktrees::new(&self.data_directory)
    }

    fn create_worktree(&self) -> PathBuf {
        self.worktrees()
            .create(&self.checkout, REPOSITORY_ID, WORKTREE_NAME, BRANCH, "HEAD")
            .expect("create worktree")
    }

    fn provision(&self, decisions: &dyn ProvisioningDecisions) -> ProvisioningOutcome {
        self.try_provision(decisions).expect("provisioning runs")
    }

    fn try_provision(
        &self,
        decisions: &dyn ProvisioningDecisions,
    ) -> Result<ProvisioningOutcome, ProvisioningError> {
        WorktreeProvisioner::new(self.worktrees())
            .with_export_allowlist(self.exports.clone())
            .provision(
                &ProvisioningRequest {
                    repository: &self.checkout,
                    repository_id: REPOSITORY_ID,
                    name: WORKTREE_NAME,
                    branch: BRANCH,
                },
                decisions,
            )
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

/// A caller that allows every hook and continues past every failure, so a test
/// that is not about the decisions does not have to spell them out.
struct Permissive {
    authorizations: Mutex<Vec<Vec<Vec<String>>>>,
    failures: Mutex<Vec<String>>,
}

impl Permissive {
    fn new() -> Self {
        Self {
            authorizations: Mutex::new(Vec::new()),
            failures: Mutex::new(Vec::new()),
        }
    }

    fn authorized_commands(&self) -> Vec<Vec<Vec<String>>> {
        self.authorizations.lock().expect("lock").clone()
    }

    fn failed_hooks(&self) -> Vec<String> {
        self.failures.lock().expect("lock").clone()
    }
}

impl ProvisioningDecisions for Permissive {
    fn authorize(&self, request: &HookAuthorizationRequest<'_>) -> HookAuthorization {
        self.authorizations.lock().expect("lock").push(
            request
                .hooks
                .iter()
                .map(|hook| hook.command.clone())
                .collect(),
        );
        HookAuthorization::Allow
    }

    fn on_hook_failure(&self, failure: &HookFailure) -> HookFailureResponse {
        self.failures
            .lock()
            .expect("lock")
            .push(failure.name.clone());
        HookFailureResponse::Continue
    }
}

struct Refusing;

impl ProvisioningDecisions for Refusing {
    fn authorize(&self, _request: &HookAuthorizationRequest<'_>) -> HookAuthorization {
        HookAuthorization::Deny
    }

    fn on_hook_failure(&self, _failure: &HookFailure) -> HookFailureResponse {
        HookFailureResponse::Abort
    }
}

struct Aborting;

impl ProvisioningDecisions for Aborting {
    fn authorize(&self, _request: &HookAuthorizationRequest<'_>) -> HookAuthorization {
        HookAuthorization::Allow
    }

    fn on_hook_failure(&self, _failure: &HookFailure) -> HookFailureResponse {
        HookFailureResponse::Abort
    }
}

fn applied(outcome: ProvisioningOutcome) -> agens_tools::ProvisioningReport {
    match outcome {
        ProvisioningOutcome::Applied(report) => report,
        other => panic!("expected provisioning to be applied, got {other:?}"),
    }
}

fn copied_paths(report: &agens_tools::ProvisioningReport) -> Vec<String> {
    report
        .copied
        .iter()
        .map(|path| path.display().to_string())
        .collect()
}

#[test]
fn a_repository_that_declares_nothing_is_left_exactly_as_git_created_it() {
    let repository = Repository::new();
    repository.write(".env", "SECRET=1\n");
    let worktree = repository.create_worktree();

    let outcome = repository.provision(&Permissive::new());

    assert!(
        matches!(outcome, ProvisioningOutcome::NotDeclared),
        "an undeclared contract must not provision anything: {outcome:?}"
    );
    assert!(!worktree.join(".env").exists());
}

#[test]
fn only_files_git_reports_as_ignored_and_untracked_are_copied() {
    let repository = Repository::new();
    repository.write(".env", "SECRET=1\n");
    repository.write("ignored/fixture.bin", "generated\n");
    repository.write("build/artifact", "artifact\n");
    repository.write("untracked.txt", "not ignored\n");
    repository.declare(
        r#"
include = """
.env
ignored/
untracked.txt
tracked.txt
"""
"#,
    );

    let worktree = repository.create_worktree();
    let report = applied(repository.provision(&Permissive::new()));

    assert_eq!(
        copied_paths(&report),
        vec![".env".to_owned(), "ignored/fixture.bin".to_owned()]
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join(".env")).expect("copied env"),
        "SECRET=1\n"
    );
    assert!(
        !worktree.join("untracked.txt").exists(),
        "an untracked file git does not ignore is not eligible"
    );
    assert!(
        !worktree.join("build/artifact").exists(),
        "an ignored file the contract does not name is not eligible"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("tracked.txt")).expect("tracked file"),
        "initial\n",
        "a tracked file arrives through git and is never re-copied"
    );
}

#[test]
fn an_existing_destination_is_never_overwritten() {
    let repository = Repository::new();
    repository.write(".env", "SECRET=1\n");
    repository.declare("include = \".env\"\n");

    let worktree = repository.create_worktree();
    std::fs::write(worktree.join(".env"), "ALREADY=here\n").expect("write destination");

    let report = applied(repository.provision(&Permissive::new()));

    assert!(
        report.copied.is_empty(),
        "a destination that already exists is not recorded as created: {:?}",
        report.copied
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join(".env")).expect("destination"),
        "ALREADY=here\n"
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_directory_is_recreated_as_a_link_and_never_followed() {
    let repository = Repository::new();
    std::fs::create_dir_all(repository.checkout.join("outside")).expect("create outside");
    std::fs::write(repository.checkout.join("outside/secret.txt"), "secret\n")
        .expect("write outside");
    std::os::unix::fs::symlink("outside", repository.checkout.join("ignored"))
        .expect("symlink directory");
    repository.declare("include = \"ignored\"\n");

    let worktree = repository.create_worktree();
    let report = applied(repository.provision(&Permissive::new()));

    assert_eq!(copied_paths(&report), vec!["ignored".to_owned()]);
    let link = worktree.join("ignored");
    assert!(
        link.symlink_metadata()
            .expect("destination link")
            .file_type()
            .is_symlink(),
        "the destination must stay a link"
    );
    assert_eq!(
        std::fs::read_link(&link).expect("link target"),
        Path::new("outside")
    );
    assert!(
        !worktree.join("outside/secret.txt").exists(),
        "the walk must not descend through a symlinked directory"
    );
}

#[cfg(unix)]
#[test]
fn the_mode_of_a_copied_file_is_preserved() {
    use std::os::unix::fs::PermissionsExt;

    let repository = Repository::new();
    repository.write("ignored/setup.sh", "#!/bin/sh\nexit 0\n");
    std::fs::set_permissions(
        repository.checkout.join("ignored/setup.sh"),
        std::fs::Permissions::from_mode(0o750),
    )
    .expect("set source mode");
    repository.declare("include = \"ignored/\"\n");

    let worktree = repository.create_worktree();
    applied(repository.provision(&Permissive::new()));

    let mode = std::fs::metadata(worktree.join("ignored/setup.sh"))
        .expect("copied file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o750);
}

#[test]
fn hooks_run_in_the_declared_order_once_the_caller_authorizes_them() {
    let repository = Repository::new();
    repository.declare(
        r#"
[[hooks]]
name = "first"
command = ["/bin/sh", "-c", "printf 'first\n' >> order.txt"]

[[hooks]]
name = "second"
command = ["/bin/sh", "-c", "printf 'second\n' >> order.txt"]
"#,
    );

    let worktree = repository.create_worktree();
    let decisions = Permissive::new();
    let report = applied(repository.provision(&decisions));

    assert!(report.hooks_authorized);
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(
        std::fs::read_to_string(worktree.join("order.txt")).expect("hook output"),
        "first\nsecond\n"
    );
    assert_eq!(
        decisions.authorized_commands(),
        vec![vec![
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "printf 'first\n' >> order.txt".to_owned()
            ],
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "printf 'second\n' >> order.txt".to_owned()
            ],
        ]],
        "the authorization request names exactly what will run"
    );
}

#[test]
fn a_declined_authorization_runs_no_hook_and_says_so() {
    let repository = Repository::new();
    repository.write(".env", "SECRET=1\n");
    repository.declare(
        r#"
include = ".env"

[[hooks]]
name = "setup"
command = ["/bin/sh", "-c", "printf 'ran\n' > ran.txt"]
"#,
    );

    let worktree = repository.create_worktree();
    let report = applied(repository.provision(&Refusing));

    assert!(!report.hooks_authorized);
    assert!(!worktree.join("ran.txt").exists(), "no hook may have run");
    assert_eq!(
        copied_paths(&report),
        vec![".env".to_owned()],
        "the inclusion list is declared data, not execution, and is applied regardless"
    );
}

#[test]
fn a_hook_inherits_the_environment_and_what_an_earlier_hook_exported() {
    let repository = Repository::new().exporting(&["CARGO_TARGET_DIR"]);
    repository.declare(
        r#"
[[hooks]]
name = "export"
command = ["/bin/sh", "-c", "printf 'CARGO_TARGET_DIR=/shared/target\n' >> \"$AGENS_WORKTREE_ENV\""]

[[hooks]]
name = "observe"
command = ["/bin/sh", "-c", "printf '%s|%s\n' \"$CARGO_TARGET_DIR\" \"$AGENS_PROVISIONING_PROBE\" > observed.txt"]
"#,
    );

    let worktree = repository.create_worktree();
    // SAFETY: the value is only read by the hook this test starts.
    unsafe {
        std::env::set_var("AGENS_PROVISIONING_PROBE", "inherited");
    }
    let report = applied(repository.provision(&Permissive::new()));
    unsafe {
        std::env::remove_var("AGENS_PROVISIONING_PROBE");
    }

    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(
        std::fs::read_to_string(worktree.join("observed.txt")).expect("hook output"),
        "/shared/target|inherited\n"
    );
    assert_eq!(
        report.environment,
        BTreeMap::from([("CARGO_TARGET_DIR".to_owned(), "/shared/target".to_owned())]),
        "an allowed export reaches the hooks that follow it"
    );
    assert!(report.dropped_exports.is_empty());
}

#[test]
fn an_export_the_caller_never_allowed_is_dropped_rather_than_inherited() {
    let repository = Repository::new().exporting(&["CARGO_TARGET_DIR"]);
    repository.declare(
        r#"
[[hooks]]
name = "export"
command = ["/bin/sh", "-c", "printf 'PATH=/tmp/hostile\nLD_PRELOAD=/tmp/hostile.so\nCARGO_TARGET_DIR=/shared/target\n' >> \"$AGENS_WORKTREE_ENV\""]

[[hooks]]
name = "observe"
command = ["/bin/sh", "-c", "printf '%s|%s\n' \"$LD_PRELOAD\" \"$CARGO_TARGET_DIR\" > observed.txt"]
"#,
    );

    let worktree = repository.create_worktree();
    let report = applied(repository.provision(&Permissive::new()));

    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(
        report.environment,
        BTreeMap::from([("CARGO_TARGET_DIR".to_owned(), "/shared/target".to_owned())]),
        "only the granted name survives"
    );
    assert_eq!(
        report.dropped_exports,
        vec!["LD_PRELOAD".to_owned(), "PATH".to_owned()],
        "what was refused is reported rather than silently discarded"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("observed.txt")).expect("hook output"),
        "|/shared/target
",
        "a refused export never reaches the hook that follows, so it cannot decide \
         what that hook runs"
    );
}

#[test]
fn an_allowlist_entry_ending_in_a_star_admits_every_name_under_that_prefix() {
    let repository = Repository::new().exporting(&["AGENS_RUN_*"]);
    repository.declare(
        r#"
[[hooks]]
name = "export"
command = ["/bin/sh", "-c", "printf 'AGENS_RUN_MODE=fast\nAGENS_RUNTIME=other\n' >> \"$AGENS_WORKTREE_ENV\""]
"#,
    );

    repository.create_worktree();
    let report = applied(repository.provision(&Permissive::new()));

    assert_eq!(
        report.environment,
        BTreeMap::from([("AGENS_RUN_MODE".to_owned(), "fast".to_owned())]),
        "the prefix is a whole prefix, not a substring"
    );
    assert_eq!(report.dropped_exports, vec!["AGENS_RUNTIME".to_owned()]);
}

/// The allowlist exists to keep `PATH` and `LD_PRELOAD` out of the environment
/// the following hooks execute in. An entry with nothing before its `*` admits
/// exactly those, so it is rejected when the allowlist is set rather than
/// carried into the match.
#[test]
fn an_allowlist_entry_that_would_admit_every_name_is_rejected() {
    let repository = Repository::new().exporting(&["*", "", "AGENS_RUN_*"]);
    repository.declare(
        r#"
[[hooks]]
name = "export"
command = ["/bin/sh", "-c", "printf 'PATH=/evil\nLD_PRELOAD=/evil.so\nAGENS_RUN_MODE=fast\n' >> \"$AGENS_WORKTREE_ENV\""]
"#,
    );

    repository.create_worktree();
    let report = applied(repository.provision(&Permissive::new()));

    assert_eq!(
        report.environment,
        BTreeMap::from([("AGENS_RUN_MODE".to_owned(), "fast".to_owned())])
    );
    assert_eq!(
        report.dropped_exports,
        vec!["LD_PRELOAD".to_owned(), "PATH".to_owned()]
    );
}

#[test]
fn a_failure_the_caller_continues_past_is_recorded_for_the_run_preamble() {
    let repository = Repository::new();
    repository.declare(
        r#"
[[hooks]]
name = "broken"
command = ["/bin/sh", "-c", "printf 'no toolchain\n' >&2; exit 3"]

[[hooks]]
name = "later"
command = ["/bin/sh", "-c", "printf 'ran\n' > ran.txt"]
"#,
    );

    let worktree = repository.create_worktree();
    let decisions = Permissive::new();
    let report = applied(repository.provision(&decisions));

    assert_eq!(decisions.failed_hooks(), vec!["broken".to_owned()]);
    assert_eq!(report.failures.len(), 1);
    let failure = report.failures.first().expect("recorded failure");
    assert_eq!(failure.name, "broken");
    assert_eq!(failure.position, 1);
    assert!(
        failure.output.contains("no toolchain"),
        "the recorded failure has to explain itself: {}",
        failure.output
    );
    assert!(
        worktree.join("ran.txt").exists(),
        "continuing means the remaining hooks still run"
    );
}

#[test]
fn a_failure_the_caller_aborts_on_removes_the_worktree_and_its_branch() {
    let repository = Repository::new();
    repository.write(".env", "SECRET=1\n");
    repository.declare(
        r#"
include = ".env"

[[hooks]]
name = "broken"
command = ["/bin/sh", "-c", "exit 1"]
"#,
    );

    let worktree = repository.create_worktree();
    let outcome = repository.provision(&Aborting);

    let ProvisioningOutcome::Aborted(failure) = outcome else {
        panic!("expected an abort, got {outcome:?}");
    };
    assert_eq!(failure.name, "broken");
    assert!(!worktree.exists(), "the worktree survived an abort");
    let branches = git(&repository.checkout, &["branch", "--list", BRANCH]);
    assert!(
        branches.trim().is_empty(),
        "the branch survived an abort: {branches}"
    );
}

#[test]
fn a_hook_that_outlives_its_declared_timeout_fails_without_hanging_the_session() {
    let repository = Repository::new();
    repository.declare(
        r#"
[[hooks]]
name = "hangs"
command = ["/bin/sh", "-c", "sleep 30"]
timeout_seconds = 1
"#,
    );

    repository.create_worktree();
    let report = applied(repository.provision(&Permissive::new()));

    let failure = report.failures.first().expect("recorded failure");
    assert_eq!(failure.name, "hangs");
    assert!(
        failure.output.contains("timed out"),
        "a timeout must not read as an ordinary non-zero exit: {}",
        failure.output
    );
}

#[test]
fn a_contract_declared_in_both_supported_places_is_rejected_rather_than_shadowed() {
    let repository = Repository::new();
    repository.declare("include = \".env\"\n");
    std::fs::write(
        repository.checkout.join(".agens-worktree.toml"),
        "include = \"other\"\n",
    )
    .expect("write root contract");

    repository.create_worktree();
    let error = repository
        .try_provision(&Permissive::new())
        .expect_err("an ambiguous declaration is an error");

    assert!(
        matches!(error, ProvisioningError::AmbiguousContract { .. }),
        "{error:?}"
    );
}

#[test]
fn a_hook_without_a_command_is_rejected_before_anything_runs() {
    let repository = Repository::new();
    repository.declare(
        r#"
[[hooks]]
name = "empty"
command = []
"#,
    );

    repository.create_worktree();
    let error = repository
        .try_provision(&Permissive::new())
        .expect_err("an empty command is an error");

    assert!(
        matches!(error, ProvisioningError::Contract { .. }),
        "{error:?}"
    );
}

#[test]
fn the_authorization_request_states_that_hooks_inherit_the_daemon_credentials() {
    struct Inspecting {
        seen: Mutex<Option<(PathBuf, bool, Vec<String>)>>,
    }

    impl ProvisioningDecisions for Inspecting {
        fn authorize(&self, request: &HookAuthorizationRequest<'_>) -> HookAuthorization {
            *self.seen.lock().expect("lock") = Some((
                request.contract.to_path_buf(),
                request.inherits_credentials,
                request.hooks.iter().map(|hook| hook.name.clone()).collect(),
            ));
            HookAuthorization::Deny
        }

        fn on_hook_failure(&self, _failure: &HookFailure) -> HookFailureResponse {
            HookFailureResponse::Abort
        }
    }

    let repository = Repository::new();
    repository.declare(
        r#"
[[hooks]]
name = "devshell"
command = ["/bin/sh", "-c", "true"]
"#,
    );

    repository.create_worktree();
    let decisions = Inspecting {
        seen: Mutex::new(None),
    };
    repository.provision(&decisions);

    let (contract, inherits_credentials, names) =
        decisions.seen.lock().expect("lock").clone().expect("asked");
    assert_eq!(contract, repository.checkout.join(".agens/worktree.toml"));
    assert!(
        inherits_credentials,
        "the caller has to be told the hook receives the daemon environment"
    );
    assert_eq!(names, vec!["devshell".to_owned()]);
}

#[test]
fn a_contract_without_hooks_never_asks_for_authorization() {
    let repository = Repository::new();
    repository.write(".env", "SECRET=1\n");
    repository.declare("include = \".env\"\n");

    repository.create_worktree();
    let decisions = Permissive::new();
    let report = applied(repository.provision(&decisions));

    assert!(decisions.authorized_commands().is_empty());
    assert!(report.hooks_authorized, "there was nothing to withhold");
    assert_eq!(copied_paths(&report), vec![".env".to_owned()]);
}

#[test]
fn a_hook_runs_inside_the_worktree_and_not_in_the_source_checkout() {
    let repository = Repository::new();
    repository.declare(
        r#"
[[hooks]]
name = "where"
command = ["/bin/sh", "-c", "printf '%s\n' \"$AGENS_WORKTREE\" > where.txt"]
"#,
    );

    let worktree = repository.create_worktree();
    applied(repository.provision(&Permissive::new()));

    assert!(!repository.checkout.join("where.txt").exists());
    assert_eq!(
        std::fs::read_to_string(worktree.join("where.txt"))
            .expect("hook output")
            .trim(),
        worktree.display().to_string()
    );
}

#[test]
fn provisioning_a_worktree_that_is_not_there_fails_before_any_hook() {
    let repository = Repository::new();
    repository.declare(
        r#"
[[hooks]]
name = "setup"
command = ["/bin/sh", "-c", "true"]
"#,
    );

    let error = repository
        .try_provision(&Permissive::new())
        .expect_err("a missing worktree is an error");

    assert!(matches!(error, ProvisioningError::Missing), "{error:?}");
}

#[test]
fn a_hook_may_be_bounded_by_the_caller_rather_than_by_the_contract() {
    let repository = Repository::new();
    repository.declare(
        r#"
[[hooks]]
name = "hangs"
command = ["/bin/sh", "-c", "sleep 30"]
"#,
    );

    repository.create_worktree();
    let report = match WorktreeProvisioner::new(repository.worktrees())
        .with_hook_timeout(Duration::from_secs(1))
        .provision(
            &ProvisioningRequest {
                repository: &repository.checkout,
                repository_id: REPOSITORY_ID,
                name: WORKTREE_NAME,
                branch: BRANCH,
            },
            &Permissive::new(),
        )
        .expect("provisioning runs")
    {
        ProvisioningOutcome::Applied(report) => report,
        other => panic!("expected provisioning to be applied, got {other:?}"),
    };

    assert_eq!(report.failures.len(), 1);
}
