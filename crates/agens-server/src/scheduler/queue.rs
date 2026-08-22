//! Which queued runs are eligible, and in what order they are admitted.
//!
//! Both answers are recomputed from the store on every tick. The scheduler
//! keeps no queue of its own, so a restart loses no position and a run that
//! moved between two ticks is judged against where it actually is.

use agens_store::{ControlPlaneStore, QuotaState, RunRow, RunState, WorktreeStatus};

use super::SchedulerError;

/// Why a queued run was not even considered for a slot.
///
/// Both reasons are conditions of the world rather than faults of the run: it
/// stays queued, keeps its position, and is charged nothing. A run held back
/// here has not failed at anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ineligible {
    /// The run's provider is capped, so there is nothing to admit it into.
    ProviderCapped { provider: String },
    /// The run it depends on has not been merged and reclaimed yet.
    DependencyPending {
        dep_run_id: i64,
        /// The dependency's worktree as it stands, or `None` when the
        /// dependency itself could not be read. A foreign key keeps the second
        /// case from arising, and if it ever did, holding the dependent run is
        /// the only safe answer: admitting it would run work whose predecessor
        /// nothing can vouch for.
        worktree_status: Option<WorktreeStatus>,
    },
}

/// One queued run, carrying the row itself so the launcher gets what it needs
/// without reading it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub run_id: i64,
    pub run: RunRow,
    /// Whether this run reached the queue from `awaiting_input`,
    /// `awaiting_quota` or `interrupted`. Derived from the journal rather than
    /// held in memory, so it survives a restart the same way every other fact
    /// here does.
    pub resumed: bool,
}

impl Candidate {
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.run.provider
    }
}

/// The queue as one tick sees it: what can be admitted, and what cannot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Queue {
    /// Eligible runs, already in admission order.
    pub eligible: Vec<Candidate>,
    /// Queued runs no ceiling was consulted for, each with its reason.
    pub ineligible: Vec<(i64, Ineligible)>,
}

impl Queue {
    /// How many runs are queued, eligible or not. This is the depth that grows
    /// when the machine cannot keep up.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.eligible.len() + self.ineligible.len()
    }
}

/// Reads every queued run, splits it into eligible and not, and orders the
/// eligible half.
pub(super) fn build(store: &ControlPlaneStore) -> Result<Queue, SchedulerError> {
    let queued = store
        .runs_in_state(RunState::Queued)
        .map_err(SchedulerError::from_store)?;

    let mut queue = Queue::default();

    for run in queued {
        let Some(run_id) = run.id else {
            continue;
        };

        match eligibility(store, &run)? {
            Some(reason) => queue.ineligible.push((run_id, reason)),
            None => queue.eligible.push(candidate(store, run_id, run)?),
        }
    }

    order(&mut queue.eligible);

    Ok(queue)
}

/// The reason this run cannot be admitted at all, or `None` when it can.
fn eligibility(
    store: &ControlPlaneStore,
    run: &RunRow,
) -> Result<Option<Ineligible>, SchedulerError> {
    let provider = store
        .load_provider(&run.provider)
        .map_err(SchedulerError::from_store)?;

    // Nothing recorded for a provider means it has never reported a cap, which
    // is not the same as being capped: the first run of a fresh install has to
    // be admittable before any provider row exists.
    if provider.is_some_and(|row| row.quota_state == QuotaState::Capped) {
        return Ok(Some(Ineligible::ProviderCapped {
            provider: run.provider.clone(),
        }));
    }

    let Some(dep_run_id) = run.dep_run_id else {
        return Ok(None);
    };

    let dependency = store
        .load_run(dep_run_id)
        .map_err(SchedulerError::from_store)?;

    let worktree_status = dependency.and_then(|row| row.worktree_status);

    if worktree_status == Some(WorktreeStatus::Reclaimable) {
        Ok(None)
    } else {
        Ok(Some(Ineligible::DependencyPending {
            dep_run_id,
            worktree_status,
        }))
    }
}

fn candidate(
    store: &ControlPlaneStore,
    run_id: i64,
    run: RunRow,
) -> Result<Candidate, SchedulerError> {
    Ok(Candidate {
        run_id,
        resumed: reached_queue_by_resuming(store, run_id)?,
        run,
    })
}

/// The states a run comes back from rather than starts from.
const RESUMED_FROM: [&str; 3] = ["awaiting_input", "awaiting_quota", "interrupted"];

/// Whether the move that put this run in the queue was a resumption.
///
/// Read from the last `run_state_changed` the run machine wrote. The journal is
/// the only record of where a run came from — the row itself only says where it
/// is — and reading it here is what keeps resumed priority from being in-memory
/// state that a restart would silently drop.
fn reached_queue_by_resuming(
    store: &ControlPlaneStore,
    run_id: i64,
) -> Result<bool, SchedulerError> {
    let events = store
        .events_for_run(run_id)
        .map_err(SchedulerError::from_store)?;

    let last_move = events
        .iter()
        .rev()
        .filter(|event| event.event_type == "run_state_changed")
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.payload).ok())
        .find(|payload| payload.get("machine").and_then(serde_json::Value::as_str) == Some("run"));

    let Some(payload) = last_move else {
        return Ok(false);
    };

    let from = payload.get("from").and_then(serde_json::Value::as_str);

    Ok(from.is_some_and(|state| RESUMED_FROM.contains(&state)))
}

/// Resumed runs first, then priority descending, then first queued first.
///
/// Resumed ahead of everything else because a resumed run is work already in
/// flight: it holds a worktree and a checkpoint, and leaving it behind a fresh
/// run of higher priority spends both and finishes neither.
fn order(candidates: &mut [Candidate]) {
    candidates.sort_by(|left, right| {
        right
            .resumed
            .cmp(&left.resumed)
            .then(right.run.priority.cmp(&left.run.priority))
            .then(left.run.created_at.cmp(&right.run.created_at))
            .then(left.run_id.cmp(&right.run_id))
    });
}
