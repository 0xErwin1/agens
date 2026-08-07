//! Taking a turn back, and putting it back again.
//!
//! Undoing does not delete anything. The turn's messages stay where they are
//! and a marker moves back over them, which is what makes redo exact: there is
//! nothing to reconstruct, only a marker to move forward again. The messages
//! are dropped for real when the reader commits to the new direction by sending
//! the next prompt — see [`UndoHistory::commit`].
//!
//! The working tree is handled the same way. Each step remembers the tree as it
//! stood before the turn and as the turn left it, so undo and redo are the same
//! operation pointed at different snapshots.

/// One turn that can be taken back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UndoStep {
    /// What the reader typed to start the turn, so undoing can hand it back to
    /// the composer instead of making them retype it.
    prompt: String,
    /// How many messages the history held before the turn ran. Undoing moves
    /// the marker here; the messages past it stay in place, unsent.
    message_count: usize,
    /// The working tree before the turn ran. Undo restores this.
    before: String,
    /// The working tree as the turn left it. Redo restores this.
    after: String,
}

impl UndoStep {
    pub fn new(prompt: String, message_count: usize, before: String, after: String) -> Self {
        Self {
            prompt,
            message_count,
            before,
            after,
        }
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn message_count(&self) -> usize {
        self.message_count
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
            .map_or(total, |step| step.message_count.min(total))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn step(message_count: usize) -> UndoStep {
        UndoStep::new(
            format!("prompt-{message_count}"),
            message_count,
            format!("before-{message_count}"),
            format!("after-{message_count}"),
        )
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
        assert_eq!(undone.message_count(), 6);
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
}
