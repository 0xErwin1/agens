//! TUI task lifecycle bridging and the production `TaskRunner` implementation
//! that drives an isolated subagent turn to completion, reporting progress
//! and terminal results back to the TUI.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agens_bus::{BridgeCancel, BridgeTx};
#[cfg(any(test, feature = "probe"))]
use agens_core::HeadlessTurnError;
use agens_core::{MessagePart, TurnEvent, TurnProgressSink};
use agens_core::{SubagentErrorKind, SubagentStatus};
use agens_core::{TuiExecutionEvent, TuiRuntimeEvent, TuiSubagentEvent};
use agens_tools::{
    TaskCancellationCause, TaskExecutionEvent, TaskExecutionLifecycle, TaskExecutionRegistry,
    TaskLaunchMode, TaskRunContext, TaskRunner, TaskRunnerError, TaskTurnRequest, TaskTurnResult,
};

use crate::child::{ChildRunError, ProductionTaskExecutionContext, run_production_task};
use agens_bootstrap::Bootstrap;
use agens_diagnostics::{next_diagnostic_reference, record_subagent_terminal};
use agens_permissions::ParseToolInput;
use agens_permissions::sanitize_metric;
use agens_session::context::SessionContext;
use agens_session::context::{CompletedSubagentTurn, current_session_timestamp};
use agens_session::turns::persist_completed_subagent_turn;

#[cfg(any(test, feature = "probe"))]
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

/// Names the delegated execution a prompt belongs to, so the surface can say
/// which subagent is asking rather than putting an anonymous question in front
/// of someone running several.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptOrigin {
    pub execution: u64,
    pub agent: String,
}

/// Builds one delegated execution's prompter onto the parent's surface.
pub type PrompterFactory =
    Arc<dyn Fn(PromptOrigin) -> Box<dyn agens_permissions::PermissionPrompter> + Send + Sync>;

/// Builds one delegated execution's `ask_user` port onto the same surface.
pub type AskUserPortFactory =
    Arc<dyn Fn(PromptOrigin) -> Box<dyn agens_core::ask_user::AskUserPort> + Send + Sync>;

#[cfg(any(test, feature = "probe"))]
struct TestTaskFailure {
    error: ChildRunError,
    _source_detail: String,
}

#[derive(Clone, Default)]
pub struct TuiTaskControls(pub TaskExecutionRegistry);

impl TuiTaskControls {
    pub fn transition_to_background(&self, id: u64) -> bool {
        self.0
            .transition_to_background(agens_tools::TaskExecutionId::from_value(id))
    }
}
#[derive(Clone)]
pub struct TuiTaskLifecycleBridge {
    events: BridgeTx<TuiRuntimeEvent>,
    controls: TuiTaskControls,
    lifecycle: Arc<Mutex<Option<TaskExecutionLifecycle>>>,
    terminal_results: Arc<Mutex<BTreeMap<u64, String>>>,
    completed_turns: Arc<Mutex<BTreeMap<u64, CompletedSubagentTurn>>>,
    pub persist_completed: Option<Arc<dyn Fn(CompletedSubagentTurn) -> bool + Send + Sync>>,
}

impl TuiTaskLifecycleBridge {
    pub fn new(events: BridgeTx<TuiRuntimeEvent>, controls: TuiTaskControls) -> Self {
        Self {
            events,
            controls,
            lifecycle: Arc::new(Mutex::new(None)),
            terminal_results: Arc::new(Mutex::new(BTreeMap::new())),
            completed_turns: Arc::new(Mutex::new(BTreeMap::new())),
            persist_completed: None,
        }
    }

    pub fn with_session_writer(
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
                        SubagentErrorKind::ResultDelivery,
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

    pub fn mode(&self) -> Option<TaskLaunchMode> {
        let lifecycle = self.lifecycle.lock().ok()?;
        Some(lifecycle.as_ref()?.mode())
    }

    fn observe(&self, request: &TaskTurnRequest, lifecycle: TaskExecutionLifecycle) {
        let id = lifecycle.id().value();
        if let Ok(mut current) = self.lifecycle.lock() {
            *current = Some(lifecycle.clone());
        }
        let presentation = match lifecycle.mode() {
            TaskLaunchMode::Foreground => agens_core::TuiExecutionState::ForegroundRunning,
            TaskLaunchMode::Background => agens_core::TuiExecutionState::BackgroundRunning,
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
            TuiSubagentEvent::started_on(
                id,
                &agent,
                request.description(),
                presentation,
                Some(request.model()),
                request
                    .request_config()
                    .reasoning_effort()
                    .map(|effort| effort.as_str()),
            ),
        ));
        self.watch_lifecycle(agent, lifecycle);
    }

    fn watch_lifecycle(&self, agent: String, lifecycle: TaskExecutionLifecycle) {
        let id = lifecycle.id().value();
        let events = self.events.clone();
        let registry = self.controls.0.clone();
        let terminal_results = Arc::clone(&self.terminal_results);
        let completed_turns = Arc::clone(&self.completed_turns);
        let persist_completed = self.persist_completed.clone();
        std::thread::spawn(move || {
            let mut seen = 1;
            let mut cancellation_requested = false;
            loop {
                let requested = registry
                    .active_snapshots()
                    .iter()
                    .find(|snapshot| snapshot.id.value() == id)
                    .is_some_and(|snapshot| snapshot.cancellation_requested);
                if requested && !cancellation_requested {
                    cancellation_requested = true;
                    let _ = events.publish(
                        TuiRuntimeEvent::TaskExecution {
                            agent: agent.clone(),
                            event: TuiExecutionEvent::CancellationRequested { id },
                        },
                        &BridgeCancel::new(),
                        None,
                    );
                }
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
                            TuiExecutionEvent::Failed { .. } => {
                                (SubagentStatus::Failure, "failed on its own")
                            }
                            TuiExecutionEvent::Cancelled { .. } => (
                                SubagentStatus::Cancelled,
                                terminal_cancellation_state(&registry, id),
                            ),
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
                        // Both of these exist to tell a parent about work it
                        // was not waiting for. A foreground parent already
                        // holds this result inline, so the notice asks it to
                        // re-read what it just read and the synthetic turn
                        // records a second copy of an exchange the real turn
                        // is about to persist itself.
                        if lifecycle.mode() == TaskLaunchMode::Background {
                            let persisted = matches!(event, TuiExecutionEvent::Completed { .. })
                                && match (completed_turn, &persist_completed) {
                                    (Some(turn), Some(persist)) => persist(turn),
                                    _ => false,
                                };
                            notify_main_of_terminal_subagent(
                                &registry, &events, id, &agent, fallback, persisted,
                            );
                        }
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
                let sanitized_input = sanitize_metric(&input);
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

fn terminal_cancellation_state(registry: &TaskExecutionRegistry, id: u64) -> &'static str {
    match registry
        .snapshot(agens_tools::TaskExecutionId::from_value(id))
        .and_then(|snapshot| snapshot.cancellation_cause)
    {
        Some(TaskCancellationCause::ParentTurn) => {
            "cancelled because the parent turn was cancelled"
        }
        Some(TaskCancellationCause::TaskControl) => "cancelled by task_control cancel",
        Some(TaskCancellationCause::SessionClosed) => "cancelled because the session closed",
        None => "cancelled",
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
                SubagentErrorKind::ResultDelivery,
            )),
            &BridgeCancel::new(),
            None,
        );
    }
}

pub struct ProductionTaskRunner {
    bootstrap: Bootstrap,
    project_root: PathBuf,
    dangerous_mode: bool,
    bypass: bool,
    lifecycle_bridge: Option<TuiTaskLifecycleBridge>,
    task_registry: Option<TaskExecutionRegistry>,
    /// The parent's own MCP registry, shared rather than rebuilt. `None` only
    /// where no parent runtime handed one over, which is every test that
    /// exercises the runner without an MCP surface.
    mcp_registry: Option<Arc<Mutex<agens_tools::McpRegistry>>>,
    /// Makes a prompter onto the surface the parent is already using, one per
    /// delegated execution.
    ///
    /// A factory rather than a shared prompter because prompting is a surface:
    /// each execution owns its own, exactly as the parent turn does, and they
    /// reach the same person through the same bridge without contending for a
    /// lock while one of them is parked on an answer.
    permission_prompter: Option<PrompterFactory>,
    /// Builds this runner's executions their own `ask_user` port, so a
    /// subagent can ask the person a question exactly as the main thread does.
    ask_user_port: Option<AskUserPortFactory>,
    /// How deep the runtime that owns this runner sits. The parent turn is 0,
    /// so the executions this runner launches are at `depth + 1`.
    depth: usize,
    #[cfg(any(test, feature = "probe"))]
    probe: Option<ProductionTaskProbe>,
    #[cfg(any(test, feature = "probe"))]
    progress_probe: Option<Vec<TurnEvent>>,
    #[cfg(any(test, feature = "probe"))]
    failure_probe: Option<TestTaskFailure>,
}

impl ProductionTaskRunner {
    pub fn new(bootstrap: Bootstrap, project_root: PathBuf) -> Self {
        Self {
            bootstrap,
            project_root,
            dangerous_mode: false,
            bypass: false,
            lifecycle_bridge: None,
            task_registry: None,
            mcp_registry: None,
            permission_prompter: None,
            ask_user_port: None,
            depth: 0,
            #[cfg(any(test, feature = "probe"))]
            probe: None,
            #[cfg(any(test, feature = "probe"))]
            progress_probe: None,
            #[cfg(any(test, feature = "probe"))]
            failure_probe: None,
        }
    }

    pub fn with_lifecycle_bridge(mut self, lifecycle_bridge: TuiTaskLifecycleBridge) -> Self {
        self.task_registry = Some(lifecycle_bridge.controls.0.clone());
        self.lifecycle_bridge = Some(lifecycle_bridge);
        self
    }

    pub fn with_dangerous_mode(mut self, dangerous_mode: bool) -> Self {
        self.dangerous_mode = dangerous_mode;
        self
    }

    /// Gives delegated executions a way to reach the person at the surface.
    ///
    /// Without one a child's `Ask` is answered `Deny` by nobody, which is the
    /// only thing a headless delegation can honestly do; with one it is
    /// answered by the same person, on the same prompt, that the parent's own
    /// calls go to.
    #[must_use]
    pub fn with_permission_prompter(mut self, prompter: PrompterFactory) -> Self {
        self.permission_prompter = Some(prompter);
        self
    }

    /// Lets delegated executions ask the person a question, on the same
    /// surface and with the same standing as the main thread's own.
    #[must_use]
    pub fn with_ask_user_port(mut self, port: AskUserPortFactory) -> Self {
        self.ask_user_port = Some(port);
        self
    }

    /// Places this runner in the delegation chain. Only a child's own runtime
    /// sets it; the parent turn's runner is at the default 0.
    #[must_use]
    pub const fn with_depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    /// Admits this runner's executions into an existing registry, so a whole
    /// delegation chain shares one set of concurrency and iteration bounds
    /// rather than each level getting its own allowance.
    #[must_use]
    pub fn with_task_registry(mut self, registry: TaskExecutionRegistry) -> Self {
        self.task_registry = Some(registry);
        self
    }

    /// Marks that this runner's own permission-authorization session (built in
    /// `task.rs` for the parent's `task` tool call) should bypass `Ask`. This never
    /// reaches the launched child's own tool session in `child.rs`, which stays
    /// fail-closed regardless — see the module docs on subagent scope.
    pub fn with_bypass(mut self, bypass: bool) -> Self {
        self.bypass = bypass;
        self
    }

    pub(crate) fn bypass(&self) -> bool {
        self.bypass
    }
    #[cfg(any(test, feature = "probe"))]
    pub fn with_probe(
        bootstrap: Bootstrap,
        project_root: PathBuf,
        probe: ProductionTaskProbe,
    ) -> Self {
        Self {
            bootstrap,
            project_root,
            dangerous_mode: false,
            bypass: false,
            lifecycle_bridge: None,
            task_registry: None,
            mcp_registry: None,
            permission_prompter: None,
            ask_user_port: None,
            depth: 0,
            probe: Some(probe),
            progress_probe: None,
            failure_probe: None,
        }
    }

    #[cfg(any(test, feature = "probe"))]
    pub fn with_progress_probe(
        bootstrap: Bootstrap,
        project_root: PathBuf,
        probe: ProductionTaskProbe,
        progress: Vec<TurnEvent>,
    ) -> Self {
        Self {
            bootstrap,
            project_root,
            dangerous_mode: false,
            bypass: false,
            lifecycle_bridge: None,
            task_registry: None,
            mcp_registry: None,
            permission_prompter: None,
            ask_user_port: None,
            depth: 0,
            probe: Some(probe),
            progress_probe: Some(progress),
            failure_probe: None,
        }
    }

    #[cfg(any(test, feature = "probe"))]
    pub fn with_failure_probe(
        bootstrap: Bootstrap,
        project_root: PathBuf,
        error: ChildRunError,
        source_detail: &str,
    ) -> Self {
        Self {
            bootstrap,
            project_root,
            dangerous_mode: false,
            bypass: false,
            lifecycle_bridge: None,
            task_registry: None,
            mcp_registry: None,
            permission_prompter: None,
            ask_user_port: None,
            depth: 0,
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

    fn share_mcp_registry(&mut self, registry: Arc<Mutex<agens_tools::McpRegistry>>) {
        self.mcp_registry = Some(registry);
    }

    /// `self.bypass` deliberately does NOT reach [`ProductionTaskExecutionContext`] below, unlike
    /// `self.dangerous_mode`: `child.rs`'s own permission session is always `PermissionSession::new()`
    /// and its resolver (`ChildPermissionResolver`) always returns `Deny` on `Ask`, regardless of
    /// what a `ProductionTaskExecutionContext` field says — see `child.rs`'s characterization test.
    /// Adding a `bypass` field there would be a field nothing reads. `bypass` instead stops at this
    /// runner's own two `PermissionSession`s (`task.rs`'s gate and prompt-authorization sessions),
    /// which are what a TUI-launched subagent's tool calls are actually evaluated against.
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
        #[cfg(any(test, feature = "probe"))]
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
                };
                if let Some(lifecycle_bridge) = &self.lifecycle_bridge {
                    lifecycle_bridge
                        .record_terminal_result(execution.id().value(), Ok(&result.output));
                }
                return Ok(result);
            }
            if self.lifecycle_bridge.is_some() {
                while !context.is_cancelled() && !context.is_expired() {
                    if context
                        .execution()
                        .is_some_and(|execution| execution.mode() == TaskLaunchMode::Background)
                    {
                        return Ok(TaskTurnResult {
                            output: "probe".into(),
                        });
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                return if context.is_expired() {
                    Err(TaskRunnerError::TimedOut)
                } else {
                    Err(TaskRunnerError::Cancelled)
                };
            }
            return Ok(TaskTurnResult {
                output: "probe".into(),
            });
        }
        let cancellation = context.turn_cancellation();
        let progress = self.lifecycle_bridge.as_ref().zip(context.execution()).map(
            |(lifecycle_bridge, execution)| {
                let lifecycle_bridge = lifecycle_bridge.clone();
                let id = execution.id().value();
                Arc::new(move |event| lifecycle_bridge.observe_progress(id, event))
                    as TurnProgressSink
            },
        );
        let diagnostic_reference = next_diagnostic_reference();
        #[cfg(any(test, feature = "probe"))]
        let diagnostic_reference = if self.failure_probe.is_some() {
            "abc12345".into()
        } else {
            diagnostic_reference
        };
        #[cfg(any(test, feature = "probe"))]
        let result = self
            .failure_probe
            .as_ref()
            .map(|failure| Err(failure.error.clone()))
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
                        mcp_registry: self.mcp_registry.clone(),
                        permission_prompter: self.permission_prompter.clone(),
                        ask_user_port: self.ask_user_port.clone(),
                        depth: self.depth + 1,
                    },
                )
            });
        #[cfg(not(any(test, feature = "probe")))]
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
                mcp_registry: self.mcp_registry.clone(),
                permission_prompter: self.permission_prompter.clone(),
                ask_user_port: self.ask_user_port.clone(),
                depth: self.depth + 1,
            },
        );
        if let Err(error) = &result {
            record_subagent_terminal(
                &self.bootstrap,
                &diagnostic_reference,
                error.diagnostic_class(),
                error.input_class(),
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
        result.map(|output| TaskTurnResult { output })
    }
}

#[cfg(any(test, feature = "probe"))]
pub fn map_task_turn_error(error: HeadlessTurnError) -> TaskRunnerError {
    crate::child::child_run_error(error).task_runner_error()
}

#[cfg(test)]
mod lifecycle_bridge_tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn registry_cancellation_projects_pending_before_terminal_confirmation() {
        let registry = TaskExecutionRegistry::new();
        let id = registry.admit(TaskLaunchMode::Background).unwrap();
        let lifecycle = registry.lifecycle(id).unwrap();
        let (events, receiver) = BridgeTx::bounded(16);
        let bridge = TuiTaskLifecycleBridge::new(events, TuiTaskControls(registry.clone()));

        bridge.watch_lifecycle("reviewer".into(), lifecycle);

        assert!(registry.cancel(id));
        let pending = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            pending.into_parts().1,
            TuiRuntimeEvent::TaskExecution {
                event: TuiExecutionEvent::CancellationRequested { id: pending_id },
                ..
            } if pending_id == id.value()
        ));

        assert!(registry.finish(
            id,
            agens_tools::TaskTerminalState::Cancelled,
            agens_tools::ToolOutput::failure("task: cancelled"),
        ));
        let terminal = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            terminal.into_parts().1,
            TuiRuntimeEvent::TaskExecution {
                event: TuiExecutionEvent::Cancelled { id: terminal_id },
                ..
            } if terminal_id == id.value()
        ));
    }
}
