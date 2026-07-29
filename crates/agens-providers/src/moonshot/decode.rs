//! Turning a `chat.completion.chunk` stream into the parts of one assistant
//! turn.
//!
//! Three properties of this wire format drive the shape here, and all three were
//! observed against the live API rather than inferred:
//!
//! - A tool call is identified by its position in the array. Its id and name
//!   arrive only in the frame that opens it, and its arguments arrive as string
//!   fragments across later frames.
//! - Token usage is reported twice, in different places, and the final report
//!   arrives in a frame carrying no choices at all.
//! - Reasoning is a field of its own rather than part of the content.

use std::collections::BTreeMap;

use agens_core::{HeadlessTurnPortError, MessagePart, Usage};
use serde_json::Value;

/// A tool call being assembled across frames. Its arguments stay a string: the
/// fragments are only valid JSON once concatenated, and the harness passes the
/// text through to the tool rather than interpreting it.
#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub(super) struct CompletionsDecoder {
    text: String,
    reasoning: String,
    calls: Vec<PendingCall>,
    by_index: BTreeMap<u64, usize>,
    by_id: BTreeMap<String, usize>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

/// The field names Moonshot and its neighbours have used for reasoning text, in
/// the order they are preferred. Some endpoints send more than one with the same
/// content, so the first non-empty match wins and the rest are ignored.
const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

impl CompletionsDecoder {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Feeds one decoded `data:` payload. `[DONE]` is handled by the caller,
    /// which owns the framing.
    pub(super) fn accept(&mut self, chunk: &Value) -> Result<(), HeadlessTurnPortError> {
        if let Some(usage) = chunk.get("usage").and_then(parse_usage) {
            self.usage = Some(usage);
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };

        if let Some(usage) = choice.get("usage").and_then(parse_usage) {
            self.usage = Some(usage);
        }

        if let Some(delta) = choice.get("delta") {
            self.accept_delta(delta);
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_owned());
        }

        Ok(())
    }

    fn accept_delta(&mut self, delta: &Value) {
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            self.text.push_str(content);
        }

        if let Some(reasoning) = REASONING_FIELDS
            .iter()
            .find_map(|field| delta.get(*field).and_then(Value::as_str))
            .filter(|reasoning| !reasoning.is_empty())
        {
            self.reasoning.push_str(reasoning);
        }

        let calls = delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for call in &calls {
            self.accept_tool_call(call);
        }
    }

    /// Locates the call this fragment belongs to by index, falling back to id.
    /// Either alone is insufficient: a continuation frame carries only the
    /// index, while nothing in the format promises an index is always present.
    fn accept_tool_call(&mut self, call: &Value) {
        let index = call.get("index").and_then(Value::as_u64);
        let id = call.get("id").and_then(Value::as_str).unwrap_or_default();

        let existing = index
            .and_then(|index| self.by_index.get(&index).copied())
            .or_else(|| {
                (!id.is_empty())
                    .then(|| self.by_id.get(id).copied())
                    .flatten()
            });

        let position = match existing {
            Some(position) => position,
            None => {
                let position = self.calls.len();
                self.calls.push(PendingCall::default());
                if let Some(index) = index {
                    self.by_index.insert(index, position);
                }
                position
            }
        };

        let entry = &mut self.calls[position];

        if !id.is_empty() && entry.id.is_empty() {
            entry.id = id.to_owned();
            self.by_id.insert(id.to_owned(), position);
        }

        let Some(function) = call.get("function") else {
            return;
        };

        if let Some(name) = function.get("name").and_then(Value::as_str)
            && !name.is_empty()
            && entry.name.is_empty()
        {
            entry.name = name.to_owned();
        }

        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
            entry.arguments.push_str(arguments);
        }
    }

    pub(super) fn usage(&self) -> Option<Usage> {
        self.usage.clone()
    }

    pub(super) fn wants_tool_results(&self) -> bool {
        self.finish_reason.as_deref() == Some("tool_calls")
    }

    /// Consumes the decoder into the parts of the turn.
    ///
    /// A stream that ends without a finish reason is an error rather than a
    /// short answer: a truncated response and a complete one are otherwise
    /// indistinguishable, and treating one as the other would silently discard
    /// whatever the model had left to say.
    pub(super) fn finish(self) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
        let Some(finish_reason) = self.finish_reason else {
            return Err(HeadlessTurnPortError::ProviderProtocol);
        };

        let calls_expected = finish_reason == "tool_calls";
        if calls_expected && self.calls.is_empty() {
            return Err(HeadlessTurnPortError::ProviderProtocol);
        }
        if !calls_expected && !self.calls.is_empty() {
            return Err(HeadlessTurnPortError::ProviderProtocol);
        }

        let mut parts = Vec::new();

        if !self.reasoning.is_empty() {
            parts.push(MessagePart::Reasoning(self.reasoning));
        }
        if !self.text.is_empty() {
            parts.push(MessagePart::Text(self.text));
        }

        for call in self.calls {
            if call.id.is_empty() || call.name.is_empty() {
                return Err(HeadlessTurnPortError::ProviderProtocol);
            }
            parts.push(MessagePart::ToolCall {
                id: call.id,
                name: call.name,
                input: call.arguments,
            });
        }

        Ok(parts)
    }
}

fn parse_usage(usage: &Value) -> Option<Usage> {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);

    let input_tokens = field("prompt_tokens");
    let output_tokens = field("completion_tokens");
    let total_tokens = field("total_tokens");

    (input_tokens.is_some() || output_tokens.is_some() || total_tokens.is_some()).then_some(Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        context_window: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode(chunks: &[Value]) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
        let mut decoder = CompletionsDecoder::new();
        for chunk in chunks {
            decoder.accept(chunk)?;
        }
        decoder.finish()
    }

    fn delta(delta: Value, finish_reason: Value) -> Value {
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
    }

    #[test]
    fn content_fragments_join_into_one_text_part() {
        let parts = decode(&[
            delta(json!({"role": "assistant", "content": ""}), Value::Null),
            delta(json!({"content": "he"}), Value::Null),
            delta(json!({"content": "llo"}), Value::Null),
            delta(json!({}), json!("stop")),
        ])
        .expect("stream decodes");

        assert_eq!(parts, vec![MessagePart::Text("hello".to_owned())]);
    }

    #[test]
    fn reasoning_precedes_the_answer_it_produced() {
        let parts = decode(&[
            delta(json!({"reasoning_content": "think"}), Value::Null),
            delta(json!({"content": "answer"}), Value::Null),
            delta(json!({}), json!("stop")),
        ])
        .expect("stream decodes");

        assert_eq!(
            parts,
            vec![
                MessagePart::Reasoning("think".to_owned()),
                MessagePart::Text("answer".to_owned()),
            ]
        );
    }

    #[test]
    fn a_repeated_reasoning_field_is_counted_once() {
        let parts = decode(&[
            delta(
                json!({"reasoning_content": "once", "reasoning": "once"}),
                Value::Null,
            ),
            delta(json!({}), json!("stop")),
        ])
        .expect("stream decodes");

        assert_eq!(parts, vec![MessagePart::Reasoning("once".to_owned())]);
    }

    #[test]
    fn an_alternative_reasoning_field_still_decodes() {
        let parts = decode(&[
            delta(json!({"reasoning": "alternative"}), Value::Null),
            delta(json!({}), json!("stop")),
        ])
        .expect("stream decodes");

        assert_eq!(
            parts,
            vec![MessagePart::Reasoning("alternative".to_owned())]
        );
    }

    #[test]
    fn two_tool_calls_stay_distinct_while_their_arguments_stream() {
        let parts = decode(&[
            delta(
                json!({"tool_calls": [{"index": 0, "id": "call_0", "type": "function",
                    "function": {"name": "get_weather", "arguments": ""}}]}),
                Value::Null,
            ),
            delta(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"city\":\""}}]}),
                Value::Null,
            ),
            delta(
                json!({"tool_calls": [{"index": 1, "id": "call_1", "type": "function",
                    "function": {"name": "get_weather", "arguments": ""}}]}),
                Value::Null,
            ),
            delta(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "Paris\"}"}}]}),
                Value::Null,
            ),
            delta(
                json!({"tool_calls": [{"index": 1, "function": {"arguments": "{\"city\":\"Tokyo\"}"}}]}),
                Value::Null,
            ),
            delta(json!({}), json!("tool_calls")),
        ])
        .expect("stream decodes");

        assert_eq!(
            parts,
            vec![
                MessagePart::ToolCall {
                    id: "call_0".to_owned(),
                    name: "get_weather".to_owned(),
                    input: r#"{"city":"Paris"}"#.to_owned(),
                },
                MessagePart::ToolCall {
                    id: "call_1".to_owned(),
                    name: "get_weather".to_owned(),
                    input: r#"{"city":"Tokyo"}"#.to_owned(),
                },
            ]
        );
    }

    #[test]
    fn a_call_identified_only_by_id_still_accumulates() {
        let parts = decode(&[
            delta(
                json!({"tool_calls": [{"id": "call_0", "function": {"name": "run", "arguments": "{"}}]}),
                Value::Null,
            ),
            delta(
                json!({"tool_calls": [{"id": "call_0", "function": {"arguments": "}"}}]}),
                Value::Null,
            ),
            delta(json!({}), json!("tool_calls")),
        ])
        .expect("stream decodes");

        assert_eq!(
            parts,
            vec![MessagePart::ToolCall {
                id: "call_0".to_owned(),
                name: "run".to_owned(),
                input: "{}".to_owned(),
            }]
        );
    }

    #[test]
    fn usage_is_read_from_the_choice_that_reports_it() {
        let mut decoder = CompletionsDecoder::new();
        decoder
            .accept(
                &json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop",
                "usage": {"prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14}}]}),
            )
            .expect("chunk is accepted");

        assert_eq!(
            decoder.usage(),
            Some(Usage {
                input_tokens: Some(11),
                output_tokens: Some(3),
                total_tokens: Some(14),
                context_window: None,
            })
        );
    }

    #[test]
    fn usage_is_read_from_the_trailing_frame_that_carries_no_choices() {
        let mut decoder = CompletionsDecoder::new();
        decoder
            .accept(&json!({"choices": [], "usage": {"prompt_tokens": 7,
                "completion_tokens": 2, "total_tokens": 9}}))
            .expect("chunk is accepted");

        assert_eq!(
            decoder.usage().and_then(|usage| usage.total_tokens),
            Some(9)
        );
    }

    #[test]
    fn the_last_usage_report_wins_when_both_places_carry_one() {
        let mut decoder = CompletionsDecoder::new();
        decoder
            .accept(
                &json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop",
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}]}),
            )
            .expect("chunk is accepted");
        decoder
            .accept(&json!({"choices": [], "usage": {"prompt_tokens": 10,
                "completion_tokens": 10, "total_tokens": 20}}))
            .expect("chunk is accepted");

        assert_eq!(
            decoder.usage().and_then(|usage| usage.total_tokens),
            Some(20)
        );
    }

    #[test]
    fn a_stream_that_stops_without_a_finish_reason_is_an_error() {
        let error = decode(&[delta(json!({"content": "partial"}), Value::Null)])
            .expect_err("a truncated stream must not decode as a complete turn");

        assert_eq!(error, HeadlessTurnPortError::ProviderProtocol);
    }

    #[test]
    fn reported_tool_calls_that_never_arrived_are_an_error() {
        let error = decode(&[delta(json!({}), json!("tool_calls"))])
            .expect_err("a tool-call turn with no calls must not decode");

        assert_eq!(error, HeadlessTurnPortError::ProviderProtocol);
    }

    #[test]
    fn tool_calls_that_were_never_reported_are_an_error() {
        let error = decode(&[
            delta(
                json!({"tool_calls": [{"index": 0, "id": "call_0",
                    "function": {"name": "run", "arguments": "{}"}}]}),
                Value::Null,
            ),
            delta(json!({}), json!("stop")),
        ])
        .expect_err("calls under a stop reason must not decode");

        assert_eq!(error, HeadlessTurnPortError::ProviderProtocol);
    }

    #[test]
    fn a_tool_call_missing_its_name_is_an_error() {
        let error = decode(&[
            delta(
                json!({"tool_calls": [{"index": 0, "id": "call_0", "function": {"arguments": "{}"}}]}),
                Value::Null,
            ),
            delta(json!({}), json!("tool_calls")),
        ])
        .expect_err("an unnamed tool call must not decode");

        assert_eq!(error, HeadlessTurnPortError::ProviderProtocol);
    }

    #[test]
    fn a_tool_call_turn_reports_that_it_wants_results() {
        let mut decoder = CompletionsDecoder::new();
        decoder
            .accept(&delta(json!({}), json!("tool_calls")))
            .expect("chunk is accepted");
        assert!(decoder.wants_tool_results());

        let mut stopped = CompletionsDecoder::new();
        stopped
            .accept(&delta(json!({}), json!("stop")))
            .expect("chunk is accepted");
        assert!(!stopped.wants_tool_results());
    }
}
