use std::{
    fs,
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

use agens_core::{
    CompletedTurnRepository, CompletedTurnSnapshot, MessagePart, TurnEvent, TurnState,
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
