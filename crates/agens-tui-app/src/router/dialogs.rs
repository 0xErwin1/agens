//! Opening a dialog and acting on what a person picked in it.

use agens_session::model::{current_provider, model_source, resolved_provider};

use agens_core::{AttemptKey, RecoveryOutcome};
use agens_store::{SessionCursor, SessionStore};
use agens_tui::{
    DialogEntry, DialogView, Key, SessionDialogCursor, SessionDialogRequest, SessionDialogScope,
    TuiRouteCancellation, TuiRouteProgress, TuiSubmissionOutcome,
};

use crate::dialogs::{diagnostics_dialog, mcp_status_dialog};
use crate::extensions::render_tui_help;
use crate::files::{selected_tui_file, tui_select_candidates};
use crate::fork::{fork_cut_message_count, lineage_entries, lineage_root};
use crate::models::{
    apply_tui_effort, apply_tui_model, apply_tui_unverified_model, format_model_metadata,
};
use crate::profiles::ProfileEditorRow;
use crate::resume::{
    ResumedTuiSession, commit_tui_session_resume, load_tui_session_for_resume,
    prepare_loaded_tui_session_resume, resume_tui_session, tui_project_identifier,
};
use crate::session::{parse_recovery_action, recovery_confirmation_dialog, session_dialog_entry};
use agens_agents::{
    AgentProfileResolver, ProfileOrigin, agent_catalog_for_context,
    persist_pending_agent_correction, select_subagent, subagent_catalog,
};
use agens_auth::ChatGptAuthFlow;
use agens_bootstrap::Bootstrap;
use agens_bootstrap::session_config::SessionConfig;
use agens_bootstrap::session_root::SessionRoot;
use agens_error::CliError;
use agens_models::ModelSelection;
use agens_session::attempt::active_session_attempts;
use agens_session::context::current_session_timestamp;
use agens_session::provider::{CredentialStatus, ProviderKind};
use agens_tool_runtime::rotation::rotate_agent;
use agens_tools::McpLifecycleState;

use super::{TUI_ERROR_ACTION, TuiRuntimeRouter, auth_route_outcome};

/// Every provider, the active one first, so the models a person is most likely
/// to want are at the top of the picker without hiding the rest.
fn providers_with_active_first(active: ProviderKind) -> Vec<ProviderKind> {
    let mut providers = vec![active];
    providers.extend(
        ProviderKind::ALL
            .into_iter()
            .filter(|provider| *provider != active),
    );
    providers
}

fn profile_origin_label(origin: ProfileOrigin) -> &'static str {
    match origin {
        ProfileOrigin::ProjectProfile => "profile:project",
        ProfileOrigin::GlobalProfile => "profile:global",
        ProfileOrigin::Frontmatter => "frontmatter",
        ProfileOrigin::SessionInherited => "session-inherited",
    }
}

impl TuiRuntimeRouter {
    pub fn open_dialog(&self, route_id: &str) -> Result<TuiSubmissionOutcome, CliError> {
        let bootstrap = self.bootstrap()?;
        let dialog = match route_id {
            "dangerous" => return self.toggle_dangerous_mode(),
            "bypass" => return self.toggle_bypass_permissions(),
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
            "login" => DialogView::selection(
                "Sign in",
                Some("Choose a provider"),
                vec![
                    DialogEntry::action("ChatGPT subscription", "login:chatgpt"),
                    DialogEntry::action("OpenAI API key", "login:api-key:openai-api"),
                    DialogEntry::action("Moonshot AI API key", "login:api-key:moonshotai"),
                ],
            ),
            "diagnostics" => diagnostics_dialog(bootstrap.data_directory()),
            "provider" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                let current = current_provider(&bootstrap, &context);
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
                    .unwrap_or_else(|| resolved_provider(&bootstrap, &context).default_model())
                    .to_owned();
                let active = resolved_provider(&bootstrap, &context);
                drop(context);

                let mut entries = Vec::new();
                let mut selected = 0;
                for provider in providers_with_active_first(active) {
                    let selector =
                        ModelSelection::for_source(provider.default_model(), provider.source());
                    let values = selector.models().map_err(CliError::unavailable)?;

                    for model in values {
                        let is_current = provider == active && model.id == current;
                        if is_current {
                            selected = entries.len();
                        }

                        let label = if is_current {
                            format!("{} · {} (current)", model.id, provider.label())
                        } else {
                            format!("{} · {}", model.id, provider.label())
                        };

                        entries.push(DialogEntry::action_with_metadata(
                            label,
                            format_model_metadata(&model),
                            format!(
                                "{} {} {}",
                                model.id,
                                provider.identifier(),
                                provider.label()
                            ),
                            format_model_metadata(&model),
                            format!("model:{}:{}", provider.identifier(), model.id),
                        ));
                    }
                }

                DialogView::selection(
                    "Choose model",
                    Some(format!("All providers · current: {}", active.label())),
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
                    .unwrap_or_else(|| resolved_provider(&bootstrap, &context).default_model());
                let selector = context.selection.clone().unwrap_or_else(|| {
                    ModelSelection::for_source(model, model_source(&bootstrap, &context))
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
            // Stores live on Tui; marker outcomes ask the surface to open its overlays.
            "history" => return Ok(TuiSubmissionOutcome::PromptHistoryOverlay),
            // The catalogue lives in the surface crate that owns the keymap, so
            // a binding cannot change without its description in the same diff.
            "keys" => agens_tui::shortcuts::shortcuts_dialog(),
            "mcp" => mcp_status_dialog(self.mcp_status.snapshot()),
            "stash" => return Ok(TuiSubmissionOutcome::PromptStashOverlay),
            "mcp:reload" => {
                self.reload_non_ready_mcp_servers()?;
                mcp_status_dialog(self.mcp_status.snapshot())
            }
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
            "subagent-profiles" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?
                    .clone();
                let root = agens_session::root::resolve_tui_session_root(&context, &bootstrap)?;
                let session_config =
                    SessionConfig::resolve(&SessionRoot::confined_to(root), &bootstrap)?;
                let presentation = self.presentation()?;
                let session_effort = self.task_parent_request_config()?.reasoning_effort();
                let compatibility =
                    agens_agents::AgentModelCompatibility::for_context(&bootstrap, &context)?;
                let profiles = session_config.agent_profiles();
                let rows = subagent_catalog(&bootstrap, &context)?
                    .map(|agent| {
                        let resolved = AgentProfileResolver::new(profiles).resolve(
                            &agent.name,
                            agent.model.as_deref(),
                            agent.reasoning_effort,
                            presentation.model(),
                            session_effort,
                        );
                        let unavailable = !compatibility.is_available(&resolved.model.value);
                        ProfileEditorRow::new(
                            &agent.name,
                            resolved.model.value,
                            resolved.model.origin,
                            resolved.effort.value.map(|value| value.as_str()),
                            resolved.effort.origin,
                            unavailable,
                        )
                    })
                    .collect::<Vec<_>>();
                let mut editor_state = self
                    .profile_editor
                    .lock()
                    .map_err(|_| CliError::storage("profile editor is unavailable"))?;
                let scope = editor_state
                    .as_ref()
                    .map_or(crate::profiles::ProfileScope::Global, |editor| {
                        editor.scope()
                    });
                *editor_state = Some(crate::profiles::ProfileEditor::new(rows));
                editor_state
                    .as_mut()
                    .expect("profile editor was initialized")
                    .set_scope(scope);
                let rows = editor_state
                    .as_ref()
                    .expect("profile editor was initialized")
                    .rows();
                let focus = self
                    .profile_focus
                    .lock()
                    .map_err(|_| CliError::storage("profile focus is unavailable"))?
                    .take();
                let selected = focus.and_then(|name| rows.iter().position(|row| row.name == name));
                let entries = rows
                    .iter()
                    .map(|row| {
                        let unavailable = if row.unavailable {
                            " (unavailable)"
                        } else {
                            ""
                        };
                        let effort = row.effort.value.as_deref().unwrap_or("default");
                        DialogEntry::safe_action(
                            format!(
                                "{} · {} [{}] · {} [{}]{}",
                                row.name,
                                row.model.value,
                                profile_origin_label(row.model.origin),
                                effort,
                                profile_origin_label(row.effort.origin),
                                unavailable
                            ),
                            format!("subagent-profiles:edit:{}", row.name),
                        )
                        .with_id(&row.name)
                    })
                    .collect();
                drop(editor_state);
                let scope_label = match scope {
                    crate::profiles::ProfileScope::Global => "global",
                    crate::profiles::ProfileScope::Project => "project",
                };
                let dialog = DialogView::selection(
                    "Subagent profiles",
                    Some(format!("Enter model · ←/→ effort · ⌫ reset · Tab scope: {scope_label} · / search · Esc close")),
                    entries,
                )
                .with_empty_message("No subagents are available.")
                .with_cancellation_action("subagent-profiles:close")
                .with_selected_key_action(Key::Left, "subagent-profiles:cycle-effort:{selected}:prev")
                .with_selected_key_action(Key::Right, "subagent-profiles:cycle-effort:{selected}:next")
                .with_selected_key_action(Key::Backspace, "subagent-profiles:reset-row:{selected}")
                .with_selected_key_action(Key::Tab, "subagent-profiles:scope:toggle");
                if let Some(index) = selected {
                    dialog.with_selected(index)
                } else {
                    dialog
                }
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
        if route_id == "subagent" || route_id == "mcp:reload" {
            Ok(TuiSubmissionOutcome::SafeDialog(dialog))
        } else {
            Ok(TuiSubmissionOutcome::Dialog(dialog))
        }
    }

    /// Reconnects every configured server that is not currently `Ready`, via
    /// the runtime's real reload path rather than a local re-snapshot.
    ///
    /// A server that never got past configuration (an invalid timeout, a
    /// rejected name) has no factory to retry and is intentionally skipped —
    /// `configured_server_names` never included it in the first place.
    fn reload_non_ready_mcp_servers(&self) -> Result<(), CliError> {
        let mut runtime = self
            .mcp_runtime
            .lock()
            .map_err(|_| CliError::storage("MCP runtime is unavailable"))?;
        let configured = runtime
            .registry
            .lock()
            .map_err(|_| CliError::storage("MCP runtime is unavailable"))?
            .configured_server_names();
        let snapshot = self.mcp_status.snapshot();
        for name in configured {
            let ready = snapshot
                .server(&name)
                .is_some_and(|status| status.state() == McpLifecycleState::Ready);
            if !ready {
                let _ = runtime.reload_server(&name);
            }
        }
        Ok(())
    }

    pub(super) fn session_dialog_outcome(
        &self,
        request: SessionDialogRequest,
    ) -> TuiSubmissionOutcome {
        let fallback_request = request.clone();
        match self.load_session_dialog(request) {
            Ok(dialog) => TuiSubmissionOutcome::Dialog(dialog),
            Err(_) => TuiSubmissionOutcome::Dialog(DialogView::sessions_error(
                fallback_request,
                "Saved sessions could not be loaded.",
            )),
        }
    }

    /// Answers the lineage browser, reporting a failure inside the overlay.
    ///
    /// A lineage that could not be read is shown as an error row rather than as
    /// a local error toast: the reader opened an overlay and the answer belongs
    /// where they are looking.
    pub(super) fn session_tree_outcome(
        &self,
        request: agens_tui::SessionTreeRequest,
    ) -> TuiSubmissionOutcome {
        match self.load_session_tree(&request) {
            Ok(entries) => {
                TuiSubmissionOutcome::Dialog(DialogView::session_tree_page(entries, request))
            }
            Err(error) => {
                TuiSubmissionOutcome::Dialog(DialogView::session_tree_error(request, error.message))
            }
        }
    }

    fn load_session_tree(
        &self,
        request: &agens_tui::SessionTreeRequest,
    ) -> Result<Vec<DialogEntry>, CliError> {
        let bootstrap = self.bootstrap()?;
        let current_session = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?
            .identifier;
        let session_id = self.requested_session(request.root(), current_session)?;
        let store = SessionStore::open(bootstrap.data_directory())
            .map_err(|_| CliError::storage("sessions database is unavailable"))?;

        let root = lineage_root(&store, session_id)
            .map_err(|_| CliError::storage("this session's lineage could not be read"))?;
        lineage_entries(&store, root, current_session, (self.clock)())
            .map_err(|_| CliError::storage("this session's lineage could not be read"))
    }

    /// Copies the session up to the reader's fork point and enters the copy.
    ///
    /// The fork is a session like any other, so entering it is the ordinary
    /// resume path rather than a second way in. The source is left untouched:
    /// forking is the non-destructive counterpart of `/undo`, which is why a
    /// failure here can always leave the reader where they already were.
    pub(super) fn fork_session_outcome(
        &self,
        request: agens_tui::SessionForkRequest,
        cancellation: &TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        match self.fork_and_enter(&request, cancellation) {
            Ok(outcome) => outcome,
            Err(error) => TuiSubmissionOutcome::LocalActionableError {
                message: error.to_string(),
                action: TUI_ERROR_ACTION.into(),
            },
        }
    }

    fn fork_and_enter(
        &self,
        request: &agens_tui::SessionForkRequest,
        cancellation: &TuiRouteCancellation,
    ) -> Result<TuiSubmissionOutcome, CliError> {
        let bootstrap = self.bootstrap()?;
        let expected = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?
            .clone();
        let source_id = self.requested_session(request.session(), expected.identifier)?;

        // The live history is the one the terminal counted its turns against, so
        // it is what the cut is derived from whenever the fork is of the session
        // the reader is actually in.
        let history = if request.session().is_some() || expected.identifier != Some(source_id) {
            SessionStore::open(bootstrap.data_directory())
                .map_err(|_| CliError::storage("sessions database is unavailable"))?
                .load_session_for_resume(source_id)
                .map_err(|_| CliError::storage("the session to fork is unavailable"))?
                .messages
        } else {
            expected.messages.clone()
        };

        let cut = fork_cut_message_count(&history, request.turn_prefix())
            .ok_or_else(|| CliError::usage("there is no turn to fork at"))?;

        let mut store = SessionStore::open(bootstrap.data_directory())
            .map_err(|_| CliError::storage("sessions database is unavailable"))?;
        let sequence = store
            .message_sequence_at(source_id, cut)
            .map_err(|_| CliError::storage("the fork point could not be read"))?
            .ok_or_else(|| CliError::usage("this session has nothing saved to fork"))?;
        let fork_id = store
            .fork_session(source_id, sequence)
            .map_err(|error| CliError::storage(error.to_string()))?;
        drop(store);

        let resumed = prepare_loaded_tui_session_resume(
            &bootstrap,
            fork_id,
            load_tui_session_for_resume(&bootstrap, fork_id)?,
            &self.credentials,
        )?;

        commit_tui_session_resume(
            &bootstrap,
            &self.session,
            &expected,
            resumed,
            cancellation,
            |context| self.on_session_resume_committed(&bootstrap, context),
        )
    }

    /// The session a lineage or fork request names, or the one the reader is in.
    ///
    /// The terminal cannot name a session — it is shown labels, not identities —
    /// so a request without one is about the active session, and a session that
    /// has never been saved has no identity to resolve to yet.
    fn requested_session(
        &self,
        requested: Option<&str>,
        current_session: Option<i64>,
    ) -> Result<i64, CliError> {
        match requested {
            Some(identifier) => identifier
                .parse()
                .map_err(|_| CliError::usage("session action is invalid")),
            None => current_session
                .ok_or_else(|| CliError::usage("this session has not been saved yet")),
        }
    }

    pub(super) fn load_session_dialog(
        &self,
        request: SessionDialogRequest,
    ) -> Result<DialogView, CliError> {
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

    #[cfg(any(test, feature = "test-support"))]
    pub fn route_dialog_action(
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

    pub(super) fn route_dialog_action_with_cancellation(
        &self,
        action_id: &str,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
        cancellation: &TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        match action_id {
            "login:chatgpt" => {
                return self.open_dialog("connect").unwrap_or_else(|error| {
                    TuiSubmissionOutcome::LocalActionableError {
                        message: error.to_string(),
                        action: TUI_ERROR_ACTION.into(),
                    }
                });
            }
            "login:api-key:openai-api" => {
                return TuiSubmissionOutcome::SecretEntry(agens_tui::SecretEntryView::new(
                    "Sign in to OpenAI API",
                    Some("Enter your OpenAI API key"),
                    action_id,
                ));
            }
            "login:api-key:moonshotai" => {
                return TuiSubmissionOutcome::SecretEntry(agens_tui::SecretEntryView::new(
                    "Sign in to Moonshot AI",
                    Some("Enter your Moonshot AI API key"),
                    action_id,
                ));
            }
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
                if let Some(action) = action_id.strip_prefix("subagent-profiles:") {
                    return self.apply_profile_editor_action(action);
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
                let message = if let Some(selection) = action_id.strip_prefix("model:") {
                    match selection.split_once(':') {
                        Some((provider, model)) => {
                            self.apply_provider_model(&bootstrap, provider, model)?
                        }
                        None => apply_tui_model(&bootstrap, selection, &self.session)?,
                    }
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

    fn apply_profile_editor_action(&self, action: &str) -> Result<TuiSubmissionOutcome, CliError> {
        use crate::profiles::CycleDirection;

        if action == "close" {
            self.profile_editor
                .lock()
                .map_err(|_| CliError::storage("profile editor is unavailable"))?
                .take();
            self.profile_focus
                .lock()
                .map_err(|_| CliError::storage("profile focus is unavailable"))?
                .take();
            return Ok(TuiSubmissionOutcome::LocalInfo(
                "Subagent profiles closed.".into(),
            ));
        }
        if action == "back" {
            return self.open_dialog("subagent-profiles");
        }
        if action == "scope:toggle" {
            self.profile_editor
                .lock()
                .map_err(|_| CliError::storage("profile editor is unavailable"))?
                .as_mut()
                .ok_or_else(|| CliError::usage("profile editor is unavailable"))?
                .toggle_scope();
            return self.open_dialog("subagent-profiles");
        }
        if let Some(name) = action.strip_prefix("edit:") {
            return self.profile_model_dialog(name);
        }
        if let Some(rest) = action.strip_prefix("set-model:") {
            let (name, model) = rest
                .split_once(':')
                .ok_or_else(|| CliError::usage("profile model action is invalid"))?;
            let scope = self
                .profile_editor
                .lock()
                .map_err(|_| CliError::storage("profile editor is unavailable"))?
                .as_ref()
                .ok_or_else(|| CliError::usage("profile editor is unavailable"))?
                .scope();
            let store = self
                .profile_store
                .as_ref()
                .ok_or_else(|| CliError::unavailable("profile storage is unavailable"))?;
            store
                .save(
                    scope,
                    name,
                    &agens_config::AgentProfilePatch {
                        model: Some(Some(model.to_owned())),
                        effort: None,
                    },
                )
                .map_err(CliError::storage)?;
            *self
                .profile_focus
                .lock()
                .map_err(|_| CliError::storage("profile focus is unavailable"))? =
                Some(name.to_owned());
            self.profile_editor
                .lock()
                .map_err(|_| CliError::storage("profile editor is unavailable"))?
                .take();
            return self.open_dialog("subagent-profiles");
        }
        if let Some(rest) = action.strip_prefix("cycle-effort:") {
            let (name, direction) = rest
                .rsplit_once(':')
                .ok_or_else(|| CliError::usage("profile effort action is invalid"))?;
            let direction = match direction {
                "prev" => CycleDirection::Prev,
                "next" => CycleDirection::Next,
                _ => return Err(CliError::usage("profile effort direction is invalid")),
            };
            let (scope, effort) = {
                let editor = self
                    .profile_editor
                    .lock()
                    .map_err(|_| CliError::storage("profile editor is unavailable"))?;
                let editor = editor
                    .as_ref()
                    .ok_or_else(|| CliError::usage("profile editor is unavailable"))?;
                (
                    editor.scope(),
                    editor
                        .effort_after(name, direction)
                        .ok_or_else(|| CliError::usage("profile agent is unavailable"))?,
                )
            };
            let store = self
                .profile_store
                .as_ref()
                .ok_or_else(|| CliError::unavailable("profile storage is unavailable"))?;
            store
                .save(
                    scope,
                    name,
                    &agens_config::AgentProfilePatch {
                        model: None,
                        effort: Some(effort),
                    },
                )
                .map_err(CliError::storage)?;
            *self
                .profile_focus
                .lock()
                .map_err(|_| CliError::storage("profile focus is unavailable"))? =
                Some(name.to_owned());
            self.profile_editor
                .lock()
                .map_err(|_| CliError::storage("profile editor is unavailable"))?
                .take();
            return self.open_dialog("subagent-profiles");
        }
        if let Some(name) = action.strip_prefix("reset-row:") {
            let scope = self
                .profile_editor
                .lock()
                .map_err(|_| CliError::storage("profile editor is unavailable"))?
                .as_ref()
                .ok_or_else(|| CliError::usage("profile editor is unavailable"))?
                .scope();
            let store = self
                .profile_store
                .as_ref()
                .ok_or_else(|| CliError::unavailable("profile storage is unavailable"))?;
            store
                .save(
                    scope,
                    name,
                    &agens_config::AgentProfilePatch {
                        model: Some(None),
                        effort: Some(None),
                    },
                )
                .map_err(CliError::storage)?;
            *self
                .profile_focus
                .lock()
                .map_err(|_| CliError::storage("profile focus is unavailable"))? =
                Some(name.to_owned());
            self.profile_editor
                .lock()
                .map_err(|_| CliError::storage("profile editor is unavailable"))?
                .take();
            return self.open_dialog("subagent-profiles");
        }
        Err(CliError::usage("profile editor action is unavailable"))
    }

    fn profile_model_dialog(&self, name: &str) -> Result<TuiSubmissionOutcome, CliError> {
        let bootstrap = self.bootstrap()?;
        let context = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        let active = resolved_provider(&bootstrap, &context);
        drop(context);
        let mut entries = Vec::new();
        for provider in providers_with_active_first(active) {
            let selector = ModelSelection::for_source(provider.default_model(), provider.source());
            for model in selector.models().map_err(CliError::unavailable)? {
                entries.push(DialogEntry::safe_action(
                    format!("{} · {}", model.id, provider.label()),
                    format!("subagent-profiles:set-model:{name}:{}", model.id),
                ));
            }
        }
        Ok(TuiSubmissionOutcome::Dialog(
            DialogView::selection(
                "Choose profile model",
                Some("Active provider catalog · Esc back"),
                entries,
            )
            .with_cancellation_action("subagent-profiles:back"),
        ))
    }

    pub(super) fn recover_tui_session_attempt(
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
}
