//! What a rewind reads like once it reaches the terminal.
//!
//! The end-to-end tests in `agens-session` cover which paths move; these cover
//! the one thing that disappears with the terminal — the lines the reader is
//! handed for an outcome that has already been decided.

use agens_session::undo::{Rewind, UndoOutcome};
use agens_tui_app::undo::{rewind_detail, rewind_failure, rewind_summary, rewind_uncommitted};

fn paths(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| (*path).to_owned()).collect()
}

/// A skipped file is the one outcome the reader has to act on, so it is named
/// rather than counted.
#[test]
fn a_skipped_file_is_named_where_the_reader_can_read_it() {
    let outcome = UndoOutcome {
        skipped: paths(&["kept.txt"]),
        ..UndoOutcome::default()
    };

    assert!(
        rewind_detail(&outcome).is_some_and(|detail| detail.contains("kept.txt")),
        "{outcome:?}"
    );
}

/// What the snapshot never covered is a standing property of the project, so it
/// is carried as a count in the summary rather than a list.
#[test]
fn an_uncovered_path_is_reported_in_the_summary() {
    let outcome = UndoOutcome {
        restored: paths(&["kept.txt"]),
        uncovered: paths(&["ignored.txt"]),
        ..UndoOutcome::default()
    };

    assert!(
        rewind_summary(&outcome, Rewind::Back).contains("outside the snapshot"),
        "{outcome:?}"
    );
}

/// A turn that moved no file still undid something, and the summary says that
/// rather than reporting a restore that did not happen.
#[test]
fn a_rewind_that_moved_no_file_says_so_and_opens_no_detail() {
    let outcome = UndoOutcome::default();

    assert!(
        rewind_summary(&outcome, Rewind::Back).contains("no file had changed"),
        "{outcome:?}"
    );
    assert!(rewind_detail(&outcome).is_none(), "{outcome:?}");
}

/// A restore writes one path at a time once a batch fails, so a failure can
/// leave part of the turn moved. Telling the reader the turn was left in place
/// would send them looking for files that are already gone.
#[test]
fn a_failure_that_moved_some_files_does_not_claim_the_turn_was_left_in_place() {
    let outcome = UndoOutcome {
        restored: paths(&["moved.txt"]),
        removed: paths(&["gone.txt"]),
        failed: paths(&["locked.txt"]),
        ..UndoOutcome::default()
    };
    let message = rewind_failure(&outcome, Rewind::Back);

    assert!(
        message.contains("2 other file(s) were already moved"),
        "{message}"
    );
    assert!(!message.contains("the turn was left in place"), "{message}");
    assert!(message.contains("locked.txt"), "{message}");
}

/// A failure that moved nothing is the one case where the reader can be told
/// the turn is exactly where it was.
#[test]
fn a_failure_that_moved_nothing_says_the_turn_was_left_in_place() {
    let outcome = UndoOutcome {
        failed: paths(&["locked.txt"]),
        ..UndoOutcome::default()
    };

    assert!(
        rewind_failure(&outcome, Rewind::Back).contains("the turn was left in place"),
        "{outcome:?}"
    );
}

/// A tree that moved without its marker is reported as the disagreement it is,
/// not as a rewind that did nothing.
#[test]
fn an_uncommitted_rewind_reports_the_tree_moving_without_the_transcript() {
    let message = rewind_uncommitted(Rewind::Back);

    assert!(message.contains("moved the working tree"), "{message}");
    assert!(
        message.contains("transcript was left as it was"),
        "{message}"
    );
}
