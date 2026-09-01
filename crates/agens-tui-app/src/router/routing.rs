//! Turning a submission into a turn.
//!
//! Every route runs under a cancellation the caller owns, so a keystroke can
//! stop a turn that is already in flight.

use agens_core::hosted::{
    CatalogKind, CatalogResult, HostedControlKind, HostedMcpAction, HostedMcpResult,
    HostedTaskReplay, HostedTaskState, WorkspaceFileContent,
};
use agens_core::{TuiExecutionEvent, TuiRuntimeEvent};
use agens_tui::{
    DialogEntry, DialogView, SecretInput, TuiRouteCancellation, TuiRouteProgress, TuiRouteRequest,
    TuiSubmissionOutcome,
};

use agens_auth::ChatGptAuthFlow;
use agens_error::CliError;
use agens_providers::chatgpt_login::upsert_provider_entry;
use agens_session::provider::ProviderKind;

use super::{TUI_ERROR_ACTION, TuiRuntimeRouter, auth_route_outcome};

/// What a command refused for running only on an idle session is answered
/// with, wherever that session is hosted.
const BUSY_REFUSAL: &str = "This command is unavailable while a response is in progress.";

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
        if self.attached_backend.is_some() {
            let result = match request {
                TuiRouteRequest::Input(input) => self.resolve_daemon_attached(text_message(input)),
                TuiRouteRequest::InputMessage(input) => self.resolve_daemon_attached(input),
                TuiRouteRequest::BusyInput(input) => {
                    self.route_attached_busy_input(text_message(input))
                }
                TuiRouteRequest::BusyMessage(input) => self.route_attached_busy_input(input),
                TuiRouteRequest::DialogAction(action) => self.resolve_daemon_dialog_action(&action),
                _ => Err(CliError::usage(
                    "this action is not supported while attached to a daemon",
                )),
            };
            return result.unwrap_or_else(|error| TuiSubmissionOutcome::LocalActionableError {
                message: error.to_string(),
                action: TUI_ERROR_ACTION.into(),
            });
        }

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

    /// A submission made while the daemon is answering.
    ///
    /// A command that may only run on an idle session is refused here, before
    /// anything is sent: the state it would run on belongs to the turn, so the
    /// daemon could only take it once that turn is over, and waiting for that
    /// is the terminal not drawing for as long as the answer takes. Everything
    /// else routes as it does when the session is idle, which for a prompt
    /// means queueing behind the running turn.
    fn route_attached_busy_input(
        &self,
        input: agens_core::Message,
    ) -> Result<TuiSubmissionOutcome, CliError> {
        let text = agens_tui::user_message_text(&input);
        if idle_only_command(&text) {
            return Ok(TuiSubmissionOutcome::BusyRefusal(BUSY_REFUSAL.into()));
        }

        self.resolve_daemon_attached(input).map(queue_attached)
    }

    fn resolve_daemon_attached(
        &self,
        input: agens_core::Message,
    ) -> Result<TuiSubmissionOutcome, CliError> {
        let display = agens_tui::user_message_text(&input);
        let command = display.trim();
        if command.starts_with('/') {
            return self.resolve_daemon_command(command);
        }

        let backend = self.attached_backend()?;
        let mut parts = Vec::new();
        for part in input.parts {
            let agens_core::MessagePart::Text(text) = part else {
                parts.push(part);
                continue;
            };
            let mut expanded = String::new();
            for segment in text.split_inclusive(char::is_whitespace) {
                let token = segment.trim_end_matches(char::is_whitespace);
                let whitespace = &segment[token.len()..];
                let Some(selector) = token.strip_prefix('@').filter(|value| !value.is_empty())
                else {
                    expanded.push_str(token);
                    expanded.push_str(whitespace);
                    continue;
                };
                match backend
                    .read_file(std::path::Path::new(selector))?
                    .map_err(file_error)?
                {
                    WorkspaceFileContent::Text { path, text } => {
                        expanded.push_str(&format!(
                            "<file path=\"{}\">\n{}\n</file>",
                            path.display(),
                            text
                        ));
                    }
                    WorkspaceFileContent::Media {
                        mime,
                        media_id: Some(media_id),
                        ..
                    } => {
                        if !expanded.is_empty() {
                            parts
                                .push(agens_core::MessagePart::Text(std::mem::take(&mut expanded)));
                        }
                        parts.push(agens_core::MessagePart::Media { media_id, mime });
                    }
                    WorkspaceFileContent::Media { media_id: None, .. } => {
                        return Err(CliError::usage(
                            "daemon media reference did not include a durable media identifier",
                        ));
                    }
                }
                expanded.push_str(whitespace);
            }
            if !expanded.is_empty() {
                parts.push(agens_core::MessagePart::Text(expanded));
            }
        }

        Ok(TuiSubmissionOutcome::ProviderMessage {
            display,
            message: agens_core::Message {
                role: agens_core::Role::User,
                parts,
            },
        })
    }

    fn resolve_daemon_command(&self, command: &str) -> Result<TuiSubmissionOutcome, CliError> {
        let backend = self.attached_backend()?;
        let mut words = command.split_whitespace();
        let name = words.next().unwrap_or_default().trim_start_matches('/');
        match name {
            // Resolved on the client, like "attach" and "select": the daemon
            // never sees "/cd" — the surface quits and the attached run loop
            // re-attaches against the requested checkout.
            "cd" => {
                let argument = command
                    .trim_start_matches('/')
                    .strip_prefix("cd")
                    .unwrap_or_default()
                    .trim();
                self.request_checkout_switch(argument)?;
                return Ok(TuiSubmissionOutcome::Quit);
            }
            "attach" => {
                let selector = words
                    .next()
                    .ok_or_else(|| CliError::usage("/attach requires a daemon workspace path"))?;
                let content = backend
                    .read_file(std::path::Path::new(selector))?
                    .map_err(file_error)?;
                let WorkspaceFileContent::Media {
                    mime,
                    media_id: Some(media_id),
                    ..
                } = content
                else {
                    return Err(CliError::usage(
                        "/attach requires a daemon-owned media file",
                    ));
                };
                let attachment = agens_core::PromptAttachment::new(media_id, mime);
                let staged_media = {
                    let mut session = self
                        .session
                        .lock()
                        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                    session.pending_media_ids.push(media_id);
                    session.pending_media_mimes.push(attachment.mime.clone());
                    session
                        .pending_media_ids
                        .iter()
                        .copied()
                        .zip(session.pending_media_mimes.iter().cloned())
                        .map(|(id, mime)| agens_core::PromptAttachment::new(id, mime))
                        .collect()
                };
                return Ok(TuiSubmissionOutcome::MediaAttached {
                    message: format!("Attached media #{media_id} from the daemon."),
                    staged_media,
                });
            }
            "skills" => {
                return Ok(TuiSubmissionOutcome::Dialog(catalog_dialog(
                    "Skills",
                    backend.catalog(CatalogKind::Skill)?,
                )?));
            }
            "mcp" => {
                let action = words.next();
                let result = match action {
                    None => backend.mcp_status()?,
                    Some(action) => {
                        let server = words.next().ok_or_else(|| {
                            CliError::usage("/mcp control requires a server name")
                        })?;
                        let action = match action {
                            "connect" => HostedMcpAction::Connect,
                            "disconnect" => HostedMcpAction::Disconnect,
                            "reconnect" => HostedMcpAction::Reconnect,
                            _ => {
                                return Err(CliError::usage(
                                    "/mcp supports connect, disconnect, or reconnect",
                                ));
                            }
                        };
                        backend.mcp_control(server, action)?
                    }
                };
                return Ok(TuiSubmissionOutcome::Dialog(mcp_dialog(result)));
            }
            "select" => {
                let files = backend
                    .list_files(std::path::Path::new("."))?
                    .map_err(file_error)?
                    .into_iter()
                    .map(|file| {
                        DialogEntry::action_with_detail(
                            file.path().display().to_string(),
                            Some(format!("{} bytes · {:?}", file.byte_len(), file.kind())),
                            format!("attach:{}", file.path().display()),
                        )
                    })
                    .collect();
                return Ok(TuiSubmissionOutcome::Dialog(DialogView::selection(
                    "Select file",
                    Some("Daemon workspace files"),
                    files,
                )));
            }
            "bypass" => {
                let reply = backend.command("/bypass")?.message;
                let enabled = match reply.as_str() {
                    agens_core::hosted::BYPASS_ON_REPLY => true,
                    agens_core::hosted::BYPASS_OFF_REPLY => false,
                    // A daemon that answered something else still answered;
                    // show it without guessing at footer state.
                    _ => return Ok(TuiSubmissionOutcome::LocalInfo(reply)),
                };
                return Ok(TuiSubmissionOutcome::BypassChanged {
                    message: reply,
                    enabled,
                });
            }
            "dangerous" => {
                let reply = backend.command("/dangerous")?.message;
                let enabled = match reply.as_str() {
                    agens_core::hosted::DANGEROUS_ON_REPLY => true,
                    agens_core::hosted::DANGEROUS_OFF_REPLY => false,
                    _ => return Ok(TuiSubmissionOutcome::LocalInfo(reply)),
                };
                return Ok(TuiSubmissionOutcome::DangerousChanged {
                    message: reply,
                    enabled,
                });
            }
            "quit" => return Ok(TuiSubmissionOutcome::Quit),
            _ => {}
        }

        let known = catalog_contains(backend.catalog(CatalogKind::Command)?, name)?
            || catalog_contains(backend.catalog(CatalogKind::Skill)?, name)?;
        if !known {
            return Err(CliError::usage(format!("unknown daemon command: /{name}")));
        }
        // Whatever the command was, the daemon owns what the session is
        // configured as after it. A command that changed the model, the effort
        // or the agent's model comes back described, and the footer follows
        // that description rather than the reply text; one that changed nothing
        // comes back undescribed and leaves the footer as it was.
        let reply = backend.command(command)?;

        Ok(match reply.presentation {
            Some(presentation) => TuiSubmissionOutcome::ContextChanged {
                message: reply.message,
                presentation,
            },
            None => TuiSubmissionOutcome::LocalInfo(reply.message),
        })
    }

    /// Validates a `/cd` target the way the daemon validates a chat's
    /// checkout: `~` expands, the path must resolve on this filesystem, and it
    /// must name a directory. The canonical path is recorded for the attached
    /// run loop, which re-attaches against it — the daemon keys open chats by
    /// checkout, so canonicalizing here is what makes both sides agree on
    /// which conversation that is.
    fn request_checkout_switch(&self, argument: &str) -> Result<(), CliError> {
        if argument.is_empty() {
            return Err(CliError::usage("/cd requires a directory: /cd <path>"));
        }

        let expanded = agens_config::expand_home_prefix(argument, std::env::home_dir().as_deref());
        let target = expanded.canonicalize().map_err(|error| {
            CliError::usage(format!(
                "the directory {} cannot be opened: {error}",
                expanded.display()
            ))
        })?;

        if !target.is_dir() {
            return Err(CliError::usage(format!(
                "{} is not a directory",
                target.display()
            )));
        }

        let mut slot = self
            .checkout_switch
            .lock()
            .map_err(|_| CliError::storage("the checkout switch is unavailable"))?;
        *slot = Some(target);

        Ok(())
    }

    fn resolve_daemon_dialog_action(&self, action: &str) -> Result<TuiSubmissionOutcome, CliError> {
        if let Some(path) = action.strip_prefix("attach:") {
            return self.resolve_daemon_command(&format!("/attach {path}"));
        }
        if let Some(server) = action.strip_prefix("mcp:reconnect:") {
            return self.resolve_daemon_command(&format!("/mcp reconnect {server}"));
        }
        if action == "bypass" {
            return self.resolve_daemon_command("/bypass");
        }
        if action == "dangerous" {
            return self.resolve_daemon_command("/dangerous");
        }
        Err(CliError::usage("unknown attached dialog action"))
    }

    pub(crate) fn attached_task_events(&self) -> Result<Vec<TuiRuntimeEvent>, CliError> {
        let replay = self
            .attached_backend()?
            .task_snapshot()?
            .map_err(task_error)?;
        let mut events = Vec::new();
        match replay {
            HostedTaskReplay::Events(tail) => {
                events.extend(tail.into_iter().filter_map(hosted_event));
            }
            HostedTaskReplay::SnapshotTail {
                snapshot,
                events: tail,
            } => {
                events.extend(snapshot.tasks().iter().filter_map(|task| {
                    hosted_task_event(task.task_id(), task.state(), "attached")
                }));
                events.extend(tail.into_iter().filter_map(hosted_event));
                for turn in snapshot.child_turns() {
                    if let Ok(id) = turn.task_id().parse::<u64>() {
                        events.push(TuiRuntimeEvent::RestoredCompletedSubagent {
                            id,
                            agent: "attached".into(),
                            task_summary: format!("Restored task {}", turn.task_id()),
                            final_result: turn.payload().to_owned(),
                            tool_uses: 0,
                        });
                    }
                }
            }
            HostedTaskReplay::Gap { oldest_cursor } => {
                return Err(CliError::storage(format!(
                    "daemon task history starts at cursor {oldest_cursor}"
                )));
            }
        }
        Ok(events)
    }

    pub(crate) fn attached_background_task(&self, id: u64) -> bool {
        self.attached_control(HostedControlKind::Background, Some(id))
    }

    pub(crate) fn attached_cancel_task(&self, id: u64) -> bool {
        self.attached_control(HostedControlKind::Cancel, Some(id))
    }

    pub(crate) fn attached_cancel_all_tasks(&self) -> Vec<u64> {
        self.attached_control(HostedControlKind::CancelAll, None)
            .then(Vec::new)
            .unwrap_or_default()
    }

    pub(crate) fn attached_send_task_message(&self, id: u64, message: String) -> bool {
        self.attached_control(HostedControlKind::Message(message), Some(id))
    }

    fn attached_control(&self, kind: HostedControlKind, id: Option<u64>) -> bool {
        self.attached_backend()
            .and_then(|backend| backend.task_control(kind, id))
            .is_ok_and(|result| result.is_ok())
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
            super::BusyPolicy::Reject => TuiSubmissionOutcome::BusyRefusal(BUSY_REFUSAL.into()),
            super::BusyPolicy::Local | super::BusyPolicy::Quit | super::BusyPolicy::Invalid => self
                .resolve_with_cancellation(input, &cancellation)
                .unwrap_or_else(|error| TuiSubmissionOutcome::LocalActionableError {
                    message: error.to_string(),
                    action: TUI_ERROR_ACTION.into(),
                }),
        }
    }
}

fn file_error(error: agens_core::hosted::FileError) -> CliError {
    CliError::new(
        agens_error::ExitStatus::Failure,
        "file",
        format!("daemon workspace file failed: {error:?}"),
    )
}

fn task_error(error: agens_core::hosted::TaskControlError) -> CliError {
    CliError::new(
        agens_error::ExitStatus::Failure,
        "task",
        format!("daemon task operation failed: {error:?}"),
    )
}

fn catalog_contains(result: CatalogResult, name: &str) -> Result<bool, CliError> {
    match result {
        CatalogResult::Current(snapshot) => {
            Ok(snapshot.entries().iter().any(|entry| entry.name() == name))
        }
        CatalogResult::Stale { current_revision } => Err(CliError::unavailable(format!(
            "daemon catalog changed to revision {current_revision}; retry"
        ))),
        CatalogResult::Unsupported => Err(CliError::unavailable("daemon catalog is unsupported")),
    }
}

fn catalog_dialog(title: &str, result: CatalogResult) -> Result<DialogView, CliError> {
    let CatalogResult::Current(snapshot) = result else {
        return match result {
            CatalogResult::Stale { current_revision } => Err(CliError::unavailable(format!(
                "daemon catalog changed to revision {current_revision}; retry"
            ))),
            CatalogResult::Unsupported => {
                Err(CliError::unavailable("daemon catalog is unsupported"))
            }
            CatalogResult::Current(_) => unreachable!(),
        };
    };
    let entries = snapshot
        .entries()
        .iter()
        .map(|entry| {
            DialogEntry::action_with_detail(
                format!("/{}", entry.name()),
                Some(entry.description()),
                format!("fill:/{} ", entry.name()),
            )
        })
        .collect();
    Ok(DialogView::selection(
        title,
        Some(format!("Daemon catalog · revision {}", snapshot.revision())),
        entries,
    ))
}

fn mcp_dialog(result: HostedMcpResult) -> DialogView {
    let entries = result
        .servers()
        .iter()
        .map(|server| {
            DialogEntry::action_with_detail(
                server.name(),
                Some(format!(
                    "{:?} · generation {}{}",
                    server.state(),
                    server.generation(),
                    server
                        .error()
                        .map_or(String::new(), |error| format!(" · {error}"))
                )),
                format!("mcp:reconnect:{}", server.name()),
            )
        })
        .collect();
    DialogView::selection("MCP servers", result.error(), entries)
}

fn hosted_event(event: agens_core::hosted::HostedTaskEvent) -> Option<TuiRuntimeEvent> {
    hosted_task_event(event.task_id(), event.state(), event.payload())
}

fn hosted_task_event(
    task_id: &str,
    state: HostedTaskState,
    payload: &str,
) -> Option<TuiRuntimeEvent> {
    let id = task_id.parse::<u64>().ok()?;
    let agent = payload.split_once(':').map_or(payload, |(_, agent)| agent);
    let event = match state {
        HostedTaskState::Running => TuiExecutionEvent::ForegroundStarted { id },
        HostedTaskState::Background => TuiExecutionEvent::BackgroundStarted { id },
        HostedTaskState::Completed => TuiExecutionEvent::Completed { id },
        HostedTaskState::Cancelled => TuiExecutionEvent::Cancelled { id },
        HostedTaskState::Failed => TuiExecutionEvent::Failed { id },
    };
    Some(TuiRuntimeEvent::TaskExecution {
        agent: agent.to_owned(),
        event,
    })
}

/// Whether an input names a built-in command that may only run between turns.
fn idle_only_command(input: &str) -> bool {
    let trimmed = input.trim();
    let Some(invocation) = trimmed.strip_prefix('/') else {
        return false;
    };
    let name_end = invocation
        .find(char::is_whitespace)
        .unwrap_or(invocation.len());

    crate::extensions::tui_builtin_busy_policy(&invocation[..name_end])
        .is_some_and(|policy| policy == agens_tools::CommandBusyPolicy::IdleOnly)
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
