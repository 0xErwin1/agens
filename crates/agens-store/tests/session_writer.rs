use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_core::{
    AttemptFinishOutcome, AttemptKey, BeginSessionAttemptError, CompletedSessionTurn,
    MAX_RETRY_PROMPT_BYTES, Message, MessagePart, ReasoningEffort, RecoveryOutcome, Role,
    SessionAttemptFailureKind, SessionAttemptStatus, SessionMessage, SessionMetadata,
};
use agens_store::{
    ForkSessionError, SessionCursor, SessionStore, StoredSession, ingest_media_bytes,
};
use rusqlite::Connection;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

/// A temporary store directory that removes itself when the test ends, whether
/// it ends by returning or by panicking on a failed assertion.
struct TestDirectory(std::path::PathBuf);

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for TestDirectory {
    type Target = std::path::Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<std::path::Path> for TestDirectory {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

/// A directory no other run can be holding: the process id alone repeats once
/// the operating system reuses it, and a leftover database under the same name
/// would be opened instead of a fresh one.
fn directory() -> TestDirectory {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the fixture clock is after the epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "agens-store-writer-{}-{started}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    TestDirectory(directory)
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

fn collect_session_pages(
    store: &SessionStore,
    project: Option<&str>,
    query: &str,
) -> Vec<StoredSession> {
    let mut cursor: Option<SessionCursor> = None;
    let mut sessions = Vec::new();

    loop {
        let page = store.list_session_page(project, query, cursor, 64).unwrap();
        sessions.extend(page.sessions);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return sessions;
        }
    }
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
        parent_session_id: None,
        fork_message_count: None,
    };
    let mut store = SessionStore::open(&directory).unwrap();

    let attempt = store
        .begin_session_attempt(&metadata, "retry".into())
        .unwrap();
    assert!(matches!(
        store.begin_session_attempt(&metadata, "again".into()),
        Err(BeginSessionAttemptError::AlreadyRunning(summary)) if summary.key() == attempt.key()
    ));
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
}

#[test]
fn successful_attempt_finish_is_atomic_exact_and_private() {
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
        parent_session_id: None,
        fork_message_count: None,
    };
    let completed = turn(vec![
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("question".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::ToolCall {
                id: "call-1".into(),
                name: "search".into(),
                input: r#"{"query":"answer"}"#.into(),
            }],
        },
        Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "answer".into(),
                is_error: false,
            }],
        },
    ]);
    let mut store = SessionStore::open(&directory).unwrap();
    let attempt = store
        .begin_session_attempt(&metadata, "private-prompt-token".into())
        .unwrap();

    assert_eq!(
        store
            .persist_completed_session_attempt(attempt.key(), &metadata, &completed, 21)
            .unwrap(),
        AttemptFinishOutcome::Finished
    );
    assert_eq!(
        store
            .persist_completed_session_attempt(attempt.key(), &metadata, &completed, 22)
            .unwrap(),
        AttemptFinishOutcome::Stale
    );

    let connection = Connection::open(store.database_path()).unwrap();
    assert_eq!(normalized_counts(&connection), (1, 1, 3, 3));
    assert_eq!(
        connection
            .query_row(
                "SELECT status, failure_kind, retry_prompt, completed_turn_sequence FROM session_attempts WHERE id = ?1",
                [attempt.key().attempt_id()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, Option<i64>>(3)?)),
            )
            .unwrap(),
        ("completed".into(), None, None, Some(1))
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT completed_turn_count, resumable FROM sessions WHERE id = ?1",
                [metadata.id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
            )
            .unwrap(),
        (1, true)
    );
    assert!(!format!("{attempt:?}").contains("private-prompt-token"));
    assert!(!format!("{:?}", BeginSessionAttemptError::Store).contains("private-prompt-token"));
}

#[test]
fn partial_attempt_persistence_appends_history_and_drops_the_retry_prompt() {
    let directory = directory();
    let metadata = SessionMetadata {
        id: 5,
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
    };
    let partial = turn(vec![
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("question".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("interrupted note".into())],
        },
    ]);
    let mut store = SessionStore::open(&directory).unwrap();
    let attempt = store
        .begin_session_attempt(&metadata, "private-prompt-token".into())
        .unwrap();

    assert_eq!(
        store
            .persist_partial_session_attempt(
                attempt.key(),
                &metadata,
                &partial,
                SessionAttemptStatus::Cancelled,
                21,
            )
            .unwrap(),
        AttemptFinishOutcome::Finished
    );
    assert_eq!(
        store
            .persist_partial_session_attempt(
                attempt.key(),
                &metadata,
                &partial,
                SessionAttemptStatus::Cancelled,
                22,
            )
            .unwrap(),
        AttemptFinishOutcome::Stale
    );
    assert!(
        store
            .persist_partial_session_attempt(
                attempt.key(),
                &metadata,
                &partial,
                SessionAttemptStatus::Completed,
                23,
            )
            .is_err()
    );

    let connection = Connection::open(store.database_path()).unwrap();
    assert_eq!(normalized_counts(&connection), (1, 1, 2, 2));
    assert_eq!(
        connection
            .query_row(
                "SELECT status, failure_kind, retry_prompt, finished_at, completed_turn_sequence
                 FROM session_attempts WHERE id = ?1",
                [attempt.key().attempt_id()],
                |row| Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                )),
            )
            .unwrap(),
        ("cancelled".into(), Some("cancelled".into()), None, 21, None)
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT completed_turn_count, resumable FROM sessions WHERE id = ?1",
                [metadata.id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
            )
            .unwrap(),
        (1, true)
    );
    assert!(store.load_retry_boundary(attempt.key()).unwrap().is_none());
    assert_eq!(
        collect_session_pages(&store, Some(&metadata.project), "")
            .iter()
            .map(|session| (session.metadata.id, session.metadata.completed_turn_count))
            .collect::<Vec<_>>(),
        [(metadata.id, 1)]
    );

    let stored = store.load_session_for_resume(metadata.id).unwrap();
    assert_eq!(stored.metadata.completed_turn_count, 1);
    assert_eq!(stored.messages, partial.messages());
}

#[test]
fn attempt_begin_bounds_prompts_allocates_zero_ids_and_preserves_other_sessions() {
    let directory = directory();
    let mut store = SessionStore::open(&directory).unwrap();
    let existing = SessionMetadata {
        id: 1,
        project: "existing".into(),
        title: "existing".into(),
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
    };
    let existing_attempt = store
        .begin_session_attempt(&existing, "existing-secret".into())
        .unwrap();
    let new_session = SessionMetadata {
        id: 0,
        project: "new".into(),
        title: "new".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 30,
        updated_at: 40,
        completed_turn_count: 0,
        resumable: false,
        parent_session_id: None,
        fork_message_count: None,
    };
    let exact_limit = format!(
        "{}abcd",
        "✓".repeat((MAX_RETRY_PROMPT_BYTES - 4) / "✓".len())
    );
    assert_eq!(exact_limit.len(), MAX_RETRY_PROMPT_BYTES);
    let allocated = store
        .begin_session_attempt(&new_session, exact_limit.clone())
        .unwrap();
    assert_ne!(allocated.key().session_id(), existing.id);
    assert!(matches!(
        store.begin_session_attempt(&new_session, format!("{exact_limit}x")),
        Err(BeginSessionAttemptError::Store)
    ));

    let connection = Connection::open(store.database_path()).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT status, retry_prompt FROM session_attempts WHERE id = ?1",
                [existing_attempt.key().attempt_id()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap(),
        ("running".into(), "existing-secret".into())
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM sessions WHERE id = 1", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        1
    );
}

#[test]
fn terminal_attempt_cas_shapes_never_append_history() {
    let directory = directory();
    let mut store = SessionStore::open(&directory).unwrap();

    for (index, (status, kind)) in [
        (
            SessionAttemptStatus::Cancelled,
            SessionAttemptFailureKind::Cancelled,
        ),
        (
            SessionAttemptStatus::Failed,
            SessionAttemptFailureKind::Failed,
        ),
        (
            SessionAttemptStatus::ProviderError,
            SessionAttemptFailureKind::ProviderError,
        ),
        (
            SessionAttemptStatus::Interrupted,
            SessionAttemptFailureKind::Interrupted,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let metadata = SessionMetadata {
            id: (index + 10) as i64,
            project: format!("project-{index}"),
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
        };
        let attempt = store
            .begin_session_attempt(&metadata, format!("private-{index}"))
            .unwrap();

        assert_eq!(
            store
                .finish_session_attempt(attempt.key(), status, 21)
                .unwrap(),
            AttemptFinishOutcome::Finished
        );
        assert_eq!(
            store
                .finish_session_attempt(attempt.key(), status, 22)
                .unwrap(),
            AttemptFinishOutcome::Stale
        );

        let connection = Connection::open(store.database_path()).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT status, failure_kind, finished_at, completed_turn_sequence FROM session_attempts WHERE id = ?1",
                    [attempt.key().attempt_id()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, Option<i64>>(3)?)),
                )
                .unwrap(),
            (attempt_status(status).into(), attempt_failure_kind(kind).into(), 21, None)
        );
        assert_eq!(
            normalized_counts(&connection),
            ((index + 1) as i64, 0, 0, 0)
        );
    }
}

#[test]
fn session_pages_use_stable_keyset_search_scope_and_bounded_sizes() {
    let directory = directory();
    let mut store = SessionStore::open(&directory).unwrap();

    for id in 1..=501 {
        let metadata = SessionMetadata {
            id,
            project: if id % 2 == 0 { "current" } else { "other" }.into(),
            title: if id == 1 {
                "Needle Café".into()
            } else {
                format!("session-{id}")
            },
            active_agent: if id == 2 { "reviewer" } else { "primary" }.into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: id,
            updated_at: id / 2,
            completed_turn_count: 0,
            resumable: false,
            parent_session_id: None,
            fork_message_count: None,
        };
        store
            .begin_session_attempt(&metadata, format!("private-{id}"))
            .unwrap();
    }
    let latest = store
        .load_session_for_resume(501)
        .unwrap()
        .latest_attempt
        .unwrap();
    store
        .finish_session_attempt(latest.key(), SessionAttemptStatus::Failed, 300)
        .unwrap();
    let replacement = SessionMetadata {
        id: 501,
        project: "other".into(),
        title: "session-501".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 501,
        updated_at: 301,
        completed_turn_count: 0,
        resumable: false,
        parent_session_id: None,
        fork_message_count: None,
    };
    store
        .begin_session_attempt(&replacement, "replacement-private".into())
        .unwrap();

    let first = store.list_session_page(None, "", None, 100).unwrap();
    assert_eq!(first.sessions.len(), 64);
    assert_eq!(first.sessions[0].metadata.id, 501);
    assert_eq!(first.sessions[1].metadata.id, 500);
    assert_eq!(first.sessions[63].metadata.id, 438);
    assert!(first.next_cursor.is_some());
    assert!(first.sessions.iter().all(|session| {
        session
            .latest_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.status() == SessionAttemptStatus::Running)
    }));
    assert_eq!(
        first.sessions[0]
            .latest_attempt
            .as_ref()
            .unwrap()
            .sequence(),
        2
    );
    assert!(!format!("{first:?}").contains("private-"));

    let inserted = SessionMetadata {
        id: 999,
        project: "current".into(),
        title: "inserted-newer".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 999,
        updated_at: 1_000,
        completed_turn_count: 0,
        resumable: false,
        parent_session_id: None,
        fork_message_count: None,
    };
    store
        .begin_session_attempt(&inserted, "inserted-private".into())
        .unwrap();

    let mut traversed = first.sessions;
    let mut cursor = first.next_cursor;
    while let Some(current) = cursor {
        let page = store
            .list_session_page(None, "", Some(current), 64)
            .unwrap();
        traversed.extend(page.sessions);
        cursor = page.next_cursor;
    }
    let identifiers = traversed
        .iter()
        .map(|session| session.metadata.id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(traversed.len(), 501);
    assert_eq!(identifiers.len(), 501);
    assert!(!identifiers.contains(&999));
    assert_eq!(traversed[64].metadata.id, 437);
    assert!(traversed.windows(2).all(|pair| {
        let left = (pair[0].metadata.updated_at, pair[0].metadata.id);
        let right = (pair[1].metadata.updated_at, pair[1].metadata.id);
        left > right
    }));

    let all = collect_session_pages(&store, None, "");
    let current = collect_session_pages(&store, Some("current"), "");
    let by_title = collect_session_pages(&store, None, "needle café");
    let by_identifier = collect_session_pages(&store, None, "1");
    let by_project = collect_session_pages(&store, None, "OTHER");
    let by_agent = collect_session_pages(&store, None, "REVIEWER");

    assert_eq!(all.len(), 502);
    assert_eq!(all[0].metadata.id, 999);
    assert_eq!(current.len(), 251);
    assert!(
        current
            .iter()
            .all(|session| session.metadata.project == "current")
    );
    assert_eq!(by_title.len(), 1);
    assert_eq!(by_title[0].metadata.id, 1);
    assert!(by_identifier.len() > 64);
    assert!(
        by_identifier
            .iter()
            .all(|session| session.metadata.id.to_string().contains('1'))
    );
    assert_eq!(by_project.len(), 251);
    assert_eq!(by_agent.len(), 1);
    assert_eq!(by_agent[0].metadata.id, 2);

    let loaded = store.load_session_for_resume(501).unwrap();
    let running = loaded.latest_attempt.unwrap();
    assert!(loaded.messages.is_empty());
    assert_eq!(running.key().session_id(), 501);
}

#[test]
fn zero_turn_session_without_retained_retry_prompt_is_not_resumable() {
    let directory = directory();
    let mut store = SessionStore::open(&directory).unwrap();
    let metadata = SessionMetadata {
        id: 7,
        project: "project".into(),
        title: "empty".into(),
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
    };
    let attempt = store
        .begin_session_attempt(&metadata, "discarded draft".into())
        .unwrap();
    store
        .finish_session_attempt(attempt.key(), SessionAttemptStatus::Failed, 30)
        .unwrap();
    Connection::open(store.database_path())
        .unwrap()
        .execute(
            "UPDATE session_attempts SET retry_prompt = NULL WHERE id = ?1",
            [attempt.key().attempt_id()],
        )
        .unwrap();

    let page = store.list_session_page(None, "", None, 64).unwrap();

    assert!(page.sessions.is_empty());
    assert!(store.load_session_for_resume(metadata.id).is_err());
}

#[test]
fn explicit_attempt_recovery_is_exact_stale_safe_and_history_preserving() {
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
        parent_session_id: None,
        fork_message_count: None,
    };
    let mut store = SessionStore::open(&directory).unwrap();
    let attempt = store
        .begin_session_attempt(&metadata, "private".into())
        .unwrap();

    assert_eq!(
        store.recover_running_attempt(attempt.key(), 30).unwrap(),
        RecoveryOutcome::Recovered(
            agens_core::SessionAttemptSummary::new(
                attempt.key(),
                1,
                SessionAttemptStatus::Interrupted,
                Some(SessionAttemptFailureKind::Interrupted),
                20,
                Some(30)
            )
            .unwrap()
        )
    );
    assert_eq!(
        store.recover_running_attempt(attempt.key(), 31).unwrap(),
        RecoveryOutcome::Stale
    );
    let recovered_boundary = store.load_retry_boundary(attempt.key()).unwrap().unwrap();
    assert_eq!(recovered_boundary.prompt(), "private");
    let recovered_session = store.load_session_for_resume(metadata.id).unwrap();
    assert!(
        store
            .load_retry_boundary(AttemptKey::new(7, attempt.key().attempt_id() + 1).unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::Failed, 32)
            .unwrap(),
        AttemptFinishOutcome::Stale
    );

    let unrelated_metadata = SessionMetadata {
        id: 8,
        project: "unrelated".into(),
        title: "unrelated".into(),
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
    };
    let unrelated = store
        .begin_session_attempt(&unrelated_metadata, "unrelated-private".into())
        .unwrap();
    let retried = store
        .begin_session_attempt(
            &recovered_session.metadata,
            recovered_boundary.prompt().to_owned(),
        )
        .unwrap();

    assert!(store.load_retry_boundary(attempt.key()).unwrap().is_none());
    assert_eq!(
        store
            .load_retry_boundary(retried.key())
            .unwrap()
            .unwrap()
            .prompt(),
        "private"
    );
    assert_eq!(
        store
            .load_retry_boundary(unrelated.key())
            .unwrap()
            .unwrap()
            .prompt(),
        "unrelated-private"
    );
    assert!(matches!(
        store.begin_session_attempt(&recovered_session.metadata, "replacement-private".into()),
        Err(BeginSessionAttemptError::AlreadyRunning(summary)) if summary.key() == retried.key()
    ));
    assert_eq!(
        store
            .load_retry_boundary(retried.key())
            .unwrap()
            .unwrap()
            .prompt(),
        "private"
    );
    assert_eq!(
        store
            .finish_session_attempt(retried.key(), SessionAttemptStatus::Failed, 33)
            .unwrap(),
        AttemptFinishOutcome::Finished
    );
    let connection = Connection::open(store.database_path()).unwrap();
    assert_eq!(normalized_counts(&connection), (2, 0, 0, 0));
    assert_eq!(
        connection
            .query_row(
                "SELECT status, finished_at FROM session_attempts WHERE id = ?1",
                [attempt.key().attempt_id()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            )
            .unwrap(),
        ("interrupted".into(), 30)
    );

    let successful = store
        .begin_session_attempt(&metadata, "successful history".into())
        .unwrap();
    assert_eq!(
        store
            .persist_completed_session_attempt(
                successful.key(),
                &metadata,
                &turn(vec![
                    Message {
                        role: Role::System,
                        parts: vec![MessagePart::Text("system".into())],
                    },
                    Message {
                        role: Role::User,
                        parts: vec![MessagePart::Text("successful history".into())],
                    },
                    Message {
                        role: Role::Assistant,
                        parts: vec![MessagePart::Text("history answer".into())],
                    },
                ]),
                40,
            )
            .unwrap(),
        AttemptFinishOutcome::Finished
    );

    let history = store.load_session_for_resume(metadata.id).unwrap();
    let completion_first = store
        .begin_session_attempt(&history.metadata, "completion first".into())
        .unwrap();
    assert_eq!(
        store
            .persist_completed_session_attempt(
                completion_first.key(),
                &history.metadata,
                &turn(vec![
                    Message {
                        role: Role::User,
                        parts: vec![MessagePart::Text("completion first".into())],
                    },
                    Message {
                        role: Role::Assistant,
                        parts: vec![MessagePart::Text("completion answer".into())],
                    },
                ]),
                50,
            )
            .unwrap(),
        AttemptFinishOutcome::Finished
    );
    assert_eq!(
        store
            .recover_running_attempt(completion_first.key(), 51)
            .unwrap(),
        RecoveryOutcome::Stale
    );

    let before_recovery = store.load_session_for_resume(metadata.id).unwrap();
    let recovery_first = store
        .begin_session_attempt(&before_recovery.metadata, "recovery first".into())
        .unwrap();
    assert!(matches!(
        store
            .recover_running_attempt(recovery_first.key(), 60)
            .unwrap(),
        RecoveryOutcome::Recovered(_)
    ));
    assert_eq!(
        store
            .persist_completed_session_attempt(
                recovery_first.key(),
                &before_recovery.metadata,
                &turn(vec![
                    Message {
                        role: Role::User,
                        parts: vec![MessagePart::Text("must not append".into())],
                    },
                    Message {
                        role: Role::Assistant,
                        parts: vec![MessagePart::Text("must not append".into())],
                    },
                ]),
                61,
            )
            .unwrap(),
        AttemptFinishOutcome::Stale
    );
    assert_eq!(
        store
            .recover_running_attempt(
                AttemptKey::new(8, recovery_first.key().attempt_id()).unwrap(),
                62,
            )
            .unwrap(),
        RecoveryOutcome::Stale
    );
    let preserved = store.load_session_for_resume(metadata.id).unwrap();
    assert_eq!(preserved.metadata.completed_turn_count, 2);
    assert_eq!(preserved.messages.len(), 5);
}

fn attempt_status(status: SessionAttemptStatus) -> &'static str {
    match status {
        SessionAttemptStatus::Running => "running",
        SessionAttemptStatus::Completed => "completed",
        SessionAttemptStatus::Cancelled => "cancelled",
        SessionAttemptStatus::Failed => "failed",
        SessionAttemptStatus::ProviderError => "provider_error",
        SessionAttemptStatus::Interrupted => "interrupted",
    }
}

fn attempt_failure_kind(kind: SessionAttemptFailureKind) -> &'static str {
    match kind {
        SessionAttemptFailureKind::Cancelled => "cancelled",
        SessionAttemptFailureKind::Failed => "failed",
        SessionAttemptFailureKind::ProviderError => "provider_error",
        SessionAttemptFailureKind::Interrupted => "interrupted",
    }
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
        parent_session_id: None,
        fork_message_count: None,
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
        parent_session_id: None,
        fork_message_count: None,
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

    let connection = Connection::open(directory.join("agens.db")).unwrap();
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
}

#[test]
fn atomically_rolls_back_failed_writes_and_appends_from_stale_metadata() {
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
        parent_session_id: None,
        fork_message_count: None,
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
    assert_eq!(
        stale
            .persist_completed_session_turn(&metadata, &completed)
            .unwrap()
            .completed_turn_count,
        2
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
        (2, true)
    );
    assert_eq!(normalized_counts(&connection), (1, 2, 6, 6));
}

#[test]
fn appends_completed_turn_when_a_concurrent_subagent_turn_advanced_the_count() {
    let directory = directory();
    let metadata = SessionMetadata {
        id: 11,
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
    };
    let completed = |text: &str| {
        turn(vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text(text.into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text(text.into())],
            },
        ])
    };

    let mut store = SessionStore::open(&directory).unwrap();
    let resumed = store
        .persist_completed_session_turn(&metadata, &completed("history"))
        .unwrap();
    assert_eq!(resumed.completed_turn_count, 1);

    let parent_snapshot = resumed.clone();
    let attempt = store
        .begin_session_attempt(&parent_snapshot, "parent prompt".into())
        .unwrap();
    let subagent = store
        .persist_completed_session_turn(&parent_snapshot, &completed("subagent"))
        .unwrap();
    assert_eq!(subagent.completed_turn_count, 2);
    let second_subagent = store
        .persist_completed_session_turn(&parent_snapshot, &completed("second subagent"))
        .unwrap();
    assert_eq!(second_subagent.completed_turn_count, 3);

    assert_eq!(
        store
            .persist_completed_session_attempt(
                attempt.key(),
                &parent_snapshot,
                &completed("parent"),
                99,
            )
            .unwrap(),
        AttemptFinishOutcome::Finished
    );

    let stored = store.load_session_for_resume(metadata.id).unwrap();
    assert_eq!(stored.metadata.completed_turn_count, 4);
    assert_eq!(stored.metadata.updated_at, 99);
    assert!(stored.metadata.resumable);
    assert_eq!(
        stored
            .messages
            .iter()
            .filter_map(|message| match message.parts.first() {
                Some(MessagePart::Text(text)) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![
            "history",
            "history",
            "subagent",
            "subagent",
            "second subagent",
            "second subagent",
            "parent",
            "parent",
        ]
    );
}

#[test]
fn media_parts_round_trip_without_source_path() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let metadata = SessionMetadata {
        id: 11,
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
    };
    let completed = turn(vec![
        Message {
            role: Role::User,
            parts: vec![
                MessagePart::Text("what is this".into()),
                MessagePart::Media {
                    media_id: media.id,
                    mime: media.mime.clone(),
                },
            ],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("an image".into())],
        },
    ]);
    let mut store = SessionStore::open(&directory).unwrap();
    let attempt = store
        .begin_session_attempt(&metadata, "retry with media".into())
        .unwrap();

    assert_eq!(
        store
            .persist_completed_session_attempt(attempt.key(), &metadata, &completed, 21)
            .unwrap(),
        AttemptFinishOutcome::Finished
    );

    let stored = store.load_session_for_resume(metadata.id).unwrap();
    assert_eq!(
        stored.messages[0].parts,
        vec![
            MessagePart::Text("what is this".into()),
            MessagePart::Media {
                media_id: media.id,
                mime: "image/png".into(),
            },
        ]
    );

    let connection = Connection::open(store.database_path()).unwrap();
    let (kind, media_id, mime, text): (String, Option<i64>, Option<String>, Option<String>) =
        connection
            .query_row(
                "SELECT kind, media_id, mime, text FROM message_parts
                 WHERE session_id = ?1 AND message_sequence = 1 AND sequence = 1",
                [metadata.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
    assert_eq!(kind, "media");
    assert_eq!(media_id, Some(media.id));
    assert_eq!(mime.as_deref(), Some("image/png"));
    assert_eq!(text, None);
}

#[test]
fn begin_session_attempt_allows_empty_prompt_when_media_ids_present() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"media-only", "image/png").unwrap();
    let metadata = SessionMetadata {
        id: 21,
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
    };
    let mut store = SessionStore::open(&directory).unwrap();

    let attempt = store
        .begin_session_attempt_with_media(&metadata, String::new(), vec![media.id])
        .expect("empty prompt with media must begin");
    let boundary = store.load_retry_boundary(attempt.key()).unwrap().unwrap();
    assert_eq!(boundary.prompt(), "");
    assert_eq!(boundary.media_ids(), &[media.id]);

    assert!(
        store
            .begin_session_attempt_with_media(
                &SessionMetadata {
                    id: 22,
                    project: "other".into(),
                    title: "other".into(),
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
                },
                String::new(),
                Vec::new(),
            )
            .is_err(),
        "empty prompt without media must still fail"
    );
}

#[test]
fn retry_boundary_round_trips_media_ids_without_source_paths() {
    let directory = directory();
    let first = ingest_media_bytes(&directory, b"retry-a", "image/png").unwrap();
    let second = ingest_media_bytes(&directory, b"retry-b", "image/jpeg").unwrap();
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
        parent_session_id: None,
        fork_message_count: None,
    };
    let mut store = SessionStore::open(&directory).unwrap();
    let attempt = store
        .begin_session_attempt_with_media(
            &metadata,
            "private multimodal draft".into(),
            vec![first.id, second.id],
        )
        .unwrap();

    let boundary = store.load_retry_boundary(attempt.key()).unwrap().unwrap();
    assert_eq!(boundary.prompt(), "private multimodal draft");
    assert_eq!(boundary.media_ids(), &[first.id, second.id]);

    let connection = Connection::open(store.database_path()).unwrap();
    let stored_json: Option<String> = connection
        .query_row(
            "SELECT retry_media_ids FROM session_attempts WHERE id = ?1",
            [attempt.key().attempt_id()],
            |row| row.get(0),
        )
        .unwrap();
    let json = stored_json.expect("retry_media_ids must be stored");
    assert_eq!(json, format!("[{},{}]", first.id, second.id));
    assert!(!json.contains('/'));
    assert!(!json.contains("source"));

    let empty = store
        .begin_session_attempt_with_media(
            &SessionMetadata {
                id: 13,
                project: "other".into(),
                title: "other".into(),
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
            },
            "text only".into(),
            Vec::new(),
        )
        .unwrap();
    let empty_boundary = store.load_retry_boundary(empty.key()).unwrap().unwrap();
    assert_eq!(empty_boundary.media_ids(), &[] as &[i64]);
    let empty_json: Option<String> = connection
        .query_row(
            "SELECT retry_media_ids FROM session_attempts WHERE id = ?1",
            [empty.key().attempt_id()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(empty_json, None);
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
        parent_session_id: None,
        fork_message_count: None,
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
            latest_attempt: None,
        }
    );
    drop(store);

    let reopened = SessionStore::open(&directory).unwrap();
    assert_eq!(reopened.list_sessions().unwrap(), vec![updated]);
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
        parent_session_id: None,
        fork_message_count: None,
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
        parent_session_id: None,
        fork_message_count: None,
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
    assert!(schema.ends_with(
        "provider_id,model_id,reasoning_effort,confinement_root,bypass_permission_prompts,parent_session_id,fork_message_count"
    ));
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
}

#[test]
fn a_freshly_created_session_records_its_own_confinement_root() {
    let directory = directory();
    let metadata = SessionMetadata {
        id: 0,
        project: "/original/root".into(),
        title: "confinement".into(),
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
    };
    let mut store = SessionStore::open(&directory).unwrap();
    let attempt = store
        .begin_session_attempt(&metadata, "prompt".into())
        .unwrap();

    assert_eq!(
        store.confinement_root(attempt.key().session_id()).unwrap(),
        std::path::PathBuf::from("/original/root")
    );
}

#[test]
fn confinement_root_falls_back_to_project_for_rows_recorded_before_the_column_existed() {
    let directory = directory();
    let metadata = SessionMetadata {
        id: 0,
        project: "/legacy/root".into(),
        title: "legacy".into(),
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
    };
    let mut store = SessionStore::open(&directory).unwrap();
    let attempt = store
        .begin_session_attempt(&metadata, "prompt".into())
        .unwrap();
    let session_id = attempt.key().session_id();

    Connection::open(store.database_path())
        .unwrap()
        .execute(
            "UPDATE sessions SET confinement_root = NULL WHERE id = ?1",
            [session_id],
        )
        .unwrap();

    assert_eq!(
        store.confinement_root(session_id).unwrap(),
        std::path::PathBuf::from("/legacy/root")
    );
}

#[test]
fn a_freshly_created_session_has_no_recorded_bypass_permission_prompts_value() {
    let directory = directory();
    let metadata = SessionMetadata {
        id: 0,
        project: "/original/root".into(),
        title: "bypass".into(),
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
    };
    let mut store = SessionStore::open(&directory).unwrap();
    let attempt = store
        .begin_session_attempt(&metadata, "prompt".into())
        .unwrap();

    assert_eq!(
        store
            .bypass_permission_prompts(attempt.key().session_id())
            .unwrap(),
        None
    );
}

#[test]
fn setting_bypass_permission_prompts_round_trips_true_and_false() {
    let directory = directory();
    let metadata = SessionMetadata {
        id: 0,
        project: "/original/root".into(),
        title: "bypass".into(),
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
    };
    let mut store = SessionStore::open(&directory).unwrap();
    let attempt = store
        .begin_session_attempt(&metadata, "prompt".into())
        .unwrap();
    let session_id = attempt.key().session_id();

    store
        .set_bypass_permission_prompts(session_id, true)
        .unwrap();
    assert_eq!(
        store.bypass_permission_prompts(session_id).unwrap(),
        Some(true)
    );

    store
        .set_bypass_permission_prompts(session_id, false)
        .unwrap();
    assert_eq!(
        store.bypass_permission_prompts(session_id).unwrap(),
        Some(false)
    );
}

/// A session with three completed turns, each recorded through its own attempt, so a truncation
/// can be checked against real turn, message, part and attempt rows rather than a hand-built one.
fn session_with_three_turns(directory: &std::path::Path, media_id: i64) -> (SessionStore, i64) {
    let metadata = SessionMetadata {
        id: 0,
        project: "project".into(),
        title: "history".into(),
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
    };
    let turns = [
        turn(vec![
            Message {
                role: Role::User,
                parts: vec![
                    MessagePart::Text("look at this".into()),
                    MessagePart::Media {
                        media_id,
                        mime: "image/png".into(),
                    },
                ],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text("a diagram".into())],
            },
        ]),
        turn(vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("search it".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::ToolCall {
                    id: "call-1".into(),
                    name: "search".into(),
                    input: r#"{"query":"answer"}"#.into(),
                }],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "call-1".into(),
                    content: "found".into(),
                    is_error: false,
                }],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text("here it is".into())],
            },
        ]),
        turn(vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("the turn the reader takes back".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text("undone".into())],
            },
        ]),
    ];

    let mut store = SessionStore::open(directory).unwrap();
    let mut session_id = 0;
    for (index, completed) in turns.iter().enumerate() {
        let started_at = 20 + index as i64;
        let metadata = SessionMetadata {
            id: session_id,
            updated_at: started_at,
            ..metadata.clone()
        };
        let attempt = store
            .begin_session_attempt(&metadata, format!("prompt-{index}"))
            .unwrap();
        session_id = attempt.key().session_id();
        let metadata = SessionMetadata {
            id: session_id,
            ..metadata
        };
        assert_eq!(
            store
                .persist_completed_session_attempt(attempt.key(), &metadata, completed, started_at)
                .unwrap(),
            AttemptFinishOutcome::Finished
        );
    }

    (store, session_id)
}

fn attempt_turn_sequences(store: &SessionStore, session_id: i64) -> Vec<Option<i64>> {
    let connection = Connection::open(store.database_path()).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT completed_turn_sequence FROM session_attempts
             WHERE session_id = ?1 ORDER BY sequence",
        )
        .unwrap();
    statement
        .query_map([session_id], |row| row.get::<_, Option<i64>>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// The claim the whole feature rests on: after a truncation the session reloads through the same
/// path a resume uses, the taken-back turn is gone, and everything before it comes back unchanged.
#[test]
fn truncating_a_session_drops_the_later_turns_and_leaves_the_prefix_unchanged() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);
    let before = store.load_session_for_resume(session_id).unwrap();
    assert_eq!(before.messages.len(), 8);

    store.truncate_session_history(session_id, 6, 8).unwrap();

    let after = store.load_session_for_resume(session_id).unwrap();
    assert_eq!(after.messages, before.messages[..6].to_vec());
    assert_eq!(after.metadata.completed_turn_count, 2);
    assert!(after.metadata.resumable);
    assert_eq!(
        after.messages[0].parts[1],
        MessagePart::Media {
            media_id: media.id,
            mime: "image/png".into(),
        },
        "a surviving media part still resolves to its blob"
    );

    let connection = Connection::open(store.database_path()).unwrap();
    assert_eq!(normalized_counts(&connection), (1, 2, 6, 7));
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM media", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1,
        "media rows are shared by content hash and are not a session's to delete"
    );
    assert_eq!(
        attempt_turn_sequences(&store, session_id),
        vec![Some(1), Some(2)],
        "the attempt that completed the dropped turn goes with it"
    );
}

/// The surviving prefix is a message boundary, not a turn boundary: one that lands inside a turn
/// keeps that turn, with only the messages past the boundary removed.
#[test]
fn a_surviving_prefix_inside_a_turn_keeps_the_turn_it_lands_in() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);
    let before = store.load_session_for_resume(session_id).unwrap();

    store.truncate_session_history(session_id, 3, 8).unwrap();

    let after = store.load_session_for_resume(session_id).unwrap();
    assert_eq!(after.messages, before.messages[..3].to_vec());
    assert_eq!(after.metadata.completed_turn_count, 2);
    assert_eq!(
        attempt_turn_sequences(&store, session_id),
        vec![Some(1), Some(2)],
        "the partially truncated turn still has its attempt"
    );

    let connection = Connection::open(store.database_path()).unwrap();
    assert_eq!(normalized_counts(&connection), (1, 2, 3, 4));
}

/// Truncating everything leaves a session that exists and can be written to again, with counters
/// that agree with the nothing it now holds.
#[test]
fn truncating_a_session_to_nothing_clears_its_history_and_counters() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);

    store.truncate_session_history(session_id, 0, 8).unwrap();

    let connection = Connection::open(store.database_path()).unwrap();
    assert_eq!(normalized_counts(&connection), (1, 0, 0, 0));
    assert_eq!(
        connection
            .query_row(
                "SELECT completed_turn_count, resumable FROM sessions WHERE id = ?1",
                [session_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap(),
        (0, 0)
    );
    assert!(
        attempt_turn_sequences(&store, session_id).is_empty(),
        "no attempt survives a turn it completed"
    );
}

/// A truncation that cannot finish must leave the session exactly as it was: a session missing
/// half of a turn is one that cannot be resumed at all.
#[test]
fn a_truncation_that_cannot_finish_leaves_every_row_in_place() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);
    let before = store.load_session_for_resume(session_id).unwrap();

    let connection = Connection::open(store.database_path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER refuse_turn_delete BEFORE DELETE ON turns
             BEGIN SELECT RAISE(ABORT, 'turn delete refused'); END;",
        )
        .unwrap();

    assert!(
        store.truncate_session_history(session_id, 6, 8).is_err(),
        "a refused delete must be reported, not swallowed"
    );
    assert_eq!(
        store.load_session_for_resume(session_id).unwrap(),
        before,
        "the messages deleted before the failure are rolled back with it"
    );
    assert_eq!(normalized_counts(&connection), (1, 3, 8, 9));
    assert_eq!(
        attempt_turn_sequences(&store, session_id),
        vec![Some(1), Some(2), Some(3)]
    );

    connection
        .execute_batch("DROP TRIGGER refuse_turn_delete;")
        .unwrap();
    store.truncate_session_history(session_id, 6, 8).unwrap();
    assert_eq!(normalized_counts(&connection), (1, 2, 6, 7));
}

/// A session that already holds no more than the surviving prefix has nothing to truncate, and a
/// truncation asked for anyway must not touch it.
#[test]
fn truncating_past_the_stored_history_changes_nothing() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);
    let before = store.load_session_for_resume(session_id).unwrap();

    store.truncate_session_history(session_id, 12, 12).unwrap();

    assert_eq!(store.load_session_for_resume(session_id).unwrap(), before);
}

/// A turn persisted after the measured history — a sub-agent turn recorded out of band, or one
/// that raced the truncation — belongs to no undone range, so the truncation must leave it alone
/// while still dropping everything the caller did measure past the prefix.
#[test]
fn a_turn_persisted_after_the_measured_history_survives_the_truncation() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);
    let before = store.load_session_for_resume(session_id).unwrap();
    let mut metadata = before.metadata.clone();
    metadata.updated_at = 40;
    let out_of_band = vec![
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("a sub-agent task".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("a sub-agent turn".into())],
        },
    ];
    store
        .persist_completed_session_turn(&metadata, &turn(out_of_band.clone()))
        .unwrap();

    store
        .truncate_session_history(session_id, 6, before.messages.len())
        .unwrap();

    let after = store.load_session_for_resume(session_id).unwrap();
    let mut expected = before.messages[..6].to_vec();
    expected.extend(out_of_band);
    assert_eq!(
        after.messages, expected,
        "the undone turn goes and the turn nobody undid stays"
    );
    assert_eq!(
        after.metadata.completed_turn_count, 3,
        "the surviving out-of-band turn is still counted"
    );
}

/// Everything the parent must still hold after a fork, read back through the paths a resume and
/// the session browser use rather than by inspecting rows one at a time.
fn parent_evidence(store: &SessionStore, session_id: i64) -> (StoredSession, Vec<Option<i64>>) {
    (
        store.load_session_for_resume(session_id).unwrap(),
        attempt_turn_sequences(store, session_id),
    )
}

fn session_attempt_count(store: &SessionStore, session_id: i64) -> i64 {
    let connection = Connection::open(store.database_path()).unwrap();
    connection
        .query_row(
            "SELECT count(*) FROM session_attempts WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .unwrap()
}

/// The claim the feature rests on: a fork is a second session holding a copy of the prefix, and
/// the session it was forked from is left exactly as it was.
#[test]
fn forking_copies_the_prefix_and_leaves_the_parent_untouched() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);
    let before = parent_evidence(&store, session_id);
    assert_eq!(before.0.messages.len(), 8);

    let fork_id = store.fork_session(session_id, 6).unwrap();

    assert_ne!(fork_id, session_id);
    assert_eq!(
        parent_evidence(&store, session_id),
        before,
        "the parent keeps its messages, turns, counters and attempt history"
    );

    let fork = store.load_session_for_resume(fork_id).unwrap();
    assert_eq!(fork.messages, before.0.messages[..6].to_vec());
    assert_eq!(fork.metadata.parent_session_id, Some(session_id));
    assert_eq!(fork.metadata.fork_message_count, Some(6));
    assert_eq!(fork.metadata.project, before.0.metadata.project);
    assert_eq!(fork.metadata.title, before.0.metadata.title);
    assert_eq!(fork.metadata.active_agent, before.0.metadata.active_agent);
    assert_eq!(fork.metadata.completed_turn_count, 2);
    assert!(fork.metadata.resumable);
    assert_eq!(fork.latest_attempt, None, "a fork has run nothing yet");
    assert_eq!(session_attempt_count(&store, fork_id), 0);

    let connection = Connection::open(store.database_path()).unwrap();
    assert_eq!(normalized_counts(&connection), (2, 5, 14, 16));
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM media", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1,
        "media rows are shared by content hash and a fork references rather than copies them"
    );
    assert_eq!(
        fork.messages[0].parts[1],
        MessagePart::Media {
            media_id: media.id,
            mime: "image/png".into(),
        },
        "the copied media part resolves to the same shared blob"
    );
    assert_eq!(
        fork.messages[3].parts[0],
        MessagePart::ToolCall {
            id: "call-1".into(),
            name: "search".into(),
            input: r#"{"query":"answer"}"#.into(),
        },
    );
    assert_eq!(
        fork.messages[4].parts[0],
        MessagePart::ToolResult {
            tool_call_id: "call-1".into(),
            content: "found".into(),
            is_error: false,
        },
    );
}

/// The cut point is a message boundary, not a turn boundary: one that lands inside a turn copies
/// that turn with only the messages under the cut, exactly as a truncation keeps the turn it
/// lands in.
#[test]
fn forking_inside_a_turn_copies_the_turn_it_lands_in() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);
    let before = parent_evidence(&store, session_id);

    let fork_id = store.fork_session(session_id, 3).unwrap();

    let fork = store.load_session_for_resume(fork_id).unwrap();
    assert_eq!(fork.messages, before.0.messages[..3].to_vec());
    assert_eq!(
        fork.metadata.completed_turn_count, 2,
        "the turn the cut lands in is copied with the messages under the cut"
    );
    assert!(fork.metadata.resumable);
    assert_eq!(fork.metadata.fork_message_count, Some(3));
    assert_eq!(
        parent_evidence(&store, session_id),
        before,
        "a mid-turn fork still leaves the parent whole"
    );
}

/// Message sequences are per-session, so a fork keeps the parent's numbering rather than
/// renumbering from one; the turn a message belongs to has to be copied under the same sequence
/// for the foreign key to hold at all.
#[test]
fn a_fork_keeps_the_message_and_turn_sequences_it_copied() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);

    let fork_id = store.fork_session(session_id, 6).unwrap();

    let connection = Connection::open(store.database_path()).unwrap();
    let sequences = |table: &str, id: i64| {
        let mut statement = connection
            .prepare(&format!(
                "SELECT sequence FROM {table} WHERE session_id = ?1 ORDER BY sequence"
            ))
            .unwrap();
        statement
            .query_map([id], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };

    assert_eq!(sequences("messages", fork_id), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(sequences("turns", fork_id), vec![1, 2]);
    assert_eq!(sequences("turns", session_id), vec![1, 2, 3]);
}

/// Neither rejection is clamped into a fork of whatever happened to be stored.
#[test]
fn forking_refuses_an_unknown_source_and_a_prefix_outside_the_history() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);

    assert_eq!(
        store.fork_session(session_id + 500, 1).unwrap_err(),
        ForkSessionError::UnknownSession(session_id + 500)
    );
    assert_eq!(
        store.fork_session(session_id, 9).unwrap_err(),
        ForkSessionError::PrefixOutOfRange {
            requested: 9,
            available: 8,
        }
    );
    assert_eq!(
        store.fork_session(session_id, 0).unwrap_err(),
        ForkSessionError::PrefixOutOfRange {
            requested: 0,
            available: 8,
        }
    );

    let connection = Connection::open(store.database_path()).unwrap();
    assert_eq!(
        normalized_counts(&connection),
        (1, 3, 8, 9),
        "a refused fork writes nothing"
    );
}

/// The lineage a browser needs: the parent lists its forks, a fork lists none of its own, and the
/// ordinary session page carries the lineage columns for every row it already returned.
#[test]
fn session_children_and_pages_carry_the_fork_lineage() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);

    let first_fork = store.fork_session(session_id, 2).unwrap();
    let second_fork = store.fork_session(session_id, 6).unwrap();
    let grandchild = store.fork_session(first_fork, 1).unwrap();

    let children = store.list_session_children(session_id).unwrap();
    assert_eq!(
        children
            .iter()
            .map(|child| child.metadata.id)
            .collect::<Vec<_>>(),
        vec![first_fork, second_fork]
    );
    assert_eq!(children[0].metadata.fork_message_count, Some(2));
    assert_eq!(children[1].metadata.fork_message_count, Some(6));
    assert_eq!(children[0].latest_attempt, None);
    assert_eq!(
        store
            .list_session_children(first_fork)
            .unwrap()
            .iter()
            .map(|child| child.metadata.id)
            .collect::<Vec<_>>(),
        vec![grandchild]
    );
    assert_eq!(
        store.list_session_children(second_fork).unwrap(),
        Vec::new()
    );

    let listed = collect_session_pages(&store, Some("project"), "");
    let lineage = listed
        .iter()
        .map(|session| {
            (
                session.metadata.id,
                session.metadata.parent_session_id,
                session.metadata.fork_message_count,
            )
        })
        .collect::<Vec<_>>();
    assert!(lineage.contains(&(session_id, None, None)));
    assert!(lineage.contains(&(first_fork, Some(session_id), Some(2))));
    assert!(lineage.contains(&(second_fork, Some(session_id), Some(6))));
    assert!(lineage.contains(&(grandchild, Some(first_fork), Some(1))));
}

/// The step a lineage browser climbs with, and its answer for the two sessions that have nowhere
/// to climb to: one that was started rather than forked, and one that is not there at all.
#[test]
fn a_session_parent_is_readable_and_absent_for_a_root_and_an_unknown_session() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);

    let fork_id = store.fork_session(session_id, 4).unwrap();

    assert_eq!(store.session_parent(fork_id).unwrap(), Some(session_id));
    assert_eq!(store.session_parent(session_id).unwrap(), None);
    assert_eq!(store.session_parent(session_id + 500).unwrap(), None);
}

/// A lineage lists a session because something was forked from it, so reading one applies no
/// resumability filter — and a session that is gone reads as an absence, not a failure.
#[test]
fn reading_one_session_ignores_resumability_and_reports_a_missing_one_as_absent() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);
    let fork_id = store.fork_session(session_id, 4).unwrap();

    let read = store.read_session(session_id).unwrap().unwrap();
    assert_eq!(read.metadata.id, session_id);
    assert_eq!(read.metadata.parent_session_id, None);
    assert!(
        read.messages.is_empty(),
        "a lineage row carries metadata, not a transcript"
    );
    assert_eq!(
        store
            .read_session(fork_id)
            .unwrap()
            .unwrap()
            .metadata
            .fork_message_count,
        Some(4)
    );

    assert_eq!(store.read_session(session_id + 500).unwrap(), None);
}

/// The translation a caller needs because it holds a count and the fork needs a sequence. A
/// truncation leaves the surviving messages with the numbering they already had, so the two stop
/// agreeing exactly when a caller would most likely assume they still do.
#[test]
fn a_message_count_resolves_to_its_sequence_across_a_gap_left_by_a_truncation() {
    let directory = directory();
    let media = ingest_media_bytes(&directory, b"session-media", "image/png").unwrap();
    let (mut store, session_id) = session_with_three_turns(&directory, media.id);

    assert_eq!(store.message_sequence_at(session_id, 1).unwrap(), Some(1));
    assert_eq!(store.message_sequence_at(session_id, 8).unwrap(), Some(8));
    assert_eq!(store.message_sequence_at(session_id, 0).unwrap(), None);
    assert_eq!(store.message_sequence_at(session_id, 9).unwrap(), None);

    // Drop the third turn's messages, then re-append: the survivors keep 1..=6 and the new
    // messages continue past the numbers the deleted ones held.
    store.truncate_session_history(session_id, 6, 8).unwrap();
    let sequences: Vec<i64> = Connection::open(store.database_path())
        .unwrap()
        .prepare("SELECT sequence FROM messages WHERE session_id = ?1 ORDER BY sequence")
        .unwrap()
        .query_map([session_id], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(sequences, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(store.message_sequence_at(session_id, 6).unwrap(), Some(6));
    assert_eq!(store.message_sequence_at(session_id, 7).unwrap(), None);

    // The sequence a count resolves to is the one the fork must be given.
    let fork_id = store
        .fork_session(
            session_id,
            store.message_sequence_at(session_id, 4).unwrap().unwrap(),
        )
        .unwrap();
    assert_eq!(
        store
            .load_session_for_resume(fork_id)
            .unwrap()
            .messages
            .len(),
        4
    );
}

/// Reopening an already-migrated database applies nothing a second time: the ledger keeps one row
/// for the fork migration and the columns and forest index it added stay single.
#[test]
fn the_fork_lineage_migration_is_idempotent_on_an_already_migrated_database() {
    let directory = directory();
    let store = SessionStore::open(&directory).unwrap();
    drop(store);

    let store = SessionStore::open(&directory).unwrap();
    let connection = Connection::open(store.database_path()).unwrap();

    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM schema_migrations WHERE id = '0011_session_fork_lineage'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM pragma_table_info('sessions')
                 WHERE name IN ('parent_session_id', 'fork_message_count')",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        2
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema
                 WHERE type = 'index' AND name = 'sessions_forest'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}
