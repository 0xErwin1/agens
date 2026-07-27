//! Turn-to-history encoding: builds the `CompletedSessionTurn` persisted for a completed
//! headless or subagent turn, and sanitizes subagent summaries and results before they are
//! written to durable storage.

use std::sync::{Arc, Mutex};

use agens_core::{
    CompletedSessionTurn, CompletedTurnSnapshot, Message, MessagePart, Role, SessionMessage,
    SessionMetadata, TurnEvent,
};
use agens_store::SessionStore;

use crate::tools::task::default_model;
use crate::tui::provider::TuiProvider;
use crate::tui::session::{CompletedSubagentTurn, TuiSessionContext};
use crate::{Bootstrap, CliError};

/// Builds the metadata for the next persisted attempt: unchanged when resuming an existing
/// session (its `project` was already recorded), or freshly seeded from the process's own
/// discovered root when no session exists yet — the only point where that discovery is a valid
/// confinement source, since no session has a recorded root to read back before its own first
/// persisted attempt.
pub(crate) fn next_session_metadata(
    bootstrap: &Bootstrap,
    title: &str,
    resumed: Option<&SessionMetadata>,
    active_agent: Option<&str>,
    provider_id: Option<String>,
    model_id: String,
    reasoning_effort: Option<agens_core::ReasoningEffort>,
) -> Result<SessionMetadata, CliError> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| CliError::storage("session clock is unavailable"))?
        .as_secs() as i64;

    if let Some(metadata) = resumed {
        return Ok(SessionMetadata {
            updated_at: timestamp,
            provider_id,
            model_id: Some(model_id),
            reasoning_effort,
            ..metadata.clone()
        });
    }

    Ok(SessionMetadata {
        id: 0,
        project: crate::session_root::SessionRoot::discover_for_new_session(bootstrap)
            .map(|root| root.path().display().to_string())
            .unwrap_or_else(|| "default".to_owned()),
        title: title.to_owned(),
        active_agent: active_agent.unwrap_or("primary").to_owned(),
        provider_id,
        model_id: Some(model_id),
        reasoning_effort,
        created_at: timestamp,
        updated_at: timestamp,
        completed_turn_count: 0,
        resumable: false,
    })
}

pub(crate) fn completed_session_turn(
    prompt: &str,
    snapshot: &CompletedTurnSnapshot,
    pending_system_reminder: Option<&str>,
) -> Result<CompletedSessionTurn, CliError> {
    completed_session_turn_from_events(prompt, snapshot.events(), pending_system_reminder)
}

pub(crate) fn completed_session_turn_from_events(
    prompt: &str,
    events: &[TurnEvent],
    pending_system_reminder: Option<&str>,
) -> Result<CompletedSessionTurn, CliError> {
    let mut messages = pending_system_reminder
        .map(|reminder| Message {
            role: Role::System,
            parts: vec![MessagePart::Text(reminder.to_owned())],
        })
        .into_iter()
        .collect::<Vec<_>>();
    messages.push(Message {
        role: Role::User,
        parts: vec![MessagePart::Text(prompt.to_owned())],
    });
    let mut role = None;
    let mut parts = Vec::new();
    for event in events {
        let (next_role, part) = match event {
            TurnEvent::ProviderPart(part) => (Role::Assistant, part),
            TurnEvent::ToolResult(part) => (Role::Tool, part),
            TurnEvent::StateChanged(_)
            | TurnEvent::Usage(_)
            | TurnEvent::ToolCallRequested { .. }
            | TurnEvent::ToolResultFacts { .. } => continue,
        };
        if role != Some(next_role) {
            if let Some(role) = role {
                flush_parts(&mut messages, role, &mut parts);
            }
            role = Some(next_role);
        }
        parts.push(part.clone());
    }
    if let Some(role) = role {
        flush_parts(&mut messages, role, &mut parts);
    }

    let messages = messages
        .into_iter()
        .map(SessionMessage::try_from)
        .collect::<Result<_, _>>()
        .map_err(|_| CliError::storage("completed session could not be encoded"))?;
    CompletedSessionTurn::new(messages)
        .map_err(|_| CliError::storage("completed session could not be encoded"))
}

pub(crate) fn completed_subagent_session_turn(
    turn: &CompletedSubagentTurn,
    call_id: &str,
) -> Result<CompletedSessionTurn, CliError> {
    let call_id = call_id.to_owned();
    let agent = sanitize_subagent_summary(&turn.agent);
    let task = sanitize_subagent_summary(&turn.task);
    let final_result = sanitize_subagent_result_for_persistence(&turn.final_result);
    let input = serde_json::json!({
        "agent": agent,
        "description": task,
    })
    .to_string();
    let messages = vec![
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text(task)],
        },
        Message {
            role: Role::Assistant,
            parts: vec![
                MessagePart::ToolCall {
                    id: call_id.clone(),
                    name: "native::task".into(),
                    input,
                },
                MessagePart::Reasoning(format!("{} tool uses", turn.tool_uses)),
            ],
        },
        Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: call_id,
                content: final_result,
                is_error: false,
            }],
        },
    ];
    let messages = messages
        .into_iter()
        .map(SessionMessage::try_from)
        .collect::<Result<_, _>>()
        .map_err(|_| CliError::storage("completed session could not be encoded"))?;
    CompletedSessionTurn::new(messages)
        .map_err(|_| CliError::storage("completed session could not be encoded"))
}

pub(crate) const SUBAGENT_CALL_ID_PREFIX: &str = "subagent:";
const MAX_SUBAGENT_SUMMARY_CHARS: usize = 256;
pub(crate) const MAX_PERSISTED_SUBAGENT_RESULT_CHARS: usize = 65_536;
pub(crate) const SUBAGENT_RESULT_TRUNCATION_MARKER: &str =
    "\n[truncated: only the first 65536 characters of this subagent result were persisted]";
const CREDENTIAL_MARKERS: [&str; 5] = ["api_key", "authorization", "password", "secret", "token"];

/// A subagent tool-call id must be unique inside the session, not merely inside the process:
/// execution ids restart at one in every process, so a resumed session would otherwise persist a
/// duplicate call id and make the whole history unencodable for the provider.
fn next_subagent_call_id(history: &[Message]) -> String {
    let highest = history
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| match part {
            MessagePart::ToolCall { id, .. } => id.strip_prefix(SUBAGENT_CALL_ID_PREFIX),
            _ => None,
        })
        .filter_map(|value| value.parse::<u64>().ok())
        .max()
        .unwrap_or(0);

    format!("{SUBAGENT_CALL_ID_PREFIX}{}", highest.saturating_add(1))
}

pub(crate) fn sanitize_subagent_summary(value: &str) -> String {
    if contains_credential_marker(value) {
        "[redacted]".into()
    } else {
        value.chars().take(MAX_SUBAGENT_SUMMARY_CHARS).collect()
    }
}

/// The persisted result is the model's only durable record of a background subagent's work, so it
/// keeps the same budget the foreground task path allows and every removal stays visible: silent
/// truncation or a wholesale replacement would make the model reason over a fragment it cannot see.
fn sanitize_subagent_result_for_persistence(value: &str) -> String {
    let redacted = redact_credential_lines(value);
    let mut bounded = redacted
        .chars()
        .take(MAX_PERSISTED_SUBAGENT_RESULT_CHARS)
        .collect::<String>();
    if redacted.chars().count() > MAX_PERSISTED_SUBAGENT_RESULT_CHARS {
        bounded.push_str(SUBAGENT_RESULT_TRUNCATION_MARKER);
    }
    bounded
}

fn redact_credential_lines(value: &str) -> String {
    value
        .lines()
        .map(|line| {
            if contains_credential_marker(line) {
                format!(
                    "[withheld: {} characters matched a credential pattern]",
                    line.chars().count()
                )
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn contains_credential_marker(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    CREDENTIAL_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

pub(crate) fn persist_completed_subagent_turn(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<TuiSessionContext>>,
    turn: CompletedSubagentTurn,
) -> Result<(), CliError> {
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    let provider = context.provider.map(|provider| match provider {
        TuiProvider::OpenAiApi => "openai-api".to_owned(),
        TuiProvider::OpenAiChatGpt => "openai-chatgpt".to_owned(),
    });
    let model = context
        .selection
        .as_ref()
        .map(|selection| selection.model().to_owned())
        .or_else(|| bootstrap.model().map(ToOwned::to_owned))
        .unwrap_or_else(|| default_model(bootstrap).to_owned());
    let active_agent = context
        .active_agent
        .as_ref()
        .map(|agent| agent.name.as_str());
    let metadata = next_session_metadata(
        bootstrap,
        &turn.task,
        context.metadata.as_ref(),
        active_agent,
        provider,
        model,
        None,
    )?;
    let mut store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let persisted_history = context
        .identifier
        .and_then(|identifier| store.load_session_for_resume(identifier).ok())
        .map(|session| session.messages);
    let call_id = next_subagent_call_id(persisted_history.as_deref().unwrap_or(&context.messages));
    let metadata = store
        .persist_completed_session_turn(
            &metadata,
            &completed_subagent_session_turn(&turn, &call_id)?,
        )
        .map_err(|_| CliError::storage("completed session could not be saved"))?;
    let messages = store
        .load_session_for_resume(metadata.id)
        .map_err(|_| CliError::storage("completed session could not be loaded"))?
        .messages;
    context.identifier = Some(metadata.id);
    context.metadata = Some(metadata);
    context.messages = messages;
    Ok(())
}

fn flush_parts(messages: &mut Vec<Message>, role: Role, parts: &mut Vec<MessagePart>) {
    if !parts.is_empty() {
        messages.push(Message {
            role,
            parts: std::mem::take(parts),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agens_core::{TurnState, Usage};

    use super::*;
    use crate::test_support::{tui_session_bootstrap, tui_session_directory};

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
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
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
