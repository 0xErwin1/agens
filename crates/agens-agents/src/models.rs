//! Which models an agent may run.
//!
//! Two validators, because the question has two forms: what the session's
//! provider currently supports, and what a delegated task was handed.

use std::collections::BTreeSet;
use std::sync::Arc;

use agens_bootstrap::Bootstrap;
use agens_error::CliError;
use agens_models::{ModelSelection, ModelSource};
use agens_models::{default_model, unknown_provider_message};
use agens_session::context::SessionContext;
use agens_session::model::model_source;
use agens_session::provider::ProviderKind;
use agens_tools::AgentModelValidator;

#[derive(Clone)]
pub struct AgentModelCompatibility {
    available: Arc<BTreeSet<String>>,
}

impl AgentModelCompatibility {
    pub fn for_source(source: ModelSource) -> Result<Self, CliError> {
        let available = ModelSelection::for_source("gpt-4.1", source)
            .model_values()
            .map_err(CliError::unavailable)?
            .into_iter()
            .collect();
        Ok(Self {
            available: Arc::new(available),
        })
    }

    pub fn for_context(bootstrap: &Bootstrap, context: &SessionContext) -> Result<Self, CliError> {
        Self::for_source(model_source(bootstrap, context))
    }

    pub fn is_available(&self, model: &str) -> bool {
        self.available.contains(model)
    }
}

impl AgentModelValidator for AgentModelCompatibility {
    fn validate_model(&self, model: &str) -> Result<(), agens_tools::AgentModelValidationError> {
        self.is_available(model)
            .then_some(())
            .ok_or(agens_tools::AgentModelValidationError::Unavailable)
    }
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

pub fn task_model_catalog(bootstrap: &Bootstrap) -> Result<Vec<String>, CliError> {
    let source = bootstrap
        .provider_type()
        .and_then(ProviderKind::parse)
        .map(ProviderKind::source)
        .ok_or_else(|| CliError::configuration("task provider is unavailable"))?;
    let model = default_model(bootstrap.provider_type()).ok_or_else(|| {
        CliError::configuration(unknown_provider_message(bootstrap.provider_type()))
    })?;

    ModelSelection::for_source(model, source)
        .model_values()
        .map_err(CliError::unavailable)
}
