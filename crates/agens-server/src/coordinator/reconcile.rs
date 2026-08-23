//! Boot reconciliation: what the coordinator does before it admits anything.
//!
//! A daemon that was killed left rows behind that describe a world that no
//! longer exists. The most expensive of them is a run recorded as `running`:
//! nothing is executing it, nothing will ever report it finished, and until
//! something moves it, it counts against `max_concurrent` for the rest of the
//! installation's life. Every queued run behind it is deferred on a ceiling
//! that four dead rows are holding.
//!
//! Reconciliation is the pass that reads that state back and makes it true
//! again. Its order is fixed and no step of it invokes a model:
//!
//! 1. Open the database, and verify its integrity and its migrations.
//! 2. Scan active runs against live sessions — after a boot there are none —
//!    and move `running → interrupted`.
//! 3. Recompute every timer from the database.
//! 4. Check the worktrees on disk against the runs that claim them, and hand
//!    what nothing claims to the cleaning flow.
//! 5. Raise the surface clients attach to.
//! 6. Move `interrupted → queued`, with resume priority.
//!
//! Steps 5 and 6 are in that order on purpose: the runs coming back are visible
//! to whoever is watching before they start executing again.
//!
//! **`interrupted` is not a failed attempt.** The turn in flight was lost, and
//! losing it is not the worker's doing, so the attempt it was executing is
//! closed with `interrupted` and the retry budget is left where it was. A
//! restart that charged the agent for the restart would let a run exhaust its
//! budget without ever having failed.
//!
//! The pass writes nothing to the filesystem. A worktree nothing claims is
//! journaled as a request for the cleaning flow to act on; removing directories
//! is the reclaim pass's, and it re-derives merge state before it touches
//! anything.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agens_store::{EventClass, EventRow, RunState, WorktreeStatus};

use crate::fsm::{Principal, RunFacts, RunTrigger, StateMachines, TransitionRejection};
use crate::timers::{TimerTick, TimerWheel};

/// The journal entry a worktree directory no run claims is reported as.
///
/// It carries the path and nothing else: the pass that acts on it re-derives
/// everything else from git, because a disposition taken from a stored fact is
/// exactly what the worktree machine's guards exist to refuse.
pub const WORKTREE_ORPHANED_EVENT: &str = "worktree_orphaned";

/// The journal entry a run whose recorded worktree is gone is reported as.
pub const WORKTREE_MISSING_EVENT: &str = "worktree_missing";

/// Where a repository's session worktrees live under the data directory.
const WORKTREES_DIRECTORY: &str = "worktrees";

/// One worktree directory on disk that no run claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrphanWorktree {
    pub path: PathBuf,
    /// The journal entry the cleaning flow reads this request from.
    pub event_id: i64,
}

/// One run whose recorded worktree is not on disk any more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingWorktree {
    pub run_id: i64,
    pub path: PathBuf,
    pub event_id: i64,
}

/// What one boot reconciliation found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BootReconciliation {
    /// Runs the database recorded as executing, with no session behind them.
    pub interrupted: Vec<i64>,
    /// Those of them that went back to the queue.
    pub resumed: Vec<i64>,
    /// Every deadline, recomputed from the database rather than carried over.
    pub timers: TimerTick,
    pub orphan_worktrees: Vec<OrphanWorktree>,
    pub missing_worktrees: Vec<MissingWorktree>,
}

/// Steps 2 to 4: everything that happens before clients can attach.
///
/// Step 1 is the caller's, because opening the store is how the caller obtained
/// the machines this takes.
pub(super) fn reconcile_before_surface(
    machines: &mut StateMachines,
    data_directory: &Path,
    wheel: &TimerWheel,
    now: i64,
) -> Result<BootReconciliation, TransitionRejection> {
    let interrupted = interrupt_orphaned_runs(machines, now)?;
    let timers = wheel.tick(machines)?;
    let (orphan_worktrees, missing_worktrees) = verify_worktrees(machines, data_directory, now)?;

    Ok(BootReconciliation {
        interrupted,
        resumed: Vec::new(),
        timers,
        orphan_worktrees,
        missing_worktrees,
    })
}

/// Step 6: the interrupted runs go back to the queue, ahead of fresh work.
///
/// Separate from the pass above because it runs after the client surface is up,
/// which is the whole point of the order: a run resuming is something a person
/// watching can see happen rather than find already done.
pub(super) fn resume_interrupted(
    machines: &mut StateMachines,
    now: i64,
) -> Result<Vec<i64>, TransitionRejection> {
    let mut resumed = Vec::new();

    for run in machines.store().runs_in_state(RunState::Interrupted)? {
        let Some(run_id) = run.id else {
            continue;
        };

        let facts = RunFacts {
            now,
            principal: Principal::Coordinator,
            ..RunFacts::default()
        };

        if machines
            .apply_run(run_id, RunTrigger::Resume, &facts)?
            .applied()
            .is_some()
        {
            resumed.push(run_id);
        }
    }

    Ok(resumed)
}

/// Step 2: every run the database says is executing, when nothing is.
///
/// The session scan is the empty set by construction. A coordinator composes
/// its supervisor in the same call that runs this, so there is no session alive
/// to match a row against — which is exactly the condition
/// `running → interrupted` describes, and why the reconciliation guard is the
/// only way to reach that state.
fn interrupt_orphaned_runs(
    machines: &mut StateMachines,
    now: i64,
) -> Result<Vec<i64>, TransitionRejection> {
    let mut interrupted = Vec::new();

    for run in machines.store().runs_in_state(RunState::Running)? {
        let Some(run_id) = run.id else {
            continue;
        };

        let facts = RunFacts {
            now,
            principal: Principal::Coordinator,
            boot_reconciliation: true,
            ..RunFacts::default()
        };

        if machines
            .apply_run(run_id, RunTrigger::Reconcile, &facts)?
            .applied()
            .is_some()
        {
            interrupted.push(run_id);
        }
    }

    Ok(interrupted)
}

/// Step 4: the worktrees on disk against the runs that claim them.
///
/// Two mismatches, and they are not the same problem. A directory nothing
/// claims is work the control plane has forgotten and is a request for the
/// cleaning flow. A run that claims a directory which is not there has lost the
/// work it was measured against, and journaling it is what makes that
/// diagnosable instead of surfacing later as a launch that cannot find its
/// root.
///
/// Both are journaled once per boot rather than deduplicated against earlier
/// entries: the boot is the occasion, and a second boot that still finds them
/// is reporting a condition that is still true.
fn verify_worktrees(
    machines: &mut StateMachines,
    data_directory: &Path,
    now: i64,
) -> Result<(Vec<OrphanWorktree>, Vec<MissingWorktree>), TransitionRejection> {
    let mut claimed: BTreeSet<PathBuf> = BTreeSet::new();
    let mut missing = Vec::new();

    for state in ACTIVE_RUN_STATES {
        for run in machines.store().runs_in_state(*state)? {
            let (Some(run_id), Some(path)) = (run.id, run.worktree_path.as_deref()) else {
                continue;
            };

            if run.worktree_status == Some(WorktreeStatus::Cleaned) {
                continue;
            }

            let path = PathBuf::from(path);
            claimed.insert(path.clone());

            if !path.is_dir() {
                missing.push((run_id, path));
            }
        }
    }

    let orphans: Vec<PathBuf> = worktrees_on_disk(data_directory)
        .into_iter()
        .filter(|path| !claimed.contains(path))
        .collect();

    let mut reported_missing = Vec::with_capacity(missing.len());
    for (run_id, path) in missing {
        let event_id = journal_one(
            machines,
            &EventRow {
                id: None,
                run_id: Some(run_id),
                event_type: WORKTREE_MISSING_EVENT.to_owned(),
                class: EventClass::Infra,
                payload: serde_json::json!({ "worktree_path": path.display().to_string() })
                    .to_string(),
                ts: now,
            },
        )?;

        reported_missing.push(MissingWorktree {
            run_id,
            path,
            event_id,
        });
    }

    let mut reported_orphans = Vec::with_capacity(orphans.len());
    for path in orphans {
        let event_id = journal_one(
            machines,
            &EventRow {
                id: None,
                // No run owns it. That is the finding, not a gap in the entry.
                run_id: None,
                event_type: WORKTREE_ORPHANED_EVENT.to_owned(),
                class: EventClass::Infra,
                payload: serde_json::json!({ "worktree_path": path.display().to_string() })
                    .to_string(),
                ts: now,
            },
        )?;

        reported_orphans.push(OrphanWorktree { path, event_id });
    }

    Ok((reported_orphans, reported_missing))
}

/// The states in which a run still has a claim on its worktree. A finished run
/// keeps its worktree until the reclaim pass releases it, so `done` and
/// `failed` claim theirs too.
const ACTIVE_RUN_STATES: &[RunState] = &[
    RunState::Draft,
    RunState::Queued,
    RunState::Running,
    RunState::AwaitingInput,
    RunState::AwaitingQuota,
    RunState::Interrupted,
    RunState::Done,
    RunState::Failed,
];

/// Every session worktree directory under the data directory.
///
/// The layout is `worktrees/<repository id>/<name>`, so exactly two levels are
/// walked. A directory that cannot be read yields nothing rather than failing
/// the boot: the pass reports work for the cleaning flow, and a boot that
/// refused to start over an unreadable directory would be trading a recoverable
/// daemon for a tidy one.
fn worktrees_on_disk(data_directory: &Path) -> Vec<PathBuf> {
    let root = data_directory.join(WORKTREES_DIRECTORY);
    let mut found = Vec::new();

    let Ok(repositories) = std::fs::read_dir(&root) else {
        return found;
    };

    for repository in repositories.flatten() {
        if !repository.path().is_dir() {
            continue;
        }

        let Ok(worktrees) = std::fs::read_dir(repository.path()) else {
            continue;
        };

        for worktree in worktrees.flatten() {
            if worktree.path().is_dir() {
                found.push(worktree.path());
            }
        }
    }

    found.sort();

    found
}

fn journal_one(machines: &mut StateMachines, event: &EventRow) -> Result<i64, TransitionRejection> {
    machines
        .journal(std::slice::from_ref(event))?
        .first()
        .copied()
        .ok_or_else(|| {
            TransitionRejection::Storage(format!(
                "the {} entry was journaled without an id",
                event.event_type
            ))
        })
}
