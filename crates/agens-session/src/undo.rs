//! Taking a turn back, and putting it back again.
//!
//! Undoing does not delete anything. The turn's messages stay where they are
//! and a marker moves back over them, which is what makes redo exact: there is
//! nothing to reconstruct, only a marker to move forward again. The messages
//! are dropped when the reader commits to the new direction by sending the next
//! prompt — see [`UndoHistory::commit`], which reports the prefix that survives,
//! and [`crate::context::SessionContext::commit_undo`], which drops the rest
//! from the history in hand and from the store together.
//!
//! The working tree is handled the same way. Each step remembers the tree as it
//! stood before the turn and as the turn left it, so undo and redo are the same
//! operation pointed at different snapshots.
//!
//! Moving the tree is the other half of this module: which snapshot a direction
//! restores, which paths it may write, and which it must leave alone. Every git
//! call there runs a subprocess, so the session state and the tree move are kept
//! apart: a caller reads the step it wants under its lock, moves the tree with
//! the lock released, and only then commits the marker. The marker therefore
//! never moves for a tree that did not.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agens_bootstrap::Bootstrap;
use agens_core::{Message, MessagePart};
use agens_snapshot::{SnapshotId, WorkspaceSnapshots};

use crate::context::SessionContext;
use crate::root::resolve_tui_session_root;

/// One turn that can be taken back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UndoStep {
    /// What the reader typed to start the turn, so undoing can hand it back to
    /// the composer instead of making them retype it.
    prompt: String,
    /// Where this turn's own messages begin in the history it left behind, as
    /// derived by [`turn_boundary`]. Undoing moves the marker here; the
    /// messages past it stay in place, unsent.
    boundary: usize,
    /// The working tree before the turn ran. Undo restores this.
    before: String,
    /// The working tree as the turn left it. Redo restores this.
    after: String,
}

impl UndoStep {
    pub fn new(prompt: String, boundary: usize, before: String, after: String) -> Self {
        Self {
            prompt,
            boundary,
            before,
            after,
        }
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn boundary(&self) -> usize {
        self.boundary
    }

    pub fn before(&self) -> &str {
        &self.before
    }

    pub fn after(&self) -> &str {
        &self.after
    }
}

/// The turns a session can take back, and the ones it has.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UndoHistory {
    done: Vec<UndoStep>,
    undone: Vec<UndoStep>,
}

impl UndoHistory {
    /// Records a completed turn as undoable.
    ///
    /// A new turn is a new direction, so anything waiting to be redone is gone:
    /// its messages are about to be dropped and its snapshots no longer
    /// describe a state the reader could return to.
    pub fn record(&mut self, step: UndoStep) {
        self.undone.clear();
        self.done.push(step);
    }

    /// The turn `/undo` would take back, without taking it back.
    pub fn undoable(&self) -> Option<&UndoStep> {
        self.done.last()
    }

    /// The turn `/redo` would put back, without putting it back.
    pub fn redoable(&self) -> Option<&UndoStep> {
        self.undone.last()
    }

    /// Moves the most recent turn onto the undone stack and reports it.
    pub fn undo(&mut self) -> Option<UndoStep> {
        let step = self.done.pop()?;
        self.undone.push(step.clone());
        Some(step)
    }

    /// Moves the most recently undone turn back and reports it.
    pub fn redo(&mut self) -> Option<UndoStep> {
        let step = self.undone.pop()?;
        self.done.push(step.clone());
        Some(step)
    }

    /// How many messages of a history of `total` are still part of the
    /// conversation.
    ///
    /// Everything past this point belongs to an undone turn: still present, so
    /// redo can be exact, but not part of what the model is asked to continue.
    pub fn visible_message_count(&self, total: usize) -> usize {
        self.undone
            .last()
            .map_or(total, |step| step.boundary.min(total))
    }

    /// Whether anything is currently held back by an undo.
    pub fn has_undone_turns(&self) -> bool {
        !self.undone.is_empty()
    }

    /// Accepts the undo as final and reports how many messages survive.
    ///
    /// Called when the reader sends the next prompt: that is the moment the
    /// undone turns stop being recoverable and their messages can be dropped.
    pub fn commit(&mut self, total: usize) -> usize {
        let surviving = self.visible_message_count(total);
        self.undone.clear();
        surviving
    }
}

/// Where a completed turn's own messages begin in the history it left behind.
///
/// The count the history held before the turn ran does not survive the turn: a
/// completed turn replaces the history with the one reloaded from the store and
/// re-appends any turn the store had not caught up with yet, so the same number
/// can name a different message afterwards. The boundary is derived from both
/// lists instead — the prefix they still agree on, pulled back to the last point
/// where every tool call has its result, so a marker can never hand the model
/// half of a tool exchange.
///
/// Where the two lists disagree earlier than the turn did, the boundary lands
/// early and holds back more than the turn wrote. That direction is deliberate:
/// showing the reader less than they undid is recoverable, sending the model a
/// turn they took back is not.
pub fn turn_boundary(previous: &[Message], current: &[Message]) -> usize {
    let common = previous
        .iter()
        .zip(current)
        .take_while(|(before, after)| before == after)
        .count();

    balanced_prefix(current, common)
}

/// The longest prefix of `messages` no longer than `limit` in which every tool
/// call is answered.
fn balanced_prefix(messages: &[Message], limit: usize) -> usize {
    let mut unanswered = BTreeSet::new();
    let mut balanced = 0;

    for (index, message) in messages.iter().take(limit).enumerate() {
        for part in &message.parts {
            match part {
                MessagePart::ToolCall { id, .. } => {
                    unanswered.insert(id.as_str());
                }
                MessagePart::ToolResult { tool_call_id, .. } => {
                    unanswered.remove(tool_call_id.as_str());
                }
                _ => {}
            }
        }

        if unanswered.is_empty() {
            balanced = index + 1;
        }
    }

    balanced
}

/// Which way a rewind moves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rewind {
    /// `/undo`: back to the tree as it stood before the turn.
    Back,
    /// `/redo`: forward to the tree the turn left behind.
    Forward,
}

/// Why a session cannot take a turn back right now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UndoUnavailable {
    /// The project is not a git worktree, so there is nothing to snapshot
    /// against.
    NotSnapshotted,
    /// Nothing has been recorded yet, or every recorded turn is already undone.
    NothingToUndo,
    /// Nothing has been undone, so there is nothing to put back.
    NothingToRedo,
    /// The working tree could not be compared against the snapshot. Nothing was
    /// restored: which files are safe to write is exactly what the comparison
    /// answers, so without it the only safe move is to stop.
    Uninspectable(String),
}

/// What taking a turn back did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UndoOutcome {
    /// The prompt that started the turn, handed back to the composer.
    pub prompt: String,
    pub restored: Vec<String>,
    pub removed: Vec<String>,
    /// Files that moved on after the turn ended. Left exactly as they are: a
    /// convenience command does not get to overwrite work it did not do.
    pub skipped: Vec<String>,
    /// Paths the snapshot never held — over the size cap, or ignored by the
    /// project. Whatever the turn did to them is not in the snapshot either, so
    /// a rewind cannot speak for them and says so instead.
    pub uncovered: Vec<String>,
    pub failed: Vec<String>,
}

impl UndoOutcome {
    /// Whether the tree reached the state that was asked for.
    ///
    /// A skipped file is a completed move — leaving somebody else's work alone
    /// is the intended outcome. A failed one is not: part of the turn is still
    /// on disk.
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// The confinement root the session's snapshots belong to.
///
/// Split from [`open_session_snapshots`] because only this half needs the
/// session: opening the repository spawns git and must not run while a caller
/// holds the session lock.
pub fn session_snapshot_root(bootstrap: &Bootstrap, context: &SessionContext) -> Option<PathBuf> {
    resolve_tui_session_root(context, bootstrap).ok()
}

/// Opens the snapshot repository for a session root.
///
/// `None` means this project cannot be snapshotted, which is an answer rather
/// than a failure: the commands say so and stay out of the way.
pub fn open_session_snapshots(bootstrap: &Bootstrap, root: &Path) -> Option<WorkspaceSnapshots> {
    WorkspaceSnapshots::open(bootstrap.data_directory(), root)
        .ok()
        .flatten()
}

/// Records a completed turn as undoable.
///
/// Both snapshots are required: without the one taken before the turn there is
/// nothing to restore, and without the one taken after it a redo would have to
/// re-run the model instead of putting the work back.
///
/// A turn that left neither a message nor a changed file is not recorded. It
/// has nothing to take back, and recording it would cost the reader one `/undo`
/// per failed attempt while discarding whatever was waiting to be redone.
pub fn record_turn(
    context: &mut SessionContext,
    prompt: &str,
    boundary: usize,
    before: Option<SnapshotId>,
    after: Option<SnapshotId>,
) {
    let (Some(before), Some(after)) = (before, after) else {
        return;
    };
    if boundary >= context.messages.len() && before == after {
        return;
    }

    let prompt = context
        .take_typed_prompt_for(prompt)
        .unwrap_or_else(|| prompt.to_owned());
    context.undo.record(UndoStep::new(
        prompt,
        boundary,
        before.as_str().to_owned(),
        after.as_str().to_owned(),
    ));
}

/// The turn a rewind in `direction` would move, without moving it.
pub fn pending_turn(
    context: &SessionContext,
    direction: Rewind,
) -> Result<UndoStep, UndoUnavailable> {
    match direction {
        Rewind::Back => context
            .undo
            .undoable()
            .cloned()
            .ok_or(UndoUnavailable::NothingToUndo),
        Rewind::Forward => context
            .undo
            .redoable()
            .cloned()
            .ok_or(UndoUnavailable::NothingToRedo),
    }
}

/// Moves the working tree to the other side of `step`.
///
/// Touches no session state: the caller commits the marker with
/// [`commit_rewind`] once the tree agrees, so a tree that did not move cannot
/// leave the transcript claiming it did.
pub fn rewind_tree(
    snapshots: Option<&WorkspaceSnapshots>,
    step: &UndoStep,
    direction: Rewind,
) -> Result<UndoOutcome, UndoUnavailable> {
    let Some(snapshots) = snapshots else {
        return Err(UndoUnavailable::NotSnapshotted);
    };

    let (target, from) = match direction {
        Rewind::Back => (step.before(), step.after()),
        Rewind::Forward => (step.after(), step.before()),
    };

    move_tree(
        snapshots,
        &SnapshotId::from_hash(target.to_owned()),
        &SnapshotId::from_hash(from.to_owned()),
        step.prompt(),
    )
}

/// Moves the marker over the messages after the tree has been moved.
pub fn commit_rewind(context: &mut SessionContext, direction: Rewind) {
    match direction {
        Rewind::Back => context.undo.undo(),
        Rewind::Forward => context.undo.redo(),
    };
}

/// Moves the working tree from `from` to `target`, restoring only the paths the
/// two disagree about and only where nobody else has since written.
///
/// `from` is the state the turn is known to have left behind. A path that no
/// longer matches it moved on after the turn ended, so it is somebody else's
/// work and stays untouched. That probe is the only thing standing between an
/// undo and the reader's own edits, so a failure to run it aborts the whole
/// move rather than degrading into "nothing diverged, restore everything".
fn move_tree(
    snapshots: &WorkspaceSnapshots,
    target: &SnapshotId,
    from: &SnapshotId,
    prompt: &str,
) -> Result<UndoOutcome, UndoUnavailable> {
    let mut outcome = UndoOutcome {
        prompt: prompt.to_owned(),
        ..UndoOutcome::default()
    };

    let touched = snapshots.changed_since(target).map_err(uninspectable)?;
    let diverged = snapshots
        .changed_since(from)
        .map_err(uninspectable)?
        .into_iter()
        .collect::<BTreeSet<_>>();

    let (skipped, restorable): (Vec<String>, Vec<String>) = touched
        .into_iter()
        .partition(|path| diverged.contains(path));
    outcome.skipped = skipped;
    outcome.uncovered = snapshots.uncovered(target).map_err(uninspectable)?;

    match snapshots.restore(target, &restorable) {
        Ok(report) => {
            outcome.restored = report.restored;
            outcome.removed = report.removed;
            outcome.failed = report.failed;
            for path in report.uncovered {
                if !outcome.uncovered.contains(&path) {
                    outcome.uncovered.push(path);
                }
            }
        }
        Err(_) => outcome.failed = restorable,
    }

    Ok(outcome)
}

fn uninspectable(error: agens_snapshot::SnapshotError) -> UndoUnavailable {
    UndoUnavailable::Uninspectable(error.to_string())
}

#[cfg(test)]
mod tests {
    use agens_core::Role;

    use super::*;

    fn step(boundary: usize) -> UndoStep {
        UndoStep::new(
            format!("prompt-{boundary}"),
            boundary,
            format!("before-{boundary}"),
            format!("after-{boundary}"),
        )
    }

    fn text(role: Role, body: &str) -> Message {
        Message {
            role,
            parts: vec![MessagePart::Text(body.into())],
        }
    }

    fn call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::ToolCall {
                id: id.into(),
                name: "native::task".into(),
                input: "{}".into(),
            }],
        }
    }

    fn result(id: &str) -> Message {
        Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: id.into(),
                content: "done".into(),
                is_error: false,
            }],
        }
    }

    /// The round trip is the whole point: undo then redo has to leave the
    /// history exactly where it started, or redo is a re-run wearing a name.
    #[test]
    fn undo_then_redo_returns_the_history_to_where_it_was() {
        let mut history = UndoHistory::default();
        history.record(step(2));
        history.record(step(6));

        assert_eq!(history.visible_message_count(10), 10);

        let undone = history.undo().expect("a turn to take back");
        assert_eq!(undone.boundary(), 6);
        assert_eq!(history.visible_message_count(10), 6);

        history.redo().expect("a turn to put back");
        assert_eq!(history.visible_message_count(10), 10);
    }

    /// Walking back several turns has to keep moving the marker back, not stop
    /// at the first one.
    #[test]
    fn successive_undos_walk_the_marker_further_back() {
        let mut history = UndoHistory::default();
        history.record(step(2));
        history.record(step(6));

        history.undo();
        history.undo();
        assert_eq!(history.visible_message_count(10), 2);
        assert!(history.undo().is_none(), "there is nothing older to undo");

        history.redo();
        assert_eq!(history.visible_message_count(10), 6);
    }

    /// Once the reader sends a new prompt the undone turns are not coming back,
    /// and a redo that resurrected them would contradict the message they just
    /// sent.
    #[test]
    fn a_recorded_turn_discards_anything_waiting_to_be_redone() {
        let mut history = UndoHistory::default();
        history.record(step(2));
        history.undo();
        assert!(history.redoable().is_some());

        history.record(step(4));
        assert!(history.redoable().is_none());
        assert_eq!(history.visible_message_count(10), 10);
    }

    /// Committing reports the surviving prefix so the caller can truncate both
    /// its own history and the store with one number.
    #[test]
    fn committing_reports_the_surviving_prefix_and_ends_the_undo() {
        let mut history = UndoHistory::default();
        history.record(step(3));
        history.undo();

        assert!(history.has_undone_turns());
        assert_eq!(history.commit(9), 3);
        assert!(!history.has_undone_turns());
        assert_eq!(
            history.visible_message_count(9),
            9,
            "with nothing undone the whole history is live again"
        );
    }

    /// A marker recorded against a longer history must never index past a
    /// shorter one; a resumed session can hold fewer messages than it did.
    #[test]
    fn the_marker_never_exceeds_the_history_it_is_applied_to() {
        let mut history = UndoHistory::default();
        history.record(step(8));
        history.undo();

        assert_eq!(history.visible_message_count(4), 4);
    }

    /// The ordinary case: the turn appended to the history it was given, and the
    /// boundary is where the appending started.
    #[test]
    fn the_boundary_is_where_the_turn_started_appending() {
        let previous = vec![
            text(Role::User, "first"),
            text(Role::Assistant, "first answer"),
        ];
        let mut current = previous.clone();
        current.push(text(Role::User, "second"));
        current.push(text(Role::Assistant, "second answer"));

        assert_eq!(turn_boundary(&previous, &current), 2);
    }

    /// The count recorded before the turn is not the boundary once the reloaded
    /// history reorders what came before it: a turn that was appended in memory
    /// and only later persisted comes back in a different place, and a length
    /// would point into the new turn instead of at its first message.
    #[test]
    fn a_reordered_history_moves_the_boundary_off_the_recorded_length() {
        let previous = vec![
            text(Role::User, "first"),
            text(Role::Assistant, "first answer"),
            text(Role::User, "a subagent turn appended in memory"),
        ];
        let current = vec![
            text(Role::User, "first"),
            text(Role::Assistant, "first answer"),
            text(Role::User, "second"),
            text(Role::Assistant, "second answer"),
            text(Role::User, "a subagent turn appended in memory"),
        ];

        assert_eq!(
            turn_boundary(&previous, &current),
            2,
            "the boundary is the prefix both histories agree on, not the earlier length"
        );
    }

    /// A marker landing between a tool call and its result would leave the model
    /// a call nothing ever answered, which providers reject outright.
    #[test]
    fn the_boundary_never_splits_a_tool_call_from_its_result() {
        let previous = vec![
            text(Role::User, "review the patch"),
            call("subagent:1"),
            result("subagent:1"),
        ];
        let current = vec![
            text(Role::User, "review the patch"),
            call("subagent:1"),
            text(Role::Assistant, "a different continuation"),
        ];

        assert_eq!(
            turn_boundary(&previous, &current),
            1,
            "the unanswered call is pulled out of the live prefix with its turn"
        );
    }

    /// Nothing in common means nothing to keep; the marker must not invent a
    /// prefix out of a history it does not recognise.
    #[test]
    fn a_history_with_nothing_in_common_has_no_live_prefix() {
        let previous = vec![text(Role::User, "first")];
        let current = vec![text(Role::User, "something else")];

        assert_eq!(turn_boundary(&previous, &current), 0);
    }
}
