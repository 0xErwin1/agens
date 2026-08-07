//! Taking a turn back from the terminal: the snapshots a turn is bracketed
//! with, and what `/undo` and `/redo` do with them.
//!
//! The session owns which turns can be taken back; this module owns the two
//! things that need a filesystem — capturing the working tree at a turn
//! boundary, and putting the captured state back.

use std::path::Path;

use agens_bootstrap::Bootstrap;
use agens_session::context::SessionContext;
use agens_session::root::resolve_tui_session_root;
use agens_session::undo::UndoStep;
use agens_snapshot::{SnapshotId, WorkspaceSnapshots};

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
}

impl std::fmt::Display for UndoUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSnapshotted => formatter.write_str(
                "Undo needs a git worktree: this project is not one, so no turn was snapshotted.",
            ),
            Self::NothingToUndo => formatter.write_str("There is no turn to undo."),
            Self::NothingToRedo => formatter.write_str("There is no undone turn to redo."),
        }
    }
}

/// What taking a turn back did, in the words the reader gets back.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UndoOutcome {
    /// The prompt that started the turn, handed back to the composer.
    pub prompt: String,
    pub restored: Vec<String>,
    pub removed: Vec<String>,
    /// Files that moved on after the turn ended. Left exactly as they are: a
    /// convenience command does not get to overwrite work it did not do.
    pub skipped: Vec<String>,
    pub failed: Vec<String>,
}

impl UndoOutcome {
    pub fn message(&self, verb: &str) -> String {
        let mut parts = Vec::new();
        let changed = self.restored.len() + self.removed.len();
        if changed > 0 {
            parts.push(format!("{verb} {changed} file(s)"));
        } else {
            parts.push(format!("{verb} the turn; no file had changed"));
        }
        if !self.skipped.is_empty() {
            parts.push(format!(
                "left {} alone because they changed after the turn: {}",
                self.skipped.len(),
                self.skipped.join(", ")
            ));
        }
        if !self.failed.is_empty() {
            parts.push(format!("could not restore {}", self.failed.join(", ")));
        }
        format!("{}.", parts.join("; "))
    }
}

/// Opens the snapshot repository for the session's own confinement root.
///
/// `None` means this project cannot be snapshotted, which is an answer rather
/// than a failure: the commands say so and stay out of the way.
pub fn session_snapshots(
    bootstrap: &Bootstrap,
    context: &SessionContext,
) -> Option<WorkspaceSnapshots> {
    let root = resolve_tui_session_root(context, bootstrap).ok()?;
    open_snapshots(bootstrap.data_directory(), &root)
}

fn open_snapshots(data_directory: &Path, root: &Path) -> Option<WorkspaceSnapshots> {
    WorkspaceSnapshots::open(data_directory, root)
        .ok()
        .flatten()
}

/// Records a completed turn as undoable.
///
/// Both snapshots are required: without the one taken before the turn there is
/// nothing to restore, and without the one taken after it a redo would have to
/// re-run the model instead of putting the work back.
pub fn record_turn(
    context: &mut SessionContext,
    prompt: &str,
    message_count: usize,
    before: Option<SnapshotId>,
    after: Option<SnapshotId>,
) {
    let (Some(before), Some(after)) = (before, after) else {
        return;
    };
    context.undo.record(UndoStep::new(
        prompt.to_owned(),
        message_count,
        before.as_str().to_owned(),
        after.as_str().to_owned(),
    ));
}

/// Takes the most recent turn back.
pub fn undo_turn(
    snapshots: Option<&WorkspaceSnapshots>,
    context: &mut SessionContext,
) -> Result<UndoOutcome, UndoUnavailable> {
    let Some(snapshots) = snapshots else {
        return Err(UndoUnavailable::NotSnapshotted);
    };
    if context.undo.undoable().is_none() {
        return Err(UndoUnavailable::NothingToUndo);
    }
    let step = context.undo.undo().expect("a turn was available to undo");

    let outcome = move_tree(
        snapshots,
        &SnapshotId::from_hash(step.before().to_owned()),
        &SnapshotId::from_hash(step.after().to_owned()),
        step.prompt(),
    );
    Ok(outcome)
}

/// Puts the most recently undone turn back.
pub fn redo_turn(
    snapshots: Option<&WorkspaceSnapshots>,
    context: &mut SessionContext,
) -> Result<UndoOutcome, UndoUnavailable> {
    let Some(snapshots) = snapshots else {
        return Err(UndoUnavailable::NotSnapshotted);
    };
    if context.undo.redoable().is_none() {
        return Err(UndoUnavailable::NothingToRedo);
    }
    let step = context.undo.redo().expect("a turn was available to redo");

    let outcome = move_tree(
        snapshots,
        &SnapshotId::from_hash(step.after().to_owned()),
        &SnapshotId::from_hash(step.before().to_owned()),
        step.prompt(),
    );
    Ok(outcome)
}

/// Moves the working tree from `from` to `target`, restoring only the paths the
/// two disagree about and only where nobody else has since written.
///
/// `from` is the state the turn is known to have left behind. A path that no
/// longer matches it moved on after the turn ended, so it is somebody else's
/// work and stays untouched.
fn move_tree(
    snapshots: &WorkspaceSnapshots,
    target: &SnapshotId,
    from: &SnapshotId,
    prompt: &str,
) -> UndoOutcome {
    let mut outcome = UndoOutcome {
        prompt: prompt.to_owned(),
        ..UndoOutcome::default()
    };

    let Ok(touched) = snapshots.changed_since(target) else {
        return outcome;
    };
    let diverged = snapshots.changed_since(from).unwrap_or_default();

    let (skipped, restorable): (Vec<String>, Vec<String>) = touched
        .into_iter()
        .partition(|path| diverged.contains(path));
    outcome.skipped = skipped;

    if let Ok(report) = snapshots.restore(target, &restorable) {
        outcome.restored = report.restored;
        outcome.removed = report.removed;
        outcome.failed = report.failed;
    } else {
        outcome.failed = restorable;
    }

    outcome
}
