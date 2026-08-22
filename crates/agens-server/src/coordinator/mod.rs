//! The composition root: the one place the coordinator's pieces are assembled.
//!
//! Everything the daemon runs on exists on its own and is testable on its own —
//! the state machines, the scheduler, the gates, the timer wheel, ingest, the
//! sessions and the service core. None of them knows how to find the others.
//! This is where they are given each other, once, and where the loops that give
//! them an occasion to run are started.
//!
//! Four loops, each with one reason to exist:
//!
//! - **Admission** ticks when a run enters the queue and on a heartbeat, because
//!   a slot also frees when a session ends and nothing announces that.
//! - **The timer wheel** ticks on a heartbeat and recomputes every deadline from
//!   the database, so a daemon that was down keeps no deadline in memory to be
//!   wrong about.
//! - **Ingest** drains the harness's facts, which is the only path a worker's
//!   evidence has into the control plane.
//! - **The journal publisher** reads the journal's tail once for every
//!   subscriber and hands each one what its filter asked for.
//!
//! The loops are threads rather than tasks on the daemon's runtime. All four
//! reach a synchronous SQLite connection and would otherwise sit on the
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

mod reconcile;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use agens_store::{ControlPlaneStore, DirectiveStore, EventRow, RunRow};
use agens_tools::SessionWorktrees;

use crate::api::{ApiCore, Ports};
use crate::fsm::StateMachines;
use crate::ingest::{FactReceiver, FactSender, Ingest, channel as ingest_channel};
use crate::ports::{
    Admissions, GitWorktreeGate, JournalFeed, RunDeliveries, SupervisedSessions, run_mailbox,
};
use crate::scheduler::{
    LaunchError, PendingRun, RunSession, Scheduler, SchedulerLimits, SchedulerLoad,
    SupervisorLauncher,
};
use crate::sessions::SessionSupervisor;
use crate::timers::{TimerSettings, TimerWheel};

pub use reconcile::{
    BootReconciliation, MissingWorktree, OrphanWorktree, WORKTREE_MISSING_EVENT,
    WORKTREE_ORPHANED_EVENT,
};

/// How often a loop looks again when nothing woke it.
const HEARTBEAT: Duration = Duration::from_millis(250);

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
    /// How long a loop waits before looking again on its own.
    pub heartbeat: Duration,
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
            heartbeat: HEARTBEAT,
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
    loops: Vec<JoinHandle<()>>,
}

impl Coordinator {
    /// Composes the coordinator over one data directory, reconciles the state
    /// the last process left behind, and starts its loops.
    ///
    /// The supervisor arrives from the daemon rather than being built here: the
    /// sessions the scheduler starts and the sessions the daemon stops on its
    /// way out have to be the same ones.
    pub fn start(
        data_directory: &Path,
        settings: &CoordinatorSettings,
        supervisor: SessionSupervisor,
        worker: RunWorkerFactory,
    ) -> Result<Self, CoordinatorError> {
        let machines = StateMachines::new(open_control_plane(data_directory)?);
        let admissions = Arc::new(Admissions::new());
        let feed = Arc::new(JournalFeed::new());

        let ports = Ports {
            scheduler: Arc::clone(&admissions) as Arc<dyn crate::api::SchedulerPort>,
            worktrees: Arc::new(GitWorktreeGate::new(
                SessionWorktrees::new(data_directory),
                settings.main_ref.clone(),
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

        let core = Arc::new(Mutex::new(ApiCore::new(machines, ports)));
        let (facts, reports) = ingest_channel();
        let stopping = Arc::new(AtomicBool::new(false));

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
            ),
            timer_loop(settings, &core, &admissions, &stopping),
            ingest_loop(data_directory, settings, reports, &stopping)?,
            publisher_loop(data_directory, settings, feed, &stopping)?,
        ];

        let coordinator = Self {
            core,
            reconciliation,
            admissions,
            facts,
            stopping,
            loops,
        };

        // Step 6, after the loops are up: a run coming back is something a
        // watcher sees happen rather than finds already done. Assembled first
        // so that a resume which cannot be applied stops the loops it would
        // otherwise leave ticking behind a composition that failed.
        coordinator.resume_reconciled()
    }

    /// Step 6, through the same core the loops tick against.
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
                self.stop();

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

    /// Stops the loops and waits for them.
    ///
    /// Takes the coordinator by value: after this the core is still readable
    /// through the handles already given out, and nothing is ticking against
    /// it, which is exactly the state a shutdown wants.
    pub fn stop(self) {
        self.stopping.store(true, Ordering::Release);
        self.admissions.wake();

        for handle in self.loops {
            let _ = handle.join();
        }
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
) -> JoinHandle<()> {
    let core = Arc::clone(core);
    let admissions = Arc::clone(admissions);
    let stopping = Arc::clone(stopping);
    let scheduler = Scheduler::new(settings.scheduler.clone());
    let heartbeat = settings.heartbeat;
    let backoff = settings.heartbeat * FAILED_LAUNCH_BACKOFF;
    let data_directory = data_directory.to_path_buf();

    std::thread::spawn(move || {
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

            let failed = match core.lock() {
                // A tick that could not read the queue did nothing and is not
                // fatal: the next occasion reads it again, and the runs it
                // would have admitted are still queued where they were.
                Ok(mut core) => scheduler
                    .tick(core.machines_mut(), &launcher, &load)
                    .map_or(true, |report| !report.failures.is_empty()),
                Err(_) => true,
            };

            // A launch that did not work leaves its run queued, so the next
            // occasion offers it the same slot and it fails the same way. The
            // pause is what keeps that from spending a session per heartbeat
            // on a run nothing has changed about.
            if failed {
                std::thread::sleep(backoff);
            }
        }
    })
}

/// The timer wheel, recomputing every deadline from the database.
fn timer_loop(
    settings: &CoordinatorSettings,
    core: &Arc<Mutex<ApiCore>>,
    admissions: &Arc<Admissions>,
    stopping: &Arc<AtomicBool>,
) -> JoinHandle<()> {
    let core = Arc::clone(core);
    let admissions = Arc::clone(admissions);
    let stopping = Arc::clone(stopping);
    let wheel = TimerWheel::new(settings.timers);
    let heartbeat = settings.heartbeat;

    std::thread::spawn(move || {
        while !stopping.load(Ordering::Acquire) {
            let requeued = match core.lock() {
                Ok(mut core) => wheel
                    .tick(core.machines_mut())
                    .map(|tick| !tick.quota_resets.is_empty())
                    .unwrap_or_default(),
                Err(_) => false,
            };

            // A provider whose reset arrived put its runs back in the queue,
            // and the machine that moved them is not the core, so nothing else
            // tells admission to look.
            if requeued {
                admissions.wake();
            }

            std::thread::sleep(heartbeat);
        }
    })
}

/// Ingest: the harness's facts, folded into the journal and into run health.
fn ingest_loop(
    data_directory: &Path,
    settings: &CoordinatorSettings,
    reports: FactReceiver,
    stopping: &Arc<AtomicBool>,
) -> Result<JoinHandle<()>, CoordinatorError> {
    let mut ingest = Ingest::new(open_control_plane(data_directory)?);
    let stopping = Arc::clone(stopping);
    let heartbeat = settings.heartbeat;

    Ok(std::thread::spawn(move || {
        while !stopping.load(Ordering::Acquire) {
            // A refused fact is already carried back on its own outcome, and
            // the reporter is the party that can do something about it.
            let _ = ingest.drain_available(&reports);

            std::thread::sleep(heartbeat);
        }
    }))
}

/// The journal publisher: the tail of the journal, once, for every subscriber.
fn publisher_loop(
    data_directory: &Path,
    settings: &CoordinatorSettings,
    feed: Arc<JournalFeed>,
    stopping: &Arc<AtomicBool>,
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
        // belongs to never changes repository.
        let mut repositories: std::collections::HashMap<i64, Option<String>> =
            std::collections::HashMap::new();

        while !stopping.load(Ordering::Acquire) {
            std::thread::sleep(heartbeat);

            // The head is read before the subscribers are counted, and never
            // after: a subscriber that arrived in between would otherwise have
            // the entries it was registered for skipped over by a watermark
            // taken past them.
            let head = store.latest_event_id().unwrap_or(watermark);

            if feed.subscribers() == 0 {
                // Nobody is watching, so the tail is not read. The watermark
                // still moves, because a subscriber arriving later asks for
                // what happens next rather than for what it missed.
                watermark = head;
                continue;
            }

            let Ok(events) = store.events_after(watermark, PUBLISH_BATCH) else {
                continue;
            };

            for event in &events {
                watermark = event.id.unwrap_or(watermark).max(watermark);
                feed.publish(event, repository_of(&store, &mut repositories, event));
            }
        }
    }))
}

/// How many journal entries one pass hands out. A bound rather than the whole
/// tail: a burst is delivered over several passes instead of holding the
/// publisher inside one query while it grows.
const PUBLISH_BATCH: usize = 256;

fn repository_of<'a>(
    store: &ControlPlaneStore,
    known: &'a mut std::collections::HashMap<i64, Option<String>>,
    event: &EventRow,
) -> Option<&'a str> {
    let run_id = event.run_id?;

    known
        .entry(run_id)
        .or_insert_with(|| store.load_run(run_id).ok().flatten().map(|run| run.repo_id))
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
