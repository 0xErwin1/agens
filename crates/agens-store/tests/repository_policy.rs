//! The register of what the operator decided about a repository's provisioning
//! hooks.
//!
//! It lives in the control plane so that the record of whose code may run with
//! the daemon's credentials is not a document under the data directory that a
//! run's own worktree could append its fingerprint to.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agens_store::{RepositoryPolicyStore, StoredHookTrust, StoredPendingTrust};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

const REPO: &str = "a1b2c3d4e5f60718";

fn data_directory() -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-store-repository-policy-{}-{suffix}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();

    directory
}

fn pending(question_id: i64) -> StoredPendingTrust {
    StoredPendingTrust {
        question_id,
        repo_id: REPO.to_owned(),
        repository: PathBuf::from("/srv/checkouts/agens"),
        asked_at: 1_700_000_000,
    }
}

#[test]
fn a_repository_nobody_decided_on_is_unknown_rather_than_trusted() {
    let store = RepositoryPolicyStore::open(data_directory()).unwrap();

    assert_eq!(store.hook_trust(REPO).unwrap(), StoredHookTrust::Unknown);
    assert!(!store.is_pending(1).unwrap());
}

#[test]
fn a_decision_survives_the_process_that_recorded_it() {
    let directory = data_directory();
    let mut store = RepositoryPolicyStore::open(&directory).unwrap();

    store
        .decide(REPO, &PathBuf::from("/srv/checkouts/agens"), true, 1)
        .unwrap();
    drop(store);

    let reopened = RepositoryPolicyStore::open(&directory).unwrap();

    assert_eq!(reopened.hook_trust(REPO).unwrap(), StoredHookTrust::Granted);
}

#[test]
fn a_later_decision_replaces_the_earlier_one_for_the_same_repository() {
    let mut store = RepositoryPolicyStore::open(data_directory()).unwrap();

    store
        .decide(REPO, &PathBuf::from("/srv/checkouts/agens"), true, 1)
        .unwrap();
    store
        .decide(REPO, &PathBuf::from("/srv/checkouts/agens"), false, 2)
        .unwrap();

    assert_eq!(
        store.hook_trust(REPO).unwrap(),
        StoredHookTrust::Refused,
        "a repository holds one decision, not a history the reader has to pick from"
    );
}

#[test]
fn answering_a_recorded_question_decides_the_repository_and_closes_the_question() {
    let mut store = RepositoryPolicyStore::open(data_directory()).unwrap();

    store.record_pending(&pending(7)).unwrap();

    assert!(store.is_pending(7).unwrap());
    assert!(store.resolve_pending(7, true).unwrap());
    assert_eq!(store.hook_trust(REPO).unwrap(), StoredHookTrust::Granted);
    assert!(
        !store.is_pending(7).unwrap(),
        "a question answered once is not answered again"
    );
}

#[test]
fn an_answer_to_a_question_the_register_never_recorded_decides_nothing() {
    let mut store = RepositoryPolicyStore::open(data_directory()).unwrap();

    assert!(!store.resolve_pending(11, true).unwrap());
    assert_eq!(store.hook_trust(REPO).unwrap(), StoredHookTrust::Unknown);
}

#[test]
fn a_decision_naming_no_repository_is_refused() {
    let mut store = RepositoryPolicyStore::open(data_directory()).unwrap();

    assert!(
        store
            .decide("  ", &PathBuf::from("/srv/checkouts/agens"), true, 1)
            .is_err()
    );
}

#[test]
fn a_second_handle_reads_what_the_first_one_wrote() {
    let directory = data_directory();
    let mut writer = RepositoryPolicyStore::open(&directory).unwrap();
    let reader = RepositoryPolicyStore::open(&directory).unwrap();

    writer
        .decide(REPO, &PathBuf::from("/srv/checkouts/agens"), true, 1)
        .unwrap();

    assert_eq!(
        reader.hook_trust(REPO).unwrap(),
        StoredHookTrust::Granted,
        "the operator grants from a second process and the daemon reads it without restarting"
    );
}
