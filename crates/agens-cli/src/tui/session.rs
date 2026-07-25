use agens_core::{AgentDefinition, AttemptKey, Message, SessionAttemptStatus, SessionMetadata};
use agens_store::{SessionStore, StoredSession};
use agens_tools::{AgentModelValidator, EffectiveCapabilitySet, ToolDispatcher};
use agens_tui::{Conversation, DialogEntry, DialogView};

use crate::TuiProvider;
use crate::headless::HeadlessChatRequest;
use crate::model_registry::TuiModelSelector;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TuiSessionContext {
    pub(crate) identifier: Option<i64>,
    pub(crate) metadata: Option<SessionMetadata>,
    pub(crate) messages: Vec<Message>,
    pub(crate) restored_history: Vec<Conversation>,
    pub(crate) active_agent: Option<ActiveAgentRuntime>,
    pub(crate) pending_system_reminder: Option<String>,
    pub(crate) selection: Option<TuiModelSelector>,
    pub(crate) provider: Option<TuiProvider>,
    pub(crate) chatgpt_unavailable: bool,
    pub(crate) resume_error: Option<String>,
    pub(crate) resume_notice: Option<String>,
    pub(crate) agent_correction_pending: bool,
    pub(crate) resume_draft: Option<ResumeDraft>,
    pub(crate) selected_subagent: Option<String>,
    pub(crate) dangerous_mode: bool,
    pub(crate) running: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ResumeDraft(String);

impl ResumeDraft {
    pub(crate) fn new(prompt: String) -> Self {
        Self(prompt)
    }

    pub(crate) fn into_inner(self) -> String {
        self.0
    }
}

impl std::ops::Deref for ResumeDraft {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Debug for ResumeDraft {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ResumeDraft([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletedSubagentTurn {
    pub(crate) id: u64,
    pub(crate) agent: String,
    pub(crate) task: String,
    pub(crate) final_result: String,
    pub(crate) tool_uses: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TuiSessionMutationError {
    Busy,
}

pub(crate) fn reset_tui_session(
    context: &mut TuiSessionContext,
) -> Result<(), TuiSessionMutationError> {
    if context.running {
        return Err(TuiSessionMutationError::Busy);
    }

    *context = TuiSessionContext::fresh();
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActiveAgentRuntime {
    pub(crate) name: String,
    pub(crate) model: Option<String>,
    pub(crate) system_prompt: String,
    pub(crate) capabilities: EffectiveCapabilitySet,
}
impl ActiveAgentRuntime {
    pub(crate) fn build(
        agent: &AgentDefinition,
        inherited_model: Option<&str>,
        project: &str,
        dispatcher: &ToolDispatcher,
        validator: &dyn AgentModelValidator,
    ) -> Result<Self, AgentRotationError> {
        if agent
            .model
            .as_deref()
            .is_some_and(|model| validator.validate_model(model).is_err())
        {
            return Err(AgentRotationError::ModelUnavailable);
        }
        let model = agent
            .model
            .as_deref()
            .or(inherited_model)
            .map(str::to_owned);
        Ok(Self {
            name: agent.name.clone(),
            model,
            system_prompt: agent.system_prompt.clone(),
            capabilities: EffectiveCapabilitySet::from_agent(agent, project, dispatcher),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentRotationError {
    Busy,
    ModelUnavailable,
    Persistence,
}
pub(crate) fn rotate_active_agent(
    context: &mut TuiSessionContext,
    candidate: &AgentDefinition,
    inherited_model: Option<&str>,
    project: &str,
    dispatcher: &ToolDispatcher,
    validator: &dyn AgentModelValidator,
    store: Option<&mut SessionStore>,
) -> Result<(), AgentRotationError> {
    if context.running {
        return Err(AgentRotationError::Busy);
    }
    let next =
        ActiveAgentRuntime::build(candidate, inherited_model, project, dispatcher, validator)?;
    let reminder = context.active_agent.as_ref().and_then(|current| {
        next.capabilities
            .is_expansion_from(&current.capabilities)
            .then(|| {
                format!(
                    "Agent capabilities expanded: {} -> {}.",
                    current.name, next.name
                )
            })
    });

    let metadata = match (&context.metadata, store) {
        (Some(metadata), Some(store)) => {
            let mut metadata = metadata.clone();
            metadata.active_agent = next.name.clone();
            metadata.updated_at = session_timestamp().ok_or(AgentRotationError::Persistence)?;
            store
                .update_session(&metadata)
                .map_err(|_| AgentRotationError::Persistence)?;
            Some(metadata)
        }
        (Some(_), None) => return Err(AgentRotationError::Persistence),
        (None, _) => None,
    };

    context.active_agent = Some(next);
    context.metadata = metadata;
    if reminder.is_some() {
        context.pending_system_reminder = reminder;
    }

    Ok(())
}

fn session_timestamp() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
}

pub(crate) fn current_session_timestamp() -> i64 {
    session_timestamp().unwrap_or_default()
}

pub(crate) fn parse_recovery_action(action_id: &str) -> Option<AttemptKey> {
    let mut parts = action_id.split(':');
    let (Some("session"), Some("recover"), Some(session_id), Some(attempt_id), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };

    AttemptKey::new(session_id.parse().ok()?, attempt_id.parse().ok()?).ok()
}

pub(crate) fn session_dialog_entry(
    session: &StoredSession,
    current_session: Option<i64>,
    all_projects: bool,
    now: i64,
) -> DialogEntry {
    let metadata = &session.metadata;
    let age = session_relative_age(metadata.updated_at, now);
    let turns = if metadata.completed_turn_count == 1 {
        "1 turn".to_owned()
    } else {
        format!("{} turns", metadata.completed_turn_count)
    };
    let current = (current_session == Some(metadata.id)).then_some(" · current");
    let root = all_projects.then(|| format!(" · root={}", compact_session_root(&metadata.project)));
    let attempt_status = session
        .latest_attempt
        .as_ref()
        .map(|attempt| {
            format!(
                " · Attempt: {}",
                session_attempt_status_label(attempt.status())
            )
        })
        .unwrap_or_default();
    let row_detail = format!("{turns} · {age}");
    let selected_detail = format!(
        "Turns: {} · Agent: {}{}\nProvider: {} · Model: {}\nEffort: {} · Updated: {} ({}){} · ID: {} · {}{}",
        metadata.completed_turn_count,
        metadata.active_agent,
        current.unwrap_or_default(),
        metadata.provider_id.as_deref().unwrap_or("current runtime"),
        metadata.model_id.as_deref().unwrap_or("current runtime"),
        metadata
            .reasoning_effort
            .map(agens_core::ReasoningEffort::as_str)
            .unwrap_or_else(|| {
                if metadata.provider_id.is_some() || metadata.model_id.is_some() {
                    "Default"
                } else {
                    "current runtime"
                }
            }),
        metadata.updated_at,
        age,
        root.as_deref().unwrap_or_default(),
        metadata.id,
        metadata.title,
        attempt_status,
    );

    DialogEntry::action_with_metadata(
        format!("#{} {}", metadata.id, metadata.title),
        row_detail,
        format!(
            "{} {} {} {}",
            metadata.id, metadata.title, metadata.project, metadata.active_agent
        ),
        selected_detail,
        format!("session:{}", metadata.id),
    )
}

fn session_attempt_status_label(status: agens_core::SessionAttemptStatus) -> &'static str {
    match status {
        agens_core::SessionAttemptStatus::Running => "running",
        agens_core::SessionAttemptStatus::Completed => "completed",
        agens_core::SessionAttemptStatus::Cancelled => "cancelled",
        agens_core::SessionAttemptStatus::Failed => "failed",
        agens_core::SessionAttemptStatus::ProviderError => "provider error",
        agens_core::SessionAttemptStatus::Interrupted => "interrupted",
    }
}

pub(crate) fn resume_retry_notice(status: SessionAttemptStatus) -> Option<&'static str> {
    match status {
        SessionAttemptStatus::Cancelled
        | SessionAttemptStatus::Interrupted
        | SessionAttemptStatus::Failed
        | SessionAttemptStatus::ProviderError => {
            Some("Recovered failed prompt · Enter retry · Esc discard")
        }
        SessionAttemptStatus::Running | SessionAttemptStatus::Completed => None,
    }
}

pub(crate) fn recovery_confirmation_dialog(
    metadata: &SessionMetadata,
    attempt: &agens_core::SessionAttemptSummary,
    refusal: Option<&str>,
) -> DialogView {
    let mut help = format!(
        "Session: {} · ID: {}\nStatus: running\nStarted: {}\nThis may invalidate an attempt still running in another process.",
        metadata.title,
        metadata.id,
        attempt.started_at(),
    );
    if let Some(refusal) = refusal {
        help.push('\n');
        help.push_str(refusal);
    }

    DialogView::selection(
        "Recover interrupted attempt",
        Some(help),
        vec![
            DialogEntry::action(
                "Recover interrupted attempt",
                format!(
                    "session:recover:{}:{}",
                    attempt.key().session_id(),
                    attempt.key().attempt_id()
                ),
            ),
            DialogEntry::cancel("Cancel"),
        ],
    )
}

fn compact_session_root(root: &str) -> String {
    const MAX_CHARS: usize = 30;
    let character_count = root.chars().count();
    if character_count <= MAX_CHARS {
        return root.into();
    }

    format!(
        "...{}",
        root.chars()
            .skip(character_count - MAX_CHARS)
            .collect::<String>()
    )
}

pub(crate) fn session_relative_age(updated_at: i64, now: i64) -> String {
    let age = now.saturating_sub(updated_at);
    match age {
        ..=0 => "now".into(),
        1..=59 => format!("{age}s ago"),
        60..=3_599 => format!("{}m ago", age / 60),
        3_600..=86_399 => format!("{}h ago", age / 3_600),
        _ => format!("{}d ago", age / 86_400),
    }
}

impl TuiSessionContext {
    pub(crate) fn fresh() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn resumed(
        identifier: i64,
        metadata: SessionMetadata,
        messages: Vec<Message>,
        active_agent: ActiveAgentRuntime,
    ) -> Self {
        Self {
            identifier: Some(identifier),
            metadata: Some(metadata),
            messages,
            restored_history: Vec::new(),
            active_agent: Some(active_agent),
            pending_system_reminder: None,
            selection: None,
            provider: None,
            chatgpt_unavailable: false,
            resume_error: None,
            resume_notice: None,
            agent_correction_pending: false,
            resume_draft: None,
            selected_subagent: None,
            dangerous_mode: false,
            running: false,
        }
    }

    pub(crate) fn restored(
        identifier: i64,
        metadata: SessionMetadata,
        messages: Vec<Message>,
        restored_history: Vec<Conversation>,
    ) -> Self {
        Self {
            identifier: Some(identifier),
            metadata: Some(metadata),
            messages,
            restored_history,
            active_agent: None,
            pending_system_reminder: None,
            selection: None,
            provider: None,
            chatgpt_unavailable: false,
            resume_error: None,
            resume_notice: None,
            agent_correction_pending: false,
            resume_draft: None,
            selected_subagent: None,
            dangerous_mode: false,
            running: false,
        }
    }

    pub(crate) fn note(&self) -> String {
        if let Some(notice) = &self.resume_notice {
            return notice.clone();
        }
        if let Some(error) = &self.resume_error {
            return error.clone();
        }
        let identifier = self
            .identifier
            .expect("resumed TUI session context always has an identifier");
        let metadata = self
            .metadata
            .as_ref()
            .expect("resumed TUI session context always has metadata");
        format!(
            "Resumed session {identifier}: agent={} turns={}",
            metadata.active_agent, metadata.completed_turn_count
        )
    }

    pub(crate) fn apply_to(&self, mut request: HeadlessChatRequest) -> HeadlessChatRequest {
        request.dangerous_mode = self.dangerous_mode;
        if self.identifier.is_some() {
            request.history = self.messages.clone();
            request.session = self.metadata.clone();
        }

        let selected_model = self.selection.as_ref().map(|selection| {
            request.model = Some(selection.model().to_owned());
            request.request_config = selection.request_config().clone();
            request.session_reasoning_effort = selection.reasoning_effort_value();
            selection.model()
        });
        if let Some(agent) = &self.active_agent {
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
        request.pending_system_reminder = self.pending_system_reminder.clone();

        request
    }
}
