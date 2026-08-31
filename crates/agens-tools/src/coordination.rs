//! The `team_*` tools: the manager's surface onto the control plane.
//!
//! Every one of them is the same shape — parse a typed request, hand it to a
//! [`CoordinationPort`], report what came back — and that shape is what keeps
//! the hard invariant of this group true by construction: **no `team_*` tool
//! mutates code and none of them runs git.**
//!
//! It holds because of what this module cannot reach rather than because of
//! what it declines to do. A [`TeamTool`] owns exactly one thing, the port, and
//! the port's whole vocabulary is run identifiers and bounded text
//! ([`agens_core::coordination`]). No argument any of these tools accepts is a
//! path, a revision or a branch, so there is no value here a filesystem or a
//! git invocation could be built from, and nothing in this module opens a file,
//! spawns a process or reaches [`crate::NativeTools`] — which is where every
//! confined filesystem and process surface of this crate lives.
//!
//! `team_merge` and `team_reclaim` are therefore requests and not actions. The
//! first opens the authorization the user answers; the second asks the
//! coordinator to release a worktree, which the coordinator re-derives from git
//! and refuses when git disagrees. Praetor authorizes and asks; it never
//! executes.

use agens_core::Error;
use agens_core::ToolAccess;
use agens_core::coordination::{
    AnswerReceipt, AnswerRequest, CancelRequest, CoordinationError, CoordinationPort,
    CoordinationRequestError, DirectRequest, EscalateRequest, MAX_ANSWER_CHARS,
    MAX_DIRECTIVE_CHARS, MAX_DOD_CHARS, MAX_GUIDANCE_CHARS, MAX_REASON_CHARS, MAX_SCOPE_CHARS,
    MAX_TASK_CHARS, MergeRequest, MergeRequestReceipt, ReclaimReceipt, ReclaimRequest,
    ReportRequest, RetryRequest, RunReport, RunStateReceipt, SpawnReceipt, SpawnRequest,
    TeamQuestion, TeamRun, TeamStatus,
};
use agens_core::run_introspection::{
    Ask, MAX_ASK_DECISION_CHARS, MAX_ASK_OPTION_ID_CHARS, MAX_ASK_OPTION_LABEL_CHARS,
    MAX_ASK_OPTIONS, MAX_ASK_RECOMMENDATION_CHARS,
};
use serde_json::{Map, Value};

use crate::run_introspection::{
    describe_ask_error, object_with_only, optional_integer, optional_string, parse_option,
    required_string,
};
use crate::{
    DispatchTool, ToolExecutionContext, ToolExecutionStatus, ToolOutput, sanitized_execution_status,
};

/// One operation of the coordination group.
///
/// The group is a table rather than ten types because every member differs only
/// in its arguments and its receipt. Adding one without deciding its name, its
/// schema and whether it changes anything does not compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeamVerb {
    Status,
    Report,
    Answer,
    Escalate,
    Direct,
    Cancel,
    Spawn,
    Retry,
    Merge,
    Reclaim,
}

impl TeamVerb {
    /// The group, in the order a surface offers it.
    pub const ALL: [Self; 10] = [
        Self::Status,
        Self::Report,
        Self::Answer,
        Self::Escalate,
        Self::Direct,
        Self::Cancel,
        Self::Spawn,
        Self::Retry,
        Self::Merge,
        Self::Reclaim,
    ];

    #[must_use]
    pub const fn tool_name(self) -> &'static str {
        match self {
            Self::Status => "team_status",
            Self::Report => "team_report",
            Self::Answer => "team_answer",
            Self::Escalate => "team_escalate",
            Self::Direct => "team_direct",
            Self::Cancel => "team_cancel",
            Self::Spawn => "team_spawn",
            Self::Retry => "team_retry",
            Self::Merge => "team_merge",
            Self::Reclaim => "team_reclaim",
        }
    }

    /// What a caller is told the operation does.
    ///
    /// The two request verbs say so in their first clause. A manager that reads
    /// `team_merge` as "merge" would report work as landed that nobody has
    /// authorized, and the description is where that is actually read.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Status => {
                "Every run of the team you manage, with everything currently waiting on an answer"
            }
            Self::Report => {
                "One run in full: its scope, its attempts, what it claimed and what it is blocked on"
            }
            Self::Answer => {
                "Answer a detail question a run is blocked on, choosing one of the options it \
                 offered. An authorization is never a detail question and is refused here"
            }
            Self::Escalate => {
                "Hand a decision to the person, with the options it is choosing between. It lands \
                 in their inbox"
            }
            Self::Direct => {
                "Queue guidance that changes what a run is doing. It is delivered at the run's \
                 next safe point rather than immediately"
            }
            Self::Cancel => "Stop a run, with why it is being stopped",
            Self::Spawn => {
                "Propose a run for the team. It lands as a draft: only the person's approval \
                 freezes its scope and queues it"
            }
            Self::Retry => {
                "Queue another attempt at a scope that was already approved, with the guidance \
                 that makes it a different attempt"
            }
            Self::Merge => {
                "Ask the person to authorize landing a run's branch. This authorizes nothing and \
                 merges nothing: it opens the decision, frozen over the bytes as they stand"
            }
            Self::Reclaim => {
                "Ask the coordinator to release a run's worktree. It re-derives from git whether \
                 the branch really landed and refuses when it did not"
            }
        }
    }

    /// Whether the operation changes anything.
    ///
    /// Nothing in this group reaches the filesystem, so this is about the
    /// control plane: the two reads are projections of rows the state machines
    /// already wrote.
    #[must_use]
    pub const fn access(self) -> ToolAccess {
        match self {
            Self::Status | Self::Report => ToolAccess::ReadOnly,
            _ => ToolAccess::Write,
        }
    }

    #[must_use]
    pub fn input_schema(self) -> Value {
        match self {
            Self::Status => object_schema(serde_json::json!({}), &[]),
            Self::Report | Self::Reclaim => {
                object_schema(serde_json::json!({"run_id": run_identifier()}), &["run_id"])
            }
            Self::Answer => object_schema(
                serde_json::json!({
                    "question_id": run_identifier(),
                    "answer": bounded_text(MAX_ANSWER_CHARS),
                }),
                &["question_id", "answer"],
            ),
            Self::Escalate => object_schema(
                serde_json::json!({
                    "run_id": run_identifier(),
                    "blocked_decision": bounded_text(MAX_ASK_DECISION_CHARS),
                    "options": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_ASK_OPTIONS,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["id", "label"],
                            "properties": {
                                "id": bounded_text(MAX_ASK_OPTION_ID_CHARS),
                                "label": bounded_text(MAX_ASK_OPTION_LABEL_CHARS),
                                "consequence": bounded_text(MAX_ASK_OPTION_LABEL_CHARS),
                            }
                        }
                    },
                    "recommendation": bounded_text(MAX_ASK_RECOMMENDATION_CHARS),
                }),
                &["run_id", "blocked_decision", "options"],
            ),
            Self::Direct => object_schema(
                serde_json::json!({
                    "run_id": run_identifier(),
                    "directive": bounded_text(MAX_DIRECTIVE_CHARS),
                }),
                &["run_id", "directive"],
            ),
            Self::Cancel => object_schema(
                serde_json::json!({
                    "run_id": run_identifier(),
                    "reason": bounded_text(MAX_REASON_CHARS),
                }),
                &["run_id", "reason"],
            ),
            Self::Spawn => object_schema(
                serde_json::json!({
                    "task": bounded_text(MAX_TASK_CHARS),
                    "scope": bounded_text(MAX_SCOPE_CHARS),
                    "dod": bounded_text(MAX_DOD_CHARS),
                    "priority": {"type": "integer"},
                    "parent_run_id": run_identifier(),
                    "dep_run_id": run_identifier(),
                }),
                &["task", "scope", "dod"],
            ),
            Self::Retry => object_schema(
                serde_json::json!({
                    "run_id": run_identifier(),
                    "guidance": bounded_text(MAX_GUIDANCE_CHARS),
                }),
                &["run_id", "guidance"],
            ),
            Self::Merge => object_schema(
                serde_json::json!({
                    "run_id": run_identifier(),
                    "reason": bounded_text(MAX_REASON_CHARS),
                }),
                &["run_id", "reason"],
            ),
        }
    }

    /// The argument names this verb accepts, and nothing else. A key outside
    /// the list is a malformed call rather than one that is quietly ignored.
    fn accepted_arguments(self) -> &'static [&'static str] {
        match self {
            Self::Status => &[],
            Self::Report | Self::Reclaim => &["run_id"],
            Self::Answer => &["question_id", "answer"],
            Self::Escalate => &["run_id", "blocked_decision", "options", "recommendation"],
            Self::Direct => &["run_id", "directive"],
            Self::Cancel | Self::Merge => &["run_id", "reason"],
            Self::Spawn => &[
                "task",
                "scope",
                "dod",
                "priority",
                "parent_run_id",
                "dep_run_id",
            ],
            Self::Retry => &["run_id", "guidance"],
        }
    }
}

/// One operation of the coordination group, bound to the team it manages.
///
/// It holds a port and nothing else. There is no working directory here, no
/// confinement root and no process surface, because none of these operations
/// has anywhere to put one.
pub struct TeamTool {
    verb: TeamVerb,
    port: Box<dyn CoordinationPort>,
}

impl TeamTool {
    pub fn new(verb: TeamVerb, port: Box<dyn CoordinationPort>) -> Self {
        Self { verb, port }
    }

    #[must_use]
    pub const fn verb(&self) -> TeamVerb {
        self.verb
    }

    fn dispatch(&mut self, arguments: &Value) -> Result<Value, String> {
        let object = object_with_only(arguments, self.verb.accepted_arguments())
            .ok_or_else(|| "arguments are invalid".to_owned())?;

        match self.verb {
            TeamVerb::Status => self.port.status().map(encode_status).map_err(describe),
            TeamVerb::Report => {
                let request =
                    ReportRequest::new(run_id(object, "run_id")?).map_err(describe_request)?;

                self.port
                    .report(&request)
                    .map(encode_report)
                    .map_err(describe)
            }
            TeamVerb::Answer => {
                let request = AnswerRequest::new(
                    run_id(object, "question_id")?,
                    required_string(object, "answer")?,
                )
                .map_err(describe_request)?;

                self.port
                    .answer(&request)
                    .map(encode_answer)
                    .map_err(describe)
            }
            TeamVerb::Escalate => {
                let request = EscalateRequest::new(run_id(object, "run_id")?, parse_ask(object)?)
                    .map_err(describe_request)?;

                self.port
                    .escalate(&request)
                    .map(|receipt| {
                        serde_json::json!({
                            "status": "escalated",
                            "question_id": receipt.question_id,
                            "run_id": receipt.run_id,
                        })
                    })
                    .map_err(describe)
            }
            TeamVerb::Direct => {
                let request = DirectRequest::new(
                    run_id(object, "run_id")?,
                    required_string(object, "directive")?,
                )
                .map_err(describe_request)?;

                self.port
                    .direct(&request)
                    .map(
                        |receipt| serde_json::json!({"status": "queued", "run_id": receipt.run_id}),
                    )
                    .map_err(describe)
            }
            TeamVerb::Cancel => {
                let request = CancelRequest::new(
                    run_id(object, "run_id")?,
                    required_string(object, "reason")?,
                )
                .map_err(describe_request)?;

                self.port
                    .cancel(&request)
                    .map(|receipt| encode_run_state(&receipt, "cancelled"))
                    .map_err(describe)
            }
            TeamVerb::Spawn => {
                let request = SpawnRequest::new(
                    required_string(object, "task")?,
                    required_string(object, "scope")?,
                    required_string(object, "dod")?,
                    optional_integer(object, "priority")?.unwrap_or_default(),
                    optional_integer(object, "parent_run_id")?,
                    optional_integer(object, "dep_run_id")?,
                )
                .map_err(describe_request)?;

                self.port
                    .spawn(&request)
                    .map(|receipt| encode_spawn(&receipt))
                    .map_err(describe)
            }
            TeamVerb::Retry => {
                let request = RetryRequest::new(
                    run_id(object, "run_id")?,
                    required_string(object, "guidance")?,
                )
                .map_err(describe_request)?;

                self.port
                    .retry(&request)
                    .map(|receipt| encode_run_state(&receipt, "queued"))
                    .map_err(describe)
            }
            TeamVerb::Merge => {
                let request = MergeRequest::new(
                    run_id(object, "run_id")?,
                    required_string(object, "reason")?,
                )
                .map_err(describe_request)?;

                self.port
                    .request_merge(&request)
                    .map(|receipt| encode_merge_request(&receipt))
                    .map_err(describe)
            }
            TeamVerb::Reclaim => {
                let request =
                    ReclaimRequest::new(run_id(object, "run_id")?).map_err(describe_request)?;

                self.port
                    .request_reclaim(&request)
                    .map(|receipt| encode_reclaim(&receipt))
                    .map_err(describe)
            }
        }
    }
}

impl DispatchTool for TeamTool {
    /// The operation itself. Nothing in a coordination call is a
    /// permission-bearing target — no path, no host, no command — so a rule can
    /// only usefully match which operation it is.
    fn permission_target(&self, _arguments: &Value) -> Result<String, Error> {
        Ok(self.verb.tool_name().to_owned())
    }

    fn execute(
        &mut self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, Error> {
        if context.is_cancelled() {
            return Ok(sanitized_execution_status(ToolExecutionStatus::Cancelled));
        }

        let name = self.verb.tool_name();

        match self.dispatch(&arguments) {
            Ok(value) => Ok(ToolOutput::success(value.to_string())),
            Err(reason) => Ok(ToolOutput::failure(format!("{name}: {reason}"))),
        }
    }
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": properties,
    })
}

fn bounded_text(max_chars: usize) -> Value {
    serde_json::json!({"type": "string", "minLength": 1, "maxLength": max_chars})
}

fn run_identifier() -> Value {
    serde_json::json!({"type": "integer", "minimum": 1})
}

fn run_id(object: &Map<String, Value>, key: &'static str) -> Result<i64, String> {
    object
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{key} is required and must be a whole number"))
}

fn parse_ask(object: &Map<String, Value>) -> Result<Ask, String> {
    let options = object
        .get("options")
        .and_then(Value::as_array)
        .ok_or_else(|| "options must be an array".to_owned())?
        .iter()
        .map(parse_option)
        .collect::<Result<Vec<_>, _>>()?;

    Ask::new(
        required_string(object, "blocked_decision")?,
        options,
        optional_string(object, "recommendation")?,
    )
    .map_err(describe_ask_error)
}

/// The port's refusal, in the terms the caller can act on.
///
/// [`CoordinationError::NoTeam`] and [`CoordinationError::Unauthorized`] are
/// spelled out as the dead ends they are: nothing about the arguments changes
/// either answer, and a caller that reads them as a transient failure retries a
/// call that cannot ever work.
fn describe(error: CoordinationError) -> String {
    match error {
        CoordinationError::NoTeam => {
            "this session manages no team, so there is nothing to coordinate. Calling again will \
             not change that"
                .to_owned()
        }
        CoordinationError::Unauthorized(detail) => {
            format!("{detail}. Calling again will not change that")
        }
        CoordinationError::Refused(detail) | CoordinationError::NotFound(detail) => detail,
        CoordinationError::Unavailable => "the control plane is unavailable".to_owned(),
    }
}

fn describe_request(error: CoordinationRequestError) -> String {
    match error {
        CoordinationRequestError::EmptyField(field) => format!("{field} cannot be empty"),
        CoordinationRequestError::FieldTooLong(field) => format!("{field} is too long"),
        CoordinationRequestError::ControlCharacter(field) => {
            format!("{field} contains a control character")
        }
        CoordinationRequestError::NotAnIdentifier(field) => {
            format!("{field} must be the identifier of an existing row")
        }
        CoordinationRequestError::RequestTooLarge => "the request is too large".to_owned(),
        CoordinationRequestError::Question(error) => describe_ask_error(error),
    }
}

fn encode_status(status: TeamStatus) -> Value {
    serde_json::json!({
        "repo_id": status.repo_id,
        "runs": status.runs.iter().map(encode_run).collect::<Vec<_>>(),
        "open_questions": status
            .open_questions
            .iter()
            .map(encode_question)
            .collect::<Vec<_>>(),
    })
}

fn encode_report(report: RunReport) -> Value {
    serde_json::json!({
        "run": encode_run(&report.run),
        "scope": report.scope,
        "dod": report.dod,
        "provider": report.provider,
        "result": report.result,
        "attempts": report
            .attempts
            .iter()
            .map(|attempt| serde_json::json!({
                "attempt": attempt.attempt,
                "outcome": attempt.outcome,
                "retry_trigger": attempt.retry_trigger,
                "started_at": attempt.started_at,
                "ended_at": attempt.ended_at,
            }))
            .collect::<Vec<_>>(),
        "questions": report.questions.iter().map(encode_question).collect::<Vec<_>>(),
        "findings": report
            .findings
            .iter()
            .map(|finding| serde_json::json!({
                "description": finding.description,
                "evidence_class": finding.evidence_class,
                "causal_disposition": finding.causal_disposition,
                "created_at": finding.created_at,
            }))
            .collect::<Vec<_>>(),
        "health": report.health.as_ref().map(|health| serde_json::json!({
            "noop_turns": health.noop_turns,
            "last_progress_turn": health.last_progress_turn,
            "tokens_since_progress": health.tokens_since_progress,
        })),
    })
}

fn encode_run(run: &TeamRun) -> Value {
    serde_json::json!({
        "run_id": run.run_id,
        "task": run.task,
        "state": run.state,
        "priority": run.priority,
        "worktree_status": run.worktree_status,
        "parent_run_id": run.parent_run_id,
        "created_at": run.created_at,
    })
}

fn encode_question(question: &TeamQuestion) -> Value {
    serde_json::json!({
        "question_id": question.question_id,
        "run_id": question.run_id,
        "kind": question.kind,
        "blocked_decision": question.blocked_decision,
        "options": question
            .options
            .iter()
            .map(|option| serde_json::json!({
                "id": option.id(),
                "label": option.label(),
                "consequence": option.consequence(),
            }))
            .collect::<Vec<_>>(),
        "recommendation": question.recommendation,
        "expires_at": question.expires_at,
    })
}

fn encode_answer(receipt: AnswerReceipt) -> Value {
    serde_json::json!({
        "status": "answered",
        "question_id": receipt.question_id,
        "run_id": receipt.run_id,
        "run_resumed": receipt.run_resumed,
    })
}

fn encode_spawn(receipt: &SpawnReceipt) -> Value {
    serde_json::json!({
        "status": "proposed",
        "run_id": receipt.run_id,
        "state": receipt.state,
    })
}

fn encode_run_state(receipt: &RunStateReceipt, moved_status: &str) -> Value {
    serde_json::json!({
        "status": if receipt.moved { moved_status } else { "unchanged" },
        "run_id": receipt.run_id,
        "state": receipt.state,
    })
}

fn encode_merge_request(receipt: &MergeRequestReceipt) -> Value {
    serde_json::json!({
        "status": "authorization_requested",
        "question_id": receipt.question_id,
        "run_id": receipt.run_id,
        "tree_hash": receipt.tree_hash,
        "paths_digest": receipt.paths_digest,
    })
}

fn encode_reclaim(receipt: &ReclaimReceipt) -> Value {
    serde_json::json!({
        "status": if receipt.moved { "released" } else { "unchanged" },
        "run_id": receipt.run_id,
        "worktree_status": receipt.worktree_status,
    })
}
