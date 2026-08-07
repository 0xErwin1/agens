//! Taking a turn back from the terminal: the snapshots a turn is bracketed
//! with, and what `/undo` and `/redo` do with them.
//!
//! The session owns which turns can be taken back; this module owns the two
//! things that need a filesystem — capturing the working tree at a turn
//! boundary, and putting the captured state back.
//!
//! Every git call here runs a subprocess, so the session state and the tree
//! move are kept apart: a caller reads the step it wants under its lock, moves
//! the tree with the lock released, and only then commits the marker. The
//! marker therefore never moves for a tree that did not.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agens_bootstrap::Bootstrap;
use agens_session::context::SessionContext;
use agens_session::root::resolve_tui_session_root;
use agens_session::undo::UndoStep;
use agens_snapshot::{SnapshotId, WorkspaceSnapshots};

/// Which way a rewind moves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rewind {
    /// `/undo`: back to the tree as it stood before the turn.
    Back,
    /// `/redo`: forward to the tree the turn left behind.
    Forward,
}

impl Rewind {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Back => "Undid",
            Self::Forward => "Redid",
        }
    }
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

impl std::fmt::Display for UndoUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSnapshotted => formatter.write_str(
                "Undo needs a git worktree: this project is not one, so no turn was snapshotted.",
            ),
            Self::NothingToUndo => formatter.write_str("There is no turn to undo."),
            Self::NothingToRedo => formatter.write_str("There is no undone turn to redo."),
            Self::Uninspectable(detail) => write!(
                formatter,
                "The working tree could not be compared against the snapshot ({detail}), so nothing was changed."
            ),
        }
    }
}

/// How many paths a message names before it starts counting them instead.
const LISTED_PATHS: usize = 10;

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

    /// One line for the status bar: counts only, so it fits.
    pub fn summary(&self, direction: Rewind) -> String {
        let verb = direction.verb();
        let mut parts = Vec::new();

        let changed = self.restored.len() + self.removed.len();
        if changed > 0 {
            parts.push(format!("{verb} {changed} file(s)"));
        } else {
            parts.push(format!("{verb} the turn; no file had changed"));
        }
        if !self.skipped.is_empty() {
            parts.push(format!(
                "left {} alone because they changed after the turn",
                self.skipped.len()
            ));
        }
        if !self.uncovered.is_empty() {
            parts.push(format!(
                "{} path(s) were outside the snapshot and could not be checked",
                self.uncovered.len()
            ));
        }

        format!("{}.", parts.join("; "))
    }

    /// The paths behind the counts, for a channel the reader can actually read.
    ///
    /// Only a skipped file opens this: it is the one outcome the reader has to
    /// act on. What the snapshot never covered is a standing property of the
    /// project rather than news about this turn, so it is carried as a count in
    /// [`UndoOutcome::summary`] and only spelled out when the reader is already
    /// being shown the paths.
    pub fn detail(&self) -> Option<String> {
        if self.skipped.is_empty() {
            return None;
        }

        let mut sections = vec![format!(
            "Left alone because they changed after the turn:\n{}",
            path_list(&self.skipped)
        )];
        if !self.uncovered.is_empty() {
            sections.push(format!(
                "Outside the snapshot, so the turn could not be undone for them:\n{}",
                path_list(&self.uncovered)
            ));
        }

        Some(sections.join("\n\n"))
    }

    /// What to tell the reader when part of the turn could not be moved.
    pub fn failure(&self, direction: Rewind) -> String {
        format!(
            "{} could not restore {} file(s), so the turn was left in place:\n{}",
            match direction {
                Rewind::Back => "Undo",
                Rewind::Forward => "Redo",
            },
            self.failed.len(),
            path_list(&self.failed)
        )
    }
}

fn path_list(paths: &[String]) -> String {
    let mut listed = paths
        .iter()
        .take(LISTED_PATHS)
        .map(|path| format!("  {path}"))
        .collect::<Vec<_>>();
    if let Some(remaining) = paths
        .len()
        .checked_sub(LISTED_PATHS)
        .filter(|rest| *rest > 0)
    {
        listed.push(format!("  and {remaining} more"));
    }

    listed.join("\n")
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
