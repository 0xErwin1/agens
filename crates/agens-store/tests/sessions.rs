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
fn creates_lists_and_loads_completed_turns_in_persisted_order() {
    let directory = data_directory();
    let first = completed_snapshot("first");
    let second = completed_snapshot("second");

    let stored_turns = {
        let mut store = SessionStore::open(&directory).unwrap();
        assert_eq!(store.database_path(), directory.join("rust-sessions.db"));
        assert!(!directory.join("rust-permissions.db").exists());

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
    assert_eq!(
        reopened
            .load_completed_turn_for_resume(stored_turns[1].id)
            .unwrap(),
        second
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
             BEFORE INSERT ON completed_turn_events
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

    assert_eq!(store.load_completed_turn_for_resume(1).unwrap(), snapshot);

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
                    &format!("UPDATE completed_turn_events SET {field} = NULL WHERE sequence = ?1"),
                    [sequence],
                )
                .unwrap();

            assert!(
                store.load_completed_turn_for_resume(1).is_err(),
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
                    &format!(
                        "UPDATE completed_turn_events SET {field} = {value} WHERE sequence = ?1"
                    ),
                    [sequence],
                )
                .unwrap();

            assert!(
                store.load_completed_turn_for_resume(1).is_err(),
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
                &format!("UPDATE completed_turn_events SET {assignment} WHERE sequence = ?1"),
                [sequence],
            )
            .unwrap();

        assert!(
            store.load_completed_turn_for_resume(1).is_err(),
            "corruption {assignment} must fail closed"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
