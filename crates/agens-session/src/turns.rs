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

const EMPTY_TOOL_RESULT_CONTENT: &str = "[tool returned no output]";

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
    completed_session_turn_with_media(prompt, &[], snapshot, pending_system_reminder)
}

/// Like [`completed_session_turn`], but persists path-free user [`MessagePart::Media`]
/// parts alongside the prompt text (same shape as the live headless request).
pub fn completed_session_turn_with_media(
    prompt: &str,
    media: &[(i64, String)],
    snapshot: &CompletedTurnSnapshot,
    pending_system_reminder: Option<&str>,
) -> Result<CompletedSessionTurn, CliError> {
    completed_session_turn_from_events_with_media(
        prompt,
        media,
        snapshot.events(),
        pending_system_reminder,
    )
}

pub fn completed_session_turn_from_events(
    prompt: &str,
    events: &[TurnEvent],
    pending_system_reminder: Option<&str>,
) -> Result<CompletedSessionTurn, CliError> {
    completed_session_turn_from_events_with_media(prompt, &[], events, pending_system_reminder)
}

/// Builds a durable completed turn whose user message includes text (when non-empty)
/// and path-free media refs. Media-only turns (empty prompt + media) are valid.
pub fn completed_session_turn_from_events_with_media(
    prompt: &str,
    media: &[(i64, String)],
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
        parts: durable_user_parts(prompt, media),
    });
    let mut role = None;
    let mut parts = Vec::new();
    for event in events {
        let (next_role, part) = match event {
            TurnEvent::ProviderPart(part) => (Role::Assistant, part.clone()),
            TurnEvent::ToolResult(part) => (Role::Tool, persistable_tool_result(part)),
            TurnEvent::StateChanged(_)
            | TurnEvent::Usage(_)
            | TurnEvent::ToolCallRequested { .. }
            | TurnEvent::ToolResultFacts { .. }
            | TurnEvent::ProviderRetry { .. } => continue,
        };
        if role != Some(next_role) {
            if let Some(role) = role {
                flush_parts(&mut messages, role, &mut parts);
            }
            role = Some(next_role);
        }
        push_coalesced_part(&mut parts, part);
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

/// User parts for durable history: text if non-empty, then path-free media refs.
/// Falls back to a single text part when both prompt and media are empty so the
/// empty-prompt legacy path still produces a (failing-or-valid) user message shape.
fn durable_user_parts(prompt: &str, media: &[(i64, String)]) -> Vec<MessagePart> {
    let mut parts = Vec::new();

    if !prompt.is_empty() {
        parts.push(MessagePart::Text(prompt.to_owned()));
    }

    for (media_id, mime) in media {
        parts.push(MessagePart::Media {
            media_id: *media_id,
            mime: mime.clone(),
        });
    }

    if parts.is_empty() {
        parts.push(MessagePart::Text(prompt.to_owned()));
    }

    parts
}

fn push_coalesced_part(parts: &mut Vec<MessagePart>, part: MessagePart) {
    match (parts.last_mut(), &part) {
        (Some(MessagePart::Text(current)), MessagePart::Text(next)) => current.push_str(next),
        (Some(MessagePart::Reasoning(current)), MessagePart::Reasoning(next)) => {
            current.push_str(next);
        }
        _ => parts.push(part),
    }
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
    let tool_result = persistable_tool_result(&MessagePart::ToolResult {
        tool_call_id: call_id.clone(),
        content: final_result,
        is_error: false,
    });
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
                    // The wire name the model itself used. Recording the
                    // dispatcher's `native::` name instead put a string the
                    // provider rejects into replayable history.
                    name: "task".into(),
                    input,
                },
                MessagePart::Reasoning(format!("{} tool uses", turn.tool_uses)),
            ],
        },
        Message {
            role: Role::Tool,
            parts: vec![tool_result],
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

fn persistable_tool_result(part: &MessagePart) -> MessagePart {
    match part {
        MessagePart::ToolResult {
            tool_call_id,
            content,
            is_error,
        } if content.is_empty() => MessagePart::ToolResult {
            tool_call_id: tool_call_id.clone(),
            content: EMPTY_TOOL_RESULT_CONTENT.into(),
            is_error: *is_error,
        },
        _ => part.clone(),
    }
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
    fn completed_turn_with_media_persists_text_and_path_free_media_parts() {
        let events = [TurnEvent::ProviderPart(MessagePart::Text(
            "saw the image".into(),
        ))];
        let media = [
            (7_i64, "image/png".to_owned()),
            (11, "image/jpeg".to_owned()),
        ];

        let turn =
            completed_session_turn_from_events_with_media("describe this", &media, &events, None)
                .expect("text plus media should encode");

        let user = &turn.messages()[0];
        assert_eq!(user.role, Role::User);
        assert_eq!(
            user.parts,
            vec![
                MessagePart::Text("describe this".into()),
                MessagePart::Media {
                    media_id: 7,
                    mime: "image/png".into(),
                },
                MessagePart::Media {
                    media_id: 11,
                    mime: "image/jpeg".into(),
                },
            ]
        );
        for part in &user.parts {
            match part {
                MessagePart::Media { media_id, mime } => {
                    assert!(*media_id > 0);
                    assert!(!mime.is_empty());
                }
                MessagePart::Text(text) => assert!(!text.is_empty()),
                other => panic!("unexpected durable user part: {other:?}"),
            }
        }
    }

    #[test]
    fn completed_turn_media_only_empty_prompt_skips_empty_text_part() {
        let events = [TurnEvent::ProviderPart(MessagePart::Text("ok".into()))];
        let media = [(3_i64, "image/png".to_owned())];

        let turn = completed_session_turn_from_events_with_media("", &media, &events, None)
            .expect("media-only turn must not hit EmptyPart");

        assert_eq!(
            turn.messages()[0].parts,
            vec![MessagePart::Media {
                media_id: 3,
                mime: "image/png".into(),
            }]
        );
    }

    #[test]
    fn completed_turn_without_media_api_stays_text_only() {
        let events = [TurnEvent::ProviderPart(MessagePart::Text("reply".into()))];
        let turn = completed_session_turn_from_events("hello", &events, None).unwrap();

        assert_eq!(
            turn.messages()[0].parts,
            vec![MessagePart::Text("hello".into())]
        );
    }

    #[test]
    fn completed_turn_coalesces_adjacent_streamed_text_and_reasoning_fragments() {
        let events = [
            TurnEvent::ProviderPart(MessagePart::Reasoning("think".into())),
            TurnEvent::ProviderPart(MessagePart::Reasoning("ing".into())),
            TurnEvent::ProviderPart(MessagePart::Text("hel".into())),
            TurnEvent::ProviderPart(MessagePart::Text("lo".into())),
        ];

        let turn = completed_session_turn_from_events("prompt", &events, None).unwrap();
        let assistant = &turn.messages()[1];

        assert_eq!(
            assistant.parts,
            vec![
                MessagePart::Reasoning("thinking".into()),
                MessagePart::Text("hello".into()),
            ]
        );
    }

    #[test]
    fn completed_turn_marks_an_empty_tool_result_as_no_output() {
        let events = [
            TurnEvent::ProviderPart(MessagePart::ToolCall {
                id: "call-1".into(),
                name: "search".into(),
                input: r#"{"query":"absent"}"#.into(),
            }),
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: String::new(),
                is_error: false,
            }),
            TurnEvent::ProviderPart(MessagePart::Text("No matches found.".into())),
        ];

        let turn = completed_session_turn_from_events("search", &events, None)
            .expect("an empty tool result should remain persistable");

        assert_eq!(
            turn.messages()[2].parts,
            vec![MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "[tool returned no output]".into(),
                is_error: false,
            }]
        );
    }

    #[test]
    fn completed_subagent_turn_marks_an_empty_result_as_no_output() {
        let turn = CompletedSubagentTurn {
            id: 1,
            agent: "reviewer".into(),
            task: "review the patch".into(),
            final_result: String::new(),
            tool_uses: 0,
        };

        let turn = completed_subagent_session_turn(&turn, "subagent:1")
            .expect("an empty subagent result should remain persistable");

        assert_eq!(
            turn.messages()[2].parts,
            vec![MessagePart::ToolResult {
                tool_call_id: "subagent:1".into(),
                content: "[tool returned no output]".into(),
                is_error: false,
            }]
        );
    }

    /// The provider rejects the WHOLE request when a replayed history item
    /// carries a name outside `^[a-zA-Z0-9_-]+$`, so recording the dispatcher's
    /// internal name here poisoned every later turn of the session.
    #[test]
    fn a_completed_subagent_records_the_wire_name_the_model_used() {
        let turn = CompletedSubagentTurn {
            id: 1,
            agent: "reviewer".into(),
            task: "review the patch".into(),
            final_result: "done".into(),
            tool_uses: 2,
        };

        let turn = completed_subagent_session_turn(&turn, "subagent:1")
            .expect("a completed subagent turn should be persistable");

        let Some(MessagePart::ToolCall { name, .. }) = turn.messages()[1].parts.first() else {
            panic!("the assistant message opens with the task call");
        };
        assert_eq!(name, "task");
    }

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
