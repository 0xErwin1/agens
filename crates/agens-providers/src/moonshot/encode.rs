//! Building a Moonshot chat-completions request body.
//!
//! Moonshot speaks the chat-completions dialect, where the conversation is
//! replayed in full on every call and tool results are ordinary messages. That
//! is the whole reason this encoder exists separately from the responses-API one
//! next to it, which continues a server-held thread by id instead.

use agens_core::{Message, MessagePart, ReasoningEffort, Role};
use serde_json::{Value, json};

use crate::OpenAiFunctionTool;

use super::compat;

/// Everything a request body depends on beyond the conversation itself.
pub(super) struct RequestOptions<'a> {
    pub model: &'a str,
    pub tools: &'a [OpenAiFunctionTool],
    pub parallel_tool_calls: bool,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// The tool shape Moonshot expects.
///
/// `strict` is omitted deliberately. The live API accepts it, but nothing in
/// this harness depends on constrained decoding, and an omitted key cannot be
/// rejected by a future tightening of what values it takes.
fn tool_json(tool: &OpenAiFunctionTool) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name(),
            "description": tool.description(),
            "parameters": tool.parameters(),
        }
    })
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Encodes one message into the zero or more chat-completions messages it maps
/// onto. A single assistant turn carrying both text and tool calls is one
/// message, while its tool results are separate `tool` messages, so the counts
/// do not line up one to one.
fn encode_message(message: &Message, encoded: &mut Vec<Value>) {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();

    for part in &message.parts {
        match part {
            MessagePart::Text(value) => text.push_str(value),
            MessagePart::Reasoning(value) => reasoning.push_str(value),
            MessagePart::ToolCall { id, name, input } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": input },
            })),
            MessagePart::ToolResult {
                tool_call_id,
                content,
                ..
            } => encoded.push(json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": content,
            })),
        }
    }

    if message.role == Role::Tool {
        return;
    }

    if text.is_empty() && reasoning.is_empty() && tool_calls.is_empty() {
        return;
    }

    let mut encoded_message = json!({ "role": role_name(message.role) });
    let object = encoded_message
        .as_object_mut()
        .expect("a json! object literal is an object");

    if tool_calls.is_empty() {
        object.insert("content".to_owned(), Value::String(text));
    } else {
        // An assistant message that only calls tools still needs the key, and
        // Moonshot rejects an empty string where it accepts an explicit null.
        object.insert(
            "content".to_owned(),
            if text.is_empty() {
                Value::Null
            } else {
                Value::String(text)
            },
        );
        object.insert("tool_calls".to_owned(), Value::Array(tool_calls));
    }

    encoded.push(encoded_message);
}

pub(super) fn encode_messages(messages: &[Message]) -> Vec<Value> {
    let mut encoded = Vec::new();
    for message in messages {
        encode_message(message, &mut encoded);
    }
    encoded
}

/// The full request body for one streaming call.
///
/// Usage only arrives when `stream_options.include_usage` asks for it, so the
/// key is always present rather than conditional on anything.
pub(super) fn encode_request(messages: &[Message], options: &RequestOptions<'_>) -> Value {
    let mut request = json!({
        "model": options.model,
        "messages": encode_messages(messages),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    let object = request
        .as_object_mut()
        .expect("a json! object literal is an object");

    if !options.tools.is_empty() {
        object.insert(
            "tools".to_owned(),
            Value::Array(options.tools.iter().map(tool_json).collect()),
        );
        object.insert("tool_choice".to_owned(), Value::String("auto".to_owned()));
        object.insert(
            "parallel_tool_calls".to_owned(),
            Value::Bool(options.parallel_tool_calls),
        );
    }

    if let Some(effort) = compat::reasoning_effort(options.model, options.reasoning_effort) {
        object.insert(
            "reasoning_effort".to_owned(),
            Value::String(effort.to_owned()),
        );
    }

    request
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> OpenAiFunctionTool {
        OpenAiFunctionTool::new(
            "get_weather",
            "Get the weather",
            json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        )
        .expect("fixture tool is valid")
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text(text.to_owned())],
        }
    }

    fn options<'a>(model: &'a str, tools: &'a [OpenAiFunctionTool]) -> RequestOptions<'a> {
        RequestOptions {
            model,
            tools,
            parallel_tool_calls: true,
            reasoning_effort: None,
        }
    }

    #[test]
    fn a_request_always_asks_for_usage_in_the_stream() {
        let request = encode_request(&[user("hello")], &options("kimi-k3", &[]));

        assert_eq!(request["stream"], json!(true));
        assert_eq!(request["stream_options"]["include_usage"], json!(true));
    }

    #[test]
    fn a_request_carries_no_output_cap_and_no_openai_only_keys() {
        let request = encode_request(&[user("hello")], &options("kimi-k3", &[]));
        let object = request.as_object().expect("request is an object");

        for absent in ["max_tokens", "max_completion_tokens", "store", "thinking"] {
            assert!(!object.contains_key(absent), "{absent} must not be sent");
        }
    }

    #[test]
    fn tools_are_sent_without_a_strict_flag() {
        let tools = [tool()];
        let request = encode_request(&[user("weather?")], &options("kimi-k3", &tools));

        let function = &request["tools"][0]["function"];
        assert_eq!(function["name"], json!("get_weather"));
        assert!(
            function.get("strict").is_none(),
            "strict must not be sent in any form"
        );
        assert_eq!(request["tool_choice"], json!("auto"));
    }

    #[test]
    fn parallel_tool_calls_follows_the_configured_value() {
        let tools = [tool()];

        let mut enabled = options("kimi-k3", &tools);
        enabled.parallel_tool_calls = true;
        assert_eq!(
            encode_request(&[user("weather?")], &enabled)["parallel_tool_calls"],
            json!(true)
        );

        let mut disabled = options("kimi-k3", &tools);
        disabled.parallel_tool_calls = false;
        assert_eq!(
            encode_request(&[user("weather?")], &disabled)["parallel_tool_calls"],
            json!(false)
        );
    }

    #[test]
    fn a_toolless_request_omits_every_tool_related_key() {
        let request = encode_request(&[user("hello")], &options("kimi-k3", &[]));
        let object = request.as_object().expect("request is an object");

        for absent in ["tools", "tool_choice", "parallel_tool_calls"] {
            assert!(!object.contains_key(absent), "{absent} must not be sent");
        }
    }

    #[test]
    fn reasoning_effort_reaches_only_the_model_that_accepts_it() {
        let mut with_effort = options("kimi-k3", &[]);
        with_effort.reasoning_effort = Some(ReasoningEffort::Max);
        assert_eq!(
            encode_request(&[user("hello")], &with_effort)["reasoning_effort"],
            json!("max")
        );

        let mut other_model = options("kimi-k2.6", &[]);
        other_model.reasoning_effort = Some(ReasoningEffort::Max);
        let request = encode_request(&[user("hello")], &other_model);
        assert!(
            request
                .as_object()
                .expect("object")
                .get("reasoning_effort")
                .is_none(),
            "kimi-k2.6 exposes no effort knob"
        );
    }

    #[test]
    fn a_system_prompt_uses_the_system_role() {
        let messages = [
            Message {
                role: Role::System,
                parts: vec![MessagePart::Text("be brief".to_owned())],
            },
            user("hello"),
        ];

        let encoded = encode_messages(&messages);

        assert_eq!(encoded[0]["role"], json!("system"));
        assert_eq!(encoded[0]["content"], json!("be brief"));
    }

    #[test]
    fn an_assistant_tool_call_replays_with_its_results_as_separate_messages() {
        let messages = [
            user("weather in Paris and Tokyo?"),
            Message {
                role: Role::Assistant,
                parts: vec![
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
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![
                    MessagePart::ToolResult {
                        tool_call_id: "call_0".to_owned(),
                        content: "sunny".to_owned(),
                        is_error: false,
                    },
                    MessagePart::ToolResult {
                        tool_call_id: "call_1".to_owned(),
                        content: "rainy".to_owned(),
                        is_error: false,
                    },
                ],
            },
        ];

        let encoded = encode_messages(&messages);

        assert_eq!(encoded.len(), 4);
        assert_eq!(encoded[1]["role"], json!("assistant"));
        assert_eq!(encoded[1]["content"], Value::Null);
        assert_eq!(encoded[1]["tool_calls"].as_array().expect("array").len(), 2);
        assert_eq!(encoded[1]["tool_calls"][0]["id"], json!("call_0"));
        assert_eq!(
            encoded[1]["tool_calls"][1]["function"]["arguments"],
            json!(r#"{"city":"Tokyo"}"#)
        );

        assert_eq!(encoded[2]["role"], json!("tool"));
        assert_eq!(encoded[2]["tool_call_id"], json!("call_0"));
        assert_eq!(encoded[2]["content"], json!("sunny"));
        assert_eq!(encoded[3]["tool_call_id"], json!("call_1"));
    }

    #[test]
    fn assistant_text_survives_alongside_its_tool_calls() {
        let messages = [Message {
            role: Role::Assistant,
            parts: vec![
                MessagePart::Text("checking".to_owned()),
                MessagePart::ToolCall {
                    id: "call_0".to_owned(),
                    name: "get_weather".to_owned(),
                    input: "{}".to_owned(),
                },
            ],
        }];

        let encoded = encode_messages(&messages);

        assert_eq!(encoded[0]["content"], json!("checking"));
        assert_eq!(encoded[0]["tool_calls"].as_array().expect("array").len(), 1);
    }
}
