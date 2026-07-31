//! Resolving a slash command, and the bootstrap a turn runs against.

use agens_session::model::current_provider;

use agens_core::HeadlessTurnError;
use agens_tui::{TuiPresentation, TuiRouteCancellation, TuiSubmissionOutcome};

use crate::engine::{seed_fresh_tui_context, write_through_bypass_permission_prompts};
use crate::extensions::RESERVED_TUI_COMMANDS;
use crate::models::{select_tui_effort, select_tui_model};
use crate::resume::{commit_tui_session_resume, resume_tui_session};
use crate::turn::tui_session_presentation;
use agens_agents::select_subagent;
use agens_bootstrap::Bootstrap;
use agens_error::{CliError, ExitStatus};
use agens_session::context::reset_session;
use agens_session::provider::ProviderKind;
use agens_tool_runtime::rotation::rotate_agent;

use super::TuiRuntimeRouter;

impl TuiRuntimeRouter {
    #[cfg(any(test, feature = "test-support"))]
    pub fn resolve(&self, input: String) -> Result<TuiSubmissionOutcome, CliError> {
        self.resolve_with_cancellation(input, &TuiRouteCancellation::new())
    }

    pub(super) fn resolve_with_cancellation(
        &self,
        input: String,
        cancellation: &TuiRouteCancellation,
    ) -> Result<TuiSubmissionOutcome, CliError> {
        if !input.starts_with('/') {
            return Ok(TuiSubmissionOutcome::ProviderTurn {
                display: input.clone(),
                prompt: input,
            });
        }

        let command = input.trim();
        let invocation = command
            .strip_prefix('/')
            .expect("slash command input was checked");
        let name_end = invocation
            .find(char::is_whitespace)
            .unwrap_or(invocation.len());
        let (name, arguments) = invocation.split_at(name_end);
        let arguments = arguments.trim();
        let bootstrap = self.bootstrap()?;
        let outcome = match command {
            "/dangerous" => return self.toggle_dangerous_mode(),
            "/bypass" => return self.toggle_bypass_permissions(),
            "/help" => self.open_dialog("help")?,
            "/mcp" => self.open_dialog("mcp")?,
            "/select" => self.open_dialog("select")?,
            "/quit" => TuiSubmissionOutcome::Quit,
            "/sessions" | "/resume" => self.open_dialog("sessions")?,
            "/connect" => self.open_dialog("connect")?,
            "/disconnect" => self.open_dialog("disconnect")?,
            "/login" => self.open_dialog("login")?,
            "/diagnostics" => self.open_dialog("diagnostics")?,
            "/provider" => self.open_dialog("provider")?,
            command if command.starts_with("/provider ") => TuiSubmissionOutcome::ContextChanged {
                message: self.apply_provider(&bootstrap, &command[10..])?,
                presentation: self.presentation()?,
            },
            "/new" => {
                let mut session = self.session.lock().map_err(|_| {
                    CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
                })?;
                reset_session(&mut session)
                    .map_err(|_| CliError::runtime(HeadlessTurnError::State))?;
                let notice = seed_fresh_tui_context(&bootstrap, &mut session)?;
                drop(session);
                TuiSubmissionOutcome::ResetSucceeded {
                    message: notice.map_or_else(
                        || "Started a new session.".into(),
                        |notice| format!("Started a new session. {notice}"),
                    ),
                    presentation: self.presentation()?,
                }
            }
            command if command.starts_with("/resume ") => {
                let expected = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?
                    .clone();
                if expected.running {
                    return Err(CliError::runtime(HeadlessTurnError::State));
                }
                let identifier = command[8..]
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| CliError::usage("/resume requires a numeric session id"))?;
                let resumed = resume_tui_session(
                    &bootstrap,
                    identifier,
                    self.skills()?.as_ref(),
                    &self.credentials,
                )?;
                commit_tui_session_resume(
                    &bootstrap,
                    &self.session,
                    &expected,
                    resumed,
                    cancellation,
                    |context| self.on_session_resume_committed(&bootstrap, context),
                )?
            }
            command if command.starts_with("/agent ") => TuiSubmissionOutcome::ContextChanged {
                message: rotate_agent(
                    &bootstrap,
                    &command[7..],
                    &self.session,
                    self.skills()?.as_ref(),
                )?,
                presentation: self.presentation()?,
            },
            "/agent" => self.open_dialog("agent")?,
            command if command.starts_with("/subagent ") => TuiSubmissionOutcome::ContextChanged {
                message: select_subagent(&bootstrap, &command[10..], &self.session)?,
                presentation: self.presentation()?,
            },
            "/subagent" => self.open_dialog("subagent")?,
            "/subagent-profiles" => self.open_dialog("subagent-profiles")?,
            "/subagents" => TuiSubmissionOutcome::TranscriptDialog,
            "/model" => self.open_dialog("model")?,
            command if command.starts_with("/model ") => TuiSubmissionOutcome::ContextChanged {
                message: select_tui_model(&bootstrap, command, &self.session)?,
                presentation: self.presentation()?,
            },
            "/effort" => self.open_dialog("effort")?,
            command if command.starts_with("/effort ") => TuiSubmissionOutcome::ContextChanged {
                message: select_tui_effort(&bootstrap, command, &self.session)?,
                presentation: self.presentation()?,
            },
            _ if RESERVED_TUI_COMMANDS.contains(&name) => {
                return Err(CliError::usage(format!("unknown TUI command: {command}")));
            }
            _ => match self.commands()?.command(name) {
                Some(command) => TuiSubmissionOutcome::ProviderTurn {
                    display: input.clone(),
                    prompt: command.expand(arguments),
                },
                None => match self.skills()?.skill(name) {
                    Some(skill) => TuiSubmissionOutcome::ProviderTurn {
                        display: input.clone(),
                        prompt: format!(
                            "## Skill: {}\n{}\n\n## User arguments\n{}",
                            skill.name(),
                            skill.load_instructions().map_err(|_| {
                                CliError::usage(format!("skill /{name} is unavailable"))
                            })?,
                            arguments
                        ),
                    },
                    None => {
                        return Err(CliError::usage(format!("unknown TUI command: {command}")));
                    }
                },
            },
        };
        Ok(outcome)
    }

    pub fn presentation(&self) -> Result<TuiPresentation, CliError> {
        let bootstrap = self.bootstrap()?;
        let session = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        Ok(tui_session_presentation(&bootstrap, &session))
    }

    pub(super) fn toggle_dangerous_mode(&self) -> Result<TuiSubmissionOutcome, CliError> {
        let enabled = {
            let mut session = self
                .session
                .lock()
                .map_err(|_| CliError::storage("TUI session is unavailable"))?;
            session.dangerous_mode = !session.dangerous_mode;
            session.dangerous_mode
        };

        Ok(TuiSubmissionOutcome::ContextChanged {
            message: format!("Dangerous mode: {}.", if enabled { "on" } else { "off" }),
            presentation: self.presentation()?,
        })
    }

    /// Toggles the session-only permission-bypass flag. Never writes configuration; when the
    /// session already has a row (an identifier), the new value is written through immediately
    /// via the same best-effort path used after a completed turn.
    pub(super) fn toggle_bypass_permissions(&self) -> Result<TuiSubmissionOutcome, CliError> {
        let (enabled, identifier) = {
            let mut session = self
                .session
                .lock()
                .map_err(|_| CliError::storage("TUI session is unavailable"))?;
            session.bypass_permissions = !session.bypass_permissions;
            (session.bypass_permissions, session.identifier)
        };
        let mut message = format!("Permission bypass: {}.", if enabled { "on" } else { "off" });
        if let Some(identifier) = identifier {
            let bootstrap = self.bootstrap()?;
            // This is the moment the user actually asked for a change, so a failed write is
            // surfaced here rather than swallowed — leaving it silent would let a deliberate
            // OFF toggle appear to have worked while the persisted value stayed stale ON.
            if write_through_bypass_permission_prompts(&bootstrap, identifier, enabled).is_err() {
                message.push_str(" This could not be saved and may not persist across resume.");
            }
        }

        Ok(TuiSubmissionOutcome::ContextChanged {
            message,
            presentation: self.presentation()?,
        })
    }

    pub fn bootstrap(&self) -> Result<Bootstrap, CliError> {
        self.bootstrap
            .lock()
            .map(|bootstrap| bootstrap.clone())
            .map_err(|_| CliError::storage("TUI provider state is unavailable"))
    }

    pub fn turn_bootstrap(&self) -> Result<Bootstrap, CliError> {
        let mut bootstrap = self.bootstrap()?;
        let context = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        if context.chatgpt_unavailable {
            return Err(CliError::authentication(
                "ChatGPT credentials are unavailable; run /connect",
            ));
        }
        let provider = current_provider(&bootstrap, &context)
            .ok_or_else(|| CliError::configuration("TUI provider is unavailable"))?;
        if let Some(selection) = &context.selection {
            bootstrap.model = Some(selection.model().to_owned());
        }
        drop(context);

        bootstrap.provider_type = Some(provider.identifier().into());
        bootstrap.api_key = match provider {
            ProviderKind::OpenAiChatGpt => {
                if !self
                    .credentials
                    .status(&bootstrap.paths.credentials, provider)
                    .available()
                {
                    return Err(CliError::authentication(
                        "ChatGPT credentials are unavailable or invalid; run /connect",
                    ));
                }
                None
            }
            ProviderKind::OpenAiApi | ProviderKind::Moonshot => Some(
                self.credentials
                    .provider_api_key(&bootstrap.paths.credentials, provider)
                    .ok_or_else(|| {
                        CliError::authentication(format!(
                            "{} authentication is unavailable",
                            provider.label()
                        ))
                    })?,
            ),
        };
        Ok(bootstrap)
    }

    pub fn task_parent_request_config(&self) -> Result<agens_core::RequestConfig, CliError> {
        self.session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))
            .map(|context| {
                context
                    .selection
                    .as_ref()
                    .map(|selection| selection.request_config().clone())
                    .unwrap_or_default()
            })
    }
}
