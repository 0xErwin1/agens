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
use rusqlite::Connection;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

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

/// The exact v5 normalized session schema, reproduced verbatim so that
/// `validate_normalized_session_schema`'s statement-by-statement comparison matches after
/// whitespace normalization. Any drift here from `agens-store`'s own schema strings makes these
/// fixtures fail for the wrong reason.
fn full_normalized_v5_schema() -> &'static str {
    "CREATE TABLE sessions (
        id INTEGER PRIMARY KEY,
        project TEXT NOT NULL CHECK(project <> ''),
        title TEXT NOT NULL,
        active_agent TEXT NOT NULL CHECK(active_agent <> ''),
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        completed_turn_count INTEGER NOT NULL DEFAULT 0 CHECK(completed_turn_count >= 0),
        resumable INTEGER NOT NULL DEFAULT 0 CHECK(resumable IN(0, 1)),
        provider_id TEXT CHECK(provider_id <> '' AND length(provider_id) <= 64),
        model_id TEXT CHECK(model_id <> '' AND length(model_id) <= 64),
        reasoning_effort TEXT CHECK(reasoning_effort IN('none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max')),
        CHECK(resumable = (completed_turn_count > 0))
    );
    CREATE TABLE turns (
        session_id INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence > 0),
        completed_at INTEGER NOT NULL,
        PRIMARY KEY(session_id, sequence),
        FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
    );
    CREATE TABLE messages (
        session_id INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence > 0),
        turn_sequence INTEGER NOT NULL CHECK(turn_sequence > 0),
        role TEXT NOT NULL CHECK(role IN('system', 'user', 'assistant', 'tool')),
        PRIMARY KEY(session_id, sequence),
        FOREIGN KEY(session_id, turn_sequence) REFERENCES turns(session_id, sequence) ON DELETE CASCADE
    );
    CREATE TABLE message_parts (
        session_id INTEGER NOT NULL,
        message_sequence INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence >= 0),
        kind TEXT NOT NULL CHECK(kind IN('text', 'reasoning', 'tool_call', 'tool_result')),
        text TEXT,
        call_id TEXT,
        name TEXT,
        input_json TEXT,
        content TEXT,
        is_error INTEGER CHECK(is_error IN(0, 1)),
        PRIMARY KEY(session_id, message_sequence, sequence),
        FOREIGN KEY(session_id, message_sequence) REFERENCES messages(session_id, sequence) ON DELETE CASCADE,
        CHECK((kind IN('text', 'reasoning') AND text IS NOT NULL AND call_id IS NULL AND name IS NULL AND input_json IS NULL AND content IS NULL AND is_error IS NULL) OR (kind = 'tool_call' AND text IS NULL AND call_id IS NOT NULL AND call_id <> '' AND name IS NOT NULL AND name <> '' AND input_json IS NOT NULL AND content IS NULL AND is_error IS NULL) OR (kind = 'tool_result' AND text IS NULL AND call_id IS NOT NULL AND call_id <> '' AND name IS NULL AND input_json IS NULL AND content IS NOT NULL AND is_error IS NOT NULL))
    );
    CREATE INDEX sessions_list ON sessions(resumable, updated_at DESC, id DESC);
    CREATE INDEX messages_turn_order ON messages(session_id, turn_sequence, sequence);
    CREATE INDEX parts_message_order ON message_parts(session_id, message_sequence, sequence);
    CREATE TABLE session_attempts (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence > 0),
        status TEXT NOT NULL CHECK(status IN('running', 'completed', 'cancelled', 'failed', 'provider_error', 'interrupted')),
        failure_kind TEXT CHECK(failure_kind IN('cancelled', 'failed', 'provider_error', 'interrupted')),
        retry_prompt TEXT CHECK(retry_prompt IS NULL OR (length(CAST(retry_prompt AS BLOB)) BETWEEN 1 AND 65536)),
        started_at INTEGER NOT NULL,
        finished_at INTEGER,
        completed_turn_sequence INTEGER,
        FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
        FOREIGN KEY(session_id, completed_turn_sequence) REFERENCES turns(session_id, sequence) ON DELETE SET NULL,
        CHECK((status = 'running' AND failure_kind IS NULL AND retry_prompt IS NOT NULL AND finished_at IS NULL AND completed_turn_sequence IS NULL) OR
              (status = 'completed' AND failure_kind IS NULL AND retry_prompt IS NULL AND finished_at IS NOT NULL AND completed_turn_sequence IS NOT NULL) OR
              (status = 'cancelled' AND failure_kind = 'cancelled' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR
              (status = 'failed' AND failure_kind = 'failed' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR
              (status = 'provider_error' AND failure_kind = 'provider_error' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR
              (status = 'interrupted' AND failure_kind = 'interrupted' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL))
    );
    CREATE UNIQUE INDEX session_attempts_session_sequence ON session_attempts(session_id, sequence);
    CREATE UNIQUE INDEX session_attempts_one_running ON session_attempts(session_id) WHERE status = 'running';
    CREATE INDEX session_attempts_latest ON session_attempts(session_id, sequence DESC, id DESC);"
}

/// Seeds a fully migrated ledger (all three known migrations already recorded) alongside the
/// legacy archive tables (with a caller-controlled index shape) and the exact v5 normalized
/// schema, so `SessionStore::open` skips migration entirely and exercises only the post-open
/// `validate_v5_schema` shape check.
fn seed_migrated_agens_db(connection: &Connection, legacy_index_sql: &str) {
    connection
        .execute_batch(&format!(
            "CREATE TABLE schema_migrations (
                 id TEXT PRIMARY KEY,
                 applied_at INTEGER NOT NULL
             );
             INSERT INTO schema_migrations (id, applied_at) VALUES
                 ('0001_permission_grants', 0),
                 ('0002_model_preference', 0),
                 ('0003_sessions_v5', 0);
             CREATE TABLE legacy_turns (
                 id INTEGER PRIMARY KEY,
                 status TEXT NOT NULL CHECK(status = 'non_resumable'),
                 reason TEXT NOT NULL,
                 source_event_count INTEGER NOT NULL CHECK(source_event_count >= 0)
             );
             CREATE TABLE legacy_turn_events (
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
                 PRIMARY KEY(turn_id, sequence),
                 FOREIGN KEY(turn_id) REFERENCES legacy_turns(id) ON DELETE CASCADE
             );
             {legacy_index_sql}
             {}",
            full_normalized_v5_schema()
        ))
        .unwrap();
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
fn normalized_v5_schema() {
    let directory = data_directory();

    let store = SessionStore::open(&directory).unwrap();
    let connection = Connection::open(store.database_path()).unwrap();
    let schema = connection
        .prepare(
            "SELECT sql FROM sqlite_schema
             WHERE type IN ('table', 'index')
                AND name IN ('sessions', 'turns', 'messages', 'message_parts',
                             'sessions_list', 'messages_turn_order', 'parts_message_order',
                             'session_attempts', 'session_attempts_session_sequence',
                             'session_attempts_one_running', 'session_attempts_latest')
             ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join(" ");

    assert!(schema.contains("CREATE TABLE sessions"));
    assert!(schema.contains("CREATE TABLE turns"));
    assert!(schema.contains("CREATE TABLE messages"));
    assert!(schema.contains("CREATE TABLE message_parts"));
    assert!(schema.contains("CREATE TABLE session_attempts"));
    assert!(schema.contains("CREATE INDEX sessions_list"));
    assert!(schema.contains("CREATE INDEX messages_turn_order"));
    assert!(schema.contains("CREATE INDEX parts_message_order"));
    assert!(schema.contains("CREATE UNIQUE INDEX session_attempts_session_sequence"));
    assert!(schema.contains("CREATE UNIQUE INDEX session_attempts_one_running"));
    assert!(schema.contains("CREATE INDEX session_attempts_latest"));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn fresh_v5_legacy_coexistence() {
    let directory = data_directory();
    let first = completed_snapshot("first");
    let second = completed_snapshot("second");

    let stored_turns = {
        let mut store = SessionStore::open(&directory).unwrap();
        assert_eq!(store.database_path(), directory.join("agens.db"));
        let database = Connection::open(store.database_path()).unwrap();
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
fn rejects_supported_versions_with_an_incompatible_session_schema_shape() {
    let directory = data_directory();
    let database = directory.join("agens.db");
    let connection = Connection::open(&database).unwrap();
    seed_migrated_agens_db(
        &connection,
        "CREATE UNIQUE INDEX legacy_turn_events_turn_sequence
         ON legacy_turn_events(turn_id, sequence);",
    );
    connection
        .execute_batch(
            "DROP TABLE sessions;
             CREATE TABLE sessions (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
    drop(connection);

    let error = SessionStore::open(&directory).err().unwrap().to_string();

    assert!(
        error.contains("sessions validate normalized schema"),
        "{error}"
    );
    assert!(error.contains(database.to_string_lossy().as_ref()));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn accepts_only_the_exact_supported_session_indexes() {
    let fixtures = [
        (
            "supported",
            "CREATE UNIQUE INDEX legacy_turn_events_turn_sequence
             ON legacy_turn_events(turn_id, sequence);",
            true,
        ),
        ("missing", "", false),
        (
            "wrong name",
            "CREATE UNIQUE INDEX wrong_turn_sequence
             ON legacy_turn_events(turn_id, sequence);",
            false,
        ),
        (
            "wrong uniqueness",
            "CREATE INDEX legacy_turn_events_turn_sequence
             ON legacy_turn_events(turn_id, sequence);",
            false,
        ),
        (
            "wrong column order",
            "CREATE UNIQUE INDEX legacy_turn_events_turn_sequence
             ON legacy_turn_events(sequence, turn_id);",
            false,
        ),
        (
            "extra index",
            "CREATE UNIQUE INDEX legacy_turn_events_turn_sequence
             ON legacy_turn_events(turn_id, sequence);
             CREATE INDEX unexpected_legacy_turn_events_kind
             ON legacy_turn_events(kind);",
            false,
        ),
    ];

    for (name, legacy_index_sql, should_open) in fixtures {
        let directory = data_directory();
        let database = directory.join("agens.db");
        let connection = Connection::open(&database).unwrap();
        seed_migrated_agens_db(&connection, legacy_index_sql);
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
