//! TUI task lifecycle bridging and the production `TaskRunner` implementation
//! that drives an isolated subagent turn to completion, reporting progress
//! and terminal results back to the TUI.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[cfg(test)]
use agens_core::HeadlessTurnError;
use agens_core::{HeadlessTurnCancellation, MessagePart, TurnEvent, TurnProgressSink};
use agens_core::{SubagentErrorKind, SubagentStatus};
use agens_tools::{
    TaskExecutionEvent, TaskExecutionLifecycle, TaskExecutionRegistry, TaskLaunchMode,
    TaskRunContext, TaskRunner, TaskRunnerError, TaskTurnRequest, TaskTurnResult,
};
use agens_tui::{BridgeCancel, BridgeTx, TuiExecutionEvent, TuiRuntimeEvent, TuiSubagentEvent};

use crate::Bootstrap;
use crate::diagnostics::{next_diagnostic_reference, record_subagent_terminal};
use crate::permissions::ParseToolInput;
use crate::session::context::SessionContext;
use crate::session::context::{CompletedSubagentTurn, current_session_timestamp};
use crate::tools::child::{ChildRunError, ProductionTaskExecutionContext, run_production_task};
use crate::tui::metrics::sanitize_tui_metric;
use crate::turns::persist_completed_subagent_turn;

#[cfg(test)]
type ProductionTaskProbe = Arc<
    Mutex<
        Vec<(
            agens_tools::TaskExecutionId,
            TaskLaunchMode,
            String,
            Option<agens_core::ReasoningEffort>,
        )>,
    >,
>;

#[cfg(test)]
struct TestTaskFailure {
    error: ChildRunError,
    _source_detail: String,
}

#[derive(Clone, Default)]
pub(crate) struct TuiTaskControls(pub(crate) TaskExecutionRegistry);

impl TuiTaskControls {
    pub(crate) fn transition_to_background(&self, id: u64) -> bool {
        self.0
            .transition_to_background(agens_tools::TaskExecutionId::from_value(id))
    }
}
#[derive(Clone)]
pub(crate) struct TuiTaskLifecycleBridge {
    events: BridgeTx<TuiRuntimeEvent>,
    controls: TuiTaskControls,
    lifecycle: Arc<Mutex<Option<TaskExecutionLifecycle>>>,
    terminal_results: Arc<Mutex<BTreeMap<u64, String>>>,
    completed_turns: Arc<Mutex<BTreeMap<u64, CompletedSubagentTurn>>>,
    pub(crate) persist_completed: Option<Arc<dyn Fn(CompletedSubagentTurn) -> bool + Send + Sync>>,
}

impl TuiTaskLifecycleBridge {
    pub(crate) fn new(events: BridgeTx<TuiRuntimeEvent>, controls: TuiTaskControls) -> Self {
        Self {
            events,
            controls,
            lifecycle: Arc::new(Mutex::new(None)),
            terminal_results: Arc::new(Mutex::new(BTreeMap::new())),
            completed_turns: Arc::new(Mutex::new(BTreeMap::new())),
            persist_completed: None,
        }
    }

    pub(crate) fn with_session_writer(
        mut self,
        bootstrap: Bootstrap,
        session: Arc<Mutex<SessionContext>>,
    ) -> Self {
        let events = self.events.clone();
        self.persist_completed = Some(Arc::new(move |turn: CompletedSubagentTurn| {
            let id = turn.id;
            if persist_completed_subagent_turn(&bootstrap, &session, turn).is_err() {
                let _ = events.publish(
                    TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::error(
                        id,
                        SubagentErrorKind::Runtime,
                    )),
                    &BridgeCancel::new(),
                    None,
                );
                return false;
            }
            true
        }));
        self
    }

    pub(crate) fn mode(&self) -> Option<TaskLaunchMode> {
        let lifecycle = self.lifecycle.lock().ok()?;
        Some(lifecycle.as_ref()?.mode())
    }

    fn observe(&self, request: &TaskTurnRequest, lifecycle: TaskExecutionLifecycle) {
        let id = lifecycle.id().value();
        if let Ok(mut current) = self.lifecycle.lock() {
            *current = Some(lifecycle.clone());
        }
        let presentation = match lifecycle.mode() {
            TaskLaunchMode::Foreground => agens_tui::TuiExecutionState::ForegroundRunning,
            TaskLaunchMode::Background => agens_tui::TuiExecutionState::BackgroundRunning,
        };
        let agent = request.agent_name().to_owned();
        if let Ok(mut turns) = self.completed_turns.lock() {
            turns.insert(
                id,
                CompletedSubagentTurn {
                    id,
                    agent: agent.clone(),
                    task: request.description().to_owned(),
                    final_result: String::new(),
                    tool_uses: 0,
                },
            );
        }
        self.publish(TuiRuntimeEvent::TaskExecution {
            agent: agent.clone(),
            event: match lifecycle.mode() {
                TaskLaunchMode::Foreground => TuiExecutionEvent::ForegroundStarted { id },
                TaskLaunchMode::Background => TuiExecutionEvent::BackgroundStarted { id },
            },
        });
        self.publish(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::started(id, &agent, request.description(), presentation),
        ));
        let events = self.events.clone();
        let registry = self.controls.0.clone();
        let terminal_results = Arc::clone(&self.terminal_results);
        let completed_turns = Arc::clone(&self.completed_turns);
        let persist_completed = self.persist_completed.clone();
        std::thread::spawn(move || {
            let mut seen = 1;
            loop {
                let lifecycle_events = lifecycle.events();
                for event in &lifecycle_events[seen..] {
                    let event = match *event {
                        TaskExecutionEvent::Admitted(_, TaskLaunchMode::Foreground) => {
                            TuiExecutionEvent::ForegroundStarted { id }
                        }
                        TaskExecutionEvent::Admitted(_, TaskLaunchMode::Background) => {
                            TuiExecutionEvent::BackgroundStarted { id }
                        }
                        TaskExecutionEvent::Backgrounded(_) => {
                            TuiExecutionEvent::Backgrounded { id }
                        }
                        TaskExecutionEvent::Completed(_) => TuiExecutionEvent::Completed { id },
                        TaskExecutionEvent::Failed(_) => TuiExecutionEvent::Failed { id },
                        TaskExecutionEvent::Cancelled(_) => TuiExecutionEvent::Cancelled { id },
                    };
                    let _ = events.publish(
                        TuiRuntimeEvent::TaskExecution {
                            agent: agent.clone(),
                            event,
                        },
                        &BridgeCancel::new(),
                        None,
                    );
                    if matches!(
                        event,
                        TuiExecutionEvent::Completed { .. }
                            | TuiExecutionEvent::Failed { .. }
                            | TuiExecutionEvent::Cancelled { .. }
                    ) {
                        let (status, fallback) = match event {
                            TuiExecutionEvent::Completed { .. } => {
                                (SubagentStatus::Success, "completed")
                            }
                            TuiExecutionEvent::Failed { .. } => (SubagentStatus::Failure, "failed"),
                            TuiExecutionEvent::Cancelled { .. } => {
                                (SubagentStatus::Cancelled, "cancelled")
                            }
                            _ => unreachable!("terminal event was matched above"),
                        };
                        let final_result = terminal_results
                            .lock()
                            .ok()
                            .and_then(|mut results| results.remove(&id))
                            .unwrap_or_else(|| fallback.into());
                        let _ = events.publish(
                            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::terminal(
                                id,
                                status,
                                final_result,
                            )),
                            &BridgeCancel::new(),
                            None,
                        );
                        let completed_turn = completed_turns
                            .lock()
                            .ok()
                            .and_then(|mut turns| turns.remove(&id));
                        let persisted = matches!(event, TuiExecutionEvent::Completed { .. })
                            && match (completed_turn, &persist_completed) {
                                (Some(turn), Some(persist)) => persist(turn),
                                _ => false,
                            };
                        notify_main_of_terminal_subagent(
                            &registry, &events, id, &agent, fallback, persisted,
                        );
                        return;
                    }
                }
                seen = lifecycle_events.len();
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
    }

    fn observe_progress(&self, id: u64, event: TurnEvent) {
        if matches!(event, TurnEvent::ToolCallRequested { .. })
            && let Ok(mut turns) = self.completed_turns.lock()
            && let Some(turn) = turns.get_mut(&id)
        {
            turn.tool_uses += 1;
        }
        let event = match event {
            TurnEvent::ProviderPart(MessagePart::Reasoning(delta)) => {
                TuiSubagentEvent::reasoning(id, delta)
            }
            TurnEvent::ProviderPart(MessagePart::Text(delta)) => TuiSubagentEvent::text(id, delta),
            TurnEvent::ToolCallRequested {
                id: call_id,
                name,
                input,
            } => {
                // Parse the sanitized input so a redacted secret never
                // survives inside `parsed`'s `Other { raw, .. }` fallback.
                let sanitized_input = sanitize_tui_metric(&input);
                let parsed = agens_core::ToolInput::parse(&name, &sanitized_input);
                TuiSubagentEvent::tool_call_with_parsed(id, call_id, name, input, parsed)
            }
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id,
                content,
                is_error,
            }) => TuiSubagentEvent::tool_result(id, tool_call_id, content, is_error),
            _ => return,
        };
        self.publish(TuiRuntimeEvent::SubagentExecution(event));
    }

    fn record_terminal_result(&self, id: u64, result: Result<&str, &TaskRunnerError>) {
        let final_result = match result {
            Ok(result) => result.into(),
            Err(TaskRunnerError::Cancelled) => "cancelled".into(),
            Err(_) => "failed".into(),
        };
        if let Ok(mut results) = self.terminal_results.lock() {
            results.insert(id, final_result);
        }
        if let Ok(result) = result
            && let Ok(mut turns) = self.completed_turns.lock()
            && let Some(turn) = turns.get_mut(&id)
        {
            turn.final_result = result.into();
        }
    }

    fn publish(&self, event: TuiRuntimeEvent) {
        let _ = self.events.publish(event, &BridgeCancel::new(), None);
    }
}

/// A finished background subagent is otherwise only shown to the user, never told to the model.
/// The notice stays a pointer rather than the payload: the persisted turn remains the source of
/// truth, and the bounded untrusted mailbox delivers this on the next main turn without starting
/// one.
fn notify_main_of_terminal_subagent(
    registry: &TaskExecutionRegistry,
    events: &BridgeTx<TuiRuntimeEvent>,
    id: u64,
    agent: &str,
    state: &str,
    persisted: bool,
) {
    let record = if persisted {
        "The full result is recorded in this session history"
    } else {
        "No durable result was recorded"
    };
    let notice = format!(
        "subagent #{id} ({agent}) finished with state={state} completed_at={} (unix seconds). \
         {record}; run task_control action=status id={id} for the recorded outcome.",
        current_session_timestamp()
    );

    if registry
        .notify_main(agens_tools::TaskExecutionId::from_value(id), notice)
        .is_err()
    {
        let _ = events.publish(
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::error(
                id,
                SubagentErrorKind::Runtime,
            )),
            &BridgeCancel::new(),
            None,
        );
    }
}

pub(crate) struct ProductionTaskRunner {
    bootstrap: Bootstrap,
    project_root: PathBuf,
    dangerous_mode: bool,
    lifecycle_bridge: Option<TuiTaskLifecycleBridge>,
    task_registry: Option<TaskExecutionRegistry>,
    #[cfg(test)]
    probe: Option<ProductionTaskProbe>,
    #[cfg(test)]
    progress_probe: Option<Vec<TurnEvent>>,
    #[cfg(test)]
    failure_probe: Option<TestTaskFailure>,
}

impl ProductionTaskRunner {
    pub(crate) fn new(bootstrap: Bootstrap, project_root: PathBuf) -> Self {
        Self {
            bootstrap,
            project_root,
            dangerous_mode: false,
            lifecycle_bridge: None,
            task_registry: None,
            #[cfg(test)]
            probe: None,
            #[cfg(test)]
            progress_probe: None,
            #[cfg(test)]
            failure_probe: None,
        }
    }

    pub(crate) fn with_lifecycle_bridge(
        mut self,
        lifecycle_bridge: TuiTaskLifecycleBridge,
    ) -> Self {
        self.task_registry = Some(lifecycle_bridge.controls.0.clone());
        self.lifecycle_bridge = Some(lifecycle_bridge);
        self
    }

    pub(crate) fn with_dangerous_mode(mut self, dangerous_mode: bool) -> Self {
        self.dangerous_mode = dangerous_mode;
        self
    }
    #[cfg(test)]
    pub(crate) fn with_probe(
        bootstrap: Bootstrap,
        project_root: PathBuf,
        probe: ProductionTaskProbe,
    ) -> Self {
        Self {
            bootstrap,
            project_root,
            dangerous_mode: false,
            lifecycle_bridge: None,
            task_registry: None,
            probe: Some(probe),
            progress_probe: None,
            failure_probe: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_progress_probe(
        bootstrap: Bootstrap,
        project_root: PathBuf,
        probe: ProductionTaskProbe,
        progress: Vec<TurnEvent>,
    ) -> Self {
        Self {
            bootstrap,
            project_root,
            dangerous_mode: false,
            lifecycle_bridge: None,
            task_registry: None,
            probe: Some(probe),
            progress_probe: Some(progress),
            failure_probe: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_failure_probe(
        bootstrap: Bootstrap,
        project_root: PathBuf,
        error: ChildRunError,
        source_detail: &str,
    ) -> Self {
        Self {
            bootstrap,
            project_root,
            dangerous_mode: false,
            lifecycle_bridge: None,
            task_registry: None,
            probe: None,
            progress_probe: None,
            failure_probe: Some(TestTaskFailure {
                error,
                _source_detail: source_detail.into(),
            }),
        }
    }
}

impl TaskRunner for ProductionTaskRunner {
    fn execution_registry(&self) -> Option<TaskExecutionRegistry> {
        self.task_registry.clone()
    }

    fn run(
        &self,
        request: TaskTurnRequest,
        context: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        if let (Some(lifecycle_bridge), Some(execution)) =
            (&self.lifecycle_bridge, context.execution())
        {
            lifecycle_bridge.observe(&request, execution.clone());
        }
        #[cfg(test)]
        if let Some(probe) = &self.probe {
            let execution = context
                .execution()
                .expect("registered task has execution context");
            probe.lock().expect("task probe lock").push((
                execution.id(),
                execution.mode(),
                request.model().to_owned(),
                request.request_config().reasoning_effort(),
            ));
            if let (Some(lifecycle_bridge), Some(execution), Some(progress)) = (
                &self.lifecycle_bridge,
                context.execution(),
                &self.progress_probe,
            ) {
                for event in progress.iter().cloned() {
                    lifecycle_bridge.observe_progress(execution.id().value(), event);
                }
            }
            if self.progress_probe.is_some() {
                let result = TaskTurnResult {
                    output: "probe".into(),
                    iterations: 1,
                };
                if let Some(lifecycle_bridge) = &self.lifecycle_bridge {
                    lifecycle_bridge
                        .record_terminal_result(execution.id().value(), Ok(&result.output));
                }
                return Ok(result);
            }
            if self.lifecycle_bridge.is_some() {
                while !context
                    .cancellation
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    if context
                        .execution()
                        .is_some_and(|execution| execution.mode() == TaskLaunchMode::Background)
                    {
                        return Ok(TaskTurnResult {
                            output: "probe".into(),
                            iterations: 1,
                        });
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                return Err(TaskRunnerError::Cancelled);
            }
            return Ok(TaskTurnResult {
                output: "probe".into(),
                iterations: 1,
            });
        }
        let cancellation = HeadlessTurnCancellation::with_cancellation_and_deadline(
            Arc::clone(&context.cancellation),
            None,
        );
        let progress = self.lifecycle_bridge.as_ref().zip(context.execution()).map(
            |(lifecycle_bridge, execution)| {
                let lifecycle_bridge = lifecycle_bridge.clone();
                let id = execution.id().value();
                Arc::new(move |event| lifecycle_bridge.observe_progress(id, event))
                    as TurnProgressSink
            },
        );
        let diagnostic_reference = next_diagnostic_reference();
        #[cfg(test)]
        let diagnostic_reference = if self.failure_probe.is_some() {
            "abc12345".into()
        } else {
            diagnostic_reference
        };
        #[cfg(test)]
        let result = self
            .failure_probe
            .as_ref()
            .map(|failure| Err(failure.error))
            .unwrap_or_else(|| {
                run_production_task(
                    request,
                    ProductionTaskExecutionContext {
                        bootstrap: &self.bootstrap,
                        project_root: &self.project_root,
                        dangerous_mode: self.dangerous_mode,
                        cancellation: &cancellation,
                        progress: progress.as_ref(),
                        diagnostic_reference: &diagnostic_reference,
                        task_registry: context.execution_registry(),
                        execution_id: context.execution().expect("registered task execution").id(),
                    },
                )
            });
        #[cfg(not(test))]
        let result = run_production_task(
            request,
            ProductionTaskExecutionContext {
                bootstrap: &self.bootstrap,
                project_root: &self.project_root,
                dangerous_mode: self.dangerous_mode,
                cancellation: &cancellation,
                progress: progress.as_ref(),
                diagnostic_reference: &diagnostic_reference,
                task_registry: context.execution_registry(),
                execution_id: context.execution().expect("registered task execution").id(),
            },
        );
        if let Err(error) = &result {
            record_subagent_terminal(
                &self.bootstrap,
                &diagnostic_reference,
                error.diagnostic_class(),
            );
        }
        if let (Some(lifecycle_bridge), Some(execution), Err(error)) =
            (&self.lifecycle_bridge, context.execution(), &result)
            && let Some(kind) = error.tui_kind()
        {
            lifecycle_bridge.publish(TuiRuntimeEvent::SubagentExecution(
                TuiSubagentEvent::error_with_reference(
                    execution.id().value(),
                    kind,
                    &diagnostic_reference,
                ),
            ));
        }
        let result = result.map_err(ChildRunError::task_runner_error);
        if let (Some(lifecycle_bridge), Some(execution)) =
            (&self.lifecycle_bridge, context.execution())
        {
            lifecycle_bridge.record_terminal_result(
                execution.id().value(),
                result.as_ref().map(String::as_str),
            );
        }
        result.map(|output| TaskTurnResult {
            output,
            iterations: 1,
        })
    }
}

#[cfg(test)]
pub(crate) fn map_task_turn_error(error: HeadlessTurnError) -> TaskRunnerError {
    match error {
        HeadlessTurnError::Cancelled => TaskRunnerError::Cancelled,
        HeadlessTurnError::TimedOut => TaskRunnerError::TimedOut,
        HeadlessTurnError::Provider
        | HeadlessTurnError::ProviderRejected
        | HeadlessTurnError::ProviderRateLimited
        | HeadlessTurnError::ProviderServer
        | HeadlessTurnError::ProviderProtocol => TaskRunnerError::ProviderFailure,
        HeadlessTurnError::MaxIterations => TaskRunnerError::IterationLimit,
        _ => TaskRunnerError::ChildFailure,
    }
}

#[cfg(test)]
mod tests {
    use agens_core::{
        PermissionDecision, PermissionMode, PermissionPattern, PermissionPolicy, PermissionRule,
        PermissionSession,
    };
    use agens_store::SessionStore;
    use agens_tools::{
        SkillCatalog, TaskTerminalState, ToolDispatchRequest, ToolEvaluationOutcome,
        ToolExecutionContext,
    };
    use agens_tui::TuiPermissionReply;

    use super::*;
    use crate::CliError;
    use crate::dispatch::{TuiSelectedTaskLaunch, launch_selected_tui_task};
    use crate::permissions::prompt::production_tui_permission_bridge;
    use crate::test_support::{tui_session_bootstrap, tui_session_directory};
    use crate::tools::task::production_tui_task_runtime_with_runner;

    #[test]
    fn production_task_error_mapping_reserves_provider_for_provider_failures() {
        assert_eq!(
            map_task_turn_error(HeadlessTurnError::MaxIterations),
            TaskRunnerError::IterationLimit
        );
        assert_eq!(
            map_task_turn_error(HeadlessTurnError::Provider),
            TaskRunnerError::ProviderFailure
        );
        assert_eq!(
            map_task_turn_error(HeadlessTurnError::Tool),
            TaskRunnerError::ChildFailure
        );
    }

    #[test]
    fn p1c1_terminal_observer_excludes_non_completed_matrix() {
        for (label, terminal) in [
            ("cancelled", Some(TaskTerminalState::Cancelled)),
            ("timed-out", Some(TaskTerminalState::Failed)),
            ("incomplete", None),
            ("failed", Some(TaskTerminalState::Failed)),
        ] {
            let temporary = tui_session_directory(&format!("p1c1-{label}"));
            let bootstrap = tui_session_bootstrap(
                &temporary,
                &[(
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                )],
            );
            let (events, _receiver) = BridgeTx::bounded(8);
            let controls = TuiTaskControls::default();
            let session = Arc::new(Mutex::new(SessionContext {
                selected_subagent: Some("reviewer".into()),
                ..SessionContext::fresh()
            }));
            let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, controls.clone())
                .with_session_writer(bootstrap.clone(), Arc::clone(&session));
            let mut runtime = production_tui_task_runtime_with_runner(
                &bootstrap,
                &crate::session_root::discovered_root_for_tests(&bootstrap),
                &SkillCatalog::default(),
                production_tui_permission_bridge().0,
                ProductionTaskRunner::with_probe(
                    bootstrap.clone(),
                    crate::session_root::discovered_root_for_tests(&bootstrap),
                    Arc::new(Mutex::new(Vec::new())),
                )
                .with_lifecycle_bridge(lifecycle_bridge),
            )
            .unwrap();
            runtime.authorized.gate.policy = PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    PermissionDecision::Allow,
                    PermissionPattern::Exact("native::task".into()),
                    PermissionPattern::Any,
                )],
            );
            let cancellation = HeadlessTurnCancellation::new();
            let worker_session = Arc::clone(&session);
            let worker_cancellation = cancellation.clone();
            let worker = std::thread::spawn(move || {
                launch_selected_tui_task(
                    &mut runtime,
                    &worker_session,
                    "review task",
                    false,
                    &worker_cancellation,
                )
            });
            let lifecycle =
                crate::test_support::wait_for("the running task to be observed", || {
                    controls
                        .0
                        .lifecycle(agens_tools::TaskExecutionId::from_value(1))
                });

            assert!(session.lock().unwrap().identifier.is_none());
            assert!(lifecycle.transition_to_background());
            assert!(session.lock().unwrap().identifier.is_none());
            if let Some(terminal) = terminal {
                assert!(lifecycle.finish(terminal));
            }
            if label == "failed" {
                assert!(!lifecycle.finish(TaskTerminalState::Completed));
            }

            cancellation.cancel();
            let _ = worker.join().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));

            assert!(session.lock().unwrap().identifier.is_none());
            assert!(
                SessionStore::open(bootstrap.data_directory())
                    .unwrap()
                    .list_sessions()
                    .unwrap()
                    .is_empty()
            );

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    #[test]
    fn u15_a1b2_selected_launch_uses_the_registered_production_task_runner() {
        let temporary = tui_session_directory("selected-task-launch");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (bridge, requests) = production_tui_permission_bridge();
        let reply_bridge = bridge.clone();
        let reply = std::thread::spawn(move || {
            let request = requests
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("selected task should request permission");
            reply_bridge.reply(request.id(), TuiPermissionReply::AllowOnce)
        });
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &crate::session_root::discovered_root_for_tests(&bootstrap),
            &SkillCatalog::default(),
            bridge,
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                crate::session_root::discovered_root_for_tests(&bootstrap),
                Arc::clone(&probe),
            ),
        )
        .unwrap();
        let session = Arc::new(Mutex::new(SessionContext {
            selected_subagent: Some("reviewer".into()),
            ..SessionContext::fresh()
        }));
        let cancellation = HeadlessTurnCancellation::new();
        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let mut dispatcher = runtime.dispatcher.lock().unwrap();
        let ToolEvaluationOutcome::Authorized(handle) = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new(
                    "project",
                    "native::task",
                    serde_json::json!({
                        "agent": "reviewer",
                        "description": "model task",
                        "background": true,
                    }),
                ),
            )
            .unwrap()
        else {
            panic!("registered model task should authorize");
        };
        assert_eq!(
            dispatcher
                .execute(
                    handle,
                    &ToolExecutionContext::from_headless_adapter(cancellation.adapter_view()),
                )
                .unwrap()
                .content,
            "Subagent #1 running in background"
        );
        drop(dispatcher);

        assert_eq!(
            launch_selected_tui_task(&mut runtime, &session, "review task", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Dispatched)
        );
        let probe = probe.lock().unwrap();
        assert_eq!(probe.len(), 2);
        assert_eq!(probe[0].1, TaskLaunchMode::Background);
        assert_eq!(probe[1].1, TaskLaunchMode::Foreground);
        assert_ne!(probe[0].0, probe[1].0);
        assert!(reply.join().unwrap());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn p1c1_p1b_authorized_runner_persists_one_completed_subagent_turn() {
        let temporary = tui_session_directory("p1b-child-events");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (events, receiver) = BridgeTx::bounded(16);
        let controls = TuiTaskControls::default();
        let session = Arc::new(Mutex::new(SessionContext {
            selected_subagent: Some("reviewer".into()),
            ..SessionContext::fresh()
        }));
        let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, controls)
            .with_session_writer(bootstrap.clone(), Arc::clone(&session));
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &crate::session_root::discovered_root_for_tests(&bootstrap),
            &SkillCatalog::default(),
            production_tui_permission_bridge().0,
            ProductionTaskRunner::with_progress_probe(
                bootstrap.clone(),
                crate::session_root::discovered_root_for_tests(&bootstrap),
                Arc::clone(&probe),
                vec![
                    TurnEvent::ProviderPart(MessagePart::Reasoning("inspect".into())),
                    TurnEvent::ProviderPart(MessagePart::Text("partial".into())),
                    TurnEvent::ToolCallRequested {
                        id: "read-1".into(),
                        name: "native::read".into(),
                        input: format!("authorization {}", "x".repeat(300)),
                    },
                    TurnEvent::ToolResult(MessagePart::ToolResult {
                        tool_call_id: "read-1".into(),
                        content: format!("token {}", "y".repeat(300)),
                        is_error: false,
                    }),
                ],
            )
            .with_lifecycle_bridge(lifecycle_bridge),
        )
        .unwrap();
        runtime.authorized.gate.policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let cancellation = HeadlessTurnCancellation::new();

        assert_eq!(
            launch_selected_tui_task(&mut runtime, &session, "review task", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Dispatched)
        );

        let mut received = Vec::new();
        for _ in 0..8 {
            match receiver.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(event) => received.push(event.into_parts().1),
                Err(error) => {
                    panic!("child event should reach the TUI bridge: {received:?}: {error}")
                }
            }
        }
        assert_eq!(
            received,
            vec![
                TuiRuntimeEvent::TaskExecution {
                    agent: "reviewer".into(),
                    event: TuiExecutionEvent::ForegroundStarted { id: 1 },
                },
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::started(
                    1,
                    "reviewer",
                    "review task",
                    agens_tui::TuiExecutionState::ForegroundRunning,
                )),
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::reasoning(1, "inspect")),
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::text(1, "partial")),
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::tool_call(
                    1,
                    "read-1",
                    "native::read",
                    "[redacted]",
                )),
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::tool_result(
                    1,
                    "read-1",
                    "[redacted]",
                    false,
                )),
                TuiRuntimeEvent::TaskExecution {
                    agent: "reviewer".into(),
                    event: TuiExecutionEvent::Completed { id: 1 },
                },
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::terminal(
                    1,
                    SubagentStatus::Success,
                    "probe",
                )),
            ]
        );
        assert_eq!(probe.lock().unwrap().len(), 1);
        let session_id = crate::test_support::wait_for(
            "the completed terminal to persist exactly one durable turn",
            || session.lock().unwrap().identifier,
        );
        let stored = SessionStore::open(bootstrap.data_directory())
            .unwrap()
            .load_session_for_resume(session_id)
            .unwrap();
        assert_eq!(stored.metadata.completed_turn_count, 1);
        assert_eq!(stored.messages.len(), 3);
        assert_eq!(
            stored.messages[2].parts[0],
            MessagePart::ToolResult {
                tool_call_id: "subagent:1".into(),
                content: "probe".into(),
                is_error: false,
            }
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn failed_subagent_turn_persistence_publishes_a_runtime_error() {
        let temporary = tui_session_directory("subagent-persistence-failure");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        std::fs::create_dir_all(bootstrap.data_directory().join("agens.db")).unwrap();
        let (events, receiver) = BridgeTx::bounded(4);
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let bridge = TuiTaskLifecycleBridge::new(events, TuiTaskControls::default())
            .with_session_writer(bootstrap.clone(), Arc::clone(&session));
        let persist = bridge
            .persist_completed
            .clone()
            .expect("session writer should be installed");

        persist(CompletedSubagentTurn {
            id: 7,
            agent: "reviewer".into(),
            task: "review task".into(),
            final_result: "done".into(),
            tool_uses: 1,
        });

        assert_eq!(
            receiver
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("persistence failure should reach the TUI bridge")
                .into_parts()
                .1,
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::error(
                7,
                SubagentErrorKind::Runtime,
            ))
        );
        assert!(session.lock().unwrap().identifier.is_none());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn production_runner_error_publication_orders_sanitized_typed_failure_before_terminal() {
        for (
            source,
            expected_error,
            expected_kind,
            expected_execution,
            expected_status,
            expected_result,
        ) in [
            (
                ChildRunError::Authentication,
                TaskRunnerError::ProviderFailure,
                Some(SubagentErrorKind::Authentication),
                TuiExecutionEvent::Failed { id: 1 },
                SubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Context,
                TaskRunnerError::ProviderFailure,
                Some(SubagentErrorKind::Context),
                TuiExecutionEvent::Failed { id: 1 },
                SubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Network,
                TaskRunnerError::ProviderFailure,
                Some(SubagentErrorKind::Network),
                TuiExecutionEvent::Failed { id: 1 },
                SubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Provider,
                TaskRunnerError::ProviderFailure,
                Some(SubagentErrorKind::Provider),
                TuiExecutionEvent::Failed { id: 1 },
                SubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Protocol,
                TaskRunnerError::ProviderFailure,
                Some(SubagentErrorKind::Protocol),
                TuiExecutionEvent::Failed { id: 1 },
                SubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::RateLimited,
                TaskRunnerError::ProviderFailure,
                Some(SubagentErrorKind::RateLimited),
                TuiExecutionEvent::Failed { id: 1 },
                SubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Rejected,
                TaskRunnerError::ProviderFailure,
                Some(SubagentErrorKind::Rejected),
                TuiExecutionEvent::Failed { id: 1 },
                SubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Server,
                TaskRunnerError::ProviderFailure,
                Some(SubagentErrorKind::Server),
                TuiExecutionEvent::Failed { id: 1 },
                SubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Tool,
                TaskRunnerError::ChildFailure,
                Some(SubagentErrorKind::Tool),
                TuiExecutionEvent::Failed { id: 1 },
                SubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Runtime,
                TaskRunnerError::ChildFailure,
                Some(SubagentErrorKind::Runtime),
                TuiExecutionEvent::Failed { id: 1 },
                SubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Cancelled,
                TaskRunnerError::Cancelled,
                None,
                TuiExecutionEvent::Cancelled { id: 1 },
                SubagentStatus::Cancelled,
                "cancelled",
            ),
            (
                ChildRunError::TimedOut,
                TaskRunnerError::TimedOut,
                None,
                TuiExecutionEvent::Failed { id: 1 },
                SubagentStatus::Failure,
                "failed",
            ),
        ] {
            let temporary = tui_session_directory("runner-error-publication");
            let bootstrap = tui_session_bootstrap(
                &temporary,
                &[(
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                )],
            );
            let (events, receiver) = BridgeTx::bounded(8);
            let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, TuiTaskControls::default());
            let mut runtime = production_tui_task_runtime_with_runner(
                &bootstrap,
                &crate::session_root::discovered_root_for_tests(&bootstrap),
                &SkillCatalog::default(),
                production_tui_permission_bridge().0,
                ProductionTaskRunner::with_failure_probe(
                    bootstrap.clone(),
                    crate::session_root::discovered_root_for_tests(&bootstrap),
                    source,
                    "provider-token=super-secret-error-detail",
                )
                .with_lifecycle_bridge(lifecycle_bridge),
            )
            .unwrap();
            runtime.authorized.gate.policy = PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    PermissionDecision::Allow,
                    PermissionPattern::Exact("native::task".into()),
                    PermissionPattern::Any,
                )],
            );
            let session = Arc::new(Mutex::new(SessionContext {
                selected_subagent: Some("reviewer".into()),
                ..SessionContext::fresh()
            }));

            assert_eq!(
                launch_selected_tui_task(
                    &mut runtime,
                    &session,
                    "review task",
                    false,
                    &HeadlessTurnCancellation::new(),
                ),
                Err(CliError::runtime(HeadlessTurnError::Tool))
            );

            let mut expected = vec![
                TuiRuntimeEvent::TaskExecution {
                    agent: "reviewer".into(),
                    event: TuiExecutionEvent::ForegroundStarted { id: 1 },
                },
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::started(
                    1,
                    "reviewer",
                    "review task",
                    agens_tui::TuiExecutionState::ForegroundRunning,
                )),
            ];
            if let Some(kind) = expected_kind {
                expected.push(TuiRuntimeEvent::SubagentExecution(
                    TuiSubagentEvent::error_with_reference(1, kind, "abc12345"),
                ));
            }
            expected.push(TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: expected_execution,
            });
            expected.push(TuiRuntimeEvent::SubagentExecution(
                TuiSubagentEvent::terminal(1, expected_status, expected_result),
            ));

            let received = (0..expected.len())
                .map(|_| {
                    receiver
                        .recv_timeout(std::time::Duration::from_secs(1))
                        .expect("runner failure should publish every bridge event")
                        .into_parts()
                        .1
                })
                .collect::<Vec<_>>();
            assert_eq!(received, expected);
            assert!(
                received
                    .iter()
                    .all(|event| !format!("{event:?}").contains("super-secret"))
            );
            assert!(
                receiver
                    .recv_timeout(std::time::Duration::from_millis(20))
                    .is_err(),
                "runner failure must publish exactly one terminal"
            );
            assert_eq!(expected_error, source.task_runner_error());

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }
}
