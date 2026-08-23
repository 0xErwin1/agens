//! Ingest: the in-process channel the harness reports facts through, and the
//! single writer that turns them into journal entries and health signals.
//!
//! One step, not two. A reported fact is normalized, folded into the run's
//! derived state, and written — the journal entry, the health snapshot, and the
//! first freeze of the run's genesis paths — inside one transaction. A journal
//! that recorded a fact whose consequence was never applied would leave the two
//! disagreeing about the same run.
//!
//! Three properties this module is built around:
//!
//! - **`run_health` is derived and recomputable.** The row is a projection of
//!   the journal, and [`Ingest::recompute`] rebuilds it by replaying that
//!   journal through the same fold the live path uses. Nothing reads the row to
//!   decide anything the journal does not already say.
//! - **A fact belongs to an attempt, not to a run.** A fact naming an attempt
//!   that is no longer the run's live one is refused rather than folded: a
//!   straggler from an abandoned attempt that refreshed the live attempt's
//!   liveness would hide a hung worker behind another attempt's timer.
//! - **The detectors detect.** They journal a signal and hand it up; they never
//!   move a run, fail an attempt or reclaim a slot. A slow but correct worker
//!   costs more when it is killed than when it is watched.
//!
//! Ingest holds its own [`ControlPlaneStore`], as the evidence ledger's store
//! already does beside the session writer: it is the single writer of the
//! health rows and of the harness's half of the journal, and the state machines
//! hold the store for the run state they own. The two write disjoint columns,
//! each conditionally, under WAL.

mod checkpoint;
mod detectors;
mod health;

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use agens_core::ToolResultFacts;
use agens_store::{
    ControlPlaneError, ControlPlaneStore, EventClass, EventRow, IngestWrite, RunHealthRow,
};

use crate::cache::RunCache;

pub use checkpoint::{CheckpointClaim, ReportedCheckpoint};
pub(crate) use detectors::detect_worker_lost;
pub use detectors::{CheckpointStanding, HealthSignal, HealthThresholds, LostReason};

use health::{HealthState, Observation};

/// One fact the harness or the timer wheel reports.
///
/// The lifecycle variants are the session events the design names beside the
/// tool results: a turn's boundaries, and the context exhaustion that parks a
/// run rather than failing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngestFact {
    /// One row of the evidence ledger, as the tool reported it.
    ToolResult(ToolResultFacts),
    TurnStarted,
    TurnEnded {
        tokens: u64,
    },
    ContextExhausted,
    /// The provider refused the turn for quota. The run parks on the reset, so
    /// the turn is not the idle one its lack of progress would otherwise make
    /// it look like.
    QuotaReached,
    Checkpoint(ReportedCheckpoint),
    /// The promised checkpoint's grace elapsed. Reported by the timer wheel,
    /// which is the only component that recomputes deadlines from the database.
    CheckpointExpired,
}

/// A fact with the identity that lets it be attributed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportedFact {
    pub run_id: i64,
    /// The physical execution that produced it (`session_attempts.id`), which
    /// is what the evidence ledger is keyed by.
    pub attempt_id: i64,
    /// Turn index within the attempt. Non-negative.
    pub turn: i64,
    /// Epoch seconds. Ingest reads no clock.
    pub now: i64,
    pub fact: IngestFact,
}

/// The identity a fact about a run travels under, read from the run's own live
/// attempt rather than supplied by the reporter.
///
/// It exists for the reporters that observe a run from outside the turn — the
/// timer wheel is the one this was written for. A worker knows which physical
/// execution it is; a wheel sweeping every running run does not, and guessing
/// would attribute a fact to an attempt the run has already left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attribution {
    /// `session_attempts.id`, which is what the evidence ledger is keyed by.
    pub attempt_id: i64,
    /// The run's attempt number, which is also its turn index: one admission
    /// is one turn.
    pub turn: i64,
}

/// How a fact about `run_id` reported right now would be attributed.
///
/// `None` when the run has no attempt yet or its live attempt has not been
/// correlated with a physical execution: neither is a failure, and both mean
/// there is nothing a fact could be attributed to.
pub(crate) fn attribution_of(
    store: &ControlPlaneStore,
    run_id: i64,
) -> Result<Option<Attribution>, ControlPlaneError> {
    let Some(attempt) = store.attempts_for_run(run_id)?.pop() else {
        return Ok(None);
    };

    Ok(attempt.session_attempt_id.map(|attempt_id| Attribution {
        attempt_id,
        turn: attempt.n,
    }))
}

/// Why a reported fact was not ingested. Nothing was written in any of these
/// cases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngestRejection {
    NoSuchRun(i64),
    /// The run has no attempt yet, so nothing it reports can be attributed.
    NoLiveAttempt(i64),
    /// The fact names an attempt that is not the run's live one.
    StaleAttempt {
        run_id: i64,
        reported: i64,
        live: Option<i64>,
    },
    /// The fact violated a bound ingest enforces before folding anything.
    Malformed(String),
    /// The reader that drains the channel is gone.
    ChannelClosed,
    /// The queue was full for longer than a reporter waits. Nothing was
    /// written, and the fact is the reporter's to account for.
    Backlogged,
    Storage(String),
}

impl std::fmt::Display for IngestRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchRun(run_id) => write!(formatter, "no run with id {run_id}"),
            Self::NoLiveAttempt(run_id) => write!(formatter, "run {run_id} has no live attempt"),
            Self::StaleAttempt {
                run_id,
                reported,
                live,
            } => write!(
                formatter,
                "attempt {reported} is not the live attempt of run {run_id} ({live:?})"
            ),
            Self::Malformed(detail) => write!(formatter, "unusable reported fact: {detail}"),
            Self::ChannelClosed => formatter.write_str("the ingest channel has no reader"),
            Self::Backlogged => {
                formatter.write_str("the ingest channel stayed full for longer than a report waits")
            }
            Self::Storage(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for IngestRejection {}

impl From<ControlPlaneError> for IngestRejection {
    fn from(error: ControlPlaneError) -> Self {
        Self::Storage(error.to_string())
    }
}

/// What one ingested fact produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedFact {
    pub health: RunHealthRow,
    /// Journal ids in the order they were written.
    pub event_ids: Vec<i64>,
    /// The paths frozen as this run's genesis paths, when this fact is the one
    /// that froze them.
    pub frozen_genesis_paths: Option<Vec<String>>,
    /// What the detectors made of the fact. Journaled, and Praetor's to judge.
    pub signals: Vec<HealthSignal>,
}

/// One drained fact and what became of it. The fact travels with its outcome so
/// a refused one is visible rather than dropped.
#[derive(Debug)]
pub struct DrainedFact {
    pub fact: ReportedFact,
    pub outcome: Result<AcceptedFact, IngestRejection>,
}

/// How many reported facts may wait for the single writer.
///
/// Bounded for the reason the fan-out's backlog is: the writer drains once per
/// heartbeat against SQLite, and a queue with no ceiling turns a run reporting
/// faster than that into memory the daemon never gets back. Deep enough that a
/// turn's ordinary burst of tool results never meets it.
const INGEST_BACKLOG: usize = 1_024;

/// How long a reporter waits for room before giving the fact up.
///
/// The policy the ceiling needs, and it is deliberately not "block until the
/// writer catches up": a worker parked forever on a full queue is a turn that
/// stopped for a reason nothing in the run says. It waits several heartbeats,
/// which is the writer being slow, and then reports the fact refused.
const INGEST_PATIENCE: Duration = Duration::from_secs(5);

/// How often a waiting reporter looks for room again.
const INGEST_POLL: Duration = Duration::from_millis(10);

/// The reporting end of the in-process ingest channel. Cloneable, so every
/// session's sink holds one.
#[derive(Clone, Debug)]
pub struct FactSender {
    outbound: SyncSender<ReportedFact>,
    patience: Duration,
}

impl FactSender {
    /// Queues one fact for the single writer, waiting for room while the
    /// backlog is full.
    ///
    /// A fact that could not be queued is refused rather than dropped
    /// silently: the reporter is the party that knows which run it belongs to,
    /// and a health plane missing a turn it was told about should say so.
    pub fn report(&self, fact: ReportedFact) -> Result<(), IngestRejection> {
        let deadline = Instant::now() + self.patience;
        let mut fact = fact;

        loop {
            match self.outbound.try_send(fact) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => return Err(IngestRejection::ChannelClosed),
                Err(TrySendError::Full(_)) if Instant::now() >= deadline => {
                    return Err(IngestRejection::Backlogged);
                }
                Err(TrySendError::Full(returned)) => {
                    fact = returned;
                    std::thread::sleep(INGEST_POLL);
                }
            }
        }
    }
}

/// The draining end. Held by whoever owns the [`Ingest`] writer.
#[derive(Debug)]
pub struct FactReceiver(Receiver<ReportedFact>);

/// Opens the in-process ingest channel.
#[must_use]
pub fn channel() -> (FactSender, FactReceiver) {
    channel_with_backlog(INGEST_BACKLOG, INGEST_PATIENCE)
}

fn channel_with_backlog(backlog: usize, patience: Duration) -> (FactSender, FactReceiver) {
    let (outbound, receiver) = sync_channel(backlog);

    (FactSender { outbound, patience }, FactReceiver(receiver))
}

/// The single writer of the harness's facts and of the health they derive.
pub struct Ingest {
    store: ControlPlaneStore,
    thresholds: HealthThresholds,
    /// Per-run derived state, rebuilt from the journal for any run this process
    /// has not seen — which after a restart is every one of them.
    ///
    /// Bounded, because nothing tells ingest that a run ended: every run that
    /// ever reported a fact would otherwise keep its fold here for the life of
    /// the daemon. What an eviction costs is one replay of that run's journal,
    /// and the run it evicts is the one nothing has reported about in longest.
    states: RunCache<HealthState>,
}

/// How many runs ingest keeps a fold for. Well past the runs that can be
/// executing at once, so a busy daemon replays nothing in steady state.
const STATE_MEMO: usize = 256;

impl Ingest {
    #[must_use]
    pub fn new(store: ControlPlaneStore) -> Self {
        Self::with_thresholds(store, HealthThresholds::default())
    }

    #[must_use]
    pub fn with_thresholds(store: ControlPlaneStore, thresholds: HealthThresholds) -> Self {
        Self {
            store,
            thresholds,
            states: RunCache::with_capacity(STATE_MEMO),
        }
    }

    /// Read access for the surfaces that project the control plane.
    #[must_use]
    pub const fn store(&self) -> &ControlPlaneStore {
        &self.store
    }

    /// Ingests every fact queued right now, without blocking on more.
    pub fn drain_available(&mut self, receiver: &FactReceiver) -> Vec<DrainedFact> {
        let mut drained = Vec::new();

        loop {
            match receiver.0.try_recv() {
                Ok(fact) => {
                    let outcome = self.accept(&fact);
                    drained.push(DrainedFact { fact, outcome });
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return drained,
            }
        }
    }

    /// Persists one reported fact and the health it derives.
    pub fn accept(&mut self, reported: &ReportedFact) -> Result<AcceptedFact, IngestRejection> {
        self.check_attribution(reported)?;

        let observation = normalize(&reported.fact)?;
        let mut state = self.state_for(reported.run_id)?;
        let folded = state.fold(reported.turn, &observation);

        let mut events = vec![observation.to_event(
            reported.run_id,
            reported.attempt_id,
            reported.turn,
            reported.now,
            folded.credited_progress,
        )];

        let frozen_genesis_paths = folded
            .freeze_due
            .then(|| self.store.touched_paths_for_run(reported.run_id))
            .transpose()?
            .filter(|paths| !paths.is_empty());

        if let Some(paths) = &frozen_genesis_paths {
            let freeze = Observation::GenesisPathsFrozen {
                paths: paths.clone(),
            };
            state.fold(reported.turn, &freeze);
            events.push(freeze.to_event(
                reported.run_id,
                reported.attempt_id,
                reported.turn,
                reported.now,
                false,
            ));
        }

        let health = state.snapshot(reported.run_id, reported.now);
        let signals = raise_signals(&mut state, &health, &folded, &self.thresholds);
        events.extend(signals.iter().map(|signal| signal_event(reported, signal)));

        let genesis_json = frozen_genesis_paths
            .as_ref()
            .map(|paths| serde_json::json!(paths).to_string());

        let outcome = self.store.apply_ingest(&IngestWrite {
            run_id: reported.run_id,
            health: &health,
            freeze_genesis_paths: genesis_json.as_deref(),
            events: &events,
        })?;

        self.states.insert(reported.run_id, state);

        Ok(AcceptedFact {
            health,
            event_ids: outcome.event_ids,
            frozen_genesis_paths,
            signals,
        })
    }

    /// Rebuilds a run's health row from its journal alone, touching neither the
    /// cached state nor the stored row.
    ///
    /// The row the live path wrote and the row this returns have to agree; that
    /// they do is what "derived, never a source of truth" means in practice.
    pub fn recompute(&self, run_id: i64, now: i64) -> Result<RunHealthRow, IngestRejection> {
        Ok(self.replay(run_id)?.snapshot(run_id, now))
    }

    fn state_for(&mut self, run_id: i64) -> Result<HealthState, IngestRejection> {
        match self.states.get(run_id) {
            Some(state) => Ok(state.clone()),
            None => self.replay(run_id),
        }
    }

    fn replay(&self, run_id: i64) -> Result<HealthState, IngestRejection> {
        let mut state = HealthState::default();

        for event in self.store.events_for_run(run_id)? {
            if let Some((observation, turn)) = Observation::from_event(&event) {
                state.fold(turn, &observation);
            }
        }

        Ok(state)
    }

    /// A lifecycle fact belongs to the attempt that produced it. One naming an
    /// attempt the run has already left is refused, so a straggler cannot
    /// refresh the live attempt's signals.
    fn check_attribution(&self, reported: &ReportedFact) -> Result<(), IngestRejection> {
        if reported.turn < 0 {
            return Err(IngestRejection::Malformed(format!(
                "turn {} is negative",
                reported.turn
            )));
        }

        if self.store.load_run(reported.run_id)?.is_none() {
            return Err(IngestRejection::NoSuchRun(reported.run_id));
        }

        let live = self
            .store
            .attempts_for_run(reported.run_id)?
            .pop()
            .ok_or(IngestRejection::NoLiveAttempt(reported.run_id))?
            .session_attempt_id;

        if live == Some(reported.attempt_id) {
            Ok(())
        } else {
            Err(IngestRejection::StaleAttempt {
                run_id: reported.run_id,
                reported: reported.attempt_id,
                live,
            })
        }
    }
}

/// Normalizes a reported fact into the closed set the fold reads, capping the
/// one unbounded value a caller supplies.
fn normalize(fact: &IngestFact) -> Result<Observation, IngestRejection> {
    Ok(match fact {
        IngestFact::ToolResult(facts) => Observation::from_tool_result(facts),
        IngestFact::TurnStarted => Observation::TurnStarted,
        IngestFact::TurnEnded { tokens } => Observation::TurnEnded {
            tokens: i64::try_from(*tokens).unwrap_or(i64::MAX),
        },
        IngestFact::ContextExhausted => Observation::ContextExhausted,
        IngestFact::QuotaReached => Observation::QuotaReached,
        IngestFact::Checkpoint(claim) => Observation::Checkpoint {
            evidence_class: claim.evidence_class,
            claims_progress: claim.claims_progress,
        },
        IngestFact::CheckpointExpired => Observation::CheckpointExpired,
    })
}

/// Runs both detectors over the state this fact left behind.
///
/// Divergence is raised by the fact that caused it, so a gate can block before
/// the work lands. The lost worker is edge-triggered: a stall that stays true
/// raises one signal, not one per turn.
fn raise_signals(
    state: &mut HealthState,
    health: &RunHealthRow,
    folded: &health::Folded,
    thresholds: &HealthThresholds,
) -> Vec<HealthSignal> {
    let mut signals = Vec::new();

    if let Some(path) = &folded.divergent_path {
        signals.push(HealthSignal::Divergence { path: path.clone() });
    }

    if folded.uncomparable_mutation {
        signals.push(HealthSignal::UnrepresentableMutation);
    }

    if !state.lost_reported()
        && let Some(lost) = detect_worker_lost(health, &state.checkpoint(), thresholds)
    {
        state.mark_lost_reported();
        signals.push(lost);
    }

    signals
}

fn signal_event(reported: &ReportedFact, signal: &HealthSignal) -> EventRow {
    let detail = match signal {
        HealthSignal::WorkerLost { reason, noop_turns } => serde_json::json!({
            "reason": reason.as_str(),
            "noop_turns": noop_turns,
        }),
        HealthSignal::Divergence { path } => serde_json::json!({
            "reason": "path_outside_genesis_paths",
            "path": path,
        }),
        HealthSignal::UnrepresentableMutation => serde_json::json!({
            "reason": "unrepresentable_path",
        }),
    };

    let mut payload = serde_json::json!({ "attempt_id": reported.attempt_id });
    if let (Some(target), Some(detail)) = (payload.as_object_mut(), detail.as_object()) {
        for (key, value) in detail {
            target.insert(key.clone(), value.clone());
        }
    }

    EventRow {
        id: None,
        run_id: Some(reported.run_id),
        event_type: signal.event_type().to_owned(),
        class: EventClass::Infra,
        payload: payload.to_string(),
        ts: reported.now,
    }
}

#[cfg(test)]
mod channel_tests {
    use std::time::{Duration, Instant};

    use super::{IngestFact, IngestRejection, ReportedFact, channel_with_backlog};

    const PATIENCE: Duration = Duration::from_millis(50);

    fn turn_started() -> ReportedFact {
        ReportedFact {
            run_id: 1,
            attempt_id: 1,
            turn: 0,
            now: 1_700_000_000,
            fact: IngestFact::TurnStarted,
        }
    }

    #[test]
    fn a_reporter_waits_for_the_writer_rather_than_queueing_without_end() {
        let (sender, _receiver) = channel_with_backlog(1, PATIENCE);

        assert_eq!(sender.report(turn_started()), Ok(()), "the first fact fits");

        let started = Instant::now();

        assert_eq!(
            sender.report(turn_started()),
            Err(IngestRejection::Backlogged),
            "a queue nothing is draining refuses the fact instead of growing"
        );
        assert!(
            started.elapsed() >= PATIENCE,
            "it waited for the single writer before giving up"
        );
    }

    #[test]
    fn room_that_appears_inside_the_wait_is_used() {
        let (sender, receiver) = channel_with_backlog(1, Duration::from_secs(10));

        sender.report(turn_started()).expect("the first fact fits");

        let draining = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let taken = receiver.0.recv().expect("the fact is there");

            (receiver, taken)
        });

        assert_eq!(
            sender.report(turn_started()),
            Ok(()),
            "a writer that caught up inside the wait takes the fact"
        );

        let (_receiver, taken) = draining.join().expect("the draining thread finishes");
        assert_eq!(taken.run_id, 1);
    }

    #[test]
    fn a_queue_with_no_reader_is_not_something_to_wait_out() {
        let (sender, receiver) = channel_with_backlog(1, Duration::from_secs(60));
        drop(receiver);

        let started = Instant::now();

        assert_eq!(
            sender.report(turn_started()),
            Err(IngestRejection::ChannelClosed)
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the daemon is shutting down, and a reporter does not wait that out"
        );
    }
}
