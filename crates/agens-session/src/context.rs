//! Session state, held apart from any surface that renders it.
//!
//! This is what a session *is* — identity, metadata, confinement root, history,
//! active agent — as opposed to what a surface shows about it. It still carries
//! a few view-only fields and one rendering type; those move out next, and until
//! they do this module is not yet listed in the surface-boundary check.

use agens_core::{AgentDefinition, Message, SessionMetadata};
use agens_store::SessionStore;
use agens_tools::{AgentModelValidator, EffectiveCapabilitySet, ToolDispatcher};

use crate::provider::ProviderKind;
use agens_models::ModelSelection;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionContext {
    pub identifier: Option<i64>,
    pub metadata: Option<SessionMetadata>,
    /// The confinement root recorded on a resumed session, read back from its own persisted
    /// `confinement_root` rather than re-derived from the current process's working directory.
    /// `None` for a session that has not been created yet; see
    /// [`crate::session::root::resolve_tui_session_root`] for the fallback that applies then.
    pub confinement_root: Option<std::path::PathBuf>,
    pub messages: Vec<Message>,
    pub active_agent: Option<ActiveAgentRuntime>,
    pub pending_system_reminder: Option<String>,
    pub selection: Option<ModelSelection>,
    pub provider: Option<ProviderKind>,
    pub chatgpt_unavailable: bool,
    pub resume_error: Option<String>,
    pub resume_notice: Option<String>,
    pub agent_correction_pending: bool,
    pub resume_draft: Option<ResumeDraft>,
    pub selected_subagent: Option<String>,
    pub dangerous_mode: bool,
    /// Whether `Ask` permission prompts are bypassed for this session. Seeded from
    /// `agent.bypass_permission_prompts` (global configuration only) for a brand-new session, but
    /// carried as the session's OWN recorded value on resume rather than re-seeded — see
    /// [`crate::root::resolve_tui_session_root`]'s sibling in `agens-bootstrap` for the
    /// configuration read, and `agens-store`'s `bypass_permission_prompts` accessor for the
    /// persisted value a resume reads back.
    pub bypass_permissions: bool,
    pub running: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ResumeDraft(String);

impl ResumeDraft {
    pub fn new(prompt: String) -> Self {
        Self(prompt)
    }

    pub fn into_inner(self) -> String {
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
pub struct ActiveAgentRuntime {
    pub name: String,
    pub model: Option<String>,
    pub system_prompt: String,
    pub capabilities: EffectiveCapabilitySet,
}
impl ActiveAgentRuntime {
    pub fn build(
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedSubagentTurn {
    pub id: u64,
    pub agent: String,
    pub task: String,
    pub final_result: String,
    pub tool_uses: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionMutationError {
    Busy,
}

pub fn reset_session(context: &mut SessionContext) -> Result<(), SessionMutationError> {
    if context.running {
        return Err(SessionMutationError::Busy);
    }

    *context = SessionContext::fresh();
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRotationError {
    Busy,
    ModelUnavailable,
    Persistence,
}
pub fn rotate_active_agent(
    context: &mut SessionContext,
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

pub fn current_session_timestamp() -> i64 {
    session_timestamp().unwrap_or_default()
}

impl SessionContext {
    pub fn fresh() -> Self {
        Self::default()
    }

    /// A resumed context assembled directly. Not test-gated: its callers are
    /// tests in another crate, which `cfg(test)` cannot reach.
    pub fn resumed(
        identifier: i64,
        metadata: SessionMetadata,
        messages: Vec<Message>,
        active_agent: ActiveAgentRuntime,
        confinement_root: std::path::PathBuf,
    ) -> Self {
        Self {
            identifier: Some(identifier),
            metadata: Some(metadata),
            confinement_root: Some(confinement_root),
            messages,
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
            bypass_permissions: false,
            running: false,
        }
    }

    /// Builds a session context for a session that was just resumed from storage.
    ///
    /// `confinement_root` is required, not optional: a resumed session's tools must always be
    /// confined to the root recorded for it, and making the parameter mandatory here rules out a
    /// caller silently constructing a "resumed" context that falls back to the process's own
    /// discovered root through [`crate::session::root::resolve_tui_session_root`]'s `None` branch.
    pub fn restored(
        identifier: i64,
        metadata: SessionMetadata,
        messages: Vec<Message>,
        confinement_root: std::path::PathBuf,
    ) -> Self {
        Self {
            identifier: Some(identifier),
            metadata: Some(metadata),
            confinement_root: Some(confinement_root),
            messages,
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
            bypass_permissions: false,
            running: false,
        }
    }

    pub fn note(&self) -> String {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bypass_permissions_defaults_to_false_for_a_fresh_session() {
        assert!(!SessionContext::fresh().bypass_permissions);
    }

    #[test]
    fn bypass_permissions_defaults_to_false_for_a_restored_session() {
        let metadata = SessionMetadata {
            id: 1,
            project: "project".into(),
            title: "title".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: true,
        };
        let restored =
            SessionContext::restored(1, metadata, Vec::new(), std::path::PathBuf::from("project"));

        assert!(!restored.bypass_permissions);
    }
}
