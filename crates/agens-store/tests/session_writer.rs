use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_core::{
    CompletedSessionTurn, Message, MessagePart, Role, SessionMessage, SessionMetadata,
};
use agens_store::SessionStore;
use rusqlite::Connection;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-store-writer-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn turn(messages: Vec<Message>) -> CompletedSessionTurn {
    CompletedSessionTurn::new(
        messages
            .into_iter()
            .map(SessionMessage::try_from)
            .collect::<Result<_, _>>()
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn persists_text_completed_turn_and_reopens_in_order() {
    let directory = directory();
    let metadata = SessionMetadata {
        id: 7,
        project: "project".into(),
        title: "title".into(),
        active_agent: "primary".into(),
        created_at: 10,
        updated_at: 20,
        completed_turn_count: 0,
        resumable: false,
    };
    let first = turn(vec![
        Message {
            role: Role::System,
            parts: vec![MessagePart::Text("system".into())],
        },
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("first user".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("first assistant".into())],
        },
    ]);
    let second = turn(vec![
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("second user".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("second assistant".into())],
        },
    ]);
    let appended_metadata = SessionMetadata {
        completed_turn_count: 1,
        resumable: true,
        ..metadata.clone()
    };
    let unsupported = turn(vec![
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("unsupported user".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Reasoning("unsupported reasoning".into())],
        },
    ]);

    let mut store = SessionStore::open(&directory).unwrap();
    assert!(
        store
            .persist_completed_session_turn(&metadata, &unsupported)
            .is_err()
    );
    assert_eq!(
        Connection::open(store.database_path())
            .unwrap()
            .query_row("SELECT count(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
    store
        .persist_completed_session_turn(&metadata, &first)
        .unwrap();
    store
        .persist_completed_session_turn(&appended_metadata, &second)
        .unwrap();
    drop(store);

    let reopened = SessionStore::open(&directory).unwrap();
    let connection = Connection::open(reopened.database_path()).unwrap();
    assert_eq!(
        connection.query_row("SELECT project, title, active_agent, created_at, updated_at, completed_turn_count, resumable FROM sessions", [], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?, row.get::<_, i64>(5)?, row.get::<_, bool>(6)?))).unwrap(),
        ("project".into(), "title".into(), "primary".into(), 10, 20, 2, true),
    );
    assert_eq!(connection.prepare("SELECT turn_sequence, role, text FROM messages JOIN message_parts ON messages.session_id = message_parts.session_id AND messages.sequence = message_parts.message_sequence ORDER BY messages.sequence, message_parts.sequence").unwrap().query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap(), vec![(1, "system".into(), "system".into()), (1, "user".into(), "first user".into()), (1, "assistant".into(), "first assistant".into()), (2, "user".into(), "second user".into()), (2, "assistant".into(), "second assistant".into())]);

    fs::remove_dir_all(directory).unwrap();
}
