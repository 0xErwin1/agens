//! Model and reasoning-effort selection for the TUI: applying a selection to
//! the active session (persisting it to the sessions/preference stores),
//! seeding a fresh session from the last remembered selection, and rendering
//! model metadata for `/model` and `/effort` command responses.

use std::sync::{Arc, Mutex};

use agens_store::{ModelPreference, PreferenceStore, SessionStore};

use crate::bootstrap::Bootstrap;
use crate::error::{CliError, ExitStatus};
use crate::model_registry;
use crate::model_registry::{TuiModelSelector, TuiModelSource};
use crate::tools::task::default_model;
use crate::tui::provider::TuiProvider;
use crate::tui::session::TuiSessionContext;
use crate::tui::turn::current_tui_provider;

pub(crate) fn apply_tui_selection(
    bootstrap: &Bootstrap,
    context: &mut TuiSessionContext,
    provider: TuiProvider,
    selector: TuiModelSelector,
) -> Result<(), CliError> {
    if let Some(mut metadata) = context.metadata.clone() {
        metadata.provider_id = Some(provider.identifier().into());
        metadata.model_id = Some(selector.model().into());
        metadata.reasoning_effort = selector.reasoning_effort_value();
        SessionStore::open(bootstrap.data_directory())
            .and_then(|mut store| store.update_session_selection(&metadata))
            .map_err(|_| CliError::storage("session selection could not be saved"))?;
        context.metadata = Some(metadata);
    }
    PreferenceStore::open(bootstrap.data_directory())
        .and_then(|mut store| {
            store.remember_model(&ModelPreference::new(
                selector.model(),
                selector.reasoning_effort_value(),
            ))
        })
        .map_err(|_| CliError::storage("model preference could not be saved"))?;
    context.provider = Some(provider);
    context.selection = Some(selector);
    context.active_agent = None;
    Ok(())
}

/// Resolves the model for a fresh session: a CLI flag or configured model first, then the last
/// remembered selection, then the hardcoded default.
///
/// A model written into configuration by hand is a deliberate statement, so a terminal pick never
/// silently overrides it. Returns the notice the user must see when a remembered selection cannot
/// be honored, because falling back to a different model without saying so is indistinguishable
/// from the preference being ignored.
pub(crate) fn seed_remembered_tui_selection(
    bootstrap: &Bootstrap,
    context: &mut TuiSessionContext,
) -> Option<String> {
    if bootstrap.model().is_some() {
        return None;
    }

    let preference = match PreferenceStore::open(bootstrap.data_directory())
        .and_then(|store| store.remembered_model())
    {
        Ok(Some(preference)) => preference,
        Ok(None) => return None,
        Err(_) => return Some("Remembered model selection could not be read.".to_owned()),
    };
    let source = tui_model_source(bootstrap, context);
    let default = default_model(bootstrap);
    let mut selector = TuiModelSelector::for_source(default, source);
    if selector.apply_model(preference.model()).is_err() {
        return Some(format!(
            "Remembered model {} is unavailable for {}; using {default}.",
            preference.model(),
            source.label()
        ));
    }

    let dropped_effort = preference
        .reasoning_effort()
        .is_some_and(|effort| selector.apply_reasoning_effort(effort.as_str()).is_err());
    let notice = dropped_effort.then(|| {
        format!(
            "Remembered reasoning effort is unsupported by {}; using Default.",
            preference.model()
        )
    });
    context.selection = Some(selector);
    notice
}

pub(crate) fn tui_model_source(
    bootstrap: &Bootstrap,
    context: &TuiSessionContext,
) -> TuiModelSource {
    current_tui_provider(bootstrap, context)
        .unwrap_or(TuiProvider::OpenAiApi)
        .source()
}

pub(crate) fn format_model_metadata(model: &model_registry::ModelMetadata) -> String {
    let context = model
        .context
        .map(format_token_count)
        .unwrap_or_else(|| "?".into());
    let output = model
        .output
        .map(format_token_count)
        .unwrap_or_else(|| "?".into());
    let reasoning = match model.reasoning {
        Some(true) => "reasoning",
        Some(false) => "no reasoning",
        None => "reasoning unknown",
    };
    format!("{context} context | {output} output | {reasoning}")
}

pub(crate) fn format_token_count(tokens: u64) -> String {
    if tokens.is_multiple_of(1_000) {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

pub(crate) fn select_tui_model(
    bootstrap: &Bootstrap,
    command: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let model = command.strip_prefix("/model").unwrap_or_default().trim();
    if model.is_empty() {
        let context = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        let selector =
            TuiModelSelector::for_source("gpt-4.1", tui_model_source(bootstrap, &context));
        let values = selector
            .model_values()
            .map_err(CliError::unavailable)?
            .join(", ");
        let current = context
            .selection
            .as_ref()
            .map(|selection| selection.model())
            .or_else(|| bootstrap.model())
            .unwrap_or_else(|| default_model(bootstrap));
        return Ok(format!("Model: {current}. Available: {values}."));
    }

    apply_tui_model(bootstrap, model, session)
}

pub(crate) fn apply_tui_model(
    bootstrap: &Bootstrap,
    model: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let mut context = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    let mut selector = context.selection.clone().unwrap_or_else(|| {
        TuiModelSelector::for_source(model, tui_model_source(bootstrap, &context))
    });
    let previous_effort = selector.reasoning_effort();
    selector
        .apply_model(model)
        .map_err(CliError::configuration)?;
    let reset_effort = previous_effort.filter(|_| selector.reasoning_effort().is_none());
    let provider = current_tui_provider(bootstrap, &context)
        .ok_or_else(|| CliError::configuration("TUI provider is unavailable"))?;
    apply_tui_selection(bootstrap, &mut context, provider, selector)?;
    Ok(reset_effort.map_or_else(
        || format!("Model: {model}."),
        |effort| {
            format!(
                "Model: {model}. Reasoning effort reset to Default because {effort} is unsupported."
            )
        },
    ))
}

pub(crate) fn apply_tui_unverified_model(
    bootstrap: &Bootstrap,
    model: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let mut context = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    let mut selector = context.selection.clone().unwrap_or_else(|| {
        TuiModelSelector::for_source(model, tui_model_source(bootstrap, &context))
    });
    let reset_effort = selector.reasoning_effort().is_some();
    selector
        .apply_unverified_model(model)
        .map_err(CliError::configuration)?;
    let provider = current_tui_provider(bootstrap, &context)
        .ok_or_else(|| CliError::configuration("TUI provider is unavailable"))?;
    apply_tui_selection(bootstrap, &mut context, provider, selector)?;

    Ok(if reset_effort {
        format!("Model: {model} (unverified metadata). Reasoning effort reset to Default.")
    } else {
        format!("Model: {model} (unverified metadata).")
    })
}

pub(crate) fn select_tui_effort(
    bootstrap: &Bootstrap,
    command: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let effort = command.strip_prefix("/effort").unwrap_or_default().trim();
    let context = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    if effort.is_empty() {
        let current = context
            .selection
            .as_ref()
            .and_then(|selection| selection.reasoning_effort())
            .unwrap_or("default");
        return Ok(format!("Reasoning effort: {current}."));
    }

    drop(context);
    apply_tui_effort(bootstrap, effort, session)
}

pub(crate) fn apply_tui_effort(
    bootstrap: &Bootstrap,
    effort: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let mut context = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    let model = context
        .selection
        .as_ref()
        .map(|selection| selection.model())
        .or_else(|| bootstrap.model())
        .unwrap_or_else(|| default_model(bootstrap));
    let mut selector = TuiModelSelector::for_source(model, tui_model_source(bootstrap, &context));
    selector
        .apply_reasoning_effort(effort)
        .map_err(CliError::configuration)?;
    let provider = current_tui_provider(bootstrap, &context)
        .ok_or_else(|| CliError::configuration("TUI provider is unavailable"))?;
    apply_tui_selection(bootstrap, &mut context, provider, selector)?;
    let effort = if effort == "default" {
        "Default"
    } else {
        effort
    };
    Ok(format!("Reasoning effort: {effort}."))
}
