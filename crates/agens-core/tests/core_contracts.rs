use agens_core::{Error, ErrorCategory, Message, MessagePart, Role, TurnState};

#[test]
fn message_preserves_each_closed_part_payload() {
    let message = Message {
        role: Role::Assistant,
        parts: vec![
            MessagePart::Text("answer".into()),
            MessagePart::Reasoning("considering options".into()),
            MessagePart::ToolCall {
                id: "call-1".into(),
                name: "search".into(),
                input: "{\"query\":\"agens\"}".into(),
            },
            MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "result".into(),
                is_error: false,
            },
        ],
    };

    assert_eq!(message.role, Role::Assistant);
    assert_eq!(message.parts.len(), 4);
    assert_eq!(
        message.parts[2],
        MessagePart::ToolCall {
            id: "call-1".into(),
            name: "search".into(),
            input: "{\"query\":\"agens\"}".into(),
        }
    );
}

#[test]
fn terminal_turn_states_are_distinct_from_active_states() {
    assert!(TurnState::Completed.is_terminal());
    assert!(TurnState::Cancelled.is_terminal());
    assert!(TurnState::Failed.is_terminal());
    assert!(!TurnState::Requesting.is_terminal());
    assert!(!TurnState::Streaming.is_terminal());
    assert!(!TurnState::Dispatching.is_terminal());
}

#[test]
fn typed_errors_keep_their_category_and_context() {
    let provider_error = Error::Provider("invalid response frame".into());
    let cancelled = Error::Cancelled;

    assert_eq!(provider_error.category(), ErrorCategory::Provider);
    assert_eq!(
        provider_error.to_string(),
        "provider: invalid response frame"
    );
    assert_eq!(cancelled.category(), ErrorCategory::Cancelled);
    assert_eq!(cancelled.to_string(), "cancelled");
}
