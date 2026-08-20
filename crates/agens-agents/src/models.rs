//! Which models an agent may run.
//!
//! Two validators, because the question has two forms: what the session's
//! provider currently supports, and what a delegated task was handed.

use std::collections::BTreeSet;
use std::sync::Arc;

use agens_bootstrap::Bootstrap;
use agens_error::CliError;
use agens_models::{ModelSelection, ModelSource, QualifiedModel};
use agens_session::context::SessionContext;
use agens_session::provider::{authenticated_sources, bootstrap_authentication};
use agens_tools::{AgentModelValidationError, AgentModelValidator};

#[derive(Clone)]
pub struct AgentModelCompatibility {
    authenticated: Arc<Vec<ModelSource>>,
}

impl AgentModelCompatibility {
    /// A validator over exactly one provider. Kept for callers that already
    /// know which provider a run reached.
    pub fn for_source(source: ModelSource) -> Result<Self, CliError> {
        Self::for_authenticated(vec![source])
    }

    /// A validator over every provider this run can authenticate.
    ///
    /// An agent names its own provider through its model identifier, so a
    /// catalog may legitimately mix them; validating against one provider
    /// would reject exactly the agents that mixing exists for.
    pub fn for_authenticated(authenticated: Vec<ModelSource>) -> Result<Self, CliError> {
        Ok(Self {
            authenticated: Arc::new(authenticated),
        })
    }

    /// A validator for a live session.
    ///
    /// Built from what this run can authenticate rather than from the session's
    /// own provider: an agent names its provider through its model, and the
    /// session's is not the only one it may name.
    pub fn for_context(bootstrap: &Bootstrap, _: &SessionContext) -> Result<Self, CliError> {
        Self::for_authenticated(authenticated_sources(&bootstrap_authentication(bootstrap)))
    }

    pub fn is_available(&self, model: &str) -> bool {
        self.validate_model(model).is_ok()
    }
}

impl AgentModelValidator for AgentModelCompatibility {
    /// Accepts a `provider/model` identifier when that provider is
    /// authenticated, and a bare one when exactly one authenticated provider
    /// serves it.
    fn validate_model(&self, model: &str) -> Result<(), AgentModelValidationError> {
        let Ok(parsed) = QualifiedModel::parse(model) else {
            return Err(AgentModelValidationError::Unavailable);
        };

        // A prefix names the provider outright, so the bundled catalog does not
        // get to decide whether the model exists.
        if let Some(named) = parsed.source() {
            return self.authenticated.contains(&named).then_some(()).ok_or(
                AgentModelValidationError::Unreachable {
                    provider: named.provider_type(),
                },
            );
        }

        let serving = agens_models::sources_serving(parsed.model());
        if serving.is_empty() {
            return Err(AgentModelValidationError::Unavailable);
        }

        let reachable: Vec<ModelSource> = serving
            .iter()
            .copied()
            .filter(|source| self.authenticated.contains(source))
            .collect();

        match (reachable.as_slice(), serving.as_slice()) {
            ([_], _) => Ok(()),
            ([], [unreachable, ..]) => Err(AgentModelValidationError::Unreachable {
                provider: unreachable.provider_type(),
            }),
            _ => Err(AgentModelValidationError::Ambiguous {
                candidates: provider_names(&reachable),
            }),
        }
    }
}

fn provider_names(sources: &[ModelSource]) -> String {
    sources
        .iter()
        .map(|source| format!("\"{}\"", source.provider_type()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone)]
pub struct TaskModelValidator {
    available: Arc<BTreeSet<String>>,
}

impl TaskModelValidator {
    pub fn new(models: &[String]) -> Self {
        Self {
            available: Arc::new(models.iter().cloned().collect()),
        }
    }
}

impl AgentModelValidator for TaskModelValidator {
    fn validate_model(&self, model: &str) -> Result<(), agens_tools::AgentModelValidationError> {
        self.available
            .contains(model)
            .then_some(())
            .ok_or(agens_tools::AgentModelValidationError::Unavailable)
    }
}

/// Every model a delegated task may be given: the union of the catalogs of
/// every provider this run can authenticate, each listed bare and qualified.
///
/// The qualified form has to be listed too, because a subagent profile names
/// its provider that way and an unlisted identifier silently falls back to the
/// parent's model.
pub fn task_model_catalog(bootstrap: &Bootstrap) -> Result<Vec<String>, CliError> {
    let sources = authenticated_sources(&bootstrap_authentication(bootstrap));
    if sources.is_empty() {
        return Err(CliError::configuration("task provider is unavailable"));
    }

    let mut models = Vec::new();
    for source in sources {
        let listed = ModelSelection::for_source(default_model_for(source), source)
            .model_values()
            .map_err(CliError::unavailable)?;

        for model in listed {
            models.push(format!("{}/{model}", source.provider_type()));
            models.push(model);
        }
    }
    models.sort_unstable();
    models.dedup();

    Ok(models)
}

const fn default_model_for(source: ModelSource) -> &'static str {
    match source {
        ModelSource::OpenAiApi => "gpt-4.1",
        ModelSource::ChatGptSubscription => "gpt-5.5",
        ModelSource::MoonshotApi => "kimi-k3",
    }
}
