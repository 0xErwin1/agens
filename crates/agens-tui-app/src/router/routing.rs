//! Turning a submission into a turn.
//!
//! Every route runs under a cancellation the caller owns, so a keystroke can
//! stop a turn that is already in flight.

use agens_tui::{
    SecretInput, TuiRouteCancellation, TuiRouteProgress, TuiRouteRequest, TuiSubmissionOutcome,
};

use agens_auth::ChatGptAuthFlow;
use agens_error::CliError;
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
            TuiRouteRequest::BusyInput(input) => {
                return self.route_busy_input(input, cancellation);
            }
            TuiRouteRequest::AttachClipboardImage { bytes, mime } => {
                return self.attach_clipboard_image(bytes, mime);
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

    fn attach_clipboard_image(&self, bytes: Vec<u8>, mime: Option<String>) -> TuiSubmissionOutcome {
        let result = (|| {
            let bootstrap = self.bootstrap()?;
            let mut session = self
                .session
                .lock()
                .map_err(|_| CliError::storage("TUI session is unavailable"))?;
            let (media_id, mime) =
                crate::files::ingest_tui_media_bytes(&bootstrap, &bytes, mime.as_deref())?;
            session.push_pending_media(media_id, mime);
            let media_chips = session.pending_media_chip_labels();
            let chip = media_chips
                .last()
                .cloned()
                .unwrap_or_else(|| "[Image #?]".into());
            Ok(TuiSubmissionOutcome::MediaAttached {
                message: format!("Attached {chip} from clipboard."),
                media_chips,
            })
        })();
        result.unwrap_or_else(
            |error: CliError| TuiSubmissionOutcome::LocalActionableError {
                message: error.to_string(),
                action: TUI_ERROR_ACTION.into(),
            },
        )
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

    fn route_busy_input(
        &self,
        input: String,
        cancellation: TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        match self.classify_busy_input(&input) {
            super::BusyPolicy::Queue => {
                match self.resolve_with_cancellation(input, &cancellation) {
                    Ok(TuiSubmissionOutcome::ProviderTurn { display, prompt }) => {
                        TuiSubmissionOutcome::BusyProviderTurn { display, prompt }
                    }
                    Ok(outcome) => outcome,
                    Err(error) => TuiSubmissionOutcome::LocalActionableError {
                        message: error.to_string(),
                        action: TUI_ERROR_ACTION.into(),
                    },
                }
            }
            super::BusyPolicy::Reject => TuiSubmissionOutcome::BusyRefusal(
                "This command is unavailable while a response is in progress.".into(),
            ),
            super::BusyPolicy::Local | super::BusyPolicy::Quit | super::BusyPolicy::Invalid => self
                .resolve_with_cancellation(input, &cancellation)
                .unwrap_or_else(|error| TuiSubmissionOutcome::LocalActionableError {
                    message: error.to_string(),
                    action: TUI_ERROR_ACTION.into(),
                }),
        }
    }
}
