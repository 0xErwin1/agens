//! Which models an agent may run.
//!
//! Two validators, because the question has two forms: what the session's
//! provider currently supports, and what a delegated task was handed.

use std::collections::BTreeSet;
use std::sync::Arc;

use agens_bootstrap::Bootstrap;
use agens_error::CliError;
use agens_models::{ModelSelection, ModelSource, QualifiedModel};
use agens_models::{default_model, unknown_provider_message};
use agens_session::context::SessionContext;
use agens_session::model::model_source;
use agens_session::provider::ProviderKind;
use agens_tools::{AgentModelValidationError, AgentModelValidator};

#[derive(Clone)]
pub struct AgentModelCompatibility {
    source: ModelSource,
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
            source,
            available: Arc::new(available),
        })
    }

    pub fn for_context(bootstrap: &Bootstrap, context: &SessionContext) -> Result<Self, CliError> {
        Self::for_source(model_source(bootstrap, context))
    }

    pub fn is_available(&self, model: &str) -> bool {
        self.validate_model(model).is_ok()
    }

    /// The provider that would serve `model` if this session were using it, or
    /// `None` when no bundled catalog lists it at all.
    fn served_elsewhere(&self, model: &str) -> Option<ModelSource> {
        agens_models::sources_serving(model)
            .into_iter()
            .find(|source| *source != self.source)
    }

    fn mismatch(&self, requested: ModelSource) -> AgentModelValidationError {
        AgentModelValidationError::ProviderMismatch {
            requested: requested.provider_type(),
            active: self.source.provider_type(),
        }
    }
}

impl AgentModelValidator for AgentModelCompatibility {
    /// Accepts a bare identifier against the session's own provider, and a
    /// `provider/model` identifier only against the provider it names.
    fn validate_model(&self, model: &str) -> Result<(), AgentModelValidationError> {
        let Ok(parsed) = QualifiedModel::parse(model) else {
            return Err(AgentModelValidationError::Unavailable);
        };

        if let Some(requested) = parsed.source().filter(|source| *source != self.source) {
            return Err(if parsed.is_available() {
                self.mismatch(requested)
            } else {
                AgentModelValidationError::Unavailable
            });
        }

        if self.available.contains(parsed.model()) {
            return Ok(());
        }

        Err(match self.served_elsewhere(parsed.model()) {
            Some(requested) => self.mismatch(requested),
            None => AgentModelValidationError::Unavailable,
        })
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
