use agens_core::{Error, Message, MessagePart, Role};
use agens_providers::{
    OpenAiFunctionTool, decode_openai_response_events, encode_openai_response_request,
    encode_openai_response_request_with_messages, encode_openai_response_request_with_tools,
};
use serde_json::json;

#[test]
fn encodes_a_text_user_prompt_as_a_streaming_responses_request() {
    let request = encode_openai_response_request(
        "gpt-5.6",
        &Message {
            role: Role::User,
            parts: vec![MessagePart::Text("Where is Paris?".to_owned())],
        },
    )
    .expect("text prompt should encode");

    assert_eq!(
        request,
        r#"{"input":[{"content":"Where is Paris?","role":"user"}],"model":"gpt-5.6","stream":true}"#
    );
}

#[test]
fn rejects_request_parts_that_have_no_wire_mapping_in_this_slice() {
    let request = encode_openai_response_request(
        "gpt-5.6",
        &Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Reasoning("hidden work".to_owned())],
        },
    );

    assert_eq!(
        request,
        Err(Error::Provider(
            "OpenAI request error: only a single text part is supported".to_owned()
        ))
    );
}

#[test]
fn encodes_validated_function_tools_as_flat_responses_api_definitions() {
    let tool = OpenAiFunctionTool::new(
        "lookup_weather",
        "Looks up the current weather for a city.",
        json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
            "additionalProperties": false,
        }),
    )
    .expect("a complete function tool should be valid");

    let request = encode_openai_response_request_with_tools(
        "gpt-5.6",
        &Message {
            role: Role::User,
            parts: vec![MessagePart::Text(
                "What is the weather in Paris?".to_owned(),
            )],
        },
        &[tool],
    )
    .expect("tool-enabled request should encode");

    assert_eq!(
        request,
        r#"{"input":[{"content":"What is the weather in Paris?","role":"user"}],"model":"gpt-5.6","stream":true,"tools":[{"description":"Looks up the current weather for a city.","name":"lookup_weather","parameters":{"additionalProperties":false,"properties":{"city":{"type":"string"}},"required":["city"],"type":"object"},"strict":true,"type":"function"}]}"#
    );
}

#[test]
fn encodes_resumed_messages_in_order_with_mixed_parts() {
    let messages = vec![
        Message {
            role: Role::System,
            parts: vec![MessagePart::Text(
                "Follow the active agent instructions.".to_owned(),
            )],
        },
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("What is the weather?".to_owned())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![
                MessagePart::Text("I will look it up.".to_owned()),
                MessagePart::Reasoning("Use the weather tool.".to_owned()),
                MessagePart::ToolCall {
                    id: "call_weather".to_owned(),
                    name: "lookup_weather".to_owned(),
                    input: r#"{"city":"Paris"}"#.to_owned(),
                },
            ],
        },
        Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: "call_weather".to_owned(),
                content: "sunny".to_owned(),
                is_error: false,
            }],
        },
    ];

    let request = encode_openai_response_request_with_messages("gpt-5.6", &messages, &[])
        .expect("resumed history should encode");

    assert_eq!(
        request,
        r#"{"input":[{"content":[{"text":"Follow the active agent instructions.","type":"input_text"}],"role":"system"},{"content":[{"text":"What is the weather?","type":"input_text"}],"role":"user"},{"content":[{"text":"I will look it up.","type":"output_text"}],"role":"assistant"},{"summary":[{"text":"Use the weather tool.","type":"summary_text"}],"type":"reasoning"},{"arguments":"{\"city\":\"Paris\"}","call_id":"call_weather","name":"lookup_weather","type":"function_call"},{"call_id":"call_weather","output":"sunny","type":"function_call_output"}],"model":"gpt-5.6","stream":true}"#
    );
}

#[test]
fn rejects_invalid_resumed_message_history() {
    let unmatched_result = vec![Message {
        role: Role::Tool,
        parts: vec![MessagePart::ToolResult {
            tool_call_id: "missing".to_owned(),
            content: "no matching call".to_owned(),
            is_error: true,
        }],
    }];

    assert_eq!(
        encode_openai_response_request_with_messages("gpt-5.6", &unmatched_result, &[]),
        Err(Error::Provider(
            "OpenAI request error: resumed history is invalid".to_owned()
        ))
    );

    let malformed_tool_call = vec![Message {
        role: Role::Assistant,
        parts: vec![MessagePart::ToolCall {
            id: "call_weather".to_owned(),
            name: "lookup_weather".to_owned(),
            input: "not json".to_owned(),
        }],
    }];

    assert_eq!(
        encode_openai_response_request_with_messages("gpt-5.6", &malformed_tool_call, &[]),
        Err(Error::Provider(
            "OpenAI request error: resumed history is invalid".to_owned()
        ))
    );
}

#[test]
fn function_tools_require_a_nonempty_name_description_and_object_root_schema() {
    for (name, description, parameters) in [
        ("", "Looks up weather.", json!({"type": "object"})),
        ("lookup", "", json!({"type": "object"})),
        ("lookup", "Looks up weather.", json!({})),
        (
            "lookup",
            "Looks up weather.",
            json!({"type": "array", "items": {}}),
        ),
    ] {
        assert_eq!(
            OpenAiFunctionTool::new(name, description, parameters),
            Err(Error::Provider(
                "OpenAI request error: function tools require a name, description, and object parameters"
                    .to_owned()
            ))
        );
    }
}

#[test]
fn rejects_tool_calls_without_a_response_id_or_with_a_reused_call_id() {
    let missing_response_id = [
        r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_123","call_id":"call_123","name":"lookup","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.done","item_id":"fc_123","arguments":"{}"}"#,
        r#"{"type":"response.completed"}"#,
    ];
    let reused_call_id = [
        r#"{"type":"response.created","response":{"id":"resp_123"}}"#,
        r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_123","call_id":"call_123","name":"lookup","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.done","item_id":"fc_123","arguments":"{}"}"#,
        r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_456","call_id":"call_123","name":"lookup","arguments":""}}"#,
    ];

    assert_eq!(
        decode_openai_response_events(missing_response_id),
        Err(Error::Provider(
            "OpenAI stream protocol error: tool calls require a response ID".to_owned()
        ))
    );
    assert_eq!(
        decode_openai_response_events(reused_call_id),
        Err(Error::Provider(
            "OpenAI stream protocol error: duplicate function call ID".to_owned()
        ))
    );
}

#[test]
fn rejects_empty_response_ids_and_function_identity_fields() {
    let empty_response_id = [
        r#"{"type":"response.created","response":{"id":""}}"#,
        r#"{"type":"response.completed","response":{"id":""}}"#,
    ];
    let empty_function_id = [
        r#"{"type":"response.created","response":{"id":"resp_123"}}"#,
        r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"","call_id":"call_123","name":"lookup","arguments":""}}"#,
    ];

    assert_eq!(
        decode_openai_response_events(empty_response_id),
        Err(Error::Provider(
            "OpenAI stream protocol error: event is missing a required non-empty string field"
                .to_owned()
        ))
    );
    assert_eq!(
        decode_openai_response_events(empty_function_id),
        Err(Error::Provider(
            "OpenAI stream protocol error: event is missing a required non-empty string field"
                .to_owned()
        ))
    );
}

#[test]
fn rejects_a_reused_function_item_id_after_its_first_call_completes() {
    let events = [
        r#"{"type":"response.created","response":{"id":"resp_123"}}"#,
        r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_123","call_id":"call_123","name":"lookup","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.done","item_id":"fc_123","arguments":"{}"}"#,
        r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_123","call_id":"call_456","name":"lookup","arguments":""}}"#,
    ];

    assert_eq!(
        decode_openai_response_events(events),
        Err(Error::Provider(
            "OpenAI stream protocol error: duplicate function call item".to_owned()
        ))
    );
}

#[test]
fn decodes_text_reasoning_and_function_call_parts_in_arrival_order() {
    let events = [
        r#"{"type":"response.created","response":{"id":"resp_123"}}"#,
        r#"{"type":"response.output_text.delta","delta":"Hello"}"#,
        r#"{"type":"response.output_item.done","item":{"type":"reasoning","summary":[{"type":"summary_text","text":"Plan first"}]}}"#,
        r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_123","call_id":"call_123","name":"lookup","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_123","delta":"{\"city\":"}"#,
        r#"{"type":"response.function_call_arguments.done","item_id":"fc_123","name":"lookup","arguments":"{\"city\":\"Paris\"}"}"#,
        r#"{"type":"response.completed"}"#,
    ];

    let parts = decode_openai_response_events(events).expect("stream should decode");

    assert_eq!(
        parts,
        vec![
            MessagePart::Text("Hello".to_owned()),
            MessagePart::Reasoning("Plan first".to_owned()),
            MessagePart::ToolCall {
                id: "call_123".to_owned(),
                name: "lookup".to_owned(),
                input: "{\"city\":\"Paris\"}".to_owned(),
            },
        ]
    );
}

#[test]
fn reports_protocol_failures_without_returning_partial_parts() {
    let upstream_error = [r#"{"type":"error","message":"upstream rejected the request"}"#];
    let malformed_event = [r#"{"type":"response.output_text.delta","delta":"missing quote}"#];

    assert_eq!(
        decode_openai_response_events(upstream_error),
        Err(Error::Provider(
            "OpenAI stream failed: upstream provider reported an error".to_owned()
        ))
    );
    assert_eq!(
        decode_openai_response_events(malformed_event),
        Err(Error::Provider(
            "OpenAI stream protocol error: invalid event JSON".to_owned()
        ))
    );
}

#[test]
fn rejects_failed_or_incomplete_streams_without_returning_partial_parts() {
    let failed_stream = [
        r#"{"type":"response.output_text.delta","delta":"partial"}"#,
        r#"{"type":"response.failed","response":{"error":{"message":"upstream rejected the request"}}}"#,
    ];
    let truncated_stream = [r#"{"type":"response.output_text.delta","delta":"partial"}"#];
    let incomplete_function_call = [
        r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_123","call_id":"call_123","name":"lookup","arguments":""}}"#,
        r#"{"type":"response.completed"}"#,
    ];

    assert_eq!(
        decode_openai_response_events(failed_stream),
        Err(Error::Provider(
            "OpenAI stream failed: upstream provider reported an error".to_owned()
        ))
    );
    assert_eq!(
        decode_openai_response_events(truncated_stream),
        Err(Error::Provider(
            "OpenAI stream protocol error: stream ended before response.completed".to_owned()
        ))
    );
    assert_eq!(
        decode_openai_response_events(incomplete_function_call),
        Err(Error::Provider(
            "OpenAI stream protocol error: stream completed with unfinished function calls"
                .to_owned()
        ))
    );
}
