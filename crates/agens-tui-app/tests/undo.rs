//! What a rewind reads like once it reaches the terminal.
//!
//! The end-to-end tests in `agens-session` cover which paths move; these cover
//! the one thing that disappears with the terminal — the lines the reader is
//! handed for an outcome that has already been decided.

use agens_session::undo::{Rewind, UndoOutcome};
use agens_tui_app::undo::{rewind_detail, rewind_summary};

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
