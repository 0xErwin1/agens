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

#[cfg(any(test, feature = "test-support"))]
fn legacy_outcome(outcome: TuiSubmissionOutcome) -> TuiSubmissionOutcome {
    match outcome {
        TuiSubmissionOutcome::ProviderMessage { display, message } => {
            TuiSubmissionOutcome::ProviderTurn {
                display,
                prompt: agens_tui::user_message_text(&message),
            }
        }
        TuiSubmissionOutcome::BusyProviderMessage { display, message } => {
            TuiSubmissionOutcome::BusyProviderTurn {
                display,
                prompt: agens_tui::user_message_text(&message),
            }
        }
        outcome => outcome,
    }
}

fn text_message(text: String) -> agens_core::Message {
    agens_core::Message {
        role: agens_core::Role::User,
        parts: (!text.is_empty())
            .then_some(agens_core::MessagePart::Text(text))
            .into_iter()
            .collect(),
    }
}

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
        legacy_outcome(self.route_with_progress_cancellable(
            text_message(input),
            progress,
            TuiRouteCancellation::new(),
        ))
    }

    fn route_with_progress_cancellable(
        &self,
        input: agens_core::Message,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
        cancellation: TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        let input_text = agens_tui::user_message_text(&input);
        let command = input_text.trim();
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

    pub(crate) fn route_attached_request(
        &self,
        request: TuiRouteRequest,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
        cancellation: TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        let result = match request {
            TuiRouteRequest::Input(input) => {
                self.resolve_attached(text_message(input), &cancellation)
            }
            TuiRouteRequest::InputMessage(input) => self.resolve_attached(input, &cancellation),
            TuiRouteRequest::BusyInput(input) => self
                .resolve_attached(text_message(input), &cancellation)
                .map(queue_attached),
            TuiRouteRequest::BusyMessage(input) => self
                .resolve_attached(input, &cancellation)
                .map(queue_attached),
            TuiRouteRequest::AttachClipboardImage { .. }
            | TuiRouteRequest::ReplaceStagedMedia { .. } => {
                return self.route_request_with_cancellation(request, progress, cancellation);
            }
            _ => Err(CliError::usage(
                "this action is not supported while attached to a daemon",
            )),
        };
        result.unwrap_or_else(|error| TuiSubmissionOutcome::LocalActionableError {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
        })
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
                return self.route_with_progress_cancellable(
                    text_message(input),
                    progress,
                    cancellation,
                );
            }
            TuiRouteRequest::InputMessage(input) => {
                return self.route_with_progress_cancellable(input, progress, cancellation);
            }
            TuiRouteRequest::BusyInput(input) => {
                return self.route_busy_input(text_message(input), cancellation);
            }
            TuiRouteRequest::BusyMessage(input) => {
                return self.route_busy_input(input, cancellation);
            }
            TuiRouteRequest::AttachClipboardImage { bytes, mime } => {
                return self.attach_clipboard_image(bytes, mime);
            }
            TuiRouteRequest::ReplaceStagedMedia { attachments } => {
                return self.replace_staged_media(attachments);
            }
            TuiRouteRequest::SubmitSecret { action_id, secret } => {
                return self.submit_secret(action_id, secret);
            }
            TuiRouteRequest::OpenDialog(route_id) => self.open_dialog(&route_id),
            TuiRouteRequest::SessionPage(request) => {
                return self.session_dialog_outcome(request);
            }
            TuiRouteRequest::SessionTree(request) => {
                return self.session_tree_outcome(request);
            }
            TuiRouteRequest::ForkSession(request) => {
                return self.fork_session_outcome(request, &cancellation);
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
            let chip = session
                .pending_media_chip_labels()
                .last()
                .cloned()
                .unwrap_or_else(|| "[Image #?]".into());
            Ok(TuiSubmissionOutcome::MediaAttached {
                message: format!("Attached {chip} from clipboard."),
                staged_media: crate::files::session_staged_media(&session),
            })
        })();
        result.unwrap_or_else(
            |error: CliError| TuiSubmissionOutcome::LocalActionableError {
                message: error.to_string(),
                action: TUI_ERROR_ACTION.into(),
            },
        )
    }

    /// Replaces the session's staged media with a restored attachment set.
    ///
    /// Driven by stash pop, overlay paste, and history browse: the composer
    /// chips changed on the surface, and what the next submit sends must match.
    ///
    /// A recorded id proven unreachable is dropped rather than staged: staging it would fail
    /// the preflight of every later submit, with no way to take a chip back off the composer.
    ///
    /// A lookup that merely fails — a database that will not open, a busy writer, a blob whose
    /// existence could not even be checked — proves nothing, so the attachment stays staged and
    /// the failure is reported as such. The
    /// stash pop that feeds this path has already deleted the durable row, so dropping on
    /// an unproven failure would destroy the attachment for good.
    fn replace_staged_media(
        &self,
        attachments: Vec<agens_core::PromptAttachment>,
    ) -> TuiSubmissionOutcome {
        let result = (|| {
            let bootstrap = self.bootstrap()?;
            let mut session = self
                .session
                .lock()
                .map_err(|_| CliError::storage("TUI session is unavailable"))?;

            let mut staged: Vec<agens_core::PromptAttachment> =
                Vec::with_capacity(attachments.len());
            let mut dropped = 0usize;
            let mut unverified = 0usize;

            for attachment in attachments {
                match crate::files::check_restored_media(
                    bootstrap.data_directory(),
                    attachment.media_id,
                ) {
                    crate::files::RestoredMediaCheck::Reachable => staged.push(attachment),
                    crate::files::RestoredMediaCheck::ProvenGone => dropped += 1,
                    crate::files::RestoredMediaCheck::Unverified => {
                        unverified += 1;
                        staged.push(attachment);
                    }
                }
            }

            session.pending_media_ids = staged
                .iter()
                .map(|attachment| attachment.media_id)
                .collect();
            session.pending_media_mimes = staged
                .iter()
                .map(|attachment| attachment.mime.clone())
                .collect();

            Ok(TuiSubmissionOutcome::StagedMediaReplaced {
                staged_media: staged,
                notice: crate::files::restored_attachments_notice(dropped, unverified),
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
        input: agens_core::Message,
        cancellation: TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        let input_text = agens_tui::user_message_text(&input);
        match self.classify_busy_input(&input_text) {
            super::BusyPolicy::Queue => {
                match self.resolve_with_cancellation(input, &cancellation) {
                    Ok(TuiSubmissionOutcome::ProviderMessage { display, message }) => {
                        TuiSubmissionOutcome::BusyProviderMessage { display, message }
                    }
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

fn queue_attached(outcome: TuiSubmissionOutcome) -> TuiSubmissionOutcome {
    match outcome {
        TuiSubmissionOutcome::ProviderMessage { display, message } => {
            TuiSubmissionOutcome::BusyProviderMessage { display, message }
        }
        TuiSubmissionOutcome::ProviderTurn { display, prompt } => {
            TuiSubmissionOutcome::BusyProviderTurn { display, prompt }
        }
        outcome => outcome,
    }
}
