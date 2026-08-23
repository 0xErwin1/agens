//! What the worker reports about the turn it is running.
//!
//! Ingest is the only path a worker's evidence has into the control plane, and
//! this is its only producer in the daemon: the turn's own progress sink is
//! already carrying every tool result and every turn boundary, so the facts are
//! read off it rather than reconstructed from anything the model said.
//!
//! Two things have to be true before a fact means anything, and both of them
//! are this module's job:
//!
//! - **The physical attempt is named.** A fact belongs to an attempt, and the
//!   attempt ingest checks it against is `attempts.session_attempt_id`. The
//!   logical attempt is opened by the admission transition, before anything
//!   executes; the physical one is opened by the turn, from inside the session.
//!   Nothing can join them until the turn has begun, so the join is written
//!   here, once, the first time the turn says anything.
//! - **The join is written before the facts are.** Until it is, the evidence
//!   ledger cannot be reached from a run at all: the genesis-path freeze reads
//!   it through that same column, and a freeze that found nothing leaves the
//!   run with no declared paths to measure divergence against.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agens_core::{SessionMetadata, TurnEvent, TurnProgressSink};
use agens_server::{
    ApiCore, AttemptResolver, Attribution, CheckpointReporting, FactSender, IngestFact,
    ReportedFact,
};
use agens_store::SessionStore;

/// One turn's reporting surface, shared between the progress sink and the run's
/// introspection tools.
pub(crate) struct WorkerFacts {
    core: Arc<Mutex<ApiCore>>,
    facts: FactSender,
    run_id: i64,
    /// The durable session row the turn writes against. The physical attempt is
    /// this session's latest one.
    session_id: i64,
    data_directory: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Resolved and correlated once. `None` means the turn has not spoken yet.
    attribution: Option<Attribution>,
    /// Tokens the provider reported for this turn, as they arrive.
    tokens: u64,
}

impl WorkerFacts {
    pub(crate) fn new(
        core: &Arc<Mutex<ApiCore>>,
        facts: FactSender,
        run_id: i64,
        session: &SessionMetadata,
        data_directory: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            core: Arc::clone(core),
            facts,
            run_id,
            session_id: session.id,
            data_directory,
            state: Mutex::new(State::default()),
        })
    }

    /// The sink the turn reports through.
    pub(crate) fn progress_sink(self: &Arc<Self>) -> TurnProgressSink {
        let facts = Arc::clone(self);

        Arc::new(move |event: TurnEvent| facts.observe(&event))
    }

    /// How the run's `checkpoint` tool names the execution it is reporting from.
    pub(crate) fn checkpoint_reporting(self: &Arc<Self>) -> CheckpointReporting {
        let facts = Arc::clone(self);
        let resolver: AttemptResolver = Arc::new(move || facts.attribution());

        CheckpointReporting::new(self.facts.clone(), resolver)
    }

    /// Reports the turn's end, with everything the provider charged it.
    ///
    /// Called after the turn returns rather than from the sink: a turn ends by
    /// returning, and the states the sink sees say what the provider was doing,
    /// not that the harness is finished with the attempt. A turn that never
    /// reached the model reports nothing, because there is no attempt to
    /// attribute the ending to.
    pub(crate) fn report_turn_ended(&self) {
        let Some(attribution) = self.resolved() else {
            return;
        };

        let tokens = self.state.lock().map_or(0, |state| state.tokens);

        self.report(attribution, IngestFact::TurnEnded { tokens });
    }

    /// Reports that the provider refused this turn for quota.
    ///
    /// Reported before the turn's ending, because that is what tells the fold
    /// the turn apart from an idle one: a worker waiting on a reset made no
    /// progress and is not thereby a stalled worker.
    pub(crate) fn report_quota_reached(&self) {
        let Some(attribution) = self.resolved() else {
            return;
        };

        self.report(attribution, IngestFact::QuotaReached);
    }

    fn observe(&self, event: &TurnEvent) {
        // Every event is an occasion to establish the correlation, because the
        // first one is the earliest moment the physical attempt exists.
        let Some(attribution) = self.attribution() else {
            return;
        };

        match event {
            TurnEvent::Usage(usage) => {
                if let (Ok(mut state), Some(total)) = (self.state.lock(), usage.total_tokens) {
                    state.tokens = state.tokens.saturating_add(total);
                }
            }
            TurnEvent::ToolResultFacts { facts, .. } => {
                self.report(attribution, IngestFact::ToolResult(facts.clone()));
            }
            _ => {}
        }
    }

    /// The identity this turn's facts travel under, resolving and correlating
    /// it the first time it is asked for.
    ///
    /// Reporting the turn's start is part of establishing it: the fold reads a
    /// turn's boundaries, and a turn whose first fact arrived before its start
    /// would credit the previous turn's ledger.
    fn attribution(&self) -> Option<Attribution> {
        if let Some(attribution) = self.resolved() {
            return Some(attribution);
        }

        let attribution = self.correlate()?;

        let mut state = self.state.lock().ok()?;
        if let Some(already) = state.attribution {
            return Some(already);
        }
        state.attribution = Some(attribution);
        drop(state);

        self.report(attribution, IngestFact::TurnStarted);

        Some(attribution)
    }

    fn resolved(&self) -> Option<Attribution> {
        self.state.lock().ok().and_then(|state| state.attribution)
    }

    /// Joins the run's live attempt to the physical execution the turn opened,
    /// and reads back the identity that join produced.
    ///
    /// The physical id comes from the session store rather than from a tool's
    /// own facts: a turn that calls only `checkpoint` and `ask` reports no
    /// filesystem facts at all, and a correlation that waited for one would
    /// leave exactly the runs that behave well unattributable.
    ///
    /// Only an attempt that is still open. A turn that says something after
    /// its own leg was closed — one still finishing while a transition ended
    /// the attempt — would otherwise point the closed leg at this execution,
    /// and every fact after that would be accepted against a leg the control
    /// plane has already accounted for.
    fn correlate(&self) -> Option<Attribution> {
        let session_attempt_id = SessionStore::open(&self.data_directory)
            .ok()?
            .read_session(self.session_id)
            .ok()??
            .latest_attempt?
            .key()
            .attempt_id();

        let mut core = self.core.lock().ok()?;
        let attempt = core
            .machines()
            .store()
            .attempts_for_run(self.run_id)
            .ok()?
            .pop()
            .filter(|attempt| attempt.ended_at.is_none())?;
        let attempt_id = attempt.id?;

        core.correlate_attempt(attempt_id, session_attempt_id)
            .ok()?;

        Some(Attribution {
            attempt_id: session_attempt_id,
            turn: attempt.n,
        })
    }

    /// Queues one fact. A queue with no reader is the daemon shutting down, and
    /// a worker is not the party that acts on that.
    fn report(&self, attribution: Attribution, fact: IngestFact) {
        let _ = self.facts.report(ReportedFact {
            run_id: self.run_id,
            attempt_id: attribution.attempt_id,
            turn: attribution.turn,
            now: now(),
            fact,
        });
    }
}

/// Epoch seconds. Ingest reads no clock, so its reporters say what "now" means.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}
