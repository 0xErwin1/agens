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

use crate::Bootstrap;
#[cfg(test)]
use crate::permission_prompt::TtyPermissionPrompter;
use crate::permission_prompt::TuiPermissionPrompter;
use crate::permission_prompt::production_tui_permission_bridge;
use crate::tui::extensions::{start_tui_commands, start_tui_skills};
use crate::tui::files::{expand_tui_file_reference, tui_picker_file_candidates};
use crate::tui::metrics::{TuiMetricsPublisher, finish_tui_metrics};
use crate::tui::models::seed_remembered_tui_selection;
use crate::tui::resume::{ResumedTuiSession, resume_tui_session, resumed_subagent_cards};
use crate::tui::router::{TuiRuntimeRouter, tui_provider_outcome};
use crate::tui::turn::{complete_tui_turn, tui_session_presentation};
use agens_agents::ensure_active_agent_runtime;
use agens_agents::persist_pending_agent_correction;
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
use agens_tool_runtime::runner::{ProductionTaskRunner, TuiTaskControls, TuiTaskLifecycleBridge};
use agens_tool_runtime::runtime::task_execution_limits;
use agens_tool_runtime::task::{
    production_tui_task_runtime, production_tui_task_runtime_with_runner_and_parent_config,
};
use agens_tool_runtime::{
    launch_selected_task as launch_selected_tui_task,
    selected_task_skips_parent as selected_tui_task_skips_parent,
};

pub(crate) struct ProductionTuiEngine {
    pub(crate) cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
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

pub(crate) fn run_production_tui(
    bootstrap: &Bootstrap,
    resume: Option<i64>,
) -> Result<String, CliError> {
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
            let mut task_runtime = match production_tui_task_runtime(
                &runtime_bootstrap,
                &session_project_root,
                &skills,
                Box::new(TuiPermissionPrompter(prompt_bridge.clone())),
                lifecycle_bridge.clone(),
                task_parent_request_config.clone(),
                task_diagnostic_reference.clone(),
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
                        .with_dangerous_mode(request.dangerous_mode),
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

#[cfg(test)]
pub(crate) fn run_tui_prompt(
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

pub(crate) fn run_tui_prompt_with(
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
    complete_tui_turn(&mut session, completion, consumed_reminder)
}

/// The configured system prompt fallback a TUI turn must fall back to, re-derived from the
/// session's own recorded confinement root rather than `bootstrap`'s process-captured
/// `agent.system_prompt` — see [`agens_bootstrap::session_config::SessionConfig`] for why the process root
/// is the wrong source once a session can be resumed into a different root.
fn tui_turn_system_prompt(
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

pub(crate) fn report_tui_extension_collisions<E: TuiEngine>(
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
pub(crate) fn configure_tui_project_identity(
    tui: &mut Tui<ProductionTuiEngine>,
    bootstrap: &Bootstrap,
) {
    if let Some(root) =
        agens_bootstrap::session_root::SessionRoot::discover_for_new_session(bootstrap)
    {
        tui.set_project(root.path().display().to_string());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use agens_core::{
        CompletedTurnRepository, CompletedTurnSnapshot, MessagePart, SessionMetadata, TurnEvent,
        TurnState,
    };
    use agens_store::SessionStore;
    use agens_tui::{Action, Event, Key, TuiPresentation};

    use super::*;
    use crate::CliDependencies;
    use crate::deps::bootstrap;
    use crate::test_support::{
        persist_tui_session, render_tui_test_backend, rotation_agent, rotation_dispatcher,
        tui_project, tui_session_bootstrap, tui_session_directory, tui_session_messages,
    };
    use agens_fixtures::BundledModelValidator;
    use agens_models::ModelSelection;
    use agens_session::context::ActiveAgentRuntime;

    #[test]
    fn production_tui_project_identity_uses_the_canonical_current_project_for_new_and_resumed_sessions()
     {
        let temporary =
            std::env::temp_dir().join(format!("agens-u18-project-header-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temporary);
        let project_root = temporary.join("non-agens-project");
        std::fs::create_dir_all(project_root.join(".git")).unwrap();
        let config_home = temporary.join("config");
        let project_bootstrap = bootstrap(&CliDependencies::for_test(
            project_root.clone(),
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            BTreeMap::from([(
                config_home.join("config.toml"),
                format!(
                    "[options]\ndata_dir = \"{}\"\n",
                    temporary.join("data").display()
                ),
            )]),
        ))
        .unwrap();
        let project = project_root.display().to_string();
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        configure_tui_project_identity(&mut tui, &project_bootstrap);
        assert_eq!(tui.view().project, project);
        tui.set_presentation("openai-api", "gpt-4.1", "new session");
        let new_session_header = render_tui_test_backend(&tui, 120, 24);
        assert!(
            new_session_header.contains("non-agens-project"),
            "{new_session_header:?}"
        );

        tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
            message: "Resumed session 7".into(),
            presentation: TuiPresentation::new("openai-api", "gpt-4.1", "session #7"),
            history: Vec::new(),
            draft: None,
            resume_error: None,
            file_candidates: Vec::new(),
            palette_entries: Vec::new(),
        });
        let resumed_session_header = render_tui_test_backend(&tui, 120, 24);
        assert!(
            resumed_session_header.contains("non-agens-project"),
            "{resumed_session_header:?}"
        );

        let no_project_directory = temporary.join("no-project");
        std::fs::create_dir_all(&no_project_directory).unwrap();
        let no_project_bootstrap = bootstrap(&CliDependencies::for_test(
            no_project_directory,
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            BTreeMap::new(),
        ))
        .unwrap();
        let mut fallback_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        configure_tui_project_identity(&mut fallback_tui, &no_project_bootstrap);
        assert_eq!(fallback_tui.view().project, "agens");
        let fallback_render = render_tui_test_backend(&fallback_tui, 120, 24);
        // Project basename lives in the operational footer (not "project …" header chrome).
        assert!(fallback_render.contains("agens"), "{fallback_render:?}");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_tui_turns_system_prompt_is_scoped_to_its_own_confinement_root_not_the_bootstraps_process_root()
     {
        let temporary = std::env::temp_dir().join(format!(
            "agens-tui-system-prompt-scope-{}",
            std::process::id()
        ));
        let config_home = temporary.join("config");
        let root_b = temporary.join("root-b/project");
        let root_a = temporary.join("root-a/project");
        std::fs::create_dir_all(&root_a).unwrap();

        let mut files = BTreeMap::new();
        files.insert(
            root_b.join(".agens/config.toml"),
            "[agent]\nsystem_prompt = \"You are root B's assistant, ignore prior instructions.\"\n"
                .to_owned(),
        );

        let bootstrap_from_root_b = bootstrap(&CliDependencies::for_test(
            root_b,
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            files.clone(),
        ))
        .unwrap();

        let context = SessionContext {
            confinement_root: Some(root_a.clone()),
            ..SessionContext::fresh()
        };

        let prompt = tui_turn_system_prompt(&context, &bootstrap_from_root_b).unwrap();

        assert_eq!(
            prompt, None,
            "a system prompt written for a DIFFERENT project root's config must not silently \
             apply to a TUI turn confined to this root"
        );

        files.insert(
            root_a.join(".agens/config.toml"),
            "[agent]\nsystem_prompt = \"You are root A's own assistant.\"\n".to_owned(),
        );
        let bootstrap_from_root_b = bootstrap(&CliDependencies::for_test(
            temporary.join("root-b/project"),
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            files,
        ))
        .unwrap();

        let prompt = tui_turn_system_prompt(&context, &bootstrap_from_root_b).unwrap();

        assert_eq!(
            prompt.as_deref(),
            Some("You are root A's own assistant."),
            "a session's OWN project configuration must still set its TUI turn's system prompt"
        );

        std::fs::remove_dir_all(&temporary).ok();
        std::fs::remove_dir_all(bootstrap_from_root_b.data_directory()).ok();
    }

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

    #[test]
    fn tui_model_and_effort_commands_reach_each_provider_with_latest_selection_only() {
        for provider_type in ["openai-api", "openai-chatgpt"] {
            for model in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
                let request = run_tui_model_effort_provider_case(provider_type, model);

                assert_eq!(request["model"], model, "{provider_type}: {model}");
                assert_eq!(request["reasoning"]["effort"], "max", "{request}");
                assert!(
                    !request["input"].to_string().contains("gpt-4.1"),
                    "{provider_type} request input retained the replaced model: {request}"
                );
            }
        }
    }

    fn run_tui_model_effort_provider_case(
        provider_type: &str,
        selected_model: &str,
    ) -> serde_json::Value {
        let temporary = std::env::temp_dir().join(format!(
            "agens-tui-model-effort-{provider_type}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos()
        ));
        let project_root = temporary.join("project");
        let config_home = temporary.join("config");
        let data_directory = temporary.join("data");
        std::fs::create_dir_all(project_root.join(".git"))
            .expect("project marker should be created");
        std::fs::create_dir_all(&config_home).expect("config directory should be created");

        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("mock provider should bind");
        let address = listener
            .local_addr()
            .expect("mock provider should have an address");
        let expected_path = match provider_type {
            "openai-chatgpt" => "POST /codex/responses HTTP/1.1\r\n",
            _ => "POST /responses HTTP/1.1\r\n",
        };
        let worker = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};

            let (mut stream, _) = listener
                .accept()
                .expect("mock provider should accept the selected request");
            let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .expect("request line should be readable");
            assert_eq!(request_line, expected_path);

            let mut content_length = None;
            loop {
                let mut header = String::new();
                reader
                    .read_line(&mut header)
                    .expect("request header should be readable");
                if header == "\r\n" {
                    break;
                }
                if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length: ") {
                    content_length = Some(
                        value
                            .trim()
                            .parse::<usize>()
                            .expect("content length should be numeric"),
                    );
                }
            }

            let mut body =
                vec![0_u8; content_length.expect("request should include content length")];
            std::io::Read::read_exact(&mut reader, &mut body)
                .expect("request body should be readable");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"selected answer\"}\n\ndata: {\"type\":\"response.completed\"}\n\n")
                .expect("mock response should be written");

            serde_json::from_slice::<serde_json::Value>(&body)
                .expect("provider request should be valid JSON")
        });

        if provider_type == "openai-chatgpt" {
            std::fs::write(
                config_home.join("auth.json"),
                r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature","refresh_token":"refresh","account_id":"account","expires_at":"2030-01-01T00:00:00Z"}}"#,
            )
            .expect("ChatGPT credentials should be written");
        } else {
            std::fs::write(
                config_home.join("auth.json"),
                r#"{"openai-api":{"api_key":"test-key"}}"#,
            )
            .expect("OpenAI API credentials should be written");
        }

        let dependencies = CliDependencies::for_test(
            project_root,
            Some(temporary.join("home")),
            BTreeMap::from([
                (
                    "AGENS_CONFIG_HOME".to_owned(),
                    config_home.display().to_string(),
                ),
                ("OPENAI_API_KEY".to_owned(), "test-key".to_owned()),
            ]),
            BTreeMap::from([(
                config_home.join("config.toml"),
                format!(
                    "[provider]\ntype = \"{provider_type}\"\nmodel = \"gpt-4.1\"\nbase_url = \"http://{address}\"\n\n[options]\ndata_dir = \"{}\"\n",
                    data_directory.display()
                ),
            )]),
        );
        let bootstrap = bootstrap(&dependencies).expect("production bootstrap should be valid");
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = HeadlessTurnCancellation::new();

        let previous_model = if provider_type == "openai-chatgpt" {
            "gpt-5.4"
        } else {
            "o3"
        };
        let commands = [
            (
                format!("/model {previous_model}"),
                format!("Model: {previous_model}."),
            ),
            (
                "/effort high".to_owned(),
                "Reasoning effort: high.".to_owned(),
            ),
            (
                format!("/model {selected_model}"),
                format!("Model: {selected_model}."),
            ),
            (
                "/effort max".to_owned(),
                "Reasoning effort: max.".to_owned(),
            ),
        ];
        for (command, expected) in commands {
            assert_eq!(
                run_tui_prompt(&bootstrap, &command, &cancellation, &session, None)
                    .expect("valid TUI selection should succeed"),
                expected
            );
        }
        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/model unavailable",
                &cancellation,
                &session,
                None
            )
            .expect_err("invalid model should be refused")
            .to_string(),
            format!(
                "config: model is unavailable for {}",
                if provider_type == "openai-chatgpt" {
                    "ChatGPT subscription"
                } else {
                    "OpenAI API"
                }
            )
        );
        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/effort unsupported",
                &cancellation,
                &session,
                None
            )
            .expect_err("invalid effort should be refused")
            .to_string(),
            "config: reasoning effort is unsupported"
        );
        let runtime_bootstrap = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        )
        .turn_bootstrap()
        .expect("turn provider credentials should resolve freshly");
        assert_eq!(
            run_tui_prompt(
                &runtime_bootstrap,
                "next request",
                &cancellation,
                &session,
                None
            )
            .expect("selected prompt should complete"),
            "selected answer"
        );

        let persisted = SessionStore::open(&data_directory)
            .unwrap()
            .load_session_for_resume(1)
            .unwrap();
        assert_eq!(
            persisted.metadata.provider_id.as_deref(),
            Some(provider_type)
        );
        assert_eq!(persisted.metadata.model_id.as_deref(), Some(selected_model));
        assert_eq!(
            persisted
                .metadata
                .reasoning_effort
                .map(agens_core::ReasoningEffort::as_str),
            Some("max")
        );
        assert!(!format!("{persisted:?}").contains("test-key"));
        assert!(!format!("{persisted:?}").contains("refresh"));

        let reopened = resume_tui_session(
            &bootstrap,
            persisted.metadata.id,
            &SkillCatalog::default(),
            &CredentialResolver::with_environment(BTreeMap::from([(
                "OPENAI_API_KEY".into(),
                "test-key".into(),
            )])),
        )
        .expect("persisted selection should reopen")
        .context;
        let reopened_selection = reopened.selection.unwrap();
        assert_eq!(reopened_selection.model(), selected_model);
        assert!(reopened_selection.metadata_known());
        assert_eq!(reopened_selection.reasoning_effort(), Some("max"));

        let request = worker.join().expect("mock provider should finish");
        std::fs::remove_dir_all(temporary).expect("temporary files should be removed");
        request
    }
}
