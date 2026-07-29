//! Turn-to-history encoding: builds the `CompletedSessionTurn` persisted for a completed
//! headless or subagent turn, and sanitizes subagent summaries and results before they are
//! written to durable storage.

use std::sync::{Arc, Mutex};

use agens_core::{
    CompletedSessionTurn, CompletedTurnSnapshot, Message, MessagePart, Role, SessionMessage,
    SessionMetadata, TurnEvent,
};
use agens_store::SessionStore;

use crate::context::CompletedSubagentTurn;
use crate::context::SessionContext;
use crate::provider::ProviderKind;
use agens_bootstrap::Bootstrap;
use agens_error::CliError;

/// Builds the metadata for the next persisted attempt: unchanged when resuming an existing
/// session (its `project` was already recorded), or freshly seeded from the process's own
/// discovered root when no session exists yet — the only point where that discovery is a valid
/// confinement source, since no session has a recorded root to read back before its own first
/// persisted attempt.
pub fn next_session_metadata(
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
        project: agens_bootstrap::session_root::SessionRoot::discover_for_new_session(bootstrap)
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

pub fn completed_session_turn(
    prompt: &str,
    snapshot: &CompletedTurnSnapshot,
    pending_system_reminder: Option<&str>,
) -> Result<CompletedSessionTurn, CliError> {
    completed_session_turn_from_events(prompt, snapshot.events(), pending_system_reminder)
}

pub fn completed_session_turn_from_events(
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

pub fn completed_subagent_session_turn(
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

pub const SUBAGENT_CALL_ID_PREFIX: &str = "subagent:";
const MAX_SUBAGENT_SUMMARY_CHARS: usize = 256;
pub const MAX_PERSISTED_SUBAGENT_RESULT_CHARS: usize = 65_536;
pub const SUBAGENT_RESULT_TRUNCATION_MARKER: &str =
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

pub fn sanitize_subagent_summary(value: &str) -> String {
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

pub fn persist_completed_subagent_turn(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<SessionContext>>,
    turn: CompletedSubagentTurn,
) -> Result<(), CliError> {
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    let provider = context.provider.map(persisted_provider_identifier);
    let model = context
        .selection
        .as_ref()
        .map(|selection| selection.model().to_owned())
        .or_else(|| bootstrap.model().map(ToOwned::to_owned))
        .unwrap_or_else(|| {
            crate::model::resolved_provider(bootstrap, &context)
                .default_model()
                .to_owned()
        });
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

fn persisted_provider_identifier(provider: ProviderKind) -> String {
    provider.identifier().to_owned()
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
    use super::*;

    #[test]
    fn moonshot_persisted_identifier_round_trips() {
        let persisted = persisted_provider_identifier(ProviderKind::Moonshot);

        assert_eq!(persisted, "moonshotai");
        assert_eq!(
            ProviderKind::parse(&persisted),
            Some(ProviderKind::Moonshot)
        );
    }
}
