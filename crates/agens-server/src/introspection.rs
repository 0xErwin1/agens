//! Where a worker's checkpoints and questions become control-plane rows.
//!
//! This is the implementation half of [`agens_core::run_introspection`]: the
//! tools crate collects a typed payload and knows nothing about SQLite, and
//! this module writes it through the state machines that own the tables.
//!
//! The two shapes are deliberately different, because what they cost the run is
//! different. A checkpoint adds to the journal and the evidence: it moves no
//! row, runs no guard, and a worker reporting one keeps working. A question
//! parks the run, so it goes through the run machine's `ask` transition and
//! lands with the question in the same write — a run on `awaiting_input` with
//! no question row can neither be answered nor resumed.
//!
//! Nothing here reads a clock. The timestamp comes from the clock the caller
//! installed, the same discipline the machines and the store keep, so a
//! coordinator replaying or reconciling decides what "now" means.

use std::sync::{Arc, Mutex};

use agens_core::run_introspection::{
    Ask, AskReceipt, Checkpoint, CheckpointReceipt, EvidenceClaim, RunIntrospectionError,
    RunIntrospectionPort,
};
use agens_store::{
    CausalDisposition, EventClass, EventRow, EvidenceClass, FindingRow, QuestionKind, QuestionRow,
    QuestionState,
};

use crate::api::ApiCore;
use crate::fsm::{Principal, RunEffect, RunFacts, RunTrigger, TransitionRejection};
use crate::ingest::{
    Attribution, BacklogNotice, CheckpointClaim, FactSender, IngestFact, RefusedReport,
    ReportedCheckpoint, ReportedFact,
};
use crate::timers::CHECKPOINT_EVENT;

/// Reads the current time as epoch seconds.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The journal entry a suspension the sessions port refused is recorded as.
const SESSION_SUSPEND_REFUSED_EVENT: &str = "session_suspend_refused";

/// What a run's checkpoint tool calls itself in a record of a fact it could
/// not report.
const CHECKPOINT_REPORTER: &str = "checkpoint_tool";

/// Resolves the physical execution the reports are coming from.
///
/// A function rather than a value because the surface is built before the turn
/// opens its session attempt: the tool runtime is constructed first, and the
/// row the reports have to name does not exist until the turn begins. `None`
/// means the correlation is not established, which is the one case where a
/// report has nothing to be attributed to.
pub type AttemptResolver = Arc<dyn Fn() -> Option<Attribution> + Send + Sync>;

/// Where a checkpoint goes after the journal: the ingest channel, and the
/// identity the fact travels under.
///
/// Optional on the surface because the checkpoint tool is also exercised
/// without a coordinator behind it. What it changes is not what is written but
/// what is derived: without it a checkpoint reaches the journal and the
/// evidence, and run health never hears that the worker said anything.
#[derive(Clone)]
pub struct CheckpointReporting {
    facts: FactSender,
    attempt: AttemptResolver,
}

impl CheckpointReporting {
    #[must_use]
    pub const fn new(facts: FactSender, attempt: AttemptResolver) -> Self {
        Self { facts, attempt }
    }

    fn attribution(&self) -> Option<Attribution> {
        (self.attempt)()
    }
}

/// One run's introspection surface, bound to the attempt that is executing it.
///
/// It reaches the machines through the service core because the core owns them,
/// and it holds the run and attempt identity rather than taking them per call:
/// a worker cannot name a run it is not executing, so there is no argument for
/// it to get wrong.
pub struct RunIntrospection {
    core: Arc<Mutex<ApiCore>>,
    run_id: i64,
    /// The physical execution this checkpoint was reported from. Written into
    /// the journal entry because it is the key half the evidence ledger is
    /// keyed by, and correlating a checkpoint with the paths that ledger
    /// recorded for the same attempt is what the genesis-path freeze needs.
    session_id: Option<i64>,
    session_attempt_id: Option<i64>,
    clock: Clock,
    reporting: Option<CheckpointReporting>,
    /// Whether this run already has an entry for the backlog it last met.
    backlog: BacklogNotice,
}

impl RunIntrospection {
    #[must_use]
    pub fn new(core: Arc<Mutex<ApiCore>>, run_id: i64, clock: Clock) -> Self {
        Self {
            core,
            run_id,
            session_id: None,
            session_attempt_id: None,
            clock,
            reporting: None,
            backlog: BacklogNotice::default(),
        }
    }

    /// Sends every checkpoint on to ingest as well as to the journal.
    #[must_use]
    pub fn reporting_to(mut self, reporting: CheckpointReporting) -> Self {
        self.reporting = Some(reporting);
        self
    }

    /// Names the physical execution the reports come from.
    #[must_use]
    pub const fn for_attempt(
        mut self,
        session_id: Option<i64>,
        session_attempt_id: Option<i64>,
    ) -> Self {
        self.session_id = session_id;
        self.session_attempt_id = session_attempt_id;
        self
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn checkpoint_event(
        &self,
        checkpoint: &Checkpoint,
        session_attempt_id: Option<i64>,
        now: i64,
    ) -> EventRow {
        EventRow {
            id: None,
            run_id: Some(self.run_id),
            event_type: CHECKPOINT_EVENT.to_owned(),
            // A checkpoint is the worker describing its own work, which is what
            // separates the agent class from the machinery around it.
            class: EventClass::Agent,
            payload: self
                .checkpoint_payload(checkpoint, session_attempt_id)
                .to_string(),
            ts: now,
        }
    }

    /// The typed journal payload.
    ///
    /// `credits_progress` and the per-class counts are derived here rather than
    /// left for a reader to recompute, because run health consumes them on
    /// every checkpoint and two derivations of the same rule drift.
    ///
    /// The worker's self-declared deadline is written as `promised_at`, which
    /// is the name the timer wheel reads it under: a checkpoint whose deadline
    /// the wheel cannot find declares no deadline at all.
    fn checkpoint_payload(
        &self,
        checkpoint: &Checkpoint,
        session_attempt_id: Option<i64>,
    ) -> serde_json::Value {
        let claims: Vec<serde_json::Value> = checkpoint.claims().iter().map(claim_json).collect();

        serde_json::json!({
            "attempt": {
                "session_id": self.session_id,
                "session_attempt_id": session_attempt_id,
            },
            "next_goal": checkpoint.next_goal(),
            "hypothesis": checkpoint.hypothesis(),
            "revised_estimate_seconds": checkpoint.revised_estimate_seconds(),
            "blockers": checkpoint.blockers(),
            "promised_at": checkpoint.next_checkpoint_at(),
            "touched_paths": checkpoint.touched_paths(),
            "carries_diff": checkpoint.carries_diff(),
            "credits_progress": checkpoint.credits_progress(),
            "evidence": claims,
            "evidence_classes": {
                "deterministic": class_count(checkpoint, agens_core::run_introspection::EvidenceClass::Deterministic),
                "inferential": class_count(checkpoint, agens_core::run_introspection::EvidenceClass::Inferential),
                "insufficient": class_count(checkpoint, agens_core::run_introspection::EvidenceClass::Insufficient),
            },
        })
    }

    fn finding_rows(&self, checkpoint: &Checkpoint, now: i64) -> Vec<FindingRow> {
        checkpoint
            .claims()
            .iter()
            .map(|claim| FindingRow {
                id: None,
                run_id: self.run_id,
                // Filled in by the store from the journal entry it is writing
                // in the same transaction.
                checkpoint_id: None,
                description: claim.description().to_owned(),
                evidence_class: evidence_class(claim.evidence_class()),
                proof_refs: serde_json::Value::from(claim.proof_refs()).to_string(),
                causal_disposition: causal_disposition(claim.disposition()),
                created_at: now,
            })
            .collect()
    }

    fn question_row(&self, ask: &Ask, now: i64) -> QuestionRow {
        let options: Vec<serde_json::Value> = ask
            .options()
            .iter()
            .map(|option| {
                serde_json::json!({
                    "id": option.id(),
                    "label": option.label(),
                    "consequence": option.consequence(),
                })
            })
            .collect();

        QuestionRow {
            id: None,
            run_id: self.run_id,
            // A worker's question is never an approval: an approval authorizes
            // bytes and is the user's to create, and its receipt columns are
            // what the pre-merge gate re-derives against.
            kind: QuestionKind::Question,
            blocked_decision: ask.blocked_decision().to_owned(),
            options: serde_json::Value::Array(options).to_string(),
            recommendation: ask.recommendation().map(str::to_owned),
            answer: None,
            author: None,
            expires_at: None,
            tree_hash: None,
            paths_digest: None,
            state: QuestionState::Open,
            created_at: now,
        }
    }

    /// Records a checkpoint the ingest queue would not take, once while the
    /// backlog stands.
    ///
    /// Best effort: it is already reporting that a write did not happen, and a
    /// second failure has nowhere to go.
    fn observe_report(&mut self, refused: Option<RefusedReport>) {
        let Some(refused) = self.backlog.observe(refused) else {
            return;
        };

        if let Ok(mut core) = self.core.lock() {
            let _ = core.journal_backlogged_fact(CHECKPOINT_REPORTER, &refused);
        }
    }

    /// The service core, locked for the span of one write.
    ///
    /// A poisoned lock is refused rather than recovered: the core's invariants
    /// were established by an operation that did not finish, and a worker's
    /// report is not the place to decide they still hold.
    fn core(&self) -> Result<std::sync::MutexGuard<'_, ApiCore>, RunIntrospectionError> {
        self.core
            .lock()
            .map_err(|_| RunIntrospectionError::Unavailable)
    }
}

impl RunIntrospectionPort for RunIntrospection {
    fn checkpoint(
        &mut self,
        checkpoint: &Checkpoint,
    ) -> Result<CheckpointReceipt, RunIntrospectionError> {
        let now = self.now();
        // Resolved before the write so the journal entry and the fact name the
        // same physical execution: an entry attributed to one attempt and a
        // health signal attributed to another describe two different runs.
        let attribution = self
            .reporting
            .as_ref()
            .and_then(CheckpointReporting::attribution);
        let session_attempt_id = self
            .session_attempt_id
            .or_else(|| attribution.and_then(|attempt| attempt.attempt_id));

        let event = self.checkpoint_event(checkpoint, session_attempt_id, now);
        let findings = self.finding_rows(checkpoint, now);

        let write = self
            .core()?
            .machines_mut()
            .record_checkpoint(&event, &findings)
            .map_err(refused)?;

        // A checkpoint that reached the journal happened, whatever ingest makes
        // of the fact. A queue with no reader is the daemon shutting down, and
        // failing the worker's tool call over it would turn an orderly stop
        // into a failed attempt. A queue that is merely full is a different
        // thing: the checkpoint is in the journal and the health plane will
        // never hear about it, so the run says so.
        if let (Some(reporting), Some(attribution)) = (self.reporting.as_ref(), attribution) {
            let refused = reporting.facts.report(ReportedFact {
                run_id: self.run_id,
                attempt_id: attribution.attempt_id,
                turn: attribution.turn,
                now,
                fact: IngestFact::Checkpoint(reported_checkpoint(checkpoint)),
            });

            self.observe_report(refused.err());
        }

        Ok(CheckpointReceipt {
            checkpoint_event_id: write.checkpoint_id,
            finding_ids: write.finding_ids,
            credited_progress: checkpoint.credits_progress(),
        })
    }

    fn ask(&mut self, ask: &Ask) -> Result<AskReceipt, RunIntrospectionError> {
        let now = self.now();
        let question = self.question_row(ask, now);

        let mut core = self.core()?;

        let outcome = core
            .machines_mut()
            .apply_run(
                self.run_id,
                RunTrigger::Ask,
                &RunFacts {
                    now,
                    // The coordinator is reporting a fact the harness gave it.
                    // A client cannot park somebody else's run by claiming one.
                    principal: Principal::Coordinator,
                    opened_question: Some(question),
                    ..RunFacts::default()
                },
            )
            .map_err(refused)?;

        // The run parked, so the session behind it is asked to stop. It is
        // asked rather than fails the call: the worker is inside this very tool
        // call, cancellation is cooperative, and the question is already
        // durable — the ask succeeded whether or not the session hears about it
        // before the turn ends.
        if outcome
            .applied()
            .is_some_and(|applied| applied.effects.contains(&RunEffect::SuspendSession))
            && let Err(error) = core.ports().sessions.suspend(self.run_id)
        {
            // A suspension that could not be performed leaves a session
            // running behind a parked run, which is the state this exists to
            // end. It is journaled rather than raised, so the record of it
            // outlives the turn that failed to stop.
            let _ = core.machines_mut().journal(&[EventRow {
                id: None,
                run_id: Some(self.run_id),
                event_type: SESSION_SUSPEND_REFUSED_EVENT.to_owned(),
                class: EventClass::Infra,
                payload: serde_json::json!({ "reason": error.to_string() }).to_string(),
                ts: now,
            }]);
        }

        drop(core);

        let question_id = outcome
            .applied()
            .and_then(|transition| transition.opened_question_id)
            .ok_or_else(|| {
                RunIntrospectionError::Refused(
                    "the run parked without opening the question it parked on".to_owned(),
                )
            })?;

        Ok(AskReceipt {
            question_id,
            run_id: self.run_id,
        })
    }
}

/// Closes the seam ingest left open: it declared the half of a checkpoint it
/// consumes as a trait rather than depending on this row's shape, and this is
/// the row.
///
/// `claims_progress` reads the disposition, because that is where a worker says
/// whether it is reporting its own work or something it found already there.
/// The pairing is the same one [`EvidenceClaim::credits_progress`] makes, so
/// the number written into the journal and the number health derives cannot
/// disagree.
impl CheckpointClaim for EvidenceClaim {
    fn evidence_class(&self) -> EvidenceClass {
        // Spelled through the domain enum rather than through `self`, where the
        // inherent accessor and this trait method share a name.
        evidence_class(agens_core::run_introspection::EvidenceClaim::evidence_class(self))
    }

    fn claims_progress(&self) -> bool {
        self.disposition() == agens_core::run_introspection::CausalDisposition::CandidateCaused
    }
}

/// The one claim that carries the checkpoint, as ingest reads it.
///
/// Health folds a checkpoint as a single claim, so a checkpoint with several
/// is reduced to the one that decides the outcome: the first that credits
/// progress, and otherwise the first there is. A checkpoint with no claim at
/// all is still a checkpoint — it marks the run as having reported, and it
/// credits nothing.
fn reported_checkpoint(checkpoint: &Checkpoint) -> ReportedCheckpoint {
    checkpoint
        .claims()
        .iter()
        .find(|claim| claim.credits_progress())
        .or_else(|| checkpoint.claims().first())
        .map_or(
            ReportedCheckpoint::new(EvidenceClass::Insufficient, false),
            ReportedCheckpoint::from_claim,
        )
}

fn refused(rejection: TransitionRejection) -> RunIntrospectionError {
    RunIntrospectionError::Refused(rejection.to_string())
}

fn claim_json(claim: &EvidenceClaim) -> serde_json::Value {
    serde_json::json!({
        "description": claim.description(),
        "evidence_class": claim.evidence_class().as_str(),
        "proof_refs": claim.proof_refs(),
        "disposition": claim.disposition().as_str(),
        "credits_progress": claim.evidence_class().credits_progress(),
    })
}

fn class_count(
    checkpoint: &Checkpoint,
    class: agens_core::run_introspection::EvidenceClass,
) -> usize {
    checkpoint
        .claims()
        .iter()
        .filter(|claim| claim.evidence_class() == class)
        .count()
}

/// The domain's class and the column's class are the same three values kept in
/// two crates that may not depend on each other, so the mapping is written out
/// rather than derived.
fn evidence_class(class: agens_core::run_introspection::EvidenceClass) -> EvidenceClass {
    match class {
        agens_core::run_introspection::EvidenceClass::Deterministic => EvidenceClass::Deterministic,
        agens_core::run_introspection::EvidenceClass::Inferential => EvidenceClass::Inferential,
        agens_core::run_introspection::EvidenceClass::Insufficient => EvidenceClass::Insufficient,
    }
}

fn causal_disposition(
    disposition: agens_core::run_introspection::CausalDisposition,
) -> CausalDisposition {
    match disposition {
        agens_core::run_introspection::CausalDisposition::CandidateCaused => {
            CausalDisposition::CandidateCaused
        }
        agens_core::run_introspection::CausalDisposition::PreExisting => {
            CausalDisposition::PreExisting
        }
        agens_core::run_introspection::CausalDisposition::Unknown => CausalDisposition::Unknown,
    }
}
