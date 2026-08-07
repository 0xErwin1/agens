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
pub fn rewind_failure(outcome: &UndoOutcome, direction: Rewind) -> String {
    format!(
        "{} could not restore {} file(s), so the turn was left in place:\n{}",
        match direction {
            Rewind::Back => "Undo",
            Rewind::Forward => "Redo",
        },
        outcome.failed.len(),
        path_list(&outcome.failed)
    )
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
