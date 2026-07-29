//! Turning a session into the request a provider receives: its messages, its
//! reasoning effort, and the delegation instruction appended to the prompt.

use std::sync::Arc;

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
    let mut messages = request.history.clone();
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
