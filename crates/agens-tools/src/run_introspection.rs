//! The two tools a worker reports its own progress through.
//!
//! Both take a typed payload and hand it to a
//! [`agens_core::run_introspection::RunIntrospectionPort`], which is what makes
//! them first-class rather than prose a later stage parses back into fields.
//! The tool's job is to reject a malformed payload before anything downstream
//! sees it, and to say what was recorded.
//!
//! Like [`crate::AskUserTool`] and [`crate::SkillResourceTool`], they own their
//! own schema, permission target and execution instead of going through
//! `NativeToolCatalog`: neither has a project-confined filesystem surface to
//! dispatch into, and both are bound to the run the session is executing rather
//! than to a directory.
//!
//! `ask` is the session's own, never a delegated child's. A subagent runs
//! inside its parent's attempt and reports through the execution that launched
//! it, so a child that could park the run would suspend a session it does not
//! own, on a question the thread that delegated to it never asked. It is kept
//! out the same way `cd` and `worktree` are: by not being on the surface a
//! child is resolved against, so no declaration can name it and nothing
//! registers it.

use agens_core::Error;
use agens_core::run_introspection::{
    Ask, AskError, AskOption, AskReceipt, CausalDisposition, Checkpoint, CheckpointError,
    CheckpointReceipt, EvidenceClaim, EvidenceClass, MAX_ASK_DECISION_CHARS,
    MAX_ASK_OPTION_ID_CHARS, MAX_ASK_OPTION_LABEL_CHARS, MAX_ASK_OPTIONS,
    MAX_ASK_RECOMMENDATION_CHARS, MAX_BLOCKER_CHARS, MAX_CHECKPOINT_BLOCKERS,
    MAX_CHECKPOINT_CLAIMS, MAX_CHECKPOINT_GOAL_CHARS, MAX_CHECKPOINT_HYPOTHESIS_CHARS,
    MAX_CHECKPOINT_TOUCHED_PATHS, MAX_CLAIM_DESCRIPTION_CHARS, MAX_CLAIM_PROOF_REFS,
    MAX_PROOF_REF_CHARS, MAX_TOUCHED_PATH_CHARS, RunIntrospectionPort,
};
use serde_json::{Map, Value};

use crate::{
    DispatchTool, ToolExecutionContext, ToolExecutionStatus, ToolOutput, sanitized_execution_status,
};

/// Reports a milestone: what was established since the last one, where the work
/// is going, and when the next report is due.
pub struct CheckpointTool {
    port: Box<dyn RunIntrospectionPort>,
}

impl CheckpointTool {
    pub fn new(port: Box<dyn RunIntrospectionPort>) -> Self {
        Self { port }
    }

    #[must_use]
    pub fn input_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["next_goal"],
            "properties": {
                "evidence": {
                    "type": "array",
                    "maxItems": MAX_CHECKPOINT_CLAIMS,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["description", "evidence_class"],
                        "properties": {
                            "description": {"type": "string", "minLength": 1, "maxLength": MAX_CLAIM_DESCRIPTION_CHARS},
                            "evidence_class": {"type": "string", "enum": ["deterministic", "inferential", "insufficient"]},
                            "proof_refs": {
                                "type": "array",
                                "maxItems": MAX_CLAIM_PROOF_REFS,
                                "items": {"type": "string", "minLength": 1, "maxLength": MAX_PROOF_REF_CHARS}
                            },
                            "disposition": {"type": "string", "enum": ["candidate_caused", "pre_existing", "unknown"]}
                        }
                    }
                },
                "hypothesis": {"type": "string", "minLength": 1, "maxLength": MAX_CHECKPOINT_HYPOTHESIS_CHARS},
                "next_goal": {"type": "string", "minLength": 1, "maxLength": MAX_CHECKPOINT_GOAL_CHARS},
                "revised_estimate_seconds": {"type": "integer", "minimum": 0},
                "blockers": {
                    "type": "array",
                    "maxItems": MAX_CHECKPOINT_BLOCKERS,
                    "items": {"type": "string", "minLength": 1, "maxLength": MAX_BLOCKER_CHARS}
                },
                "next_checkpoint_at": {"type": "integer", "minimum": 0},
                "touched_paths": {
                    "type": "array",
                    "maxItems": MAX_CHECKPOINT_TOUCHED_PATHS,
                    "items": {"type": "string", "minLength": 1, "maxLength": MAX_TOUCHED_PATH_CHARS}
                }
            }
        })
    }
}

impl DispatchTool for CheckpointTool {
    /// Nothing in a checkpoint is a permission-bearing target: a rule could
    /// only usefully match the tool name, so this mirrors [`crate::AskUserTool`]
    /// rather than projecting a field out of the arguments.
    fn permission_target(&self, _arguments: &Value) -> Result<String, Error> {
        Ok("checkpoint".to_owned())
    }

    fn execute(
        &mut self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, Error> {
        if context.is_cancelled() {
            return Ok(sanitized_execution_status(ToolExecutionStatus::Cancelled));
        }

        let checkpoint = match parse_checkpoint(&arguments) {
            Ok(checkpoint) => checkpoint,
            Err(reason) => {
                return Ok(ToolOutput::failure(format!("checkpoint: {reason}")));
            }
        };

        match self.port.checkpoint(&checkpoint) {
            Ok(receipt) => Ok(ToolOutput::success(encode_checkpoint(&receipt))),
            Err(error) => Ok(ToolOutput::failure(format!("checkpoint: {error}"))),
        }
    }
}

/// Raises the one question a worker has: it records the blocked decision and
/// parks the run on it.
pub struct AskTool {
    port: Box<dyn RunIntrospectionPort>,
}

impl AskTool {
    pub fn new(port: Box<dyn RunIntrospectionPort>) -> Self {
        Self { port }
    }

    #[must_use]
    pub fn input_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["blocked_decision", "options"],
            "properties": {
                "blocked_decision": {"type": "string", "minLength": 1, "maxLength": MAX_ASK_DECISION_CHARS},
                "options": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_ASK_OPTIONS,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["id", "label"],
                        "properties": {
                            "id": {"type": "string", "minLength": 1, "maxLength": MAX_ASK_OPTION_ID_CHARS},
                            "label": {"type": "string", "minLength": 1, "maxLength": MAX_ASK_OPTION_LABEL_CHARS},
                            "consequence": {"type": "string", "minLength": 1, "maxLength": MAX_ASK_OPTION_LABEL_CHARS}
                        }
                    }
                },
                "recommendation": {"type": "string", "minLength": 1, "maxLength": MAX_ASK_RECOMMENDATION_CHARS}
            }
        })
    }
}

impl DispatchTool for AskTool {
    fn permission_target(&self, _arguments: &Value) -> Result<String, Error> {
        Ok("ask".to_owned())
    }

    /// The call returns as soon as the question is durable. It does not wait
    /// for an answer: the session is suspended once the turn ends, and the
    /// answer reaches the resumed session through the safe-point queue, so
    /// blocking here would hold a provider handle across a human-scale wait.
    fn execute(
        &mut self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, Error> {
        if context.is_cancelled() {
            return Ok(sanitized_execution_status(ToolExecutionStatus::Cancelled));
        }

        let ask = match parse_ask(&arguments) {
            Ok(ask) => ask,
            Err(reason) => return Ok(ToolOutput::failure(format!("ask: {reason}"))),
        };

        match self.port.ask(&ask) {
            Ok(receipt) => Ok(ToolOutput::success(encode_ask(&receipt))),
            Err(error) => Ok(ToolOutput::failure(format!("ask: {error}"))),
        }
    }
}

fn encode_checkpoint(receipt: &CheckpointReceipt) -> String {
    serde_json::json!({
        "status": "recorded",
        "checkpoint_id": receipt.checkpoint_event_id,
        "finding_ids": receipt.finding_ids,
        "credited_progress": receipt.credited_progress,
    })
    .to_string()
}

fn encode_ask(receipt: &AskReceipt) -> String {
    serde_json::json!({
        "status": "asked",
        "question_id": receipt.question_id,
        "run_id": receipt.run_id,
        "run_state": "awaiting_input",
    })
    .to_string()
}

fn parse_checkpoint(arguments: &Value) -> Result<Checkpoint, String> {
    let object = object_with_only(
        arguments,
        &[
            "evidence",
            "hypothesis",
            "next_goal",
            "revised_estimate_seconds",
            "blockers",
            "next_checkpoint_at",
            "touched_paths",
        ],
    )
    .ok_or_else(|| "arguments are invalid".to_owned())?;

    let claims = match object.get("evidence") {
        None => Vec::new(),
        Some(Value::Array(values)) => values
            .iter()
            .map(parse_claim)
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err("evidence must be an array".to_owned()),
    };

    Checkpoint::new(
        claims,
        optional_string(object, "hypothesis")?,
        required_string(object, "next_goal")?,
        optional_integer(object, "revised_estimate_seconds")?,
        string_array(object, "blockers")?,
        optional_integer(object, "next_checkpoint_at")?,
        string_array(object, "touched_paths")?,
    )
    .map_err(describe_checkpoint_error)
}

fn parse_claim(value: &Value) -> Result<EvidenceClaim, String> {
    let object = object_with_only(
        value,
        &["description", "evidence_class", "proof_refs", "disposition"],
    )
    .ok_or_else(|| "an evidence entry is invalid".to_owned())?;

    let class = EvidenceClass::parse(
        object
            .get("evidence_class")
            .and_then(Value::as_str)
            .ok_or_else(|| "evidence_class is required".to_owned())?,
    )
    .ok_or_else(|| {
        "evidence_class must be deterministic, inferential or insufficient".to_owned()
    })?;

    let disposition = match object.get("disposition") {
        None => CausalDisposition::default(),
        Some(Value::String(text)) => CausalDisposition::parse(text).ok_or_else(|| {
            "disposition must be candidate_caused, pre_existing or unknown".to_owned()
        })?,
        Some(_) => return Err("disposition must be a string".to_owned()),
    };

    Ok(EvidenceClaim::new(
        required_string(object, "description")?,
        string_array(object, "proof_refs")?,
        class,
        disposition,
    ))
}

fn parse_ask(arguments: &Value) -> Result<Ask, String> {
    let object = object_with_only(
        arguments,
        &["blocked_decision", "options", "recommendation"],
    )
    .ok_or_else(|| "arguments are invalid".to_owned())?;

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

fn parse_option(value: &Value) -> Result<AskOption, String> {
    let object = object_with_only(value, &["id", "label", "consequence"])
        .ok_or_else(|| "an option is invalid".to_owned())?;

    Ok(AskOption::new(
        required_string(object, "id")?,
        required_string(object, "label")?,
        optional_string(object, "consequence")?,
    ))
}

fn object_with_only<'a>(value: &'a Value, allowed: &[&str]) -> Option<&'a Map<String, Value>> {
    let object = value.as_object()?;

    object
        .keys()
        .all(|key| allowed.contains(&key.as_str()))
        .then_some(object)
}

fn required_string(object: &Map<String, Value>, key: &'static str) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{key} is required and must be a string"))
}

fn optional_string(
    object: &Map<String, Value>,
    key: &'static str,
) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn optional_integer(object: &Map<String, Value>, key: &'static str) -> Result<Option<i64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("{key} must be a whole number")),
    }
}

fn string_array(object: &Map<String, Value>, key: &'static str) -> Result<Vec<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("every {key} entry must be a string"))
            })
            .collect(),
        Some(_) => Err(format!("{key} must be an array of strings")),
    }
}

/// The domain's rejection, said in the terms the worker used.
///
/// The deterministic-proof arm is spelled out rather than named, because it is
/// the one refusal a worker is expected to act on by reclassifying its own
/// claim instead of by resending the same payload.
fn describe_checkpoint_error(error: CheckpointError) -> String {
    match error {
        CheckpointError::NoNextGoal => "next_goal is required".to_owned(),
        CheckpointError::TooManyClaims => {
            format!("at most {MAX_CHECKPOINT_CLAIMS} evidence entries")
        }
        CheckpointError::TooManyProofRefs => {
            format!("at most {MAX_CLAIM_PROOF_REFS} proof references per claim")
        }
        CheckpointError::TooManyBlockers => {
            format!("at most {MAX_CHECKPOINT_BLOCKERS} blockers")
        }
        CheckpointError::TooManyTouchedPaths => {
            format!("at most {MAX_CHECKPOINT_TOUCHED_PATHS} touched paths")
        }
        CheckpointError::DeterministicClaimWithoutProof => {
            "a deterministic claim needs at least one proof reference a reader can re-run; \
             report it as inferential or insufficient instead"
                .to_owned()
        }
        CheckpointError::EmptyField(field) => format!("{field} cannot be empty"),
        CheckpointError::FieldTooLong(field) => format!("{field} is too long"),
        CheckpointError::ControlCharacter(field) => {
            format!("{field} contains a control character")
        }
        CheckpointError::NegativeEstimate => {
            "revised_estimate_seconds cannot be negative".to_owned()
        }
        CheckpointError::CheckpointTooLarge => "the checkpoint is too large".to_owned(),
    }
}

fn describe_ask_error(error: AskError) -> String {
    match error {
        AskError::NoBlockedDecision => "blocked_decision is required".to_owned(),
        AskError::NoOptions => "a question needs the options it is choosing between".to_owned(),
        AskError::TooManyOptions => format!("at most {MAX_ASK_OPTIONS} options"),
        AskError::DuplicateOptionId => "two options share an id".to_owned(),
        AskError::UnknownRecommendation => "recommendation must name one of the options".to_owned(),
        AskError::EmptyField(field) => format!("{field} cannot be empty"),
        AskError::FieldTooLong(field) => format!("{field} is too long"),
        AskError::ControlCharacter(field) => format!("{field} contains a control character"),
        AskError::AskTooLarge => "the question is too large".to_owned(),
    }
}
