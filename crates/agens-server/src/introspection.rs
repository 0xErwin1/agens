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

use crate::fsm::{Principal, RunFacts, RunTrigger, StateMachines, TransitionRejection};

/// The journal entry type every checkpoint is written as. Findings point back
/// at the row it creates; there is no separate checkpoint table.
pub const CHECKPOINT_EVENT: &str = "checkpoint";

/// Reads the current time as epoch seconds.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// One run's introspection surface, bound to the attempt that is executing it.
///
/// It holds the machines rather than the store because the store is theirs, and
/// it holds the run and attempt identity rather than taking them per call: a
/// worker cannot name a run it is not executing, so there is no argument for it
/// to get wrong.
pub struct RunIntrospection {
    machines: Arc<Mutex<StateMachines>>,
    run_id: i64,
    /// The physical execution this checkpoint was reported from. Written into
    /// the journal entry because it is the key half the evidence ledger is
    /// keyed by, and correlating a checkpoint with the paths that ledger
    /// recorded for the same attempt is what the genesis-path freeze needs.
    session_id: Option<i64>,
    session_attempt_id: Option<i64>,
    clock: Clock,
}

impl RunIntrospection {
    #[must_use]
    pub fn new(machines: Arc<Mutex<StateMachines>>, run_id: i64, clock: Clock) -> Self {
        Self {
            machines,
            run_id,
            session_id: None,
            session_attempt_id: None,
            clock,
        }
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

    fn checkpoint_event(&self, checkpoint: &Checkpoint, now: i64) -> EventRow {
        EventRow {
            id: None,
            run_id: Some(self.run_id),
            event_type: CHECKPOINT_EVENT.to_owned(),
            // A checkpoint is the worker describing its own work, which is what
            // separates the agent class from the machinery around it.
            class: EventClass::Agent,
            payload: self.checkpoint_payload(checkpoint).to_string(),
            ts: now,
        }
    }

    /// The typed journal payload.
    ///
    /// `credits_progress` and the per-class counts are derived here rather than
    /// left for a reader to recompute, because run health consumes them on
    /// every checkpoint and two derivations of the same rule drift.
    fn checkpoint_payload(&self, checkpoint: &Checkpoint) -> serde_json::Value {
        let claims: Vec<serde_json::Value> = checkpoint.claims().iter().map(claim_json).collect();

        serde_json::json!({
            "attempt": {
                "session_id": self.session_id,
                "session_attempt_id": self.session_attempt_id,
            },
            "next_goal": checkpoint.next_goal(),
            "hypothesis": checkpoint.hypothesis(),
            "revised_estimate_seconds": checkpoint.revised_estimate_seconds(),
            "blockers": checkpoint.blockers(),
            "next_checkpoint_at": checkpoint.next_checkpoint_at(),
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

    fn machines(&self) -> Result<std::sync::MutexGuard<'_, StateMachines>, RunIntrospectionError> {
        self.machines
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
        let event = self.checkpoint_event(checkpoint, now);
        let findings = self.finding_rows(checkpoint, now);

        let write = self
            .machines()?
            .record_checkpoint(&event, &findings)
            .map_err(refused)?;

        Ok(CheckpointReceipt {
            checkpoint_event_id: write.checkpoint_id,
            finding_ids: write.finding_ids,
            credited_progress: checkpoint.credits_progress(),
        })
    }

    fn ask(&mut self, ask: &Ask) -> Result<AskReceipt, RunIntrospectionError> {
        let now = self.now();
        let question = self.question_row(ask, now);

        let outcome = self
            .machines()?
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
