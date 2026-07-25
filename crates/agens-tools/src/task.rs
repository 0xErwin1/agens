use agens_core::{AgentDefinition, AgentMode, Error, HeadlessTaskTerminal, RequestConfig};
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    AgentCatalog, AgentModelValidator, DispatchTool, IS_SUBAGENT_WORKER, SkillCatalog,
    ToolExecutionContext, ToolOutput, install_subagent_panic_hook,
};

const MAX_TASK_DESCRIPTION_CHARS: usize = 16_384;
const MAX_TASK_MODEL_CHARS: usize = 64;
const MAX_TASK_SKILLS: usize = 128;
const MAX_TASK_SKILL_NAME_CHARS: usize = 64;
const MAX_TASK_ITERATIONS: usize = 16;
const MAX_TASK_OUTPUT_CHARS: usize = 65_536;
const MAX_TASK_CONCURRENCY: usize = 4;
const MAX_TASK_AGENT_SCHEMA_DESCRIPTION_CHARS: usize = 160;
const MAX_TASK_MODEL_SCHEMA_ENTRIES: usize = 256;
const TASK_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_TASK_MESSAGES_PER_TARGET: usize = 32;
const MAX_TASK_MESSAGE_BYTES: usize = 8 * 1024;
const MAX_TASK_MAILBOX_BYTES: usize = 64 * 1024;

type BeforePublicationHook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;
type ModelResolutionDiagnostics =
    Arc<dyn Fn(TaskModelResolutionError) -> Option<String> + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskExecutionId(u64);

impl TaskExecutionId {
    pub const fn from_value(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskLaunchMode {
    Foreground,
    Background,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskTerminalState {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskExecutionEvent {
    Admitted(TaskExecutionId, TaskLaunchMode),
    Backgrounded(TaskExecutionId),
    Completed(TaskExecutionId),
    Failed(TaskExecutionId),
    Cancelled(TaskExecutionId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskControlAction {
    Background,
    Cancel,
    Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskMessageSource {
    Main,
    User,
    Execution(TaskExecutionId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskMessageTarget {
    Main,
    Execution(TaskExecutionId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskMessage {
    source: TaskMessageSource,
    content: String,
}

impl TaskMessage {
    pub const fn source(&self) -> TaskMessageSource {
        self.source
    }

    pub fn content(&self) -> &str {
        &self.content
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskRegistryError {
    UnknownExecution,
    TerminalExecution,
    InvalidControl,
    ForbiddenRoute,
    MessageLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskExecutionSnapshot {
    pub id: TaskExecutionId,
    pub mode: TaskLaunchMode,
    pub terminal: Option<TaskTerminalState>,
    pub result: Option<ToolOutput>,
}

#[derive(Clone)]
pub struct TaskControlTool {
    registry: TaskExecutionRegistry,
    source: TaskMessageSource,
}

impl TaskControlTool {
    pub fn new(registry: TaskExecutionRegistry, source: TaskMessageSource) -> Self {
        Self { registry, source }
    }

    pub fn input_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["action", "id"],
            "properties": {
                "action": {"type": "string", "enum": ["background", "cancel", "status"]},
                "id": {"type": "integer", "minimum": 1}
            }
        })
    }
}

impl DispatchTool for TaskControlTool {
    fn permission_target(&self, arguments: &Value) -> Result<String, Error> {
        parse_execution_id(arguments).map(|id| id.value().to_string())
    }

    fn execute(&mut self, _: &ToolExecutionContext, arguments: Value) -> Result<ToolOutput, Error> {
        let id = parse_execution_id(&arguments)?;
        let action = match arguments.get("action").and_then(Value::as_str) {
            Some("background") => TaskControlAction::Background,
            Some("cancel") => TaskControlAction::Cancel,
            Some("status") => TaskControlAction::Status,
            _ => return Ok(ToolOutput::failure("task control arguments are invalid")),
        };
        if arguments.as_object().is_none_or(|object| object.len() != 2) {
            return Ok(ToolOutput::failure("task control arguments are invalid"));
        }

        if action == TaskControlAction::Status {
            if let TaskMessageSource::Execution(source_id) = self.source
                && source_id != id
            {
                return Ok(ToolOutput::failure("task control target is unavailable"));
            }
            return Ok(match self.registry.snapshot(id) {
                Some(snapshot) => ToolOutput::success(task_status(&snapshot)),
                None => ToolOutput::failure("task control target is unavailable"),
            });
        }

        Ok(match self.registry.control(self.source, id, action) {
            Ok(()) => match action {
                TaskControlAction::Background => {
                    ToolOutput::success(format!("Subagent #{} moved to background", id.value()))
                }
                TaskControlAction::Cancel => {
                    ToolOutput::success(format!("Subagent #{} cancellation requested", id.value()))
                }
                TaskControlAction::Status => unreachable!("status handled above"),
            },
            Err(_) => ToolOutput::failure("task control target is unavailable"),
        })
    }
}

#[derive(Clone)]
pub struct TaskMessageTool {
    registry: TaskExecutionRegistry,
    source: TaskMessageSource,
}

impl TaskMessageTool {
    pub fn new(registry: TaskExecutionRegistry, source: TaskMessageSource) -> Self {
        Self { registry, source }
    }

    pub fn input_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["message", "target"],
            "properties": {
                "message": {"type": "string", "minLength": 1, "maxLength": 8192},
                "target": {
                    "oneOf": [
                        {"type": "integer", "minimum": 1},
                        {"type": "string", "enum": ["main"]}
                    ]
                }
            }
        })
    }
}

impl DispatchTool for TaskMessageTool {
    fn permission_target(&self, arguments: &Value) -> Result<String, Error> {
        match parse_message_target(arguments)? {
            TaskMessageTarget::Main => Ok("main".into()),
            TaskMessageTarget::Execution(id) => Ok(id.value().to_string()),
        }
    }

    fn execute(&mut self, _: &ToolExecutionContext, arguments: Value) -> Result<ToolOutput, Error> {
        if arguments.as_object().is_none_or(|object| object.len() != 2) {
            return Ok(ToolOutput::failure("task message arguments are invalid"));
        }
        let target = parse_message_target(&arguments)?;
        let Some(message) = arguments.get("message").and_then(Value::as_str) else {
            return Ok(ToolOutput::failure("task message arguments are invalid"));
        };

        Ok(
            match self
                .registry
                .send_message(self.source, target, message.into())
            {
                Ok(()) => ToolOutput::success("task message queued"),
                Err(_) => ToolOutput::failure("task message target is unavailable"),
            },
        )
    }
}

fn enqueue_message(
    mailbox: &mut TaskMailbox,
    source: TaskMessageSource,
    content: String,
) -> Result<(), TaskRegistryError> {
    if mailbox.messages.len() >= MAX_TASK_MESSAGES_PER_TARGET
        || mailbox.bytes + content.len() > MAX_TASK_MAILBOX_BYTES
    {
        return Err(TaskRegistryError::MessageLimit);
    }

    mailbox.bytes += content.len();
    mailbox.messages.push_back(TaskMessage { source, content });
    Ok(())
}

fn parse_execution_id(arguments: &Value) -> Result<TaskExecutionId, Error> {
    arguments
        .get("id")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .map(TaskExecutionId)
        .ok_or_else(|| Error::Tool("task control arguments are invalid".into()))
}

fn parse_message_target(arguments: &Value) -> Result<TaskMessageTarget, Error> {
    match arguments.get("target") {
        Some(Value::String(target)) if target == "main" => Ok(TaskMessageTarget::Main),
        Some(Value::Number(target)) => target
            .as_u64()
            .filter(|id| *id > 0)
            .map(|id| TaskMessageTarget::Execution(TaskExecutionId(id)))
            .ok_or_else(|| Error::Tool("task message arguments are invalid".into())),
        _ => Err(Error::Tool("task message arguments are invalid".into())),
    }
}

fn task_status(snapshot: &TaskExecutionSnapshot) -> String {
    let status = match snapshot.terminal {
        Some(TaskTerminalState::Completed) => "completed",
        Some(TaskTerminalState::Failed) => "failed",
        Some(TaskTerminalState::Cancelled) => "cancelled",
        None if snapshot.mode == TaskLaunchMode::Foreground => "foreground running",
        None => "background running",
    };
    let mut output = format!("Subagent #{}: {status}", snapshot.id.value());
    if let Some(result) = &snapshot.result {
        output.push('\n');
        output.push_str(&result.content);
    }
    output
}

#[derive(Clone, Default)]
pub struct TaskExecutionRegistry {
    inner: Arc<Mutex<TaskExecutionRegistryState>>,
}

#[derive(Default)]
struct TaskExecutionRegistryState {
    next_id: u64,
    executions: BTreeMap<TaskExecutionId, TaskExecutionRecord>,
    main_mailbox: TaskMailbox,
}

struct TaskExecutionRecord {
    lifecycle: TaskExecutionLifecycle,
    cancellation: Arc<AtomicBool>,
    result: Option<ToolOutput>,
    mailbox: TaskMailbox,
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct TaskMailbox {
    messages: VecDeque<TaskMessage>,
    bytes: usize,
}

impl TaskExecutionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn admit(&self, mode: TaskLaunchMode) -> Option<TaskExecutionId> {
        self.join_finished_workers();
        let mut registry = self.inner.lock().expect("task registry lock poisoned");
        let active = registry
            .executions
            .values()
            .filter(|execution| execution.lifecycle.terminal().is_none())
            .count();
        if active >= MAX_TASK_CONCURRENCY || registry.next_id == u64::MAX {
            return None;
        }

        registry.next_id += 1;
        let id = TaskExecutionId(registry.next_id);
        registry.executions.insert(
            id,
            TaskExecutionRecord {
                lifecycle: TaskExecutionLifecycle::new(id, mode),
                cancellation: Arc::new(AtomicBool::new(false)),
                result: None,
                mailbox: TaskMailbox::default(),
                worker: None,
            },
        );
        Some(id)
    }

    pub fn lifecycle(&self, id: TaskExecutionId) -> Option<TaskExecutionLifecycle> {
        self.inner
            .lock()
            .ok()?
            .executions
            .get(&id)
            .map(|execution| execution.lifecycle.clone())
    }

    pub fn cancellation_handle(&self, id: TaskExecutionId) -> Option<Arc<AtomicBool>> {
        self.inner
            .lock()
            .ok()?
            .executions
            .get(&id)
            .map(|execution| Arc::clone(&execution.cancellation))
    }

    pub fn snapshot(&self, id: TaskExecutionId) -> Option<TaskExecutionSnapshot> {
        let registry = self.inner.lock().ok()?;
        let execution = registry.executions.get(&id)?;
        Some(TaskExecutionSnapshot {
            id,
            mode: execution.lifecycle.mode(),
            terminal: execution.lifecycle.terminal(),
            result: execution.result.clone(),
        })
    }

    pub fn result(&self, id: TaskExecutionId) -> Option<ToolOutput> {
        self.snapshot(id)?.result
    }

    pub fn finish(
        &self,
        id: TaskExecutionId,
        terminal: TaskTerminalState,
        result: ToolOutput,
    ) -> bool {
        let Ok(mut registry) = self.inner.lock() else {
            return false;
        };
        let Some(execution) = registry.executions.get_mut(&id) else {
            return false;
        };
        if !execution.lifecycle.finish(terminal) {
            return false;
        }

        execution.result = Some(result);
        true
    }

    pub fn control(
        &self,
        source: TaskMessageSource,
        id: TaskExecutionId,
        action: TaskControlAction,
    ) -> Result<(), TaskRegistryError> {
        if let TaskMessageSource::Execution(source_id) = source
            && source_id != id
        {
            return Err(TaskRegistryError::ForbiddenRoute);
        }

        let mut registry = self
            .inner
            .lock()
            .map_err(|_| TaskRegistryError::UnknownExecution)?;
        let execution = registry
            .executions
            .get_mut(&id)
            .ok_or(TaskRegistryError::UnknownExecution)?;
        if execution.lifecycle.terminal().is_some() {
            return Err(TaskRegistryError::TerminalExecution);
        }

        match action {
            TaskControlAction::Background => execution
                .lifecycle
                .transition_to_background()
                .then_some(())
                .ok_or(TaskRegistryError::InvalidControl),
            TaskControlAction::Cancel => {
                execution.cancellation.store(true, Ordering::Release);
                Ok(())
            }
            TaskControlAction::Status => Ok(()),
        }
    }

    pub fn transition_to_background(&self, id: TaskExecutionId) -> bool {
        self.control(TaskMessageSource::Main, id, TaskControlAction::Background)
            .is_ok()
    }

    pub fn cancel(&self, id: TaskExecutionId) -> bool {
        self.control(TaskMessageSource::Main, id, TaskControlAction::Cancel)
            .is_ok()
    }

    pub fn is_cancelled(&self, id: TaskExecutionId) -> bool {
        self.cancellation_handle(id)
            .is_some_and(|cancellation| cancellation.load(Ordering::Acquire))
    }

    pub fn send_message(
        &self,
        source: TaskMessageSource,
        target: TaskMessageTarget,
        content: String,
    ) -> Result<(), TaskRegistryError> {
        if content.is_empty() || content.len() > MAX_TASK_MESSAGE_BYTES {
            return Err(TaskRegistryError::MessageLimit);
        }
        match (source, target) {
            (
                TaskMessageSource::Main | TaskMessageSource::User,
                TaskMessageTarget::Execution(_),
            )
            | (TaskMessageSource::Execution(_), TaskMessageTarget::Main) => {}
            _ => return Err(TaskRegistryError::ForbiddenRoute),
        }

        let mut registry = self
            .inner
            .lock()
            .map_err(|_| TaskRegistryError::UnknownExecution)?;
        if let TaskMessageSource::Execution(source_id) = source {
            let source = registry
                .executions
                .get(&source_id)
                .ok_or(TaskRegistryError::UnknownExecution)?;
            if source.lifecycle.terminal().is_some() {
                return Err(TaskRegistryError::TerminalExecution);
            }
        }
        let mailbox = match target {
            TaskMessageTarget::Main => &mut registry.main_mailbox,
            TaskMessageTarget::Execution(id) => {
                let execution = registry
                    .executions
                    .get_mut(&id)
                    .ok_or(TaskRegistryError::UnknownExecution)?;
                if execution.lifecycle.terminal().is_some() {
                    return Err(TaskRegistryError::TerminalExecution);
                }
                &mut execution.mailbox
            }
        };
        enqueue_message(mailbox, source, content)
    }

    /// Queues a lifecycle notice for the main agent. `send_message` refuses a terminal source
    /// because a finished execution must not keep talking, yet the completion notice itself is
    /// only available once the execution is already terminal.
    pub fn notify_main(
        &self,
        id: TaskExecutionId,
        content: String,
    ) -> Result<(), TaskRegistryError> {
        if content.is_empty() || content.len() > MAX_TASK_MESSAGE_BYTES {
            return Err(TaskRegistryError::MessageLimit);
        }

        let mut registry = self
            .inner
            .lock()
            .map_err(|_| TaskRegistryError::UnknownExecution)?;
        if !registry.executions.contains_key(&id) {
            return Err(TaskRegistryError::UnknownExecution);
        }

        enqueue_message(
            &mut registry.main_mailbox,
            TaskMessageSource::Execution(id),
            content,
        )
    }

    pub fn drain_messages(&self, target: TaskMessageTarget) -> Vec<TaskMessage> {
        let Ok(mut registry) = self.inner.lock() else {
            return Vec::new();
        };
        let mailbox = match target {
            TaskMessageTarget::Main => &mut registry.main_mailbox,
            TaskMessageTarget::Execution(id) => {
                let Some(execution) = registry.executions.get_mut(&id) else {
                    return Vec::new();
                };
                &mut execution.mailbox
            }
        };
        mailbox.bytes = 0;
        mailbox.messages.drain(..).collect()
    }

    pub fn wait_for_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let idle = self.inner.lock().is_ok_and(|registry| {
                registry
                    .executions
                    .values()
                    .all(|execution| execution.lifecycle.terminal().is_some())
            });
            if idle {
                self.join_finished_workers();
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(TASK_RESULT_POLL_INTERVAL);
        }
    }

    pub fn cancel_all(&self) {
        if let Ok(registry) = self.inner.lock() {
            for execution in registry.executions.values() {
                if execution.lifecycle.terminal().is_none() {
                    execution.cancellation.store(true, Ordering::Release);
                }
            }
        }
    }

    fn set_worker(&self, id: TaskExecutionId, worker: thread::JoinHandle<()>) {
        if let Ok(mut registry) = self.inner.lock()
            && let Some(execution) = registry.executions.get_mut(&id)
        {
            execution.worker = Some(worker);
        }
    }

    fn join_finished_workers(&self) {
        let workers = self
            .inner
            .lock()
            .map(|mut registry| {
                registry
                    .executions
                    .values_mut()
                    .filter(|execution| execution.lifecycle.terminal().is_some())
                    .filter_map(|execution| execution.worker.take())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for worker in workers {
            let _ = worker.join();
        }
    }
}

#[derive(Clone)]
pub struct TaskExecutionLifecycle {
    inner: Arc<Mutex<TaskExecutionLifecycleState>>,
}

struct TaskExecutionLifecycleState {
    id: TaskExecutionId,
    mode: TaskLaunchMode,
    terminal: Option<TaskTerminalState>,
    events: Vec<TaskExecutionEvent>,
}

impl TaskExecutionLifecycle {
    fn new(id: TaskExecutionId, mode: TaskLaunchMode) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TaskExecutionLifecycleState {
                id,
                mode,
                terminal: None,
                events: vec![TaskExecutionEvent::Admitted(id, mode)],
            })),
        }
    }

    pub fn id(&self) -> TaskExecutionId {
        self.inner.lock().expect("task lifecycle lock poisoned").id
    }

    pub fn mode(&self) -> TaskLaunchMode {
        self.inner
            .lock()
            .expect("task lifecycle lock poisoned")
            .mode
    }

    pub fn events(&self) -> Vec<TaskExecutionEvent> {
        self.inner
            .lock()
            .expect("task lifecycle lock poisoned")
            .events
            .clone()
    }

    pub fn terminal(&self) -> Option<TaskTerminalState> {
        self.inner
            .lock()
            .expect("task lifecycle lock poisoned")
            .terminal
    }

    pub fn transition_to_background(&self) -> bool {
        let mut lifecycle = self.inner.lock().expect("task lifecycle lock poisoned");
        if lifecycle.mode != TaskLaunchMode::Foreground || lifecycle.terminal.is_some() {
            return false;
        }

        let id = lifecycle.id;
        lifecycle.mode = TaskLaunchMode::Background;
        lifecycle.events.push(TaskExecutionEvent::Backgrounded(id));
        true
    }

    pub fn finish(&self, terminal: TaskTerminalState) -> bool {
        let mut lifecycle = self.inner.lock().expect("task lifecycle lock poisoned");
        if lifecycle.terminal.is_some() {
            return false;
        }

        let id = lifecycle.id;
        lifecycle.terminal = Some(terminal);
        lifecycle.events.push(match terminal {
            TaskTerminalState::Completed => TaskExecutionEvent::Completed(id),
            TaskTerminalState::Failed => TaskExecutionEvent::Failed(id),
            TaskTerminalState::Cancelled => TaskExecutionEvent::Cancelled(id),
        });
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskInvocation {
    agent: Option<String>,
    model: Option<String>,
    skills: Option<Vec<String>>,
    background: bool,
    description: String,
}

impl TaskInvocation {
    pub fn from_value(value: Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or("task arguments must be an object")?;
        if object.len() > 5
            || object.keys().any(|key| {
                key != "agent"
                    && key != "background"
                    && key != "description"
                    && key != "model"
                    && key != "skills"
            })
        {
            return Err("task arguments are invalid".into());
        }

        let agent = match object.get("agent") {
            Some(Value::String(value)) if is_bounded_name(value, MAX_TASK_SKILL_NAME_CHARS) => {
                Some(value.clone())
            }
            Some(_) => return Err("task agent is invalid".into()),
            None => None,
        };
        let model = match object.get("model") {
            Some(Value::String(value)) if is_bounded_name(value, MAX_TASK_MODEL_CHARS) => {
                Some(value.clone())
            }
            Some(_) => return Err("task model is invalid".into()),
            None => None,
        };
        let skills = match object.get("skills") {
            Some(Value::Array(values))
                if values.len() <= MAX_TASK_SKILLS
                    && values.iter().all(|value| {
                        value
                            .as_str()
                            .is_some_and(|name| is_bounded_name(name, MAX_TASK_SKILL_NAME_CHARS))
                    })
                    && values
                        .iter()
                        .map(|value| value.as_str().expect("validated task skill"))
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        == values.len() =>
            {
                Some(
                    values
                        .iter()
                        .map(|value| value.as_str().expect("validated task skill").to_owned())
                        .collect(),
                )
            }
            Some(_) => return Err("task skills are invalid".into()),
            None => None,
        };
        let background = match object.get("background") {
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err("task background is invalid".into()),
            None => false,
        };
        let description = object
            .get("description")
            .and_then(Value::as_str)
            .filter(|value| {
                !value.is_empty() && value.chars().count() <= MAX_TASK_DESCRIPTION_CHARS
            })
            .ok_or("task description is invalid")?
            .to_owned();

        Ok(Self {
            agent,
            model,
            skills,
            background,
            description,
        })
    }
}

fn is_bounded_name(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.chars().count() <= limit
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSkill {
    name: String,
    description: String,
    instructions: String,
}

impl TaskSkill {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskTurnRequest {
    agent_name: String,
    agent_description: String,
    system_prompt: String,
    model: String,
    request_config: RequestConfig,
    skills: Vec<TaskSkill>,
    description: String,
}

impl TaskTurnRequest {
    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }

    pub fn agent_description(&self) -> &str {
        &self.agent_description
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub const fn request_config(&self) -> &RequestConfig {
        &self.request_config
    }

    pub fn skills(&self) -> &[TaskSkill] {
        &self.skills
    }

    pub fn description(&self) -> &str {
        &self.description
    }
}

pub struct TaskTurnResult {
    pub output: String,
    pub iterations: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskRunnerError {
    Cancelled,
    TimedOut,
    ProviderFailure,
    IterationLimit,
    ChildFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskModelResolutionError {
    ModelUnavailable,
}

#[derive(Clone)]
pub struct TaskRunContext {
    pub cancellation: Arc<AtomicBool>,
    execution: Option<TaskExecutionLifecycle>,
    registry: TaskExecutionRegistry,
    before_publication: BeforePublicationHook,
}

impl TaskRunContext {
    fn new(
        cancellation: Arc<AtomicBool>,
        execution: TaskExecutionLifecycle,
        registry: TaskExecutionRegistry,
    ) -> Self {
        Self {
            cancellation,
            execution: Some(execution),
            registry,
            before_publication: Arc::new(Mutex::new(None)),
        }
    }

    pub fn execution(&self) -> Option<&TaskExecutionLifecycle> {
        self.execution.as_ref()
    }

    pub fn execution_registry(&self) -> &TaskExecutionRegistry {
        &self.registry
    }

    pub fn drain_messages(&self) -> Vec<TaskMessage> {
        self.execution
            .as_ref()
            .map(|execution| {
                self.registry
                    .drain_messages(TaskMessageTarget::Execution(execution.id()))
            })
            .unwrap_or_default()
    }

    pub fn set_before_publication_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .before_publication
            .lock()
            .expect("task publication hook lock poisoned") = Some(Box::new(hook));
    }

    fn run_before_publication_hook(&self) {
        if let Some(hook) = self
            .before_publication
            .lock()
            .expect("task publication hook lock poisoned")
            .take()
        {
            hook();
        }
    }

    fn terminal_output(&self) -> Option<ToolOutput> {
        if self.cancellation.load(Ordering::Acquire) {
            return Some(task_terminal(HeadlessTaskTerminal::Cancelled));
        }
        None
    }
}

pub trait TaskRunner: Send + Sync + 'static {
    fn execution_registry(&self) -> Option<TaskExecutionRegistry> {
        None
    }

    fn run(
        &self,
        request: TaskTurnRequest,
        context: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError>;
}

pub struct TaskTool<R> {
    agents: AgentCatalog,
    skills: SkillCatalog,
    parent_model: String,
    parent_request_config: RequestConfig,
    available_models: Vec<String>,
    model_validator: Arc<dyn AgentModelValidator + Send + Sync>,
    model_resolution_diagnostics: Option<ModelResolutionDiagnostics>,
    runner: Arc<R>,
    registry: TaskExecutionRegistry,
}

impl<R> Clone for TaskTool<R> {
    fn clone(&self) -> Self {
        Self {
            agents: self.agents.clone(),
            skills: self.skills.clone(),
            parent_model: self.parent_model.clone(),
            parent_request_config: self.parent_request_config.clone(),
            available_models: self.available_models.clone(),
            model_validator: Arc::clone(&self.model_validator),
            model_resolution_diagnostics: self.model_resolution_diagnostics.clone(),
            runner: Arc::clone(&self.runner),
            registry: self.registry.clone(),
        }
    }
}

impl<R: TaskRunner> TaskTool<R> {
    pub fn from_catalogs_with_model_validator(
        agents: AgentCatalog,
        skills: SkillCatalog,
        parent_model: impl Into<String>,
        model_validator: impl AgentModelValidator + Send + Sync + 'static,
        runner: R,
    ) -> Self {
        Self::from_catalogs_with_parent_config(
            agents,
            skills,
            parent_model,
            RequestConfig::default(),
            Vec::new(),
            model_validator,
            runner,
        )
    }

    pub fn from_catalogs_with_parent_config(
        agents: AgentCatalog,
        skills: SkillCatalog,
        parent_model: impl Into<String>,
        parent_request_config: RequestConfig,
        available_models: Vec<String>,
        model_validator: impl AgentModelValidator + Send + Sync + 'static,
        runner: R,
    ) -> Self {
        let mut available_models = available_models
            .into_iter()
            .filter(|model| is_safe_model_identifier(model))
            .collect::<Vec<_>>();
        available_models.sort();
        available_models.dedup();
        available_models.truncate(MAX_TASK_MODEL_SCHEMA_ENTRIES);
        let registry = runner.execution_registry().unwrap_or_default();
        Self {
            agents,
            skills,
            parent_model: parent_model.into(),
            parent_request_config,
            available_models,
            model_validator: Arc::new(model_validator),
            model_resolution_diagnostics: None,
            runner: Arc::new(runner),
            registry,
        }
    }

    pub fn with_execution_registry(mut self, registry: TaskExecutionRegistry) -> Self {
        self.registry = registry;
        self
    }

    pub fn execution_registry(&self) -> &TaskExecutionRegistry {
        &self.registry
    }

    pub fn with_model_resolution_diagnostics(
        mut self,
        diagnostics: impl Fn(TaskModelResolutionError) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.model_resolution_diagnostics = Some(Arc::new(diagnostics));
        self
    }

    pub fn input_schema() -> Value {
        serde_json::json!({"type":"object","additionalProperties":false,"required":["description"],"properties":{"agent":{"type":"string","minLength":1,"maxLength":64},"background":{"type":"boolean"},"description":{"type":"string","minLength":1,"maxLength":16384},"model":{"type":"string","minLength":1,"maxLength":64},"skills":{"type":"array","maxItems":128,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":64}}}})
    }

    pub fn catalog_input_schema(&self) -> Value {
        let mut agents = self
            .agents
            .subagents()
            .filter(|agent| agent.mode == AgentMode::Subagent)
            .map(|agent| {
                (
                    agent.name.clone(),
                    sanitized_schema_description(&agent.description),
                )
            })
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| left.0.cmp(&right.0));

        let names = agents
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let descriptions = agents
            .iter()
            .map(|(name, description)| format!("- {name}: {description}"))
            .collect::<Vec<_>>()
            .join("\n");
        let agent_description = format!("Eligible subagents:\n{descriptions}");

        let mut schema = Self::input_schema();
        let agent = &mut schema["properties"]["agent"];
        agent["enum"] = Value::from(names);
        agent["description"] = Value::from(agent_description);
        schema["properties"]["model"]["enum"] = Value::from(self.available_models.clone());
        schema
    }

    fn resolve_agent(&self, requested: Option<&str>) -> Result<&AgentDefinition, ToolOutput> {
        requested
            .and_then(|name| self.agents.agent(name))
            .or_else(|| {
                requested
                    .is_none()
                    .then(|| {
                        self.agents
                            .subagents()
                            .filter(|agent| agent.mode == AgentMode::Subagent)
                            .min_by(|left, right| left.name.cmp(&right.name))
                    })
                    .flatten()
            })
            .filter(|agent| agent.mode == AgentMode::Subagent)
            .ok_or_else(|| task_terminal(HeadlessTaskTerminal::AgentUnavailable))
    }

    fn resolve(&self, invocation: TaskInvocation) -> Result<TaskTurnRequest, ToolOutput> {
        let agent = self.resolve_agent(invocation.agent.as_deref())?;

        let explicit_model = invocation.model.or_else(|| agent.model.clone());
        let (model, request_config) = match explicit_model {
            Some(model) => {
                if self.model_validator.validate_model(&model).is_err() {
                    return Err(self.model_unavailable_output());
                }
                let request_config = if model == self.parent_model {
                    self.parent_request_config.clone()
                } else {
                    RequestConfig::default()
                };
                (model, request_config)
            }
            None => (
                self.parent_model.clone(),
                self.parent_request_config.clone(),
            ),
        };

        let skills = self.resolve_skills(agent, invocation.skills.as_deref())?;
        Ok(TaskTurnRequest {
            agent_name: agent.name.clone(),
            agent_description: agent.description.clone(),
            system_prompt: agent.system_prompt.clone(),
            model,
            request_config,
            skills,
            description: invocation.description,
        })
    }

    fn model_unavailable_output(&self) -> ToolOutput {
        let mut output = task_terminal(HeadlessTaskTerminal::ModelUnavailable);
        let reference = self
            .model_resolution_diagnostics
            .as_ref()
            .and_then(|diagnostics| diagnostics(TaskModelResolutionError::ModelUnavailable))
            .filter(|reference| is_diagnostic_reference(reference));
        if let Some(reference) = reference {
            output.content.push_str(" [ref: ");
            output.content.push_str(&reference);
            output.content.push(']');
        }
        output
    }

    fn resolve_skills(
        &self,
        agent: &AgentDefinition,
        requested: Option<&[String]>,
    ) -> Result<Vec<TaskSkill>, ToolOutput> {
        let names = requested.unwrap_or(&agent.skills);
        if !names.iter().all(|name| agent.skills.contains(name)) {
            return Err(task_terminal(HeadlessTaskTerminal::SkillUnavailable));
        }

        names
            .iter()
            .map(|name| {
                let skill = self
                    .skills
                    .skill(name)
                    .ok_or_else(|| task_terminal(HeadlessTaskTerminal::SkillUnavailable))?;
                let instructions = skill
                    .load_instructions()
                    .map_err(|_| task_terminal(HeadlessTaskTerminal::SkillUnavailable))?;
                Ok(TaskSkill {
                    name: skill.name().to_owned(),
                    description: skill.description().to_owned(),
                    instructions,
                })
            })
            .collect()
    }
}

fn sanitized_schema_description(description: &str) -> String {
    let normalized = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = normalized.to_ascii_lowercase();
    if ["api_key", "authorization", "password", "secret", "token"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return "[redacted]".into();
    }

    normalized
        .chars()
        .take(MAX_TASK_AGENT_SCHEMA_DESCRIPTION_CHARS)
        .collect()
}

impl<R: TaskRunner> DispatchTool for TaskTool<R> {
    fn permission_target(&self, arguments: &Value) -> Result<String, Error> {
        let invocation = TaskInvocation::from_value(arguments.clone())
            .map_err(|_| Error::Tool("task arguments are invalid".into()))?;
        self.resolve_agent(invocation.agent.as_deref())
            .map(|agent| agent.name.clone())
            .map_err(|_| Error::Tool("task: requested agent is unavailable".into()))
    }

    fn execute(
        &mut self,
        parent: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, Error> {
        let mode = TaskInvocation::from_value(arguments.clone())
            .map(|invocation| {
                if invocation.background {
                    TaskLaunchMode::Background
                } else {
                    TaskLaunchMode::Foreground
                }
            })
            .unwrap_or(TaskLaunchMode::Foreground);
        self.execute_with_launch_mode(parent, arguments, mode)
    }
}

impl<R: TaskRunner> TaskTool<R> {
    pub fn execute_with_launch_mode(
        &mut self,
        parent: &ToolExecutionContext,
        arguments: Value,
        mode: TaskLaunchMode,
    ) -> Result<ToolOutput, Error> {
        let invocation = match TaskInvocation::from_value(arguments) {
            Ok(invocation) => invocation,
            Err(_) => return Ok(task_terminal(HeadlessTaskTerminal::InputLimit)),
        };
        if parent.is_cancelled() {
            return Ok(task_terminal(HeadlessTaskTerminal::Cancelled));
        }
        let request = match self.resolve(invocation) {
            Ok(request) => request,
            Err(output) => return Ok(output),
        };
        let Some(execution_id) = self.registry.admit(mode) else {
            return Ok(task_terminal(HeadlessTaskTerminal::ConcurrencyLimit));
        };
        let lifecycle = self
            .registry
            .lifecycle(execution_id)
            .expect("admitted task lifecycle");
        let cancellation = self
            .registry
            .cancellation_handle(execution_id)
            .expect("admitted task cancellation");
        let context = TaskRunContext::new(cancellation, lifecycle, self.registry.clone());
        let (sender, receiver) = mpsc::channel();
        let runner = Arc::clone(&self.runner);
        let registry = self.registry.clone();
        let worker_context = context.clone();
        let worker = thread::spawn(move || {
            let mut output = {
                let _panic_hook = TaskPanicHookGuard::new();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    if let Some(output) = worker_context.terminal_output() {
                        return Ok(output);
                    }
                    let result = runner.run(request, &worker_context)?;
                    Ok(task_result_output(result, &worker_context))
                }))
                .unwrap_or(Err(TaskRunnerError::ChildFailure));
                result.unwrap_or_else(task_error_output)
            };

            worker_context.run_before_publication_hook();
            if let Some(cancelled) = worker_context.terminal_output() {
                output = cancelled;
            }
            registry.finish(execution_id, task_terminal_state(&output), output.clone());
            let _ = sender.send(output);
        });
        self.registry.set_worker(execution_id, worker);

        if mode == TaskLaunchMode::Background {
            return Ok(background_output(execution_id));
        }

        loop {
            if parent.is_cancelled() {
                self.registry.cancel(execution_id);
                return Ok(task_terminal(HeadlessTaskTerminal::Cancelled));
            }
            if self
                .registry
                .lifecycle(execution_id)
                .is_some_and(|lifecycle| lifecycle.mode() == TaskLaunchMode::Background)
            {
                return Ok(background_output(execution_id));
            }

            match receiver.recv_timeout(TASK_RESULT_POLL_INTERVAL) {
                Ok(output) => return Ok(output),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let output = task_terminal(HeadlessTaskTerminal::ChildFailure);
                    self.registry
                        .finish(execution_id, TaskTerminalState::Failed, output.clone());
                    return Ok(output);
                }
            }
        }
    }
}

fn task_terminal_state(output: &ToolOutput) -> TaskTerminalState {
    match output.terminal() {
        Some(HeadlessTaskTerminal::Cancelled) => TaskTerminalState::Cancelled,
        Some(_) => TaskTerminalState::Failed,
        None => TaskTerminalState::Completed,
    }
}

fn task_result_output(result: TaskTurnResult, context: &TaskRunContext) -> ToolOutput {
    if let Some(output) = context.terminal_output() {
        return output;
    }
    if result.iterations > MAX_TASK_ITERATIONS {
        return task_terminal(HeadlessTaskTerminal::IterationLimit);
    }
    if result.output.chars().count() > MAX_TASK_OUTPUT_CHARS {
        return task_terminal(HeadlessTaskTerminal::OutputLimit);
    }
    ToolOutput::success(result.output)
}

fn background_output(id: TaskExecutionId) -> ToolOutput {
    ToolOutput::success(format!("Subagent #{} running in background", id.value()))
}

fn task_error_output(error: TaskRunnerError) -> ToolOutput {
    match error {
        TaskRunnerError::Cancelled => task_terminal(HeadlessTaskTerminal::Cancelled),
        TaskRunnerError::TimedOut => task_terminal(HeadlessTaskTerminal::TimedOut),
        TaskRunnerError::ProviderFailure => task_terminal(HeadlessTaskTerminal::ProviderFailure),
        TaskRunnerError::IterationLimit => task_terminal(HeadlessTaskTerminal::IterationLimit),
        TaskRunnerError::ChildFailure => task_terminal(HeadlessTaskTerminal::ChildFailure),
    }
}

struct TaskPanicHookGuard;

impl TaskPanicHookGuard {
    fn new() -> Self {
        install_subagent_panic_hook();
        IS_SUBAGENT_WORKER.with(|is_worker| is_worker.set(true));
        Self
    }
}

impl Drop for TaskPanicHookGuard {
    fn drop(&mut self) {
        IS_SUBAGENT_WORKER.with(|is_worker| is_worker.set(false));
    }
}

fn task_terminal(terminal: HeadlessTaskTerminal) -> ToolOutput {
    ToolOutput::task_terminal(terminal)
}

fn is_diagnostic_reference(reference: &str) -> bool {
    reference.len() == 8
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn is_safe_model_identifier(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= MAX_TASK_MODEL_CHARS
        && model.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}
