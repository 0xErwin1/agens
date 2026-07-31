//! Turning a session into the request a provider receives: its messages, its
//! reasoning effort, and the delegation instruction appended to the prompt.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use agens_core::{Message, MessagePart, PermissionMode, Role, SessionMetadata};
use agens_tools::{EffectiveCapabilitySet, SkillCatalog};

use agens_bootstrap::Bootstrap;
use agens_session::context::SessionContext;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadlessChatRequest {
    pub prompt: String,
    pub history: Vec<Message>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub max_iterations: Option<usize>,
    pub mode: PermissionMode,
    pub dangerously_allow_all: bool,
    pub dangerous_mode: bool,
    pub request_config: agens_core::RequestConfig,
    pub session_reasoning_effort: Option<agens_core::ReasoningEffort>,
    pub session: Option<SessionMetadata>,
    pub active_agent: Option<String>,
    pub effective_capabilities: Option<EffectiveCapabilitySet>,
    pub pending_system_reminder: Option<String>,
    pub skills: Option<Arc<SkillCatalog>>,
}

pub fn provider_messages(
    request: &HeadlessChatRequest,
    include_system_prompt: bool,
) -> Vec<Message> {
    let mut messages = replay_safe_history(&request.history);
    if include_system_prompt
        && request.skills.is_some()
        && let Some(system_prompt) = &request.system_prompt
    {
        messages.insert(
            0,
            Message {
                role: Role::System,
                parts: vec![MessagePart::Text(system_prompt.clone())],
            },
        );
    }
    if let Some(reminder) = &request.pending_system_reminder {
        messages.push(Message {
            role: Role::System,
            parts: vec![MessagePart::Text(reminder.clone())],
        });
    }
    messages.push(Message {
        role: Role::User,
        parts: vec![MessagePart::Text(request.prompt.clone())],
    });
    messages
}

fn replay_safe_history(history: &[Message]) -> Vec<Message> {
    let mut call_counts = BTreeMap::new();
    let mut result_counts = BTreeMap::new();
    for part in history.iter().flat_map(|message| &message.parts) {
        match part {
            MessagePart::ToolCall { id, .. } => {
                *call_counts.entry(id.as_str()).or_insert(0_usize) += 1;
            }
            MessagePart::ToolResult { tool_call_id, .. } => {
                *result_counts
                    .entry(tool_call_id.as_str())
                    .or_insert(0_usize) += 1;
            }
            MessagePart::Text(_) | MessagePart::Reasoning(_) => {}
        }
    }
    let mut retained_call_ids = BTreeSet::new();
    let mut replay = Vec::with_capacity(history.len());

    for message in history {
        let parts = match message.role {
            Role::Assistant => {
                let call_ids = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        MessagePart::ToolCall { id, .. } => Some(id.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let batch_is_complete = !call_ids.is_empty()
                    && call_ids.iter().all(|id| {
                        call_counts.get(id).copied() == Some(1)
                            && result_counts.get(id).copied() == Some(1)
                    });
                if batch_is_complete {
                    retained_call_ids.extend(call_ids);
                }
                message
                    .parts
                    .iter()
                    .filter(|part| {
                        !matches!(part, MessagePart::ToolCall { .. }) || batch_is_complete
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            }
            Role::Tool => message
                .parts
                .iter()
                .filter(|part| match part {
                    MessagePart::ToolResult { tool_call_id, .. } => {
                        retained_call_ids.contains(tool_call_id.as_str())
                    }
                    _ => false,
                })
                .cloned()
                .collect(),
            Role::System | Role::User => message.parts.clone(),
        };
        if !parts.is_empty() {
            replay.push(Message {
                role: message.role,
                parts,
            });
        }
    }

    replay
}

/// Applies the configured reasoning effort to a request that carries none.
/// An explicit model selection or `/effort` choice already populated the
/// request config, and must not be overwritten by the configured default.
/// Fills a request from a session. A free function rather than a method because
/// the request is the CLI's type and the session is not.
pub fn apply_session_to_request(
    context: &SessionContext,
    mut request: HeadlessChatRequest,
) -> HeadlessChatRequest {
    request.dangerous_mode = context.dangerous_mode;
    request.dangerously_allow_all |= context.bypass_permissions;
    if context.identifier.is_some() {
        request.history = context.messages.clone();
        request.session = context.metadata.clone();
    }

    let selected_model = context.selection.as_ref().map(|selection| {
        request.model = Some(selection.model().to_owned());
        request.request_config = selection.request_config().clone();
        request.session_reasoning_effort = selection.reasoning_effort_value();
        selection.model()
    });
    if let Some(agent) = &context.active_agent {
        let overrides_selection = selected_model.is_some_and(|selected| {
            agent
                .model
                .as_deref()
                .is_some_and(|model| model != selected)
        });
        if request.model.is_none() || overrides_selection {
            request.model = agent.model.clone();
        }
        if overrides_selection {
            request.request_config = Default::default();
            request.session_reasoning_effort = None;
        }
        request
            .system_prompt
            .get_or_insert_with(|| agent.system_prompt.clone());
        request.active_agent = Some(agent.name.clone());
        request.effective_capabilities = Some(agent.capabilities.clone());
    }
    request.pending_system_reminder = context.pending_system_reminder.clone();

    request
}

pub fn seed_configured_reasoning_effort(request: &mut HeadlessChatRequest, bootstrap: &Bootstrap) {
    if request.request_config.reasoning_effort().is_some() {
        return;
    }
    let Some(effort) = bootstrap.reasoning_effort() else {
        return;
    };
    let Ok(config) = agens_core::RequestConfig::with_reasoning_effort(effort) else {
        return;
    };

    request.session_reasoning_effort = config.reasoning_effort();
    request.request_config = config;
}

pub fn explicit_task_delegation_prompt(base: &str) -> String {
    const INSTRUCTION: &str = "When the user explicitly asks for subagent delegation, use the `task` tool instead of completing the delegated work inline. Use `task_control` to inspect, background, or cancel a live execution and `task_message` to send bounded coordination without waiting for completion.";

    if base.contains(INSTRUCTION) {
        base.to_owned()
    } else {
        format!("{base}\n\n{INSTRUCTION}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agens_core::PermissionMode;
    use agens_session::context::SessionContext;

    fn bare_request() -> HeadlessChatRequest {
        HeadlessChatRequest {
            prompt: String::new(),
            history: Vec::new(),
            model: None,
            system_prompt: None,
            max_iterations: None,
            mode: PermissionMode::Edit,
            dangerously_allow_all: false,
            dangerous_mode: false,
            request_config: agens_core::RequestConfig::default(),
            session_reasoning_effort: None,
            session: None,
            active_agent: None,
            effective_capabilities: None,
            pending_system_reminder: None,
            skills: None,
        }
    }

    #[test]
    fn provider_history_omits_an_incomplete_tool_batch_without_losing_partial_prose() {
        let mut request = bare_request();
        request.prompt = "continue".into();
        request.history = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("inspect".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::Reasoning("checking".into()),
                    MessagePart::ToolCall {
                        id: "complete".into(),
                        name: "native::read".into(),
                        input: r#"{"path":"a"}"#.into(),
                    },
                    MessagePart::ToolCall {
                        id: "missing".into(),
                        name: "native::read".into(),
                        input: r#"{"path":"b"}"#.into(),
                    },
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "complete".into(),
                    content: "a".into(),
                    is_error: false,
                }],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text("partial prose".into())],
            },
        ];

        let messages = provider_messages(&request, false);

        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[1].parts,
            vec![MessagePart::Reasoning("checking".into())]
        );
        assert_eq!(
            messages[2].parts,
            vec![MessagePart::Text("partial prose".into())]
        );
        assert_eq!(
            messages[3].parts,
            vec![MessagePart::Text("continue".into())]
        );
        assert!(messages.iter().all(|message| message.role != Role::Tool));
    }

    #[test]
    fn provider_history_omits_every_occurrence_of_a_reused_tool_call_id() {
        let mut request = bare_request();
        request.prompt = "continue".into();
        request.history = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("write once".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::ToolCall {
                    id: "reused".into(),
                    name: "native::write".into(),
                    input: r#"{"path":"a","content":"first"}"#.into(),
                }],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "reused".into(),
                    content: "wrote a".into(),
                    is_error: false,
                }],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::ToolCall {
                    id: "reused".into(),
                    name: "native::write".into(),
                    input: r#"{"path":"a","content":"second"}"#.into(),
                }],
            },
        ];

        let messages = provider_messages(&request, false);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(
            messages[1].parts,
            vec![MessagePart::Text("continue".into())]
        );
    }

    #[test]
    fn session_bypass_widens_the_request_to_allow_all() {
        let context = SessionContext {
            bypass_permissions: true,
            ..SessionContext::fresh()
        };

        let request = apply_session_to_request(&context, bare_request());

        assert!(request.dangerously_allow_all);
    }

    #[test]
    fn session_bypass_never_narrows_a_request_that_already_allows_all() {
        let context = SessionContext {
            bypass_permissions: false,
            ..SessionContext::fresh()
        };
        let request = HeadlessChatRequest {
            dangerously_allow_all: true,
            ..bare_request()
        };

        let request = apply_session_to_request(&context, request);

        assert!(request.dangerously_allow_all);
    }
}
