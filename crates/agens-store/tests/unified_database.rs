use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_store::PermissionGrantStore;
use rusqlite::Connection;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-store-unified-database-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn ledger_rows(database_path: &std::path::Path) -> Vec<(String, i64)> {
    let connection = Connection::open(database_path).unwrap();
    let mut statement = connection
        .prepare("SELECT id, applied_at FROM schema_migrations ORDER BY id")
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn permission_grants_open_creates_the_unified_database_file() {
    let directory = data_directory();

    let store = PermissionGrantStore::open(&directory).unwrap();

    assert_eq!(store.database_path(), directory.join("agens.db"));
    assert!(!directory.join("permissions.db").exists());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn the_ledger_records_each_applied_migration_once() {
    let directory = data_directory();

    let store = PermissionGrantStore::open(&directory).unwrap();
    let rows = ledger_rows(&store.database_path());

    assert_eq!(
        rows.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        vec!["0001_permission_grants"]
    );

    let user_version: i64 = Connection::open(store.database_path())
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(user_version, 0);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_reopen_applies_no_migrations_and_leaves_the_ledger_unchanged() {
    let directory = data_directory();

    let first = PermissionGrantStore::open(&directory).unwrap();
    let before = ledger_rows(&first.database_path());
    drop(first);

    let second = PermissionGrantStore::open(&directory).unwrap();
    let after = ledger_rows(&second.database_path());

    assert_eq!(before, after);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_unknown_ledger_id_is_tolerated_and_known_missing_migrations_still_apply() {
    let directory = data_directory();
    let database_path = directory.join("agens.db");

    {
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (
                     id TEXT PRIMARY KEY,
                     applied_at INTEGER NOT NULL
                 );
                 INSERT INTO schema_migrations (id, applied_at) VALUES ('9999_unknown', 0);",
            )
            .unwrap();
    }

    let store = PermissionGrantStore::open(&directory).unwrap();
    let rows = ledger_rows(&store.database_path());

    assert_eq!(
        rows,
        vec![
            ("0001_permission_grants".to_owned(), rows[0].1),
            ("9999_unknown".to_owned(), 0),
        ]
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_database_with_tables_but_no_ledger_is_rejected_as_an_unrecognized_layout() {
    let directory = data_directory();
    let database_path = directory.join("agens.db");

    let ddl = "CREATE TABLE permission_grants (
        id INTEGER PRIMARY KEY,
        project TEXT NOT NULL,
        decision TEXT NOT NULL,
        tool_kind TEXT NOT NULL,
        tool_value TEXT,
        target_kind TEXT NOT NULL,
        target_value TEXT
    );";
    {
        let connection = Connection::open(&database_path).unwrap();
        connection.execute_batch(ddl).unwrap();
    }
    let before = fs::read(&database_path).unwrap();

    let error = PermissionGrantStore::open(&directory)
        .err()
        .unwrap()
        .to_string();

    assert!(error.contains("check database layout"), "{error}");
    assert!(error.contains(database_path.to_string_lossy().as_ref()));

    let after = fs::read(&database_path).unwrap();
    assert_eq!(before, after);

    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn existing_loose_permissions_are_tightened_on_reopen() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let directory = data_directory();
    {
        let store = PermissionGrantStore::open(&directory).unwrap();
        drop(store);
    }

    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(
        directory.join("agens.db"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    PermissionGrantStore::open(&directory).unwrap();

    assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
        fs::metadata(directory.join("agens.db")).unwrap().mode() & 0o777,
        0o600
    );

    fs::remove_dir_all(directory).unwrap();
}
