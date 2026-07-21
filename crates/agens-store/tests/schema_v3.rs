use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_store::SessionStore;
use rusqlite::Connection;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct MigrationFaultGuard(std::path::PathBuf);

impl Drop for MigrationFaultGuard {
    fn drop(&mut self) {
        fs::remove_file(&self.0).unwrap();
    }
}

fn populated_v2() -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "agens-store-v3-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir_all(&directory).unwrap();
    let connection = Connection::open(directory.join("rust-sessions.db")).unwrap();
    connection.execute_batch(
        "CREATE TABLE legacy_turns (
             id INTEGER PRIMARY KEY, status TEXT NOT NULL, reason TEXT NOT NULL,
             source_event_count INTEGER NOT NULL
         );
         CREATE TABLE legacy_turn_events (
             turn_id INTEGER NOT NULL, sequence INTEGER NOT NULL, kind TEXT NOT NULL,
             state TEXT, part_kind TEXT, call_id TEXT, name TEXT, input TEXT, content TEXT,
             is_error INTEGER, PRIMARY KEY(turn_id, sequence),
             FOREIGN KEY(turn_id) REFERENCES legacy_turns(id) ON DELETE CASCADE
         );
         CREATE UNIQUE INDEX legacy_turn_events_turn_sequence
             ON legacy_turn_events(turn_id, sequence);
         CREATE TABLE sessions (
             id INTEGER PRIMARY KEY, project TEXT NOT NULL CHECK(project <> ''), title TEXT NOT NULL,
             active_agent TEXT NOT NULL CHECK(active_agent <> ''), created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             completed_turn_count INTEGER NOT NULL DEFAULT 0 CHECK(completed_turn_count >= 0),
             resumable INTEGER NOT NULL DEFAULT 0 CHECK(resumable IN(0, 1)),
             CHECK(resumable = (completed_turn_count > 0))
         );
         CREATE TABLE turns (
             session_id INTEGER NOT NULL, sequence INTEGER NOT NULL CHECK(sequence > 0),
             completed_at INTEGER NOT NULL, PRIMARY KEY(session_id, sequence),
             FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
         );
         CREATE TABLE messages (
             session_id INTEGER NOT NULL, sequence INTEGER NOT NULL CHECK(sequence > 0),
             turn_sequence INTEGER NOT NULL CHECK(turn_sequence > 0),
             role TEXT NOT NULL CHECK(role IN('system', 'user', 'assistant', 'tool')),
             PRIMARY KEY(session_id, sequence),
             FOREIGN KEY(session_id, turn_sequence) REFERENCES turns(session_id, sequence) ON DELETE CASCADE
         );
         CREATE TABLE message_parts (
             session_id INTEGER NOT NULL, message_sequence INTEGER NOT NULL,
             sequence INTEGER NOT NULL CHECK(sequence >= 0),
             kind TEXT NOT NULL CHECK(kind IN('text', 'reasoning', 'tool_call', 'tool_result')),
             text TEXT, call_id TEXT, name TEXT, input_json TEXT, content TEXT,
             is_error INTEGER CHECK(is_error IN(0, 1)),
             PRIMARY KEY(session_id, message_sequence, sequence),
             FOREIGN KEY(session_id, message_sequence) REFERENCES messages(session_id, sequence) ON DELETE CASCADE,
             CHECK((kind IN('text', 'reasoning') AND text IS NOT NULL AND call_id IS NULL AND name IS NULL AND input_json IS NULL AND content IS NULL AND is_error IS NULL) OR (kind = 'tool_call' AND text IS NULL AND call_id IS NOT NULL AND call_id <> '' AND name IS NOT NULL AND name <> '' AND input_json IS NOT NULL AND content IS NULL AND is_error IS NULL) OR (kind = 'tool_result' AND text IS NULL AND call_id IS NOT NULL AND call_id <> '' AND name IS NULL AND input_json IS NULL AND content IS NOT NULL AND is_error IS NOT NULL))
         );
         CREATE INDEX sessions_list ON sessions(resumable, updated_at DESC, id DESC);
         CREATE INDEX messages_turn_order ON messages(session_id, turn_sequence, sequence);
         CREATE INDEX parts_message_order ON message_parts(session_id, message_sequence, sequence);
         INSERT INTO legacy_turns VALUES(40, 'non_resumable', 'v1 lacks session/user/project/title/agent/timestamps', 0);
         INSERT INTO sessions VALUES(7, '/project/a', 'saved', 'primary', 10, 20, 1, 1);
         INSERT INTO turns VALUES(7, 1, 20);
         INSERT INTO messages VALUES(7, 1, 1, 'user'), (7, 2, 1, 'assistant');
         INSERT INTO message_parts(session_id, message_sequence, sequence, kind, text)
             VALUES(7, 1, 0, 'text', 'question'), (7, 2, 0, 'text', 'answer');
         PRAGMA user_version = 2;"
    ).unwrap();
    directory
}

#[test]
fn populated_v2_migrates_losslessly_with_null_metadata_and_reopens_idempotently() {
    let directory = populated_v2();
    let store = SessionStore::open(&directory).unwrap();
    let database = Connection::open(store.database_path()).unwrap();

    assert_eq!(
        database
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        database
            .query_row(
                "SELECT provider_id, model_id, reasoning_effort FROM sessions WHERE id = 7",
                [],
                |row| Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?
                ))
            )
            .unwrap(),
        (None, None, None)
    );
    assert_eq!(
        database
            .query_row("SELECT count(*) FROM messages", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        database
            .query_row("SELECT count(*) FROM legacy_turns", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(database);
    drop(store);

    SessionStore::open(&directory).unwrap();
    let reopened = Connection::open(directory.join("rust-sessions.db")).unwrap();
    assert_eq!(
        reopened
            .query_row("SELECT count(*) FROM sessions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        reopened
            .query_row("SELECT count(*) FROM message_parts", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn interrupted_v2_migration_rolls_back_schema_version_and_values() {
    let directory = populated_v2();
    let database = directory.join("rust-sessions.db");
    let fault_path = std::path::PathBuf::from(format!("{}.migration-fault", database.display()));
    fs::write(&fault_path, "before-v3-commit").unwrap();
    let fault = MigrationFaultGuard(fault_path);

    assert!(SessionStore::open(&directory).is_err());
    drop(fault);
    let connection = Connection::open(&database).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM pragma_table_info('sessions')",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        8
    );
    assert_eq!(
        connection
            .query_row("SELECT title FROM sessions WHERE id = 7", [], |row| row
                .get::<_, String>(
                0
            ))
            .unwrap(),
        "saved"
    );
    drop(connection);

    SessionStore::open(&directory).unwrap();
    fs::remove_dir_all(directory).unwrap();
}
