//! Building a Moonshot chat-completions request body.
//!
//! Moonshot speaks the chat-completions dialect, where the conversation is
//! replayed in full on every call and tool results are ordinary messages. That
//! is the whole reason this encoder exists separately from the responses-API one
//! next to it, which continues a server-held thread by id instead.

use std::collections::{BTreeMap, HashMap};

use agens_core::{Message, MessagePart, ReasoningEffort, Role};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::{Value, json};

use crate::OpenAiFunctionTool;

use super::compat;

/// Durable media blobs keyed by `media_id` for chat-completions image_url encoding.
pub(super) type MediaBlobs = BTreeMap<i64, Vec<u8>>;

/// Failures while mapping domain messages onto the chat-completions wire shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EncodeError {
    MediaUnavailable { media_id: i64 },
    UnsupportedMediaMime { mime: String },
}

/// Everything a request body depends on beyond the conversation itself.
pub(super) struct RequestOptions<'a> {
    pub model: &'a str,
    pub tools: &'a [OpenAiFunctionTool],
    pub parallel_tool_calls: bool,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Chat-completions history is not valid for the wire (tool_calls adjacency).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HistoryValidationError;

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

/// Collects `tool_call_id → function name` from every ToolCall part so tool
/// result messages can carry the `name` field Moonshot expects.
fn tool_call_names(messages: &[Message]) -> HashMap<String, String> {
    let mut names = HashMap::new();

    for message in messages {
        for part in &message.parts {
            if let MessagePart::ToolCall { id, name, .. } = part {
                names.insert(id.clone(), name.clone());
            }
        }
    }

    names
}

fn media_data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", BASE64_STANDARD.encode(bytes))
}

fn moonshot_image_content_item(
    media_id: i64,
    mime: &str,
    media_blobs: &MediaBlobs,
) -> Result<Value, EncodeError> {
    let Some(bytes) = media_blobs.get(&media_id) else {
        return Err(EncodeError::MediaUnavailable { media_id });
    };

    if !mime.starts_with("image/") {
        return Err(EncodeError::UnsupportedMediaMime {
            mime: mime.to_owned(),
        });
    }

    Ok(json!({
        "type": "image_url",
        "image_url": { "url": media_data_url(mime, bytes) },
    }))
}

/// Encodes one message into the zero or more chat-completions messages it maps
/// onto. A single assistant turn carrying both text and tool calls is one
/// message, while its tool results are separate `tool` messages, so the counts
/// do not line up one to one.
fn encode_message(
    message: &Message,
    tool_names: &HashMap<String, String>,
    media_blobs: &MediaBlobs,
    encoded: &mut Vec<Value>,
) -> Result<(), EncodeError> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut content_items = Vec::new();
    let mut has_media = false;

    for part in &message.parts {
        match part {
            MessagePart::Text(value) => {
                text.push_str(value);
                if !value.is_empty() {
                    content_items.push(json!({ "type": "text", "text": value }));
                }
            }
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
            } => {
                let mut tool_message = json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                });

                if let Some(name) = tool_names.get(tool_call_id) {
                    tool_message
                        .as_object_mut()
                        .expect("a json! object literal is an object")
                        .insert("name".to_owned(), Value::String(name.clone()));
                }

                encoded.push(tool_message);
            }
            MessagePart::Media { media_id, mime } => {
                has_media = true;
                content_items.push(moonshot_image_content_item(*media_id, mime, media_blobs)?);
            }
        }
    }

    if message.role == Role::Tool {
        return Ok(());
    }

    if text.is_empty() && reasoning.is_empty() && tool_calls.is_empty() && !has_media {
        return Ok(());
    }

    let mut encoded_message = json!({ "role": role_name(message.role) });
    let object = encoded_message
        .as_object_mut()
        .expect("a json! object literal is an object");

    if tool_calls.is_empty() {
        if has_media {
            object.insert("content".to_owned(), Value::Array(content_items));
        } else {
            object.insert("content".to_owned(), Value::String(text));
        }
    } else {
        // An assistant message that only calls tools still needs the key, and
        // Moonshot rejects an empty string where it accepts an explicit null.
        // Media is user-only in the domain; tool_calls stay on the string/null shape.
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
    Ok(())
}

pub(super) fn encode_messages(
    messages: &[Message],
    media_blobs: &MediaBlobs,
) -> Result<Vec<Value>, EncodeError> {
    let tool_names = tool_call_names(messages);
    let mut encoded = Vec::new();

    for message in messages {
        encode_message(message, &tool_names, media_blobs, &mut encoded)?;
    }

    Ok(encoded)
}

/// Chat-completions requires every assistant `tool_calls` batch to be followed
/// immediately by `role=tool` messages covering exactly those `tool_call_id`s
/// before any other role appears.
pub(super) fn validate_chat_completions_history(
    messages: &[Message],
    media_blobs: &MediaBlobs,
) -> Result<(), HistoryValidationError> {
    let encoded = encode_messages(messages, media_blobs).map_err(|_| HistoryValidationError)?;
    validate_encoded_tool_call_adjacency(&encoded)
}

fn validate_encoded_tool_call_adjacency(encoded: &[Value]) -> Result<(), HistoryValidationError> {
    let mut index = 0;

    while index < encoded.len() {
        let message = &encoded[index];
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");

        if role == "assistant"
            && let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array)
            && !tool_calls.is_empty()
        {
            let expected_ids: Vec<&str> = tool_calls
                .iter()
                .map(|call| {
                    call.get("id")
                        .and_then(Value::as_str)
                        .ok_or(HistoryValidationError)
                })
                .collect::<Result<_, _>>()?;

            let mut covered = Vec::with_capacity(expected_ids.len());
            let mut cursor = index + 1;

            while cursor < encoded.len()
                && encoded[cursor].get("role").and_then(Value::as_str) == Some("tool")
            {
                let tool_call_id = encoded[cursor]
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .ok_or(HistoryValidationError)?;

                if !expected_ids.contains(&tool_call_id) || covered.contains(&tool_call_id) {
                    return Err(HistoryValidationError);
                }

                covered.push(tool_call_id);
                cursor += 1;
            }

            if covered.len() != expected_ids.len() {
                return Err(HistoryValidationError);
            }

            index = cursor;
            continue;
        }

        index += 1;
    }

    Ok(())
}

/// The full request body for one streaming call.
///
/// Usage only arrives when `stream_options.include_usage` asks for it, so the
/// key is always present rather than conditional on anything.
pub(super) fn encode_request(
    messages: &[Message],
    options: &RequestOptions<'_>,
    media_blobs: &MediaBlobs,
) -> Result<Value, EncodeError> {
    let mut request = json!({
        "model": options.model,
        "messages": encode_messages(messages, media_blobs)?,
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

    Ok(request)
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

    fn empty_media() -> MediaBlobs {
        MediaBlobs::new()
    }

    #[test]
    fn a_request_always_asks_for_usage_in_the_stream() {
        let request =
            encode_request(&[user("hello")], &options("kimi-k3", &[]), &empty_media()).unwrap();

        assert_eq!(request["stream"], json!(true));
        assert_eq!(request["stream_options"]["include_usage"], json!(true));
    }

    #[test]
    fn a_request_carries_no_output_cap_and_no_openai_only_keys() {
        let request =
            encode_request(&[user("hello")], &options("kimi-k3", &[]), &empty_media()).unwrap();
        let object = request.as_object().expect("request is an object");

        for absent in ["max_tokens", "max_completion_tokens", "store", "thinking"] {
            assert!(!object.contains_key(absent), "{absent} must not be sent");
        }
    }

    #[test]
    fn tools_are_sent_without_a_strict_flag() {
        let tools = [tool()];
        let request = encode_request(
            &[user("weather?")],
            &options("kimi-k3", &tools),
            &empty_media(),
        )
        .unwrap();

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
            encode_request(&[user("weather?")], &enabled, &empty_media()).unwrap()["parallel_tool_calls"],
            json!(true)
        );

        let mut disabled = options("kimi-k3", &tools);
        disabled.parallel_tool_calls = false;
        assert_eq!(
            encode_request(&[user("weather?")], &disabled, &empty_media()).unwrap()["parallel_tool_calls"],
            json!(false)
        );
    }

    #[test]
    fn a_toolless_request_omits_every_tool_related_key() {
        let request =
            encode_request(&[user("hello")], &options("kimi-k3", &[]), &empty_media()).unwrap();
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
            encode_request(&[user("hello")], &with_effort, &empty_media()).unwrap()["reasoning_effort"],
            json!("max")
        );

        let mut other_model = options("kimi-k2.6", &[]);
        other_model.reasoning_effort = Some(ReasoningEffort::Max);
        let request = encode_request(&[user("hello")], &other_model, &empty_media()).unwrap();
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

        let encoded = encode_messages(&messages, &BTreeMap::new()).expect("encode");

        assert_eq!(encoded[0]["role"], json!("system"));
        assert_eq!(encoded[0]["content"], json!("be brief"));
    }

    #[test]
    fn multimodal_user_content_becomes_a_text_and_image_url_array() {
        let messages = [Message {
            role: Role::User,
            parts: vec![
                MessagePart::Text("what is this?".to_owned()),
                MessagePart::Media {
                    media_id: 11,
                    mime: "image/png".to_owned(),
                },
            ],
        }];
        let mut media = BTreeMap::new();
        media.insert(11, b"fake-png-bytes".to_vec());

        let encoded = encode_messages(&messages, &media).expect("multimodal encode");

        assert_eq!(encoded[0]["role"], json!("user"));
        let content = encoded[0]["content"].as_array().expect("content array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0], json!({"type": "text", "text": "what is this?"}));
        assert_eq!(content[1]["type"], json!("image_url"));
        assert_eq!(
            content[1]["image_url"]["url"],
            json!("data:image/png;base64,ZmFrZS1wbmctYnl0ZXM=")
        );
    }

    #[test]
    fn media_only_user_message_is_a_single_image_url_item() {
        let messages = [Message {
            role: Role::User,
            parts: vec![MessagePart::Media {
                media_id: 4,
                mime: "image/jpeg".to_owned(),
            }],
        }];
        let mut media = BTreeMap::new();
        media.insert(4, b"jpeg-bytes".to_vec());

        let encoded = encode_messages(&messages, &media).expect("media-only encode");

        let content = encoded[0]["content"].as_array().expect("content array");
        assert_eq!(content.len(), 1);
        assert_eq!(
            content[0]["image_url"]["url"],
            json!("data:image/jpeg;base64,anBlZy1ieXRlcw==")
        );
    }

    #[test]
    fn text_only_user_content_remains_a_plain_string() {
        let encoded = encode_messages(&[user("hello")], &BTreeMap::new()).expect("text encode");
        assert_eq!(encoded[0]["content"], json!("hello"));
        assert!(encoded[0]["content"].is_string());
    }

    #[test]
    fn missing_media_blob_fails_encode() {
        let messages = [Message {
            role: Role::User,
            parts: vec![MessagePart::Media {
                media_id: 99,
                mime: "image/png".to_owned(),
            }],
        }];

        assert_eq!(
            encode_messages(&messages, &BTreeMap::new()),
            Err(EncodeError::MediaUnavailable { media_id: 99 })
        );
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

        let encoded = encode_messages(&messages, &empty_media()).expect("encode");

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
        assert_eq!(encoded[2]["name"], json!("get_weather"));
        assert_eq!(encoded[2]["content"], json!("sunny"));
        assert_eq!(encoded[3]["tool_call_id"], json!("call_1"));
        assert_eq!(encoded[3]["name"], json!("get_weather"));
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

        let encoded = encode_messages(&messages, &empty_media()).expect("encode");

        assert_eq!(encoded[0]["content"], json!("checking"));
        assert_eq!(encoded[0]["tool_calls"].as_array().expect("array").len(), 1);
    }

    #[test]
    fn unpaired_tool_calls_fail_history_validation() {
        let messages = [
            user("weather?"),
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::ToolCall {
                    id: "call_0".to_owned(),
                    name: "get_weather".to_owned(),
                    input: r#"{"city":"Paris"}"#.to_owned(),
                }],
            },
        ];

        assert_eq!(
            validate_chat_completions_history(&messages, &empty_media()),
            Err(HistoryValidationError)
        );
    }

    #[test]
    fn a_user_message_between_tool_calls_and_results_fails_validation() {
        let messages = [
            user("weather?"),
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::ToolCall {
                    id: "call_0".to_owned(),
                    name: "get_weather".to_owned(),
                    input: r#"{"city":"Paris"}"#.to_owned(),
                }],
            },
            user("coordination"),
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "call_0".to_owned(),
                    content: "sunny".to_owned(),
                    is_error: false,
                }],
            },
        ];

        assert_eq!(
            validate_chat_completions_history(&messages, &empty_media()),
            Err(HistoryValidationError)
        );
    }

    #[test]
    fn a_complete_multi_tool_batch_validates_and_names_tool_messages() {
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
            user("thanks"),
        ];

        assert_eq!(
            validate_chat_completions_history(&messages, &empty_media()),
            Ok(())
        );

        let encoded = encode_messages(&messages, &empty_media()).expect("encode");
        assert_eq!(encoded[2]["role"], json!("tool"));
        assert_eq!(encoded[2]["name"], json!("get_weather"));
        assert_eq!(encoded[2]["tool_call_id"], json!("call_0"));
        assert_eq!(encoded[3]["name"], json!("get_weather"));
        assert_eq!(encoded[3]["tool_call_id"], json!("call_1"));
        assert_eq!(encoded[4]["role"], json!("user"));
        assert_eq!(encoded[4]["content"], json!("thanks"));
    }
}
