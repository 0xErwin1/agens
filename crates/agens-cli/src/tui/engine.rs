//! The production `TuiEngine` implementation and the entry points that drive
//! an interactive TUI session (`run_production_tui`) or a single scripted
//! prompt (`run_tui_prompt*`, test-only) to completion.

use std::sync::{Arc, Mutex};

use agens_core::{HeadlessTurnCancellation, HeadlessTurnError, PermissionMode, TurnProgressSink};
use agens_tools::{
    CommandCatalog, SkillCatalog, TaskExecutionRegistry, TaskMessageSource, TaskMessageTarget,
};
use agens_tui::{
    BridgeCancel, Engine as TuiEngine, Tui, TuiProviderOutcome, TuiSubmissionOutcome,
    TuiSubmitOrigin, run_with_default_progress_submit_with_permissions_and_task_controls,
};

use crate::Bootstrap;
use crate::bootstrap::seed_configured_reasoning_effort;
use crate::diagnostics::next_diagnostic_reference;
use crate::dispatch::{
    launch_selected_tui_task, origin_launches_selected_subagent, selected_tui_task_skips_parent,
};
use crate::error::{CliError, ExitStatus};
use crate::headless::{
    HeadlessChatCompletion, HeadlessChatFailure, HeadlessChatRequest,
    run_production_headless_chat_with_progress,
};
use crate::permissions::production_tui_permission_bridge;
use crate::tools::runner::{ProductionTaskRunner, TuiTaskControls, TuiTaskLifecycleBridge};
use crate::tools::runtime::task_execution_limits;
use crate::tools::task::{
    production_tui_task_runtime, production_tui_task_runtime_with_runner_and_parent_config,
};
use crate::tui::agents::persist_pending_agent_correction;
use crate::tui::extensions::{start_tui_commands, start_tui_skills};
use crate::tui::files::{expand_tui_file_reference, tui_picker_file_candidates};
use crate::tui::metrics::{TuiMetricsPublisher, finish_tui_metrics};
use crate::tui::models::seed_remembered_tui_selection;
use crate::tui::provider::TuiCredentialResolver;
use crate::tui::resume::{
    ensure_active_tui_agent_runtime, resume_tui_session, resumed_subagent_cards,
};
use crate::tui::router::{TuiRuntimeRouter, tui_provider_outcome};
use crate::tui::session::{ResumeDraft, TuiSessionContext};
use crate::tui::turn::{complete_tui_turn, tui_session_presentation};

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
    let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
    let task_controls = TuiTaskControls(TaskExecutionRegistry::with_limits(task_execution_limits(
        bootstrap.subagent_limits(),
    )));
    let engine = ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    };
    let mut tui = Tui::new(engine);
    configure_tui_project_identity(&mut tui, bootstrap);
    tui.set_collapse_thinking(bootstrap.collapse_thinking);
    let skills = start_tui_skills(&mut tui, bootstrap)?;
    if let Some(identifier) = resume {
        let mut resumed = resume_tui_session(
            bootstrap,
            identifier,
            &skills,
            &TuiCredentialResolver::production(),
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
            history: std::mem::take(&mut resumed.restored_history),
            draft,
            resume_error,
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

    let commands = start_tui_commands(&mut tui, bootstrap)?;
    report_tui_extension_collisions(&mut tui, &commands, &skills);
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        session,
        Arc::clone(&cancellation),
        commands,
        Arc::clone(&skills),
    );
    tui.set_palette_entries(router.palette_entries().to_vec());
    match tui_picker_file_candidates(bootstrap) {
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
            let mut task_runtime = match production_tui_task_runtime(
                &runtime_bootstrap,
                &router.skills,
                prompt_bridge.clone(),
                lifecycle_bridge.clone(),
                task_parent_request_config.clone(),
                task_diagnostic_reference.clone(),
            ) {
                Ok(runtime) => runtime,
                Err(error) => return tui_provider_outcome(Err(error)),
            };
            if let Err(error) = ensure_active_tui_agent_runtime(
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
                        matches!(origin, TuiSubmitOrigin::Background),
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
                Some(Arc::clone(&router.skills)),
                |request| {
                    let project_root = runtime_bootstrap.project_root().ok_or_else(|| {
                        CliError::configuration("native tools require a project root")
                    })?;
                    let task_runtime = production_tui_task_runtime_with_runner_and_parent_config(
                        &runtime_bootstrap,
                        &router.skills,
                        prompt_bridge.clone(),
                        ProductionTaskRunner::new(
                            runtime_bootstrap.clone(),
                            project_root.to_path_buf(),
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
                        Some(prompt_bridge.clone()),
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
    session: &Arc<Mutex<TuiSessionContext>>,
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
                None,
                None,
                None,
            )
        }),
    }
}

pub(crate) fn run_tui_prompt_with(
    bootstrap: &Bootstrap,
    prompt: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
    skills: Option<Arc<SkillCatalog>>,
    run: impl FnOnce(HeadlessChatRequest) -> Result<HeadlessChatCompletion, HeadlessChatFailure>,
) -> Result<String, CliError> {
    let prompt = expand_tui_file_reference(bootstrap, prompt)?;
    let request = {
        let mut session = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        if session.running {
            return Err(CliError::runtime(HeadlessTurnError::State));
        }
        session.running = true;
        let mut request = session.apply_to(HeadlessChatRequest {
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
        });
        if let Some(skills) = skills {
            let base = request
                .system_prompt
                .take()
                .or_else(|| bootstrap.system_prompt.clone())
                .unwrap_or_else(|| "You are Agens, a helpful coding agent.".into());
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

pub(crate) fn configure_tui_project_identity(
    tui: &mut Tui<ProductionTuiEngine>,
    bootstrap: &Bootstrap,
) {
    if let Some(project_root) = bootstrap.project_root() {
        tui.set_project(project_root.display().to_string());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use agens_tui::{Action, Event, Key, TuiPresentation};

    use super::*;
    use crate::CliDependencies;
    use crate::bootstrap::bootstrap;
    use crate::test_support::render_tui_test_backend;

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
}
