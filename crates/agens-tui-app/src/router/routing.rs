//! Turning a submission into a turn.
//!
//! Every route runs under a cancellation the caller owns, so a keystroke can
//! stop a turn that is already in flight.

use agens_tui::{
    SecretInput, TuiRouteCancellation, TuiRouteProgress, TuiRouteRequest, TuiSubmissionOutcome,
};

use agens_auth::ChatGptAuthFlow;
use agens_providers::chatgpt_login::upsert_provider_entry;
use agens_session::provider::ProviderKind;

use super::{TUI_ERROR_ACTION, TuiRuntimeRouter, auth_route_outcome};

impl TuiRuntimeRouter {
    #[cfg(any(test, feature = "test-support"))]
    pub fn route(&self, input: String) -> TuiSubmissionOutcome {
        let (progress, _) = std::sync::mpsc::channel();
        self.route_with_progress(input, progress)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn route_with_progress(
        &self,
        input: String,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) -> TuiSubmissionOutcome {
        self.route_with_progress_cancellable(input, progress, TuiRouteCancellation::new())
    }

    fn route_with_progress_cancellable(
        &self,
        input: String,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
        cancellation: TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        let command = input.trim();
        let auth = match command {
            "/connect --device-auth" => Some(self.connect(ChatGptAuthFlow::Device, progress)),
            _ => None,
        };
        if let Some(result) = auth {
            return auth_route_outcome(result);
        }
        self.resolve_with_cancellation(input, &cancellation)
            .unwrap_or_else(|error| TuiSubmissionOutcome::LocalActionableError {
                message: error.to_string(),
                action: TUI_ERROR_ACTION.into(),
            })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn route_request(
        &self,
        request: TuiRouteRequest,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) -> TuiSubmissionOutcome {
        self.route_request_with_cancellation(request, progress, TuiRouteCancellation::new())
    }

    pub fn route_request_with_cancellation(
        &self,
        request: TuiRouteRequest,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
        cancellation: TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        let result = match request {
            TuiRouteRequest::DeviceAuthOpenUrl(url) => {
                return auth_route_outcome(self.open_device_auth_url(&url));
            }
            TuiRouteRequest::Input(input) => {
                return self.route_with_progress_cancellable(input, progress, cancellation);
            }
            TuiRouteRequest::SubmitSecret { action_id, secret } => {
                return self.submit_secret(action_id, secret);
            }
            TuiRouteRequest::OpenDialog(route_id) => self.open_dialog(&route_id),
            TuiRouteRequest::SessionPage(request) => {
                return self.session_dialog_outcome(request);
            }
            TuiRouteRequest::DialogAction(action_id) => {
                return self.route_dialog_action_with_cancellation(
                    &action_id,
                    progress,
                    &cancellation,
                );
            }
        };
        result.unwrap_or_else(|error| TuiSubmissionOutcome::LocalActionableError {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
        })
    }

    fn submit_secret(&self, action_id: String, secret: SecretInput) -> TuiSubmissionOutcome {
        let provider = match action_id.as_str() {
            "login:api-key:openai-api" => ProviderKind::OpenAiApi,
            "login:api-key:moonshotai" => ProviderKind::Moonshot,
            _ => {
                return TuiSubmissionOutcome::LocalActionableError {
                    message: "login action is invalid".into(),
                    action: TUI_ERROR_ACTION.into(),
                };
            }
        };
        let result = self.bootstrap().and_then(|bootstrap| {
            upsert_provider_entry(
                &bootstrap.paths.credentials,
                provider.identifier(),
                serde_json::json!({ "api_key": secret.into_string() }),
            )
            .map_err(|_| {
                agens_error::CliError::authentication("API-key credentials could not be saved")
            })
        });
        match result {
            Ok(()) => {
                TuiSubmissionOutcome::LocalInfo(format!("Logged in to {}.", provider.identifier()))
            }
            Err(_) => TuiSubmissionOutcome::LocalActionableError {
                message: "API-key credentials could not be saved".into(),
                action: TUI_ERROR_ACTION.into(),
            },
        }
    }
}
