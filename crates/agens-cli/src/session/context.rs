//! Session state, held apart from any surface that renders it.
//!
//! This is what a session *is* — identity, metadata, confinement root, history,
//! active agent — as opposed to what a surface shows about it. It still carries
//! a few view-only fields and one rendering type; those move out next, and until
//! they do this module is not yet listed in the surface-boundary check.

use agens_core::{AgentDefinition, Message, SessionMetadata};
use agens_tools::{AgentModelValidator, EffectiveCapabilitySet, ToolDispatcher};
use agens_tui::Conversation;

use crate::model_registry::TuiModelSelector;
use crate::tui::provider::TuiProvider;
use crate::tui::session::AgentRotationError;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionContext {
    pub(crate) identifier: Option<i64>,
    pub(crate) metadata: Option<SessionMetadata>,
    /// The confinement root recorded on a resumed session, read back from its own persisted
    /// `confinement_root` rather than re-derived from the current process's working directory.
    /// `None` for a session that has not been created yet; see
    /// [`crate::session_root::resolve_tui_session_root`] for the fallback that applies then.
    pub(crate) confinement_root: Option<std::path::PathBuf>,
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
