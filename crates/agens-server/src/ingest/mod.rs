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

use std::{
    collections::HashMap,
    sync::mpsc::{Receiver, Sender, TryRecvError, channel as mpsc_channel},
};

use agens_core::ToolResultFacts;
use agens_store::{
    ControlPlaneError, ControlPlaneStore, EventClass, EventRow, IngestWrite, RunHealthRow,
};

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

/// The reporting end of the in-process ingest channel. Cloneable, so every
/// session's sink holds one.
#[derive(Clone, Debug)]
pub struct FactSender(Sender<ReportedFact>);

impl FactSender {
    /// Queues one fact for the single writer.
    pub fn report(&self, fact: ReportedFact) -> Result<(), IngestRejection> {
        self.0
            .send(fact)
            .map_err(|_| IngestRejection::ChannelClosed)
    }
}

/// The draining end. Held by whoever owns the [`Ingest`] writer.
#[derive(Debug)]
pub struct FactReceiver(Receiver<ReportedFact>);

/// Opens the in-process ingest channel.
#[must_use]
pub fn channel() -> (FactSender, FactReceiver) {
    let (sender, receiver) = mpsc_channel();
    (FactSender(sender), FactReceiver(receiver))
}

/// The single writer of the harness's facts and of the health they derive.
pub struct Ingest {
    store: ControlPlaneStore,
    thresholds: HealthThresholds,
    /// Per-run derived state, rebuilt from the journal for any run this process
    /// has not seen — which after a restart is every one of them.
    states: HashMap<i64, HealthState>,
}

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
            states: HashMap::new(),
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

    fn state_for(&self, run_id: i64) -> Result<HealthState, IngestRejection> {
        match self.states.get(&run_id) {
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
