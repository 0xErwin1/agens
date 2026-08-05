use agens_core::ask_user::{
    AskUserAnswer, AskUserMode, AskUserOption, AskUserPort, AskUserQuestion, AskUserReply,
    AskUserRequest, AskUserUnavailable, MAX_ASK_USER_CONTEXT_CHARS, MAX_ASK_USER_EXPLANATION_CHARS,
    MAX_ASK_USER_ID_CHARS, MAX_ASK_USER_LABEL_CHARS, MAX_ASK_USER_OPTIONS,
    MAX_ASK_USER_PROMPT_CHARS, MAX_ASK_USER_QUESTIONS, MAX_ASK_USER_TITLE_CHARS,
};
use agens_core::{Error, HeadlessTurnCancellation};
use serde_json::{Map, Value};

use crate::{
    DispatchTool, ToolExecutionContext, ToolExecutionStatus, ToolOutput, sanitized_execution_status,
};

/// The provider-visible native tool that opens a bounded structured prompt on
/// whichever interactive surface `port` is bound to.
///
/// Like [`crate::SkillResourceTool`], this tool owns its own
/// `input_schema`/`permission_target`/`execute` rather than going through
/// `NativeToolCatalog`, because it has no project-confined filesystem
/// surface to dispatch into.
pub struct AskUserTool {
    port: Box<dyn AskUserPort>,
}

impl AskUserTool {
    pub fn new(port: Box<dyn AskUserPort>) -> Self {
        Self { port }
    }

    pub fn input_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["questions"],
            "properties": {
                "title": {"type": "string", "minLength": 1, "maxLength": MAX_ASK_USER_TITLE_CHARS},
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_ASK_USER_QUESTIONS,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["id", "prompt", "mode", "options"],
                        "properties": {
                            "id": {"type": "string", "minLength": 1, "maxLength": MAX_ASK_USER_ID_CHARS},
                            "prompt": {"type": "string", "minLength": 1, "maxLength": MAX_ASK_USER_PROMPT_CHARS},
                            "explanation": {"type": "string", "maxLength": MAX_ASK_USER_EXPLANATION_CHARS},
                            "mode": {"type": "string", "enum": ["single", "multiple"]},
                            "allow_other": {"type": "boolean"},
                            "allow_note": {"type": "boolean"},
                            "allow_discuss": {"type": "boolean"},
                            "options": {
                                "type": "array",
                                "minItems": 1,
                                "maxItems": MAX_ASK_USER_OPTIONS,
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["id", "label"],
                                    "properties": {
                                        "id": {"type": "string", "minLength": 1, "maxLength": MAX_ASK_USER_ID_CHARS},
                                        "label": {"type": "string", "minLength": 1, "maxLength": MAX_ASK_USER_LABEL_CHARS},
                                        "explanation": {"type": "string", "maxLength": MAX_ASK_USER_EXPLANATION_CHARS},
                                        "context": {"type": "string", "maxLength": MAX_ASK_USER_CONTEXT_CHARS}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })
    }
}

impl DispatchTool for AskUserTool {
    /// The question set is not a permission-bearing target: there is nothing
    /// a rule could usefully match on except the tool name itself, so this
    /// mirrors `RegisteredMcpTool`'s constant target rather than projecting
    /// a field out of the arguments.
    fn permission_target(&self, _arguments: &Value) -> Result<String, Error> {
        Ok("ask_user".to_string())
    }

    /// Asking a person something is the one tool call that is not on the
    /// clock.
    ///
    /// Every other tool inherits this context's deadline, and a context built
    /// from a turn that set none inherits the bash fallback instead — which is
    /// how a question a reader was still reading used to be answered
    /// "expired" after two minutes. This call therefore reads only the
    /// cancellation half of the context, before and after the wait: a
    /// cancelled turn still ends the question, an elapsed deadline never does.
    fn execute(
        &mut self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, Error> {
        if context.is_cancelled() {
            return Ok(sanitized_execution_status(ToolExecutionStatus::Cancelled));
        }

        let request = parse_request(&arguments)
            .map_err(|()| Error::Tool("ask_user arguments are invalid".into()))?;

        let cancellation = HeadlessTurnCancellation::with_cancellation_and_deadline(
            context.cancellation_handle(),
            None,
        );

        let reply = self.port.ask(&request, &cancellation);

        if request.validate_reply(&reply).is_err() {
            return Ok(ToolOutput::failure("ask_user: reply is invalid"));
        }

        if context.is_cancelled() {
            return Ok(sanitized_execution_status(ToolExecutionStatus::Cancelled));
        }

        Ok(ToolOutput::success(encode_reply(&request, &reply)))
    }
}

fn parse_request(arguments: &Value) -> Result<AskUserRequest, ()> {
    let object = object_with_only(arguments, &["title", "questions"]).ok_or(())?;

    let title = optional_str(object, "title")?;

    let questions_array = object
        .get("questions")
        .and_then(Value::as_array)
        .ok_or(())?;

    let mut questions = Vec::with_capacity(questions_array.len());
    for question_value in questions_array {
        questions.push(parse_question(question_value)?);
    }

    AskUserRequest::new(title, questions).map_err(|_| ())
}

fn parse_question(value: &Value) -> Result<AskUserQuestion, ()> {
    let object = object_with_only(
        value,
        &[
            "id",
            "prompt",
            "explanation",
            "mode",
            "allow_other",
            "allow_note",
            "allow_discuss",
            "options",
        ],
    )
    .ok_or(())?;

    let id = required_str(object, "id")?;
    let prompt = required_str(object, "prompt")?;
    let explanation = optional_str(object, "explanation")?;

    let mode = match object.get("mode").and_then(Value::as_str) {
        Some("single") => AskUserMode::Single,
        Some("multiple") => AskUserMode::Multiple,
        _ => return Err(()),
    };

    let allow_other = optional_bool(object, "allow_other")?;
    let allow_note = optional_bool(object, "allow_note")?;
    let allow_discuss = optional_bool(object, "allow_discuss")?;

    let options_array = object.get("options").and_then(Value::as_array).ok_or(())?;

    let mut options = Vec::with_capacity(options_array.len());
    for option_value in options_array {
        options.push(parse_option(option_value)?);
    }

    Ok(AskUserQuestion::new(
        id,
        prompt,
        explanation,
        mode,
        options,
        allow_other,
        allow_note,
        allow_discuss,
    ))
}

fn parse_option(value: &Value) -> Result<AskUserOption, ()> {
    let object = object_with_only(value, &["id", "label", "explanation", "context"]).ok_or(())?;

    let id = required_str(object, "id")?;
    let label = required_str(object, "label")?;
    let explanation = optional_str(object, "explanation")?;
    let context = optional_str(object, "context")?;

    Ok(AskUserOption::new(id, label, explanation, context))
}

fn object_with_only<'a>(value: &'a Value, allowed: &[&str]) -> Option<&'a Map<String, Value>> {
    let object = value.as_object()?;

    if object.keys().all(|key| allowed.contains(&key.as_str())) {
        Some(object)
    } else {
        None
    }
}

fn required_str(object: &Map<String, Value>, key: &str) -> Result<String, ()> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(())
}

fn optional_str(object: &Map<String, Value>, key: &str) -> Result<Option<String>, ()> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(()),
    }
}

fn optional_bool(object: &Map<String, Value>, key: &str) -> Result<bool, ()> {
    match object.get(key) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(()),
    }
}

/// Encodes a reply into the tool's deterministic, fixed-key-order JSON
/// envelope.
///
/// Object key order is not something `serde_json::Value` preserves in this
/// workspace (there is no `preserve_order` feature enabled, so a `Value`
/// object serializes its keys alphabetically). The envelope is therefore
/// built as a plain string with explicit key order, using `serde_json` only
/// to escape individual scalar and array values.
fn encode_reply(request: &AskUserRequest, reply: &AskUserReply) -> String {
    match reply {
        AskUserReply::Answered(answers) => encode_answered(request, answers),
        AskUserReply::Discuss { question_id, note } => encode_discuss(question_id, note.as_deref()),
        AskUserReply::Cancelled => "{\"status\":\"cancelled\"}".to_string(),
        AskUserReply::Unavailable(reason) => encode_unavailable(*reason),
    }
}

fn encode_answered(request: &AskUserRequest, answers: &[AskUserAnswer]) -> String {
    let entries: Vec<String> = request
        .questions()
        .iter()
        .zip(answers.iter())
        .map(|(question, answer)| encode_answer(question, answer))
        .collect();

    format!(
        "{{\"status\":\"answered\",\"answers\":[{}]}}",
        entries.join(",")
    )
}

/// `selected` is re-projected into the question's declared option order
/// rather than echoed in whatever order the answer arrived in, so the model
/// always sees a stable shape regardless of toggle order on the surface.
///
/// `question_id` is sourced from `question.id()` rather than
/// `answer.question_id`, for the same reason: the envelope's determinism
/// should not depend on what the reply happened to carry. The two values are
/// only ever equal today because `validate_answered_reply` enforces that
/// equality before this function is reached, but this function no longer
/// relies on that upstream guarantee to produce a trustworthy id.
///
/// Key order is fixed: `question_id`, `answered`, `selected`, `other`, `note`.
/// `answered` is false when the user left the question with no selection and
/// no free-text other (a deliberate skip); a note alone does not count as an
/// answer.
fn encode_answer(question: &AskUserQuestion, answer: &AskUserAnswer) -> String {
    let selected: Vec<&str> = question
        .options()
        .iter()
        .map(AskUserOption::id)
        .filter(|option_id| {
            answer
                .selected
                .iter()
                .any(|selected_id| selected_id == option_id)
        })
        .collect();

    let answered = !selected.is_empty()
        || matches!(answer.other.as_deref(), Some(other) if !other.trim().is_empty());

    format!(
        "{{\"question_id\":{},\"answered\":{},\"selected\":{},\"other\":{},\"note\":{}}}",
        json_string(question.id()),
        if answered { "true" } else { "false" },
        json_string_array(&selected),
        json_optional_string(answer.other.as_deref()),
        json_optional_string(answer.note.as_deref()),
    )
}

fn encode_discuss(question_id: &str, note: Option<&str>) -> String {
    format!(
        "{{\"status\":\"discuss\",\"question_id\":{},\"note\":{}}}",
        json_string(question_id),
        json_optional_string(note),
    )
}

fn encode_unavailable(reason: AskUserUnavailable) -> String {
    let reason = match reason {
        AskUserUnavailable::NoInteractiveSurface => "no interactive surface",
        AskUserUnavailable::SurfaceClosed => "interactive surface closed",
    };

    format!(
        "{{\"status\":\"unavailable\",\"reason\":{}}}",
        json_string(reason)
    )
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string always serializes to valid JSON")
}

fn json_string_array(values: &[&str]) -> String {
    serde_json::to_string(values).expect("a string array always serializes to valid JSON")
}

fn json_optional_string(value: Option<&str>) -> String {
    match value {
        Some(value) => json_string(value),
        None => "null".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{AskUserAnswer, AskUserMode, AskUserOption, AskUserQuestion, encode_answer};

    /// Bypasses the domain-validated `AskUserRequest::new` constructor on
    /// purpose: some of these bytes (a NUL, an ESC, a bare newline) are
    /// control characters that current validation already refuses on every
    /// path that reaches this encoder. That refusal is a property of
    /// today's validation layer, not of the encoder. This test pins the
    /// encoder's own correctness independently, so a future edit that
    /// starts interpolating a value directly into the envelope string
    /// cannot silently reintroduce JSON injection even if validation rules
    /// change or are bypassed elsewhere.
    #[test]
    fn encode_answer_escapes_hostile_strings_without_breaking_json_structure() {
        let hostile_values = [
            "double quote: \"",
            "backslash: \\",
            "newline: \n",
            "nul: \0",
            "esc: \u{1b}",
            "line separator: \u{2028}",
            "CJK: 日本語",
            "emoji: 🎉",
            "injection payload: \",\"status\":\"answered\",\"injected\":\"",
        ];

        for hostile in hostile_values {
            let option_a = AskUserOption::new("opt-a", "Option A", None, None);
            let option_b = AskUserOption::new(hostile, "Option B", None, None);
            let question = AskUserQuestion::new(
                hostile,
                "prompt",
                None,
                AskUserMode::Multiple,
                vec![option_a.clone(), option_b.clone()],
                true,
                true,
                false,
            );
            let answer = AskUserAnswer {
                question_id: hostile.to_string(),
                selected: vec![option_a.id().to_string(), option_b.id().to_string()],
                other: Some(hostile.to_string()),
                note: Some(hostile.to_string()),
            };

            let encoded = encode_answer(&question, &answer);
            let parsed: serde_json::Value =
                serde_json::from_str(&encoded).unwrap_or_else(|error| {
                    panic!(
                        "hostile input {hostile:?} produced invalid JSON: {error}\nraw: {encoded}"
                    )
                });

            let object = parsed.as_object().expect("answer is a JSON object");
            let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                ["answered", "note", "other", "question_id", "selected"],
                "hostile input {hostile:?} changed the answer object's key set"
            );
            assert_eq!(
                object["answered"],
                serde_json::Value::Bool(true),
                "hostile input {hostile:?} should still mark the answer as answered"
            );

            assert_eq!(
                object["question_id"],
                serde_json::Value::String(question.id().to_string())
            );
            assert_eq!(
                object["other"],
                serde_json::Value::String(hostile.to_string())
            );
            assert_eq!(
                object["note"],
                serde_json::Value::String(hostile.to_string())
            );
            assert_eq!(
                object["selected"],
                serde_json::json!([option_a.id(), hostile]),
                "hostile input {hostile:?} did not round-trip through the selected array"
            );
        }
    }

    /// Exercises the printable subset of the hostile set (no control
    /// characters, since those are rejected by `AskUserRequest::new` before
    /// this envelope-level function is reached) end to end, to prove a
    /// hostile value cannot escape its own JSON string and clobber the
    /// envelope's `status` key or inject a sibling key.
    #[test]
    fn encode_reply_never_lets_hostile_content_clobber_the_status_key() {
        use super::{AskUserReply, AskUserRequest, encode_reply};

        let injection = "\",\"status\":\"answered\",\"injected\":\"malicious";
        let option = AskUserOption::new("opt", "Option", None, None);
        let question = AskUserQuestion::new(
            injection,
            "prompt",
            None,
            AskUserMode::Single,
            vec![option.clone()],
            true,
            true,
            false,
        );
        let request =
            AskUserRequest::new(None, vec![question]).expect("printable hostile request is valid");

        let answer = AskUserAnswer {
            question_id: injection.to_string(),
            selected: vec![option.id().to_string()],
            other: Some(injection.to_string()),
            note: Some(injection.to_string()),
        };
        let reply = AskUserReply::Answered(vec![answer]);

        let encoded = encode_reply(&request, &reply);
        let parsed: serde_json::Value =
            serde_json::from_str(&encoded).expect("hostile envelope is still valid JSON");

        let object = parsed.as_object().expect("envelope is a JSON object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["answers", "status"],
            "hostile input injected a sibling top-level key"
        );
        assert_eq!(
            object["status"],
            serde_json::Value::String("answered".to_string())
        );
    }
}
