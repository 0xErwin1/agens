use agens_core::{Error, Message, MessagePart, Role};
use agens_providers::{decode_openai_response_events, encode_openai_response_request};

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
fn decodes_text_reasoning_and_function_call_parts_in_arrival_order() {
    let events = [
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
            "OpenAI stream failed: upstream rejected the request".to_owned()
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
            "OpenAI stream failed: upstream rejected the request".to_owned()
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
