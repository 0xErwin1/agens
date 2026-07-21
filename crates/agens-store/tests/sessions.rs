use std::{
    fs,
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

use agens_core::{
    CompletedTurnRepository, CompletedTurnSnapshot, MessagePart, TurnCoordinator, TurnEvent,
    TurnState,
};
use agens_store::SessionStore;
use rusqlite::{Connection, OpenFlags};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct MigrationFaultGuard {
    path: std::path::PathBuf,
}

impl MigrationFaultGuard {
    fn set(database: &std::path::Path, point: &str) -> Self {
        let path = std::path::PathBuf::from(format!("{}.migration-fault", database.display()));
        fs::write(&path, point).unwrap();

        Self { path }
    }
}

impl Drop for MigrationFaultGuard {
    fn drop(&mut self) {
        fs::remove_file(&self.path).unwrap();
    }
}

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-store-sessions-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn completed_snapshot(text: &str) -> CompletedTurnSnapshot {
    CompletedTurnSnapshot::from_persisted_events(vec![
        TurnEvent::StateChanged(TurnState::Requesting),
        TurnEvent::StateChanged(TurnState::Streaming),
        TurnEvent::ProviderPart(MessagePart::Text(text.into())),
        TurnEvent::StateChanged(TurnState::Completed),
    ])
    .unwrap()
}

fn completed_snapshot_with_all_persisted_variants() -> CompletedTurnSnapshot {
    let mut coordinator = TurnCoordinator::new();
    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::Text("text".into()))
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::Reasoning("reasoning".into()))
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "tool".into(),
            input: "{}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result("call-1", "result".into(), false)
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::Text("final".into()))
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();

    CompletedTurnSnapshot::from_persisted_events(coordinator.events().to_vec()).unwrap()
}

fn create_supported_session_schema(connection: &Connection, index_sql: &str) {
    connection
        .execute_batch(&format!(
            "CREATE TABLE completed_turns (id INTEGER PRIMARY KEY);
             CREATE TABLE completed_turn_events (
                 turn_id INTEGER NOT NULL,
                 sequence INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 state TEXT,
                 part_kind TEXT,
                 call_id TEXT,
                 name TEXT,
                 input TEXT,
                 content TEXT,
                 is_error INTEGER,
                 PRIMARY KEY (turn_id, sequence),
                 FOREIGN KEY (turn_id) REFERENCES completed_turns(id)
             );
             {index_sql}
             PRAGMA user_version = 1;"
        ))
        .unwrap();
}

fn create_populated_wal_v1_fixture(directory: &std::path::Path) {
    let database = directory.join("rust-sessions.db");
    let connection = Connection::open(database).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    create_supported_session_schema(
        &connection,
        "CREATE UNIQUE INDEX completed_turn_events_turn_sequence
         ON completed_turn_events(turn_id, sequence);",
    );
    connection
        .execute_batch(
            "INSERT INTO completed_turns(id) VALUES(7), (8), (11), (13);
              INSERT INTO completed_turn_events VALUES
                 (7, 1, 'state_changed', 'requesting', NULL, NULL, NULL, NULL, NULL, NULL),
                 (7, 2, 'provider_part', NULL, 'text', NULL, NULL, NULL, 'WAL content', NULL),
                 (7, 3, 'provider_part', NULL, 'tool_call', 'call-1', 'tool', '{\"key\":true}', NULL, NULL),
                 (11, 1, 'tool_result', NULL, NULL, 'call-1', NULL, NULL, 'result\nwith punctuation: !?', 0),
                 (11, 2, 'tool_result', NULL, NULL, 'call-2', NULL, NULL, 'error', 1);",
        )
        .unwrap();
}

type V1Event = (
    i64,
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
);

#[derive(Debug, PartialEq)]
struct V1Contents {
    turns: Vec<i64>,
    events: Vec<V1Event>,
}

fn v1_contents(connection: &Connection) -> V1Contents {
    let turns = connection
        .prepare("SELECT id FROM completed_turns ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let events = connection
        .prepare(
            "SELECT turn_id, sequence, kind, state, part_kind, call_id, name, input, content,
                    is_error
             FROM completed_turn_events ORDER BY turn_id, sequence",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();

    V1Contents { turns, events }
}

fn archive_contents(connection: &Connection) -> V1Contents {
    let turns = connection
        .prepare("SELECT id FROM legacy_turns ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let events = connection
        .prepare(
            "SELECT turn_id, sequence, kind, state, part_kind, call_id, name, input, content,
                    is_error
             FROM legacy_turn_events ORDER BY turn_id, sequence",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();

    V1Contents { turns, events }
}

fn table_signature(connection: &Connection, table: &str) -> Vec<(i64, String, String, i64, i64)> {
    connection
        .prepare(&format!("PRAGMA table_info('{table}')"))
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(5)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn foreign_key_signature(
    connection: &Connection,
    table: &str,
) -> Vec<(String, String, String, String)> {
    connection
        .prepare(&format!("PRAGMA foreign_key_list('{table}')"))
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(2)?, row.get(3)?, row.get(4)?, row.get(6)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn index_signature(connection: &Connection, table: &str) -> Vec<(String, i64, String)> {
    connection
        .prepare(&format!("PRAGMA index_list('{table}')"))
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn exact_v1_manifest(connection: &Connection) -> String {
    let lines = connection
        .prepare(
            "SELECT 'schema|' || type || '|' || name || '|' || quote(sql)
             FROM sqlite_schema WHERE type IN ('index', 'table') AND name LIKE 'completed_turn%'
             UNION ALL SELECT 'turn_count|' || count(*) FROM completed_turns
             UNION ALL SELECT 'event_count|' || count(*) FROM completed_turn_events
             UNION ALL SELECT 'completed_turns|' || id FROM completed_turns
             UNION ALL SELECT 'completed_turn_events|' || turn_id || '|' || sequence || '|' ||
                 quote(kind) || '|' || quote(state) || '|' || quote(part_kind) || '|' ||
                 quote(call_id) || '|' || quote(name) || '|' || quote(input) || '|' ||
                 quote(content) || '|' || quote(is_error)
             FROM completed_turn_events ORDER BY 1",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();

    format!("version=1\nquick_check=ok\n{}\n", lines.join("\n"))
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

#[test]
fn opens_populated_wal_v1_as_v3_smoke() {
    let directory = data_directory();
    create_populated_wal_v1_fixture(&directory);

    let store = SessionStore::open(&directory).unwrap();
    let database = store.database_path();
    let connection = Connection::open(&database).unwrap();

    assert!(directory.join("rust-sessions.db.v1.bak").exists());
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM legacy_turns", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert!(
        connection
            .query_row("SELECT count(*) FROM completed_turns", [], |row| row
                .get::<_, i64>(0))
            .is_err()
    );
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
    drop(connection);
    drop(store);

    SessionStore::open(&directory).unwrap();
    assert!(!directory.join("rust-sessions.db.v1.bak.1").exists());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn normalized_v3_schema() {
    let directory = data_directory();
    create_populated_wal_v1_fixture(&directory);

    let store = SessionStore::open(&directory).unwrap();
    let connection = Connection::open(store.database_path()).unwrap();
    let schema = connection
        .prepare(
            "SELECT sql FROM sqlite_schema
             WHERE type IN ('table', 'index')
               AND name IN ('sessions', 'turns', 'messages', 'message_parts',
                            'sessions_list', 'messages_turn_order', 'parts_message_order')
             ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join(" ");

    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert!(schema.contains("CREATE TABLE sessions"));
    assert!(schema.contains("CREATE TABLE turns"));
    assert!(schema.contains("CREATE TABLE messages"));
    assert!(schema.contains("CREATE TABLE message_parts"));
    assert!(schema.contains("CREATE INDEX sessions_list"));
    assert!(schema.contains("CREATE INDEX messages_turn_order"));
    assert!(schema.contains("CREATE INDEX parts_message_order"));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn migration_preserves_v1_losslessly() {
    let directory = data_directory();
    create_populated_wal_v1_fixture(&directory);
    let database = directory.join("rust-sessions.db");
    let source = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let expected = v1_contents(&source);
    drop(source);

    let store = SessionStore::open(&directory).unwrap();
    let archive = Connection::open(store.database_path()).unwrap();

    assert_eq!(archive_contents(&archive), expected);
    assert_eq!(
        archive
            .query_row("SELECT count(*) FROM legacy_turns", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert_eq!(
        archive
            .query_row("SELECT count(*) FROM legacy_turn_events", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        5
    );
    assert_eq!(
        archive
            .prepare("SELECT id, status, reason, source_event_count FROM legacy_turns ORDER BY id",)
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap(),
        vec![
            (
                7,
                "non_resumable".into(),
                "v1 lacks session/user/project/title/agent/timestamps".into(),
                3
            ),
            (
                8,
                "non_resumable".into(),
                "v1 lacks session/user/project/title/agent/timestamps".into(),
                0
            ),
            (
                11,
                "non_resumable".into(),
                "v1 lacks session/user/project/title/agent/timestamps".into(),
                2
            ),
            (
                13,
                "non_resumable".into(),
                "v1 lacks session/user/project/title/agent/timestamps".into(),
                0
            ),
        ]
    );
    assert_eq!(
        archive
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table'
                 AND name IN ('sessions', 'turns', 'messages', 'message_parts')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        4
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn migration_validates_schema_and_reopen_contract() {
    let directory = data_directory();
    create_populated_wal_v1_fixture(&directory);
    let source = Connection::open_with_flags(
        directory.join("rust-sessions.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let expected = v1_contents(&source);
    drop(source);

    let store = SessionStore::open(&directory).unwrap();
    let database = store.database_path();
    drop(store);

    let archive = Connection::open(&database).unwrap();
    assert_eq!(
        table_signature(&archive, "legacy_turns"),
        vec![
            (0, "id".into(), "INTEGER".into(), 0, 1),
            (1, "status".into(), "TEXT".into(), 1, 0),
            (2, "reason".into(), "TEXT".into(), 1, 0),
            (3, "source_event_count".into(), "INTEGER".into(), 1, 0),
        ]
    );
    assert_eq!(
        table_signature(&archive, "legacy_turn_events"),
        vec![
            (0, "turn_id".into(), "INTEGER".into(), 1, 1),
            (1, "sequence".into(), "INTEGER".into(), 1, 2),
            (2, "kind".into(), "TEXT".into(), 1, 0),
            (3, "state".into(), "TEXT".into(), 0, 0),
            (4, "part_kind".into(), "TEXT".into(), 0, 0),
            (5, "call_id".into(), "TEXT".into(), 0, 0),
            (6, "name".into(), "TEXT".into(), 0, 0),
            (7, "input".into(), "TEXT".into(), 0, 0),
            (8, "content".into(), "TEXT".into(), 0, 0),
            (9, "is_error".into(), "INTEGER".into(), 0, 0),
        ]
    );
    assert_eq!(
        foreign_key_signature(&archive, "legacy_turn_events"),
        vec![(
            "legacy_turns".into(),
            "turn_id".into(),
            "id".into(),
            "CASCADE".into(),
        )]
    );
    assert_eq!(
        index_signature(&archive, "legacy_turn_events"),
        vec![
            ("legacy_turn_events_turn_sequence".into(), 1, "c".into(),),
            (
                "sqlite_autoindex_legacy_turn_events_1".into(),
                1,
                "pk".into(),
            ),
        ]
    );
    assert_eq!(archive_contents(&archive), expected);
    assert_eq!(
        archive
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table'
                 AND name IN ('completed_turns', 'completed_turn_events')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(
        archive
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        archive
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
    drop(archive);

    SessionStore::open(&directory).unwrap();
    assert!(!directory.join("rust-sessions.db.v1.bak.1").exists());

    let tampered = Connection::open(&database).unwrap();
    tampered
        .execute_batch(
            "PRAGMA writable_schema = ON;
             UPDATE sqlite_schema
             SET sql = replace(sql, 'ON DELETE CASCADE', 'ON DELETE NO ACTION')
             WHERE type = 'table' AND name = 'legacy_turn_events';
             PRAGMA writable_schema = OFF;",
        )
        .unwrap();
    drop(tampered);

    assert!(SessionStore::open(&directory).is_err());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_tampered_v1_before_destructive_finalization() {
    let directory = data_directory();
    create_populated_wal_v1_fixture(&directory);
    let database = directory.join("rust-sessions.db");
    let tampered = Connection::open(&database).unwrap();
    tampered
        .execute_batch(
            "PRAGMA writable_schema = ON;
             UPDATE sqlite_schema
             SET sql = replace(sql, 'FOREIGN KEY (turn_id) REFERENCES completed_turns(id)',
                               'FOREIGN KEY (turn_id) REFERENCES completed_turns(id) ON DELETE CASCADE')
             WHERE type = 'table' AND name = 'completed_turn_events';
             PRAGMA writable_schema = OFF;",
        )
        .unwrap();
    drop(tampered);

    assert!(SessionStore::open(&directory).is_err());

    let source = Connection::open(&database).unwrap();
    assert_eq!(
        source
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(v1_contents(&source).turns, vec![7, 8, 11, 13]);
    assert_eq!(
        source
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'legacy_turns'",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
        0
    );
    assert!(!directory.join("rust-sessions.db.v1.bak").exists());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn migration_faults_preserve_v1_or_recover_only_a_committed_v3() {
    for (point, commits) in [
        ("after-backup-step", false),
        ("after-backup-finalize", false),
        ("after-backup-install", false),
        ("after-backup-parent-fsync", false),
        ("during-mutation", false),
        ("before-commit", false),
        ("after-commit-before-reopen", true),
    ] {
        let directory = data_directory();
        create_populated_wal_v1_fixture(&directory);
        let database = directory.join("rust-sessions.db");
        let source =
            Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let expected = v1_contents(&source);
        drop(source);

        let fault = MigrationFaultGuard::set(&database, point);
        assert!(
            SessionStore::open(&directory).is_err(),
            "{point} must fail closed"
        );
        drop(fault);

        let inspected = Connection::open(&database).unwrap();
        let version = inspected
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap();
        if commits {
            assert_eq!(version, 3, "{point} may expose only a committed v3");
            assert_eq!(archive_contents(&inspected), expected);
            assert!(
                inspected
                    .query_row("SELECT count(*) FROM completed_turns", [], |row| row
                        .get::<_, i64>(0))
                    .is_err()
            );
        } else {
            assert_eq!(version, 1, "{point} must roll back to v1");
            assert_eq!(v1_contents(&inspected), expected);
            assert_eq!(
                inspected
                    .query_row(
                        "SELECT count(*) FROM sqlite_schema WHERE name IN ('legacy_turns', 'legacy_turn_events')",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                0
            );
        }
        drop(inspected);

        let retried = SessionStore::open(&directory).unwrap();
        let recovered = Connection::open(retried.database_path()).unwrap();
        assert_eq!(
            archive_contents(&recovered),
            expected,
            "{point} retry content"
        );
        drop(recovered);
        drop(retried);
        assert!(!directory.join("rust-sessions.db.v1.bak.2").exists());

        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn migration_retry_uses_a_new_backup_suffix_without_clobbering_collision() {
    let directory = data_directory();
    create_populated_wal_v1_fixture(&directory);
    let existing_backup = directory.join("rust-sessions.db.v1.bak");
    fs::write(&existing_backup, "do not replace").unwrap();

    let fault = MigrationFaultGuard::set(&directory.join("rust-sessions.db"), "before-commit");
    assert!(SessionStore::open(&directory).is_err());
    drop(fault);

    SessionStore::open(&directory).unwrap();
    assert_eq!(fs::read(&existing_backup).unwrap(), b"do not replace");
    assert!(directory.join("rust-sessions.db.v1.bak.1").exists());
    assert!(directory.join("rust-sessions.db.v1.bak.2").exists());

    SessionStore::open(&directory).unwrap();
    assert!(!directory.join("rust-sessions.db.v1.bak.3").exists());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn creates_a_verified_wal_snapshot_and_exact_v1_manifest() {
    let directory = data_directory();
    let database = directory.join("rust-sessions.db");
    let writer = Connection::open(&database).unwrap();
    writer.pragma_update(None, "journal_mode", "WAL").unwrap();
    create_supported_session_schema(
        &writer,
        "CREATE UNIQUE INDEX completed_turn_events_turn_sequence
         ON completed_turn_events(turn_id, sequence);",
    );
    writer
        .execute_batch(
            "INSERT INTO completed_turns(id) VALUES(7), (8), (11);
             INSERT INTO completed_turn_events
              VALUES(7, 3, 'provider_part', NULL, 'text', NULL, NULL, NULL,
                     'WAL content', NULL),
                    (7, 4, 'provider_part', NULL, 'tool_call', 'call-1', 'tool', '{}',
                     NULL, NULL),
                    (11, 1, 'state_changed', 'completed', NULL, NULL, NULL, NULL,
                     NULL, NULL),
                    (11, 2, 'tool_result', NULL, NULL, 'call-2', NULL, NULL,
                     'result', 0);",
        )
        .unwrap();

    SessionStore::open(&directory).unwrap();
    let backup = directory.join("rust-sessions.db.v1.bak");
    let manifest = fs::read_to_string(backup.with_extension("bak.manifest")).unwrap();
    let snapshot = Connection::open_with_flags(&backup, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();

    let expected = V1Contents {
        turns: vec![7, 8, 11],
        events: vec![
            (
                7,
                3,
                "provider_part".into(),
                None,
                Some("text".into()),
                None,
                None,
                None,
                Some("WAL content".into()),
                None,
            ),
            (
                7,
                4,
                "provider_part".into(),
                None,
                Some("tool_call".into()),
                Some("call-1".into()),
                Some("tool".into()),
                Some("{}".into()),
                None,
                None,
            ),
            (
                11,
                1,
                "state_changed".into(),
                Some("completed".into()),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            (
                11,
                2,
                "tool_result".into(),
                None,
                None,
                Some("call-2".into()),
                None,
                None,
                Some("result".into()),
                Some(0),
            ),
        ],
    };
    assert_eq!(v1_contents(&snapshot), expected);
    assert_eq!(manifest, exact_v1_manifest(&snapshot));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn preserves_existing_backup_and_stale_temp_with_a_deterministic_suffix() {
    let directory = data_directory();
    let database = directory.join("rust-sessions.db");
    let connection = Connection::open(&database).unwrap();
    create_supported_session_schema(
        &connection,
        "CREATE UNIQUE INDEX completed_turn_events_turn_sequence
         ON completed_turn_events(turn_id, sequence);",
    );
    drop(connection);
    fs::write(directory.join("rust-sessions.db.v1.bak"), "existing").unwrap();
    fs::write(directory.join("rust-sessions.db.v1.bak.1.tmp"), "stale").unwrap();

    SessionStore::open(&directory).unwrap();
    let backup = directory.join("rust-sessions.db.v1.bak.2");

    assert_eq!(backup, directory.join("rust-sessions.db.v1.bak.2"));
    assert_eq!(
        fs::read(directory.join("rust-sessions.db.v1.bak")).unwrap(),
        b"existing"
    );
    assert_eq!(
        fs::read(directory.join("rust-sessions.db.v1.bak.1.tmp")).unwrap(),
        b"stale"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn fresh_v2_legacy_coexistence() {
    let directory = data_directory();
    let first = completed_snapshot("first");
    let second = completed_snapshot("second");

    let stored_turns = {
        let mut store = SessionStore::open(&directory).unwrap();
        assert_eq!(store.database_path(), directory.join("rust-sessions.db"));
        assert!(!directory.join("rust-permissions.db").exists());
        let database = Connection::open(store.database_path()).unwrap();
        assert_eq!(
            database
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            database
                .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(
            database
                .query_row("SELECT count(*) FROM sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(database);

        block_on_ready(store.persist_completed_turn(first.clone())).unwrap();
        block_on_ready(store.persist_completed_turn(second.clone())).unwrap();

        store.list_completed_turns().unwrap()
    };

    assert_eq!(stored_turns.len(), 2);
    assert_eq!(stored_turns[0].id, 1);
    assert_eq!(stored_turns[0].snapshot, first);
    assert_eq!(stored_turns[1].id, 2);
    assert_eq!(stored_turns[1].snapshot, second);

    let reopened = SessionStore::open(&directory).unwrap();
    assert_eq!(reopened.list_completed_turns().unwrap(), stored_turns);
    assert!(
        reopened
            .load_completed_turn_for_resume(stored_turns[1].id)
            .is_err()
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rolls_back_a_completed_turn_when_an_event_write_fails() {
    let directory = data_directory();
    let mut store = SessionStore::open(&directory).unwrap();
    let database = store.database_path();

    Connection::open(&database)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_second_event
             BEFORE INSERT ON legacy_turn_events
             WHEN NEW.sequence = 1
             BEGIN
                 SELECT RAISE(ABORT, 'reject event');
             END;",
        )
        .unwrap();

    assert!(block_on_ready(store.persist_completed_turn(completed_snapshot("rollback"))).is_err());
    assert!(store.list_completed_turns().unwrap().is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_unsupported_session_schema_versions_with_path_and_operation_context() {
    let directory = data_directory();
    let database = directory.join("rust-sessions.db");
    Connection::open(&database)
        .unwrap()
        .pragma_update(None, "user_version", 999)
        .unwrap();

    let error = SessionStore::open(&directory).err().unwrap().to_string();

    assert!(error.contains("sessions check schema version"));
    assert!(error.contains(database.to_string_lossy().as_ref()));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_supported_versions_with_an_incompatible_session_schema_shape() {
    let directory = data_directory();
    let database = directory.join("rust-sessions.db");
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE completed_turns (id TEXT PRIMARY KEY);
             CREATE TABLE completed_turn_events (
                 turn_id INTEGER NOT NULL,
                 sequence INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 state TEXT,
                 part_kind TEXT,
                 call_id TEXT,
                 name TEXT,
                 input TEXT,
                 content TEXT,
                 is_error INTEGER,
                 PRIMARY KEY (turn_id, sequence),
                 FOREIGN KEY (turn_id) REFERENCES completed_turns(id)
             );",
        )
        .unwrap();
    connection.pragma_update(None, "user_version", 1).unwrap();
    drop(connection);

    let error = SessionStore::open(&directory).err().unwrap().to_string();

    assert!(error.contains("sessions verify schema"));
    assert!(error.contains(database.to_string_lossy().as_ref()));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn accepts_only_the_exact_supported_session_indexes() {
    let fixtures = [
        (
            "supported",
            "CREATE UNIQUE INDEX completed_turn_events_turn_sequence
             ON completed_turn_events(turn_id, sequence);",
            true,
        ),
        ("missing", "", false),
        (
            "wrong name",
            "CREATE UNIQUE INDEX wrong_turn_sequence
             ON completed_turn_events(turn_id, sequence);",
            false,
        ),
        (
            "wrong uniqueness",
            "CREATE INDEX completed_turn_events_turn_sequence
             ON completed_turn_events(turn_id, sequence);",
            false,
        ),
        (
            "wrong column order",
            "CREATE UNIQUE INDEX completed_turn_events_turn_sequence
             ON completed_turn_events(sequence, turn_id);",
            false,
        ),
        (
            "extra index",
            "CREATE UNIQUE INDEX completed_turn_events_turn_sequence
             ON completed_turn_events(turn_id, sequence);
             CREATE INDEX unexpected_completed_turn_events_kind
             ON completed_turn_events(kind);",
            false,
        ),
        (
            "extra parent index",
            "CREATE UNIQUE INDEX completed_turn_events_turn_sequence
             ON completed_turn_events(turn_id, sequence);
             CREATE INDEX unexpected_completed_turns_id ON completed_turns(id);",
            false,
        ),
    ];

    for (name, index_sql, should_open) in fixtures {
        let directory = data_directory();
        let database = directory.join("rust-sessions.db");
        let connection = Connection::open(&database).unwrap();
        create_supported_session_schema(&connection, index_sql);
        drop(connection);

        let result = SessionStore::open(&directory);
        assert_eq!(
            result.is_ok(),
            should_open,
            "{name} index fixture must {}",
            if should_open { "open" } else { "fail closed" }
        );

        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn round_trips_all_persisted_event_variants_losslessly() {
    let directory = data_directory();
    let snapshot = completed_snapshot_with_all_persisted_variants();
    let mut store = SessionStore::open(&directory).unwrap();

    block_on_ready(store.persist_completed_turn(snapshot.clone())).unwrap();

    assert_eq!(store.list_completed_turns().unwrap()[0].snapshot, snapshot);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_missing_or_cross_variant_persisted_event_fields() {
    let snapshot = completed_snapshot_with_all_persisted_variants();
    let required_fields = [
        (0, ["state"].as_slice()),
        (1, ["state"].as_slice()),
        (2, ["part_kind", "content"].as_slice()),
        (3, ["part_kind", "content"].as_slice()),
        (4, ["part_kind", "call_id", "name", "input"].as_slice()),
        (5, ["state"].as_slice()),
        (6, ["call_id", "name", "input"].as_slice()),
        (7, ["call_id", "content", "is_error"].as_slice()),
        (8, ["state"].as_slice()),
        (9, ["state"].as_slice()),
        (10, ["part_kind", "content"].as_slice()),
        (11, ["state"].as_slice()),
    ];
    let forbidden_fields = [
        (
            0,
            [
                "part_kind",
                "call_id",
                "name",
                "input",
                "content",
                "is_error",
            ]
            .as_slice(),
        ),
        (
            1,
            [
                "part_kind",
                "call_id",
                "name",
                "input",
                "content",
                "is_error",
            ]
            .as_slice(),
        ),
        (
            2,
            ["state", "call_id", "name", "input", "is_error"].as_slice(),
        ),
        (
            3,
            ["state", "call_id", "name", "input", "is_error"].as_slice(),
        ),
        (4, ["state", "content", "is_error"].as_slice()),
        (
            5,
            [
                "part_kind",
                "call_id",
                "name",
                "input",
                "content",
                "is_error",
            ]
            .as_slice(),
        ),
        (6, ["state", "part_kind", "content", "is_error"].as_slice()),
        (7, ["state", "part_kind", "name", "input"].as_slice()),
        (
            8,
            [
                "part_kind",
                "call_id",
                "name",
                "input",
                "content",
                "is_error",
            ]
            .as_slice(),
        ),
        (
            9,
            [
                "part_kind",
                "call_id",
                "name",
                "input",
                "content",
                "is_error",
            ]
            .as_slice(),
        ),
        (
            10,
            ["state", "call_id", "name", "input", "is_error"].as_slice(),
        ),
        (
            11,
            [
                "part_kind",
                "call_id",
                "name",
                "input",
                "content",
                "is_error",
            ]
            .as_slice(),
        ),
    ];

    for (sequence, fields) in required_fields {
        for field in fields {
            let directory = data_directory();
            let mut store = SessionStore::open(&directory).unwrap();
            block_on_ready(store.persist_completed_turn(snapshot.clone())).unwrap();

            Connection::open(store.database_path())
                .unwrap()
                .execute(
                    &format!("UPDATE legacy_turn_events SET {field} = NULL WHERE sequence = ?1"),
                    [sequence],
                )
                .unwrap();

            assert!(
                store.list_completed_turns().is_err(),
                "missing required {field} for sequence {sequence} must fail closed"
            );
            fs::remove_dir_all(directory).unwrap();
        }
    }

    for (sequence, fields) in forbidden_fields {
        for field in fields {
            let directory = data_directory();
            let mut store = SessionStore::open(&directory).unwrap();
            block_on_ready(store.persist_completed_turn(snapshot.clone())).unwrap();

            let value = if *field == "is_error" {
                "1"
            } else {
                "'forbidden'"
            };
            Connection::open(store.database_path())
                .unwrap()
                .execute(
                    &format!("UPDATE legacy_turn_events SET {field} = {value} WHERE sequence = ?1"),
                    [sequence],
                )
                .unwrap();

            assert!(
                store.list_completed_turns().is_err(),
                "forbidden {field} for sequence {sequence} must fail closed"
            );
            fs::remove_dir_all(directory).unwrap();
        }
    }
}

#[test]
fn rejects_unknown_persisted_event_tags_and_invalid_required_field_types() {
    let snapshot = completed_snapshot_with_all_persisted_variants();
    let corruptions = [
        ("kind = 'unknown'", 0),
        ("part_kind = 'unknown'", 2),
        ("is_error = 2", 7),
        ("content = CAST(X'00' AS BLOB)", 2),
    ];

    for (assignment, sequence) in corruptions {
        let directory = data_directory();
        let mut store = SessionStore::open(&directory).unwrap();
        block_on_ready(store.persist_completed_turn(snapshot.clone())).unwrap();

        Connection::open(store.database_path())
            .unwrap()
            .execute(
                &format!("UPDATE legacy_turn_events SET {assignment} WHERE sequence = ?1"),
                [sequence],
            )
            .unwrap();

        assert!(
            store.list_completed_turns().is_err(),
            "corruption {assignment} must fail closed"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
