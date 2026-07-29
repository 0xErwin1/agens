//! Connecting, disconnecting and reconciling the provider a session speaks
//! through.

use agens_session::model::{effective_model, resolved_provider};
use std::fs;

use agens_core::{HeadlessTurnCancellation, HeadlessTurnError};
use agens_providers::chatgpt_login::LoginCancellation;
use agens_tui::TuiRouteProgress;

use crate::models::{apply_tui_model, apply_tui_selection};
use agens_auth::{ChatGptAuthFlow, ChatGptAuthProgress};
use agens_bootstrap::{Bootstrap, ProviderSource, resolve_provider_type};
use agens_error::CliError;
use agens_models::ModelSelection;
use agens_session::provider::{ProviderKind, snapshot_chatgpt_credentials};

use super::{AuthRouteError, TuiRuntimeRouter};

impl TuiRuntimeRouter {
    pub fn connect(
        &self,
        flow: ChatGptAuthFlow,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) -> Result<String, AuthRouteError> {
        let path = self
            .bootstrap()
            .map_err(AuthRouteError::Runtime)?
            .paths
            .credentials;
        let credentials_before =
            snapshot_chatgpt_credentials(&path).map_err(AuthRouteError::Runtime)?;
        let runtime_before = self
            .session
            .lock()
            .map_err(|_| AuthRouteError::Runtime(CliError::storage("TUI session is unavailable")))?
            .clone();
        let operation =
            HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(600));
        *self.cancellation.lock().map_err(|_| {
            AuthRouteError::Runtime(CliError::storage("TUI cancellation is unavailable"))
        })? = Some(operation.clone());
        let view = operation.adapter_view();
        let result = self.auth.login(
            &path,
            flow,
            LoginCancellation::from_shared_flag(view.cancellation_handle()),
            view.deadline()
                .expect("authentication has a fixed deadline"),
            move |event| {
                let event = match event {
                    ChatGptAuthProgress::BrowserUrl(url) => TuiRouteProgress::BrowserUrl(url),
                    ChatGptAuthProgress::DeviceCode {
                        verification_url,
                        user_code,
                    } => TuiRouteProgress::DeviceCode {
                        verification_url,
                        user_code,
                    },
                };
                let _ = progress.send(event);
            },
        );
        if let Ok(mut active) = self.cancellation.lock() {
            *active = None;
        }
        result.map_err(AuthRouteError::Auth)?;
        if let Err(error) = self.reconcile_provider(true) {
            if (self.credential_restorer)(&path, credentials_before).is_err() {
                self.mark_chatgpt_unavailable()
                    .map_err(AuthRouteError::Runtime)?;
                return Err(AuthRouteError::Runtime(CliError::storage(
                    "ChatGPT credential recovery failed",
                )));
            }
            *self.session.lock().map_err(|_| {
                AuthRouteError::Runtime(CliError::storage("TUI session is unavailable"))
            })? = runtime_before;
            return Err(AuthRouteError::Runtime(error));
        }
        Ok("Connected to ChatGPT.".into())
    }

    pub fn disconnect(&self) -> Result<String, AuthRouteError> {
        let path = self
            .bootstrap()
            .map_err(AuthRouteError::Runtime)?
            .paths
            .credentials;
        let removed = self.auth.disconnect(&path).map_err(AuthRouteError::Auth)?;
        if removed {
            if let Err(error) = self.reconcile_provider(false) {
                self.mark_chatgpt_unavailable()
                    .map_err(AuthRouteError::Runtime)?;
                return Err(AuthRouteError::Runtime(error));
            }
            Ok("Disconnected from ChatGPT.".into())
        } else {
            Ok("No ChatGPT credentials were stored.".into())
        }
    }

    pub(super) fn reconcile_provider(&self, connected: bool) -> Result<(), CliError> {
        let bootstrap = self.bootstrap()?;
        match bootstrap.provider_source {
            ProviderSource::Auto => {
                let provider = if connected {
                    "openai-chatgpt".to_owned()
                } else {
                    let credentials = fs::read_to_string(&bootstrap.paths.credentials).ok();
                    resolve_provider_type(
                        None,
                        credentials.as_deref(),
                        &(self.credentials.environment)(),
                    )?
                    .ok_or_else(|| {
                        CliError::authentication(
                            "ChatGPT credentials are unavailable; run /connect",
                        )
                    })?
                };
                self.apply_provider(&bootstrap, &provider)?;
            }
            ProviderSource::ExplicitChatGpt if connected => {
                self.apply_provider(&bootstrap, "openai-chatgpt")?;
            }
            ProviderSource::ExplicitChatGpt => self.mark_chatgpt_unavailable()?,
            ProviderSource::ExplicitOther => {}
        }
        Ok(())
    }

    pub(super) fn mark_chatgpt_unavailable(&self) -> Result<(), CliError> {
        let mut context = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        context.provider = None;
        context.chatgpt_unavailable = true;
        context.active_agent = None;
        Ok(())
    }

    /// Selects a model that may belong to a provider other than the active one.
    ///
    /// Switching first is what makes the model picker able to list every
    /// provider: choosing a model the current provider cannot serve would
    /// otherwise be rejected, or worse, accepted and sent to the wrong account.
    /// The provider switch keeps its own credential check, so picking a model
    /// from a provider you are not signed in to fails there rather than here.
    pub(super) fn apply_provider_model(
        &self,
        bootstrap: &Bootstrap,
        provider: &str,
        model: &str,
    ) -> Result<String, CliError> {
        let requested = ProviderKind::parse(provider)
            .ok_or_else(|| CliError::usage("provider is not implemented"))?;

        let active = {
            let context = self
                .session
                .lock()
                .map_err(|_| CliError::storage("TUI session is unavailable"))?;
            resolved_provider(bootstrap, &context)
        };

        if requested == active {
            return apply_tui_model(bootstrap, model, &self.session);
        }

        self.apply_provider(bootstrap, provider)?;
        let message = apply_tui_model(bootstrap, model, &self.session)?;

        Ok(format!("Provider: {}. {message}", requested.label()))
    }

    pub(super) fn apply_provider(
        &self,
        bootstrap: &Bootstrap,
        provider: &str,
    ) -> Result<String, CliError> {
        let provider = ProviderKind::parse(provider)
            .ok_or_else(|| CliError::usage("provider is not implemented"))?;
        let mut context = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        if context.running {
            return Err(CliError::runtime(HeadlessTurnError::State));
        }
        let status = self
            .credentials
            .status(&bootstrap.paths.credentials, provider);
        if !status.available() {
            let message = if provider == ProviderKind::OpenAiChatGpt {
                "ChatGPT subscription requires connection; run /connect"
            } else {
                "OpenAI API credentials are unavailable"
            };
            return Err(CliError::authentication(message));
        }

        let current_model = effective_model(bootstrap, &context);
        let previous_effort = context
            .selection
            .as_ref()
            .and_then(ModelSelection::reasoning_effort);
        let mut next = ModelSelection::for_source(&current_model, provider.source());
        let compatible = next
            .model_values()
            .map_err(CliError::unavailable)?
            .iter()
            .any(|model| model == &current_model);
        let label = provider.label();
        let message = if compatible {
            let reset_effort =
                previous_effort.is_some_and(|effort| next.apply_reasoning_effort(effort).is_err());
            if reset_effort {
                format!(
                    "Provider: {label}. Model retained: {current_model}. Reasoning effort reset to Default."
                )
            } else {
                format!("Provider: {label}. Model retained: {current_model}.")
            }
        } else {
            let previous = current_model.clone();
            let default = provider.default_model();
            next = ModelSelection::for_source(default, provider.source());
            format!(
                "Provider: {label}. Model reset to {default} and reasoning effort reset to Default because {previous} is unavailable."
            )
        };
        apply_tui_selection(bootstrap, &mut context, provider, next)?;
        context.chatgpt_unavailable = false;
        context.resume_error = None;
        Ok(message)
    }
}
