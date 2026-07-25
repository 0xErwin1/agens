//! TUI task lifecycle bridging and the production `TaskRunner` implementation
//! that drives an isolated subagent turn to completion, reporting progress
//! and terminal results back to the TUI.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[cfg(test)]
use agens_core::HeadlessTurnError;
use agens_core::{HeadlessTurnCancellation, MessagePart, TurnEvent, TurnProgressSink};
use agens_tools::{
    TaskExecutionEvent, TaskExecutionLifecycle, TaskExecutionRegistry, TaskLaunchMode,
    TaskRunContext, TaskRunner, TaskRunnerError, TaskTurnRequest, TaskTurnResult,
};
use agens_tui::{
    BridgeCancel, BridgeTx, TuiExecutionEvent, TuiRuntimeEvent, TuiSubagentErrorKind,
    TuiSubagentEvent, TuiSubagentStatus,
};

use crate::Bootstrap;
use crate::diagnostics::{next_diagnostic_reference, record_subagent_terminal};
use crate::permissions::ParseToolInput;
use crate::tools::child::{ChildRunError, ProductionTaskExecutionContext, run_production_task};
use crate::tui::metrics::sanitize_tui_metric;
use crate::tui::session::{CompletedSubagentTurn, TuiSessionContext, current_session_timestamp};
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
        session: Arc<Mutex<TuiSessionContext>>,
    ) -> Self {
        let events = self.events.clone();
        self.persist_completed = Some(Arc::new(move |turn: CompletedSubagentTurn| {
            let id = turn.id;
            if persist_completed_subagent_turn(&bootstrap, &session, turn).is_err() {
                let _ = events.publish(
                    TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::error(
                        id,
                        TuiSubagentErrorKind::Runtime,
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
                                (TuiSubagentStatus::Success, "completed")
                            }
                            TuiExecutionEvent::Failed { .. } => {
                                (TuiSubagentStatus::Failure, "failed")
                            }
                            TuiExecutionEvent::Cancelled { .. } => {
                                (TuiSubagentStatus::Cancelled, "cancelled")
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
                TuiSubagentErrorKind::Runtime,
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
