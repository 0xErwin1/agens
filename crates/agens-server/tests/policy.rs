//! The operator's own file: which checkouts the daemon serves, whose hooks it
//! will execute, and what those hooks may export.
//!
//! Every default here is the closed one. A daemon that has never been
//! configured serves nothing and runs nobody's hooks, because the alternative
//! is a socket any local process can reach deciding to execute a repository's
//! code on the strength of the request naming it.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agens_server::{HookTrust, PendingHookTrust, PolicyStore, RepositoryPolicy};

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

#[test]
fn a_daemon_with_no_policy_serves_no_checkout_and_trusts_no_repository() {
    let directory = data_directory();
    let policy = PolicyStore::open(&directory).expect("an absent policy is not an error");

    assert!(
        !policy.admits(&checkout(&directory, "anywhere")),
        "an unconfigured daemon admits nothing rather than everything"
    );
    assert_eq!(policy.hook_trust(REPO), HookTrust::Unknown);
    assert!(policy.hook_exports().is_empty());
    assert!(
        policy.admission_remedy().contains("project_roots"),
        "the refusal tells the operator what to write and where"
    );
}

#[test]
fn a_configured_root_admits_what_is_under_it_and_nothing_beside_it() {
    let directory = data_directory();
    let served = checkout(&directory, "dev");
    let inside = checkout(&served, "agens");
    let beside = checkout(&directory, "development");

    fs::write(
        directory.join("worktree-policy.toml"),
        format!("project_roots = [\"{}\"]\n", served.display()),
    )
    .unwrap();

    let policy = PolicyStore::open(&directory).expect("the policy parses");

    assert!(policy.admits(&served), "the root itself is served");
    assert!(policy.admits(&inside), "so is a checkout under it");
    assert!(
        !policy.admits(&beside),
        "a name that merely starts with the root's is a different directory"
    );
}

#[test]
fn a_policy_that_cannot_be_read_is_an_error_rather_than_an_empty_one() {
    let directory = data_directory();
    fs::write(directory.join("worktree-policy.toml"), "project_roots = \n").unwrap();

    assert!(
        PolicyStore::open(&directory).is_err(),
        "a typo becomes a daemon that refuses everything for a reason nobody can see"
    );
}

#[test]
fn answering_a_pending_question_grants_the_repository_durably() {
    let directory = data_directory();
    let repository = checkout(&directory, "agens");

    let policy = PolicyStore::open(&directory).unwrap();
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

    let reopened = PolicyStore::open(&directory).expect("the policy was written back");

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

    let policy = PolicyStore::open(&directory).unwrap();
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
    let policy = PolicyStore::open(&directory).unwrap();

    assert!(
        !policy.resolve_pending(11, true).unwrap(),
        "only a question the policy recorded can grant what that question was about"
    );
    assert_eq!(policy.hook_trust(REPO), HookTrust::Unknown);
}
