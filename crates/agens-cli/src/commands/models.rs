//! The `models` command: lists the bundled model catalog.

use crate::error::CliError;
use crate::model_registry;

pub(crate) fn run_models() -> Result<String, CliError> {
    model_registry::bundled_openai_models()
        .map(|models| model_registry::format_models(&models))
        .map_err(|_| CliError::unavailable("model registry is unavailable"))
}
