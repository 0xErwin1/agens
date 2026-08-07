//! Turning a reader's "fork here" into a cut the store can make, and a session's
//! lineage into the rows the tree overlay lists.
//!
//! The terminal points at a transcript block and counts the turns it has drawn;
//! the store cuts at a message sequence. Neither can do the other's arithmetic:
//! the terminal never sees the persisted messages a turn expands into, and the
//! store never sees which of them were drawn. This module is where the two meet.

use agens_core::{Message, Role};
use agens_store::{SessionStore, SessionStoreError, StoredSession};
use agens_tui::DialogEntry;

use crate::resume::is_persisted_subagent_turn;
use crate::session::session_dialog_entry;

/// Rows a lineage listing returns before it stops descending.
///
/// The overlay itself keeps only the first 64 rows, so gathering past that point
/// would be work whose result is dropped. The bound is the listing's, not the
/// lineage's: a forest wider than this is truncated, never rejected.
const MAX_LINEAGE_ROWS: usize = 64;

/// How deep a lineage listing descends before it stops following forks.
///
/// A fork of a fork is ordinary, but a chain this long is a loop far more often
/// than it is a real lineage, and a parent id carries no constraint that would
/// stop one: nothing prevents a row from naming an ancestor as its parent.
const MAX_LINEAGE_DEPTH: usize = 32;

/// How many of `messages` belong to the first `turn_prefix` turns a reader saw.
///
/// The terminal counts drawn turns; this counts persisted messages, and the two
/// disagree because a completed subagent is persisted as an ordinary turn and
/// drawn as a card rather than a turn of its own. The walk therefore skips the
/// same three-message windows [`crate::resume::project_tui_history`] does, so a
/// subagent turn advances the message count without advancing the turn count.
///
/// A cut lands on a turn boundary — immediately before the user message that
/// opens the next drawn turn — which is why no tool exchange needs balancing
/// afterwards: a turn's calls and their results are all inside it.
///
/// Returns `None` for a `turn_prefix` of zero, which names no turn at all. A
/// `turn_prefix` past the end of the history keeps the whole of it: the reader
/// pointed past the last turn, and the nearest cut that exists is the end.
pub fn fork_cut_message_count(messages: &[Message], turn_prefix: u64) -> Option<usize> {
    if turn_prefix == 0 {
        return None;
    }

    let mut drawn_turns = 0_u64;
    let mut index = 0;

    while index < messages.len() {
        if messages
            .get(index..index + 3)
            .is_some_and(is_persisted_subagent_turn)
        {
            index += 3;
            continue;
        }

        if messages[index].role == Role::User {
            if drawn_turns == turn_prefix {
                return Some(index);
            }
            drawn_turns += 1;
        }
        index += 1;
    }

    Some(messages.len())
}

/// The session a lineage is rooted at: the furthest ancestor `session_id` still
/// reaches by following parent ids.
///
/// Stops at [`MAX_LINEAGE_DEPTH`] rather than following a chain forever, and
/// stops at a parent that no longer exists, which reads as the lineage ending
/// there. A session that was started rather than forked is its own root.
pub fn lineage_root(store: &SessionStore, session_id: i64) -> Result<i64, SessionStoreError> {
    let mut root = session_id;

    for _ in 0..MAX_LINEAGE_DEPTH {
        let Some(parent) = store.session_parent(root)? else {
            break;
        };
        if parent == root {
            break;
        }
        root = parent;
    }

    Ok(root)
}

/// The lineage rooted at `root`, depth-first and oldest fork first, as the rows
/// the tree overlay lists.
///
/// Every row carries the same `session:<id>` action the session picker uses, so
/// choosing one resumes it; the depth is what makes the flat list read as a
/// tree. A root that no longer exists yields no rows at all rather than an
/// error: a lineage whose root is gone is an empty lineage, not a failure.
pub fn lineage_entries(
    store: &SessionStore,
    root: i64,
    current_session: Option<i64>,
    now: i64,
) -> Result<Vec<DialogEntry>, SessionStoreError> {
    let mut entries = Vec::new();
    if let Some(root) = store.read_session(root)? {
        push_lineage_entry(&mut entries, &root, current_session, now, 0);
        append_forks(
            store,
            root.metadata.id,
            current_session,
            now,
            1,
            &mut entries,
        )?;
    }

    Ok(entries)
}

/// Appends `parent`'s forks, and each fork's own, until the listing is full.
fn append_forks(
    store: &SessionStore,
    parent: i64,
    current_session: Option<i64>,
    now: i64,
    depth: usize,
    entries: &mut Vec<DialogEntry>,
) -> Result<(), SessionStoreError> {
    if depth > MAX_LINEAGE_DEPTH || entries.len() >= MAX_LINEAGE_ROWS {
        return Ok(());
    }

    for child in store.list_session_children(parent)? {
        if entries.len() >= MAX_LINEAGE_ROWS {
            break;
        }

        push_lineage_entry(entries, &child, current_session, now, depth);
        append_forks(
            store,
            child.metadata.id,
            current_session,
            now,
            depth + 1,
            entries,
        )?;
    }

    Ok(())
}

fn push_lineage_entry(
    entries: &mut Vec<DialogEntry>,
    session: &StoredSession,
    current_session: Option<i64>,
    now: i64,
    depth: usize,
) {
    entries.push(session_dialog_entry(session, current_session, false, now).with_depth(depth));
}

#[cfg(test)]
mod tests {
    use agens_core::MessagePart;

    use super::*;

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text(text.into())],
        }
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text(text.into())],
        }
    }

    fn subagent_turn() -> Vec<Message> {
        vec![
            user("explore the repository"),
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::ToolCall {
                        id: "subagent:1".into(),
                        name: "native::task".into(),
                        input: r#"{"agent":"explore","description":"explore the repository"}"#
                            .into(),
                    },
                    MessagePart::Reasoning("3 tool uses".into()),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "subagent:1".into(),
                    content: "a map".into(),
                    is_error: false,
                }],
            },
        ]
    }

    #[test]
    fn a_cut_lands_on_the_message_that_opens_the_next_drawn_turn() {
        let messages = vec![
            user("first"),
            assistant("one"),
            user("second"),
            assistant("two"),
            user("third"),
            assistant("three"),
        ];

        assert_eq!(fork_cut_message_count(&messages, 1), Some(2));
        assert_eq!(fork_cut_message_count(&messages, 2), Some(4));
    }

    #[test]
    fn a_turn_prefix_of_zero_names_no_cut() {
        assert_eq!(fork_cut_message_count(&[user("first")], 0), None);
    }

    #[test]
    fn a_turn_prefix_past_the_last_turn_keeps_the_whole_history() {
        let messages = vec![user("first"), assistant("one")];

        assert_eq!(fork_cut_message_count(&messages, 9), Some(messages.len()));
    }

    /// A subagent is persisted as a turn and drawn as a card, so it must move the
    /// cut without being counted as a turn the reader could stand on.
    #[test]
    fn a_subagent_turn_advances_the_cut_without_advancing_the_turn_count() {
        let mut messages = vec![user("first"), assistant("one")];
        messages.extend(subagent_turn());
        messages.extend([user("second"), assistant("two")]);

        // The first drawn turn ends after the subagent's three messages, not before.
        assert_eq!(fork_cut_message_count(&messages, 1), Some(5));
        assert_eq!(fork_cut_message_count(&messages, 2), Some(messages.len()));
    }
}
