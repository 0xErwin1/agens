//! Store rows and core answers, as the wire carries them.
//!
//! Every lifecycle vocabulary — run state, worktree status, question kind and
//! state, event class — crosses as the string the store already writes. The
//! alternative is a protobuf enum per vocabulary, which is a second definition
//! of the same set that nothing keeps in step with the first; a value the store
//! learns and the enum has not would arrive as `UNSPECIFIED` and read as a bug
//! in the run rather than in the copy.
//!
//! A row missing the id the database assigned it cannot be projected, and is
//! reported rather than sent as zero: an id of zero on the wire is a run a
//! client can ask about and never find.

use agens_store::{
    AttemptRow, EventClass, EventRow, FindingRow, QuestionRow, RunHealthRow, RunRow,
};
use tonic::Status;

use crate::api::{
    AdmissionState, AnsweredQuestion, InboxItem, InboxView, RunSummary, RunView, TreeSnapshot,
};
use crate::fsm::TransitionOutcome;

use super::proto;

/// The id a stored row must have, or the reason it cannot be projected.
fn assigned(id: Option<i64>, subject: &str) -> Result<i64, Status> {
    id.ok_or_else(|| Status::internal(format!("a stored {subject} has no id")))
}

/// One transition, flattened for a client that only needs to know whether
/// anything moved and where to.
pub(super) fn transition<S, E>(outcome: &TransitionOutcome<S, E>) -> proto::Transition
where
    S: StateName,
{
    outcome.applied().map_or_else(
        || proto::Transition {
            applied: false,
            from: String::new(),
            to: String::new(),
        },
        |applied| proto::Transition {
            applied: true,
            from: applied.from.state_name().to_owned(),
            to: applied.to.state_name().to_owned(),
        },
    )
}

/// The name a lifecycle state travels under.
///
/// It is the store's own spelling in every case, so the string on the wire and
/// the string in the column are the same string.
pub(super) trait StateName: Copy {
    fn state_name(self) -> &'static str;
}

impl StateName for agens_store::RunState {
    fn state_name(self) -> &'static str {
        self.as_str()
    }
}

impl StateName for agens_store::QuestionState {
    fn state_name(self) -> &'static str {
        self.as_str()
    }
}

impl StateName for agens_store::WorktreeStatus {
    fn state_name(self) -> &'static str {
        self.as_str()
    }
}

pub(super) fn answer_ack(answered: &AnsweredQuestion) -> proto::AnswerAck {
    proto::AnswerAck {
        run_id: answered.run_id,
        question: Some(transition(&answered.question)),
        run: answered.run.as_ref().map(transition),
    }
}

pub(super) const fn admission_state(state: AdmissionState) -> proto::AdmissionState {
    proto::AdmissionState {
        paused: state.paused,
        previously_paused: state.previously_paused,
        changed: state.changed(),
    }
}

pub(super) fn event(row: &EventRow) -> Result<proto::Event, Status> {
    Ok(proto::Event {
        id: assigned(row.id, "event")?,
        run_id: row.run_id,
        r#type: row.event_type.clone(),
        class: row.class.as_str().to_owned(),
        payload: row.payload.clone(),
        ts: row.ts,
    })
}

/// Parses the classes a subscriber asked for.
///
/// An unknown class is refused rather than dropped: a filter silently widened
/// to "every class" hands the subscriber more than it asked for, and a filter
/// silently narrowed hands it a stream that looks empty for no stated reason.
pub(super) fn event_classes(names: &[String]) -> Result<Vec<EventClass>, Status> {
    names
        .iter()
        .map(|name| {
            EventClass::parse(name)
                .ok_or_else(|| Status::invalid_argument(format!("no event class named {name}")))
        })
        .collect()
}

pub(super) fn run_summary(summary: &RunSummary) -> proto::RunSummary {
    proto::RunSummary {
        run_id: summary.run_id,
        task: summary.task.clone(),
        state: summary.state.as_str().to_owned(),
        priority: summary.priority,
        provider: summary.provider.clone(),
        worktree_status: summary
            .worktree_status
            .map(|status| status.as_str().to_owned()),
        parent_run_id: summary.parent_run_id,
        external_ref: summary.external_ref.clone(),
        created_at: summary.created_at,
    }
}

pub(super) fn tree_snapshot(snapshot: &TreeSnapshot) -> proto::TreeSnapshot {
    proto::TreeSnapshot {
        repo_id: snapshot.repo_id.clone(),
        runs: snapshot.runs.iter().map(run_summary).collect(),
    }
}

pub(super) fn run(row: &RunRow) -> Result<proto::Run, Status> {
    Ok(proto::Run {
        run_id: assigned(row.id, "run")?,
        repo_id: row.repo_id.clone(),
        repo_root: row.repo_root.clone(),
        remote_url: row.remote_url.clone(),
        external_ref: row.external_ref.clone(),
        parent_run_id: row.parent_run_id,
        task: row.task.clone(),
        scope: row.scope.clone(),
        dod: row.dod.clone(),
        genesis_paths: row.genesis_paths.clone(),
        state: row.state.as_str().to_owned(),
        priority: row.priority,
        dep_run_id: row.dep_run_id,
        provider: row.provider.clone(),
        budget_tokens: row.budget_tokens,
        worktree_path: row.worktree_path.clone(),
        worktree_status: row.worktree_status.map(|status| status.as_str().to_owned()),
        created_at: row.created_at,
        result: row.result.clone(),
    })
}

pub(super) fn attempt(row: &AttemptRow) -> Result<proto::Attempt, Status> {
    Ok(proto::Attempt {
        attempt_id: assigned(row.id, "attempt")?,
        run_id: row.run_id,
        n: row.n,
        session_id: row.session_id,
        session_attempt_id: row.session_attempt_id,
        started_at: row.started_at,
        ended_at: row.ended_at,
        outcome: row.outcome.map(|outcome| outcome.as_str().to_owned()),
        retry_trigger: row.retry_trigger.map(|trigger| trigger.as_str().to_owned()),
        tokens: row.tokens,
        cost_micros: row.cost_micros,
    })
}

pub(super) fn question(row: &QuestionRow) -> Result<proto::Question, Status> {
    Ok(proto::Question {
        question_id: assigned(row.id, "question")?,
        run_id: row.run_id,
        kind: row.kind.as_str().to_owned(),
        blocked_decision: row.blocked_decision.clone(),
        options: row.options.clone(),
        recommendation: row.recommendation.clone(),
        answer: row.answer.clone(),
        author: row.author.map(|author| author.as_str().to_owned()),
        expires_at: row.expires_at,
        tree_hash: row.tree_hash.clone(),
        paths_digest: row.paths_digest.clone(),
        state: row.state.as_str().to_owned(),
        created_at: row.created_at,
    })
}

pub(super) fn finding(row: &FindingRow) -> Result<proto::Finding, Status> {
    Ok(proto::Finding {
        finding_id: assigned(row.id, "finding")?,
        run_id: row.run_id,
        checkpoint_id: row.checkpoint_id,
        description: row.description.clone(),
        evidence_class: row.evidence_class.as_str().to_owned(),
        proof_refs: row.proof_refs.clone(),
        causal_disposition: row.causal_disposition.as_str().to_owned(),
        created_at: row.created_at,
    })
}

pub(super) fn run_health(row: &RunHealthRow) -> proto::RunHealth {
    proto::RunHealth {
        run_id: row.run_id,
        last_progress_turn: row.last_progress_turn,
        noop_turns: row.noop_turns,
        failing_test_signature: row.failing_test_signature.clone(),
        tokens_since_progress: row.tokens_since_progress,
        updated_at: row.updated_at,
    }
}

pub(super) fn run_view(view: &RunView) -> Result<proto::RunView, Status> {
    Ok(proto::RunView {
        run: Some(run(&view.run)?),
        attempts: view
            .attempts
            .iter()
            .map(attempt)
            .collect::<Result<Vec<_>, _>>()?,
        questions: view
            .questions
            .iter()
            .map(question)
            .collect::<Result<Vec<_>, _>>()?,
        findings: view
            .findings
            .iter()
            .map(finding)
            .collect::<Result<Vec<_>, _>>()?,
        events: view
            .events
            .iter()
            .map(event)
            .collect::<Result<Vec<_>, _>>()?,
        health: view.health.as_ref().map(run_health),
    })
}

pub(super) fn inbox_item(item: &InboxItem) -> proto::InboxItem {
    proto::InboxItem {
        run_id: item.run_id,
        question_id: item.question_id,
        kind: item.kind.as_str().to_owned(),
        blocked_decision: item.blocked_decision.clone(),
        options: item.options.clone(),
        recommendation: item.recommendation.clone(),
        expires_at: item.expires_at,
        created_at: item.created_at,
    }
}

pub(super) fn inbox_view(view: &InboxView) -> proto::InboxView {
    proto::InboxView {
        repo_id: view.repo_id.clone(),
        items: view.items.iter().map(inbox_item).collect(),
    }
}
