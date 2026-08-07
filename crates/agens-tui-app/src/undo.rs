//! Putting a rewind into words a terminal reader gets back.
//!
//! `agens-session` decides which turn moves, which paths are restored and which
//! are left alone; this module only turns that answer into the lines the status
//! bar and the transcript show.

use agens_session::undo::{Rewind, UndoOutcome, UndoUnavailable};

/// How many paths a message names before it starts counting them instead.
const LISTED_PATHS: usize = 10;

/// Why the command did nothing, said to the reader.
pub fn unavailable_message(unavailable: &UndoUnavailable) -> String {
    match unavailable {
        UndoUnavailable::NotSnapshotted => {
            "Undo needs a git worktree: this project is not one, so no turn was snapshotted."
                .to_owned()
        }
        UndoUnavailable::NothingToUndo => "There is no turn to undo.".to_owned(),
        UndoUnavailable::SnapshotsUnavailable(detail) => format!(
            "Snapshots were unavailable this session ({detail}), so no turn was recorded to undo."
        ),
        UndoUnavailable::NothingToRedo => "There is no undone turn to redo.".to_owned(),
        UndoUnavailable::Uninspectable(detail) => format!(
            "The working tree could not be compared against the snapshot ({detail}), so nothing was changed."
        ),
    }
}

/// One line for the status bar: counts only, so it fits.
pub fn rewind_summary(outcome: &UndoOutcome, direction: Rewind) -> String {
    let verb = verb(direction);
    let mut parts = Vec::new();

    let changed = outcome.restored.len() + outcome.removed.len();
    if changed > 0 {
        parts.push(format!("{verb} {changed} file(s)"));
    } else {
        parts.push(format!("{verb} the turn; no file had changed"));
    }
    if !outcome.skipped.is_empty() {
        parts.push(format!(
            "left {} alone because they changed after the turn",
            outcome.skipped.len()
        ));
    }
    if !outcome.uncovered.is_empty() {
        parts.push(format!(
            "{} path(s) were outside the snapshot and could not be checked",
            outcome.uncovered.len()
        ));
    }

    format!("{}.", parts.join("; "))
}

/// The paths behind the counts, for a channel the reader can actually read.
///
/// Only a skipped file opens this: it is the one outcome the reader has to act
/// on. What the snapshot never covered is a standing property of the project
/// rather than news about this turn, so it is carried as a count in
/// [`rewind_summary`] and only spelled out when the reader is already being
/// shown the paths.
pub fn rewind_detail(outcome: &UndoOutcome) -> Option<String> {
    if outcome.skipped.is_empty() {
        return None;
    }

    let mut sections = vec![format!(
        "Left alone because they changed after the turn:\n{}",
        path_list(&outcome.skipped)
    )];
    if !outcome.uncovered.is_empty() {
        sections.push(format!(
            "Outside the snapshot, so the turn could not be undone for them:\n{}",
            path_list(&outcome.uncovered)
        ));
    }

    Some(sections.join("\n\n"))
}

/// What to tell the reader when part of the turn could not be moved.
///
/// A restore falls back to writing one path at a time, so a run can both move
/// files and fail on others. The reader is told which of the two happened,
/// because "nothing changed" and "some of it changed" call for different next
/// steps.
pub fn rewind_failure(outcome: &UndoOutcome, direction: Rewind) -> String {
    let command = command(direction);
    let moved = outcome.restored.len() + outcome.removed.len();
    let state = if moved > 0 {
        format!("{moved} other file(s) were already moved and the rest were left in place")
    } else {
        "the turn was left in place".to_owned()
    };

    format!(
        "{command} could not restore {} file(s), so {state}:\n{}",
        outcome.failed.len(),
        path_list(&outcome.failed)
    )
}

/// What to tell the reader when the tree moved but the transcript could not
/// follow it.
///
/// The tree is moved with the session unlocked, so the turn it belongs to can
/// stop being the one the session would take back before the marker is moved.
/// Moving the marker anyway would point it at a different turn, so it stays and
/// the reader is told the two now disagree.
pub fn rewind_uncommitted(direction: Rewind) -> String {
    format!(
        "{} moved the working tree, but the session changed while it was moving, so the transcript was left as it was.",
        command(direction)
    )
}

fn command(direction: Rewind) -> &'static str {
    match direction {
        Rewind::Back => "Undo",
        Rewind::Forward => "Redo",
    }
}

fn verb(direction: Rewind) -> &'static str {
    match direction {
        Rewind::Back => "Undid",
        Rewind::Forward => "Redid",
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
