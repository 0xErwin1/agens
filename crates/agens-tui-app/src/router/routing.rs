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
    /// A recorded id proven unreachable is dropped rather than staged, the same way a
    /// resume drops one: staging it would fail the preflight of every later submit, with
    /// no way to take a chip back off the composer.
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
                match agens_store::open_media(bootstrap.data_directory(), attachment.media_id) {
                    Ok(_) => staged.push(attachment),
                    Err(
                        agens_store::MediaStoreError::NotFound { .. }
                        | agens_store::MediaStoreError::Io { .. },
                    ) => dropped += 1,
                    Err(_) => {
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
                notice: restored_attachments_notice(dropped, unverified),
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

/// Reports what a restore did to attachments it could not stage as recorded.
///
/// `dropped` counts the ones proven unreachable, `unverified` the ones whose lookup failed
/// without proving anything — the latter stay staged, so the two claims must not be merged.
fn restored_attachments_notice(dropped: usize, unverified: usize) -> Option<String> {
    let mut parts = Vec::new();

    if dropped > 0 {
        parts.push(dropped_attachments_notice(dropped));
    }
    if unverified > 0 {
        parts.push(unverified_attachments_notice(unverified));
    }

    (!parts.is_empty()).then(|| parts.join(" "))
}

fn unverified_attachments_notice(unverified: usize) -> String {
    if unverified == 1 {
        "1 restored attachment could not be checked and was kept staged.".to_owned()
    } else {
        format!("{unverified} restored attachments could not be checked and were kept staged.")
    }
}

fn dropped_attachments_notice(dropped: usize) -> String {
    if dropped == 1 {
        "1 restored attachment is no longer available and was dropped.".to_owned()
    } else {
        format!("{dropped} restored attachments are no longer available and were dropped.")
    }
}
