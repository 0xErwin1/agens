//! Admission: who gets a slot, in what order, and what the queue reports when
//! it cannot give one out.
//!
//! The scheduler runs when a slot frees or when a run enters `queued`. It holds
//! no queue and no counters: every tick rebuilds both from the store, so a
//! restart resumes with the same queue it had and a run that moved between
//! ticks is judged against where it actually is.
//!
//! Three ceilings bound admission, and they are not interchangeable:
//!
//! - `max_concurrent` and the worktree ceiling bound the machine as a whole. A
//!   run held back by either stays queued in its position. They are counted
//!   against different things: slots against the runs executing, worktrees
//!   against every run that holds a directory, which includes the ones parked
//!   on a question and the ones still waiting for approval.
//! - Provider headroom bounds one provider. A run whose provider is out of
//!   headroom is skipped rather than allowed to hold up the queue behind it:
//!   stopping there would starve every other provider on account of one.
//!
//! Sub-agents never come through here. They are depth 1 inside a worker's
//! attempt, so they count softly against their provider's headroom and never
//! against `max_concurrent` — a worker that delegates does not thereby occupy a
//! second slot.
//!
//! A parked run holds nothing. `awaiting_input` and `awaiting_quota` are states
//! of their own, and only `running` is counted, so a worker waiting on a person
//! or on a quota reset releases its slot by virtue of not being in it.
//!
//! Nothing the scheduler declines is charged to the run. Being held back by a
//! ceiling, a capped provider or an unmet dependency leaves the run queued in
//! place, with no attempt opened and no retry budget spent. Only a launch that
//! was accepted and then did not work is a failure, and it is reported as one
//! rather than folded in with the conditions that pass on their own.

mod launcher;
mod queue;

use std::collections::BTreeMap;

use agens_store::{RunRow, RunState, WorktreeStatus};

pub use launcher::{RunSession, SupervisorLauncher};
pub use queue::{Candidate, Ineligible, Queue};

use crate::fsm::{
    Principal, RunFacts, RunTrigger, StateMachines, TransitionRejection,
    WORKTREE_HOLDING_RUN_STATES,
};
use crate::sessions::SessionId;

/// The ceilings admission is bounded by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchedulerLimits {
    /// `team.max_concurrent`: how many runs the machine executes at once.
    pub max_concurrent: usize,
    /// How many worktrees the machine can hold at once. Every running run
    /// occupies one.
    pub available_worktrees: usize,
    /// How many concurrent sessions one provider is given, by provider name.
    pub provider_capacity: BTreeMap<String, usize>,
    /// What a provider with no entry above is given.
    pub default_provider_capacity: usize,
}

impl SchedulerLimits {
    /// The capacity recorded for one provider, or the default.
    #[must_use]
    pub fn capacity_for(&self, provider: &str) -> usize {
        self.provider_capacity
            .get(provider)
            .copied()
            .unwrap_or(self.default_provider_capacity)
    }
}

/// What one tick knows that the store does not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchedulerLoad {
    /// Epoch seconds. The scheduler reads no clock, for the same reason the
    /// state machines do not.
    pub now: i64,
    /// Sub-agents alive right now, per provider. Soft: it lowers that
    /// provider's headroom and never evicts anything already running, so a
    /// worker that spawned sub-agents is never interrupted by their cost.
    pub subagents: BTreeMap<String, usize>,
}

/// A run about to be launched.
pub struct PendingRun<'a> {
    pub run_id: i64,
    pub run: &'a RunRow,
    /// Whether this is work coming back rather than starting. A resumed run
    /// picks up from its last checkpoint instead of beginning an attempt from
    /// nothing.
    pub resumed: bool,
}

/// The session an accepted launch produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaunchedSession {
    pub session: SessionId,
    /// The physical execution the attempt runs as, correlating the run with the
    /// harness's evidence ledger.
    pub session_attempt_id: Option<i64>,
}

/// Why a launch did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchError(pub String);

impl std::fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for LaunchError {}

/// How the scheduler starts a run.
///
/// A port rather than a concrete supervisor: what a session is built from — its
/// provider client, its confinement root, its MCP connections — is the API
/// core's to decide, and admission must not have to know any of it.
///
/// The launch happens before the transition, because the attempt row the
/// transition writes has to carry the session that ran it, and that identity
/// does not exist until the session does. [`RunLauncher::abandon`] closes the
/// window that opens: a transition refused after the launch would otherwise
/// leave a live session behind a run that never moved.
pub trait RunLauncher {
    fn launch(&self, pending: &PendingRun<'_>) -> Result<LaunchedSession, LaunchError>;

    /// Stops a session whose transition did not land.
    fn abandon(&self, session: SessionId);
}

/// Which ceiling a run was held behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deferral {
    /// It was never eligible in the first place.
    Ineligible(Ineligible),
    MaxConcurrent {
        running: usize,
        limit: usize,
    },
    WorktreeCeiling {
        /// Worktrees the machine already holds, this tick's admissions
        /// included.
        held: usize,
        limit: usize,
    },
    ProviderHeadroom {
        provider: String,
        running: usize,
        headroom: usize,
    },
}

/// A launch that was attempted and did not work.
///
/// Distinct from a deferral on purpose: the work could not be delivered, where
/// everything under [`Deferral`] is a condition that passes on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionFailure {
    /// The launcher refused, or the session could not be started. The run is
    /// still queued and keeps its place.
    Launch(LaunchError),
    /// The run machine refused the admission after the session had started. The
    /// session was abandoned.
    Refused(TransitionRejection),
}

/// One admitted run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admission {
    pub run_id: i64,
    pub session: SessionId,
    pub resumed: bool,
}

/// What one tick did, and what it could not do.
///
/// Every queued run that did not start appears here with a reason. That is what
/// makes backpressure visible instead of silent: a queue that grows says which
/// ceiling it is growing against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueueReport {
    /// Queued runs at the start of the tick, eligible or not.
    pub depth: usize,
    /// Runs already executing when the tick began.
    pub running_before: usize,
    pub admitted: Vec<Admission>,
    pub deferred: Vec<(i64, Deferral)>,
    pub failures: Vec<(i64, AdmissionFailure)>,
}

impl QueueReport {
    /// Whether any run stayed queued because a ceiling was reached.
    ///
    /// Runs held back for a capped provider or an unmet dependency are not
    /// backpressure: nothing more capacity could buy would release them.
    #[must_use]
    pub fn is_saturated(&self) -> bool {
        self.deferred
            .iter()
            .any(|(_, deferral)| !matches!(deferral, Deferral::Ineligible(_)))
    }

    /// How many runs stayed queued, for any reason.
    #[must_use]
    pub fn deferred_count(&self) -> usize {
        self.deferred.len()
    }
}

/// A tick could not read the state it schedules from, so it did nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchedulerError {
    Store(String),
}

impl SchedulerError {
    fn from_store(error: agens_store::ControlPlaneError) -> Self {
        Self::Store(error.to_string())
    }
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(detail) => write!(formatter, "the queue could not be read: {detail}"),
        }
    }
}

impl std::error::Error for SchedulerError {}

/// Admission over the control plane's queue.
///
/// Holds the ceilings and nothing else. The state it schedules against belongs
/// to the store, and the sessions it starts belong to the launcher.
pub struct Scheduler {
    limits: SchedulerLimits,
}

impl Scheduler {
    #[must_use]
    pub const fn new(limits: SchedulerLimits) -> Self {
        Self { limits }
    }

    #[must_use]
    pub const fn limits(&self) -> &SchedulerLimits {
        &self.limits
    }

    /// Admits what fits and reports what did not.
    ///
    /// Runs when a slot frees or when a run enters `queued`. Reading the store
    /// is the only thing that can fail the tick as a whole; a single run that
    /// cannot be started is recorded and the tick carries on to the next.
    pub fn tick(
        &self,
        machines: &mut StateMachines,
        launcher: &dyn RunLauncher,
        load: &SchedulerLoad,
    ) -> Result<QueueReport, SchedulerError> {
        let queue = queue::build(machines.store())?;
        let mut slots = Slots::read(machines, &self.limits, load)?;

        let mut report = QueueReport {
            depth: queue.depth(),
            running_before: slots.running,
            ..QueueReport::default()
        };

        for (run_id, reason) in queue.ineligible {
            report.deferred.push((run_id, Deferral::Ineligible(reason)));
        }

        for candidate in queue.eligible {
            match slots.offer(candidate.provider()) {
                Err(deferral) => report.deferred.push((candidate.run_id, deferral)),
                Ok(()) => self.admit(machines, launcher, load, &candidate, &mut report),
            }
        }

        Ok(report)
    }

    /// Starts one run's session and moves the run behind it, recording whichever
    /// half did not work.
    fn admit(
        &self,
        machines: &mut StateMachines,
        launcher: &dyn RunLauncher,
        load: &SchedulerLoad,
        candidate: &Candidate,
        report: &mut QueueReport,
    ) {
        let pending = PendingRun {
            run_id: candidate.run_id,
            run: &candidate.run,
            resumed: candidate.resumed,
        };

        let launched = match launcher.launch(&pending) {
            Ok(launched) => launched,
            Err(error) => {
                report
                    .failures
                    .push((candidate.run_id, AdmissionFailure::Launch(error)));
                return;
            }
        };

        let facts = RunFacts {
            now: load.now,
            principal: Principal::Coordinator,
            // The ceilings gave out a slot and an eligible run's provider is
            // serving, so both are facts this tick established. The worktree is
            // not: it is read back off the row the queue was built from, so the
            // guard refuses a run whose directory was reclaimed rather than
            // being told what the launcher assumed.
            slot_available: true,
            provider_serving: true,
            worktree_ready: candidate.run.worktree_status == Some(WorktreeStatus::Active),
            session_id: Some(launched.session.value()),
            session_attempt_id: launched.session_attempt_id,
            ..RunFacts::default()
        };

        match machines.apply_run(candidate.run_id, RunTrigger::Admit, &facts) {
            Ok(_) => report.admitted.push(Admission {
                run_id: candidate.run_id,
                session: launched.session,
                resumed: candidate.resumed,
            }),
            Err(rejection) => {
                launcher.abandon(launched.session);
                report
                    .failures
                    .push((candidate.run_id, AdmissionFailure::Refused(rejection)));
            }
        }
    }
}

/// The slots one tick has to give out, and what taking one costs.
struct Slots {
    running: usize,
    /// Worktrees held by runs this tick is not deciding on, plus one for every
    /// run it admits.
    held_worktrees: usize,
    concurrency_limit: usize,
    worktree_limit: usize,
    /// Runs executing, by provider.
    per_provider: BTreeMap<String, usize>,
    limits: SchedulerLimits,
    /// Sub-agents alive per provider, subtracted from that provider's capacity.
    subagents: BTreeMap<String, usize>,
}

impl Slots {
    /// Counts what is already running and what already holds a worktree, from
    /// the store rather than from memory.
    ///
    /// The two counts are not the same set and neither stands in for the other.
    /// A worktree is provisioned when the run is created and released when it
    /// is cleaned, so a run parked on a question holds a directory without
    /// occupying a slot, and counting `running` twice was what left the
    /// worktree ceiling unable to refuse anything the concurrency ceiling had
    /// not already refused.
    ///
    /// Queued runs are left out of the count on purpose: this tick is deciding
    /// on them, and each admission reserves one below. Counting them here as
    /// well would charge a queued run's own worktree against the ceiling it is
    /// asking to pass, and a machine whose worktrees all belong to queued runs
    /// would admit none of them and so never reclaim any.
    fn read(
        machines: &StateMachines,
        limits: &SchedulerLimits,
        load: &SchedulerLoad,
    ) -> Result<Self, SchedulerError> {
        let running = machines
            .store()
            .runs_in_state(RunState::Running)
            .map_err(SchedulerError::from_store)?;

        let held_worktrees = machines
            .store()
            .held_worktrees_in(&holding_states_outside_the_queue())
            .map_err(SchedulerError::from_store)?;

        let mut per_provider: BTreeMap<String, usize> = BTreeMap::new();
        for run in &running {
            *per_provider.entry(run.provider.clone()).or_default() += 1;
        }

        Ok(Self {
            running: running.len(),
            held_worktrees,
            concurrency_limit: limits.max_concurrent,
            worktree_limit: limits.available_worktrees,
            per_provider,
            limits: limits.clone(),
            subagents: load.subagents.clone(),
        })
    }
}

/// The worktree-holding states, less the one this tick is deciding on.
///
/// Queued runs are left out on purpose: each admission below reserves a
/// worktree of its own, and counting a queued run here as well would charge it
/// against the ceiling it is asking to pass.
fn holding_states_outside_the_queue() -> Vec<RunState> {
    WORKTREE_HOLDING_RUN_STATES
        .iter()
        .copied()
        .filter(|state| *state != RunState::Queued)
        .collect()
}

impl Slots {
    /// Takes a slot for one provider, or names the ceiling that refused it.
    fn offer(&mut self, provider: &str) -> Result<(), Deferral> {
        if self.running >= self.concurrency_limit {
            return Err(Deferral::MaxConcurrent {
                running: self.running,
                limit: self.concurrency_limit,
            });
        }

        if self.held_worktrees >= self.worktree_limit {
            return Err(Deferral::WorktreeCeiling {
                held: self.held_worktrees,
                limit: self.worktree_limit,
            });
        }

        let on_provider = self.per_provider.get(provider).copied().unwrap_or_default();
        let headroom = self.headroom_for(provider);

        if on_provider >= headroom {
            return Err(Deferral::ProviderHeadroom {
                provider: provider.to_owned(),
                running: on_provider,
                headroom,
            });
        }

        self.running += 1;
        self.held_worktrees += 1;
        *self.per_provider.entry(provider.to_owned()).or_default() += 1;

        Ok(())
    }

    /// A provider's capacity less the sub-agents already drawing on it.
    ///
    /// Saturating, because sub-agents are counted softly: more of them than the
    /// provider has capacity for closes admission for that provider and never
    /// goes below zero to evict what is already running.
    fn headroom_for(&self, provider: &str) -> usize {
        let subagents = self.subagents.get(provider).copied().unwrap_or_default();

        self.limits.capacity_for(provider).saturating_sub(subagents)
    }
}
