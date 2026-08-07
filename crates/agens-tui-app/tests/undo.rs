//! Taking a turn back over a real working tree.
//!
//! The unit tests in `agens-session` cover which turn is next; these cover what
//! reaches the disk, including the case the whole design exists for: a file the
//! reader edited after the turn must survive the undo.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use agens_session::context::SessionContext;
use agens_snapshot::WorkspaceSnapshots;
use agens_tui_app::undo::{UndoUnavailable, record_turn, redo_turn, undo_turn};

struct Project {
    worktree: PathBuf,
    data: PathBuf,
}

impl Project {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "agens-undo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the fixture clock is after the epoch")
                .as_nanos()
        ));
        let worktree = root.join("project");
        let data = root.join("data");
        std::fs::create_dir_all(&worktree).expect("project directory");
        std::fs::create_dir_all(&data).expect("data directory");

        git(&worktree, &["init", "--quiet"]);
        git(&worktree, &["config", "user.name", "test"]);
        git(&worktree, &["config", "user.email", "test@localhost"]);
        std::fs::write(worktree.join("kept.txt"), "original\n").expect("seed file");
        git(&worktree, &["add", "."]);
        git(&worktree, &["commit", "--quiet", "-m", "initial"]);

        Self { worktree, data }
    }

    fn write(&self, path: &str, content: &str) {
        std::fs::write(self.worktree.join(path), content).expect("write file");
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.worktree.join(path)).ok()
    }

    fn snapshots(&self) -> WorkspaceSnapshots {
        WorkspaceSnapshots::open(&self.data, &self.worktree)
            .expect("snapshot repository opens")
            .expect("project is a git worktree")
    }
}

fn git(directory: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {arguments:?} failed");
}

/// Brackets one turn: captures, lets `change` run, captures again, records.
fn turn(
    snapshots: &WorkspaceSnapshots,
    context: &mut SessionContext,
    prompt: &str,
    change: impl FnOnce(),
) {
    let before = snapshots.capture().ok();
    let message_count = context.messages.len();
    change();
    let after = snapshots.capture().ok();
    record_turn(context, prompt, message_count, before, after);
}

/// The whole feature in one pass: the turn's file goes back, and the prompt
/// comes back with it so the reader does not retype anything.
#[test]
fn undo_restores_the_turn_and_hands_the_prompt_back() {
    let project = Project::new();
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "rewrite kept.txt", || {
        project.write("kept.txt", "written by the agent\n");
    });

    let outcome = undo_turn(Some(&snapshots), &mut context).expect("a turn to undo");
    assert_eq!(outcome.prompt, "rewrite kept.txt");
    assert_eq!(outcome.restored, vec!["kept.txt".to_owned()]);
    assert_eq!(project.read("kept.txt").as_deref(), Some("original\n"));

    let outcome = redo_turn(Some(&snapshots), &mut context).expect("a turn to redo");
    assert_eq!(outcome.restored, vec!["kept.txt".to_owned()]);
    assert_eq!(
        project.read("kept.txt").as_deref(),
        Some("written by the agent\n"),
        "redo puts back exactly what the undo discarded"
    );
}

/// The case the design exists for. A convenience command does not get to
/// destroy work it did not do, so a file that moved on after the turn is
/// reported and left alone.
#[test]
fn a_file_edited_after_the_turn_is_skipped_rather_than_overwritten() {
    let project = Project::new();
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "rewrite kept.txt", || {
        project.write("kept.txt", "written by the agent\n");
    });
    project.write("kept.txt", "and then the reader edited it\n");

    let outcome = undo_turn(Some(&snapshots), &mut context).expect("a turn to undo");
    assert_eq!(outcome.skipped, vec!["kept.txt".to_owned()]);
    assert!(outcome.restored.is_empty(), "{outcome:?}");
    assert_eq!(
        project.read("kept.txt").as_deref(),
        Some("and then the reader edited it\n")
    );
}

/// Undoing more than one turn walks the tree back turn by turn, not straight to
/// the beginning.
#[test]
fn successive_undos_walk_the_tree_back_one_turn_at_a_time() {
    let project = Project::new();
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "first", || {
        project.write("kept.txt", "after the first turn\n");
    });
    turn(&snapshots, &mut context, "second", || {
        project.write("kept.txt", "after the second turn\n");
    });

    undo_turn(Some(&snapshots), &mut context).expect("the second turn");
    assert_eq!(
        project.read("kept.txt").as_deref(),
        Some("after the first turn\n")
    );

    undo_turn(Some(&snapshots), &mut context).expect("the first turn");
    assert_eq!(project.read("kept.txt").as_deref(), Some("original\n"));

    assert_eq!(
        undo_turn(Some(&snapshots), &mut context),
        Err(UndoUnavailable::NothingToUndo)
    );
}

/// A turn that changed no file is still a turn to take back, and saying so is
/// better than reporting a restore that did not happen.
#[test]
fn a_turn_that_touched_no_file_still_undoes_and_says_so() {
    let project = Project::new();
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "just answer me", || {});

    let outcome = undo_turn(Some(&snapshots), &mut context).expect("a turn to undo");
    assert!(outcome.restored.is_empty());
    assert!(outcome.removed.is_empty());
    assert!(
        outcome.message("Undid").contains("no file had changed"),
        "{outcome:?}"
    );
}

/// Without a snapshot repository there is nothing to restore from, and the
/// command has to say that rather than pretend it worked.
#[test]
fn a_project_that_cannot_be_snapshotted_reports_it() {
    let mut context = SessionContext::fresh();

    assert_eq!(
        undo_turn(None, &mut context),
        Err(UndoUnavailable::NotSnapshotted)
    );
    assert_eq!(
        redo_turn(None, &mut context),
        Err(UndoUnavailable::NotSnapshotted)
    );
}

/// Redo is only reachable through an undo: without one there is nothing to put
/// back, and re-running the turn would not be the same thing.
#[test]
fn redo_without_an_undo_reports_that_there_is_nothing_to_put_back() {
    let project = Project::new();
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "first", || {
        project.write("kept.txt", "after the first turn\n");
    });

    assert_eq!(
        redo_turn(Some(&snapshots), &mut context),
        Err(UndoUnavailable::NothingToRedo)
    );
}
