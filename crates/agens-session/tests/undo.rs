//! Taking a turn back over a real working tree.
//!
//! The unit tests beside the module cover which turn is next and where the
//! marker lands; these cover what reaches the disk, including the case the whole
//! design exists for: a file the reader edited after the turn must survive the
//! undo.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use agens_core::{Message, MessagePart, Role};
use agens_session::context::SessionContext;
use agens_session::undo::{
    Rewind, UndoOutcome, UndoUnavailable, commit_rewind, pending_turn, record_turn, rewind_tree,
};
use agens_snapshot::WorkspaceSnapshots;

/// An isolated project with a real git worktree, removed when the test ends.
struct Project {
    root: PathBuf,
    worktree: PathBuf,
    data: PathBuf,
}

impl Project {
    fn new(label: &str) -> Self {
        let root = agens_fixtures::session_directory(&format!("undo-{label}"));
        let worktree = root.join("project");
        let data = root.join("data");
        std::fs::create_dir_all(&data).expect("data directory");

        git(&worktree, &["init", "--quiet"]);
        git(&worktree, &["config", "user.name", "test"]);
        git(&worktree, &["config", "user.email", "test@localhost"]);
        std::fs::write(worktree.join("kept.txt"), "original\n").expect("seed file");
        git(&worktree, &["add", "."]);
        git(&worktree, &["commit", "--quiet", "-m", "initial"]);

        Self {
            root,
            worktree,
            data,
        }
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

    /// Makes every later git call against the project fail, which is how a
    /// snapshot comparison stops being able to answer what changed.
    fn break_git(&self) {
        std::fs::rename(self.worktree.join(".git"), self.root.join("moved-git"))
            .expect("the project repository moves aside");
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
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

fn answer(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![MessagePart::Text(text.into())],
    }
}

/// Brackets one turn the way the engine does: capture, run, capture, record.
fn turn(
    snapshots: &WorkspaceSnapshots,
    context: &mut SessionContext,
    prompt: &str,
    change: impl FnOnce(),
) {
    let before = snapshots.capture().ok();
    let boundary = context.messages.len();
    change();
    context.messages.push(Message {
        role: Role::User,
        parts: vec![MessagePart::Text(prompt.into())],
    });
    context.messages.push(answer("done"));
    let after = snapshots.capture().ok();

    record_turn(context, prompt, boundary, before, after);
}

/// Runs the whole command: read the step, move the tree, then the marker.
fn rewind(
    snapshots: Option<&WorkspaceSnapshots>,
    context: &mut SessionContext,
    direction: Rewind,
) -> Result<UndoOutcome, UndoUnavailable> {
    rewind_while(snapshots, context, direction, |_| {}).map(|(outcome, _)| outcome)
}

/// The same command with `meanwhile` standing in for whatever reaches the
/// session while the tree is moving: the command holds no lock over that stretch,
/// so a completing turn or a second rewind lands in exactly this gap.
///
/// Reports whether the marker was moved alongside the outcome.
fn rewind_while(
    snapshots: Option<&WorkspaceSnapshots>,
    context: &mut SessionContext,
    direction: Rewind,
    meanwhile: impl FnOnce(&mut SessionContext),
) -> Result<(UndoOutcome, bool), UndoUnavailable> {
    let step = pending_turn(context, direction)?;
    let outcome = rewind_tree(snapshots, &step, direction)?;
    meanwhile(context);

    let committed = outcome.is_complete() && commit_rewind(context, direction, &step);
    Ok((outcome, committed))
}

/// The whole feature in one pass: the turn's file goes back, and the prompt
/// comes back with it so the reader does not retype anything.
#[test]
fn undo_restores_the_turn_and_hands_the_prompt_back() {
    let project = Project::new("restores");
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "rewrite kept.txt", || {
        project.write("kept.txt", "written by the agent\n");
    });

    let outcome = rewind(Some(&snapshots), &mut context, Rewind::Back).expect("a turn to undo");
    assert_eq!(outcome.prompt, "rewrite kept.txt");
    assert_eq!(outcome.restored, vec!["kept.txt".to_owned()]);
    assert_eq!(project.read("kept.txt").as_deref(), Some("original\n"));
    assert!(context.live_messages().is_empty(), "{context:?}");

    let outcome = rewind(Some(&snapshots), &mut context, Rewind::Forward).expect("a turn to redo");
    assert_eq!(outcome.restored, vec!["kept.txt".to_owned()]);
    assert_eq!(
        project.read("kept.txt").as_deref(),
        Some("written by the agent\n"),
        "redo puts back exactly what the undo discarded"
    );
    assert_eq!(context.live_messages().len(), 2);
}

/// The case the design exists for. A convenience command does not get to
/// destroy work it did not do, so a file that moved on after the turn is
/// reported and left alone.
#[test]
fn a_file_edited_after_the_turn_is_skipped_rather_than_overwritten() {
    let project = Project::new("skips-undo");
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "rewrite kept.txt", || {
        project.write("kept.txt", "written by the agent\n");
    });
    project.write("kept.txt", "and then the reader edited it\n");

    let outcome = rewind(Some(&snapshots), &mut context, Rewind::Back).expect("a turn to undo");
    assert_eq!(outcome.skipped, vec!["kept.txt".to_owned()]);
    assert!(outcome.restored.is_empty(), "{outcome:?}");
    assert_eq!(
        project.read("kept.txt").as_deref(),
        Some("and then the reader edited it\n")
    );
}

/// A file the snapshot never held cannot be put back, and an undo that stayed
/// silent about it would be claiming a completeness it does not have.
#[test]
fn files_the_snapshot_never_covered_are_reported_rather_than_claimed() {
    let project = Project::new("uncovered-paths");
    project.write(".gitignore", "ignored.txt\n");
    project.write("ignored.txt", "the reader's own scratch file\n");
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "rewrite both files", || {
        project.write("kept.txt", "written by the agent\n");
        project.write("ignored.txt", "also written by the agent\n");
    });

    let outcome = rewind(Some(&snapshots), &mut context, Rewind::Back).expect("a turn to undo");
    assert_eq!(outcome.restored, vec!["kept.txt".to_owned()]);
    assert!(
        outcome.uncovered.contains(&"ignored.txt".to_owned()),
        "{outcome:?}"
    );
    assert_eq!(
        project.read("ignored.txt").as_deref(),
        Some("also written by the agent\n"),
        "an uncovered file is left exactly as it is"
    );
}

/// Redo has the same duty as undo in the other direction: an edit made while the
/// turn was undone is the reader's, and putting the turn back does not get to
/// overwrite it either.
#[test]
fn a_file_edited_after_the_undo_is_skipped_by_the_redo() {
    let project = Project::new("skips-redo");
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "rewrite kept.txt", || {
        project.write("kept.txt", "written by the agent\n");
    });
    rewind(Some(&snapshots), &mut context, Rewind::Back).expect("a turn to undo");
    project.write("kept.txt", "written while the turn was undone\n");

    let outcome = rewind(Some(&snapshots), &mut context, Rewind::Forward).expect("a turn to redo");
    assert_eq!(outcome.skipped, vec!["kept.txt".to_owned()]);
    assert!(outcome.restored.is_empty(), "{outcome:?}");
    assert_eq!(
        project.read("kept.txt").as_deref(),
        Some("written while the turn was undone\n")
    );
}

/// Undoing more than one turn walks the tree back turn by turn, not straight to
/// the beginning.
#[test]
fn successive_undos_walk_the_tree_back_one_turn_at_a_time() {
    let project = Project::new("successive");
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "first", || {
        project.write("kept.txt", "after the first turn\n");
    });
    turn(&snapshots, &mut context, "second", || {
        project.write("kept.txt", "after the second turn\n");
    });

    rewind(Some(&snapshots), &mut context, Rewind::Back).expect("the second turn");
    assert_eq!(
        project.read("kept.txt").as_deref(),
        Some("after the first turn\n")
    );

    rewind(Some(&snapshots), &mut context, Rewind::Back).expect("the first turn");
    assert_eq!(project.read("kept.txt").as_deref(), Some("original\n"));

    assert_eq!(
        rewind(Some(&snapshots), &mut context, Rewind::Back),
        Err(UndoUnavailable::NothingToUndo)
    );
}

/// A turn that answered without touching a file is still a turn to take back,
/// and reporting nothing moved is better than reporting a restore that did not
/// happen.
#[test]
fn a_turn_that_touched_no_file_still_undoes_and_reports_nothing_moved() {
    let project = Project::new("no-file");
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "just answer me", || {});

    let outcome = rewind(Some(&snapshots), &mut context, Rewind::Back).expect("a turn to undo");
    assert!(outcome.restored.is_empty());
    assert!(outcome.removed.is_empty());
    assert!(outcome.skipped.is_empty(), "{outcome:?}");
    assert!(outcome.uncovered.is_empty(), "{outcome:?}");
}

/// An attempt that failed before it wrote anything left nothing to take back.
/// Recording it anyway would cost the reader one `/undo` per failed attempt and
/// would throw away whatever was waiting to be redone.
#[test]
fn a_turn_that_failed_before_writing_anything_is_not_recorded() {
    let project = Project::new("failed-turn");
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "a turn that ran", || {
        project.write("kept.txt", "written by the agent\n");
    });
    rewind(Some(&snapshots), &mut context, Rewind::Back).expect("a turn to undo");

    let boundary = context.messages.len();
    let unchanged = snapshots.capture().ok();
    record_turn(
        &mut context,
        "a turn that failed",
        boundary,
        unchanged.clone(),
        unchanged,
    );

    assert!(
        pending_turn(&context, Rewind::Forward).is_ok(),
        "the undone turn is still waiting to be put back"
    );
    assert!(
        pending_turn(&context, Rewind::Back).is_err(),
        "a failed attempt is not a turn to take back"
    );
}

/// Without a snapshot repository there is nothing to restore from, and the
/// command has to say that rather than pretend it worked.
#[test]
fn a_project_that_cannot_be_snapshotted_reports_it() {
    let project = Project::new("unsnapshotted");
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "rewrite kept.txt", || {
        project.write("kept.txt", "written by the agent\n");
    });
    let step = pending_turn(&context, Rewind::Back).expect("a turn is available to take back");

    assert_eq!(
        rewind_tree(None, &step, Rewind::Back),
        Err(UndoUnavailable::NotSnapshotted),
        "the turn is there; only the repository to restore from is missing"
    );
    assert_eq!(
        project.read("kept.txt").as_deref(),
        Some("written by the agent\n")
    );
}

/// The comparison against the snapshot is the only thing that knows which files
/// are safe to write. When it cannot run, the command reports that and leaves
/// both the tree and the history exactly as they are.
#[test]
fn a_comparison_that_cannot_run_restores_nothing_and_keeps_the_turn() {
    let project = Project::new("uninspectable");
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "rewrite kept.txt", || {
        project.write("kept.txt", "written by the agent\n");
    });
    project.break_git();

    let moved = rewind(Some(&snapshots), &mut context, Rewind::Back);
    assert!(
        matches!(moved, Err(UndoUnavailable::Uninspectable(_))),
        "{moved:?}"
    );
    assert_eq!(
        project.read("kept.txt").as_deref(),
        Some("written by the agent\n"),
        "nothing is restored on a guess"
    );
    assert!(
        pending_turn(&context, Rewind::Back).is_ok(),
        "the turn is still there to take back once the tree can be read again"
    );
    assert_eq!(context.live_messages().len(), 2);
}

/// The tree moves with the session released, so a turn can complete while it is
/// moving. Moving the marker afterwards would take back a turn whose files were
/// never restored, so the marker stays where it is and the undo history is left
/// whole for the reader to try again.
#[test]
fn a_turn_that_completes_while_the_tree_moves_leaves_the_marker_where_it_was() {
    let project = Project::new("turn-during-undo");
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "first", || {
        project.write("kept.txt", "after the first turn\n");
    });

    let (outcome, committed) =
        rewind_while(Some(&snapshots), &mut context, Rewind::Back, |context| {
            turn(&snapshots, context, "second", || {
                project.write("kept.txt", "after a turn that landed mid-undo\n");
            });
        })
        .expect("a turn to undo");

    assert!(outcome.is_complete(), "{outcome:?}");
    assert!(
        !committed,
        "the marker must not move over a turn it did not restore"
    );
    assert_eq!(
        pending_turn(&context, Rewind::Back)
            .expect("the turn that landed is still there")
            .prompt(),
        "second"
    );
    assert_eq!(
        pending_turn(&context, Rewind::Forward),
        Err(UndoUnavailable::NothingToRedo),
        "nothing was taken back, so nothing is waiting to be put back"
    );
    assert_eq!(context.live_messages().len(), 4);
}

/// A turn claiming the session while the tree moves is the same disagreement
/// from the other side: the marker would move under a turn that is still
/// writing, so it does not move at all.
#[test]
fn a_session_that_starts_running_while_the_tree_moves_keeps_its_undo() {
    let project = Project::new("running-during-undo");
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "first", || {
        project.write("kept.txt", "after the first turn\n");
    });

    let (_, committed) = rewind_while(Some(&snapshots), &mut context, Rewind::Back, |context| {
        context.running = true;
    })
    .expect("a turn to undo");

    assert!(
        !committed,
        "a running session does not get its marker moved"
    );
    assert!(pending_turn(&context, Rewind::Back).is_ok());
    assert_eq!(
        pending_turn(&context, Rewind::Forward),
        Err(UndoUnavailable::NothingToRedo)
    );
    assert_eq!(context.live_messages().len(), 2);
}

/// Redo is only reachable through an undo: without one there is nothing to put
/// back, and re-running the turn would not be the same thing.
#[test]
fn redo_without_an_undo_reports_that_there_is_nothing_to_put_back() {
    let project = Project::new("redo-without-undo");
    let snapshots = project.snapshots();
    let mut context = SessionContext::fresh();

    turn(&snapshots, &mut context, "first", || {
        project.write("kept.txt", "after the first turn\n");
    });

    assert_eq!(
        rewind(Some(&snapshots), &mut context, Rewind::Forward),
        Err(UndoUnavailable::NothingToRedo)
    );
}
