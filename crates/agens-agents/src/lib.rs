//! Agents: which ones a session may use, which models each one can run, and
//! which one a session is running right now.
//!
//! Agent definitions come from configuration, so almost everything here is a
//! resolution against something that may have changed since it was written.

mod active;
mod catalog;
mod models;
mod resolver;

pub use active::{
    PersistedAgentResolution, PersistedAgentResolutionError, ensure_active_agent_runtime,
    initial_active_agent_name, persist_pending_agent_correction, persisted_agent_resolution_error,
    reconcile_persisted_active_agent, resolve_persisted_active_agent,
};
pub use catalog::{
    agent_catalog, agent_catalog_for_context, agent_rotation_error, discover_agent_catalog,
    select_subagent, subagent_catalog, task_agent_catalog,
};
pub use models::{AgentModelCompatibility, TaskModelValidator, task_model_catalog};
pub use resolver::{
    AgentProfileResolver, ProfileOrigin, ResolvedAgentProfile, ResolvedProfileValue,
};
