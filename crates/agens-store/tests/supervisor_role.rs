use agens_core::{
    CompletedSessionTurn, Message, MessagePart, Role, SessionMessage, SessionMetadata,
};
use agens_store::SessionStore;

fn directory(label: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agens-supervisor-role-{label}-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&path).ok();
    std::fs::create_dir_all(&path).expect("test data directory");
    path
}

fn metadata() -> SessionMetadata {
    SessionMetadata {
        id: 7,
        project: "project".into(),
        title: "title".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 10,
        updated_at: 20,
        completed_turn_count: 0,
        resumable: false,
        parent_session_id: None,
        fork_message_count: None,
    }
}

fn turn(messages: Vec<(Role, &str)>) -> CompletedSessionTurn {
    CompletedSessionTurn::new(
        messages
            .into_iter()
            .map(|(role, text)| {
                SessionMessage::try_from(Message {
                    role,
                    parts: vec![MessagePart::Text(text.to_owned())],
                })
                .expect("a text message encodes")
            })
            .collect(),
    )
    .expect("the turn is valid")
}

fn counts(directory: &std::path::Path) -> (i64, i64) {
    let connection =
        rusqlite::Connection::open(directory.join("agens.db")).expect("the database opens");
    (
        connection
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .expect("messages is readable"),
        connection
            .query_row("SELECT COUNT(*) FROM message_parts", [], |row| row.get(0))
            .expect("message_parts is readable"),
    )
}

/// The role has to survive the round trip through SQLite, not only through the
/// in-memory types. A `CHECK` that does not know the role fails the whole turn
/// rather than the one message, and no unit test sees it because none of them
/// writes to the database — a live run is what caught it.
#[test]
fn a_turn_carrying_a_supervisor_message_is_persisted() {
    let directory = directory("roundtrip");
    let metadata = metadata();
    let mut store = SessionStore::open(&directory).expect("the store opens");
    let attempt = store
        .begin_session_attempt(&metadata, "do the thing".into())
        .expect("the attempt begins");

    store
        .persist_completed_session_attempt(
            attempt.key(),
            &metadata,
            &turn(vec![
                (Role::User, "do the thing"),
                (Role::Supervisor, "prefer the manifest"),
                (Role::Assistant, "done"),
            ]),
            21,
        )
        .expect("a turn carrying a supervisor message is persisted");

    let connection =
        rusqlite::Connection::open(directory.join("agens.db")).expect("the database opens");
    let roles: Vec<String> = connection
        .prepare("SELECT role FROM messages ORDER BY sequence")
        .expect("the query prepares")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("the query runs")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("every row reads");

    assert_eq!(roles, vec!["user", "supervisor", "assistant"]);

    std::fs::remove_dir_all(&directory).ok();
}

/// The rebuild that widened the role check drops and recreates two tables. This
/// stands in for the generic preserved-row guard, which cannot run for this
/// migration: it counts rows before the DDL, and a database that never carried
/// sessions has no `messages` to count.
#[test]
fn a_supervisor_role_migration_keeps_every_message_and_part() {
    let directory = directory("preserved");
    let metadata = metadata();
    let mut store = SessionStore::open(&directory).expect("the store opens");
    let attempt = store
        .begin_session_attempt(&metadata, "first".into())
        .expect("the attempt begins");
    store
        .persist_completed_session_attempt(
            attempt.key(),
            &metadata,
            &turn(vec![(Role::User, "first"), (Role::Assistant, "second")]),
            21,
        )
        .expect("the turn persists");
    drop(store);

    let before = counts(&directory);
    assert!(before.0 > 0 && before.1 > 0, "the fixture wrote rows");

    let _ = SessionStore::open(&directory).expect("the store reopens");

    assert_eq!(counts(&directory), before, "the rebuild lost rows");

    std::fs::remove_dir_all(&directory).ok();
}
