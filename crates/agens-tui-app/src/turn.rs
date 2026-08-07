//! Turn completion and presentation helpers: adopting a completed headless
//! turn's history back into the TUI session, reconciling subagent turns that
//! were persisted out of band, and deriving the provider/model/effort
//! presentation shown for a session.

use agens_session::model::{current_provider, effective_model, model_source};
use std::collections::BTreeSet;

use agens_core::{Message, MessagePart, Role};

use agens_bootstrap::Bootstrap;
use agens_error::CliError;
use agens_headless::{HeadlessChatCompletion, HeadlessChatFailure};
use agens_models::ModelSelection;
use agens_session::context::SessionContext;
use agens_session::provider::ProviderKind;
use agens_session::turns::SUBAGENT_CALL_ID_PREFIX;
use agens_session::undo::turn_boundary;
use agens_tui::TuiPresentation;

pub fn complete_tui_turn(
    session: &mut SessionContext,
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
                if consumed_reminder {
                    session.pending_system_reminder = None;
                }
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
///
/// Such a turn goes back where the store holds it — at the point the reloaded history stops
/// agreeing with the one in hand, which is after everything both already agree on and before the
/// turn being adopted — rather than at the tail. Undo derives its bound by comparing these two
/// histories, so a turn kept anywhere but its persisted position makes them disagree earlier than
/// the turn did, and a later commit then drops every message past that earlier point from this
/// history and deletes the matching range from the store, taking turns nobody took back.
///
/// The insertion point is the boundary [`turn_boundary`] derives, so it can never fall between a
/// tool call and its result.
pub fn adopt_turn_history(session: &mut SessionContext, history: Vec<Message>) {
    let preserved = missing_subagent_turns(&session.messages, &history);
    let divergence = turn_boundary(&session.messages, &history);

    session.messages = history;
    session.messages.splice(divergence..divergence, preserved);
}

pub fn missing_subagent_turns(previous: &[Message], history: &[Message]) -> Vec<Message> {
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

pub fn subagent_call_id(part: &MessagePart) -> Option<&str> {
    match part {
        MessagePart::ToolCall { id, .. } if id.starts_with(SUBAGENT_CALL_ID_PREFIX) => Some(id),
        _ => None,
    }
}

pub fn tui_session_presentation(
    bootstrap: &Bootstrap,
    session: &SessionContext,
) -> TuiPresentation {
    let model = effective_model(bootstrap, session);
    let provider = session
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.provider_id.as_deref())
        .or_else(|| current_provider(bootstrap, session).map(ProviderKind::identifier))
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
            ModelSelection::for_source(&model, model_source(bootstrap, session))
                .reasoning_effort_default()
        });
    let mut presentation = TuiPresentation::new(provider, &model, label)
        .with_context_window(agens_models::context_window_for(&model))
        .with_dangerous_mode(session.dangerous_mode)
        .with_bypass(session.bypass_permissions);
    if let Some(effort) = effort {
        presentation = presentation.with_effort(effort);
    }
    presentation
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agens_core::SessionMetadata;
    use agens_session::attempt::PartialTurnRecord;

    use super::*;
    use crate::engine::{ProductionTuiEngine, configure_tui_project_identity};
    use crate::test_support::{
        render_tui_test_backend, tui_session_bootstrap_for_provider, tui_session_directory,
    };
    use agens_tui::Tui;

    /// A fixture directory that removes itself when the test ends, whether it ends by returning or
    /// by panicking on a failed assertion.
    struct FixtureDirectory(std::path::PathBuf);

    impl FixtureDirectory {
        fn new(label: &str) -> Self {
            Self(tui_session_directory(label))
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for FixtureDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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
        let mut context = SessionContext::fresh();
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
    fn failed_tui_turn_adopts_partial_history_and_clears_its_persisted_reminder() {
        let metadata = SessionMetadata {
            id: 2,
            project: "project".into(),
            title: "title".into(),
            active_agent: "reviewer".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 2,
            completed_turn_count: 1,
            resumable: true,
        };
        let messages = vec![
            Message {
                role: Role::System,
                parts: vec![MessagePart::Text("reminder".into())],
            },
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("question".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text("partial answer".into())],
            },
        ];
        let mut context = SessionContext::fresh();
        context.pending_system_reminder = Some("reminder".into());
        let failure = HeadlessChatFailure {
            error: CliError::runtime(agens_core::HeadlessTurnError::ProviderServer),
            partial: Some(Box::new(PartialTurnRecord {
                metadata: metadata.clone(),
                messages: messages.clone(),
            })),
        };

        assert!(complete_tui_turn(&mut context, Err(failure), true).is_err());
        assert_eq!(context.identifier, Some(metadata.id));
        assert_eq!(context.metadata, Some(metadata));
        assert_eq!(context.messages, messages);
        assert!(context.pending_system_reminder.is_none());
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
        let mut session = SessionContext {
            identifier: Some(7),
            messages: subagent_turn.clone(),
            ..SessionContext::fresh()
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

        let mut expected = subagent_turn;
        expected.extend(foreground_turn);
        assert_eq!(session.messages, expected);
    }

    #[test]
    fn a_subagent_turn_persisted_mid_flight_is_kept_where_the_store_holds_it() {
        let settled = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("open the file".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text("opened".into())],
            },
        ];
        let subagent_turns = ["subagent:1", "subagent:2"]
            .into_iter()
            .flat_map(|call_id| {
                [
                    Message {
                        role: Role::User,
                        parts: vec![MessagePart::Text(format!("task for {call_id}"))],
                    },
                    Message {
                        role: Role::Assistant,
                        parts: vec![
                            MessagePart::ToolCall {
                                id: call_id.into(),
                                name: "native::task".into(),
                                input: r#"{"agent":"reviewer","description":"review"}"#.into(),
                            },
                            MessagePart::Reasoning("2 tool uses".into()),
                        ],
                    },
                    Message {
                        role: Role::Tool,
                        parts: vec![MessagePart::ToolResult {
                            tool_call_id: call_id.into(),
                            content: "approved".into(),
                            is_error: false,
                        }],
                    },
                ]
            })
            .collect::<Vec<_>>();
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
        let mut previous = settled.clone();
        previous.extend(subagent_turns.clone());
        let mut reloaded = settled.clone();
        reloaded.extend(foreground_turn.clone());
        let mut session = SessionContext {
            identifier: Some(9),
            messages: previous.clone(),
            ..SessionContext::fresh()
        };

        adopt_turn_history(&mut session, reloaded);

        let mut expected = settled;
        expected.extend(subagent_turns);
        expected.extend(foreground_turn);
        assert_eq!(session.messages, expected);
        assert_eq!(
            agens_session::undo::turn_boundary(&previous, &session.messages),
            previous.len(),
            "the turn adopted here is the only one a later undo may take back"
        );
    }

    #[test]
    fn fresh_tui_presentation_projects_resolved_model_effort_and_context() {
        let known_root = FixtureDirectory::new("fresh-presentation-known");
        let known_bootstrap =
            tui_session_bootstrap_for_provider(known_root.path(), &[], "openai-api", "gpt-5.6-sol");
        let mut known_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        known_tui.apply_presentation(tui_session_presentation(
            &known_bootstrap,
            &SessionContext::fresh(),
        ));
        configure_tui_project_identity(&mut known_tui, &known_bootstrap);
        let known = render_tui_test_backend(&known_tui, 140, 14);

        assert!(known.contains("gpt-5.6-sol (medium) ·   0%"), "{known:?}");
        assert!(!known.contains("model · default · ctx —"), "{known:?}");

        let unknown_root = FixtureDirectory::new("fresh-presentation-unknown");
        let unknown_bootstrap = tui_session_bootstrap_for_provider(
            unknown_root.path(),
            &[],
            "openai-api",
            "gpt-future-1",
        );
        let mut unknown_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        unknown_tui.apply_presentation(tui_session_presentation(
            &unknown_bootstrap,
            &SessionContext::fresh(),
        ));
        let unknown = render_tui_test_backend(&unknown_tui, 140, 14);

        assert!(unknown.contains("gpt-future-1 · ctx —"), "{unknown:?}");
        assert!(!unknown.contains("gpt-future-1 · 0/"), "{unknown:?}");
    }

    #[test]
    fn tui_presentation_carries_the_session_bypass_state_into_the_footer() {
        let root = FixtureDirectory::new("presentation-bypass");
        let bootstrap =
            tui_session_bootstrap_for_provider(root.path(), &[], "openai-api", "gpt-5.6-sol");
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        let mut session = SessionContext::fresh();
        session.bypass_permissions = true;
        tui.apply_presentation(tui_session_presentation(&bootstrap, &session));
        let rendered = render_tui_test_backend(&tui, 140, 14);
        assert!(rendered.contains("bypass"), "{rendered:?}");
    }
}
