//! The composition root: the one place the coordinator's pieces are assembled.
//!
//! Everything the daemon runs on exists on its own and is testable on its own —
//! the state machines, the scheduler, the gates, the timer wheel, ingest, the
//! sessions and the service core. None of them knows how to find the others.
//! This is where they are given each other, once, and where the loops that give
//! them an occasion to run are started.
//!
//! Five loops, each with one reason to exist:
//!
//! - **Admission** ticks when a run enters the queue and on a heartbeat, because
//!   a slot also frees when a session ends and nothing announces that.
//! - **The timer wheel** ticks on a heartbeat and recomputes every deadline from
//!   the database, so a daemon that was down keeps no deadline in memory to be
//!   wrong about.
//! - **The gates sweep** re-derives git for every run whose work is finished and
//!   whose worktree is still held: it merges what the user authorized and
//!   releases what already landed. It is the only production caller of
//!   [`crate::Gates`], and it is what keeps a finished run from leaving a
//!   worktree active and a branch unmerged forever.
//! - **Ingest** drains the harness's facts, which is the only path a worker's
//!   evidence has into the control plane.
//! - **The journal publisher** reads the journal's tail once for every
//!   subscriber and hands each one what its filter asked for.
//!
//! The loops are threads rather than tasks on the daemon's runtime. All of
//! them reach a synchronous SQLite connection and would otherwise sit on the
//! runtime's worker threads for the length of a query, next to the facade that
//! is trying to answer a client.
//!
//! **Nothing is admitted until the state left behind has been reconciled.**
//! [`reconcile`] runs in a fixed order before the loops start and before the
//! surface clients attach to is up: rows the last process left `running` are
//! interrupted, the deadlines are recomputed from the database, and the
//! worktrees on disk are checked against the runs that claim them. Only then do
//! the interrupted runs go back to the queue. Skipping it is what leaves four
//! dead `running` rows holding `max_concurrent` for good.
//!
//! **What a run's session is made of is not decided here.** It arrives as
//! [`RunWorkerFactory`], which is the seam [`crate::SupervisorLauncher`] was
//! built to take: what a worker is given — its own provider client, its
//! confinement root, its own MCP connections — belongs to the surface that
//! knows about models, and admission must not have to know any of it.

mod fatal;
mod queue_journal;
mod reconcile;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use agens_core::HeadlessTurnCancellation;
use agens_store::{
    ControlPlaneStore, DirectiveStore, EventRow, QuestionAuthor, QuestionKind, QuestionState,
    RunRow, RunState, WorktreeStatus,
};
use agens_tools::SessionWorktrees;

use crate::api::{ApiCore, Ports};
use crate::cache::RunCache;
use crate::coordinator::fatal::FatalCore;
use crate::coordinator::queue_journal::QueueJournal;
use crate::diagnostics::CoordinatorDiagnostics;
use crate::fsm::StateMachines;
use crate::gates::{
    GATE_RESULT_EVENT, Gates, MergePath, PreMergeRequest, ReclaimRequest, SUB_AGENT_EVENT,
    SubAgentKind,
};
use crate::ingest::{
    FactReceiver, FactSender, Ingest, IngestFact, ReportedFact, attribution_of,
    channel as ingest_channel,
};
use crate::policy::{PolicySettings, PolicyStore, RepositoryPolicy};
use crate::ports::{
    Admissions, GitWorktreeGate, JournalFeed, RunDeliveries, SupervisedSessions, run_mailbox,
};
use crate::scheduler::{
    LaunchError, PendingRun, RunSession, Scheduler, SchedulerLimits, SchedulerLoad,
    SupervisorLauncher,
};
use crate::sessions::SessionSupervisor;
use crate::timers::{TimerSettings, TimerWheel};

pub use fatal::CORE_POISONED_EVENT;
pub use queue_journal::{ADMISSION_FAILED_EVENT, RUN_DEFERRED_EVENT};
pub use reconcile::{
    BootReconciliation, MissingWorktree, OrphanWorktree, WORKTREE_MISSING_EVENT,
    WORKTREE_ORPHANED_EVENT,
};

/// How often a loop looks again when nothing woke it.
const HEARTBEAT: Duration = Duration::from_millis(250);

/// How often the gates sweep looks at the worktrees that are still held.
///
/// Far slower than the heartbeat, and deliberately: every candidate costs a
/// handful of git invocations, and nothing it acts on moves on the scale of a
/// heartbeat — an authorization is a person's, and a branch landing elsewhere
/// is somebody else's push.
const GATES_SWEEP: Duration = Duration::from_secs(15);

/// How many heartbeats admission waits after a launch that did not work.
///
/// A failed launch is not a condition that passes on its own the way a ceiling
/// does: the run stays queued and the next tick offers it the same slot, so
/// without a pause one refusal becomes a session started and abandoned on every
/// heartbeat for as long as the daemon runs.
const FAILED_LAUNCH_BACKOFF: u32 = 20;

/// What the coordinator is configured with.
///
/// Resolved by whoever reads the operator's configuration, never here: this
/// crate owns the daemon and the configuration file belongs to the crate that
/// parses it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoordinatorSettings {
    pub scheduler: SchedulerLimits,
    pub timers: TimerSettings,
    /// The branch a run's work is measured against.
    pub main_ref: String,
    /// How many attempts a run's budget allows before the pre-merge gate
    /// refuses to land its work. Configuration, which is why the gate takes it
    /// with the request instead of reading it from the store.
    pub attempt_cap: i64,
    /// How long a loop waits before looking again on its own.
    pub heartbeat: Duration,
    /// How long the gates sweep waits between passes.
    pub gates_sweep: Duration,
    /// What the operator wrote down about the repositories this daemon serves.
    pub policy: PolicySettings,
    /// Whether the coordinator writes its own diagnostics log.
    ///
    /// Capture-gated like every other diagnostic, and off by default: the file
    /// is the second reader's copy of what the journal already holds, and a
    /// machine that did not ask for one should not be given a file that grows.
    pub diagnostics: bool,
}

impl Default for CoordinatorSettings {
    fn default() -> Self {
        Self {
            scheduler: SchedulerLimits {
                max_concurrent: 4,
                available_worktrees: 4,
                provider_capacity: std::collections::BTreeMap::new(),
                default_provider_capacity: 2,
            },
            timers: TimerSettings::default(),
            main_ref: "main".to_owned(),
            attempt_cap: 3,
            policy: PolicySettings::default(),
            heartbeat: HEARTBEAT,
            gates_sweep: GATES_SWEEP,
            diagnostics: false,
        }
    }
}

/// Everything a run's session is built from, at the moment it is being started.
pub struct RunLaunch<'a> {
    pub run_id: i64,
    pub run: &'a RunRow,
    /// Whether this is work coming back rather than starting.
    pub resumed: bool,
    /// The service core, for the run's own introspection surface: `checkpoint`
    /// and `ask` write through the state machines the core owns.
    pub core: Arc<Mutex<ApiCore>>,
    /// Where the worker reports the facts ingest folds into run health.
    pub facts: FactSender,
    pub data_directory: PathBuf,
    /// The name this run's queued deliveries are addressed under.
    pub mailbox: String,
}

/// How the daemon turns an admitted run into a session.
pub type RunWorkerFactory =
    Arc<dyn Fn(&RunLaunch<'_>) -> Result<RunSession, LaunchError> + Send + Sync>;

/// Why the coordinator could not be composed.
#[derive(Debug)]
pub struct CoordinatorError(String);

impl CoordinatorError {
    fn opening(component: &str, error: impl std::fmt::Display) -> Self {
        Self(format!("{component} is unavailable: {error}"))
    }
}

impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CoordinatorError {}

/// The assembled coordinator: the one service core, and the loops that run
/// against it.
pub struct Coordinator {
    core: Arc<Mutex<ApiCore>>,
    reconciliation: BootReconciliation,
    admissions: Arc<Admissions>,
    facts: FactSender,
    stopping: Arc<AtomicBool>,
    /// Read on the way out: what the loops were stopped by is not something
    /// the stop flag itself records.
    fatal: Arc<FatalCore>,
    loops: Vec<JoinHandle<()>>,
}

impl Coordinator {
    /// Composes the coordinator over one data directory, reconciles the state
    /// the last process left behind, and starts its loops.
    ///
    /// The supervisor arrives from the daemon rather than being built here: the
    /// sessions the scheduler starts and the sessions the daemon stops on its
    /// way out have to be the same ones.
    ///
    /// The daemon's own stop arrives for the same reason. A service core left
    /// poisoned has no recovery and no owner: the loops that find it can stop
    /// themselves, and only this can stop the facade that would otherwise keep
    /// answering clients from behind it.
    pub fn start(
        data_directory: &Path,
        settings: &CoordinatorSettings,
        supervisor: SessionSupervisor,
        worker: RunWorkerFactory,
        shutdown: &HeadlessTurnCancellation,
    ) -> Result<Self, CoordinatorError> {
        let machines = StateMachines::new(open_control_plane(data_directory)?);
        let admissions = Arc::new(Admissions::new());
        let feed = Arc::new(JournalFeed::new());
        let policy = Arc::new(
            PolicyStore::open(data_directory, settings.policy.clone())
                .map_err(|error| CoordinatorError::opening("the repository policy", error))?,
        );

        let ports = Ports {
            scheduler: Arc::clone(&admissions) as Arc<dyn crate::api::AdmissionControl>,
            worktrees: Arc::new(GitWorktreeGate::new(
                SessionWorktrees::new(data_directory),
                settings.main_ref.clone(),
                policy.hook_exports(),
            )),
            delivery: Arc::new(RunDeliveries::new(
                DirectiveStore::open(data_directory)
                    .map_err(|error| CoordinatorError::opening("the delivery queue", error))?,
            )),
            sessions: Arc::new(SupervisedSessions::new(
                supervisor.clone(),
                open_control_plane(data_directory)?,
            )),
            feed: Arc::clone(&feed) as Arc<dyn crate::api::EventFeed>,
        };

        let core = Arc::new(Mutex::new(ApiCore::new(
            machines,
            ports,
            policy as Arc<dyn RepositoryPolicy>,
        )));
        let (facts, reports) = ingest_channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let diagnostics = CoordinatorDiagnostics::new(data_directory, settings.diagnostics);
        let fatal = Arc::new(FatalCore::new(
            data_directory,
            &stopping,
            shutdown,
            diagnostics.clone(),
        ));

        // Steps 2 to 4, before anything is ticking or serving. Nothing else
        // holds the core yet, so the pass reads and moves the same rows the
        // loops are about to schedule against.
        let reconciliation = reconcile_boot(&core, data_directory, settings)?;

        let loops = vec![
            admission_loop(
                data_directory,
                settings,
                &core,
                &admissions,
                &stopping,
                supervisor,
                worker,
                facts.clone(),
                diagnostics.clone(),
                Arc::clone(&fatal),
            ),
            timer_loop(
                settings,
                &core,
                &admissions,
                &stopping,
                facts.clone(),
                diagnostics.clone(),
                Arc::clone(&fatal),
            ),
            gates_loop(
                data_directory,
                settings,
                &core,
                &stopping,
                Arc::clone(&fatal),
            ),
            ingest_loop(
                data_directory,
                settings,
                reports,
                &stopping,
                diagnostics.clone(),
            )?,
            publisher_loop(data_directory, settings, feed, &stopping, diagnostics)?,
        ];

        let coordinator = Self {
            core,
            reconciliation,
            admissions,
            facts,
            stopping,
            fatal,
            loops,
        };

        // Step 5, after the loops are up and before the facade answers anyone.
        // Assembled first so that a resume which cannot be applied stops the
        // loops it would otherwise leave ticking behind a composition that
        // failed.
        coordinator.resume_reconciled()
    }

    /// Step 5, through the same core the loops tick against.
    fn resume_reconciled(mut self) -> Result<Self, CoordinatorError> {
        let resumed = match self.core.lock() {
            Ok(mut core) => reconcile::resume_interrupted(core.machines_mut(), now())
                .map_err(|error| CoordinatorError::opening("boot reconciliation", error)),
            Err(_) => Err(CoordinatorError::opening(
                "the service core",
                "it was left poisoned",
            )),
        };

        match resumed {
            Ok(resumed) => {
                if !resumed.is_empty() {
                    self.admissions.wake();
                }

                self.reconciliation.resumed = resumed;

                Ok(self)
            }
            Err(error) => {
                let _ = self.stop();

                Err(error)
            }
        }
    }

    /// What the boot pass found and did, for a caller that reports it.
    #[must_use]
    pub const fn reconciliation(&self) -> &BootReconciliation {
        &self.reconciliation
    }

    /// The one service core, for the facade the daemon serves.
    #[must_use]
    pub fn core(&self) -> Arc<Mutex<ApiCore>> {
        Arc::clone(&self.core)
    }

    /// The reporting end of ingest, for a surface that has facts of its own.
    #[must_use]
    pub const fn facts(&self) -> &FactSender {
        &self.facts
    }

    /// Stops the loops, waits for them, and reports whether the core was left
    /// poisoned.
    ///
    /// Takes the coordinator by value: after this the core is still readable
    /// through the handles already given out, and nothing is ticking against
    /// it, which is exactly the state a shutdown wants.
    ///
    /// The poisoning is read after the loops are joined rather than before,
    /// because a loop that discovers it while the shutdown is already running
    /// discovers it all the same.
    pub fn stop(self) -> bool {
        self.stopping.store(true, Ordering::Release);
        self.admissions.wake();

        for handle in self.loops {
            let _ = handle.join();
        }

        self.fatal.reported()
    }
}

/// Step 1: the database, its migrations, and its integrity.
///
/// Refusing here rather than starting is deliberate. Every later step writes
/// transitions into this file, and a file that failed its own check is one the
/// coordinator cannot reconcile against without making the damage worse.
fn open_control_plane(data_directory: &Path) -> Result<ControlPlaneStore, CoordinatorError> {
    let store = ControlPlaneStore::open(data_directory)
        .map_err(|error| CoordinatorError::opening("the control plane", error))?;

    store
        .verify_integrity()
        .map_err(|error| CoordinatorError::opening("the control plane", error))?;

    Ok(store)
}

/// Steps 2 to 4, through the one core the loops will tick against.
fn reconcile_boot(
    core: &Arc<Mutex<ApiCore>>,
    data_directory: &Path,
    settings: &CoordinatorSettings,
) -> Result<BootReconciliation, CoordinatorError> {
    let wheel = TimerWheel::new(settings.timers);
    let mut core = core
        .lock()
        .map_err(|_| CoordinatorError::opening("the service core", "it was left poisoned"))?;

    reconcile::reconcile_before_surface(core.machines_mut(), data_directory, &wheel, now())
        .map_err(|error| CoordinatorError::opening("boot reconciliation", error))
}

/// Admission: a tick when a run enters the queue, and one on every heartbeat
/// because a freed slot announces nothing.
#[allow(clippy::too_many_arguments)]
fn admission_loop(
    data_directory: &Path,
    settings: &CoordinatorSettings,
    core: &Arc<Mutex<ApiCore>>,
    admissions: &Arc<Admissions>,
    stopping: &Arc<AtomicBool>,
    supervisor: SessionSupervisor,
    worker: RunWorkerFactory,
    facts: FactSender,
    diagnostics: CoordinatorDiagnostics,
    fatal: Arc<FatalCore>,
) -> JoinHandle<()> {
    let core = Arc::clone(core);
    let admissions = Arc::clone(admissions);
    let stopping = Arc::clone(stopping);
    let scheduler = Scheduler::new(settings.scheduler.clone());
    let heartbeat = settings.heartbeat;
    let data_directory = data_directory.to_path_buf();

    std::thread::spawn(move || {
        let mut queue_journal = QueueJournal::default();
        let launcher = SupervisorLauncher::new(supervisor, |pending: &PendingRun<'_>| {
            worker(&RunLaunch {
                run_id: pending.run_id,
                run: pending.run,
                resumed: pending.resumed,
                core: Arc::clone(&core),
                facts: facts.clone(),
                data_directory: data_directory.clone(),
                mailbox: run_mailbox(pending.run_id),
            })
        });

        while !stopping.load(Ordering::Acquire) {
            let open = admissions.wait_for_occasion(heartbeat);

            if !open || stopping.load(Ordering::Acquire) {
                continue;
            }

            let load = SchedulerLoad {
                now: now(),
                ..SchedulerLoad::default()
            };

            // Only a launch that was accepted and then did not work earns the
            // pause. A tick that could not read the queue launched nothing and
            // started nothing: it is a condition of the store rather than of a
            // run, the next heartbeat reads it again, and pausing twenty of
            // them over a transient `SQLITE_BUSY` would hold the whole queue
            // for something that passed on its own.
            let failed_launch = match core.lock() {
                Ok(mut core) => match core.admit_queued_runs(&scheduler, &launcher, &load) {
                    Ok(report) => {
                        queue_journal.record(core.machines_mut(), &report, load.now);
                        diagnostics.admission(&report);

                        !report.failures.is_empty()
                    }
                    Err(_) => false,
                },
                // A poisoned core is not a tick that did nothing: it is a core
                // no later tick will take either. The daemon stops here, and
                // there is nothing to pause for on the way out.
                Err(_) => {
                    fatal.poisoned("admission");

                    false
                }
            };

            // A launch that did not work leaves its run queued, so the next
            // occasion offers it the same slot and it fails the same way. The
            // pause is what keeps that from spending a session per heartbeat
            // on a run nothing has changed about.
            if failed_launch {
                pause(&stopping, heartbeat, FAILED_LAUNCH_BACKOFF);
            }
        }
    })
}

/// Waits for a number of heartbeats, and gives up the moment the daemon is
/// stopping.
///
/// One long sleep would be simpler and would make `stop()` wait out whatever
/// remained of it. A shutdown that has to sit through a backoff is a shutdown
/// whose duration is set by the last thing that went wrong.
fn pause(stopping: &AtomicBool, heartbeat: Duration, heartbeats: u32) {
    for _ in 0..heartbeats {
        if stopping.load(Ordering::Acquire) {
            return;
        }

        std::thread::sleep(heartbeat);
    }
}

/// The timer wheel, recomputing every deadline from the database.
///
/// The wheel raises signals; it applies no judgment and reports to nobody. What
/// it found due therefore has to be carried somewhere by this loop, and the two
/// kinds it finds go to different places:
///
/// - An **overdue checkpoint** is a fact about the run's health, so it is
///   reported into ingest as [`IngestFact::CheckpointExpired`]. That is the
///   only producer of the fact, and without it the lost-worker detector's
///   `CheckpointExpired` branch is unreachable.
/// - An **expired question** has already been applied and journaled by the
///   transition the wheel ran, and neither ingest nor the run machine has
///   anything further to do with it: a run parked on a question that ran out
///   stays parked, because nothing about the expiry says what the work should
///   do instead. It is carried no further on purpose.
fn timer_loop(
    settings: &CoordinatorSettings,
    core: &Arc<Mutex<ApiCore>>,
    admissions: &Arc<Admissions>,
    stopping: &Arc<AtomicBool>,
    facts: FactSender,
    diagnostics: CoordinatorDiagnostics,
    fatal: Arc<FatalCore>,
) -> JoinHandle<()> {
    let core = Arc::clone(core);
    let admissions = Arc::clone(admissions);
    let stopping = Arc::clone(stopping);
    let wheel = TimerWheel::new(settings.timers);
    let heartbeat = settings.heartbeat;

    std::thread::spawn(move || {
        while !stopping.load(Ordering::Acquire) {
            let (requeued, expired) = match core.lock() {
                Ok(mut core) => {
                    let tick = core.advance_timers(&wheel);

                    diagnostics.timers(&tick);

                    (
                        !tick.quota_resets.is_empty(),
                        // Attributed while the core is still held, from the
                        // same rows the tick read: a fact attributed to an
                        // attempt the run left between the two would be
                        // refused as a straggler.
                        expired_checkpoint_facts(core.machines().store(), &tick),
                    )
                }
                // The wheel reading a poisoned core as "nothing was due" is how
                // a daemon goes on ticking against a core it will never take
                // again. It is fatal here for the same reason it is in
                // admission.
                Err(_) => {
                    fatal.poisoned("timers");

                    (false, Vec::new())
                }
            };

            // A provider whose reset arrived put its runs back in the queue,
            // and the machine that moved them is not the core, so nothing else
            // tells admission to look.
            if requeued {
                admissions.wake();
            }

            for fact in expired {
                // A queue with no reader is the daemon shutting down. The
                // signal is already journaled, and the wheel's own
                // deduplication means this tick will not raise it again.
                let _ = facts.report(fact);
            }

            std::thread::sleep(heartbeat);
        }
    })
}

/// One `CheckpointExpired` fact per overdue checkpoint this tick raised.
///
/// A run whose live attempt has not been correlated with a physical execution
/// still produces one, attributed to that attempt with no ledger row named: a
/// worker that died during provisioning never correlates, and it is the case
/// the first checkpoint's deadline exists to catch. Only a run with no attempt
/// at all produces nothing.
fn expired_checkpoint_facts(
    store: &ControlPlaneStore,
    tick: &crate::timers::TimerTick,
) -> Vec<ReportedFact> {
    tick.overdue_checkpoints
        .iter()
        .filter_map(|overdue| {
            let attribution = attribution_of(store, overdue.run_id).ok().flatten()?;

            Some(ReportedFact {
                run_id: overdue.run_id,
                attempt_id: attribution.attempt_id,
                turn: attribution.turn,
                now: tick.now,
                fact: IngestFact::CheckpointExpired,
            })
        })
        .collect()
}

/// The gates sweep: the pre-merge and reclaim gates, given an occasion to run.
///
/// It is the one production caller of [`crate::Gates`], and it builds them for
/// the span of each candidate from `ApiCore::machines_mut`, which is what lets
/// the daemon run the gates and the service core over one control plane rather
/// than two.
///
/// The core is locked per candidate rather than per pass. Every candidate costs
/// a handful of git invocations and one of them costs a merge, and a facade
/// answering a client has no reason to wait behind the whole sweep.
fn gates_loop(
    data_directory: &Path,
    settings: &CoordinatorSettings,
    core: &Arc<Mutex<ApiCore>>,
    stopping: &Arc<AtomicBool>,
    fatal: Arc<FatalCore>,
) -> JoinHandle<()> {
    let core = Arc::clone(core);
    let stopping = Arc::clone(stopping);
    let sweep = GatesSweep {
        worktrees: SessionWorktrees::new(data_directory),
        main_ref: settings.main_ref.clone(),
        attempt_cap: settings.attempt_cap,
        fatal,
    };
    let interval = settings.gates_sweep;
    let heartbeat = settings.heartbeat;

    std::thread::spawn(move || {
        while !stopping.load(Ordering::Acquire) {
            sweep.pass(&core, now());

            // Waited in heartbeats rather than in one stretch, so a shutdown
            // does not sit out a sweep interval it is never going to use.
            let mut waited = Duration::ZERO;
            while waited < interval && !stopping.load(Ordering::Acquire) {
                std::thread::sleep(heartbeat);
                waited += heartbeat;
            }
        }
    })
}

/// One run whose worktree is still held, and the work its state calls for.
struct GateCandidate {
    run_id: i64,
    work: GateWork,
}

/// What one candidate's worktree needs done to it.
enum GateWork {
    /// Active, with an authorization the user granted and no gate has seen.
    Merge { approval_id: i64 },
    /// Active, with no authorization: releasable only if its branch already
    /// landed.
    Release {
        worktree_path: PathBuf,
        /// Whether a cleanup sub-agent has already been asked for.
        cleanup_requested: bool,
    },
    /// Already released, and still holding its directory and its place in the
    /// worktree ceiling.
    Dispose,
}

/// The sweep's own state: everything it needs that the store does not hold.
struct GatesSweep {
    worktrees: SessionWorktrees,
    main_ref: String,
    attempt_cap: i64,
    fatal: Arc<FatalCore>,
}

impl GatesSweep {
    /// One pass over every run whose work is finished and whose worktree is
    /// still active.
    fn pass(&self, core: &Mutex<ApiCore>, now: i64) {
        let Some(candidates) = self.taken(core).map(|core| candidates(&core)) else {
            return;
        };

        for candidate in candidates {
            match &candidate.work {
                GateWork::Merge { approval_id } => {
                    self.merge(core, candidate.run_id, *approval_id, now);

                    // The merge released the worktree; finishing it here rather
                    // than a sweep interval later is what keeps a merged run
                    // from holding its directory until the next pass.
                    self.dispose(core, candidate.run_id, now);
                }
                GateWork::Release {
                    worktree_path,
                    cleanup_requested,
                } => self.release(
                    core,
                    candidate.run_id,
                    worktree_path,
                    *cleanup_requested,
                    now,
                ),
                GateWork::Dispose => self.dispose(core, candidate.run_id, now),
            }
        }
    }

    /// Presents one authorization to the pre-merge gate.
    ///
    /// A refusal is journaled by the gate and never retried here: the approval
    /// it refused is bound to bytes that did not move, so the next pass would
    /// reach the same verdict and would journal it again. That is what makes an
    /// approval a candidate only until a `gate_result` names it.
    fn merge(&self, core: &Mutex<ApiCore>, run_id: i64, approval_id: i64, now: i64) {
        let request = PreMergeRequest {
            run_id,
            approval_id,
            path: MergePath::Integrate,
            main_ref: self.main_ref.clone(),
            attempt_cap: self.attempt_cap,
            now,
        };

        let Some(mut core) = self.taken(core) else {
            return;
        };

        // The verdict, whichever it is, is journaled by the gate against the
        // run. Nothing is done with it here: a merge that did not apply left a
        // sub-agent request in the journal, and the surface that may invoke one
        // is not this loop.
        let _verdict = Gates::new(core.machines_mut(), self.worktrees.clone()).pre_merge(&request);
    }

    /// Releases a worktree whose branch landed without this coordinator having
    /// merged it.
    ///
    /// The derivation here decides only whether it is worth asking, and the
    /// gate re-derives and stays the only thing that forms a verdict. Asking
    /// about a branch that has not landed would journal a refusal on every
    /// pass, for a run nobody has done anything to.
    fn release(
        &self,
        core: &Mutex<ApiCore>,
        run_id: i64,
        worktree_path: &Path,
        cleanup_requested: bool,
        now: i64,
    ) {
        let Ok(derivation) = self.worktrees.derive(worktree_path, &self.main_ref) else {
            return;
        };

        if !derivation.merged || (derivation.dirty && cleanup_requested) {
            return;
        }

        let request = ReclaimRequest {
            run_id,
            main_ref: self.main_ref.clone(),
            now,
        };

        let Some(mut core) = self.taken(core) else {
            return;
        };
        let _verdict = Gates::new(core.machines_mut(), self.worktrees.clone()).reclaim(&request);
    }

    /// Finishes a released worktree: the directory goes and the row reaches
    /// `cleaned`.
    ///
    /// Nothing else in the daemon moves a row off `reclaimable`, so without
    /// this pass every finished run goes on counting against the worktree
    /// ceiling until a person runs the cleaning flow by hand.
    fn dispose(&self, core: &Mutex<ApiCore>, run_id: i64, now: i64) {
        let request = ReclaimRequest {
            run_id,
            main_ref: self.main_ref.clone(),
            now,
        };

        let Some(mut core) = self.taken(core) else {
            return;
        };
        let _verdict = Gates::new(core.machines_mut(), self.worktrees.clone()).dispose(&request);
    }

    /// The core, or nothing and a daemon on its way down.
    ///
    /// A sweep that skipped a poisoned core would go on passing over the same
    /// candidates forever, presenting each of them to a gate that can no longer
    /// be reached.
    fn taken<'a>(&self, core: &'a Mutex<ApiCore>) -> Option<std::sync::MutexGuard<'a, ApiCore>> {
        match core.lock() {
            Ok(core) => Some(core),
            Err(_) => {
                self.fatal.poisoned("gates");

                None
            }
        }
    }
}

/// The runs whose work is over and whose worktree is still held.
///
/// A run in any other state is not one the gates have anything to say about:
/// its work is still moving, or its worktree has already been let go.
fn candidates(core: &ApiCore) -> Vec<GateCandidate> {
    let store = core.machines().store();
    let mut candidates = Vec::new();

    for state in [RunState::Done, RunState::Failed] {
        let Ok(runs) = store.runs_in_state(state) else {
            continue;
        };

        for run in runs {
            let (Some(run_id), Some(worktree_path)) = (
                run.id,
                run.worktree_path
                    .as_deref()
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from),
            ) else {
                continue;
            };

            let work = match run.worktree_status {
                Some(WorktreeStatus::Active) => match pending_approval(store, run_id) {
                    Some(approval_id) => GateWork::Merge { approval_id },
                    None => GateWork::Release {
                        worktree_path,
                        cleanup_requested: cleanup_requested(store, run_id),
                    },
                },
                Some(WorktreeStatus::Reclaimable) => GateWork::Dispose,
                _ => continue,
            };

            candidates.push(GateCandidate { run_id, work });
        }
    }

    candidates
}

/// The authorization this run's merge would go through.
///
/// Granted by the user, and not presented to a gate yet. The journal is what
/// says it was presented, because the gate writes a `gate_result` naming the
/// approval whatever verdict it reached.
fn pending_approval(store: &ControlPlaneStore, run_id: i64) -> Option<i64> {
    let questions = store.questions_for_run(run_id).ok()?;
    let presented: Vec<i64> = store
        .events_of_type_for_run(run_id, GATE_RESULT_EVENT)
        .unwrap_or_default()
        .iter()
        .filter_map(|event| {
            serde_json::from_str::<serde_json::Value>(&event.payload)
                .ok()?
                .get("approval_id")?
                .as_i64()
        })
        .collect();

    questions
        .iter()
        .filter(|question| {
            question.kind == QuestionKind::Approval
                && question.state == QuestionState::Answered
                && question.author == Some(QuestionAuthor::User)
        })
        .filter_map(|question| question.id)
        .find(|id| !presented.contains(id))
}

/// Whether a cleanup sub-agent was already asked for on this run's worktree.
///
/// Asking twice for the same uncommitted work would put one request per pass in
/// the journal for as long as nobody deals with it.
fn cleanup_requested(store: &ControlPlaneStore, run_id: i64) -> bool {
    store
        .events_of_type_for_run(run_id, SUB_AGENT_EVENT)
        .unwrap_or_default()
        .iter()
        .any(|event| event.payload.contains(SubAgentKind::Cleanup.as_str()))
}

/// Ingest: the harness's facts, folded into the journal and into run health.
fn ingest_loop(
    data_directory: &Path,
    settings: &CoordinatorSettings,
    reports: FactReceiver,
    stopping: &Arc<AtomicBool>,
    diagnostics: CoordinatorDiagnostics,
) -> Result<JoinHandle<()>, CoordinatorError> {
    let mut ingest = Ingest::new(open_control_plane(data_directory)?);
    let stopping = Arc::clone(stopping);
    let heartbeat = settings.heartbeat;

    Ok(std::thread::spawn(move || {
        while !stopping.load(Ordering::Acquire) {
            fold_reported_facts(&mut ingest, &reports, &diagnostics);

            std::thread::sleep(heartbeat);
        }

        // Whatever was reported in the last window before the stop, folded
        // before the loop ends. A worker's evidence is already journaled by
        // then and the run it belongs to is not: dropping it would leave the
        // run's health describing a window the daemon had the facts for and
        // never read.
        fold_reported_facts(&mut ingest, &reports, &diagnostics);
    }))
}

/// Folds every fact waiting right now and reports the health signals they
/// raised.
///
/// A refused fact is already carried back on its own outcome, and the reporter
/// is the party that can do something about it.
fn fold_reported_facts(
    ingest: &mut Ingest,
    reports: &FactReceiver,
    diagnostics: &CoordinatorDiagnostics,
) {
    for drained in ingest.drain_available(reports) {
        let Ok(accepted) = &drained.outcome else {
            continue;
        };

        for signal in &accepted.signals {
            diagnostics.health_signal(drained.fact.run_id, signal);
        }
    }
}

/// The journal publisher: the tail of the journal, once, for every subscriber.
fn publisher_loop(
    data_directory: &Path,
    settings: &CoordinatorSettings,
    feed: Arc<JournalFeed>,
    stopping: &Arc<AtomicBool>,
    diagnostics: CoordinatorDiagnostics,
) -> Result<JoinHandle<()>, CoordinatorError> {
    let store = open_control_plane(data_directory)?;
    let stopping = Arc::clone(stopping);
    let heartbeat = settings.heartbeat;

    let mut watermark = store
        .latest_event_id()
        .map_err(|error| CoordinatorError::opening("the journal", error))?;

    Ok(std::thread::spawn(move || {
        // One run's repository, cached: a filter is scoped by repository, the
        // entries of one run arrive in bursts, and the run a journal entry
        // belongs to never changes repository. Bounded, because nothing tells
        // this loop that a run ended and every entry is one more run it would
        // otherwise remember for the life of the daemon.
        let mut repositories: RunCache<Option<String>> = RunCache::with_capacity(PUBLISH_MEMO);

        while !stopping.load(Ordering::Acquire) {
            std::thread::sleep(heartbeat);

            // The head is read before the subscribers are counted, and never
            // after: a subscriber that arrived in between would otherwise have
            // the entries it was registered for skipped over by a watermark
            // taken past them.
            let head = store.latest_event_id().unwrap_or(watermark);

            if feed.subscribers() == 0 && !diagnostics.enabled() {
                // Nobody is watching, so the tail is not read. The watermark
                // still moves, because a subscriber arriving later asks for
                // what happens next rather than for what it missed.
                //
                // A supervisor reading the diagnostics log is a watcher with no
                // subscription, which is why capture keeps the tail being read
                // with no client attached.
                watermark = head;
                continue;
            }

            let Ok(events) = store.events_after(watermark, PUBLISH_BATCH) else {
                continue;
            };

            for event in &events {
                watermark = event.id.unwrap_or(watermark).max(watermark);
                diagnostics.journal_entry(event);
                feed.publish(event, repository_of(&store, &mut repositories, event));
            }
        }
    }))
}

/// How many journal entries one pass hands out. A bound rather than the whole
/// tail: a burst is delivered over several passes instead of holding the
/// publisher inside one query while it grows.
const PUBLISH_BATCH: usize = 256;

/// How many runs the publisher remembers a repository for. Comfortably more
/// than the runs one burst of journal entries spans, and a miss costs a single
/// row read.
const PUBLISH_MEMO: usize = 1024;

fn repository_of<'a>(
    store: &ControlPlaneStore,
    known: &'a mut RunCache<Option<String>>,
    event: &EventRow,
) -> Option<&'a str> {
    let run_id = event.run_id?;

    known
        .get_or_insert_with(run_id, || {
            store.load_run(run_id).ok().flatten().map(|run| run.repo_id)
        })
        .as_deref()
}

/// Epoch seconds, from the machine the daemon runs on. The scheduler reads no
/// clock, so its caller says what "now" means.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use agens_store::{AttemptRow, RunRow, RunState, WorktreeStatus};

    use super::{ControlPlaneStore, IngestFact, expired_checkpoint_facts};
    use crate::timers::{OverdueCheckpoint, TimerTick};

    const NOW: i64 = 1_700_000_000;

    fn scratch() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let suffix = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "agens-server-wheel-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();

        directory
    }

    /// A run that was admitted, opened its attempt, and died before the worker
    /// ever named the physical execution it was running as.
    fn uncorrelated_run(store: &mut ControlPlaneStore) -> i64 {
        let run_id = store
            .insert_run(&RunRow {
                id: None,
                repo_id: "a1b2c3d4e5f60718".to_owned(),
                repo_root: "/home/dev/agens".to_owned(),
                remote_url: None,
                external_ref: None,
                parent_run_id: None,
                task: "the deadline reaches a worker that never correlated".to_owned(),
                scope: "crates/agens-server/src/coordinator".to_owned(),
                dod: "the wheel raises the fact anyway".to_owned(),
                genesis_paths: None,
                state: RunState::Running,
                priority: 5,
                dep_run_id: None,
                provider: "scripted".to_owned(),
                budget_tokens: None,
                worktree_path: None,
                worktree_status: Some(WorktreeStatus::Active),
                created_at: NOW,
                result: None,
            })
            .unwrap();

        store
            .insert_attempt(&AttemptRow {
                id: None,
                run_id,
                n: 1,
                session_id: None,
                session_attempt_id: None,
                started_at: NOW,
                ended_at: None,
                outcome: None,
                retry_trigger: None,
                tokens: None,
                cost_micros: None,
            })
            .unwrap();

        run_id
    }

    fn overdue_tick(run_id: i64) -> TimerTick {
        TimerTick {
            now: NOW + 1_800,
            quota_resets: Vec::new(),
            expired_questions: Vec::new(),
            overdue_checkpoints: vec![OverdueCheckpoint {
                run_id,
                checkpoint_event_id: 1,
                promised_at: None,
                deadline: NOW + 1_800,
                signal_event_id: 2,
            }],
            rejections: Vec::new(),
        }
    }

    /// The whole point of the first checkpoint's deadline is the worker that
    /// died before it reported anything, and that worker never correlated. A
    /// wheel that produced no fact for it left the detector unreached and the
    /// slot held.
    #[test]
    fn an_overdue_checkpoint_is_reported_for_a_run_that_never_correlated() {
        let directory = scratch();
        let mut store = ControlPlaneStore::open(&directory).unwrap();
        let run_id = uncorrelated_run(&mut store);

        let facts = expired_checkpoint_facts(&store, &overdue_tick(run_id));

        let [fact] = facts.as_slice() else {
            panic!("expected one fact, got {facts:?}");
        };
        assert_eq!(fact.run_id, run_id);
        assert_eq!(
            fact.attempt_id, None,
            "the fact belongs to the attempt, which has no physical execution"
        );
        assert_eq!(fact.turn, 1);
        assert_eq!(fact.fact, IngestFact::CheckpointExpired);

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A run that was never admitted has no attempt for a fact to belong to.
    #[test]
    fn an_overdue_checkpoint_for_a_run_without_an_attempt_reports_nothing() {
        let directory = scratch();
        let mut store = ControlPlaneStore::open(&directory).unwrap();
        let run_id = uncorrelated_run(&mut store);
        let orphan = store
            .insert_run(&RunRow {
                id: None,
                task: "no attempt was ever opened".to_owned(),
                ..store.load_run(run_id).unwrap().unwrap()
            })
            .unwrap();

        assert!(expired_checkpoint_facts(&store, &overdue_tick(orphan)).is_empty());

        std::fs::remove_dir_all(directory).unwrap();
    }
}
