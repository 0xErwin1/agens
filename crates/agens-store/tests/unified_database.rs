use std::{
    fs,
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

use agens_core::{
    CompletedTurnRepository, CompletedTurnSnapshot, MessagePart, PermissionPattern,
    ProjectPermissionGrant, TurnEvent, TurnState,
};
use agens_store::{ModelPreference, PermissionGrantStore, PreferenceStore, SessionStore};
use rusqlite::Connection;

fn completed_snapshot(text: &str) -> CompletedTurnSnapshot {
    CompletedTurnSnapshot::from_persisted_events(vec![
        TurnEvent::StateChanged(TurnState::Requesting),
        TurnEvent::StateChanged(TurnState::Streaming),
        TurnEvent::ProviderPart(MessagePart::Text(text.into())),
        TurnEvent::StateChanged(TurnState::Completed),
    ])
    .unwrap()
}

fn block_on_ready<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = std::pin::pin!(future);

    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test repository must complete immediately"),
    }
}

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
        vec![
            "0001_permission_grants",
            "0002_model_preference",
            "0003_sessions_v5",
            "0004_tool_result_facts"
        ]
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
            ("0002_model_preference".to_owned(), rows[1].1),
            ("0003_sessions_v5".to_owned(), rows[2].1),
            ("0004_tool_result_facts".to_owned(), rows[3].1),
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

#[test]
fn two_stores_writing_concurrently_in_one_process_do_not_corrupt_or_spuriously_fail() {
    let directory = data_directory();

    let mut preference_store = PreferenceStore::open(&directory).unwrap();
    let mut permission_store = PermissionGrantStore::open(&directory).unwrap();

    assert_eq!(
        preference_store.database_path(),
        permission_store.database_path()
    );

    let preference = ModelPreference::new("gpt-5.5", None);
    let grant = ProjectPermissionGrant::allow(
        "project-a",
        PermissionPattern::Exact("native::edit".into()),
        PermissionPattern::Any,
    );

    preference_store.remember_model(&preference).unwrap();
    permission_store
        .append_grants(std::slice::from_ref(&grant))
        .unwrap();

    assert_eq!(
        preference_store.remembered_model().unwrap(),
        Some(preference)
    );
    assert_eq!(
        permission_store.grants_for_project("project-a").unwrap(),
        vec![grant]
    );

    fs::remove_dir_all(directory).unwrap();
}

/// Populates a legacy `permissions.db` with a real project-scoped grant, mirroring the pre-unified
/// `permission_grants` shape rather than an inert text fixture.
fn seed_legacy_permissions_database(path: &std::path::Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE permission_grants (
                id INTEGER PRIMARY KEY,
                project TEXT NOT NULL,
                decision TEXT NOT NULL,
                tool_kind TEXT NOT NULL,
                tool_value TEXT,
                target_kind TEXT NOT NULL,
                target_value TEXT
            );
            INSERT INTO permission_grants
                (project, decision, tool_kind, tool_value, target_kind, target_value)
            VALUES
                ('legacy-project', 'allow', 'exact', 'native::edit', 'any', NULL);",
        )
        .unwrap();
}

/// Populates a legacy `preferences.db` with a real remembered model, mirroring the pre-unified
/// `model_preference` shape rather than an inert text fixture.
fn seed_legacy_preferences_database(path: &std::path::Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE model_preference (
                id INTEGER PRIMARY KEY CHECK(id = 1),
                model TEXT NOT NULL CHECK(model <> ''),
                reasoning_effort TEXT
            );
            INSERT INTO model_preference (id, model, reasoning_effort)
            VALUES (1, 'legacy-model', 'high');",
        )
        .unwrap();
}

/// Populates a legacy v1 `sessions.db` with a real session row, mirroring the pre-unification
/// on-disk shape rather than an inert text fixture.
fn seed_legacy_sessions_database(path: &std::path::Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE sessions (
                id INTEGER PRIMARY KEY,
                title TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            INSERT INTO sessions (title, created_at) VALUES ('legacy session', 0);",
        )
        .unwrap();
}

#[test]
fn legacy_database_files_are_neither_read_nor_modified() {
    let directory = data_directory();

    seed_legacy_permissions_database(&directory.join("permissions.db"));
    seed_legacy_preferences_database(&directory.join("preferences.db"));
    seed_legacy_sessions_database(&directory.join("sessions.db"));

    let legacy_files = ["permissions.db", "preferences.db", "sessions.db"];
    let before: Vec<(std::fs::Metadata, Vec<u8>)> = legacy_files
        .iter()
        .map(|name| {
            let path = directory.join(name);
            (fs::metadata(&path).unwrap(), fs::read(&path).unwrap())
        })
        .collect();

    let mut preference_store = PreferenceStore::open(&directory).unwrap();
    let mut permission_store = PermissionGrantStore::open(&directory).unwrap();
    let mut session_store = SessionStore::open(&directory).unwrap();

    let unified_connection = Connection::open(preference_store.database_path()).unwrap();
    let unified_grant_count: i64 = unified_connection
        .query_row("SELECT count(*) FROM permission_grants", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(unified_grant_count, 0);
    drop(unified_connection);

    assert_eq!(preference_store.remembered_model().unwrap(), None);
    assert!(session_store.list_completed_turns().unwrap().is_empty());

    preference_store
        .remember_model(&ModelPreference::new("gpt-5.5", None))
        .unwrap();
    permission_store
        .append_grants(&[ProjectPermissionGrant::allow(
            "project-a",
            PermissionPattern::Any,
            PermissionPattern::Any,
        )])
        .unwrap();
    block_on_ready(session_store.persist_completed_turn(completed_snapshot("legacy coexistence")))
        .unwrap();
    drop(preference_store);
    drop(permission_store);
    drop(session_store);

    for (name, (metadata_before, contents_before)) in legacy_files.iter().zip(before) {
        let path = directory.join(name);
        let metadata_after = fs::metadata(&path).unwrap();
        let contents_after = fs::read(&path).unwrap();

        assert_eq!(metadata_before.len(), metadata_after.len(), "{name}");
        assert_eq!(contents_before, contents_after, "{name}");
        assert!(!directory.join(format!("{name}-wal")).exists(), "{name}");
        assert!(!directory.join(format!("{name}-shm")).exists(), "{name}");
    }

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_partially_applied_database_is_completed() {
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
                 INSERT INTO schema_migrations (id, applied_at)
                 VALUES ('0001_permission_grants', 0);
                 CREATE TABLE permission_grants (
                     id INTEGER PRIMARY KEY,
                     project TEXT NOT NULL,
                     decision TEXT NOT NULL,
                     tool_kind TEXT NOT NULL,
                     tool_value TEXT,
                     target_kind TEXT NOT NULL,
                     target_value TEXT
                 );
                 CREATE INDEX permission_grants_project
                     ON permission_grants(project, id);",
            )
            .unwrap();
    }

    let store = PermissionGrantStore::open(&directory).unwrap();
    let rows = ledger_rows(&store.database_path());

    assert_eq!(
        rows.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        vec![
            "0001_permission_grants",
            "0002_model_preference",
            "0003_sessions_v5",
            "0004_tool_result_facts"
        ]
    );

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

#[test]
fn all_three_domains_share_one_database_file() {
    let directory = data_directory();

    let preference_store = PreferenceStore::open(&directory).unwrap();
    let permission_store = PermissionGrantStore::open(&directory).unwrap();
    let session_store = SessionStore::open(&directory).unwrap();

    assert_eq!(preference_store.database_path(), directory.join("agens.db"));
    assert_eq!(permission_store.database_path(), directory.join("agens.db"));
    assert_eq!(session_store.database_path(), directory.join("agens.db"));
    assert_eq!(
        fs::read_dir(&directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".db"))
            .count(),
        1
    );

    let connection = Connection::open(directory.join("agens.db")).unwrap();
    let user_table_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema
             WHERE type = 'table'
               AND name IN ('permission_grants', 'model_preference', 'sessions', 'legacy_turns')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(user_table_count, 4);

    drop(connection);
    assert_eq!(
        ledger_rows(&directory.join("agens.db"))
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "0001_permission_grants",
            "0002_model_preference",
            "0003_sessions_v5",
            "0004_tool_result_facts"
        ]
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn three_stores_writing_concurrently_in_one_process_do_not_corrupt_or_spuriously_fail() {
    let directory = data_directory();

    let mut preference_store = PreferenceStore::open(&directory).unwrap();
    let mut permission_store = PermissionGrantStore::open(&directory).unwrap();
    let mut session_store = SessionStore::open(&directory).unwrap();

    let preference = ModelPreference::new("gpt-5.5", None);
    let grant = ProjectPermissionGrant::allow(
        "project-a",
        PermissionPattern::Exact("native::edit".into()),
        PermissionPattern::Any,
    );
    let snapshot = completed_snapshot("three-store concurrency");

    preference_store.remember_model(&preference).unwrap();
    permission_store
        .append_grants(std::slice::from_ref(&grant))
        .unwrap();
    block_on_ready(session_store.persist_completed_turn(snapshot.clone())).unwrap();

    assert_eq!(
        preference_store.remembered_model().unwrap(),
        Some(preference)
    );
    assert_eq!(
        permission_store.grants_for_project("project-a").unwrap(),
        vec![grant]
    );
    assert_eq!(
        session_store.list_completed_turns().unwrap()[0].snapshot,
        snapshot
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_bootstrap_only_ledger_with_no_user_tables_is_accepted() {
    let directory = data_directory();
    let database_path = directory.join("agens.db");

    {
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (
                     id TEXT PRIMARY KEY,
                     applied_at INTEGER NOT NULL
                 );",
            )
            .unwrap();
    }

    let store = PermissionGrantStore::open(&directory).unwrap();
    let rows = ledger_rows(&store.database_path());

    assert_eq!(
        rows.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        vec![
            "0001_permission_grants",
            "0002_model_preference",
            "0003_sessions_v5",
            "0004_tool_result_facts"
        ]
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_zero_row_ledger_table_alongside_user_tables_is_rejected() {
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
                 CREATE TABLE permission_grants (
                     id INTEGER PRIMARY KEY,
                     project TEXT NOT NULL,
                     decision TEXT NOT NULL,
                     tool_kind TEXT NOT NULL,
                     tool_value TEXT,
                     target_kind TEXT NOT NULL,
                     target_value TEXT
                 );",
            )
            .unwrap();
    }
    let before = fs::read(&database_path).unwrap();

    let error = PermissionGrantStore::open(&directory)
        .err()
        .unwrap()
        .to_string();

    assert!(error.contains("check database layout"), "{error}");
    assert!(error.contains("unrecognized database layout"), "{error}");

    let after = fs::read(&database_path).unwrap();
    assert_eq!(before, after);

    fs::remove_dir_all(directory).unwrap();
}
