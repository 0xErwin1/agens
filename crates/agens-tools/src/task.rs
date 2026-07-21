use agens_core::{AgentDefinition, AgentMode, Error, HeadlessTaskTerminal};
use serde_json::Value;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
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
const TASK_TIMEOUT: Duration = Duration::from_secs(30);
const TASK_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(5);
const OPEN: u8 = 0;
const CANCELLED: u8 = 1;
const PUBLISHED: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskExecutionId(u64);

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

#[derive(Clone)]
pub struct TaskRunContext {
    pub cancellation: Arc<std::sync::atomic::AtomicBool>,
    pub deadline: Instant,
    execution: Option<TaskExecutionLifecycle>,
    before_publication: Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>,
}

impl TaskRunContext {
    fn inherit(parent: &ToolExecutionContext) -> Self {
        Self {
            cancellation: parent.cancellation_handle(),
            deadline: parent.deadline().min(Instant::now() + TASK_TIMEOUT),
            execution: None,
            before_publication: Arc::new(Mutex::new(None)),
        }
    }

    fn with_execution(mut self, execution: TaskExecutionLifecycle) -> Self {
        self.execution = Some(execution);
        self
    }

    pub fn execution(&self) -> Option<&TaskExecutionLifecycle> {
        self.execution.as_ref()
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
        if Instant::now() >= self.deadline {
            return Some(task_terminal(HeadlessTaskTerminal::TimedOut));
        }
        None
    }
}

pub trait TaskRunner: Send + 'static {
    fn run(
        &mut self,
        request: TaskTurnRequest,
        context: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError>;
}

pub struct TaskTool<R> {
    agents: AgentCatalog,
    skills: SkillCatalog,
    parent_model: String,
    model_validator: Arc<dyn AgentModelValidator + Send + Sync>,
    runner: Arc<Mutex<R>>,
    active: Arc<AtomicUsize>,
    execution_ids: Arc<AtomicU64>,
}

impl<R> Clone for TaskTool<R> {
    fn clone(&self) -> Self {
        Self {
            agents: self.agents.clone(),
            skills: self.skills.clone(),
            parent_model: self.parent_model.clone(),
            model_validator: Arc::clone(&self.model_validator),
            runner: Arc::clone(&self.runner),
            active: Arc::clone(&self.active),
            execution_ids: Arc::clone(&self.execution_ids),
        }
    }
}

impl<R> TaskTool<R> {
    pub fn from_catalogs_with_model_validator(
        agents: AgentCatalog,
        skills: SkillCatalog,
        parent_model: impl Into<String>,
        model_validator: impl AgentModelValidator + Send + Sync + 'static,
        runner: R,
    ) -> Self {
        Self {
            agents,
            skills,
            parent_model: parent_model.into(),
            model_validator: Arc::new(model_validator),
            runner: Arc::new(Mutex::new(runner)),
            active: Arc::new(AtomicUsize::new(0)),
            execution_ids: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn input_schema() -> Value {
        serde_json::json!({"type":"object","additionalProperties":false,"required":["description"],"properties":{"agent":{"type":"string","minLength":1,"maxLength":64},"background":{"type":"boolean"},"description":{"type":"string","minLength":1,"maxLength":16384},"model":{"type":"string","minLength":1,"maxLength":64},"skills":{"type":"array","maxItems":128,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":64}}}})
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

        let model = invocation
            .model
            .or_else(|| agent.model.clone())
            .unwrap_or_else(|| self.parent_model.clone());
        if self.model_validator.validate_model(&model).is_err() {
            return Err(task_terminal(HeadlessTaskTerminal::ModelUnavailable));
        }

        let skills = self.resolve_skills(agent, invocation.skills.as_deref())?;
        Ok(TaskTurnRequest {
            agent_name: agent.name.clone(),
            agent_description: agent.description.clone(),
            system_prompt: agent.system_prompt.clone(),
            model,
            skills,
            description: invocation.description,
        })
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
        let context = TaskRunContext::inherit(parent);
        if let Some(output) = context.terminal_output() {
            return Ok(output);
        }
        let request = match self.resolve(invocation) {
            Ok(request) => request,
            Err(output) => return Ok(output),
        };
        let Some(permit) = TaskPermit::acquire(&self.active) else {
            return Ok(task_terminal(HeadlessTaskTerminal::ConcurrencyLimit));
        };
        if let Some(output) = context.terminal_output() {
            return Ok(output);
        }

        let execution_id = TaskExecutionId(self.execution_ids.fetch_add(1, Ordering::AcqRel) + 1);
        let lifecycle = TaskExecutionLifecycle::new(execution_id, mode);
        let context = context.with_execution(lifecycle);
        let publication = Arc::new(AtomicU8::new(OPEN));
        let (sender, receiver) = mpsc::channel();
        let runner = Arc::clone(&self.runner);
        let worker_context = context.clone();
        let worker_publication = Arc::clone(&publication);
        thread::spawn(move || {
            let _permit = permit;
            let output = {
                let _panic_hook = TaskPanicHookGuard::new();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let mut runner = runner.lock().map_err(|_| TaskRunnerError::ChildFailure)?;
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

            if worker_publication
                .compare_exchange(OPEN, PUBLISHED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                if let Some(lifecycle) = worker_context.execution() {
                    lifecycle.finish(task_terminal_state(&output));
                }
                let _ = sender.send(output);
            }
        });

        loop {
            if let Some(output) = context.terminal_output() {
                let output = finish_task_call(&publication, &receiver, output, context.execution());
                return Ok(output);
            }

            match receiver.recv_timeout(TASK_RESULT_POLL_INTERVAL) {
                Ok(output) if publication.load(Ordering::Acquire) == PUBLISHED => {
                    return Ok(output);
                }
                Ok(_) => return Ok(task_terminal(HeadlessTaskTerminal::ChildFailure)),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let output = finish_task_call(
                        &publication,
                        &receiver,
                        task_terminal(HeadlessTaskTerminal::ChildFailure),
                        context.execution(),
                    );
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

fn finish_task_call(
    publication: &AtomicU8,
    receiver: &mpsc::Receiver<ToolOutput>,
    terminal: ToolOutput,
    lifecycle: Option<&TaskExecutionLifecycle>,
) -> ToolOutput {
    match publication.compare_exchange(OPEN, CANCELLED, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            if let Some(lifecycle) = lifecycle {
                lifecycle.finish(task_terminal_state(&terminal));
            }
            terminal
        }
        Err(CANCELLED) => terminal,
        Err(PUBLISHED) => receiver
            .recv()
            .unwrap_or_else(|_| task_terminal(HeadlessTaskTerminal::ChildFailure)),
        Err(_) => task_terminal(HeadlessTaskTerminal::ChildFailure),
    }
}

fn task_terminal(terminal: HeadlessTaskTerminal) -> ToolOutput {
    ToolOutput::task_terminal(terminal)
}

struct TaskPermit {
    active: Arc<AtomicUsize>,
}

impl TaskPermit {
    fn acquire(active: &Arc<AtomicUsize>) -> Option<Self> {
        let mut current = active.load(Ordering::Acquire);
        loop {
            if current >= MAX_TASK_CONCURRENCY {
                return None;
            }
            match active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Self {
                        active: Arc::clone(active),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for TaskPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_worker_without_publication_cancels_and_cannot_publish_late_value() {
        let publication = AtomicU8::new(OPEN);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || drop(sender)).join().unwrap();

        assert_eq!(
            receiver.recv_timeout(TASK_RESULT_POLL_INTERVAL),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );

        assert_eq!(
            finish_task_call(
                &publication,
                &receiver,
                task_terminal(HeadlessTaskTerminal::ChildFailure),
                None,
            ),
            task_terminal(HeadlessTaskTerminal::ChildFailure)
        );
        assert_eq!(publication.load(Ordering::Acquire), CANCELLED);
        assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }
}
