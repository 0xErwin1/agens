use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_core::{
    CompletedSessionTurn, Message, MessagePart, Role, SessionAttemptStatus, SessionMessage,
    SessionMetadata,
};
use agens_store::{SessionStore, TranscriptCursor};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-store-transcript-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

/// Metadata as it stands *before* the attempt is persisted: the store is what
/// turns a session resumable, and `resumable` must agree with the turn count.
fn metadata(id: i64) -> SessionMetadata {
    SessionMetadata {
        id,
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

fn text(role: Role, body: &str) -> Message {
    Message {
        role,
        parts: vec![MessagePart::Text(body.into())],
    }
}

/// A message whose parts must never be split across a page boundary.
fn multi_part_reply() -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![
            MessagePart::Text("first".into()),
            MessagePart::Reasoning("thinking".into()),
            MessagePart::Text("second".into()),
        ],
    }
}

/// Persists one completed attempt per turn. A turn is a single exchange, so a
/// thread with several user messages is several turns, and the metadata has to
/// carry the growing turn count for each one.
fn session_with(directory: &std::path::Path, id: i64, turns: Vec<Vec<Message>>) -> SessionStore {
    let mut store = SessionStore::open(directory).unwrap();
    let mut metadata = metadata(id);

    for (index, messages) in turns.into_iter().enumerate() {
        let attempt = store
            .begin_session_attempt(&metadata, "retry".into())
            .unwrap();
        store
            .persist_completed_session_attempt(
                attempt.key(),
                &metadata,
                &turn(messages),
                21 + index as i64,
            )
            .unwrap();
        metadata.completed_turn_count += 1;
        metadata.resumable = true;
    }

    store
}

fn exchange(question: &str, answer: &str) -> Vec<Message> {
    vec![text(Role::User, question), text(Role::Assistant, answer)]
}

/// Bounded on purpose: a cursor that fails to advance would otherwise hang the
/// suite instead of failing it.
fn collect_pages(store: &SessionStore, id: i64, page_size: usize) -> (Vec<Message>, usize) {
    const MAX_PAGES: usize = 32;

    let mut cursor: Option<TranscriptCursor> = None;
    let mut messages = Vec::new();
    let mut pages = 0;

    loop {
        let page = store.read_transcript_page(id, cursor, page_size).unwrap();
        pages += 1;
        messages.extend(page.messages);
        cursor = page.next_cursor;

        if cursor.is_none() {
            return (messages, pages);
        }
        assert!(pages < MAX_PAGES, "the cursor is not advancing");
    }
}

#[test]
fn reads_the_whole_thread_in_order_when_the_page_holds_it() {
    let directory = directory();
    let store = session_with(&directory, 7, vec![exchange("question", "answer")]);

    let page = store.read_transcript_page(7, None, 64).unwrap();

    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].role, Role::User);
    assert_eq!(
        page.messages[0].parts,
        vec![MessagePart::Text("question".into())]
    );
    assert_eq!(page.messages[1].role, Role::Assistant);
    assert!(page.next_cursor.is_none());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn pages_the_thread_by_message_without_losing_or_repeating_one() {
    let directory = directory();
    let store = session_with(
        &directory,
        7,
        vec![exchange("one", "two"), exchange("three", "four")],
    );

    let (paged, pages) = collect_pages(&store, 7, 1);
    let whole = store.read_transcript_page(7, None, 64).unwrap().messages;

    assert_eq!(paged, whole);
    assert_eq!(paged.len(), 4);
    assert_eq!(pages, 4);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_multi_part_message_is_never_split_across_a_page_boundary() {
    let directory = directory();
    let store = session_with(
        &directory,
        7,
        vec![
            vec![text(Role::User, "question"), multi_part_reply()],
            exchange("follow up", "done"),
        ],
    );

    let (paged, _) = collect_pages(&store, 7, 1);

    assert_eq!(paged.len(), 4);
    assert_eq!(paged[1].parts.len(), 3, "{:?}", paged[1]);
    assert_eq!(paged[1], multi_part_reply());

    fs::remove_dir_all(directory).unwrap();
}

/// The transcript is evidence, so it stays readable for threads that resume
/// refuses: a failed or exhausted run is exactly the one worth reading.
#[test]
fn reads_a_thread_that_is_not_resumable() {
    let directory = directory();
    let mut store = SessionStore::open(&directory).unwrap();
    let metadata = metadata(7);
    let attempt = store
        .begin_session_attempt(&metadata, "retry".into())
        .unwrap();
    store
        .persist_partial_session_attempt(
            attempt.key(),
            &metadata,
            &turn(vec![text(Role::User, "unfinished")]),
            SessionAttemptStatus::Failed,
            21,
        )
        .unwrap();

    let page = store.read_transcript_page(7, None, 64).unwrap();

    assert_eq!(page.messages, vec![text(Role::User, "unfinished")]);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn distinguishes_an_unknown_session_from_an_empty_thread() {
    let directory = directory();
    let mut store = SessionStore::open(&directory).unwrap();
    let metadata = metadata(7);
    store
        .begin_session_attempt(&metadata, "retry".into())
        .unwrap();

    let empty = store.read_transcript_page(7, None, 64).unwrap();
    assert!(empty.messages.is_empty());
    assert!(empty.next_cursor.is_none());

    assert!(store.read_transcript_page(404, None, 64).is_err());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_a_zero_page_size_and_bounds_an_oversized_one() {
    let directory = directory();
    let store = session_with(&directory, 7, vec![exchange("one", "two")]);

    assert!(store.read_transcript_page(7, None, 0).is_err());
    assert!(store.read_transcript_page(7, None, usize::MAX).is_ok());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_cursor_past_the_end_yields_an_empty_final_page() {
    let directory = directory();
    let store = session_with(&directory, 7, vec![exchange("only", "reply")]);

    let page = store
        .read_transcript_page(7, Some(TranscriptCursor::new(i64::MAX)), 64)
        .unwrap();

    assert!(page.messages.is_empty());
    assert!(page.next_cursor.is_none());

    fs::remove_dir_all(directory).unwrap();
}
