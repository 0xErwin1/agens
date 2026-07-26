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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agens_core::SessionMetadata;

    use super::*;
    use crate::test_support::{
        render_tui_test_backend, tui_session_bootstrap_for_provider, tui_session_directory,
    };
    use crate::tui::engine::{ProductionTuiEngine, configure_tui_project_identity};
    use agens_tui::Tui;

    #[test]
    fn completed_tui_turn_clears_reminders_only_after_successful_persistence() {
        let metadata = SessionMetadata {
            id: 1,
            project: "project".into(),
            title: "title".into(),
            active_agent: "reviewer".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 2,
            completed_turn_count: 2,
            resumable: true,
        };
        let mut context = TuiSessionContext::fresh();
        context.pending_system_reminder = Some("reminder".into());

        assert_eq!(
            complete_tui_turn(
                &mut context,
                Ok(HeadlessChatCompletion {
                    text: "answer".into(),
                    metadata: metadata.clone(),
                    messages: Vec::new(),
                }),
                true,
            )
            .unwrap(),
            "answer"
        );
        assert_eq!(context.metadata, Some(metadata));
        assert!(context.pending_system_reminder.is_none());

        context.pending_system_reminder = Some("reminder".into());
        assert!(
            complete_tui_turn(&mut context, Err(CliError::storage("failed").into()), true).is_err()
        );
        assert_eq!(context.pending_system_reminder.as_deref(), Some("reminder"));
    }

    #[test]
    fn p1c4_completing_a_turn_keeps_a_subagent_turn_persisted_mid_flight() {
        let subagent_turn = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("review the patch".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::ToolCall {
                        id: "subagent:1".into(),
                        name: "native::task".into(),
                        input: r#"{"agent":"reviewer","description":"review the patch"}"#.into(),
                    },
                    MessagePart::Reasoning("3 tool uses".into()),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "subagent:1".into(),
                    content: "approved".into(),
                    is_error: false,
                }],
            },
        ];
        let foreground_turn = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("summarize the patch".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text("summary".into())],
            },
        ];
        let mut session = TuiSessionContext {
            identifier: Some(7),
            messages: subagent_turn.clone(),
            ..TuiSessionContext::fresh()
        };
        let completion = HeadlessChatCompletion {
            text: "summary".into(),
            metadata: SessionMetadata {
                id: 7,
                project: "project".into(),
                title: "conversation".into(),
                active_agent: "primary".into(),
                provider_id: None,
                model_id: None,
                reasoning_effort: None,
                created_at: 1,
                updated_at: 1,
                completed_turn_count: 1,
                resumable: true,
            },
            messages: foreground_turn.clone(),
        };

        assert_eq!(
            complete_tui_turn(&mut session, Ok(completion), false).unwrap(),
            "summary"
        );

        let mut expected = foreground_turn;
        expected.extend(subagent_turn);
        assert_eq!(session.messages, expected);
    }

    #[test]
    fn fresh_tui_presentation_projects_resolved_model_effort_and_context() {
        let known_root = tui_session_directory("fresh-presentation-known");
        let known_bootstrap =
            tui_session_bootstrap_for_provider(&known_root, &[], "openai-api", "gpt-5.6-sol");
        let mut known_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        known_tui.apply_presentation(tui_session_presentation(
            &known_bootstrap,
            &TuiSessionContext::fresh(),
        ));
        configure_tui_project_identity(&mut known_tui, &known_bootstrap);
        let known = render_tui_test_backend(&known_tui, 140, 14);

        assert!(
            known.contains("gpt-5.6-sol · medium · 0/1.1m (0%)"),
            "{known:?}"
        );
        assert!(!known.contains("model · default · ctx —"), "{known:?}");

        let unknown_root = tui_session_directory("fresh-presentation-unknown");
        let unknown_bootstrap =
            tui_session_bootstrap_for_provider(&unknown_root, &[], "openai-api", "gpt-future-1");
        let mut unknown_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        unknown_tui.apply_presentation(tui_session_presentation(
            &unknown_bootstrap,
            &TuiSessionContext::fresh(),
        ));
        let unknown = render_tui_test_backend(&unknown_tui, 140, 14);

        assert!(
            unknown.contains("gpt-future-1 · effort — · ctx —"),
            "{unknown:?}"
        );
        assert!(
            !unknown.contains("gpt-future-1 · effort — · 0/"),
            "{unknown:?}"
        );

        std::fs::remove_dir_all(known_root).unwrap();
        std::fs::remove_dir_all(unknown_root).unwrap();
    }
}
