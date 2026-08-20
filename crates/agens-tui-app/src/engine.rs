//! The production `TuiEngine` implementation and the entry points that drive
//! an interactive TUI session (`run_production_tui`) or a single scripted
//! prompt (`run_tui_prompt*`, test-only) to completion.

use std::sync::{Arc, Mutex};

use agens_core::SubmitOrigin;
use agens_core::{
    EphemeralPromptMemory, HeadlessTurnCancellation, HeadlessTurnError, PermissionMode,
    TurnProgressSink,
};
use agens_tools::{
    CommandCatalog, SkillCatalog, TaskExecutionRegistry, TaskMessageSource, TaskMessageTarget,
};
use agens_tui::{
    BridgeCancel, Engine as TuiEngine, Tui, TuiProviderOutcome, TuiSubmissionOutcome,
    run_with_default_progress_submit_with_permissions_task_controls_and_ask_user,
};

use crate::ask_user_prompt::{TuiAskUserPort, production_tui_ask_user_bridge};
use crate::extensions::{start_tui_commands, start_tui_skills};
use crate::files::{expand_tui_prompt_with_media, tui_picker_file_candidates};
use crate::metrics::{TuiMetricsPublisher, finish_tui_metrics};
use crate::models::seed_remembered_tui_selection;
#[cfg(any(test, feature = "test-support"))]
use crate::permission_prompt::TtyPermissionPrompter;
use crate::permission_prompt::TuiPermissionPrompter;
use crate::permission_prompt::production_tui_permission_bridge;
use crate::repository::start_repository_probe;
use crate::resume::{ResumedTuiSession, resume_tui_session, resumed_subagent_cards};
use crate::router::{TuiRuntimeRouter, tui_provider_outcome};
use crate::turn::{complete_tui_turn, tui_session_presentation};
use agens_agents::{
    ensure_active_agent_runtime, persist_pending_agent_correction, reconcile_persisted_active_agent,
};
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
use agens_session::undo::{
    open_session_snapshots, prune_orphan_snapshots, record_turn, session_snapshot_root,
    turn_boundary,
};
use agens_store::{PromptMemoryStore, SessionStore};
use agens_tool_runtime::runner::{ProductionTaskRunner, TuiTaskControls, TuiTaskLifecycleBridge};
use agens_tool_runtime::runtime::task_execution_limits;
use agens_tool_runtime::task::{
    production_tui_task_runtime_with_cancellation,
    production_tui_task_runtime_with_runner_parent_config_and_cancellation,
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

fn interactive_turn_cancellation() -> HeadlessTurnCancellation {
    HeadlessTurnCancellation::new()
}

/// Opens SQLite prompt memory and installs it on the surface port.
///
/// When the store cannot open, history and stash still work for this session
/// through [`EphemeralPromptMemory`], and the degradation is said out loud as
/// a one-line failure notice instead of silently losing persistence.
///
/// A store that opened but had to give up on some rows' attachments reports that too: those
/// prompts load as text, and a restore that quietly comes back without its media would
/// otherwise look like the attachments were never recorded.
fn install_prompt_memory_from_store<E: TuiEngine>(
    tui: &mut Tui<E>,
    data_directory: &std::path::Path,
) {
    match PromptMemoryStore::open(data_directory) {
        Ok(store) => {
            let undecodable = store.undecodable_attachment_rows();
            tui.set_prompt_memory(Box::new(store));
            if undecodable > 0 {
                tui.apply_runtime_event(agens_tui::TuiRuntimeEvent::Notice {
                    text: format!(
                        "{undecodable} prompt(s) loaded without their attachments (unreadable record)."
                    ),
                    severity: agens_tui::NoticeSeverity::Failure,
                });
            }
        }
        Err(error) => {
            tui.set_prompt_memory(Box::new(EphemeralPromptMemory::new()));
            tui.apply_runtime_event(agens_tui::TuiRuntimeEvent::Notice {
                text: format!(
                    "Prompt history and stash will not persist beyond this session ({error})."
                ),
                severity: agens_tui::NoticeSeverity::Failure,
            });
        }
    }
}

/// Installs the trace recorder when `AGENS_PERF_TRACE` names a directory.
///
/// This is the only path that measures a real terminal. The scenario harness
/// drives a `TestBackend`, so everything crossterm does to paint an actual tty
/// — and everything the terminal emulator does with it — is invisible there.
#[cfg(feature = "perf-audit")]
fn session_perf_recorder() -> Option<agens_perf::Recorder> {
    let directory = std::env::var("AGENS_PERF_TRACE").ok()?;
    let run_id = std::env::var("AGENS_PERF_RUN").unwrap_or_else(|_| "session".to_owned());

    match agens_perf::Recorder::install(
        agens_perf::RecorderConfig::new(directory, run_id).with_scenario("live_session"),
    ) {
        Ok(recorder) => Some(recorder),
        Err(error) => {
            eprintln!("perf tracing is off: {error}");
            None
        }
    }
}

pub fn run_production_tui(bootstrap: &Bootstrap, resume: Option<i64>) -> Result<String, CliError> {
    run_production_tui_with_profile_store(bootstrap, resume, None)
}

pub fn run_production_tui_with_profile_store(
    bootstrap: &Bootstrap,
    resume: Option<i64>,
    profile_store: Option<Arc<dyn crate::profiles::AgentProfileStore>>,
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
    install_prompt_memory_from_store(&mut tui, bootstrap.data_directory());
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
        let staged_media = crate::files::session_staged_media(&resumed);
        let resume_error = resumed.resume_error.clone();
        resumed.resume_notice = None;
        tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
            message,
            presentation,
            history,
            draft,
            staged_media,
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
        let notice = seed_fresh_tui_context(bootstrap, &mut context)?;
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
    prune_orphan_snapshots(bootstrap);
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
    let router = profile_store.map_or(router.clone(), |store| router.with_profile_store(store));
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
    // Session-scoped, so a server name noticed once stays noticed across every
    // turn until it recovers to `Ready` — never re-seeded from a fresh discovery.
    let mcp_noticed: Arc<Mutex<std::collections::BTreeSet<String>>> =
        Arc::new(Mutex::new(std::collections::BTreeSet::new()));
    let (permission_bridge, permission_requests) = production_tui_permission_bridge();
    let (ask_user_bridge, ask_user_requests) = production_tui_ask_user_bridge();
    let transition_controls = task_controls.clone();
    let cancel_controls = task_controls.clone();
    let cancel_all_controls = task_controls.clone();
    let message_controls = task_controls.clone();
    let submit_task_controls = task_controls.clone();
    let prompt_bridge = permission_bridge.clone();
    let submit_ask_user_bridge = ask_user_bridge.clone();
    #[cfg(feature = "perf-audit")]
    let perf_recorder = session_perf_recorder();

    let tui_result = run_with_default_progress_submit_with_permissions_task_controls_and_ask_user(
        &mut tui,
        move |request, progress, cancellation| {
            route_router.route_request_with_cancellation(request, progress, cancellation)
        },
        move |prompt, origin, progress, metrics| {
            let task_events = metrics.clone();
            let turn_cancellation = interactive_turn_cancellation();
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
            let metrics = Arc::new(Mutex::new(
                TuiMetricsPublisher::new(metrics, BridgeCancel::new(), model_id)
                    .with_mcp_notices(router.mcp_status.clone(), Arc::clone(&mcp_noticed)),
            ));
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
            if let Ok(metrics) = metrics.lock() {
                metrics.publish_mcp_connecting_notices();
            }
            let discovery_cancellation = turn_cancellation.adapter_view().cancellation_handle();
            let selected_origin = origin_launches_selected_subagent(origin);
            let mut selected_runtime = if selected_origin {
                match production_tui_task_runtime_with_cancellation(
                    &runtime_bootstrap,
                    &session_project_root,
                    &skills,
                    Box::new(TuiPermissionPrompter(prompt_bridge.clone(), None)),
                    lifecycle_bridge.clone(),
                    task_parent_request_config.clone(),
                    task_diagnostic_reference.clone(),
                    session_bypass,
                    Box::new(TuiAskUserPort(submit_ask_user_bridge.clone(), None)),
                    Arc::clone(&discovery_cancellation),
                ) {
                    Ok(runtime) => Some(runtime),
                    Err(error) => return tui_provider_outcome(Err(error)),
                }
            } else {
                None
            };
            if let Some(runtime) = selected_runtime.as_ref()
                && let Err(error) = ensure_active_agent_runtime(
                    &runtime_bootstrap,
                    &router.session,
                    &runtime.dispatcher,
                )
            {
                return tui_provider_outcome(Err(error));
            }
            let selected_launch = if let Some(task_runtime) = selected_runtime.as_mut() {
                selected_tui_task_skips_parent(
                    launch_selected_tui_task(
                        task_runtime,
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
            let notice_metrics = Arc::clone(&metrics);
            let snapshot_notice = move |text: String| {
                if let Ok(metrics) = notice_metrics.lock() {
                    metrics.publish_failure_notice(text);
                }
            };
            let result = run_tui_prompt_with(
                &runtime_bootstrap,
                &prompt,
                &router.session,
                Some(Arc::clone(&skills)),
                Some(&snapshot_notice),
                |request| {
                    let task_runtime =
                        production_tui_task_runtime_with_runner_parent_config_and_cancellation(
                            &runtime_bootstrap,
                            &session_project_root,
                            &skills,
                            Box::new(TuiPermissionPrompter(prompt_bridge.clone(), None)),
                            ProductionTaskRunner::new(
                                runtime_bootstrap.clone(),
                                session_project_root.clone(),
                            )
                            .with_lifecycle_bridge(lifecycle_bridge.clone())
                            .with_dangerous_mode(request.dangerous_mode)
                            .with_bypass(request.dangerously_allow_all)
                            .with_permission_prompter({
                                let bridge = prompt_bridge.clone();
                                Arc::new(move |origin: agens_tool_runtime::runner::PromptOrigin| {
                                    Box::new(TuiPermissionPrompter(
                                        bridge.clone(),
                                        Some(agens_tui::PromptOrigin {
                                            execution: origin.execution,
                                            agent: origin.agent,
                                        }),
                                    ))
                                        as Box<dyn agens_permissions::PermissionPrompter>
                                })
                            })
                            .with_ask_user_port({
                                let bridge = submit_ask_user_bridge.clone();
                                Arc::new(move |origin: agens_tool_runtime::runner::PromptOrigin| {
                                    Box::new(TuiAskUserPort(
                                        bridge.clone(),
                                        Some(agens_tui::PromptOrigin {
                                            execution: origin.execution,
                                            agent: origin.agent,
                                        }),
                                    ))
                                        as Box<dyn agens_core::ask_user::AskUserPort>
                                })
                            }),
                            task_parent_request_config.clone(),
                            Some(task_diagnostic_reference.clone()),
                            Box::new(TuiAskUserPort(submit_ask_user_bridge.clone(), None)),
                            Arc::clone(&discovery_cancellation),
                        )?;
                    ensure_active_agent_runtime(
                        &runtime_bootstrap,
                        &router.session,
                        &task_runtime.dispatcher,
                    )?;
                    run_production_headless_chat_with_progress(
                        request,
                        &runtime_bootstrap,
                        &turn_cancellation,
                        Some(&sink),
                        Box::new(TuiPermissionPrompter(prompt_bridge.clone(), None)),
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
        move || {
            cancel_all_controls
                .0
                .cancel_all()
                .into_iter()
                .map(agens_tools::TaskExecutionId::value)
                .collect()
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
        Some((ask_user_bridge, ask_user_requests)),
    );
    task_controls.0.cancel_all();
    let _ = task_controls
        .0
        .wait_for_idle(std::time::Duration::from_secs(2));
    #[cfg(feature = "perf-audit")]
    if let Some(recorder) = perf_recorder {
        match recorder.finish() {
            Ok(paths) => eprintln!("perf trace: {}", paths.jsonl.display()),
            Err(error) => eprintln!("perf trace was not written: {error}"),
        }
    }

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
                | TuiSubmissionOutcome::MediaAttached { message, .. }
                | TuiSubmissionOutcome::SelectionInfo(message)
                | TuiSubmissionOutcome::ResetSucceeded { message, .. }
                | TuiSubmissionOutcome::ContextChanged { message, .. }
                | TuiSubmissionOutcome::SessionResumed { message, .. }
                | TuiSubmissionOutcome::HistoryRewritten { message, .. }
                | TuiSubmissionOutcome::LocalActionableError { message, .. } => Ok(message),
                TuiSubmissionOutcome::ProviderTurn { .. }
                | TuiSubmissionOutcome::BusyProviderTurn { .. }
                | TuiSubmissionOutcome::BusyRefusal(_)
                | TuiSubmissionOutcome::StagedMediaReplaced { .. }
                | TuiSubmissionOutcome::SecretEntry(_)
                | TuiSubmissionOutcome::Dialog(_)
                | TuiSubmissionOutcome::SafeDialog(_)
                | TuiSubmissionOutcome::TranscriptDialog
                | TuiSubmissionOutcome::PromptHistoryOverlay
                | TuiSubmissionOutcome::PromptStashOverlay
                | TuiSubmissionOutcome::SelectionCancelled
                | TuiSubmissionOutcome::RouteCancelled
                | TuiSubmissionOutcome::SelectionError { .. } => {
                    unreachable!("slash routing returns a local result or CLI error")
                }
                TuiSubmissionOutcome::Quit => Ok(String::new()),
            }
        }
        prompt => run_tui_prompt_with(bootstrap, prompt, session, None, None, |request| {
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
    notice: Option<&dyn Fn(String)>,
    run: impl FnOnce(HeadlessChatRequest) -> Result<HeadlessChatCompletion, HeadlessChatFailure>,
) -> Result<String, CliError> {
    let expanded = {
        let context = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        expand_tui_prompt_with_media(&context, bootstrap, prompt)?
    };
    let (request, snapshot_root, previous_messages, consumed_media_ids) = {
        let mut session = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        if session.running {
            return Err(CliError::runtime(HeadlessTurnError::State));
        }
        // Snapshot staged media into the request without clearing yet. Preflight and other
        // early failures must leave chips staged; clear only after success or after the
        // attempt has produced partial history (media then lives on the session/retry path).
        let consumed_media_ids = session.pending_media_ids.clone();
        let mut media_ids = session.pending_media_ids.clone();
        let mut media_mimes = session.pending_media_mimes.clone();
        media_ids.extend(expanded.media_ids);
        media_mimes.extend(expanded.media_mimes);
        let mut request = agens_headless::apply_session_to_request(
            &session,
            HeadlessChatRequest {
                prompt: expanded.text,
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
                media_ids,
                media_mimes,
            },
        );
        if let Some(skills) = skills {
            let base = match request.system_prompt.take() {
                Some(explicit) => explicit,
                None => agens_core::prompt::base_system_prompt(
                    tui_turn_system_prompt(&session, bootstrap)?.as_deref(),
                ),
            };
            request.system_prompt = Some(parent_skill_system_prompt(&base, &skills));
        }
        seed_configured_reasoning_effort(&mut request, bootstrap);
        let snapshot_root = session_snapshot_root(bootstrap, &session);

        // Sending a prompt is the reader choosing the direction they undid
        // their way back to, so the turns they took back stop being recoverable
        // here — in the store as well as in memory — rather than lingering into
        // a history they did not ask for.
        commit_tui_undo(bootstrap, &mut session)?;

        // The session is claimed only once nothing above can still fail: a turn
        // that never starts must not leave the session marked running.
        session.running = true;

        (
            request,
            snapshot_root,
            session.messages.clone(),
            consumed_media_ids,
        )
    };

    // Bracketing the turn is what makes it undoable at all; a project that
    // cannot be snapshotted simply records no step. Opening the repository and
    // capturing both spawn git, so neither runs while the session is locked.
    // A failure never breaks the turn, but it is not silent either: the first
    // reason is said out loud once below and remembered on the session, so a
    // later `/undo` can explain why the turn went unrecorded.
    let mut snapshot_failure: Option<String> = None;
    let snapshots = match snapshot_root
        .as_deref()
        .map(|root| open_session_snapshots(bootstrap, root))
    {
        Some(Ok(snapshots)) => snapshots,
        Some(Err(error)) => {
            snapshot_failure = Some(error.to_string());
            None
        }
        None => None,
    };
    let before = snapshots.as_ref().and_then(|repository| {
        repository
            .capture()
            .map_err(|error| {
                snapshot_failure.get_or_insert(error.to_string());
            })
            .ok()
    });

    let consumed_reminder = request.pending_system_reminder.is_some();
    let completion = run(request);
    let after = snapshots.as_ref().and_then(|repository| {
        repository
            .capture()
            .map_err(|error| {
                snapshot_failure.get_or_insert(error.to_string());
            })
            .ok()
    });

    if let (Some(reason), Some(notice)) = (&snapshot_failure, notice) {
        notice(format!(
            "Snapshots are unavailable, so this turn cannot be undone ({reason})."
        ));
    }

    let mut session = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    session.running = false;
    let clear_pending_media = match &completion {
        Ok(_) => true,
        Err(failure) => failure.partial.is_some(),
        // Preflight / begin failures keep staged media so chips remain for retry.
    };
    if clear_pending_media {
        drop_consumed_pending_media(&mut session, &consumed_media_ids);
    }
    let result = complete_tui_turn(&mut session, completion, consumed_reminder);
    match snapshot_failure {
        Some(reason) => session.snapshot_degraded = Some(reason),
        None if before.is_some() && after.is_some() => session.snapshot_degraded = None,
        None => {}
    }
    let boundary = turn_boundary(&previous_messages, &session.messages);
    record_turn(&mut session, prompt, boundary, before, after);
    if let Some(identifier) = session.identifier {
        if let Some(message) =
            failed_bypass_persist_notice(write_through_bypass_permission_prompts(
                bootstrap,
                identifier,
                session.bypass_permissions,
            ))
        {
            if let Some(notice) = notice {
                notice(message);
            }
        }
    }
    result
}

fn failed_bypass_persist_notice(result: Result<(), CliError>) -> Option<String> {
    result.err().map(|_| {
        "Permission bypass state could not be saved and may not persist across resume.".to_owned()
    })
}

/// Removes exactly the attachments the finished turn carried from the session staging.
///
/// Anything staged after the turn started — a stash pop, a history restore, a clipboard attach
/// while the model was answering — belongs to the next prompt, so a blanket clear here would
/// destroy media the finished turn never sent and whose stash row is already gone. Matching is
/// positional per id so a set that stages the same media twice loses only what it consumed.
fn drop_consumed_pending_media(session: &mut SessionContext, consumed: &[i64]) {
    for media_id in consumed {
        let Some(index) = session
            .pending_media_ids
            .iter()
            .position(|staged| staged == media_id)
        else {
            continue;
        };

        session.pending_media_ids.remove(index);
        if index < session.pending_media_mimes.len() {
            session.pending_media_mimes.remove(index);
        }
    }
}

/// Drops the turns an undo held back, from the session in hand and from what it persisted.
///
/// The store is opened only when an undo is actually waiting, so a prompt on a session that undid
/// nothing cannot fail on a store it never needed. Unlike the bypass write-through below, a
/// failure here is fatal to the submission: the reader's next prompt would otherwise be answered
/// with the turn they took back still in the history the model is given.
fn commit_tui_undo(bootstrap: &Bootstrap, session: &mut SessionContext) -> Result<(), CliError> {
    if !session.undo.has_undone_turns() {
        return Ok(());
    }

    let mut store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("undone turns could not be dropped from the session"))?;
    session
        .commit_undo(Some(&mut store))
        .map_err(|_| CliError::storage("undone turns could not be dropped from the session"))
}

pub(crate) fn seed_fresh_tui_context(
    bootstrap: &Bootstrap,
    context: &mut SessionContext,
) -> Result<Option<String>, CliError> {
    reconcile_persisted_active_agent(bootstrap, context)?;
    let notice = context
        .selection
        .is_none()
        .then(|| seed_remembered_tui_selection(bootstrap, context))
        .flatten();
    seed_bypass_permissions_from_configuration(bootstrap, context)?;
    Ok(notice)
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

/// Reports skills whose name is also a command as a single line.
///
/// Neither side is lost to the collision — only `/name` is claimed by the command, while the skill
/// stays in the catalog and in the model's skill listing — so this is one informational line rather
/// than a per-name warning that reads as if the skills had been disabled.
pub fn report_tui_extension_collisions<E: TuiEngine>(
    tui: &mut Tui<E>,
    commands: &CommandCatalog,
    skills: &SkillCatalog,
) {
    let mut names = skills
        .skills()
        .map(|skill| skill.name())
        .filter(|name| commands.command(name).is_some())
        .collect::<Vec<_>>();
    if names.is_empty() {
        return;
    }

    names.sort_unstable();
    tui.add_diagnostic(format!(
        "{} a command name; /name runs the command, the skill tool still loads them: {}.",
        counted_skills_sharing(names.len()),
        names.join(", ")
    ));
}

fn counted_skills_sharing(count: usize) -> String {
    if count == 1 {
        "1 skill shares".to_owned()
    } else {
        format!("{count} skills share")
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
        tui.set_repository_probe(start_repository_probe(root.path()));
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
        bootstrap_from_configuration, persist_tui_session, rotation_agent, rotation_dispatcher,
        tui_project, tui_session_bootstrap, tui_session_bootstrap_with_global_bypass,
        tui_session_directory, tui_session_messages,
    };
    use agens_fixtures::BundledModelValidator;
    use agens_models::ModelSelection;
    use agens_session::context::ActiveAgentRuntime;

    fn bare_headless_request() -> HeadlessChatRequest {
        HeadlessChatRequest {
            prompt: "test".into(),
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
            skills: None,
            media_ids: Vec::new(),
            media_mimes: Vec::new(),
        }
    }

    #[test]
    fn fresh_session_uses_the_configured_primary_profile_model() {
        let label = "fresh-configured-primary-model";
        let temporary = std::env::temp_dir().join(format!("agens-{label}-{}", std::process::id()));
        std::fs::create_dir_all(temporary.join("project/.git")).unwrap();
        let bootstrap = bootstrap_from_configuration(
            label,
            Some(
                "[provider]\ntype = \"openai-chatgpt\"\n\
                 [agent]\ndefault_agent = \"primary\"\n\
                 [agents.primary]\nmodel = \"gpt-5.6-sol\"\neffort = \"high\"\n",
            ),
            None,
        );
        let mut context = SessionContext::fresh();

        seed_fresh_tui_context(&bootstrap, &mut context).unwrap();

        let selection = context
            .selection
            .as_ref()
            .expect("profile should select a model");
        assert_eq!(selection.model(), "gpt-5.6-sol");
        assert_eq!(selection.reasoning_effort(), Some("high"));
        assert_eq!(
            tui_session_presentation(&bootstrap, &context).model(),
            "gpt-5.6-sol"
        );
        let request = agens_headless::apply_session_to_request(&context, bare_headless_request());
        assert_eq!(request.model.as_deref(), Some("openai-chatgpt/gpt-5.6-sol"));
        assert_eq!(
            request.request_config.reasoning_effort(),
            Some(agens_core::ReasoningEffort::High)
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn new_command_reapplies_the_configured_primary_profile_model() {
        let label = "new-command-configured-primary-model";
        let temporary = std::env::temp_dir().join(format!("agens-{label}-{}", std::process::id()));
        std::fs::create_dir_all(temporary.join("project/.git")).unwrap();
        let bootstrap = bootstrap_from_configuration(
            label,
            Some(
                "[provider]\ntype = \"openai-chatgpt\"\n\
                 [agent]\ndefault_agent = \"primary\"\n\
                 [agents.primary]\nmodel = \"gpt-5.6-sol\"\neffort = \"high\"\n",
            ),
            None,
        );
        let session = Arc::new(Mutex::new(SessionContext {
            selection: Some(ModelSelection::new("gpt-5.5")),
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
        let context = session.lock().unwrap();
        let selection = context
            .selection
            .as_ref()
            .expect("profile should select a model after reset");
        assert_eq!(selection.model(), "gpt-5.6-sol");
        assert_eq!(selection.reasoning_effort(), Some("high"));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn preflight_failure_keeps_pending_media_staged() {
        let temporary = tui_session_directory("pending-media-preflight");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let _store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        {
            let mut locked = session.lock().unwrap();
            locked.push_pending_media(42, "image/png".into());
        }

        let result = run_tui_prompt_with(&bootstrap, "describe", &session, None, None, |_| {
            Err(HeadlessChatFailure::from(CliError::configuration(
                "model gpt-4.1-nano does not accept attachment mime application/pdf",
            )))
        });
        assert!(result.is_err(), "preflight-style failure must surface");
        let locked = session.lock().unwrap();
        assert_eq!(
            locked.pending_media_ids,
            vec![42],
            "staged media must remain after early failure"
        );
        assert_eq!(locked.pending_media_mimes, vec!["image/png".to_owned()]);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// Media staged while the model was answering belongs to the next prompt. A stash pop is
    /// the destructive case: its durable row is already deleted, so clearing it on completion
    /// would leave nothing anywhere.
    #[test]
    fn a_completed_turn_clears_only_the_media_it_carried() {
        let temporary = tui_session_directory("pending-media-mid-turn");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let project = tui_project(&temporary);
        drop(store);
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        {
            let mut locked = session.lock().unwrap();
            locked.push_pending_media(42, "image/png".into());
        }

        let staged_mid_turn = Arc::clone(&session);
        let result = run_tui_prompt_with(&bootstrap, "describe", &session, None, None, move |_| {
            let mut locked = staged_mid_turn.lock().unwrap();
            locked.pending_media_ids = vec![77];
            locked.pending_media_mimes = vec!["application/pdf".to_owned()];
            Ok(HeadlessChatCompletion {
                text: "answered".into(),
                metadata: SessionMetadata {
                    id: 1,
                    project,
                    title: "conversation".into(),
                    active_agent: "primary".into(),
                    provider_id: None,
                    model_id: None,
                    reasoning_effort: None,
                    created_at: 1,
                    updated_at: 1,
                    completed_turn_count: 1,
                    resumable: true,
                    parent_session_id: None,
                    fork_message_count: None,
                },
                messages: Vec::new(),
            })
        });
        assert!(result.is_ok(), "turn must complete: {result:?}");

        let locked = session.lock().unwrap();
        assert_eq!(
            locked.pending_media_ids,
            vec![77],
            "media staged mid-turn must survive completion"
        );
        assert_eq!(
            locked.pending_media_mimes,
            vec!["application/pdf".to_owned()]
        );

        drop(locked);
        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// A snapshot failure must not break the turn, but it must not be silent
    /// either: the reader is warned once, and a later `/undo` explains that
    /// the turn went unrecorded instead of claiming there is nothing to undo.
    #[test]
    fn a_failed_snapshot_capture_warns_and_does_not_abort_the_turn() {
        fn git(directory: &std::path::Path, arguments: &[&str]) {
            let status = std::process::Command::new("git")
                .args(arguments)
                .current_dir(directory)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git runs");
            assert!(status.success(), "git {arguments:?} failed");
        }

        let temporary = tui_session_directory("snapshot-capture-failure");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = temporary.join("project");

        // The project's only tracked file grew past the snapshot size cap, so
        // staging it empties the capture's index while the project still
        // tracks a file — the capture failure that can be arranged from
        // outside the snapshot crate, and one it refuses rather than
        // describing the project as empty.
        std::fs::remove_dir_all(project.join(".git")).unwrap();
        git(&project, &["init", "--quiet"]);
        git(&project, &["config", "user.name", "test"]);
        git(&project, &["config", "user.email", "test@localhost"]);
        std::fs::write(project.join("big.txt"), "small at first\n").unwrap();
        git(&project, &["add", "."]);
        git(&project, &["commit", "--quiet", "-m", "initial"]);
        std::fs::write(project.join("big.txt"), "x".repeat(3 * 1024 * 1024)).unwrap();

        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let notices = std::cell::RefCell::new(Vec::new());
        let notice = |text: String| notices.borrow_mut().push(text);

        let result =
            run_tui_prompt_with(&bootstrap, "prompt", &session, None, Some(&notice), |_| {
                Ok(HeadlessChatCompletion {
                    text: "answered".into(),
                    metadata: SessionMetadata {
                        id: 1,
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
                        parent_session_id: None,
                        fork_message_count: None,
                    },
                    messages: Vec::new(),
                })
            });

        assert!(
            result.is_ok(),
            "a snapshot failure must not break the turn: {result:?}"
        );
        let notices = notices.into_inner();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].contains("cannot be undone"),
            "the warning names the consequence: {notices:?}"
        );
        assert!(
            session.lock().unwrap().snapshot_degraded.is_some(),
            "the failure is remembered for /undo"
        );

        let answer = run_tui_prompt(
            &bootstrap,
            "/undo",
            &HeadlessTurnCancellation::new(),
            &session,
            None,
        )
        .unwrap();
        assert!(
            answer.starts_with("Snapshots were unavailable this session"),
            "{answer}"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// A prompt-memory store that cannot open must not silently disable
    /// history and stash: the session falls back to ephemeral memory and the
    /// degradation is announced as a visible one-line failure notice.
    #[test]
    fn prompt_memory_store_failure_falls_back_to_ephemeral_with_a_failure_notice() {
        let temporary = tui_session_directory("prompt-memory-fallback");
        let blocked = temporary.join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();

        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        install_prompt_memory_from_store(&mut tui, &blocked);

        assert!(
            tui.runtime_events().iter().any(|event| matches!(
                event,
                agens_tui::TuiRuntimeEvent::Notice {
                    text,
                    severity: agens_tui::NoticeSeverity::Failure,
                } if text.contains("will not persist beyond this session")
            )),
            "the degradation must be said out loud"
        );

        // History and stash still work for this session through the fallback.
        for character in "parked".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        tui.handle(Event::Key(Key::CtrlS));
        assert_eq!(tui.input(), "");
        tui.handle(Event::Key(Key::CtrlS));
        assert_eq!(tui.input(), "parked");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn interactive_turns_have_no_automatic_deadline() {
        let cancellation = interactive_turn_cancellation();

        assert!(cancellation.adapter_view().deadline().is_none());
        assert!(!cancellation.is_expired());
    }

    #[test]
    fn second_control_c_uses_the_owned_turn_cancellation_before_quit() {
        let cancellation = HeadlessTurnCancellation::new();
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(Some(cancellation.clone()))),
        });
        tui.begin_submission("active");

        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
        assert!(cancellation.is_cancelled());
        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
        assert!(tui.view().running);
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

        // Resume while a turn is running must refuse without mutating the session.
        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/resume 1",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap_err()
            .to_string(),
            "runtime: headless turn entered an invalid state"
        );
        assert_eq!(*session.lock().unwrap(), original);

        // Local subagent selection is not a turn start; it must leave the busy
        // context untouched even when it returns Ok.
        let _ = run_tui_prompt(
            &bootstrap,
            "/subagent reviewer",
            &HeadlessTurnCancellation::new(),
            &session,
            None,
        );
        assert_eq!(*session.lock().unwrap(), original);

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
        let result = run_tui_prompt_with(&bootstrap, "next request", &session, None, None, |_| {
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
    fn a_failed_bypass_persist_after_a_turn_is_named_rather_than_swallowed() {
        assert_eq!(
            failed_bypass_persist_notice(Err(CliError::storage(
                "permission bypass state could not be saved"
            ))),
            Some(
                "Permission bypass state could not be saved and may not persist across resume."
                    .to_owned()
            )
        );
        assert_eq!(failed_bypass_persist_notice(Ok(())), None);
    }

    /// The sequence the feature is judged by: a turn is taken back, the reader sends the next
    /// prompt, that turn completes — and the turn they took back is gone from what the model was
    /// given, from the history in hand, and from the store the next turn reloads from.
    #[test]
    fn a_turn_taken_back_stays_gone_once_the_next_turn_completes() {
        use agens_core::{CompletedSessionTurn, Message, Role, SessionMessage};

        fn completed(prompt: &str, answer: &str) -> CompletedSessionTurn {
            CompletedSessionTurn::new(
                [
                    Message {
                        role: Role::User,
                        parts: vec![MessagePart::Text(prompt.into())],
                    },
                    Message {
                        role: Role::Assistant,
                        parts: vec![MessagePart::Text(answer.into())],
                    },
                ]
                .into_iter()
                .map(SessionMessage::try_from)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            )
            .unwrap()
        }

        let temporary = tui_session_directory("undo-commit");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = tui_project(&temporary);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let mut metadata = SessionMetadata {
            id: 0,
            project: project.clone(),
            title: "conversation".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
            parent_session_id: None,
            fork_message_count: None,
        };
        metadata = store
            .persist_completed_session_turn(&metadata, &completed("first", "kept"))
            .unwrap();
        metadata = store
            .persist_completed_session_turn(&metadata, &completed("second", "taken back"))
            .unwrap();
        let stored = store.load_session_for_resume(metadata.id).unwrap();
        drop(store);

        let mut context = SessionContext::restored(
            metadata.id,
            stored.metadata,
            stored.messages,
            std::path::PathBuf::from(&project),
        );
        context.undo.record(agens_session::undo::UndoStep::new(
            "second".into(),
            2,
            "before".into(),
            "after".into(),
        ));
        context.undo.undo().expect("a turn to take back");
        let session = Arc::new(Mutex::new(context));

        let identifier = metadata.id;
        let result = run_tui_prompt_with(
            &bootstrap,
            "a new direction",
            &session,
            None,
            None,
            |request| {
                assert_eq!(
                    request.history.len(),
                    2,
                    "the model is asked to continue from the history the reader undid back to"
                );

                let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
                let metadata = store
                    .persist_completed_session_turn(
                        &store.load_session_for_resume(identifier).unwrap().metadata,
                        &completed("a new direction", "answered"),
                    )
                    .unwrap();
                let stored = store.load_session_for_resume(metadata.id).unwrap();

                Ok(HeadlessChatCompletion {
                    text: "answered".into(),
                    metadata: stored.metadata,
                    messages: stored.messages,
                })
            },
        );
        assert!(result.is_ok(), "{result:?}");

        let context = session.lock().unwrap();
        let texts = context
            .messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                MessagePart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec!["first", "kept", "a new direction", "answered"],
            "the taken-back turn does not come back with the next completed turn"
        );

        let store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let reloaded = store.load_session_for_resume(identifier).unwrap();
        assert_eq!(
            reloaded.messages, context.messages,
            "the store holds exactly the history the session does"
        );
        assert_eq!(reloaded.metadata.completed_turn_count, 2);

        drop(context);
        drop(store);
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
                parent_session_id: None,
                fork_message_count: None,
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
    fn an_unconfigured_skills_turn_still_starts_from_the_built_in_base_prompt() {
        let temporary = tui_session_directory("skills-base-prompt");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let skills = Some(Arc::new(SkillCatalog::default()));
        let captured_system_prompt = std::cell::RefCell::new(None);

        let result = run_tui_prompt_with(&bootstrap, "prompt", &session, skills, None, |request| {
            *captured_system_prompt.borrow_mut() = request.system_prompt.clone();
            Ok(HeadlessChatCompletion {
                text: "captured".into(),
                metadata: SessionMetadata {
                    id: 1,
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
                    parent_session_id: None,
                    fork_message_count: None,
                },
                messages: Vec::new(),
            })
        });

        assert!(result.is_ok());
        assert_eq!(
            captured_system_prompt.into_inner().as_deref(),
            Some(agens_core::prompt::BASE_SYSTEM_PROMPT),
            "an unconfigured skills-branch turn must send the built-in base prompt verbatim"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }
}
