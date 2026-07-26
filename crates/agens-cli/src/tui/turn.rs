//! Turn completion and presentation helpers: adopting a completed headless
//! turn's history back into the TUI session, reconciling subagent turns that
//! were persisted out of band, and deriving the provider/model/effort
//! presentation shown for a session.

use std::collections::BTreeSet;

use agens_core::{Message, MessagePart, Role};

use crate::bootstrap::Bootstrap;
use crate::error::CliError;
use crate::headless::{HeadlessChatCompletion, HeadlessChatFailure};
use crate::model_registry;
use crate::model_registry::TuiModelSelector;
use crate::tools::task::default_model;
use crate::tui::provider::TuiProvider;
use crate::tui::session::TuiSessionContext;
use crate::tui_model_source;
use crate::turns::SUBAGENT_CALL_ID_PREFIX;
use agens_tui::TuiPresentation;

pub(crate) fn complete_tui_turn(
    session: &mut TuiSessionContext,
    completion: Result<HeadlessChatCompletion, HeadlessChatFailure>,
    consumed_reminder: bool,
) -> Result<String, CliError> {
    let completion = match completion {
        Ok(completion) => completion,
        Err(failure) => {
            if let Some(partial) = failure.partial {
                session.identifier = Some(partial.metadata.id);
                session.metadata = Some(partial.metadata);
                adopt_turn_history(session, partial.messages);
            }

            return Err(failure.error);
        }
    };
    session.identifier = Some(completion.metadata.id);
    session.metadata = Some(completion.metadata);
    adopt_turn_history(session, completion.messages);
    if consumed_reminder {
        session.pending_system_reminder = None;
    }
    Ok(completion.text)
}

/// A background subagent turn can be persisted after the foreground turn reloaded the session, so
/// adopting the turn's history alone would drop that turn from the in-process request history for
/// the rest of the process even though the store keeps it.
pub(crate) fn adopt_turn_history(session: &mut TuiSessionContext, history: Vec<Message>) {
    let preserved = missing_subagent_turns(&session.messages, &history);
    session.messages = history;
    session.messages.extend(preserved);
}

pub(crate) fn missing_subagent_turns(previous: &[Message], history: &[Message]) -> Vec<Message> {
    let known = history
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(subagent_call_id)
        .collect::<BTreeSet<_>>();

    previous
        .windows(3)
        .filter(|window| {
            let [user, assistant, tool] = window else {
                return false;
            };
            let Some(call_id) = assistant.parts.iter().find_map(subagent_call_id) else {
                return false;
            };

            user.role == Role::User
                && !known.contains(call_id)
                && tool.parts.iter().any(|part| match part {
                    MessagePart::ToolResult { tool_call_id, .. } => tool_call_id == call_id,
                    _ => false,
                })
        })
        .flatten()
        .cloned()
        .collect()
}

pub(crate) fn subagent_call_id(part: &MessagePart) -> Option<&str> {
    match part {
        MessagePart::ToolCall { id, .. } if id.starts_with(SUBAGENT_CALL_ID_PREFIX) => Some(id),
        _ => None,
    }
}

pub(crate) fn current_tui_provider(
    bootstrap: &Bootstrap,
    context: &TuiSessionContext,
) -> Option<TuiProvider> {
    if context.chatgpt_unavailable {
        return None;
    }
    if context.resume_error.is_some()
        && context
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.provider_id.is_some())
        && context.provider.is_none()
    {
        return None;
    }
    context
        .provider
        .or_else(|| bootstrap.provider_type().and_then(TuiProvider::parse))
}

pub(crate) fn effective_tui_model(bootstrap: &Bootstrap, context: &TuiSessionContext) -> String {
    context
        .selection
        .as_ref()
        .map(TuiModelSelector::model)
        .or_else(|| {
            context
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.model_id.as_deref())
        })
        .or_else(|| bootstrap.model())
        .unwrap_or_else(|| default_model(bootstrap))
        .to_owned()
}

pub(crate) fn tui_session_presentation(
    bootstrap: &Bootstrap,
    session: &TuiSessionContext,
) -> TuiPresentation {
    let model = effective_tui_model(bootstrap, session);
    let provider = session
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.provider_id.as_deref())
        .or_else(|| current_tui_provider(bootstrap, session).map(TuiProvider::identifier))
        .unwrap_or_else(|| bootstrap.provider_type().unwrap_or("provider"));
    let label = session
        .identifier
        .map_or_else(|| "new session".into(), |id| format!("session #{id}"));
    let effort = session
        .selection
        .as_ref()
        .and_then(|selection| {
            selection
                .reasoning_effort()
                .or_else(|| selection.reasoning_effort_default())
        })
        .or_else(|| {
            TuiModelSelector::for_source(&model, tui_model_source(bootstrap, session))
                .reasoning_effort_default()
        });
    let mut presentation = TuiPresentation::new(provider, &model, label)
        .with_context_window(model_registry::context_window_for(&model))
        .with_dangerous_mode(session.dangerous_mode);
    if let Some(effort) = effort {
        presentation = presentation.with_effort(effort);
    }
    presentation
}
