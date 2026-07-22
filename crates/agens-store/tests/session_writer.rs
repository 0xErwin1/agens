use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_core::{
    AttemptFinishOutcome, CompletedSessionTurn, Message, MessagePart, ReasoningEffort, Role,
    SessionAttemptStatus, SessionMessage, SessionMetadata,
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

fn normalized_counts(connection: &Connection) -> (i64, i64, i64, i64) {
    connection
        .query_row(
            "SELECT
                 (SELECT count(*) FROM sessions),
                 (SELECT count(*) FROM turns),
                 (SELECT count(*) FROM messages),
                 (SELECT count(*) FROM message_parts)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

#[test]
fn attempt_begin_and_finish_are_targeted_cas_transactions() {
    let directory = directory();
    let metadata = SessionMetadata {
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
    };
    let mut store = SessionStore::open(&directory).unwrap();

    let attempt = store
        .begin_session_attempt(&metadata, "retry".into())
        .unwrap();
    assert!(
        store
            .begin_session_attempt(&metadata, "again".into())
            .is_err()
    );
    assert_eq!(
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::Failed, 21)
            .unwrap(),
        AttemptFinishOutcome::Finished
    );
    assert_eq!(
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::Failed, 22)
            .unwrap(),
        AttemptFinishOutcome::Stale
    );
    let connection = Connection::open(store.database_path()).unwrap();
    assert_eq!(normalized_counts(&connection), (1, 0, 0, 0));
    assert_eq!(
        connection
            .query_row(
                "SELECT status, failure_kind FROM session_attempts WHERE id = ?1",
                [attempt.key().attempt_id()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap(),
        ("failed".into(), "failed".into())
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn persists_text_completed_turn_and_reopens_in_order() {
    let directory = directory();
    let metadata = SessionMetadata {
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
    let invalid_json = turn(vec![
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("unsupported user".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::ToolCall {
                id: "call".into(),
                name: "name".into(),
                input: "not json".into(),
            }],
        },
    ]);

    let mut store = SessionStore::open(&directory).unwrap();
    assert!(
        store
            .persist_completed_session_turn(&metadata, &invalid_json)
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

#[test]
fn persists_all_typed_parts_with_canonical_tool_json() {
    let directory = directory();
    let metadata = SessionMetadata {
        id: 8,
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
    };
    let turn = turn(vec![
        Message {
            role: Role::System,
            parts: vec![MessagePart::Text("system\n✓".into())],
        },
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("user\0bytes".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![
                MessagePart::Text("answer".into()),
                MessagePart::Reasoning("because\r\n✓".into()),
                MessagePart::ToolCall {
                    id: "call-1".into(),
                    name: "search".into(),
                    input: r#"{"z":[{"b":2,"a":1}],"a":true}"#.into(),
                },
            ],
        },
        Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "result\r\n✓".into(),
                is_error: false,
            }],
        },
    ]);

    let mut store = SessionStore::open(&directory).unwrap();
    store
        .persist_completed_session_turn(&metadata, &turn)
        .unwrap();
    drop(store);

    let connection = Connection::open(directory.join("rust-sessions.db")).unwrap();
    let parts = connection.prepare("SELECT role, kind, text, call_id, name, input_json, content, is_error FROM messages JOIN message_parts ON messages.session_id = message_parts.session_id AND messages.sequence = message_parts.message_sequence ORDER BY messages.sequence, message_parts.sequence").unwrap().query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, Option<String>>(3)?, row.get::<_, Option<String>>(4)?, row.get::<_, Option<String>>(5)?, row.get::<_, Option<String>>(6)?, row.get::<_, Option<i64>>(7)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert_eq!(
        parts,
        vec![
            (
                "system".into(),
                "text".into(),
                Some("system\n✓".into()),
                None,
                None,
                None,
                None,
                None
            ),
            (
                "user".into(),
                "text".into(),
                Some("user\0bytes".into()),
                None,
                None,
                None,
                None,
                None
            ),
            (
                "assistant".into(),
                "text".into(),
                Some("answer".into()),
                None,
                None,
                None,
                None,
                None
            ),
            (
                "assistant".into(),
                "reasoning".into(),
                Some("because\r\n✓".into()),
                None,
                None,
                None,
                None,
                None
            ),
            (
                "assistant".into(),
                "tool_call".into(),
                None,
                Some("call-1".into()),
                Some("search".into()),
                Some(r#"{"a":true,"z":[{"a":1,"b":2}]}"#.into()),
                None,
                None
            ),
            (
                "tool".into(),
                "tool_result".into(),
                None,
                Some("call-1".into()),
                None,
                None,
                Some("result\r\n✓".into()),
                Some(0)
            ),
        ]
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn atomically_rolls_back_failed_writes_and_rejects_stale_metadata() {
    let directory = directory();
    let metadata = SessionMetadata {
        id: 9,
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
    };
    let invalid_json = turn(vec![
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("user".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::ToolCall {
                id: "call-1".into(),
                name: "search".into(),
                input: "not json".into(),
            }],
        },
    ]);
    let completed = turn(vec![
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("user".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::ToolCall {
                id: "call-1".into(),
                name: "search".into(),
                input: "{}".into(),
            }],
        },
        Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "result".into(),
                is_error: false,
            }],
        },
    ]);

    let mut first = SessionStore::open(&directory).unwrap();
    let mut stale = SessionStore::open(&directory).unwrap();
    let connection = Connection::open(first.database_path()).unwrap();

    assert!(
        first
            .persist_completed_session_turn(&metadata, &invalid_json)
            .is_err()
    );
    assert_eq!(normalized_counts(&connection), (0, 0, 0, 0));

    connection
        .execute_batch(
            "CREATE TRIGGER reject_tool_result
             BEFORE INSERT ON message_parts
             WHEN NEW.kind = 'tool_result'
             BEGIN SELECT RAISE(ABORT, 'test transaction failure'); END;",
        )
        .unwrap();
    assert!(
        first
            .persist_completed_session_turn(&metadata, &completed)
            .is_err()
    );
    assert_eq!(normalized_counts(&connection), (0, 0, 0, 0));

    connection
        .execute("DROP TRIGGER reject_tool_result", [])
        .unwrap();
    first
        .persist_completed_session_turn(&metadata, &completed)
        .unwrap();
    assert!(
        stale
            .persist_completed_session_turn(&metadata, &completed)
            .is_err()
    );
    drop(first);
    drop(stale);

    let reopened = SessionStore::open(&directory).unwrap();
    let connection = Connection::open(reopened.database_path()).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT completed_turn_count, resumable FROM sessions WHERE id = 9",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
            )
            .unwrap(),
        (1, true)
    );
    assert_eq!(normalized_counts(&connection), (1, 1, 3, 3));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn session_store_crud_round_trips_normalized_context() {
    let directory = directory();
    let metadata = SessionMetadata {
        id: 10,
        project: "project".into(),
        title: "original".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 10,
        updated_at: 20,
        completed_turn_count: 0,
        resumable: false,
    };
    let messages = vec![
        Message {
            role: Role::System,
            parts: vec![MessagePart::Text("system".into())],
        },
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("user".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![
                MessagePart::Reasoning("reasoning".into()),
                MessagePart::ToolCall {
                    id: "call".into(),
                    name: "search".into(),
                    input: r#"{"z":2,"a":1}"#.into(),
                },
            ],
        },
        Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: "call".into(),
                content: "result".into(),
                is_error: false,
            }],
        },
    ];
    let mut store = SessionStore::open(&directory).unwrap();
    store
        .persist_completed_session_turn(&metadata, &turn(messages.clone()))
        .unwrap();
    let updated = SessionMetadata {
        title: "renamed".into(),
        active_agent: "reviewer".into(),
        updated_at: 30,
        completed_turn_count: 1,
        resumable: true,
        ..metadata
    };

    store.update_session(&updated).unwrap();
    assert_eq!(store.list_sessions().unwrap(), vec![updated.clone()]);
    let mut expected_messages = messages;
    let MessagePart::ToolCall { input, .. } = &mut expected_messages[2].parts[1] else {
        panic!("expected tool call");
    };
    *input = r#"{"a":1,"z":2}"#.into();
    assert_eq!(
        store.load_session_for_resume(10).unwrap(),
        agens_store::StoredSession {
            metadata: updated.clone(),
            messages: expected_messages,
        }
    );
    drop(store);

    let reopened = SessionStore::open(&directory).unwrap();
    assert_eq!(reopened.list_sessions().unwrap(), vec![updated]);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn session_store_rejects_legacy_resume_and_delete_is_idempotent() {
    let directory = directory();
    let mut store = SessionStore::open(&directory).unwrap();
    Connection::open(store.database_path())
        .unwrap()
        .execute(
            "INSERT INTO legacy_turns(id, status, reason, source_event_count) VALUES (11, 'non_resumable', 'legacy', 0)",
            [],
        )
        .unwrap();
    assert!(store.load_session_for_resume(11).is_err());

    let metadata = SessionMetadata {
        id: 12,
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
    };
    store
        .persist_completed_session_turn(
            &metadata,
            &turn(vec![
                Message {
                    role: Role::User,
                    parts: vec![MessagePart::Text("user".into())],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![MessagePart::Text("assistant".into())],
                },
            ]),
        )
        .unwrap();

    store.delete_session(12).unwrap();
    store.delete_session(12).unwrap();
    assert!(store.list_sessions().unwrap().is_empty());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn selection_metadata_round_trips_updates_atomically_and_preserves_crud_boundaries() {
    let directory = directory();
    let completed = turn(vec![
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("question".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("answer".into())],
        },
    ]);
    let mut metadata = SessionMetadata {
        id: 20,
        project: "/project/a".into(),
        title: "selection".into(),
        active_agent: "primary".into(),
        provider_id: Some("openai-chatgpt".into()),
        model_id: Some("gpt-5.5".into()),
        reasoning_effort: Some(ReasoningEffort::Max),
        created_at: 10,
        updated_at: 20,
        completed_turn_count: 0,
        resumable: false,
    };
    let mut store = SessionStore::open(&directory).unwrap();
    metadata = store
        .persist_completed_session_turn(&metadata, &completed)
        .unwrap();
    assert_eq!(
        store.load_session_for_resume(20).unwrap().metadata,
        metadata
    );

    metadata.provider_id = Some("openai-api".into());
    metadata.model_id = Some("gpt-5.6".into());
    metadata.reasoning_effort = None;
    store.update_session_selection(&metadata).unwrap();
    let before_failure = store.load_session_for_resume(20).unwrap().metadata;
    Connection::open(store.database_path())
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_selection BEFORE UPDATE OF provider_id ON sessions
         BEGIN SELECT RAISE(ABORT, 'reject selection'); END;",
        )
        .unwrap();
    metadata.provider_id = Some("openai-chatgpt".into());
    metadata.model_id = Some("gpt-5.4".into());
    metadata.reasoning_effort = Some(ReasoningEffort::Low);
    assert!(store.update_session_selection(&metadata).is_err());
    assert_eq!(
        store.load_session_for_resume(20).unwrap().metadata,
        before_failure
    );

    let database = Connection::open(store.database_path()).unwrap();
    let schema = database
        .query_row(
            "SELECT group_concat(name, ',') FROM pragma_table_info('sessions')",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    assert!(schema.ends_with("provider_id,model_id,reasoning_effort"));
    for forbidden in [
        "credential",
        "token",
        "account",
        "base_url",
        "secret-sentinel",
    ] {
        assert!(!schema.contains(forbidden));
    }
    drop(database);
    store.delete_session(20).unwrap();
    assert!(store.load_session_for_resume(20).is_err());
    assert!(
        store
            .list_sessions()
            .unwrap()
            .iter()
            .all(|session| session.project != "/project/a")
    );
    fs::remove_dir_all(directory).unwrap();
}
