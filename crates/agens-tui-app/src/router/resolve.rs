//! Resolving a slash command, and the bootstrap a turn runs against.

use agens_session::model::current_provider;

use agens_core::HeadlessTurnError;
use agens_tui::{TuiPresentation, TuiRouteCancellation, TuiSubmissionOutcome};

use crate::engine::{seed_fresh_tui_context, write_through_bypass_permission_prompts};
use crate::files::{ingest_tui_media_path, resolve_attach_path};
use crate::models::{select_tui_effort, select_tui_model};
use crate::resume::{commit_tui_session_resume, resume_tui_session};
use crate::turn::tui_session_presentation;
use crate::undo::{rewind_detail, rewind_failure, rewind_summary, unavailable_message};
use agens_agents::select_subagent;
use agens_bootstrap::Bootstrap;
use agens_error::{CliError, ExitStatus};
use agens_session::context::reset_session;
use agens_session::provider::ProviderKind;
use agens_session::undo::{
    Rewind, commit_rewind, open_session_snapshots, pending_turn, rewind_tree, session_snapshot_root,
};
use agens_tool_runtime::rotation::rotate_agent;

use super::{BusyPolicy, TuiRuntimeRouter};

impl TuiRuntimeRouter {
    /// Classifies an input against the current built-in, command, and skill catalogs.
    ///
    /// The method intentionally performs no route work: callers can decide whether a busy
    /// submission is queueable before clearing the composer or mutating the scheduler.
    pub fn classify_busy_input(&self, input: &str) -> BusyPolicy {
        let Some(name) = command_name(input) else {
            return BusyPolicy::Queue;
        };

        let Ok(commands) = self.commands() else {
            return BusyPolicy::Invalid;
        };
        if let Some(command) = commands.command(name) {
            return BusyPolicy::from_catalog_policy(command.busy_policy());
        }

        if self
            .skills()
            .is_ok_and(|skills| skills.skill(name).is_some())
        {
            return BusyPolicy::Queue;
        }

        BusyPolicy::Invalid
    }

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
            "/undo" => return self.rewind_turn(Rewind::Back),
            "/redo" => return self.rewind_turn(Rewind::Forward),
            "/bypass" => return self.toggle_bypass_permissions(),
            "/help" => self.open_dialog("help")?,
            "/history" => self.open_dialog("history")?,
            "/mcp" => self.open_dialog("mcp")?,
            "/select" => self.open_dialog("select")?,
            "/stash" => self.open_dialog("stash")?,
            command if command.starts_with("/attach") => self.attach_media(command)?,
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
            _ if self
                .commands()?
                .command(name)
                .is_some_and(|command| command.is_builtin()) =>
            {
                return Err(CliError::usage(format!("unknown TUI command: {command}")));
            }
            _ => match self.commands()?.command(name) {
                Some(command) => self.expanded_turn(&input, command.expand(arguments))?,
                None => match self.skills()?.skill(name) {
                    Some(skill) => self.expanded_turn(
                        &input,
                        format!(
                            "## Skill: {}\n{}\n\n## User arguments\n{}",
                            skill.name(),
                            skill.load_instructions().map_err(|_| {
                                CliError::usage(format!("skill /{name} is unavailable"))
                            })?,
                            arguments
                        ),
                    )?,
                    None => {
                        return Err(CliError::usage(format!("unknown TUI command: {command}")));
                    }
                },
            },
        };
        Ok(outcome)
    }

    /// A turn whose prompt is an expansion of what the reader typed.
    ///
    /// The session keeps the invocation alongside the expansion so that taking
    /// the turn back hands `/name arguments` to the composer rather than the
    /// command template or the whole inlined body of a skill.
    fn expanded_turn(
        &self,
        typed: &str,
        expanded: String,
    ) -> Result<TuiSubmissionOutcome, CliError> {
        self.session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?
            .remember_expanded_prompt(typed.to_owned(), expanded.clone());

        Ok(TuiSubmissionOutcome::ProviderTurn {
            display: typed.to_owned(),
            prompt: expanded,
        })
    }

    pub fn presentation(&self) -> Result<TuiPresentation, CliError> {
        let bootstrap = self.bootstrap()?;
        let session = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        Ok(tui_session_presentation(&bootstrap, &session))
    }

    /// Takes the last turn back, or puts it back.
    ///
    /// Both directions are the same operation: move the working tree to the
    /// other snapshot, move the marker over the messages, and hand the prompt
    /// that started the turn back to the composer.
    ///
    /// The tree moves with the session lock released, because every step of it
    /// spawns git, and the marker moves only afterwards: a restore that could
    /// not finish leaves both the transcript and the files as they were, so the
    /// same command can be run again once the reader has dealt with it.
    fn rewind_turn(&self, direction: Rewind) -> Result<TuiSubmissionOutcome, CliError> {
        let bootstrap = self.bootstrap()?;
        let (root, step) = {
            let session = self
                .session
                .lock()
                .map_err(|_| CliError::storage("TUI session is unavailable"))?;
            if session.running {
                return Err(CliError::runtime(HeadlessTurnError::State));
            }

            let root = session_snapshot_root(&bootstrap, &session);
            match pending_turn(&session, direction) {
                Ok(step) => (root, step),
                Err(unavailable) => {
                    return Ok(TuiSubmissionOutcome::LocalInfo(unavailable_message(
                        &unavailable,
                    )));
                }
            }
        };

        let snapshots = root
            .as_deref()
            .and_then(|root| open_session_snapshots(&bootstrap, root));
        let outcome = match rewind_tree(snapshots.as_ref(), &step, direction) {
            Ok(outcome) => outcome,
            Err(unavailable) => {
                return Ok(TuiSubmissionOutcome::LocalInfo(unavailable_message(
                    &unavailable,
                )));
            }
        };
        if !outcome.is_complete() {
            return Ok(TuiSubmissionOutcome::LocalActionableError {
                message: rewind_failure(&outcome, direction),
                action: "resolve the listed files and run the command again".into(),
            });
        }

        {
            let mut session = self
                .session
                .lock()
                .map_err(|_| CliError::storage("TUI session is unavailable"))?;
            commit_rewind(&mut session, direction);
        }

        let history = self.live_history()?;
        Ok(TuiSubmissionOutcome::HistoryRewritten {
            message: rewind_summary(&outcome, direction),
            detail: rewind_detail(&outcome),
            presentation: self.presentation()?,
            history,
            draft: matches!(direction, Rewind::Back).then(|| outcome.prompt.clone()),
        })
    }

    /// The transcript for the messages that are still part of the conversation.
    fn live_history(&self) -> Result<Vec<agens_tui::Conversation>, CliError> {
        let session = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        Ok(crate::resume::project_tui_history(session.live_messages()))
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

    /// `/attach PATH` — ingest via store, stage media ids on the session, return path-free chips.
    pub(super) fn attach_media(&self, command: &str) -> Result<TuiSubmissionOutcome, CliError> {
        let raw = command
            .strip_prefix("/attach")
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| CliError::usage("attach requires a path: /attach <path>"))?;
        let bootstrap = self.bootstrap()?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        let path = resolve_attach_path(&session, &bootstrap, raw)?;
        let (media_id, mime) = ingest_tui_media_path(&bootstrap, &path)?;
        session.push_pending_media(media_id, mime);
        let media_chips = session.pending_media_chip_labels();
        let chip = media_chips
            .last()
            .cloned()
            .unwrap_or_else(|| "[Image #?]".into());
        Ok(TuiSubmissionOutcome::MediaAttached {
            message: format!("Attached {chip}."),
            media_chips,
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

fn command_name(input: &str) -> Option<&str> {
    let invocation = input.trim().strip_prefix('/')?;
    let name_end = invocation
        .find(char::is_whitespace)
        .unwrap_or(invocation.len());
    Some(&invocation[..name_end])
}
