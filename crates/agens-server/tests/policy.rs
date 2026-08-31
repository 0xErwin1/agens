//! The operator's decisions: which checkouts the daemon serves, whose hooks it
//! will execute, and what those hooks may export.
//!
//! Every default here is the closed one. A daemon that has never been
//! configured serves nothing and runs nobody's hooks, because the alternative
//! is a socket any local process can reach deciding to execute a repository's
//! code on the strength of the request naming it.
//!
//! The two halves are proved separately because they have different writers:
//! the roots come from the operator's configuration and nothing the daemon runs
//! can add to them, and the hook decisions live in the control plane, where a
//! run's own worktree has no file to append itself to.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agens_server::{
    HookTrust, PendingHookTrust, PolicySettings, PolicyStore, RepositoryPolicy, trust_repository,
};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

const REPO: &str = "a1b2c3d4e5f60718";

fn data_directory() -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-server-policy-{}-{suffix}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();

    directory
}

fn checkout(under: &std::path::Path, name: &str) -> PathBuf {
    let path = under.join(name);
    fs::create_dir_all(&path).unwrap();

    path.canonicalize().unwrap()
}

fn settings() -> PolicySettings {
    serving(Vec::new())
}

fn serving(roots: Vec<PathBuf>) -> PolicySettings {
    PolicySettings {
        project_roots: roots,
        hook_exports: Vec::new(),
        config_path: Some(PathBuf::from("/home/dev/.config/agens/config.toml")),
    }
}

/// A checkout with a git common directory, which is what the fingerprint the
/// register keys on is derived from.
fn repository(under: &std::path::Path, name: &str) -> PathBuf {
    let path = checkout(under, name);
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&path)
        .status()
        .expect("git is on the path");
    assert!(status.success(), "the fixture repository was initialized");

    path
}

#[test]
fn a_daemon_with_no_policy_serves_no_checkout_and_trusts_no_repository() {
    let directory = data_directory();
    let policy = PolicyStore::open(&directory, serving(Vec::new()))
        .expect("an absent policy is not an error");

    assert!(
        !policy.admits(&checkout(&directory, "anywhere")),
        "an unconfigured daemon admits nothing rather than everything"
    );
    assert_eq!(policy.hook_trust(REPO), HookTrust::Unknown);
    assert!(policy.hook_exports().is_empty());

    let remedy = policy.admission_remedy();
    assert!(
        remedy.contains("team.project_roots") && remedy.contains("config.toml"),
        "the refusal names the key and the file the operator has to write it in: {remedy}"
    );
}

#[test]
fn a_configured_root_admits_what_is_under_it_and_nothing_beside_it() {
    let directory = data_directory();
    let served = checkout(&directory, "dev");
    let inside = checkout(&served, "agens");
    let beside = checkout(&directory, "development");

    let policy = PolicyStore::open(&directory, serving(vec![served.clone()]))
        .expect("the configured roots need no file");

    assert!(policy.admits(&served), "the root itself is served");
    assert!(policy.admits(&inside), "so is a checkout under it");
    assert!(
        !policy.admits(&beside),
        "a name that merely starts with the root's is a different directory"
    );
}

#[test]
fn the_retired_policy_file_stops_the_daemon_rather_than_being_ignored() {
    let directory = data_directory();
    fs::write(
        directory.join("worktree-policy.toml"),
        "project_roots = [\"/srv/checkouts\"]\n",
    )
    .unwrap();

    let error = PolicyStore::open(&directory, serving(Vec::new()))
        .expect_err("a file that configures nothing is not silently obeyed");

    assert!(
        error.to_string().contains("team.project_roots"),
        "the refusal says where the roots moved to: {error}"
    );
}

#[cfg(unix)]
#[test]
fn a_register_anyone_can_reach_stops_the_daemon() {
    use std::os::unix::fs::PermissionsExt;

    let directory = data_directory();
    let policy = PolicyStore::open(&directory, serving(Vec::new())).expect("the register opens");
    drop(policy);

    let database = agens_store::unified_database_path(&directory);
    fs::set_permissions(&database, fs::Permissions::from_mode(0o666)).unwrap();

    let error = PolicyStore::open(&directory, serving(Vec::new()))
        .expect_err("a register the whole machine can write decides nothing");

    assert!(
        error.to_string().contains("beyond its owner"),
        "the refusal says what is wrong with the file: {error}"
    );
}

#[test]
fn answering_a_pending_question_grants_the_repository_durably() {
    let directory = data_directory();
    let repository = checkout(&directory, "agens");

    let policy = PolicyStore::open(&directory, serving(Vec::new())).unwrap();
    policy
        .record_pending(&PendingHookTrust {
            question_id: 7,
            repo_id: REPO.to_owned(),
            repository,
            asked_at: 1_700_000_000,
        })
        .unwrap();

    assert!(policy.is_pending(7));
    assert!(policy.resolve_pending(7, true).unwrap());
    assert_eq!(policy.hook_trust(REPO), HookTrust::Granted);
    assert!(
        !policy.is_pending(7),
        "a question answered once is not answered again"
    );

    drop(policy);
    let reopened = PolicyStore::open(&directory, serving(Vec::new()))
        .expect("the grant was written to the control plane");

    assert_eq!(
        reopened.hook_trust(REPO),
        HookTrust::Granted,
        "the grant outlives the daemon that recorded it"
    );
}

#[test]
fn a_refusal_is_recorded_so_the_operator_is_not_asked_again() {
    let directory = data_directory();
    let repository = checkout(&directory, "agens");

    let policy = PolicyStore::open(&directory, serving(Vec::new())).unwrap();
    policy
        .record_pending(&PendingHookTrust {
            question_id: 9,
            repo_id: REPO.to_owned(),
            repository,
            asked_at: 1_700_000_000,
        })
        .unwrap();

    assert!(policy.resolve_pending(9, false).unwrap());
    assert_eq!(policy.hook_trust(REPO), HookTrust::Refused);
}

#[test]
fn an_answer_to_a_question_the_policy_never_asked_grants_nothing() {
    let directory = data_directory();
    let policy = PolicyStore::open(&directory, serving(Vec::new())).unwrap();

    assert!(
        !policy.resolve_pending(11, true).unwrap(),
        "only a question the policy recorded can grant what that question was about"
    );
    assert_eq!(policy.hook_trust(REPO), HookTrust::Unknown);
}

#[test]
fn the_operators_trust_verb_grants_a_served_repository_without_a_question() {
    let directory = data_directory();
    let served = checkout(&directory, "dev");
    let repository = repository(&served, "agens");

    let trusted = trust_repository(
        &directory,
        serving(vec![served]),
        &repository,
        1_700_000_000,
    )
    .expect("a served checkout can be trusted");

    assert_eq!(trusted.repository, repository);

    let policy = PolicyStore::open(&directory, serving(Vec::new())).unwrap();

    assert_eq!(
        policy.hook_trust(&trusted.repo_id),
        HookTrust::Granted,
        "the daemon reads the grant the operator wrote"
    );
}

/// Runs admit any git checkout dynamically, so a grant against a checkout no
/// configuration ever named is a grant that applies to that repository's next
/// run — it is recorded, not refused.
#[test]
fn the_trust_verb_grants_a_repository_no_configuration_ever_named() {
    let directory = data_directory();
    let repository = repository(&directory, "elsewhere");

    let trusted = trust_repository(&directory, settings(), &repository, 1_700_000_000)
        .expect("an undeclared checkout can be trusted");

    let policy = PolicyStore::open(&directory, settings()).unwrap();

    assert_eq!(
        policy.hook_trust(&trusted.repo_id),
        HookTrust::Granted,
        "the grant applies to the repository the operator named"
    );
}

/// What the trust verb still refuses is a directory with no repository behind
/// it: a grant is keyed on a repository's identity, and a plain directory has
/// none.
#[test]
fn the_trust_verb_refuses_a_directory_that_is_not_a_git_worktree() {
    let directory = data_directory();
    let plain = checkout(&directory, "not-a-repo");

    let error = trust_repository(&directory, settings(), &plain, 1_700_000_000)
        .expect_err("a directory with no repository behind it cannot be trusted");

    assert!(
        error.to_string().contains("not a git worktree"),
        "the refusal names what the path is missing: {error}"
    );
}
