use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_core::{PermissionDecision, PermissionPattern, ProjectPermissionGrant};
use agens_store::PermissionGrantStore;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-store-permissions-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

#[test]
fn persists_only_project_scoped_grants_in_the_rust_permissions_database() {
    let directory = data_directory();
    let allow = ProjectPermissionGrant::allow(
        "project-a",
        PermissionPattern::Exact("native::edit".into()),
        PermissionPattern::Exact("src/lib.rs".into()),
    );
    let deny = ProjectPermissionGrant::new(
        "project-a",
        PermissionDecision::Deny,
        PermissionPattern::Exact("native::edit".into()),
        PermissionPattern::Exact("secrets.env".into()),
    );

    {
        let mut store = PermissionGrantStore::open(&directory).unwrap();
        store.append_grants(&[allow.clone(), deny.clone()]).unwrap();

        assert_eq!(store.database_path(), directory.join("rust-permissions.db"));
    }

    let reopened = PermissionGrantStore::open(&directory).unwrap();
    assert_eq!(
        reopened.grants_for_project("project-a").unwrap(),
        vec![allow, deny]
    );
    assert!(reopened.grants_for_project("project-b").unwrap().is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_non_project_scoped_grants_without_persisting_a_partial_write() {
    let directory = data_directory();
    let mut store = PermissionGrantStore::open(&directory).unwrap();
    let valid = ProjectPermissionGrant::allow(
        "project-a",
        PermissionPattern::Exact("native::edit".into()),
        PermissionPattern::Any,
    );
    let invalid = ProjectPermissionGrant::allow(
        "",
        PermissionPattern::Exact("native::edit".into()),
        PermissionPattern::Any,
    );

    assert!(store.append_grants(&[valid, invalid]).is_err());
    assert!(store.grants_for_project("project-a").unwrap().is_empty());

    fs::remove_dir_all(directory).unwrap();
}
