use agens_core::{
    Error, ErrorCategory, Message, MessagePart, Role, TurnCoordinator, TurnEvent, TurnEventError,
    TurnState, TurnTransitionError,
};

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

#[test]
fn turn_state_advances_through_a_tool_iteration_to_completion() {
    let state = TurnState::Idle
        .transition_to(TurnState::Requesting)
        .unwrap()
        .transition_to(TurnState::Streaming)
        .unwrap()
        .transition_to(TurnState::Dispatching)
        .unwrap()
        .transition_to(TurnState::Requesting)
        .unwrap()
        .transition_to(TurnState::Completed)
        .unwrap();

    assert_eq!(state, TurnState::Completed);
}

#[test]
fn every_active_turn_state_can_be_cancelled_or_failed() {
    for state in [
        TurnState::Requesting,
        TurnState::Streaming,
        TurnState::Dispatching,
    ] {
        assert_eq!(
            state.transition_to(TurnState::Cancelled),
            Ok(TurnState::Cancelled)
        );
        assert_eq!(
            state.transition_to(TurnState::Failed),
            Ok(TurnState::Failed)
        );
    }
}

#[test]
fn invalid_and_terminal_turn_transitions_return_typed_source_and_target_errors() {
    let invalid = TurnState::Idle.transition_to(TurnState::Streaming);

    assert_eq!(
        invalid,
        Err(TurnTransitionError {
            source: TurnState::Idle,
            target: TurnState::Streaming,
        })
    );

    for source in [
        TurnState::Completed,
        TurnState::Cancelled,
        TurnState::Failed,
    ] {
        let transition = source.transition_to(TurnState::Requesting);

        assert_eq!(
            transition,
            Err(TurnTransitionError {
                source,
                target: TurnState::Requesting,
            })
        );
        assert_eq!(
            transition.unwrap_err().to_string(),
            format!("invalid turn state transition: {source:?} -> Requesting")
        );
    }
}

#[test]
fn coordinator_emits_deterministic_events_for_two_tool_iterations() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::Reasoning("inspect the repository".into()))
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "search".into(),
            input: "{\"query\":\"core\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result("call-1", "found core".into(), false)
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::Text("continue".into()))
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-2".into(),
            name: "read".into(),
            input: "{\"path\":\"Cargo.toml\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result("call-2", "package manifest".into(), false)
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::Text("complete".into()))
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();

    assert_eq!(coordinator.state(), TurnState::Completed);
    assert_eq!(
        coordinator.events(),
        &[
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Reasoning("inspect the repository".into())),
            TurnEvent::ProviderPart(MessagePart::ToolCall {
                id: "call-1".into(),
                name: "search".into(),
                input: "{\"query\":\"core\"}".into(),
            }),
            TurnEvent::StateChanged(TurnState::Dispatching),
            TurnEvent::ToolCallRequested {
                id: "call-1".into(),
                name: "search".into(),
                input: "{\"query\":\"core\"}".into(),
            },
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "found core".into(),
                is_error: false,
            }),
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("continue".into())),
            TurnEvent::ProviderPart(MessagePart::ToolCall {
                id: "call-2".into(),
                name: "read".into(),
                input: "{\"path\":\"Cargo.toml\"}".into(),
            }),
            TurnEvent::StateChanged(TurnState::Dispatching),
            TurnEvent::ToolCallRequested {
                id: "call-2".into(),
                name: "read".into(),
                input: "{\"path\":\"Cargo.toml\"}".into(),
            },
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-2".into(),
                content: "package manifest".into(),
                is_error: false,
            }),
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("complete".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ]
    );
}

#[test]
fn coordinator_rejects_out_of_order_and_uncorrelated_tool_results() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    assert_eq!(
        coordinator.accept_tool_result("call-1", "result".into(), false),
        Err(TurnEventError::UnexpectedToolResult {
            tool_call_id: "call-1".into(),
        })
    );

    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "search".into(),
            input: "{}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();

    assert_eq!(
        coordinator.accept_tool_result("call-2", "result".into(), false),
        Err(TurnEventError::UnexpectedToolResult {
            tool_call_id: "call-2".into(),
        })
    );
}

#[test]
fn coordinator_rejects_provider_tool_results_without_mutating_state_or_events() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    let events_before_rejection = coordinator.events().to_vec();

    assert_eq!(
        coordinator.accept_provider_part(MessagePart::ToolResult {
            tool_call_id: "call-1".into(),
            content: "result".into(),
            is_error: false,
        }),
        Err(TurnEventError::InvalidProviderPart)
    );
    assert_eq!(coordinator.state(), TurnState::Requesting);
    assert_eq!(coordinator.events(), events_before_rejection);
}

#[test]
fn coordinator_rejects_duplicate_pending_tool_call_ids_without_mutating_state_or_events() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "search".into(),
            input: "{}".into(),
        })
        .unwrap();
    let events_before_rejection = coordinator.events().to_vec();

    assert_eq!(
        coordinator.accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            input: "{}".into(),
        }),
        Err(TurnEventError::DuplicateToolCallId {
            id: "call-1".into(),
        })
    );
    assert_eq!(coordinator.state(), TurnState::Streaming);
    assert_eq!(coordinator.events(), events_before_rejection);
}

#[test]
fn cancellation_and_failure_reject_all_further_events() {
    let mut cancelled = TurnCoordinator::new();

    cancelled.begin().unwrap();
    cancelled.cancel().unwrap();
    assert_eq!(
        cancelled.accept_provider_part(MessagePart::Text("late".into())),
        Err(TurnEventError::Transition(TurnTransitionError {
            source: TurnState::Cancelled,
            target: TurnState::Streaming,
        }))
    );

    let mut failed = TurnCoordinator::new();

    failed.begin().unwrap();
    failed.fail().unwrap();
    assert_eq!(
        failed.finish_provider_iteration(),
        Err(TurnEventError::Transition(TurnTransitionError {
            source: TurnState::Failed,
            target: TurnState::Streaming,
        }))
    );
}
