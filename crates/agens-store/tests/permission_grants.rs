use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_core::{PermissionDecision, PermissionPattern, ProjectPermissionGrant};
use agens_store::PermissionGrantStore;
use rusqlite::Connection;

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

    let database = directory.join("rust-permissions.db");
    assert_eq!(
        Connection::open(&database)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );

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
        " ",
        PermissionPattern::Exact("native::edit".into()),
        PermissionPattern::Any,
    );

    assert!(store.append_grants(&[valid, invalid]).is_err());
    assert!(store.grants_for_project("project-a").unwrap().is_empty());
    assert!(store.grants_for_project(" ").is_err());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_missing_project_lookup_and_unsupported_schema_versions_with_actionable_context() {
    let directory = data_directory();
    let store = PermissionGrantStore::open(&directory).unwrap();
    assert!(store.grants_for_project("").is_err());
    drop(store);

    let database = directory.join("rust-permissions.db");
    Connection::open(&database)
        .unwrap()
        .pragma_update(None, "user_version", 999)
        .unwrap();

    let error = PermissionGrantStore::open(&directory)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("schema version"));
    assert!(error.contains(database.to_string_lossy().as_ref()));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_supported_version_without_the_expected_schema() {
    let directory = data_directory();
    let database = directory.join("rust-permissions.db");
    Connection::open(&database)
        .unwrap()
        .pragma_update(None, "user_version", 1)
        .unwrap();

    let error = PermissionGrantStore::open(&directory)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("verify schema"));
    assert!(error.contains(database.to_string_lossy().as_ref()));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_version_one_databases_with_incompatible_permission_grant_contracts() {
    let incompatible_schemas = [
        (
            "wrong column affinity",
            "CREATE TABLE permission_grants (
                id INTEGER PRIMARY KEY,
                project BLOB NOT NULL,
                decision TEXT NOT NULL,
                tool_kind TEXT NOT NULL,
                tool_value TEXT,
                target_kind TEXT NOT NULL,
                target_value TEXT
            );
            CREATE INDEX permission_grants_project ON permission_grants(project, id);",
        ),
        (
            "nullable required column",
            "CREATE TABLE permission_grants (
                id INTEGER PRIMARY KEY,
                project TEXT,
                decision TEXT NOT NULL,
                tool_kind TEXT NOT NULL,
                tool_value TEXT,
                target_kind TEXT NOT NULL,
                target_value TEXT
            );
            CREATE INDEX permission_grants_project ON permission_grants(project, id);",
        ),
        (
            "missing primary key",
            "CREATE TABLE permission_grants (
                id INTEGER NOT NULL,
                project TEXT NOT NULL,
                decision TEXT NOT NULL,
                tool_kind TEXT NOT NULL,
                tool_value TEXT,
                target_kind TEXT NOT NULL,
                target_value TEXT
            );
            CREATE INDEX permission_grants_project ON permission_grants(project, id);",
        ),
        (
            "unexpected required default",
            "CREATE TABLE permission_grants (
                id INTEGER PRIMARY KEY,
                project TEXT NOT NULL DEFAULT '',
                decision TEXT NOT NULL,
                tool_kind TEXT NOT NULL,
                tool_value TEXT,
                target_kind TEXT NOT NULL,
                target_value TEXT
            );
            CREATE INDEX permission_grants_project ON permission_grants(project, id);",
        ),
        (
            "incompatible index",
            "CREATE TABLE permission_grants (
                id INTEGER PRIMARY KEY,
                project TEXT NOT NULL,
                decision TEXT NOT NULL,
                tool_kind TEXT NOT NULL,
                tool_value TEXT,
                target_kind TEXT NOT NULL,
                target_value TEXT
            );
            CREATE UNIQUE INDEX permission_grants_project ON permission_grants(project);",
        ),
        (
            "unexpected column",
            "CREATE TABLE permission_grants (
                id INTEGER PRIMARY KEY,
                project TEXT NOT NULL,
                decision TEXT NOT NULL,
                tool_kind TEXT NOT NULL,
                tool_value TEXT,
                target_kind TEXT NOT NULL,
                target_value TEXT,
                expires_at TEXT
            );
            CREATE INDEX permission_grants_project ON permission_grants(project, id);",
        ),
        (
            "missing nullable column",
            "CREATE TABLE permission_grants (
                id INTEGER PRIMARY KEY,
                project TEXT NOT NULL,
                decision TEXT NOT NULL,
                tool_kind TEXT NOT NULL,
                tool_value TEXT,
                target_kind TEXT NOT NULL
            );
            CREATE INDEX permission_grants_project ON permission_grants(project, id);",
        ),
        (
            "unexpected index",
            "CREATE TABLE permission_grants (
                id INTEGER PRIMARY KEY,
                project TEXT NOT NULL,
                decision TEXT NOT NULL,
                tool_kind TEXT NOT NULL,
                tool_value TEXT,
                target_kind TEXT NOT NULL,
                target_value TEXT
            );
            CREATE INDEX permission_grants_project ON permission_grants(project, id);
            CREATE INDEX permission_grants_decision ON permission_grants(decision);",
        ),
    ];

    for (name, schema) in incompatible_schemas {
        let directory = data_directory();
        let database = directory.join("rust-permissions.db");
        let connection = Connection::open(&database).unwrap();
        connection.execute_batch(schema).unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        drop(connection);

        let error = match PermissionGrantStore::open(&directory) {
            Ok(_) => panic!("{name}: incompatible schema opened successfully"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("verify schema"), "{name}: {error}");
        assert!(
            error.contains(database.to_string_lossy().as_ref()),
            "{name}: {error}"
        );

        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn corrupt_database_open_failure_includes_operation_and_path() {
    let directory = data_directory();
    let database = directory.join("rust-permissions.db");
    fs::write(&database, "not a sqlite database").unwrap();

    let error = PermissionGrantStore::open(&directory)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("permission grants read schema version"));
    assert!(error.contains(database.to_string_lossy().as_ref()));

    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn creates_or_repairs_restrictive_unix_permissions_without_widening_safe_modes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let directory = data_directory();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    let store = PermissionGrantStore::open(&directory).unwrap();
    let database = store.database_path();

    assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(&database).unwrap().mode() & 0o777, 0o600);
    drop(store);

    fs::set_permissions(&database, fs::Permissions::from_mode(0o400)).unwrap();
    PermissionGrantStore::open(&directory).unwrap();
    assert_eq!(fs::metadata(&database).unwrap().mode() & 0o777, 0o400);

    fs::remove_dir_all(directory).unwrap();
}
