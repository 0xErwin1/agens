//! Model and reasoning-effort selection for the TUI: applying a selection to
//! the active session (persisting it to the sessions/preference stores),
//! seeding a fresh session from the last remembered selection, and rendering
//! model metadata for `/model` and `/effort` command responses.

use agens_session::model::{model_source, resolved_provider};
use std::sync::{Arc, Mutex};

use agens_store::{ModelPreference, PreferenceStore, SessionStore};

use agens_bootstrap::Bootstrap;
use agens_error::{CliError, ExitStatus};
use agens_models::ModelSelection;
use agens_session::context::SessionContext;
use agens_session::model::current_provider;
use agens_session::provider::ProviderKind;

pub fn apply_tui_selection(
    bootstrap: &Bootstrap,
    context: &mut SessionContext,
    provider: ProviderKind,
    selector: ModelSelection,
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
pub fn seed_remembered_tui_selection(
    bootstrap: &Bootstrap,
    context: &mut SessionContext,
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
    let source = model_source(bootstrap, context);
    let default = resolved_provider(bootstrap, context).default_model();
    let mut selector = ModelSelection::for_source(default, source);
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

pub fn format_model_metadata(model: &agens_models::ModelMetadata) -> String {
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

pub fn format_token_count(tokens: u64) -> String {
    if tokens.is_multiple_of(1_000) {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

pub fn select_tui_model(
    bootstrap: &Bootstrap,
    command: &str,
    session: &Arc<Mutex<SessionContext>>,
) -> Result<String, CliError> {
    let model = command.strip_prefix("/model").unwrap_or_default().trim();
    if model.is_empty() {
        let context = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        let selector = ModelSelection::for_source("gpt-4.1", model_source(bootstrap, &context));
        let values = selector
            .model_values()
            .map_err(CliError::unavailable)?
            .join(", ");
        let current = context
            .selection
            .as_ref()
            .map(|selection| selection.model())
            .or_else(|| bootstrap.model())
            .unwrap_or_else(|| resolved_provider(bootstrap, &context).default_model());
        return Ok(format!("Model: {current}. Available: {values}."));
    }

    apply_tui_model(bootstrap, model, session)
}

pub fn apply_tui_model(
    bootstrap: &Bootstrap,
    model: &str,
    session: &Arc<Mutex<SessionContext>>,
) -> Result<String, CliError> {
    let mut context = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    let mut selector = context
        .selection
        .clone()
        .unwrap_or_else(|| ModelSelection::for_source(model, model_source(bootstrap, &context)));
    let previous_effort = selector.reasoning_effort();
    selector
        .apply_model(model)
        .map_err(CliError::configuration)?;
    let reset_effort = previous_effort.filter(|_| selector.reasoning_effort().is_none());
    let provider = current_provider(bootstrap, &context)
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

pub fn apply_tui_unverified_model(
    bootstrap: &Bootstrap,
    model: &str,
    session: &Arc<Mutex<SessionContext>>,
) -> Result<String, CliError> {
    let mut context = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    let mut selector = context
        .selection
        .clone()
        .unwrap_or_else(|| ModelSelection::for_source(model, model_source(bootstrap, &context)));
    let reset_effort = selector.reasoning_effort().is_some();
    selector
        .apply_unverified_model(model)
        .map_err(CliError::configuration)?;
    let provider = current_provider(bootstrap, &context)
        .ok_or_else(|| CliError::configuration("TUI provider is unavailable"))?;
    apply_tui_selection(bootstrap, &mut context, provider, selector)?;

    Ok(if reset_effort {
        format!("Model: {model} (unverified metadata). Reasoning effort reset to Default.")
    } else {
        format!("Model: {model} (unverified metadata).")
    })
}

pub fn select_tui_effort(
    bootstrap: &Bootstrap,
    command: &str,
    session: &Arc<Mutex<SessionContext>>,
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

pub fn apply_tui_effort(
    bootstrap: &Bootstrap,
    effort: &str,
    session: &Arc<Mutex<SessionContext>>,
) -> Result<String, CliError> {
    let mut context = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    let model = context
        .selection
        .as_ref()
        .map(|selection| selection.model())
        .or_else(|| bootstrap.model())
        .unwrap_or_else(|| resolved_provider(bootstrap, &context).default_model());
    let mut selector = ModelSelection::for_source(model, model_source(bootstrap, &context));
    selector
        .apply_reasoning_effort(effort)
        .map_err(CliError::configuration)?;
    let provider = current_provider(bootstrap, &context)
        .ok_or_else(|| CliError::configuration("TUI provider is unavailable"))?;
    apply_tui_selection(bootstrap, &mut context, provider, selector)?;
    let effort = if effort == "default" {
        "Default"
    } else {
        effort
    };
    Ok(format!("Reasoning effort: {effort}."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{tui_session_bootstrap, tui_session_directory};

    fn remember(bootstrap: &Bootstrap, model: &str, effort: Option<agens_core::ReasoningEffort>) {
        PreferenceStore::open(bootstrap.data_directory())
            .unwrap()
            .remember_model(&ModelPreference::new(model, effort))
            .unwrap();
    }

    #[test]
    fn a_configured_or_flagged_model_outranks_the_remembered_one() {
        let temporary = tui_session_directory("remembered-selection-outranked");
        let configured = tui_session_bootstrap(&temporary, &[]);
        remember(
            &configured,
            "gpt-5.5",
            Some(agens_core::ReasoningEffort::High),
        );
        let mut context = SessionContext::fresh();

        assert_eq!(
            seed_remembered_tui_selection(&configured, &mut context),
            None
        );
        assert!(context.selection.is_none());
        assert_eq!(
            agens_session::model::effective_model(&configured, &context),
            "gpt-4.1"
        );

        // A model flag reaches the same resolved slot as a configured model, so it outranks the
        // remembered pick through the same branch.
        let mut flagged = configured.clone();
        flagged.model = Some("o3".into());
        let mut context = SessionContext::fresh();

        assert_eq!(seed_remembered_tui_selection(&flagged, &mut context), None);
        assert!(context.selection.is_none());
        assert_eq!(
            agens_session::model::effective_model(&flagged, &context),
            "o3"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn an_unavailable_remembered_model_falls_back_visibly() {
        let temporary = tui_session_directory("remembered-selection-unavailable");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.model = None;
        remember(&bootstrap, "gpt-5.4", None);
        let mut context = SessionContext::fresh();

        assert_eq!(
            seed_remembered_tui_selection(&bootstrap, &mut context),
            Some(
                "Remembered model gpt-5.4 is unavailable for OpenAI API; using gpt-4.1.".to_owned()
            )
        );
        assert!(context.selection.is_none());
        assert_eq!(
            agens_session::model::effective_model(&bootstrap, &context),
            "gpt-4.1"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn an_effort_the_remembered_model_lost_falls_back_visibly() {
        let temporary = tui_session_directory("remembered-selection-effort");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.model = None;
        remember(
            &bootstrap,
            "gpt-4.1",
            Some(agens_core::ReasoningEffort::High),
        );
        let mut context = SessionContext::fresh();

        assert_eq!(
            seed_remembered_tui_selection(&bootstrap, &mut context),
            Some(
                "Remembered reasoning effort is unsupported by gpt-4.1; using Default.".to_owned()
            )
        );
        let selection = context.selection.as_ref().unwrap();
        assert_eq!(selection.model(), "gpt-4.1");
        assert_eq!(selection.reasoning_effort(), None);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn choosing_a_model_and_an_effort_remembers_both_for_the_next_session() {
        let temporary = tui_session_directory("remembered-selection-write");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.model = None;
        let session = Arc::new(Mutex::new(SessionContext::fresh()));

        apply_tui_model(&bootstrap, "gpt-5.5", &session).unwrap();
        apply_tui_effort(&bootstrap, "high", &session).unwrap();

        let remembered = PreferenceStore::open(bootstrap.data_directory())
            .unwrap()
            .remembered_model()
            .unwrap()
            .unwrap();
        assert_eq!(remembered.model(), "gpt-5.5");
        assert_eq!(
            remembered.reasoning_effort(),
            Some(agens_core::ReasoningEffort::High)
        );

        let mut context = SessionContext::fresh();
        assert_eq!(
            seed_remembered_tui_selection(&bootstrap, &mut context),
            None
        );
        assert_eq!(
            agens_session::model::effective_model(&bootstrap, &context),
            "gpt-5.5"
        );
        assert_eq!(
            context
                .selection
                .as_ref()
                .and_then(ModelSelection::reasoning_effort),
            Some("high")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn applying_a_moonshot_model_selects_kimi_k3_and_remembers_it() {
        let temporary = tui_session_directory("moonshot-model-selection");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.model = None;
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        session.lock().unwrap().provider = Some(agens_session::provider::ProviderKind::Moonshot);

        apply_tui_model(&bootstrap, "kimi-k3", &session).unwrap();

        let context = session.lock().unwrap();
        let selection = context.selection.as_ref().unwrap();
        assert_eq!(selection.model(), "kimi-k3");
        assert_eq!(selection.source_label(), "Moonshot AI");
        drop(context);

        let remembered = PreferenceStore::open(bootstrap.data_directory())
            .unwrap()
            .remembered_model()
            .unwrap()
            .unwrap();
        assert_eq!(remembered.model(), "kimi-k3");

        std::fs::remove_dir_all(temporary).unwrap();
    }
}
