//! The interactive-command router for the TUI: [`TuiRuntimeRouter`] resolves
//! slash commands and route requests into [`agens_tui::TuiSubmissionOutcome`]
//! / [`agens_tui::TuiProviderOutcome`] values, owns the ChatGPT auth/provider
//! state machine, and drives session/dialog navigation.
//!
//! `impl TuiRuntimeRouter` is one intentionally intact ~1,000-line block: the
//! user decided that splitting it further is out of scope for this change
//! and is declared debt (see the design's Part 2, "Boundaries that did NOT
//! hold", item 3). Do not distribute its methods across files or additional
//! `impl` blocks.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_core::{AttemptKey, HeadlessTurnCancellation, HeadlessTurnError, RecoveryOutcome};
use agens_providers::chatgpt_login::LoginCancellation;
use agens_store::{SessionCursor, SessionStore};
use agens_tools::{CommandCatalog, McpRegistry, McpStatusHandle, SkillCatalog};
use agens_tui::{
    DialogEntry, DialogView, PaletteEntry, SessionDialogCursor, SessionDialogRequest,
    SessionDialogScope, TuiPresentation, TuiProviderOutcome, TuiRouteCancellation,
    TuiRouteProgress, TuiRouteRequest, TuiSubmissionOutcome,
};

use crate::chatgpt_auth::{self, ChatGptAuthCoordinator, ChatGptAuthFlow, ChatGptAuthProgress};
use crate::mcp::load_configured_mcp_registry;
use crate::session::agents::{
    agent_catalog_for_context, persist_pending_agent_correction, rotate_agent, select_subagent,
    subagent_catalog,
};
use crate::session::attempt::active_session_attempts;
use crate::session::context::SessionContext;
use crate::session::context::{current_session_timestamp, reset_session};
use crate::session::provider::{
    ChatGptCredentialSnapshot, CredentialResolver, CredentialStatus, ProviderKind,
    restore_chatgpt_credentials, snapshot_chatgpt_credentials,
};
use crate::tools::task::default_model;
use crate::tui::dialogs::{diagnostics_dialog, mcp_status_dialog};
use crate::tui::extensions::{
    RESERVED_TUI_COMMANDS, discover_skill_catalog, discover_tui_command_catalog, render_tui_help,
    resolved_tui_palette,
};
use crate::tui::files::{selected_tui_file, tui_picker_file_candidates, tui_select_candidates};
use crate::tui::models::{
    apply_tui_effort, apply_tui_model, apply_tui_selection, apply_tui_unverified_model,
    format_model_metadata, select_tui_effort, select_tui_model, tui_model_source,
};
use crate::tui::resume::{
    ResumedTuiSession, commit_tui_session_resume, load_tui_session_for_resume,
    prepare_loaded_tui_session_resume, resume_tui_session, tui_project_identifier,
};
use crate::tui::session::{
    parse_recovery_action, recovery_confirmation_dialog, session_dialog_entry,
};
use crate::tui::turn::{current_tui_provider, effective_tui_model, tui_session_presentation};
use agens_bootstrap::{Bootstrap, ProviderSource, resolve_provider_type};
use agens_error::{CliError, ExitStatus};
use agens_models::ModelSelection;

pub(crate) const TUI_ERROR_ACTION: &str = "Correct the command or runtime condition, then retry.";

#[derive(Clone)]
pub(crate) struct TuiRuntimeRouter {
    bootstrap: Arc<Mutex<Bootstrap>>,
    pub(crate) session: Arc<Mutex<SessionContext>>,
    cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
    auth: ChatGptAuthCoordinator,
    credentials: CredentialResolver,
    /// Commands, skills, and the derived command palette, bundled behind one lock so a
    /// post-startup resume can swap all three atomically to the resumed session's own root
    /// instead of leaving them pinned to whatever root the router was constructed with.
    extensions: Arc<Mutex<RouterExtensions>>,
    pub(crate) mcp_status: McpStatusHandle,
    _mcp_registry: Arc<Mutex<McpRegistry>>,
    clock: fn() -> i64,
    credential_restorer: Arc<CredentialRestorer>,
}

struct RouterExtensions {
    commands: Arc<CommandCatalog>,
    skills: Arc<SkillCatalog>,
    palette: Vec<PaletteEntry>,
}

type CredentialRestorer =
    dyn Fn(&Path, ChatGptCredentialSnapshot) -> Result<(), CliError> + Send + Sync;

impl TuiRuntimeRouter {
    pub(crate) fn new(
        bootstrap: Bootstrap,
        session: Arc<Mutex<SessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
    ) -> Self {
        Self::with_auth_coordinator(
            bootstrap,
            session,
            cancellation,
            commands,
            skills,
            ChatGptAuthCoordinator::production(),
        )
    }

    pub(crate) fn with_auth_coordinator(
        mut bootstrap: Bootstrap,
        session: Arc<Mutex<SessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
        auth: ChatGptAuthCoordinator,
    ) -> Self {
        let has_subagents = session.lock().is_ok_and(|context| {
            subagent_catalog(&bootstrap, &context).is_ok_and(|mut agents| agents.next().is_some())
        });
        let palette = resolved_tui_palette(&commands, &skills, has_subagents);
        let project_root = bootstrap.project_root.as_deref().unwrap_or(Path::new("."));
        let registry = Arc::new(Mutex::new(load_configured_mcp_registry(
            &bootstrap,
            project_root,
        )));
        let mcp_status = registry
            .lock()
            .expect("new MCP registry lock")
            .status_handle();
        bootstrap.mcp_status = Some(mcp_status.clone());
        Self {
            bootstrap: Arc::new(Mutex::new(bootstrap)),
            session,
            cancellation,
            auth,
            credentials: CredentialResolver::production(),
            extensions: Arc::new(Mutex::new(RouterExtensions {
                commands,
                skills,
                palette,
            })),
            mcp_status,
            _mcp_registry: registry,
            clock: current_session_timestamp,
            credential_restorer: Arc::new(restore_chatgpt_credentials),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_credential_restorer(
        mut self,
        restore: impl Fn(&Path, ChatGptCredentialSnapshot) -> Result<(), CliError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.credential_restorer = Arc::new(restore);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_credential_resolver(
        bootstrap: Bootstrap,
        session: Arc<Mutex<SessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
        credentials: CredentialResolver,
    ) -> Self {
        let mut router = Self::new(bootstrap, session, cancellation, commands, skills);
        router.credentials = credentials;
        router
    }

    #[cfg(test)]
    pub(crate) fn with_clock(
        bootstrap: Bootstrap,
        session: Arc<Mutex<SessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
        clock: fn() -> i64,
    ) -> Self {
        let mut router = Self::new(bootstrap, session, cancellation, commands, skills);
        router.clock = clock;
        router
    }

    #[cfg(test)]
    pub(crate) fn route(&self, input: String) -> TuiSubmissionOutcome {
        let (progress, _) = std::sync::mpsc::channel();
        self.route_with_progress(input, progress)
    }

    #[cfg(test)]
    pub(crate) fn route_with_progress(
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

    #[cfg(test)]
    pub(crate) fn route_request(
        &self,
        request: TuiRouteRequest,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) -> TuiSubmissionOutcome {
        self.route_request_with_cancellation(request, progress, TuiRouteCancellation::new())
    }

    pub(crate) fn route_request_with_cancellation(
        &self,
        request: TuiRouteRequest,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
        cancellation: TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        let result = match request {
            TuiRouteRequest::Input(input) => {
                return self.route_with_progress_cancellable(input, progress, cancellation);
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

    pub(crate) fn open_dialog(&self, route_id: &str) -> Result<TuiSubmissionOutcome, CliError> {
        let bootstrap = self.bootstrap()?;
        let dialog = match route_id {
            "dangerous" => return self.toggle_dangerous_mode(),
            "connect" => DialogView::selection(
                "Connect to ChatGPT",
                Some("Choose an authentication flow"),
                vec![
                    DialogEntry::action("Browser", "connect:browser"),
                    DialogEntry::action("Device Code", "connect:device"),
                ],
            ),
            "disconnect" => DialogView::selection(
                "Disconnect from ChatGPT",
                Some("Remove stored ChatGPT credentials?"),
                vec![
                    DialogEntry::action("Disconnect", "disconnect:confirm"),
                    DialogEntry::cancel("Cancel"),
                ],
            ),
            "diagnostics" => diagnostics_dialog(bootstrap.data_directory()),
            "provider" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                let current = current_tui_provider(&bootstrap, &context);
                let entries = ProviderKind::ALL
                    .into_iter()
                    .filter_map(|provider| {
                        let status = self
                            .credentials
                            .status(&bootstrap.paths.credentials, provider);
                        status.available().then(|| {
                            let label = if Some(provider) == current {
                                format!("{} (current)", provider.label())
                            } else {
                                provider.label().to_owned()
                            };
                            DialogEntry::action_with_detail(
                                label,
                                Some(status.label()),
                                format!("provider:{}", provider.identifier()),
                            )
                        })
                    })
                    .collect();
                let help = current.map_or_else(
                    || "Current: not configured".to_owned(),
                    |provider| {
                        let status = self
                            .credentials
                            .status(&bootstrap.paths.credentials, provider);
                        let remediation = matches!(status, CredentialStatus::ConnectRequired)
                            .then_some(" · run /connect")
                            .unwrap_or_default();
                        format!(
                            "Current: {} · {}{remediation}",
                            provider.label(),
                            status.label()
                        )
                    },
                );
                DialogView::selection("Choose provider", Some(help), entries)
            }
            "model" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                let current = context
                    .selection
                    .as_ref()
                    .map(ModelSelection::model)
                    .or_else(|| bootstrap.model())
                    .unwrap_or_else(|| default_model(&bootstrap))
                    .to_owned();
                let source = tui_model_source(&bootstrap, &context);
                drop(context);
                let selector = ModelSelection::for_source(current.clone(), source);
                let values = selector.models().map_err(CliError::unavailable)?;
                let selected = values
                    .iter()
                    .position(|model| model.id == current)
                    .unwrap_or_default();
                let entries = values
                    .into_iter()
                    .map(|model| {
                        let label = if model.id == current {
                            format!("{} (current)", model.id)
                        } else {
                            model.id.clone()
                        };
                        DialogEntry::action_with_detail(
                            label,
                            Some(format_model_metadata(&model)),
                            format!("model:{}", model.id),
                        )
                    })
                    .collect();
                DialogView::selection(
                    "Choose model",
                    Some(format!("Source: {}", selector.source_label())),
                    entries,
                )
                .with_selected(selected)
                .with_identifier_query_action(
                    "Use ",
                    " (unverified metadata)",
                    "model-custom:",
                    64,
                )
            }
            "effort" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                let model = context
                    .selection
                    .as_ref()
                    .map(ModelSelection::model)
                    .or_else(|| bootstrap.model())
                    .unwrap_or_else(|| default_model(&bootstrap));
                let selector = context.selection.clone().unwrap_or_else(|| {
                    ModelSelection::for_source(model, tui_model_source(&bootstrap, &context))
                });
                let current = selector.reasoning_effort().unwrap_or("default");
                let values = selector.reasoning_effort_values();
                let help = selector.reasoning_effort_default().map_or_else(
                    || format!("Model: {model}"),
                    |effort| format!("Model: {model} · Default: {effort}"),
                );
                let selected = values
                    .iter()
                    .position(|effort| *effort == current)
                    .unwrap_or_default();
                let entries = values
                    .into_iter()
                    .map(|effort| {
                        let name = if effort == "default" {
                            "Default"
                        } else {
                            effort
                        };
                        let label = if effort == current {
                            format!("{name} (current)")
                        } else {
                            name.to_owned()
                        };
                        DialogEntry::action(label, format!("effort:{effort}"))
                    })
                    .collect();
                DialogView::selection("Choose effort", Some(help), entries).with_selected(selected)
            }
            "help" => DialogView::selection(
                "Commands and skills",
                Some(render_tui_help(&self.palette_entries()?)),
                Vec::new(),
            ),
            "mcp" => mcp_status_dialog(self.mcp_status.snapshot()),
            "select" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                let entries = tui_select_candidates(&context, &bootstrap)?
                    .into_iter()
                    .map(|path| DialogEntry::safe_action(&path, format!("select:{path}")))
                    .collect();
                return Ok(TuiSubmissionOutcome::SafeDialog(
                    DialogView::selection(
                        "Select project file",
                        Some("Choose one approved file"),
                        entries,
                    )
                    .with_empty_message("No approved project files are available.")
                    .with_cancellation_action("select:cancel"),
                ));
            }
            "sessions" => {
                return Ok(self.session_dialog_outcome(SessionDialogRequest::initial()));
            }
            "agent" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                let catalog = agent_catalog_for_context(&bootstrap, &context)?;
                let current = context
                    .active_agent
                    .as_ref()
                    .map(|agent| agent.name.as_str())
                    .or_else(|| {
                        context
                            .metadata
                            .as_ref()
                            .map(|metadata| metadata.active_agent.as_str())
                    })
                    .unwrap_or("primary");
                let entries = catalog
                    .primary_or_all()
                    .map(|agent| {
                        let label = if agent.name == current {
                            format!("{} (current)", agent.name)
                        } else {
                            agent.name.clone()
                        };
                        DialogEntry::action(label, format!("agent:{}", agent.name))
                    })
                    .collect();
                DialogView::selection("Choose agent", Some("Eligible primary agents"), entries)
            }
            "subagent" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                let entries = subagent_catalog(&bootstrap, &context)?
                    .map(|agent| {
                        DialogEntry::action(&agent.name, format!("subagent:{}", agent.name))
                    })
                    .collect();
                DialogView::selection("Choose subagent", Some("Eligible subagents"), entries)
                    .with_empty_message("No eligible subagents are available.")
            }
            _ => return Err(CliError::usage("TUI dialog is unavailable")),
        };
        if route_id == "subagent" {
            Ok(TuiSubmissionOutcome::SafeDialog(dialog))
        } else {
            Ok(TuiSubmissionOutcome::Dialog(dialog))
        }
    }

    fn session_dialog_outcome(&self, request: SessionDialogRequest) -> TuiSubmissionOutcome {
        let fallback_request = request.clone();
        match self.load_session_dialog(request) {
            Ok(dialog) => TuiSubmissionOutcome::Dialog(dialog),
            Err(_) => TuiSubmissionOutcome::Dialog(DialogView::sessions_error(
                fallback_request,
                "Saved sessions could not be loaded.",
            )),
        }
    }

    fn load_session_dialog(&self, request: SessionDialogRequest) -> Result<DialogView, CliError> {
        let bootstrap = self.bootstrap()?;
        let project = tui_project_identifier(&bootstrap)?;
        let project = match request.scope() {
            SessionDialogScope::CurrentProject => Some(project.as_str()),
            SessionDialogScope::AllProjects => None,
        };
        let cursor = request
            .cursor()
            .map(|cursor| SessionCursor::new(cursor.updated_at(), cursor.id()));
        let store = SessionStore::open(bootstrap.data_directory())
            .map_err(|_| CliError::storage("sessions database is unavailable"))?;
        let page = store
            .list_session_page(project, request.query(), cursor, 64)
            .map_err(|_| CliError::storage("saved sessions could not be listed"))?;
        let current_session = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?
            .identifier;
        let now = (self.clock)();
        let show_project = request.scope() == SessionDialogScope::AllProjects;
        let entries = page
            .sessions
            .iter()
            .map(|session| session_dialog_entry(session, current_session, show_project, now))
            .collect();
        let next_cursor = page
            .next_cursor
            .map(|cursor| SessionDialogCursor::new(cursor.updated_at(), cursor.id()));

        Ok(DialogView::sessions_page(entries, request, next_cursor))
    }

    #[cfg(test)]
    pub(crate) fn route_dialog_action(
        &self,
        action_id: &str,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) -> TuiSubmissionOutcome {
        self.route_dialog_action_with_cancellation(
            action_id,
            progress,
            &TuiRouteCancellation::new(),
        )
    }

    fn route_dialog_action_with_cancellation(
        &self,
        action_id: &str,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
        cancellation: &TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        match action_id {
            "connect:browser" => {
                return auth_route_outcome(self.connect(ChatGptAuthFlow::Browser, progress));
            }
            "connect:device" => {
                return auth_route_outcome(self.connect(ChatGptAuthFlow::Device, progress));
            }
            "disconnect:confirm" => return auth_route_outcome(self.disconnect()),
            _ => {}
        }
        let result =
            (|| {
                let bootstrap = self.bootstrap()?;
                if action_id == "select:cancel" {
                    return Ok(TuiSubmissionOutcome::SelectionCancelled);
                }
                if let Some(path) = action_id.strip_prefix("select:") {
                    let context = self
                        .session
                        .lock()
                        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                    return selected_tui_file(&context, &bootstrap, path).map(|path| {
                        TuiSubmissionOutcome::SelectionInfo(format!("Selected file: {path}"))
                    });
                }
                if let Some(key) = parse_recovery_action(action_id) {
                    return self.recover_tui_session_attempt(&bootstrap, key);
                }
                if let Some(identifier) = action_id.strip_prefix("session:") {
                    let expected = self
                        .session
                        .lock()
                        .map_err(|_| CliError::storage("TUI session is unavailable"))?
                        .clone();
                    let identifier = identifier
                        .parse()
                        .map_err(|_| CliError::usage("session action is invalid"))?;
                    let stored = load_tui_session_for_resume(&bootstrap, identifier)?;
                    if let Some(attempt) = stored.latest_attempt.as_ref().filter(|attempt| {
                        attempt.status() == agens_core::SessionAttemptStatus::Running
                    }) {
                        return Ok(TuiSubmissionOutcome::Dialog(recovery_confirmation_dialog(
                            &stored.metadata,
                            attempt,
                            None,
                        )));
                    }
                    let resumed = prepare_loaded_tui_session_resume(
                        &bootstrap,
                        identifier,
                        stored,
                        &self.credentials,
                    )?;
                    return commit_tui_session_resume(
                        &bootstrap,
                        &self.session,
                        &expected,
                        resumed,
                        cancellation,
                        |context| self.on_session_resume_committed(&bootstrap, context),
                    );
                }
                let message = if let Some(model) = action_id.strip_prefix("model:") {
                    apply_tui_model(&bootstrap, model, &self.session)?
                } else if let Some(model) = action_id.strip_prefix("model-custom:") {
                    apply_tui_unverified_model(&bootstrap, model, &self.session)?
                } else if let Some(provider) = action_id.strip_prefix("provider:") {
                    self.apply_provider(&bootstrap, provider)?
                } else if let Some(effort) = action_id.strip_prefix("effort:") {
                    apply_tui_effort(&bootstrap, effort, &self.session)?
                } else if let Some(agent) = action_id.strip_prefix("agent:") {
                    rotate_agent(&bootstrap, agent, &self.session, self.skills()?.as_ref())?
                } else if let Some(agent) = action_id.strip_prefix("subagent:") {
                    select_subagent(&bootstrap, agent, &self.session)?
                } else {
                    return Err(CliError::usage("TUI dialog action is unavailable"));
                };
                Ok(TuiSubmissionOutcome::ContextChanged {
                    message,
                    presentation: self.presentation()?,
                })
            })();
        match result {
            Ok(outcome) => outcome,
            Err(error) if action_id.starts_with("select:") => {
                TuiSubmissionOutcome::SelectionError {
                    message: error.to_string(),
                    action: TUI_ERROR_ACTION.into(),
                }
            }
            Err(error) => TuiSubmissionOutcome::LocalActionableError {
                message: error.to_string(),
                action: TUI_ERROR_ACTION.into(),
            },
        }
    }

    fn recover_tui_session_attempt(
        &self,
        bootstrap: &Bootstrap,
        key: AttemptKey,
    ) -> Result<TuiSubmissionOutcome, CliError> {
        let mut store = SessionStore::open(bootstrap.data_directory())
            .map_err(|_| CliError::storage("sessions database is unavailable"))?;
        let stored = store
            .load_session_for_resume(key.session_id())
            .map_err(|_| CliError::storage("saved session is unavailable"))?;
        let Some(attempt) = stored.latest_attempt.as_ref().filter(|attempt| {
            attempt.key() == key && attempt.status() == agens_core::SessionAttemptStatus::Running
        }) else {
            return self.open_dialog("sessions");
        };

        let recovery = active_session_attempts()
            .recover_running_attempt(&mut store, key, current_session_timestamp())
            .map_err(|_| CliError::storage("attempt recovery failed"))?;
        let Some(recovery) = recovery else {
            return Ok(TuiSubmissionOutcome::Dialog(recovery_confirmation_dialog(
                &stored.metadata,
                attempt,
                Some("Recovery was refused because this attempt is active in this process."),
            )));
        };
        if recovery == RecoveryOutcome::Stale {
            return self.open_dialog("sessions");
        }

        let boundary = store
            .load_retry_boundary(key)
            .map_err(|_| CliError::storage("attempt recovery failed"))?
            .ok_or_else(|| CliError::storage("attempt recovery failed"))?;
        drop(store);

        // Recovery replaces the live session without re-rendering its history, so the
        // handoff is dropped here. It used to be parked in the session context on this
        // path and never read back.
        let ResumedTuiSession {
            context: mut resumed,
            history: _,
        } = resume_tui_session(
            bootstrap,
            key.session_id(),
            self.skills()?.as_ref(),
            &self.credentials,
        )?;
        persist_pending_agent_correction(bootstrap, &mut resumed);
        self.refresh_session_extensions(bootstrap, &resumed);
        let prompt = boundary.prompt().to_owned();
        *self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))? = resumed;

        Ok(TuiSubmissionOutcome::ProviderTurn {
            display: "Retrying recovered attempt.".into(),
            prompt,
        })
    }

    pub(crate) fn skills(&self) -> Result<Arc<SkillCatalog>, CliError> {
        self.extensions
            .lock()
            .map(|extensions| Arc::clone(&extensions.skills))
            .map_err(|_| CliError::storage("TUI extension catalogs are unavailable"))
    }

    fn commands(&self) -> Result<Arc<CommandCatalog>, CliError> {
        self.extensions
            .lock()
            .map(|extensions| Arc::clone(&extensions.commands))
            .map_err(|_| CliError::storage("TUI extension catalogs are unavailable"))
    }

    pub(crate) fn palette_entries(&self) -> Result<Vec<PaletteEntry>, CliError> {
        self.extensions
            .lock()
            .map(|extensions| extensions.palette.clone())
            .map_err(|_| CliError::storage("TUI extension catalogs are unavailable"))
    }

    /// Re-discovers commands, skills, and the derived palette from the session's OWN recorded
    /// root, after a post-startup resume may have changed that root out from under this router.
    ///
    /// Best-effort: on any discovery failure the previously held catalogs are left in place,
    /// mirroring the same graceful degradation startup already applies (`tui.add_info` there,
    /// nothing to report to here since a resume commit has no `Tui` handle). This must run for
    /// every resume that lands in the live session slot, or the catalogs captured once at startup
    /// keep feeding a DIFFERENT root's skill bodies and command templates into every later turn as
    /// model-facing instruction text.
    fn refresh_session_extensions(&self, bootstrap: &Bootstrap, context: &SessionContext) {
        let Ok(project_root) = crate::session::root::resolve_tui_session_root(context, bootstrap)
        else {
            return;
        };
        let Ok(commands_discovery) = discover_tui_command_catalog(bootstrap, &project_root) else {
            return;
        };
        let Ok(skills_discovery) = discover_skill_catalog(bootstrap, &project_root) else {
            return;
        };
        let commands = Arc::new(commands_discovery.catalog().clone());
        let skills = Arc::new(skills_discovery.catalog().clone());
        let has_subagents =
            subagent_catalog(bootstrap, context).is_ok_and(|mut agents| agents.next().is_some());
        let palette = resolved_tui_palette(&commands, &skills, has_subagents);
        if let Ok(mut extensions) = self.extensions.lock() {
            extensions.commands = commands;
            extensions.skills = skills;
            extensions.palette = palette;
        }
    }

    /// Called by [`commit_tui_session_resume`] exactly once a resume has actually won its commit
    /// race (never on a rejected, stale, or cancelled attempt), with the context that is about to
    /// become the live session. Refreshes every session-scoped derived surface — the command and
    /// skill catalogs, the `@` picker candidate list, and the composer's rendered palette — from
    /// that context's own root, and returns the picker candidates and palette entries for the
    /// `SessionResumed` outcome to apply to the `Tui`.
    fn on_session_resume_committed(
        &self,
        bootstrap: &Bootstrap,
        context: &SessionContext,
    ) -> (Vec<String>, Vec<PaletteEntry>) {
        self.refresh_session_extensions(bootstrap, context);
        let file_candidates = tui_picker_file_candidates(context, bootstrap).unwrap_or_default();
        let palette_entries = self.palette_entries().unwrap_or_default();
        (file_candidates, palette_entries)
    }

    #[cfg(test)]
    pub(crate) fn resolve(&self, input: String) -> Result<TuiSubmissionOutcome, CliError> {
        self.resolve_with_cancellation(input, &TuiRouteCancellation::new())
    }

    fn resolve_with_cancellation(
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
            "/help" => self.open_dialog("help")?,
            "/mcp" => self.open_dialog("mcp")?,
            "/select" => self.open_dialog("select")?,
            "/quit" => TuiSubmissionOutcome::Quit,
            "/sessions" | "/resume" => self.open_dialog("sessions")?,
            "/connect" => self.open_dialog("connect")?,
            "/disconnect" => self.open_dialog("disconnect")?,
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
                drop(session);
                TuiSubmissionOutcome::ResetSucceeded {
                    message: "Started a new session.".into(),
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

    pub(crate) fn presentation(&self) -> Result<TuiPresentation, CliError> {
        let bootstrap = self.bootstrap()?;
        let session = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        Ok(tui_session_presentation(&bootstrap, &session))
    }

    fn toggle_dangerous_mode(&self) -> Result<TuiSubmissionOutcome, CliError> {
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

    pub(crate) fn bootstrap(&self) -> Result<Bootstrap, CliError> {
        self.bootstrap
            .lock()
            .map(|bootstrap| bootstrap.clone())
            .map_err(|_| CliError::storage("TUI provider state is unavailable"))
    }

    pub(crate) fn turn_bootstrap(&self) -> Result<Bootstrap, CliError> {
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
        let provider = current_tui_provider(&bootstrap, &context)
            .ok_or_else(|| CliError::configuration("TUI provider is unavailable"))?;
        if let Some(selection) = &context.selection {
            bootstrap.model = Some(selection.model().to_owned());
        }
        drop(context);

        bootstrap.provider_type = Some(provider.identifier().into());
        bootstrap.openai_api_key = match provider {
            ProviderKind::OpenAiApi => Some(
                self.credentials
                    .api_key(&bootstrap.paths.credentials)
                    .ok_or_else(|| {
                        CliError::authentication("OpenAI API authentication is unavailable")
                    })?,
            ),
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
        };
        Ok(bootstrap)
    }

    pub(crate) fn task_parent_request_config(&self) -> Result<agens_core::RequestConfig, CliError> {
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

    pub(crate) fn connect(
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

    pub(crate) fn disconnect(&self) -> Result<String, AuthRouteError> {
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

    fn reconcile_provider(&self, connected: bool) -> Result<(), CliError> {
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
                    )
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

    fn mark_chatgpt_unavailable(&self) -> Result<(), CliError> {
        let mut context = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        context.provider = None;
        context.chatgpt_unavailable = true;
        context.active_agent = None;
        Ok(())
    }

    fn apply_provider(&self, bootstrap: &Bootstrap, provider: &str) -> Result<String, CliError> {
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

        let current_model = effective_tui_model(bootstrap, &context);
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
            let default = ["gpt-4.1", "gpt-5.5"][provider as usize];
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

pub(crate) enum AuthRouteError {
    Auth(chatgpt_auth::ChatGptAuthError),
    Runtime(CliError),
}

pub(crate) fn auth_route_outcome(result: Result<String, AuthRouteError>) -> TuiSubmissionOutcome {
    match result {
        Ok(message) => TuiSubmissionOutcome::LocalInfo(message),
        Err(AuthRouteError::Auth(error)) => TuiSubmissionOutcome::LocalActionableError {
            message: error.message().into(),
            action: error.action().into(),
        },
        Err(AuthRouteError::Runtime(error)) => TuiSubmissionOutcome::LocalActionableError {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
        },
    }
}

pub(crate) fn tui_provider_outcome(result: Result<String, CliError>) -> TuiProviderOutcome {
    match result {
        Ok(output) => TuiProviderOutcome::Completed(output),
        Err(error) if error.category == "cancelled" => TuiProviderOutcome::Cancelled {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
        },
        Err(error) if error.message == "request exceeds the model context window" => {
            TuiProviderOutcome::Failed {
                message: error.to_string(),
                action: "Start a new session or shorten the prompt, then retry.".into(),
            }
        }
        Err(error) if error.message == "network request failed" => TuiProviderOutcome::Failed {
            message: error.to_string(),
            action: "Check the network connection, then retry.".into(),
        },
        Err(error) => TuiProviderOutcome::Failed {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use agens_core::{
        CompletedSessionTurn, Message, MessagePart, Role, SessionMessage, SessionMetadata,
    };
    use agens_providers::chatgpt_login::upsert_provider_entry;
    use agens_store::{SessionStore, StoredSession};
    use agens_tui::{Action, Event, Key, Tui};

    use super::*;
    use crate::headless::HeadlessChatCompletion;
    use crate::test_support::{
        bootstrap_from_a_different_working_directory, dispatch_tui_dialog_selection,
        enter_tui_input, open_tui_palette_dialog, persist_tui_session,
        persist_tui_session_metadata, render_tui_test_backend, rotation_dispatcher,
        run_production_batch, submit_tui_command, tui_project, tui_session_bootstrap,
        tui_session_bootstrap_for_provider, tui_session_directory, tui_session_messages,
    };
    use crate::tui::engine::{ProductionTuiEngine, run_tui_prompt_with};
    use crate::tui::extensions::{start_tui_commands, start_tui_skills};
    use crate::tui::resume::ensure_active_tui_agent_runtime;
    use agens_models::ModelSource;

    fn write_router_test_skill(root: &Path, name: &str, body: &str) {
        let directory = root.join(".agens/skills").join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name}\n---\n{body}\n"),
        )
        .unwrap();
    }

    struct CatalogConfinementFixture {
        origin: std::path::PathBuf,
        elsewhere_root: std::path::PathBuf,
        metadata_id: i64,
        router: TuiRuntimeRouter,
        tui: Tui<ProductionTuiEngine>,
    }

    /// Builds a session confined to root A and a router whose STARTUP catalogs were discovered
    /// from a DIFFERENT root B (matching production, where no `--resume` flag was given), with
    /// one skill and one distinctly named file unique to each root.
    fn catalog_confinement_fixture(label: &str) -> CatalogConfinementFixture {
        let origin = tui_session_directory(&format!("{label}-origin"));
        let origin_root = origin.join("project");
        write_router_test_skill(&origin_root, "askill", "INSTRUCTIONS-FROM-ROOT-A");
        std::fs::write(origin_root.join("only-in-a.txt"), "a").unwrap();
        let creation_bootstrap = tui_session_bootstrap(&origin, &[]);
        let mut store = SessionStore::open(creation_bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&origin), "origin");
        drop(store);

        let resume_bootstrap =
            bootstrap_from_a_different_working_directory(&origin, &format!("{label}-elsewhere"));
        let elsewhere_root =
            agens_bootstrap::session_root::discovered_root_for_tests(&resume_bootstrap);
        write_router_test_skill(&elsewhere_root, "bskill", "INSTRUCTIONS-FROM-ROOT-B");
        std::fs::write(elsewhere_root.join("only-in-elsewhere.txt"), "b").unwrap();

        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let startup_commands =
            start_tui_commands(&mut tui, &resume_bootstrap, &elsewhere_root).unwrap();
        let startup_skills =
            start_tui_skills(&mut tui, &resume_bootstrap, &elsewhere_root).unwrap();
        tui.set_palette_entries(resolved_tui_palette(
            &startup_commands,
            &startup_skills,
            false,
        ));
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            resume_bootstrap,
            session,
            cancellation,
            startup_commands,
            startup_skills,
        );

        CatalogConfinementFixture {
            origin,
            elsewhere_root,
            metadata_id: metadata.id,
            router,
            tui,
        }
    }

    impl Drop for CatalogConfinementFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.origin);
            let _ = std::fs::remove_dir_all(self.elsewhere_root.parent().unwrap());
        }
    }

    fn assert_router_confined_to_root_a(router: &TuiRuntimeRouter) {
        assert!(
            matches!(
                router.route("/bskill args".into()),
                TuiSubmissionOutcome::LocalActionableError { .. }
            ),
            "root B's skill must no longer be reachable once the session is confined to root A"
        );
        let TuiSubmissionOutcome::ProviderTurn { prompt, .. } = router.route("/askill args".into())
        else {
            panic!("root A's own skill must be reachable after resuming into root A");
        };
        assert!(prompt.contains("INSTRUCTIONS-FROM-ROOT-A"), "{prompt:?}");
    }

    /// Proves C-NEW is closed: a post-startup `/resume <id>` across a differently rooted
    /// process re-discovers the router's command/skill catalogs from the RESUMED session's own
    /// root, instead of keeping the catalogs the router was constructed with at startup.
    ///
    /// Mirrors the verifier's own probe exactly: a session created under root A is resumed from
    /// a router whose STARTUP catalogs came from a different root B (matching production, where
    /// no `--resume` flag was given and the process's own root is B). Before the fix, `/bskill`
    /// (root B's skill, model-facing instruction text) would remain reachable, and root A's own
    /// `/askill` would stay unknown, even though the session is now confined to A.
    #[test]
    fn a_post_startup_resume_command_refreshes_commands_skills_and_picker_candidates() {
        let fixture = catalog_confinement_fixture("catalog-confinement-resume-command");

        assert!(
            matches!(
                fixture.router.route("/bskill args".into()),
                TuiSubmissionOutcome::ProviderTurn { .. }
            ),
            "root B's skill must be reachable before any resume, matching production startup"
        );

        let resume_outcome = fixture
            .router
            .route(format!("/resume {}", fixture.metadata_id));
        let TuiSubmissionOutcome::SessionResumed {
            file_candidates, ..
        } = resume_outcome
        else {
            panic!("expected a successful resume, got {resume_outcome:?}");
        };

        assert_router_confined_to_root_a(&fixture.router);
        assert_eq!(
            file_candidates,
            vec!["only-in-a.txt".to_owned()],
            "the picker candidates on the resume outcome must enumerate the resumed \
             session's own root, not the resuming process's discovered root"
        );
    }

    /// Same proof as the `/resume <id>` test above, but through the session-picker dialog action
    /// (`session:{id}`) instead of the slash command — the second of the three post-startup
    /// resume entry points named by C-NEW.
    #[test]
    fn a_post_startup_session_picker_action_refreshes_commands_skills_and_picker_candidates() {
        let fixture = catalog_confinement_fixture("catalog-confinement-picker-action");

        let resume_outcome = fixture.router.route_dialog_action(
            &format!("session:{}", fixture.metadata_id),
            std::sync::mpsc::channel().0,
        );
        let TuiSubmissionOutcome::SessionResumed {
            file_candidates, ..
        } = resume_outcome
        else {
            panic!("expected a successful resume, got {resume_outcome:?}");
        };

        assert_router_confined_to_root_a(&fixture.router);
        assert_eq!(file_candidates, vec!["only-in-a.txt".to_owned()]);
    }

    /// Same proof again, but through interrupted-attempt recovery (`session:recover:<id>:<n>`)
    /// — the third of the three post-startup resume entry points named by C-NEW. This path
    /// returns `ProviderTurn`, not `SessionResumed`, so it has no picker candidates to check.
    #[test]
    fn a_post_startup_recovery_action_refreshes_commands_and_skills() {
        let fixture = catalog_confinement_fixture("catalog-confinement-recovery");
        let bootstrap = fixture.router.bootstrap().unwrap();
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let stored = store.load_session_for_resume(fixture.metadata_id).unwrap();
        let attempt = store
            .begin_session_attempt(&stored.metadata, "recovered prompt".into())
            .unwrap();
        drop(store);

        let outcome = fixture.router.route_dialog_action(
            &format!(
                "session:recover:{}:{}",
                attempt.key().session_id(),
                attempt.key().attempt_id()
            ),
            std::sync::mpsc::channel().0,
        );
        assert!(
            matches!(
                &outcome,
                TuiSubmissionOutcome::ProviderTurn { prompt, .. } if prompt == "recovered prompt"
            ),
            "{outcome:?}"
        );

        assert_router_confined_to_root_a(&fixture.router);
    }

    /// The router's own palette cache is refreshed on a post-startup `/resume` (proven above by
    /// `assert_router_confined_to_root_a`'s `/bskill` check and the file-candidate assertions),
    /// but `Tui::set_palette_entries` is only ever called once, at startup. A post-startup resume
    /// must still refresh the composer's OWN copy — the one it actually renders from — or root
    /// B's skill and command names keep showing up in the composer's autocomplete after the
    /// session is confined to root A.
    #[test]
    fn a_post_startup_resume_refreshes_the_composers_own_palette_not_just_the_routers() {
        let mut fixture = catalog_confinement_fixture("catalog-confinement-composer-palette");

        let palette_before_resume: Vec<String> = fixture
            .tui
            .view()
            .palette
            .map(|palette| {
                palette
                    .entries()
                    .iter()
                    .map(|entry| entry.name().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            palette_before_resume.is_empty(),
            "the palette dialog is not open yet, so the view must not surface any entries"
        );

        let resume_outcome = fixture
            .router
            .route(format!("/resume {}", fixture.metadata_id));
        assert!(
            fixture
                .tui
                .apply_submission_outcome(resume_outcome)
                .is_none(),
            "a session resume does not launch a provider turn"
        );

        fixture
            .tui
            .handle(agens_tui::Event::Key(agens_tui::Key::Char('/')));
        let rendered_palette_names: Vec<String> = fixture
            .tui
            .view()
            .palette
            .expect("typing a leading '/' opens the palette")
            .entries()
            .iter()
            .map(|entry| entry.name().to_owned())
            .collect();

        assert!(
            !rendered_palette_names.iter().any(|name| name == "bskill"),
            "the composer's OWN rendered palette must not keep listing root B's skill after the \
             session is confined to root A: {rendered_palette_names:?}"
        );
        assert!(
            rendered_palette_names.iter().any(|name| name == "askill"),
            "root A's own skill must be present in the composer's rendered palette after resume: \
             {rendered_palette_names:?}"
        );
    }

    fn test_chatgpt_credentials(
        access_token: &str,
    ) -> agens_providers::chatgpt_login::ChatGptCredentials {
        agens_providers::chatgpt_login::ChatGptCredentials {
            access_token: access_token.into(),
            refresh_token: "refresh".into(),
            account_id: "account".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn tui_enter_routes_unknown_slash_and_local_output_without_provider_history() {
        let temporary = tui_session_directory("enter-local-routing");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "current");
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine { cancellation });
        let input = enter_tui_input(&mut tui, "/unknown");
        let provider_invocations =
            usize::from(tui.apply_submission_outcome(router.route(input)).is_some());
        assert_eq!(provider_invocations, 0);
        assert!(tui.transcript().is_empty());
        assert!(tui.view().dialog.is_some());

        session.lock().unwrap().running = true;
        let input = enter_tui_input(&mut tui, "/new");
        tui.apply_submission_outcome(router.route(input));
        assert!(tui.view().dialog.is_some());

        session.lock().unwrap().running = false;
        let input = enter_tui_input(&mut tui, "/new");
        tui.apply_submission_outcome(router.route(input));
        assert!(tui.transcript().is_empty());
        assert_eq!(tui.view().status, Some("Started a new session."));

        let input = enter_tui_input(&mut tui, &format!("/resume {}", metadata.id));
        tui.apply_submission_outcome(router.route(input));
        assert_eq!(tui.view().session, format!("session #{}", metadata.id));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_model_effort_and_help_palette_routes_open_local_overlays_and_dispatch_once() {
        let temporary = tui_session_directory("local-overlays");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        tui.set_palette_entries(router.palette_entries().unwrap());
        let (progress, _) = std::sync::mpsc::channel();

        for (prefix, route_id, expected) in [
            ("/mo", "model", ["Choose model", "gpt-4.1 (current)"]),
            ("/ef", "effort", ["Choose effort", "Default"]),
            ("/he", "help", ["Commands and skills", "/connect"]),
        ] {
            for character in prefix.chars() {
                tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
            }
            let agens_tui::Action::OpenDialog(actual_route) =
                tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter))
            else {
                panic!("palette Enter should open the selected overlay");
            };
            assert_eq!(actual_route, route_id);
            let outcome = router.route_request(
                agens_tui::TuiRouteRequest::OpenDialog(actual_route),
                progress.clone(),
            );
            assert!(tui.apply_submission_outcome(outcome).is_none());
            let text = render_tui_test_backend(&tui, 80, 24);
            assert!(text.contains(expected[0]), "{route_id}: {text:?}");
            assert!(text.contains(expected[1]), "{route_id}: {text:?}");

            if route_id == "help" {
                assert_eq!(
                    tui.handle(agens_tui::Event::Key(agens_tui::Key::CtrlC)),
                    agens_tui::Action::Render
                );
                continue;
            }
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Down));
            let agens_tui::Action::DialogAction(action_id) =
                tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter))
            else {
                panic!("dialog Enter should emit one action ID");
            };
            let outcome = router.route_request(
                agens_tui::TuiRouteRequest::DialogAction(action_id),
                progress.clone(),
            );
            assert!(tui.apply_submission_outcome(outcome).is_none());
            assert!(tui.view().dialog.is_none());
        }

        assert!(session.lock().unwrap().messages.is_empty());
        assert!(
            tui.transcript()
                .iter()
                .all(|entry| !matches!(entry, agens_tui::TranscriptEntry::User(_)))
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_connect_and_disconnect_overlays_select_flows_and_cancel_without_credentials_mutation() {
        let temporary = tui_session_directory("auth-overlays");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        let initial_credentials = r#"{"openai-api":{"api_key":"preserved"}}"#;
        std::fs::write(&credentials_path, initial_credentials).unwrap();
        let flows = Arc::new(Mutex::new(Vec::new()));
        let coordinator = ChatGptAuthCoordinator::with_authenticator({
            let flows = Arc::clone(&flows);
            move |flow, _, publish| {
                flows.lock().unwrap().push(flow);
                publish(ChatGptAuthProgress::BrowserUrl("auth-url".into()));
                Ok(test_chatgpt_credentials("new-access"))
            }
        });
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::with_auth_coordinator(
            tui_session_bootstrap(&temporary, &[]),
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            coordinator,
        );
        tui.set_palette_entries(router.palette_entries().unwrap());
        let (progress, _) = std::sync::mpsc::channel();

        for (prefix, down, flow) in [
            ("/co", false, ChatGptAuthFlow::Browser),
            ("/co", true, ChatGptAuthFlow::Device),
        ] {
            open_tui_palette_dialog(&mut tui, &router, prefix, "connect", progress.clone());
            if down {
                tui.handle(Event::Key(Key::Down));
            }
            dispatch_tui_dialog_selection(&mut tui, &router, progress.clone());
            assert_eq!(flows.lock().unwrap().last(), Some(&flow));
        }

        open_tui_palette_dialog(&mut tui, &router, "/di", "disconnect", progress.clone());
        let connected = std::fs::read_to_string(&credentials_path).unwrap();
        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        let after_cancel = std::fs::read_to_string(&credentials_path).unwrap();
        assert_eq!(after_cancel, connected);
        open_tui_palette_dialog(&mut tui, &router, "/di", "disconnect", progress);
        dispatch_tui_dialog_selection(&mut tui, &router, std::sync::mpsc::channel().0);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_router_connect_device_disconnect_uses_coordinator_without_provider_history() {
        let temporary = tui_session_directory("auth-router");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"openai-api":{"api_key":"preserved"},"other":{"value":"kept"}}"#,
        )
        .unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::Auto;
        bootstrap.provider_type = Some("openai-api".into());
        bootstrap.openai_api_key = Some("preserved".into());
        let flows = Arc::new(Mutex::new(Vec::new()));
        let coordinator = ChatGptAuthCoordinator::with_authenticator({
            let flows = Arc::clone(&flows);
            move |flow, _, publish| {
                flows.lock().unwrap().push(flow);
                publish(ChatGptAuthProgress::BrowserUrl("auth-url".into()));
                Ok(test_chatgpt_credentials("new-access"))
            }
        });
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let router = TuiRuntimeRouter::with_auth_coordinator(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            coordinator,
        );
        let (progress_tx, progress_rx) = std::sync::mpsc::channel();

        assert!(matches!(
            router.route_with_progress("/connect --device-auth".into(), progress_tx),
            TuiSubmissionOutcome::LocalInfo(_)
        ));
        assert_eq!(progress_rx.try_iter().count(), 1);
        assert_eq!(*flows.lock().unwrap(), vec![ChatGptAuthFlow::Device]);
        let context = session.lock().unwrap();
        assert_eq!(context.provider, Some(ProviderKind::OpenAiChatGpt));
        assert!(context.messages.is_empty());
        drop(context);
        let configured = router.bootstrap().unwrap();
        assert_eq!(configured.provider_type(), Some("openai-api"));
        let connected = std::fs::read_to_string(&credentials_path).unwrap();
        assert!(connected.contains("new-access"));

        assert!(router.disconnect().is_ok());
        assert_eq!(
            session.lock().unwrap().provider,
            Some(ProviderKind::OpenAiApi)
        );
        let stored = std::fs::read_to_string(&credentials_path).unwrap();
        assert!(stored.contains("preserved"));
        assert!(stored.contains("kept"));
        assert!(!stored.contains("new-access"));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_preserves_intervening_unrelated_provider_write() {
        let temporary = tui_session_directory("refresh-rollback");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        let before = br#"{"openai-api":{"api_key":"preserved"},"openai-chatgpt":{"access_token":"old-access","refresh_token":"old-refresh","account_id":"old-account","expires_at":"2099-01-01T00:00:00Z"}}"#;
        std::fs::write(&credentials_path, before).unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::Auto;
        bootstrap.provider_type = Some("openai-api".into());
        bootstrap.openai_api_key = Some("preserved".into());
        let session = Arc::new(Mutex::new(SessionContext {
            running: true,
            ..SessionContext::fresh()
        }));
        let original_runtime = session.lock().unwrap().clone();
        let router = TuiRuntimeRouter::with_auth_coordinator(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            ChatGptAuthCoordinator::with_authenticator(|_, _, _| {
                Ok(test_chatgpt_credentials("new-access"))
            }),
        )
        .with_credential_restorer(|path, snapshot| {
            upsert_provider_entry(path, "other-provider", serde_json::json!({"key": "kept"}))
                .map_err(|_| CliError::storage("unrelated provider write failed"))?;
            restore_chatgpt_credentials(path, snapshot)
        });

        assert!(
            router
                .connect(ChatGptAuthFlow::Browser, std::sync::mpsc::channel().0)
                .is_err()
        );
        let mut expected = serde_json::from_slice::<serde_json::Value>(before).unwrap();
        expected
            .as_object_mut()
            .unwrap()
            .insert("other-provider".into(), serde_json::json!({"key": "kept"}));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&credentials_path).unwrap())
                .unwrap(),
            expected
        );
        assert_eq!(*session.lock().unwrap(), original_runtime);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_disconnects_explicit_chatgpt_fail_closed() {
        let temporary = tui_session_directory("explicit-disconnect");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"openai-api":{"api_key":"preserved"},"openai-chatgpt":{"access_token":"old-access","refresh_token":"old-refresh","account_id":"old-account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::ExplicitChatGpt;
        bootstrap.provider_type = Some("openai-chatgpt".into());
        let session = Arc::new(Mutex::new(SessionContext {
            provider: Some(ProviderKind::OpenAiChatGpt),
            ..SessionContext::fresh()
        }));
        ensure_active_tui_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );

        assert!(router.disconnect().is_ok());
        assert_eq!(session.lock().unwrap().provider, None);
        assert!(session.lock().unwrap().chatgpt_unavailable);
        assert!(session.lock().unwrap().active_agent.is_none());
        let error = match router.turn_bootstrap() {
            Ok(_) => panic!("disconnected ChatGPT runtime must be unavailable"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "auth: ChatGPT credentials are unavailable; run /connect"
        );
        assert!(
            !std::fs::read_to_string(&credentials_path)
                .unwrap()
                .contains("old-access")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_fails_closed_when_credential_restore_fails() {
        let temporary = tui_session_directory("restore-failure");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"openai-api":{"api_key":"preserved"}}"#,
        )
        .unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::Auto;
        bootstrap.provider_type = Some("openai-api".into());
        bootstrap.openai_api_key = Some("preserved".into());
        let session = Arc::new(Mutex::new(SessionContext {
            running: true,
            ..SessionContext::fresh()
        }));
        let router = TuiRuntimeRouter::with_auth_coordinator(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            ChatGptAuthCoordinator::with_authenticator(|_, _, _| {
                Ok(test_chatgpt_credentials("new-access"))
            }),
        )
        .with_credential_restorer(|_, _| Err(CliError::storage("injected restore failure")));

        let outcome = auth_route_outcome(
            router.connect(ChatGptAuthFlow::Browser, std::sync::mpsc::channel().0),
        );
        assert!(matches!(
            outcome,
            TuiSubmissionOutcome::LocalActionableError { message, .. }
                if message == "store: ChatGPT credential recovery failed"
        ));
        assert!(session.lock().unwrap().chatgpt_unavailable);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_preserves_runtime_on_credential_write_failures() {
        let temporary = tui_session_directory("credential-write-failures");
        let config_home = temporary.join("config");
        std::fs::create_dir_all(&config_home).unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.paths.credentials = config_home.clone();
        let session = Arc::new(Mutex::new(SessionContext {
            provider: Some(ProviderKind::OpenAiApi),
            ..SessionContext::fresh()
        }));
        let original_runtime = session.lock().unwrap().clone();
        let router = TuiRuntimeRouter::with_auth_coordinator(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            ChatGptAuthCoordinator::with_authenticator(|_, _, _| {
                Ok(test_chatgpt_credentials("new-access"))
            }),
        );

        for outcome in [
            auth_route_outcome(
                router.connect(ChatGptAuthFlow::Browser, std::sync::mpsc::channel().0),
            ),
            auth_route_outcome(router.disconnect()),
        ] {
            assert!(matches!(
                outcome,
                TuiSubmissionOutcome::LocalActionableError { message, .. }
                    if message == "ChatGPT credentials could not be saved"
            ));
            assert_eq!(*session.lock().unwrap(), original_runtime);
        }

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_leaves_auto_unavailable_after_disconnect_rebuild_failure()
    {
        let temporary = tui_session_directory("auto-disconnect-failure");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"openai-chatgpt":{"access_token":"old-access","refresh_token":"old-refresh","account_id":"old-account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::Auto;
        bootstrap.provider_type = Some("openai-chatgpt".into());
        let session = Arc::new(Mutex::new(SessionContext {
            provider: Some(ProviderKind::OpenAiChatGpt),
            ..SessionContext::fresh()
        }));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );

        assert!(router.disconnect().is_err());
        assert!(session.lock().unwrap().chatgpt_unavailable);
        assert!(
            !std::fs::read_to_string(&credentials_path)
                .unwrap()
                .contains("old-access")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn dangerous_mode_is_visible_press_once_and_next_turn_only() {
        let temporary = tui_session_directory("dangerous-mode");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        tui.set_presentation("openai-api", "gpt-4.1", "new session");

        assert!(!render_tui_test_backend(&tui, 120, 24).contains("agens safe"));

        let Action::OpenDialog(route_id) = tui.handle(Event::Key(Key::CtrlShiftD)) else {
            panic!("Ctrl+Shift+D should route through the dangerous-mode router path");
        };
        assert_eq!(route_id, "dangerous");
        assert!(
            tui.apply_submission_outcome(router.route_request(
                TuiRouteRequest::OpenDialog(route_id),
                std::sync::mpsc::channel().0,
            ))
            .is_none()
        );
        assert!(session.lock().unwrap().dangerous_mode);
        assert!(render_tui_test_backend(&tui, 120, 24).contains("danger"));

        assert!(
            tui.apply_submission_outcome(router.route("/dangerous".into()))
                .is_none()
        );
        assert!(!session.lock().unwrap().dangerous_mode);
        assert!(!render_tui_test_backend(&tui, 120, 24).contains("agens safe"));

        tui.apply_submission_outcome(router.route("/dangerous".into()));
        let result = run_tui_prompt_with(&bootstrap, "next request", &session, None, |request| {
            assert!(request.dangerous_mode);
            assert!(matches!(
                router.route("/dangerous".into()),
                TuiSubmissionOutcome::ContextChanged { .. }
            ));
            assert!(request.dangerous_mode);
            Ok(HeadlessChatCompletion {
                text: "captured".into(),
                metadata: SessionMetadata {
                    id: 1,
                    project: "project".into(),
                    title: "captured".into(),
                    active_agent: "primary".into(),
                    provider_id: None,
                    model_id: None,
                    reasoning_effort: None,
                    created_at: 1,
                    updated_at: 1,
                    completed_turn_count: 1,
                    resumable: true,
                },
                messages: Vec::new(),
            })
        });
        assert!(result.is_ok());
        assert!(!session.lock().unwrap().dangerous_mode);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn u15_c1a_subagent_overlay_and_alias_expose_only_eligible_agents() {
        let temporary = tui_session_directory("u15-c1a-subagents");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[
                (
                    "all",
                    "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
                ),
                (
                    "primary",
                    "---\nname: primary\ndescription: primary\nmode: primary\npermissions: []\n---\nPrimary work.\n",
                ),
                (
                    "invalid-model",
                    "---\nname: invalid-model\ndescription: invalid\nmode: subagent\nmodel: unavailable\npermissions: []\n---\nInvalid work.\n",
                ),
                (
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                ),
            ],
        );
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        assert!(
            router
                .palette_entries()
                .unwrap()
                .iter()
                .any(|entry| entry.name() == "subagent")
        );

        assert!(matches!(
            router.route("/subagent".into()),
            TuiSubmissionOutcome::SafeDialog(_)
        ));
        tui.set_running(true);
        assert!(
            tui.apply_submission_outcome(router.route("/subagent".into()))
                .is_none()
        );
        assert!(tui.view().running);
        let overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(!overlay.contains("main"));
        assert!(overlay.contains("explore"));
        assert!(overlay.contains("general"));
        assert!(overlay.contains("reviewer"));
        assert!(!overlay.contains("all"));
        assert!(!overlay.contains("primary"));
        assert!(!overlay.contains("invalid-model"));
        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::DialogAction("subagent:explore".into())
        );
        assert!(tui.transcript().is_empty());

        assert!(matches!(
            router.route("/subagent reviewer".into()),
            TuiSubmissionOutcome::ContextChanged { .. }
        ));
        assert_eq!(
            session.lock().unwrap().selected_subagent.as_deref(),
            Some("reviewer")
        );
        assert!(matches!(
            router.route("/subagent all".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ));

        let unavailable_bootstrap = tui_session_bootstrap_for_provider(
            &temporary,
            &[(
                "unavailable-provider",
                "---\nname: unavailable-provider\ndescription: unavailable\nmode: subagent\npermissions: []\n---\nUnavailable work.\n",
            )],
            "unavailable-provider",
            "gpt-4.1",
        );
        let unavailable_session = Arc::new(Mutex::new(SessionContext::fresh()));
        let unavailable_router = TuiRuntimeRouter::new(
            unavailable_bootstrap.clone(),
            Arc::clone(&unavailable_session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );

        assert!(
            !unavailable_router
                .palette_entries()
                .unwrap()
                .iter()
                .any(|entry| entry.name() == "subagent")
        );

        let unavailable_selection =
            unavailable_router.route("/subagent unavailable-provider".into());
        assert!(matches!(
            &unavailable_selection,
            TuiSubmissionOutcome::LocalActionableError { message, .. }
                if message.contains("No eligible subagents")
        ));
        assert!(
            unavailable_session
                .lock()
                .unwrap()
                .selected_subagent
                .is_none()
        );
        assert!(unavailable_session.lock().unwrap().messages.is_empty());

        let mut unavailable_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        let captured = Arc::new(Mutex::new(Vec::new()));
        submit_tui_command(
            &mut unavailable_tui,
            &unavailable_router,
            &unavailable_bootstrap,
            "/subagent unavailable-provider",
            &captured,
        );
        assert!(captured.lock().unwrap().is_empty());
        assert!(!unavailable_tui.view().running);

        let empty_selection =
            unavailable_tui.apply_submission_outcome(unavailable_router.route("/subagent".into()));
        assert_eq!(empty_selection, None);
        let unavailable_overlay = render_tui_test_backend(&unavailable_tui, 80, 24);
        assert!(
            unavailable_overlay.contains("No eligible subagents are available."),
            "{unavailable_overlay:?}"
        );
        assert_eq!(
            unavailable_tui.handle(Event::Key(Key::Enter)),
            Action::Render
        );

        unavailable_tui.apply_submission_outcome(unavailable_router.route("/subagent".into()));
        assert_eq!(
            unavailable_tui.handle(Event::Key(Key::Escape)),
            Action::Render
        );
        assert!(unavailable_tui.transcript().is_empty());
        let unavailable_context = unavailable_session.lock().unwrap();
        assert!(unavailable_context.selected_subagent.is_none());
        assert!(unavailable_context.messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn plural_subagents_command_opens_the_transcript_picker_without_changing_next_type() {
        let temporary = tui_session_directory("plural-subagents");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(SessionContext {
            selected_subagent: Some("explore".into()),
            ..SessionContext::fresh()
        }));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );

        assert!(matches!(
            router.route("/subagents".into()),
            TuiSubmissionOutcome::TranscriptDialog
        ));
        assert_eq!(
            session.lock().unwrap().selected_subagent.as_deref(),
            Some("explore")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_model_overlay_labels_source_metadata_current_and_compatible_sets() {
        for (provider, source, included, excluded) in [
            ("openai-api", "OpenAI API", "gpt-4o", "gpt-5.4"),
            (
                "openai-chatgpt",
                "ChatGPT subscription",
                "gpt-5.4",
                "gpt-4o",
            ),
        ] {
            let temporary = tui_session_directory(&format!("model-source-{provider}"));
            let bootstrap =
                tui_session_bootstrap_for_provider(&temporary, &[], provider, "gpt-5.5");
            let session = Arc::new(Mutex::new(SessionContext::fresh()));
            let cancellation = Arc::new(Mutex::new(None));
            let mut tui = Tui::new(ProductionTuiEngine {
                cancellation: Arc::clone(&cancellation),
            });
            let router = TuiRuntimeRouter::new(
                bootstrap,
                Arc::clone(&session),
                cancellation,
                Arc::new(CommandCatalog::default()),
                Arc::new(SkillCatalog::default()),
            );
            let (progress, _) = std::sync::mpsc::channel();

            assert!(
                tui.apply_submission_outcome(
                    router.route_request(TuiRouteRequest::OpenDialog("model".into()), progress)
                )
                .is_none()
            );
            let text = render_tui_test_backend(&tui, 140, 60);

            assert!(text.contains(source), "{provider}: {text:?}");
            assert!(text.contains("gpt-5.5 (current)"), "{provider}: {text:?}");
            assert!(text.contains(included), "{provider}: {text:?}");
            assert!(!text.contains(excluded), "{provider}: {text:?}");
            assert!(text.contains("272K context"), "{provider}: {text:?}");
            assert!(text.contains("128K output"), "{provider}: {text:?}");
            assert!(text.contains("reasoning"), "{provider}: {text:?}");

            let source = if provider == "openai-chatgpt" {
                ModelSource::ChatGptSubscription
            } else {
                ModelSource::OpenAiApi
            };
            let models = ModelSelection::for_source("gpt-5.5", source)
                .models()
                .unwrap();
            let family = models
                .iter()
                .filter(|model| model.id.starts_with("gpt-5.6"))
                .map(|model| {
                    (
                        model.id.as_str(),
                        model.name.as_deref(),
                        model.context,
                        model.output,
                        model.reasoning,
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                family,
                [
                    (
                        "gpt-5.6",
                        Some("GPT-5.6 (Sol alias)"),
                        Some(1_050_000),
                        Some(128_000),
                        Some(true)
                    ),
                    (
                        "gpt-5.6-luna",
                        Some("GPT-5.6 Luna"),
                        Some(1_050_000),
                        Some(128_000),
                        Some(true)
                    ),
                    (
                        "gpt-5.6-sol",
                        Some("GPT-5.6 Sol"),
                        Some(1_050_000),
                        Some(128_000),
                        Some(true)
                    ),
                    (
                        "gpt-5.6-terra",
                        Some("GPT-5.6 Terra"),
                        Some(1_050_000),
                        Some(128_000),
                        Some(true)
                    ),
                ],
                "official OpenAI GPT-5.6 catalog metadata for {provider}"
            );
            for model in &family {
                assert_eq!(
                    models
                        .iter()
                        .filter(|candidate| candidate.id == model.0)
                        .count(),
                    1,
                    "duplicate {} in {provider}",
                    model.0
                );
            }
            assert!(text.contains("gpt-5.6"), "{provider}: {text:?}");
            assert!(text.contains("gpt-5.6-luna"), "{provider}: {text:?}");
            assert!(
                !text.contains("unverified metadata"),
                "{provider}: {text:?}"
            );

            for _ in 0..4 {
                tui.handle(Event::Key(Key::Down));
            }
            let scrolled = render_tui_test_backend(&tui, 80, 24);
            assert!(scrolled.contains("gpt-5.6-sol"), "{provider}: {scrolled:?}");
            assert!(
                scrolled.contains("gpt-5.6-terra"),
                "{provider}: {scrolled:?}"
            );
            tui.handle(Event::Key(Key::Up));
            tui.handle(Event::Key(Key::Up));
            tui.handle(Event::Key(Key::Up));
            let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
                panic!("verified gpt-5.6 alias should be selectable");
            };
            let outcome = router.route_request(
                TuiRouteRequest::DialogAction(action_id),
                std::sync::mpsc::channel().0,
            );
            assert!(matches!(
                &outcome,
                TuiSubmissionOutcome::ContextChanged { message, presentation }
                    if message == "Model: gpt-5.6."
                        && presentation
                            == &TuiPresentation::new(provider, "gpt-5.6", "new session")
                                .with_effort("medium")
                                .with_context_window(Some(1_050_000))
            ));
            tui.apply_submission_outcome(outcome);
            let selection = session.lock().unwrap().selection.clone().unwrap();
            assert!(selection.metadata_known());
            assert_eq!(selection.reasoning_effort_default(), Some("medium"));
            assert_eq!(
                selection.reasoning_effort_values(),
                ["default", "none", "low", "medium", "high", "xhigh", "max"]
            );

            tui.apply_submission_outcome(router.open_dialog("model").unwrap());
            for character in "gpt-5.6-sol".chars() {
                tui.handle(Event::Key(Key::Char(character)));
            }
            let filtered = render_tui_test_backend(&tui, 80, 24);
            assert!(filtered.contains("gpt-5.6-sol"), "{provider}: {filtered:?}");
            assert!(
                !filtered.contains("unverified metadata"),
                "{provider}: {filtered:?}"
            );

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    #[test]
    fn tui_provider_overlay_filters_unavailable_entries_and_switches_without_history() {
        let temporary = tui_session_directory("provider-overlay");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        std::fs::write(
            &bootstrap.paths.credentials,
            r#"{"openai-chatgpt":{"access_token":"secret-access","refresh_token":"secret-refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let router = TuiRuntimeRouter::with_credential_resolver(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            CredentialResolver::with_environment(BTreeMap::new()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        let (progress, _) = std::sync::mpsc::channel();
        tui.apply_submission_outcome(router.route_request(
            TuiRouteRequest::OpenDialog("provider".into()),
            progress.clone(),
        ));
        let overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(
            overlay.contains("Current: OpenAI API · credential required"),
            "{overlay:?}"
        );
        assert!(overlay.contains("❯ ChatGPT subscription"), "{overlay:?}");
        assert!(overlay.contains("ready"), "{overlay:?}");
        assert!(!overlay.contains("OpenAI API (current)"), "{overlay:?}");
        assert!(!overlay.contains("secret-"), "{overlay:?}");

        dispatch_tui_dialog_selection(&mut tui, &router, progress);
        assert_eq!(tui.view().provider_model, "openai-chatgpt / gpt-5.5");
        tui.apply_submission_outcome(router.open_dialog("model").unwrap());
        let model_overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(model_overlay.contains("Source: ChatGPT subscription"));
        assert!(model_overlay.contains("gpt-5.5 (current)"));
        assert!(tui.transcript().is_empty());
        assert!(session.lock().unwrap().messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_provider_switch_reconciles_compatible_incompatible_and_busy_state_atomically() {
        let temporary = tui_session_directory("provider-reconcile");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        std::fs::write(
            &bootstrap.paths.credentials,
            r#"{"openai-chatgpt":{"access_token":"access","refresh_token":"refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let router = TuiRuntimeRouter::with_credential_resolver(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            CredentialResolver::with_environment(BTreeMap::from([(
                "OPENAI_API_KEY".into(),
                "api-secret".into(),
            )])),
        );

        let retained = router.route("/provider openai-chatgpt".into());
        assert!(
            matches!(retained, TuiSubmissionOutcome::ContextChanged { ref message, .. } if message.contains("Model retained: gpt-5.5"))
        );
        router.route("/model gpt-5.4".into());
        router.route("/effort high".into());
        let reset = router.route("/provider openai-api".into());
        assert!(
            matches!(reset, TuiSubmissionOutcome::ContextChanged { ref message, .. } if message.contains("Model reset to gpt-4.1") && message.contains("Default"))
        );
        let idle = session.lock().unwrap().clone();
        assert_eq!(idle.selection.as_ref().unwrap().model(), "gpt-4.1");
        assert_eq!(idle.selection.as_ref().unwrap().reasoning_effort(), None);
        let mut context = session.lock().unwrap();
        context.messages = tui_session_messages();
        context.running = true;
        drop(context);
        let busy = session.lock().unwrap().clone();
        assert!(matches!(
            router.route("/provider openai-chatgpt".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ));
        assert_eq!(*session.lock().unwrap(), busy);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_turn_bootstrap_resolves_changed_and_removed_credentials_without_stale_reuse() {
        let temporary = tui_session_directory("fresh-turn-credentials");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let configured_provider = bootstrap.provider_type.clone();
        let credentials = bootstrap.paths.credentials.clone();
        let environment = Arc::new(Mutex::new(BTreeMap::new()));
        let resolver = CredentialResolver::with_environment_resolver({
            let environment = Arc::clone(&environment);
            move || environment.lock().unwrap().clone()
        });
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let router = TuiRuntimeRouter::with_credential_resolver(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            resolver,
        );

        std::fs::write(&credentials, r#"{"openai-api":{"api_key":"file-one"}}"#).unwrap();
        assert_eq!(
            router.turn_bootstrap().unwrap().openai_api_key.as_deref(),
            Some("file-one")
        );
        std::fs::write(&credentials, r#"{"openai-api":{"api_key":"file-two"}}"#).unwrap();
        assert_eq!(
            router.turn_bootstrap().unwrap().openai_api_key.as_deref(),
            Some("file-two")
        );
        environment
            .lock()
            .unwrap()
            .insert("OPENAI_API_KEY".into(), "env-current".into());
        assert_eq!(
            router.turn_bootstrap().unwrap().openai_api_key.as_deref(),
            Some("env-current")
        );
        environment.lock().unwrap().clear();
        std::fs::remove_file(&credentials).unwrap();
        assert!(router.turn_bootstrap().is_err());

        session.lock().unwrap().provider = Some(ProviderKind::OpenAiChatGpt);
        std::fs::write(
            &credentials,
            r#"{"openai-chatgpt":{"access_token":"chat-access","refresh_token":"chat-refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(
            router.turn_bootstrap().unwrap().provider_type(),
            Some("openai-chatgpt")
        );
        std::fs::remove_file(&credentials).unwrap();
        assert!(router.turn_bootstrap().is_err());
        assert!(matches!(
            router.route("/provider openai-chatgpt".into()),
            TuiSubmissionOutcome::LocalActionableError { ref message, .. }
                if message.contains("run /connect")
        ));
        assert_eq!(
            router.bootstrap().unwrap().provider_type,
            configured_provider
        );
        assert!(session.lock().unwrap().messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_model_overlay_selects_exact_future_id_with_unknown_metadata_and_default_effort() {
        let temporary = tui_session_directory("unverified-model");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let (progress, _) = std::sync::mpsc::channel();
        assert!(matches!(
            router.route("/effort xhigh".into()),
            TuiSubmissionOutcome::ContextChanged { .. }
        ));
        tui.apply_submission_outcome(router.route_request(
            TuiRouteRequest::OpenDialog("model".into()),
            progress.clone(),
        ));

        for character in "gpt-future-1".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        let overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(
            overlay.contains("Use gpt-future-1 (unverified metadata)"),
            "{overlay:?}"
        );
        let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("unverified model should dispatch a local action");
        };
        let outcome = router.route_request(TuiRouteRequest::DialogAction(action_id), progress);
        let TuiSubmissionOutcome::ContextChanged {
            message,
            presentation,
        } = &outcome
        else {
            panic!("unverified model should update session context");
        };
        assert_eq!(
            message,
            "Model: gpt-future-1 (unverified metadata). Reasoning effort reset to Default."
        );
        assert_eq!(
            presentation,
            &TuiPresentation::new("openai-api", "gpt-future-1", "new session")
        );
        tui.apply_submission_outcome(outcome);

        let selection = session.lock().unwrap().selection.clone().unwrap();
        assert_eq!(selection.model(), "gpt-future-1");
        assert!(!selection.metadata_known());
        assert_eq!(selection.reasoning_effort(), None);
        assert_eq!(
            selection.request_config(),
            &agens_core::RequestConfig::default()
        );
        assert!(session.lock().unwrap().messages.is_empty());
        assert!(tui.transcript().is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_effort_overlay_and_model_change_use_grounded_sets_and_atomic_reset() {
        let temporary = tui_session_directory("effort-capabilities");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let (progress, _) = std::sync::mpsc::channel();

        assert_eq!(
            router.route("/effort xhigh".into()),
            TuiSubmissionOutcome::ContextChanged {
                message: "Reasoning effort: xhigh.".into(),
                presentation: router.presentation().unwrap(),
            }
        );
        assert!(
            tui.apply_submission_outcome(
                router.route_request(TuiRouteRequest::OpenDialog("effort".into()), progress)
            )
            .is_none()
        );
        let overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(overlay.contains("Default"), "{overlay:?}");
        assert!(overlay.contains("xhigh (current)"), "{overlay:?}");
        assert!(!overlay.contains("minimal"), "{overlay:?}");

        let reset = router.route("/model gpt-4.1".into());
        let TuiSubmissionOutcome::ContextChanged { message, .. } = reset else {
            panic!("model change should be local context information");
        };
        assert_eq!(
            message,
            "Model: gpt-4.1. Reasoning effort reset to Default because xhigh is unsupported."
        );
        let selection = session.lock().unwrap().selection.clone().unwrap();
        assert_eq!(selection.model(), "gpt-4.1");
        assert_eq!(selection.reasoning_effort(), None);
        assert_eq!(selection.request_config().reasoning_effort(), None);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_sessions_resume_and_agent_overlays_filter_navigate_cancel_and_apply_typed_outcomes() {
        let temporary = tui_session_directory("session-agent-overlays");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[
                (
                    "all",
                    "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
                ),
                (
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                ),
            ],
        );
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        tui.set_palette_entries(router.palette_entries().unwrap());
        let (progress, _) = std::sync::mpsc::channel();

        let empty = router.route_request(
            agens_tui::TuiRouteRequest::OpenDialog("sessions".into()),
            progress.clone(),
        );
        tui.apply_submission_outcome(empty);
        assert!(
            render_tui_test_backend(&tui, 80, 24)
                .contains("No resumable sessions in current project.")
        );
        tui.handle(Event::Key(Key::Escape));

        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let current = persist_tui_session(&mut store, &tui_project(&temporary), "current");
        let other = persist_tui_session(
            &mut store,
            &temporary.join("other").display().to_string(),
            "other",
        );
        drop(store);

        open_tui_palette_dialog(&mut tui, &router, "/se", "sessions", progress.clone());
        let sessions = render_tui_test_backend(&tui, 80, 24);
        assert!(sessions.contains(&format!("#{} current", current.id)));
        assert!(!sessions.contains(&format!("#{} other", other.id)));
        let original = session.lock().unwrap().clone();
        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        assert_eq!(*session.lock().unwrap(), original);

        open_tui_palette_dialog(&mut tui, &router, "/re", "sessions", progress.clone());
        dispatch_tui_dialog_selection(&mut tui, &router, progress.clone());
        assert_eq!(tui.view().session, format!("session #{}", current.id));
        assert!(tui.transcript().is_empty());
        assert!(
            tui.view()
                .status
                .is_some_and(|status| status.contains("Resumed session"))
        );

        open_tui_palette_dialog(&mut tui, &router, "/ag", "agent", progress.clone());
        let agents = render_tui_test_backend(&tui, 80, 24);
        assert!(agents.contains("primary (current)"), "{agents:?}");
        tui.handle(Event::Key(Key::Down));
        dispatch_tui_dialog_selection(&mut tui, &router, progress);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn dialog_recovery_is_confirmed_private_local_safe_and_retryable() {
        let temporary = tui_session_directory("recovery-dialog");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            Arc::clone(&cancellation),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let metadata = SessionMetadata {
            id: 1,
            project: tui_project(&temporary),
            title: "Interrupted session".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 7,
            completed_turn_count: 0,
            resumable: false,
        };
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let attempt = store
            .begin_session_attempt(&metadata, "SENTINEL_PRIVATE_RETRY".into())
            .unwrap();
        drop(store);

        let confirmation = router.route_dialog_action("session:1", std::sync::mpsc::channel().0);
        let confirmation_debug = format!("{confirmation:?}");
        let mut tui = Tui::new(ProductionTuiEngine { cancellation });
        assert!(tui.apply_submission_outcome(confirmation).is_none());
        let confirmation_text = render_tui_test_backend(&tui, 100, 24);
        assert!(confirmation_text.contains("Recover interrupted attempt"));
        assert!(confirmation_text.contains("Interrupted session"));
        assert!(confirmation_text.contains("ID: 1"));
        assert!(confirmation_text.contains("Status: running"));
        assert!(confirmation_text.contains("Started: 7"));
        assert!(
            confirmation_debug
                .contains("This may invalidate an attempt still running in another process.")
        );
        assert!(!confirmation_debug.contains("SENTINEL_PRIVATE_RETRY"));

        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        let store = SessionStore::open(bootstrap.data_directory()).unwrap();
        assert_eq!(
            store
                .load_session_for_resume(1)
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Running
        );
        drop(store);

        let locally_active_metadata = SessionMetadata {
            id: 2,
            ..metadata.clone()
        };
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let locally_active = active_session_attempts()
            .begin_and_register(
                &mut store,
                &locally_active_metadata,
                "local private retry".into(),
            )
            .unwrap();
        drop(store);
        let local_refusal = router.route_dialog_action(
            &format!(
                "session:recover:{}:{}",
                locally_active.key().session_id(),
                locally_active.key().attempt_id()
            ),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(local_refusal, TuiSubmissionOutcome::Dialog(_)));
        let store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let database_path = store.database_path();
        assert_eq!(
            store
                .load_session_for_resume(locally_active.key().session_id())
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Running
        );
        drop(store);
        active_session_attempts().unregister(&database_path, locally_active.key());

        let recovered = router.route_dialog_action(
            &format!(
                "session:recover:{}:{}",
                attempt.key().session_id(),
                attempt.key().attempt_id()
            ),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(
            recovered,
            TuiSubmissionOutcome::ProviderTurn { ref display, ref prompt }
                if display == "Retrying recovered attempt." && prompt == "SENTINEL_PRIVATE_RETRY"
        ));
        assert_eq!(session.lock().unwrap().identifier, Some(1));
        let store = SessionStore::open(bootstrap.data_directory()).unwrap();
        assert_eq!(
            store
                .load_session_for_resume(1)
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Interrupted
        );

        let stale = router.route_dialog_action(
            &format!(
                "session:recover:{}:{}",
                attempt.key().session_id(),
                attempt.key().attempt_id()
            ),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(stale, TuiSubmissionOutcome::Dialog(_)));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    fn dispatch_tui_session_page(
        tui: &mut Tui<ProductionTuiEngine>,
        router: &TuiRuntimeRouter,
        action: Action,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) {
        let Action::LoadSessionPage(request) = action else {
            panic!("session dialog action should request a page");
        };
        let outcome = router.route_request(TuiRouteRequest::SessionPage(request), progress);
        assert!(tui.apply_submission_outcome(outcome).is_none());
    }

    #[test]
    fn session_overlay_uses_real_metadata_scope_search_sort_clock_and_atomic_failure() {
        let temporary = tui_session_directory("session-metadata-overlay");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = tui_project(&temporary);
        let other_project = temporary.join("other-root").display().to_string();
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let old = persist_tui_session_metadata(&mut store, &project, "Alpha", "primary", 9_900);
        let other =
            persist_tui_session_metadata(&mut store, &other_project, "Beta", "build", 9_950);
        let mut current =
            persist_tui_session_metadata(&mut store, &project, "Gamma", "reviewer", 9_950);
        current.provider_id = Some("openai-chatgpt".into());
        current.model_id = Some("gpt-5.5".into());
        current.reasoning_effort = Some(agens_core::ReasoningEffort::High);
        store.update_session_selection(&current).unwrap();
        drop(store);

        let session = Arc::new(Mutex::new(SessionContext {
            identifier: Some(current.id),
            ..SessionContext::fresh()
        }));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        tui.set_presentation("openai-api", "gpt-4.1", format!("session #{}", current.id));
        tui.replace_history(&tui_session_messages()).unwrap();
        let router = TuiRuntimeRouter::with_clock(
            bootstrap.clone(),
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            || 10_000,
        );
        tui.set_palette_entries(router.palette_entries().unwrap());
        let (progress, _) = std::sync::mpsc::channel();
        let original_context = session.lock().unwrap().clone();

        open_tui_palette_dialog(&mut tui, &router, "/se", "sessions", progress.clone());
        let project_rows = render_tui_test_backend(&tui, 100, 26);
        assert!(project_rows.contains("Resume session · Current project"));
        assert!(project_rows.contains(&format!("#{} Gamma", current.id)));
        assert!(project_rows.contains(&format!("#{} Alpha", old.id)));
        assert!(
            project_rows.contains("1 turn · 50s ago"),
            "{project_rows:?}"
        );
        assert!(!project_rows.contains("reviewer"), "{project_rows:?}");
        assert!(!project_rows.contains("Provider:"), "{project_rows:?}");
        assert!(!project_rows.contains("Model:"), "{project_rows:?}");
        assert!(!project_rows.contains("Effort:"), "{project_rows:?}");
        assert!(!project_rows.contains("Updated:"), "{project_rows:?}");
        tui.handle(Event::Key(Key::CtrlO));
        let project_details = render_tui_test_backend(&tui, 100, 26);
        assert!(
            project_details.contains("Provider: openai-chatgpt · Model: gpt-5.5"),
            "{project_details:?}"
        );
        assert!(
            project_details.contains("Effort: high · Updated: 9950 (50s ago)"),
            "{project_details:?}"
        );
        let old_details = format!(
            "{:?}",
            session_dialog_entry(
                &StoredSession {
                    metadata: old.clone(),
                    messages: Vec::new(),
                    latest_attempt: None,
                },
                None,
                false,
                10_000,
            )
        );
        assert!(old_details.contains("Provider: current runtime"));
        assert!(old_details.contains("Model: current runtime"));
        assert!(old_details.contains("Effort: current runtime"));
        assert!(project_rows.find("Gamma").unwrap() < project_rows.find("Alpha").unwrap());
        assert!(!project_rows.contains("Beta"));

        let global_action = tui.handle(Event::Key(Key::LineStart));
        dispatch_tui_session_page(&mut tui, &router, global_action, progress.clone());
        let global_rows = render_tui_test_backend(&tui, 100, 24);
        assert!(global_rows.contains("Resume session · All projects"));
        assert!(global_rows.contains(&format!("#{} Beta", other.id)));
        assert!(!global_rows.contains("root="), "{global_rows:?}");
        assert!(!global_rows.contains("other-root"), "{global_rows:?}");
        assert!(global_rows.find("Gamma").unwrap() < global_rows.find("Beta").unwrap());
        assert!(global_rows.find("Beta").unwrap() < global_rows.find("Alpha").unwrap());

        let mut search_action = Action::Render;
        for character in "reviewer".chars() {
            search_action = tui.handle(Event::Key(Key::Char(character)));
        }
        dispatch_tui_session_page(&mut tui, &router, search_action, progress.clone());
        let agent_search = render_tui_test_backend(&tui, 100, 24);
        assert!(agent_search.contains("Gamma"));
        assert!(!agent_search.contains("Alpha"));
        assert!(!agent_search.contains("Beta"));
        tui.handle(Event::Key(Key::Escape));
        tui.apply_submission_outcome(router.open_dialog("sessions").unwrap());
        let global_action = tui.handle(Event::Key(Key::LineStart));
        dispatch_tui_session_page(&mut tui, &router, global_action, progress.clone());
        let mut search_action = Action::Render;
        for character in "other-root".chars() {
            search_action = tui.handle(Event::Key(Key::Char(character)));
        }
        dispatch_tui_session_page(&mut tui, &router, search_action, progress.clone());
        let root_search = render_tui_test_backend(&tui, 100, 24);
        assert!(root_search.contains("Beta"));
        assert!(!root_search.contains("Gamma"));
        assert_eq!(*session.lock().unwrap(), original_context);

        tui.handle(Event::Key(Key::Escape));
        tui.apply_submission_outcome(router.open_dialog("sessions").unwrap());
        SessionStore::open(bootstrap.data_directory())
            .unwrap()
            .delete_session(current.id)
            .unwrap();
        let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("session Enter should dispatch through the router");
        };
        let outcome = router.route_request(TuiRouteRequest::DialogAction(action_id), progress);
        tui.apply_submission_outcome(outcome);
        assert_eq!(tui.view().session, format!("session #{}", current.id));
        assert_eq!(*session.lock().unwrap(), original_context);
        assert!(render_tui_test_backend(&tui, 100, 24).contains("saved session is unavailable"));
        tui.handle(Event::Key(Key::Escape));
        assert!(render_tui_test_backend(&tui, 100, 24).contains("previous request"));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_resume_overlay_restores_appends_reopens_and_resets_complete_history() {
        let temporary = tui_session_directory("resume-production-path");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let first = persist_tui_session(&mut store, &tui_project(&temporary), "history");
        let restored =
            append_tui_session_turn(&mut store, &first, "second request", "second answer");
        let restored_messages = store.load_session_for_resume(restored.id).unwrap().messages;
        drop(store);

        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        tui.set_palette_entries(router.palette_entries().unwrap());
        let (progress, _) = std::sync::mpsc::channel();

        open_tui_palette_dialog(&mut tui, &router, "/re", "sessions", progress.clone());
        dispatch_tui_dialog_selection(&mut tui, &router, progress.clone());
        let restored_render = render_tui_test_backend(&tui, 120, 50);
        for expected in [
            "previous request",
            "Thought",
            "previous answer",
            "persisted reminder",
            "second request",
            "second answer",
        ] {
            assert!(restored_render.contains(expected), "{restored_render:?}");
            assert_eq!(
                restored_render.matches(expected).count(),
                1,
                "{restored_render:?}"
            );
        }
        // Tool name appears on header and result footer; assert the card chrome once.
        assert!(restored_render.contains("read {}"), "{restored_render:?}");
        assert_eq!(
            restored_render.matches("read {}").count(),
            1,
            "{restored_render:?}"
        );
        assert!(
            restored_render.contains("output collapsed"),
            "{restored_render:?}"
        );
        assert!(
            !restored_render.contains("previous reasoning"),
            "{restored_render:?}"
        );
        assert!(
            !restored_render.contains("previous result"),
            "{restored_render:?}"
        );
        assert!(
            !restored_render.contains("resume-call"),
            "{restored_render:?}"
        );

        tui.handle(Event::Key(Key::PageUp));
        let restored_anchor = (
            tui.view().following_bottom,
            tui.view().scroll_offset,
            tui.view().focus,
        );

        // Ctrl+O is thinking-first: expand collapsed reasoning before tool bodies.
        tui.handle(Event::Key(Key::CtrlO));
        assert_eq!(
            (
                tui.view().following_bottom,
                tui.view().scroll_offset,
                tui.view().focus,
            ),
            restored_anchor
        );
        let thinking_expanded = render_tui_test_backend(&tui, 120, 50);
        assert!(
            thinking_expanded.contains("previous reasoning"),
            "{thinking_expanded:?}"
        );
        assert!(
            !thinking_expanded.contains("previous result"),
            "{thinking_expanded:?}"
        );

        tui.handle(Event::Key(Key::CtrlO));
        assert_eq!(
            (
                tui.view().following_bottom,
                tui.view().scroll_offset,
                tui.view().focus,
            ),
            restored_anchor
        );
        let tools_expanded = render_tui_test_backend(&tui, 120, 50);
        assert!(
            tools_expanded.contains("previous result"),
            "{tools_expanded:?}"
        );

        tui.handle(Event::Key(Key::CtrlO));
        assert_eq!(
            (
                tui.view().following_bottom,
                tui.view().scroll_offset,
                tui.view().focus,
            ),
            restored_anchor
        );
        // Completes the Collapsed -> Truncated -> Expanded -> Collapsed
        // cycle (S1 renders Truncated and Expanded identically).
        tui.handle(Event::Key(Key::CtrlO));
        assert_eq!(
            (
                tui.view().following_bottom,
                tui.view().scroll_offset,
                tui.view().focus,
            ),
            restored_anchor
        );
        assert_eq!(
            tui.view().tool_display_modes.get("resume-call"),
            Some(&agens_tui::DisplayMode::Collapsed)
        );
        tui.handle(Event::Key(Key::End));

        assert_eq!(tui.view().session, format!("session #{}", restored.id));
        assert!(tui.transcript().is_empty());
        assert!(!restored_render.contains("INFO      Resumed session"));

        let before_failure = session.lock().unwrap().clone();
        let input = enter_tui_input(&mut tui, "/resume 999");
        tui.apply_submission_outcome(router.route(input));
        let failed = render_tui_test_backend(&tui, 120, 50);
        assert!(
            failed.contains("saved session is unavailable"),
            "{failed:?}"
        );
        assert!(failed.contains("Action:"), "{failed:?}");
        assert_eq!(tui.view().session, format!("session #{}", restored.id));
        assert_eq!(*session.lock().unwrap(), before_failure);
        assert!(tui.transcript().is_empty());

        tui.handle(Event::Key(Key::Escape));
        let prompt = enter_tui_input(&mut tui, "third request");
        let prompt = tui.apply_submission_outcome(router.route(prompt)).unwrap();
        let result = run_tui_prompt_with(
            &bootstrap,
            &prompt,
            &router.session,
            Some(router.skills().unwrap()),
            |request| {
                assert_eq!(request.history, restored_messages);
                let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
                let metadata = append_tui_session_turn(
                    &mut store,
                    request.session.as_ref().unwrap(),
                    "third request",
                    "third answer",
                );
                let messages = store.load_session_for_resume(metadata.id).unwrap().messages;
                Ok(HeadlessChatCompletion {
                    text: "third answer".into(),
                    metadata,
                    messages,
                })
            },
        );
        tui.finish_provider_turn(tui_provider_outcome(result));
        let reopened = SessionStore::open(bootstrap.data_directory())
            .unwrap()
            .load_session_for_resume(restored.id)
            .unwrap();
        assert_eq!(session.lock().unwrap().messages, reopened.messages);

        open_tui_palette_dialog(&mut tui, &router, "/re", "sessions", progress);
        dispatch_tui_dialog_selection(&mut tui, &router, std::sync::mpsc::channel().0);
        let reopened_render = render_tui_test_backend(&tui, 120, 60);
        for expected in [
            "previous request",
            "second request",
            "third request",
            "third answer",
        ] {
            assert_eq!(
                reopened_render.matches(expected).count(),
                1,
                "{reopened_render:?}"
            );
        }

        for _ in 0..20 {
            tui.handle(Event::Key(Key::PageUp));
        }
        assert!(render_tui_test_backend(&tui, 60, 14).contains("previous request"));

        let input = enter_tui_input(&mut tui, "/new");
        tui.apply_submission_outcome(router.route(input));
        let reset = render_tui_test_backend(&tui, 120, 24);
        assert_eq!(tui.view().session, "new session");
        assert!(!reset.contains("previous request"), "{reset:?}");
        assert!(!reset.contains("INFO"), "{reset:?}");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    fn append_tui_session_turn(
        store: &mut SessionStore,
        metadata: &SessionMetadata,
        user: &str,
        answer: &str,
    ) -> SessionMetadata {
        let messages = vec![
            Message {
                role: Role::System,
                parts: vec![MessagePart::Text("persisted reminder".into())],
            },
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text(user.into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text(answer.into())],
            },
        ];
        let turn = CompletedSessionTurn::new(
            messages
                .into_iter()
                .map(SessionMessage::try_from)
                .collect::<Result<_, _>>()
                .unwrap(),
        )
        .unwrap();
        store
            .persist_completed_session_turn(metadata, &turn)
            .unwrap()
    }

    #[test]
    fn tui_native_select_preserves_running_turn_outcomes_and_terminal_cleanup() {
        use std::os::unix::fs::symlink;

        let temporary = tui_session_directory("native-select");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = temporary.join("project");
        let outside = temporary.join("outside.txt");
        std::fs::write(project.join("approved.txt"), "approved").unwrap();
        std::fs::create_dir(project.join("directory")).unwrap();
        std::fs::write(project.join("large.txt"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        std::fs::write(&outside, "outside").unwrap();
        symlink(&outside, project.join("escape.txt")).unwrap();
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        let mut control = TuiTerminalControl::default();
        let mut guard = agens_tui::TerminalModeGuard::enter(&mut control).unwrap();
        let transcript_count = open_running_tui_select(&mut tui, &router);
        assert!(render_tui_test_backend(&tui, 80, 24).contains("Select project file"));
        assert_eq!(
            tui.handle(Event::Key(Key::Escape)),
            Action::SafeDialogAction("select:cancel".into())
        );
        let cancelled = router.route_request(
            TuiRouteRequest::DialogAction("select:cancel".into()),
            std::sync::mpsc::channel().0,
        );
        assert_eq!(cancelled, TuiSubmissionOutcome::SelectionCancelled);
        assert!(tui.apply_submission_outcome(cancelled).is_none());
        assert!(tui.view().dialog.is_none());
        assert!(tui.view().running);
        assert_eq!(tui.transcript().len(), transcript_count);
        assert!(
            tui.apply_submission_outcome(router.route_request(
                TuiRouteRequest::DialogAction("select:cancel".into()),
                std::sync::mpsc::channel().0,
            ))
            .is_none()
        );
        assert_eq!(tui.transcript().len(), transcript_count);
        open_running_tui_select(&mut tui, &router);
        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
        assert!(tui.view().quit_armed);
        assert!(tui.view().dialog.is_some());
        assert_eq!(
            tui.handle(Event::Key(Key::Escape)),
            Action::SafeDialogAction("select:cancel".into())
        );
        assert_eq!(
            router.route_request(
                TuiRouteRequest::DialogAction("select:cancel".into()),
                std::sync::mpsc::channel().0,
            ),
            TuiSubmissionOutcome::SelectionCancelled
        );
        guard.restore(&mut control).unwrap();
        assert_tui_terminal_restored(&control);

        let mut control = TuiTerminalControl::default();
        let mut guard = agens_tui::TerminalModeGuard::enter(&mut control).unwrap();
        let transcript_count = open_running_tui_select(&mut tui, &router);
        let Action::SafeDialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("selection Enter should use the safe local action path");
        };
        let selected = router.route_request(
            TuiRouteRequest::DialogAction(action_id),
            std::sync::mpsc::channel().0,
        );
        assert_eq!(
            selected,
            TuiSubmissionOutcome::SelectionInfo("Selected file: approved.txt".into())
        );
        assert!(tui.apply_submission_outcome(selected).is_none());
        assert!(tui.view().running);
        assert_eq!(tui.transcript().len(), transcript_count);
        guard.restore(&mut control).unwrap();
        assert_tui_terminal_restored(&control);

        let mut control = TuiTerminalControl::default();
        let mut guard = agens_tui::TerminalModeGuard::enter(&mut control).unwrap();
        let transcript_count = open_running_tui_select(&mut tui, &router);
        let rejected = router.route_request(
            TuiRouteRequest::DialogAction("select:escape.txt".into()),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(
            rejected,
            TuiSubmissionOutcome::SelectionError { .. }
        ));
        assert!(tui.apply_submission_outcome(rejected).is_none());
        assert!(tui.view().running);
        assert_eq!(tui.transcript().len(), transcript_count);
        guard.restore(&mut control).unwrap();
        assert_tui_terminal_restored(&control);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[derive(Default)]
    struct TuiTerminalControl {
        operations: Vec<agens_tui::TerminalOperation>,
    }

    impl agens_tui::TerminalControl for TuiTerminalControl {
        fn apply(&mut self, operation: agens_tui::TerminalOperation) -> std::io::Result<()> {
            self.operations.push(operation);
            Ok(())
        }
    }

    fn assert_tui_terminal_restored(control: &TuiTerminalControl) {
        use agens_tui::TerminalOperation::*;

        assert_eq!(
            control.operations,
            vec![
                EnableRaw,
                EnterAlternate,
                HideCursor,
                EnableMouse,
                EnableKeyboardEnhancement,
                EnablePaste,
                DisablePaste,
                DisableKeyboardEnhancement,
                DisableMouse,
                ShowCursor,
                LeaveAlternate,
                DisableRaw,
            ]
        );
    }

    fn open_running_tui_select(
        tui: &mut Tui<ProductionTuiEngine>,
        router: &TuiRuntimeRouter,
    ) -> usize {
        tui.begin_submission("running");
        let transcript_count = tui.transcript().len();
        for character in "/select".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }

        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::OpenDialog("select".into())
        );
        let outcome = router.route_request(
            TuiRouteRequest::OpenDialog("select".into()),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(outcome, TuiSubmissionOutcome::SafeDialog(_)));
        assert!(tui.apply_submission_outcome(outcome).is_none());
        assert!(tui.view().running);
        assert_eq!(tui.transcript().len(), transcript_count);

        transcript_count
    }

    #[test]
    fn persisted_selection_updates_and_resume_are_atomic_and_credential_fresh() {
        let temporary = tui_session_directory("persisted-selection");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let mut metadata = persist_tui_session(&mut store, &tui_project(&temporary), "selection");
        metadata.provider_id = Some("openai-api".into());
        metadata.model_id = Some("gpt-5.5".into());
        metadata.reasoning_effort = Some(agens_core::ReasoningEffort::High);
        store.update_session_selection(&metadata).unwrap();
        drop(store);
        let resolver = CredentialResolver::with_environment(BTreeMap::from([(
            "OPENAI_API_KEY".into(),
            "fresh-secret".into(),
        )]));
        let resumed =
            resume_tui_session(&bootstrap, metadata.id, &SkillCatalog::default(), &resolver)
                .unwrap()
                .context;
        assert_eq!(resumed.selection.as_ref().unwrap().model(), "gpt-5.5");
        assert_eq!(
            resumed.selection.as_ref().unwrap().reasoning_effort(),
            Some("high")
        );
        let session = Arc::new(Mutex::new(resumed));
        let router = TuiRuntimeRouter::with_credential_resolver(
            bootstrap.clone(),
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            resolver,
        );
        assert_eq!(router.turn_bootstrap().unwrap().model(), Some("gpt-5.5"));
        assert_eq!(
            router
                .task_parent_request_config()
                .unwrap()
                .reasoning_effort(),
            Some(agens_core::ReasoningEffort::High)
        );
        assert!(matches!(
            router.route("/model gpt-4.1".into()),
            TuiSubmissionOutcome::ContextChanged { .. }
        ));
        assert_eq!(router.turn_bootstrap().unwrap().model(), Some("gpt-4.1"));
        assert_eq!(
            router
                .task_parent_request_config()
                .unwrap()
                .reasoning_effort(),
            None
        );
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .model_id
                .as_deref(),
            Some("gpt-4.1")
        );

        let database = SessionStore::open(bootstrap.data_directory())
            .unwrap()
            .database_path();
        rusqlite::Connection::open(database)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_selection BEFORE UPDATE OF provider_id ON sessions
             BEGIN SELECT RAISE(ABORT, 'reject selection'); END;",
            )
            .unwrap();
        let before = session.lock().unwrap().clone();
        assert!(matches!(
            router.route("/effort default".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ));
        assert_eq!(*session.lock().unwrap(), before);

        let unavailable = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &CredentialResolver::with_environment(BTreeMap::new()),
        )
        .unwrap()
        .context;
        assert_eq!(unavailable.messages, before.messages);
        assert_eq!(
            unavailable.resume_error.as_deref(),
            Some("connect or choose provider")
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn permission_error_mapping_is_sanitized_and_fails_closed() {
        let secret_input = r#"{"command":"SENTINEL_COMMAND","token":"SENTINEL_TOKEN"}"#;
        for (name, input) in [
            ("native::read", "{malformed"),
            ("native::read", secret_input),
            ("native::unknown", r#"{"path":"SENTINEL_PATH"}"#),
        ] {
            let outcome = run_production_batch(
                "permission-evaluation-invalid",
                Vec::new(),
                vec![MessagePart::ToolCall {
                    id: "invalid".into(),
                    name: name.into(),
                    input: input.into(),
                }],
                None,
                None,
                false,
            );

            assert_eq!(outcome.result, Err(HeadlessTurnError::PermissionEvaluation));
            assert!(outcome.executions.is_empty());
        }

        for (turn_error, expected) in [
            (
                HeadlessTurnError::Permission,
                "permission: permission evaluation failed",
            ),
            (
                HeadlessTurnError::PermissionRequired,
                "permission: permission approval is required",
            ),
            (
                HeadlessTurnError::PermissionEvaluation,
                "permission: permission target could not be evaluated; correct the tool arguments and retry",
            ),
        ] {
            let error = CliError::runtime(turn_error);
            assert_eq!(error.category, "permission");
            assert_eq!(error.to_string(), expected);
            assert!(!error.to_string().contains("SENTINEL_COMMAND"));
            assert!(!error.to_string().contains("SENTINEL_TOKEN"));

            assert!(matches!(
                tui_provider_outcome(Err(error)),
                TuiProviderOutcome::Failed { message, action }
                    if message == expected && action == TUI_ERROR_ACTION
            ));
        }
    }

    #[test]
    fn provider_context_and_network_render_sanitized_actions() {
        for (turn_error, expected_message, expected_action) in [
            (
                HeadlessTurnError::ProviderContext,
                "provider: request exceeds the model context window",
                "Start a new session or shorten the prompt, then retry.",
            ),
            (
                HeadlessTurnError::ProviderNetwork,
                "provider: network request failed",
                "Check the network connection, then retry.",
            ),
        ] {
            let error = CliError::runtime(turn_error);

            assert!(matches!(
                tui_provider_outcome(Err(error)),
                TuiProviderOutcome::Failed { message, action }
                    if message == expected_message && action == expected_action
            ));
        }
    }
}
