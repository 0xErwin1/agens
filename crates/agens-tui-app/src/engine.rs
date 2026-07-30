//! The production `TuiEngine` implementation and the entry points that drive
//! an interactive TUI session (`run_production_tui`) or a single scripted
//! prompt (`run_tui_prompt*`, test-only) to completion.

use std::sync::{Arc, Mutex};

use agens_core::SubmitOrigin;
use agens_core::{HeadlessTurnCancellation, HeadlessTurnError, PermissionMode, TurnProgressSink};
use agens_tools::{
    CommandCatalog, SkillCatalog, TaskExecutionRegistry, TaskMessageSource, TaskMessageTarget,
};
use agens_tui::{
    BridgeCancel, Engine as TuiEngine, Tui, TuiProviderOutcome, TuiSubmissionOutcome,
    run_with_default_progress_submit_with_permissions_and_task_controls,
};

use crate::extensions::{start_tui_commands, start_tui_skills};
use crate::files::{expand_tui_file_reference, tui_picker_file_candidates};
use crate::metrics::{TuiMetricsPublisher, finish_tui_metrics};
use crate::models::seed_remembered_tui_selection;
#[cfg(any(test, feature = "test-support"))]
use crate::permission_prompt::TtyPermissionPrompter;
use crate::permission_prompt::TuiPermissionPrompter;
use crate::permission_prompt::production_tui_permission_bridge;
use crate::resume::{ResumedTuiSession, resume_tui_session, resumed_subagent_cards};
use crate::router::{TuiRuntimeRouter, tui_provider_outcome};
use crate::turn::{complete_tui_turn, tui_session_presentation};
use agens_agents::ensure_active_agent_runtime;
use agens_agents::persist_pending_agent_correction;
use agens_bootstrap::Bootstrap;
use agens_diagnostics::next_diagnostic_reference;
use agens_dispatch::origin_launches_selected_subagent;
use agens_error::{CliError, ExitStatus};
use agens_headless::seed_configured_reasoning_effort;
use agens_headless::{
    HeadlessChatCompletion, HeadlessChatFailure, HeadlessChatRequest,
    run_production_headless_chat_with_progress,
};
use agens_session::context::{ResumeDraft, SessionContext};
use agens_session::provider::CredentialResolver;
use agens_store::SessionStore;
use agens_tool_runtime::runner::{ProductionTaskRunner, TuiTaskControls, TuiTaskLifecycleBridge};
use agens_tool_runtime::runtime::task_execution_limits;
use agens_tool_runtime::task::{
    production_tui_task_runtime, production_tui_task_runtime_with_runner_and_parent_config,
};
use agens_tool_runtime::{
    launch_selected_task as launch_selected_tui_task,
    selected_task_skips_parent as selected_tui_task_skips_parent,
};

pub struct ProductionTuiEngine {
    pub cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
}

impl TuiEngine for ProductionTuiEngine {
    fn cancel(&mut self) {
        if let Ok(cancellation) = self.cancellation.lock()
            && let Some(cancellation) = cancellation.as_ref()
        {
            cancellation.cancel();
        }
    }
}

pub fn run_production_tui(bootstrap: &Bootstrap, resume: Option<i64>) -> Result<String, CliError> {
    let cancellation = Arc::new(Mutex::new(None));
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let task_controls = TuiTaskControls(TaskExecutionRegistry::with_limits(task_execution_limits(
        bootstrap.subagent_limits(),
    )));
    let engine = ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    };
    let mut tui = Tui::new(engine);
    configure_tui_project_identity(&mut tui, bootstrap);
    tui.set_collapse_thinking(bootstrap.collapse_thinking);
    if let Some(identifier) = resume {
        // The catalog this parameter would otherwise select is instead rediscovered below, once
        // the session's own root is known, so this value is not read for a production resume.
        let ResumedTuiSession {
            context: mut resumed,
            history,
        } = resume_tui_session(
            bootstrap,
            identifier,
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        )?;
        persist_pending_agent_correction(bootstrap, &mut resumed);
        let presentation = tui_session_presentation(bootstrap, &resumed);
        let message = resumed.note();
        let draft = resumed.resume_draft.take().map(ResumeDraft::into_inner);
        let resume_error = resumed.resume_error.clone();
        resumed.resume_notice = None;
        tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
            message,
            presentation,
            history,
            draft,
            resume_error,
            // The real picker candidates and palette are set below, once the session's own root
            // is resolved and `start_tui_skills`/`start_tui_commands` have run against it.
            file_candidates: Vec::new(),
            palette_entries: Vec::new(),
        });
        for event in resumed_subagent_cards(&resumed.messages) {
            tui.apply_runtime_event(event);
        }
        *session.lock().map_err(|_| {
            CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
        })? = resumed;
    } else {
        let mut context = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        let notice = seed_remembered_tui_selection(bootstrap, &mut context);
        seed_bypass_permissions_from_configuration(bootstrap, &mut context)?;
        tui.apply_presentation(tui_session_presentation(bootstrap, &context));
        drop(context);
        if let Some(notice) = notice {
            tui.add_info(notice);
        }
    }

    let session_root_for_startup = {
        let context = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        agens_session::root::resolve_tui_session_root(&context, bootstrap)?
    };
    let skills = start_tui_skills(&mut tui, bootstrap, &session_root_for_startup)?;
    let commands = start_tui_commands(&mut tui, bootstrap, &session_root_for_startup)?;
    report_tui_extension_collisions(&mut tui, &commands, &skills);
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        session,
        Arc::clone(&cancellation),
        commands,
        Arc::clone(&skills),
    );
    tui.set_palette_entries(router.palette_entries()?);
    let picker_candidates = router
        .session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))
        .and_then(|context| tui_picker_file_candidates(&context, bootstrap));
    match picker_candidates {
        Ok(candidates) => tui.set_file_candidates(candidates),
        Err(error) => tui.add_info(format!("File references are unavailable: {error}")),
    }
    let route_router = router.clone();
    let (permission_bridge, permission_requests) = production_tui_permission_bridge();
    let transition_controls = task_controls.clone();
    let cancel_controls = task_controls.clone();
    let message_controls = task_controls.clone();
    let submit_task_controls = task_controls.clone();
    let prompt_bridge = permission_bridge.clone();
    let tui_result = run_with_default_progress_submit_with_permissions_and_task_controls(
        &mut tui,
        move |request, progress, cancellation| {
            route_router.route_request_with_cancellation(request, progress, cancellation)
        },
        move |prompt, origin, progress, metrics| {
            let task_events = metrics.clone();
            let turn_cancellation =
                HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(120));
            let Ok(mut active) = cancellation.lock() else {
                return tui_provider_outcome(Err(CliError::new(
                    ExitStatus::Failure,
                    "ui",
                    "TUI cancellation is unavailable",
                )));
            };
            *active = Some(turn_cancellation.clone());
            drop(active);

            let model_id = router
                .presentation()
                .map(|presentation| presentation.model().to_owned())
                .unwrap_or_default();
            let metrics = Arc::new(Mutex::new(TuiMetricsPublisher::new(
                metrics,
                BridgeCancel::new(),
                model_id,
            )));
            let metrics_progress = Arc::clone(&metrics);
            let sink: TurnProgressSink = Arc::new(move |event| {
                if let Ok(mut metrics) = metrics_progress.lock() {
                    metrics.observe(&event);
                }
                let _ = progress.send(event);
            });
            let runtime_bootstrap = match router.turn_bootstrap() {
                Ok(bootstrap) => bootstrap,
                Err(error) => return tui_provider_outcome(Err(error)),
            };
            let task_parent_request_config = match router.task_parent_request_config() {
                Ok(config) => config,
                Err(error) => return tui_provider_outcome(Err(error)),
            };
            let task_diagnostic_reference = next_diagnostic_reference();
            let lifecycle_bridge =
                TuiTaskLifecycleBridge::new(task_events, submit_task_controls.clone())
                    .with_session_writer(runtime_bootstrap.clone(), Arc::clone(&router.session));
            let session_project_root = match router
                .session
                .lock()
                .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))
                .and_then(|context| {
                    agens_session::root::resolve_tui_session_root(&context, &runtime_bootstrap)
                }) {
                Ok(root) => root,
                Err(error) => return tui_provider_outcome(Err(error)),
            };
            let skills = match router.skills() {
                Ok(skills) => skills,
                Err(error) => return tui_provider_outcome(Err(error)),
            };
            // The runtime built here backs `runtime.authorized`, the ONLY path a TUI-selected
            // subagent launch reaches (`launch_selected_tui_task` below). It must carry the same
            // bypass state as the session's own turn, or a bypassed session still prompts when
            // launching a selected subagent — see the discovery this fixed for the full trace.
            let session_bypass =
                match router.session.lock().map_err(|_| {
                    CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
                }) {
                    Ok(context) => context.bypass_permissions,
                    Err(error) => return tui_provider_outcome(Err(error)),
                };
            let mut task_runtime = match production_tui_task_runtime(
                &runtime_bootstrap,
                &session_project_root,
                &skills,
                Box::new(TuiPermissionPrompter(prompt_bridge.clone())),
                lifecycle_bridge.clone(),
                task_parent_request_config.clone(),
                task_diagnostic_reference.clone(),
                session_bypass,
            ) {
                Ok(runtime) => runtime,
                Err(error) => return tui_provider_outcome(Err(error)),
            };
            if let Err(error) = ensure_active_agent_runtime(
                &runtime_bootstrap,
                &router.session,
                &task_runtime.dispatcher,
            ) {
                return tui_provider_outcome(Err(error));
            }
            let selected_launch = if origin_launches_selected_subagent(origin) {
                selected_tui_task_skips_parent(
                    launch_selected_tui_task(
                        &mut task_runtime,
                        &router.session,
                        &prompt,
                        matches!(origin, SubmitOrigin::Background),
                        &turn_cancellation,
                    ),
                    &lifecycle_bridge,
                )
            } else {
                Ok(false)
            };
            match selected_launch {
                Ok(true) => return TuiProviderOutcome::Backgrounded,
                Ok(false) => {}
                Err(error) => return tui_provider_outcome(Err(error)),
            }
            let result = run_tui_prompt_with(
                &runtime_bootstrap,
                &prompt,
                &router.session,
                Some(Arc::clone(&skills)),
                |request| {
                    let task_runtime = production_tui_task_runtime_with_runner_and_parent_config(
                        &runtime_bootstrap,
                        &session_project_root,
                        &skills,
                        Box::new(TuiPermissionPrompter(prompt_bridge.clone())),
                        ProductionTaskRunner::new(
                            runtime_bootstrap.clone(),
                            session_project_root.clone(),
                        )
                        .with_lifecycle_bridge(lifecycle_bridge.clone())
                        .with_dangerous_mode(request.dangerous_mode)
                        .with_bypass(request.dangerously_allow_all),
                        task_parent_request_config.clone(),
                        Some(task_diagnostic_reference.clone()),
                    )?;
                    run_production_headless_chat_with_progress(
                        request,
                        &runtime_bootstrap,
                        &turn_cancellation,
                        Some(&sink),
                        Box::new(TuiPermissionPrompter(prompt_bridge.clone())),
                        Some(&task_runtime),
                        Some(&task_diagnostic_reference),
                    )
                },
            );

            finish_tui_metrics(&metrics, &result);

            if let Ok(mut active) = cancellation.lock() {
                *active = None;
            }

            tui_provider_outcome(result)
        },
        move |id| transition_controls.transition_to_background(id),
        move |id| {
            cancel_controls
                .0
                .cancel(agens_tools::TaskExecutionId::from_value(id))
        },
        move |id, message| {
            message_controls
                .0
                .send_message(
                    TaskMessageSource::User,
                    TaskMessageTarget::Execution(agens_tools::TaskExecutionId::from_value(id)),
                    message,
                )
                .is_ok()
        },
        Some((permission_bridge, permission_requests)),
    );
    task_controls.0.cancel_all();
    let _ = task_controls
        .0
        .wait_for_idle(std::time::Duration::from_secs(2));
    tui_result.map_err(|_| CliError::new(ExitStatus::Failure, "ui", "terminal UI failed"))?;

    Ok(String::new())
}

#[cfg(any(test, feature = "test-support"))]
pub fn run_tui_prompt(
    bootstrap: &Bootstrap,
    prompt: &str,
    cancellation: &HeadlessTurnCancellation,
    session: &Arc<Mutex<SessionContext>>,
    progress: Option<&TurnProgressSink>,
) -> Result<String, CliError> {
    match prompt.trim() {
        command if command.starts_with('/') => {
            let router = TuiRuntimeRouter::new(
                bootstrap.clone(),
                Arc::clone(session),
                Arc::new(Mutex::new(None)),
                Arc::new(CommandCatalog::default()),
                Arc::new(SkillCatalog::default()),
            );
            match router.resolve(command.to_owned())? {
                TuiSubmissionOutcome::LocalInfo(message)
                | TuiSubmissionOutcome::SelectionInfo(message)
                | TuiSubmissionOutcome::ResetSucceeded { message, .. }
                | TuiSubmissionOutcome::ContextChanged { message, .. }
                | TuiSubmissionOutcome::SessionResumed { message, .. } => Ok(message),
                TuiSubmissionOutcome::ProviderTurn { .. }
                | TuiSubmissionOutcome::LocalActionableError { .. }
                | TuiSubmissionOutcome::Dialog(_)
                | TuiSubmissionOutcome::SafeDialog(_)
                | TuiSubmissionOutcome::TranscriptDialog
                | TuiSubmissionOutcome::SelectionCancelled
                | TuiSubmissionOutcome::RouteCancelled
                | TuiSubmissionOutcome::SelectionError { .. } => {
                    unreachable!("slash routing returns a local result or CLI error")
                }
                TuiSubmissionOutcome::Quit => Ok(String::new()),
            }
        }
        prompt => run_tui_prompt_with(bootstrap, prompt, session, None, |request| {
            run_production_headless_chat_with_progress(
                request,
                bootstrap,
                cancellation,
                progress,
                Box::new(TtyPermissionPrompter),
                None,
                None,
            )
        }),
    }
}

pub fn run_tui_prompt_with(
    bootstrap: &Bootstrap,
    prompt: &str,
    session: &Arc<Mutex<SessionContext>>,
    skills: Option<Arc<SkillCatalog>>,
    run: impl FnOnce(HeadlessChatRequest) -> Result<HeadlessChatCompletion, HeadlessChatFailure>,
) -> Result<String, CliError> {
    let prompt = {
        let context = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        expand_tui_file_reference(&context, bootstrap, prompt)?
    };
    let request = {
        let mut session = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        if session.running {
            return Err(CliError::runtime(HeadlessTurnError::State));
        }
        session.running = true;
        let mut request = agens_headless::apply_session_to_request(
            &session,
            HeadlessChatRequest {
                prompt,
                history: Vec::new(),
                model: None,
                system_prompt: None,
                max_iterations: None,
                mode: PermissionMode::Edit,
                dangerously_allow_all: false,
                dangerous_mode: false,
                request_config: agens_core::RequestConfig::default(),
                session_reasoning_effort: None,
                session: None,
                active_agent: None,
                effective_capabilities: None,
                pending_system_reminder: None,
                skills: skills.clone(),
            },
        );
        if let Some(skills) = skills {
            let base = match request.system_prompt.take() {
                Some(explicit) => explicit,
                None => tui_turn_system_prompt(&session, bootstrap)?
                    .unwrap_or_else(|| "You are Agens, a helpful coding agent.".into()),
            };
            request.system_prompt = Some(parent_skill_system_prompt(&base, &skills));
        }
        seed_configured_reasoning_effort(&mut request, bootstrap);
        request
    };
    let consumed_reminder = request.pending_system_reminder.is_some();
    let completion = run(request);
    let mut session = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    session.running = false;
    let result = complete_tui_turn(&mut session, completion, consumed_reminder);
    if let Some(identifier) = session.identifier {
        // Best-effort, like the write above: this call gets another attempt on every subsequent
        // completed turn, and the toggle command (the moment the user actually asked for a
        // change) surfaces a failed write directly instead of staying silent here too.
        let _ = write_through_bypass_permission_prompts(
            bootstrap,
            identifier,
            session.bypass_permissions,
        );
    }
    result
}

/// Seeds `context.bypass_permissions` from the GLOBAL `agent.bypass_permission_prompts`
/// configuration, for a session that has nothing of its own recorded yet: a brand-new session, or
/// one just reset by `/new`. A RESUMED session must never call this — its own recorded value (or,
/// absent one, this same configuration fallback) is read once in
/// [`crate::resume::prepare_loaded_tui_session_resume`] instead, so re-seeding it here would
/// silently re-enable a bypass the user deliberately turned off.
pub fn seed_bypass_permissions_from_configuration(
    bootstrap: &Bootstrap,
    context: &mut SessionContext,
) -> Result<(), CliError> {
    let root = agens_session::root::resolve_tui_session_root(context, bootstrap)?;
    let session_root = agens_bootstrap::session_root::SessionRoot::confined_to(root);
    let session_config =
        agens_bootstrap::session_config::SessionConfig::resolve(&session_root, bootstrap)?;
    context.bypass_permissions = session_config.bypass_permission_prompts();
    Ok(())
}

/// Records a session's bypass-permission-prompts value once it has an identifier to record it
/// against. The write itself stays best-effort — like [`agens_agents::persist_pending_agent_correction`]'s
/// own write, a session toggled and then abandoned before its first completed turn has no row to
/// write to yet, and a turn must not hard-fail just because this side record could not be made.
/// Unlike that precedent, though, a failed write here has an unsafe direction: it can leave a
/// stale `true` (or `NULL`, falling back to a `true` global configuration) on disk after the user
/// asked for `false`. The `Result` is therefore returned rather than swallowed, so callers on the
/// toggle path (the moment the user actually asked for a change) can surface it instead of staying
/// silent; see [`crate::router::TuiRuntimeRouter::toggle_bypass_permissions`].
pub(crate) fn write_through_bypass_permission_prompts(
    bootstrap: &Bootstrap,
    identifier: i64,
    enabled: bool,
) -> Result<(), CliError> {
    SessionStore::open(bootstrap.data_directory())
        .and_then(|mut store| store.set_bypass_permission_prompts(identifier, enabled))
        .map_err(|_| CliError::storage("permission bypass state could not be saved"))
}

/// The configured system prompt fallback a TUI turn must fall back to, re-derived from the
/// session's own recorded confinement root rather than `bootstrap`'s process-captured
/// `agent.system_prompt` — see [`agens_bootstrap::session_config::SessionConfig`] for why the process root
/// is the wrong source once a session can be resumed into a different root.
pub fn tui_turn_system_prompt(
    context: &SessionContext,
    bootstrap: &Bootstrap,
) -> Result<Option<String>, CliError> {
    let root = agens_session::root::resolve_tui_session_root(context, bootstrap)?;
    let session_root = agens_bootstrap::session_root::SessionRoot::confined_to(root);
    let session_config =
        agens_bootstrap::session_config::SessionConfig::resolve(&session_root, bootstrap)?;
    Ok(session_config.system_prompt().map(ToOwned::to_owned))
}

fn parent_skill_system_prompt(base: &str, skills: &SkillCatalog) -> String {
    if skills.is_empty() {
        return base.to_owned();
    }

    let metadata = skills
        .skills()
        .map(|skill| format!("- {}: {}", skill.name(), skill.description()))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{base}\n\n## Available skills\nUse the `skill` tool to load instructions or declared resources only when needed.\n{metadata}"
    )
}

pub fn report_tui_extension_collisions<E: TuiEngine>(
    tui: &mut Tui<E>,
    commands: &CommandCatalog,
    skills: &SkillCatalog,
) {
    for skill in skills
        .skills()
        .filter(|skill| commands.command(skill.name()).is_some())
    {
        tui.add_diagnostic(format!(
            "Skill /{} is shadowed by a command; command routing wins.",
            skill.name()
        ));
    }
}

/// Labels the TUI header with the process's own current project, before any session has been
/// created or resumed. Purely a display convenience, not a confinement decision, so it is one of
/// the few sites allowed to read the process-wide discovered root directly.
pub fn configure_tui_project_identity(tui: &mut Tui<ProductionTuiEngine>, bootstrap: &Bootstrap) {
    if let Some(root) =
        agens_bootstrap::session_root::SessionRoot::discover_for_new_session(bootstrap)
    {
        tui.set_project(root.path().display().to_string());
    }
}

#[cfg(test)]
mod tests {

    use agens_core::{
        CompletedTurnRepository, CompletedTurnSnapshot, MessagePart, SessionMetadata, TurnEvent,
        TurnState,
    };
    use agens_store::SessionStore;
    use agens_tui::{Action, Event, Key};

    use super::*;
    use crate::test_support::{
        persist_tui_session, rotation_agent, rotation_dispatcher, tui_project,
        tui_session_bootstrap, tui_session_bootstrap_with_global_bypass, tui_session_directory,
        tui_session_messages,
    };
    use agens_fixtures::BundledModelValidator;
    use agens_models::ModelSelection;
    use agens_session::context::ActiveAgentRuntime;

    #[test]
    fn second_control_c_uses_the_owned_turn_cancellation_before_quit() {
        let cancellation = HeadlessTurnCancellation::new();
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(Some(cancellation.clone()))),
        });
        tui.set_running(true);

        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
        assert!(!cancellation.is_cancelled());
        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Quit);
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn tui_session_resume_confines_a_cross_project_session_and_fails_closed_for_missing_and_legacy_records()
     {
        let temporary = tui_session_directory("fail-closed");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let other_project = temporary.join("other").display().to_string();
        persist_tui_session(&mut store, &other_project, "other");
        let saved_sessions = store.list_sessions().unwrap();
        drop(store);
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let original = session.lock().unwrap().clone();

        // Session 1 belongs to a different project than `bootstrap`'s own; it must still
        // resume, confined to its OWN recorded root rather than being rejected.
        run_tui_prompt(
            &bootstrap,
            "/resume 1",
            &HeadlessTurnCancellation::new(),
            &session,
            None,
        )
        .unwrap();
        assert_eq!(
            session.lock().unwrap().metadata.as_ref().unwrap().project,
            other_project
        );
        *session.lock().unwrap() = original.clone();

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/resume 2",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap_err()
            .to_string(),
            "store: saved session is unavailable"
        );
        assert_eq!(*session.lock().unwrap(), original);
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .list_sessions()
                .unwrap(),
            saved_sessions
        );

        let legacy_temporary = tui_session_directory("legacy-fail-closed");
        let legacy_bootstrap = tui_session_bootstrap(&legacy_temporary, &[]);
        let mut legacy_store = SessionStore::open(legacy_bootstrap.data_directory()).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(
                legacy_store.persist_completed_turn(
                    CompletedTurnSnapshot::from_persisted_events(vec![
                        TurnEvent::StateChanged(TurnState::Requesting),
                        TurnEvent::StateChanged(TurnState::Streaming),
                        TurnEvent::ProviderPart(MessagePart::Text("legacy answer".into())),
                        TurnEvent::StateChanged(TurnState::Completed),
                    ])
                    .unwrap(),
                ),
            )
            .unwrap();
        drop(legacy_store);
        let legacy_session = Arc::new(Mutex::new(SessionContext::fresh()));
        let legacy_original = legacy_session.lock().unwrap().clone();
        assert_eq!(
            run_tui_prompt(
                &legacy_bootstrap,
                "/resume 1",
                &HeadlessTurnCancellation::new(),
                &legacy_session,
                None,
            )
            .unwrap_err()
            .to_string(),
            "store: saved session is unavailable"
        );
        assert_eq!(*legacy_session.lock().unwrap(), legacy_original);

        std::fs::remove_dir_all(temporary).unwrap();
        std::fs::remove_dir_all(legacy_temporary).unwrap();
    }

    #[test]
    fn tui_session_busy_resume_and_subagent_commands_leave_context_unchanged() {
        let temporary = tui_session_directory("busy");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let session = Arc::new(Mutex::new(SessionContext {
            identifier: Some(7),
            selected_subagent: Some("reviewer".into()),
            running: true,
            ..SessionContext::fresh()
        }));
        let original = session.lock().unwrap().clone();

        for command in ["/resume 1", "/subagent reviewer"] {
            assert_eq!(
                run_tui_prompt(
                    &bootstrap,
                    command,
                    &HeadlessTurnCancellation::new(),
                    &session,
                    None,
                )
                .unwrap_err()
                .to_string(),
                "runtime: headless turn entered an invalid state"
            );
            assert_eq!(*session.lock().unwrap(), original);
        }

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_agent_command_rotates_to_an_eligible_primary_agent() {
        let temporary = tui_session_directory("agent-command");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "all",
                "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
            )],
        );
        let session = Arc::new(Mutex::new(SessionContext::fresh()));

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/agent all",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap(),
            "Active agent: all."
        );
        assert_eq!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("all")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_subagent_command_selects_an_exact_subagent() {
        let temporary = tui_session_directory("subagent-command");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        ensure_active_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/subagent reviewer",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap(),
            "Subagent: reviewer."
        );
        assert_eq!(
            session.lock().unwrap().selected_subagent.as_deref(),
            Some("reviewer")
        );
        assert_eq!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_fresh_session_seeds_bypass_permissions_from_global_configuration() {
        let temporary = tui_session_directory("fresh-session-bypass-on");
        let bootstrap = tui_session_bootstrap_with_global_bypass(&temporary, true);
        let mut context = SessionContext::fresh();

        seed_bypass_permissions_from_configuration(&bootstrap, &mut context).unwrap();

        assert!(context.bypass_permissions);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_fresh_session_without_a_global_declaration_stays_off() {
        let temporary = tui_session_directory("fresh-session-bypass-off");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut context = SessionContext::fresh();

        seed_bypass_permissions_from_configuration(&bootstrap, &mut context).unwrap();

        assert!(!context.bypass_permissions);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_completed_turn_writes_the_session_bypass_value_through_once_an_identifier_exists() {
        let temporary = tui_session_directory("write-through-bypass");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "seed");
        drop(store);
        let session = Arc::new(Mutex::new(SessionContext {
            bypass_permissions: true,
            ..SessionContext::fresh()
        }));

        let mut next_turn = metadata.clone();
        next_turn.completed_turn_count += 1;
        let result = run_tui_prompt_with(&bootstrap, "next request", &session, None, |_| {
            Ok(HeadlessChatCompletion {
                text: "captured".into(),
                metadata: next_turn,
                messages: Vec::new(),
            })
        });
        assert!(result.is_ok());

        let store = SessionStore::open(bootstrap.data_directory()).unwrap();
        assert_eq!(
            store.bypass_permission_prompts(metadata.id).unwrap(),
            Some(true)
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_new_command_synchronizes_idle_context() {
        let temporary = tui_session_directory("new-command");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let dispatcher = rotation_dispatcher();
        let active_agent = ActiveAgentRuntime::build(
            &rotation_agent("primary", Some("gpt-4.1"), true),
            None,
            &tui_project(&temporary),
            &dispatcher,
            &BundledModelValidator,
        )
        .unwrap();
        let session = Arc::new(Mutex::new(SessionContext {
            identifier: Some(7),
            metadata: Some(SessionMetadata {
                id: 7,
                project: tui_project(&temporary),
                title: "conversation".into(),
                active_agent: "primary".into(),
                provider_id: None,
                model_id: None,
                reasoning_effort: None,
                created_at: 1,
                updated_at: 1,
                completed_turn_count: 1,
                resumable: true,
            }),
            messages: tui_session_messages(),
            active_agent: Some(active_agent),
            pending_system_reminder: Some("previous reminder".into()),
            selection: Some(ModelSelection::new("gpt-4.1")),
            selected_subagent: Some("reviewer".into()),
            ..SessionContext::fresh()
        }));

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/new",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap(),
            "Started a new session."
        );
        assert_eq!(*session.lock().unwrap(), SessionContext::fresh());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_busy_agent_command_leaves_context_and_store_unchanged() {
        let temporary = tui_session_directory("busy-agent-command");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "all",
                "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
            )],
        );
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "current");
        let saved_sessions = store.list_sessions().unwrap();
        drop(store);
        let session = Arc::new(Mutex::new(SessionContext {
            identifier: Some(metadata.id),
            metadata: Some(metadata),
            messages: tui_session_messages(),
            selected_subagent: Some("reviewer".into()),
            running: true,
            ..SessionContext::fresh()
        }));
        let original = session.lock().unwrap().clone();

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/agent all",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap_err()
            .to_string(),
            "runtime: headless turn entered an invalid state"
        );
        assert_eq!(*session.lock().unwrap(), original);
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .list_sessions()
                .unwrap(),
            saved_sessions
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }
}
