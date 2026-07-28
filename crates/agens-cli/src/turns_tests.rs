//! Turn-persistence tests that build their fixtures through the CLI's test
//! support, so they live here rather than inside `agens-session`.

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agens_core::{Message, MessagePart, Role, TurnEvent, TurnState, Usage};

    use crate::test_support::{tui_session_bootstrap, tui_session_directory};
    use agens_session::context::{CompletedSubagentTurn, SessionContext};
    use agens_session::turns::*;

    #[test]
    fn completed_session_turn_ignores_usage_without_changing_output_history_order() {
        let events = [
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::ProviderPart(MessagePart::Text("before usage".into())),
            TurnEvent::Usage(Usage {
                input_tokens: Some(5),
                output_tokens: Some(3),
                total_tokens: Some(8),
                context_window: Some(16),
            }),
            TurnEvent::ProviderPart(MessagePart::Reasoning("after usage".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ];

        let turn = completed_session_turn_from_events("prompt", &events, None)
            .expect("completed session turn should exclude presentation usage");

        assert_eq!(
            turn.messages(),
            &[
                Message {
                    role: Role::User,
                    parts: vec![MessagePart::Text("prompt".into())],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        MessagePart::Text("before usage".into()),
                        MessagePart::Reasoning("after usage".into()),
                    ],
                },
            ]
        );
    }

    #[test]
    fn completed_session_turn_keeps_role_boundaries_around_usage() {
        let events = [
            TurnEvent::ProviderPart(MessagePart::Text("before tool".into())),
            TurnEvent::Usage(Usage::default()),
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "tool output".into(),
                is_error: false,
            }),
            TurnEvent::Usage(Usage {
                input_tokens: None,
                output_tokens: Some(0),
                total_tokens: None,
                context_window: None,
            }),
            TurnEvent::ProviderPart(MessagePart::Text("after tool".into())),
        ];

        let turn = completed_session_turn_from_events("prompt", &events, None)
            .expect("completed session turn should exclude presentation usage");

        assert_eq!(
            turn.messages(),
            &[
                Message {
                    role: Role::User,
                    parts: vec![MessagePart::Text("prompt".into())],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![MessagePart::Text("before tool".into())],
                },
                Message {
                    role: Role::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_call_id: "call-1".into(),
                        content: "tool output".into(),
                        is_error: false,
                    }],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![MessagePart::Text("after tool".into())],
                },
            ]
        );
    }

    #[test]
    fn completed_session_turn_skips_tool_result_facts_without_a_role_message() {
        let mut facts_source = agens_core::TurnCoordinator::new();
        facts_source.begin().unwrap();
        facts_source
            .accept_provider_part(MessagePart::ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                input: "{\"command\":\"exit 1\"}".into(),
            })
            .unwrap();
        facts_source.finish_provider_iteration().unwrap();
        facts_source
            .accept_tool_result(
                "call-1",
                "exit 1".into(),
                true,
                Some(agens_core::ToolResultFacts::Bash {
                    outcome: agens_core::ToolOutcome::Failed,
                    exit_code: Some(1),
                }),
            )
            .unwrap();
        let facts_event = facts_source
            .events()
            .iter()
            .find(|event| matches!(event, TurnEvent::ToolResultFacts { .. }))
            .cloned()
            .expect("facts event must be present in the source coordinator");

        let events = [
            TurnEvent::ProviderPart(MessagePart::Text("before tool".into())),
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "exit 1".into(),
                is_error: true,
            }),
            facts_event,
            TurnEvent::ProviderPart(MessagePart::Text("after tool".into())),
        ];

        let turn = completed_session_turn_from_events("prompt", &events, None)
            .expect("completed session turn should skip live-only facts events");

        assert_eq!(
            turn.messages(),
            &[
                Message {
                    role: Role::User,
                    parts: vec![MessagePart::Text("prompt".into())],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![MessagePart::Text("before tool".into())],
                },
                Message {
                    role: Role::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_call_id: "call-1".into(),
                        content: "exit 1".into(),
                        is_error: true,
                    }],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![MessagePart::Text("after tool".into())],
                },
            ]
        );
    }

    #[test]
    fn p1c1_completed_subagent_turn_redacts_and_bounds_durable_content() {
        let turn = CompletedSubagentTurn {
            id: 1,
            agent: "reviewer".into(),
            task: format!("authorization {}", "x".repeat(300)),
            final_result: "token=result".into(),
            tool_uses: 1,
        };

        let messages = completed_subagent_session_turn(&turn, "subagent:1")
            .unwrap()
            .messages()
            .to_vec();

        assert_eq!(
            messages[0].parts,
            vec![MessagePart::Text("[redacted]".into())]
        );
        assert_eq!(
            messages[2].parts,
            vec![MessagePart::ToolResult {
                tool_call_id: "subagent:1".into(),
                content: "[withheld: 12 characters matched a credential pattern]".into(),
                is_error: false,
            }]
        );
    }

    #[test]
    fn p1c1_persisted_subagent_result_stays_bounded_and_marks_every_loss() {
        let subagent_turn = |final_result: String| CompletedSubagentTurn {
            id: 1,
            agent: "reviewer".into(),
            task: "review the patch".into(),
            final_result,
            tool_uses: 1,
        };
        let persisted_result = |turn: &CompletedSubagentTurn| {
            let messages = completed_subagent_session_turn(turn, "subagent:1")
                .unwrap()
                .messages()
                .to_vec();
            match &messages[2].parts[0] {
                MessagePart::ToolResult { content, .. } => content.clone(),
                part => panic!("subagent turns persist a tool result: {part:?}"),
            }
        };

        let long = persisted_result(&subagent_turn("a".repeat(70_000)));
        assert!(long.starts_with(&"a".repeat(MAX_PERSISTED_SUBAGENT_RESULT_CHARS)));
        assert!(long.ends_with(SUBAGENT_RESULT_TRUNCATION_MARKER));
        assert_eq!(
            long.chars().count(),
            MAX_PERSISTED_SUBAGENT_RESULT_CHARS + SUBAGENT_RESULT_TRUNCATION_MARKER.chars().count()
        );

        let bounded = persisted_result(&subagent_turn("a".repeat(300)));
        assert_eq!(bounded, "a".repeat(300));

        let with_secret = persisted_result(&subagent_turn(
            "usable finding\napi_key=abcd\ntrailing finding".into(),
        ));
        assert_eq!(
            with_secret,
            "usable finding\n[withheld: 12 characters matched a credential pattern]\ntrailing finding"
        );

        let only_secret = persisted_result(&subagent_turn("token=abcd".into()));
        assert_eq!(
            only_secret,
            "[withheld: 10 characters matched a credential pattern]"
        );
    }

    #[test]
    fn p1c1_persisted_subagent_call_ids_stay_unique_when_execution_ids_restart() {
        let temporary = tui_session_directory("subagent-call-id-uniqueness");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let turn = |final_result: &str| CompletedSubagentTurn {
            id: 1,
            agent: "reviewer".into(),
            task: "review the patch".into(),
            final_result: final_result.into(),
            tool_uses: 1,
        };

        persist_completed_subagent_turn(&bootstrap, &session, turn("first")).unwrap();
        persist_completed_subagent_turn(&bootstrap, &session, turn("second")).unwrap();

        let messages = session.lock().unwrap().messages.clone();
        let call_ids = messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                MessagePart::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            call_ids,
            vec!["subagent:1".to_owned(), "subagent:2".to_owned()]
        );
        agens_providers::encode_openai_response_request_with_messages("gpt-4.1", &messages, &[])
            .expect("a resumed subagent history must encode for the provider");

        std::fs::remove_dir_all(temporary).unwrap();
    }
}
