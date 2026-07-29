//! Isolated child-turn execution for subagents: builds the provider for the
//! configured backend, runs a single turn to completion under a read-only or
//! dangerous-mode tool set, and reports a sanitized, provider-agnostic error.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_core::{
    HeadlessPermissionResolver, HeadlessToolCall, HeadlessTurnCancellation, HeadlessTurnError,
    HeadlessTurnPortError, Message, MessagePart, PermissionDecision, PermissionMode,
    PermissionPattern, PermissionPolicy, PermissionRule, PermissionSession, Role, TurnEvent,
    TurnProgressSink, TurnProvider, run_headless_turn_with_max_iterations_and_progress,
};
use agens_providers::{
    ChatGptResponsesProvider, OpenAiResponsesProvider, ProgressAwareProvider,
    ProviderDiagnosticClass, ProviderDiagnosticEvent, ProviderDiagnosticScope, ProviderDiagnostics,
};
use agens_tools::{
    TaskExecutionRegistry, TaskMessageSource, TaskMessageTarget, TaskRunnerError, TaskTurnRequest,
};

use crate::block_on_headless_turn;
use crate::runtime::production_child_tool_runtime;
use agens_bootstrap::Bootstrap;
use agens_core::DiscardCompletedTurnRepository;
use agens_core::SubagentErrorKind;
use agens_diagnostics::diagnostic_store;
use agens_dispatch::ProductionToolDispatcher;
use agens_permissions::{ProductionPermissionGate, SharedToolDispatcher};

#[derive(Clone, Copy)]
pub enum ChildRunError {
    Authentication,
    Cancelled,
    Context,
    Network,
    TimedOut,
    Provider,
    Protocol,
    RateLimited,
    Rejected,
    Server,
    Tool,
    IterationLimit,
    Runtime,
}

impl ChildRunError {
    pub const fn diagnostic_class(self) -> ProviderDiagnosticClass {
        match self {
            Self::Authentication => ProviderDiagnosticClass::Authentication,
            Self::Cancelled => ProviderDiagnosticClass::Cancelled,
            Self::Context => ProviderDiagnosticClass::Context,
            Self::Network => ProviderDiagnosticClass::Network,
            Self::TimedOut => ProviderDiagnosticClass::Deadline,
            Self::Provider => ProviderDiagnosticClass::Provider,
            Self::Protocol => ProviderDiagnosticClass::Protocol,
            Self::RateLimited => ProviderDiagnosticClass::RateLimited,
            Self::Rejected => ProviderDiagnosticClass::Rejected,
            Self::Server => ProviderDiagnosticClass::Server,
            Self::Tool => ProviderDiagnosticClass::Tool,
            Self::IterationLimit | Self::Runtime => ProviderDiagnosticClass::Runtime,
        }
    }

    pub const fn tui_kind(self) -> Option<SubagentErrorKind> {
        match self {
            Self::Cancelled | Self::TimedOut => None,
            Self::Authentication => Some(SubagentErrorKind::Authentication),
            Self::Context => Some(SubagentErrorKind::Context),
            Self::Network => Some(SubagentErrorKind::Network),
            Self::Provider => Some(SubagentErrorKind::Provider),
            Self::Protocol => Some(SubagentErrorKind::Protocol),
            Self::RateLimited => Some(SubagentErrorKind::RateLimited),
            Self::Rejected => Some(SubagentErrorKind::Rejected),
            Self::Server => Some(SubagentErrorKind::Server),
            Self::Tool => Some(SubagentErrorKind::Tool),
            Self::IterationLimit | Self::Runtime => Some(SubagentErrorKind::Runtime),
        }
    }

    pub const fn task_runner_error(self) -> TaskRunnerError {
        match self {
            Self::Cancelled => TaskRunnerError::Cancelled,
            Self::TimedOut => TaskRunnerError::TimedOut,
            Self::Authentication
            | Self::Context
            | Self::Network
            | Self::Provider
            | Self::Protocol
            | Self::RateLimited
            | Self::Rejected
            | Self::Server => TaskRunnerError::ProviderFailure,
            Self::Tool | Self::Runtime => TaskRunnerError::ChildFailure,
            Self::IterationLimit => TaskRunnerError::IterationLimit,
        }
    }
}

fn child_run_error(error: HeadlessTurnError) -> ChildRunError {
    match error {
        HeadlessTurnError::Authentication => ChildRunError::Authentication,
        HeadlessTurnError::Cancelled => ChildRunError::Cancelled,
        HeadlessTurnError::ProviderContext => ChildRunError::Context,
        HeadlessTurnError::ProviderNetwork => ChildRunError::Network,
        HeadlessTurnError::TimedOut => ChildRunError::TimedOut,
        HeadlessTurnError::Provider => ChildRunError::Provider,
        HeadlessTurnError::ProviderProtocol => ChildRunError::Protocol,
        HeadlessTurnError::ProviderRateLimited => ChildRunError::RateLimited,
        HeadlessTurnError::ProviderRejected => ChildRunError::Rejected,
        HeadlessTurnError::ProviderServer => ChildRunError::Server,
        HeadlessTurnError::Tool => ChildRunError::Tool,
        HeadlessTurnError::MaxIterations => ChildRunError::IterationLimit,
        _ => ChildRunError::Runtime,
    }
}

pub struct ProductionTaskExecutionContext<'a> {
    pub bootstrap: &'a Bootstrap,
    pub project_root: &'a Path,
    pub dangerous_mode: bool,
    pub cancellation: &'a HeadlessTurnCancellation,
    pub progress: Option<&'a TurnProgressSink>,
    pub diagnostic_reference: &'a str,
    pub task_registry: &'a TaskExecutionRegistry,
    pub execution_id: agens_tools::TaskExecutionId,
}

pub fn run_production_task(
    request: TaskTurnRequest,
    context: ProductionTaskExecutionContext<'_>,
) -> Result<String, ChildRunError> {
    let ProductionTaskExecutionContext {
        bootstrap,
        project_root,
        dangerous_mode,
        cancellation,
        progress,
        diagnostic_reference,
        task_registry,
        execution_id,
    } = context;
    let messages = vec![
        Message {
            role: Role::System,
            parts: vec![MessagePart::Text(task_system_prompt(&request))],
        },
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text(request.description().to_owned())],
        },
    ];
    let (provider_tools, tool_runtime) = production_child_tool_runtime(
        project_root,
        bootstrap.tool_limits(),
        dangerous_mode,
        task_registry.clone(),
        execution_id,
    )
    .map_err(|_| ChildRunError::Runtime)?;
    let diagnostic_store = diagnostic_store(bootstrap);
    let diagnostic_sink = Arc::new(move |event: ProviderDiagnosticEvent| {
        diagnostic_store.record(&event);
    });
    let provider_diagnostics = ProviderDiagnostics::new(
        diagnostic_reference.to_owned(),
        ProviderDiagnosticScope::Subagent,
        diagnostic_sink,
    )
    .map_err(|_| ChildRunError::Runtime)?;

    match bootstrap.provider_type() {
        Some("openai-api") => {
            let api_key = bootstrap.api_key.clone().ok_or(ChildRunError::Runtime)?;
            let base_url = task_provider_base_url(bootstrap, project_root)
                .map_err(|_| ChildRunError::Runtime)?;
            let provider =
                OpenAiResponsesProvider::from_api_key_with_messages_and_tools_and_timeout(
                    api_key,
                    base_url.as_deref(),
                    request.model().to_owned(),
                    messages,
                    provider_tools,
                    std::time::Duration::from_secs(120),
                )
                .map(|provider| {
                    provider
                        .with_parallel_tool_calls(bootstrap.parallel_tool_calls)
                        .with_request_config(request.request_config().clone())
                        .with_diagnostics(provider_diagnostics)
                })
                .map_err(|_| ChildRunError::Runtime)?;
            run_isolated_task_turn(
                provider,
                tool_runtime,
                project_root,
                dangerous_mode,
                cancellation,
                progress,
                TaskMailboxContext {
                    registry: task_registry.clone(),
                    target: TaskMessageTarget::Execution(execution_id),
                },
            )
        }
        Some("openai-chatgpt") => {
            let base_url = task_provider_base_url(bootstrap, project_root)
                .map_err(|_| ChildRunError::Runtime)?;
            let provider = ChatGptResponsesProvider::from_credentials_with_messages_and_tools_and_timeout_and_auth_url(
                &bootstrap.paths.credentials,
                base_url.as_deref(),
                None,
                request.model().to_owned(),
                task_system_prompt(&request),
                messages,
                provider_tools,
                std::time::Duration::from_secs(120),
            )
            .map(|provider| {
                provider
                    .with_parallel_tool_calls(bootstrap.parallel_tool_calls)
                    .with_request_config(request.request_config().clone())
                    .with_diagnostics(provider_diagnostics)
            })
            .map_err(|_| ChildRunError::Runtime)?;
            run_isolated_task_turn(
                provider,
                tool_runtime,
                project_root,
                dangerous_mode,
                cancellation,
                progress,
                TaskMailboxContext {
                    registry: task_registry.clone(),
                    target: TaskMessageTarget::Execution(execution_id),
                },
            )
        }
        _ => Err(ChildRunError::Runtime),
    }
}

fn task_system_prompt(request: &TaskTurnRequest) -> String {
    request
        .skills()
        .iter()
        .fold(request.system_prompt().to_owned(), |prompt, skill| {
            format!("{prompt}\n\n## {}\n{}", skill.name(), skill.instructions())
        })
}

/// The provider endpoint a subagent's turn must send its conversation to, re-derived from the
/// subagent's own recorded root rather than `bootstrap`'s process-captured `provider.base_url` —
/// a wrong root's project configuration here would silently redirect this subagent's traffic to
/// an endpoint the operator only ever configured for a different project.
pub fn task_provider_base_url(
    bootstrap: &Bootstrap,
    project_root: &Path,
) -> Result<Option<String>, agens_error::CliError> {
    let session_root =
        agens_bootstrap::session_root::SessionRoot::confined_to(project_root.to_path_buf());
    let session_config =
        agens_bootstrap::session_config::SessionConfig::resolve(&session_root, bootstrap)?;
    Ok(session_config.provider_base_url().map(ToOwned::to_owned))
}

pub struct TaskMailboxProvider<P> {
    inner: P,
    registry: Option<TaskExecutionRegistry>,
    target: TaskMessageTarget,
}

impl<P> TaskMailboxProvider<P> {
    pub fn new(
        inner: P,
        registry: Option<TaskExecutionRegistry>,
        target: TaskMessageTarget,
    ) -> Self {
        Self {
            inner,
            registry,
            target,
        }
    }
}

impl<P: TurnProvider + Send> TurnProvider for TaskMailboxProvider<P> {
    fn queue_user_messages(&mut self, messages: Vec<Message>) -> Result<(), HeadlessTurnPortError> {
        self.inner.queue_user_messages(messages)
    }

    async fn next_parts(
        &mut self,
        events: &[TurnEvent],
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
        let messages = self
            .registry
            .as_ref()
            .map(|registry| registry.drain_messages(self.target))
            .unwrap_or_default()
            .into_iter()
            .map(|message| Message {
                role: Role::User,
                parts: vec![MessagePart::Text(format!(
                    "[coordination source={} untrusted=true]\n{}",
                    task_message_source_label(message.source()),
                    message.content(),
                ))],
            })
            .collect::<Vec<_>>();
        self.inner.queue_user_messages(messages)?;
        self.inner.next_parts(events, cancellation).await
    }
}

fn task_message_source_label(source: TaskMessageSource) -> String {
    match source {
        TaskMessageSource::Main => "main".into(),
        TaskMessageSource::User => "user".into(),
        TaskMessageSource::Execution(id) => format!("subagent:{}", id.value()),
    }
}

struct TaskMailboxContext {
    registry: TaskExecutionRegistry,
    target: TaskMessageTarget,
}

/// Runs a subagent's isolated turn with no session attempt of its own, so the
/// facts it emits carry no `session_id`/`attempt_id`.
fn run_isolated_task_turn<P>(
    provider: P,
    tool_runtime: SharedToolDispatcher,
    project_root: &Path,
    dangerous_mode: bool,
    cancellation: &HeadlessTurnCancellation,
    progress: Option<&TurnProgressSink>,
    mailbox: TaskMailboxContext,
) -> Result<String, ChildRunError>
where
    P: ProgressAwareProvider + Send,
{
    let mut provider = TaskMailboxProvider::new(provider, Some(mailbox.registry), mailbox.target);
    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        [
            "native::read",
            "native::task_control",
            "native::task_message",
        ]
        .into_iter()
        .map(|tool| {
            PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact(tool.into()),
                PermissionPattern::Any,
            )
        })
        .collect(),
    );
    let grants = Arc::new(Mutex::new(Vec::new()));
    let session = PermissionSession::new();
    let pending = Arc::new(Mutex::new(BTreeMap::new()));
    let prompts = Arc::new(Mutex::new(BTreeMap::new()));
    let mut repository = DiscardCompletedTurnRepository;
    let project = project_root.display().to_string();
    let mut gate = ProductionPermissionGate::new(
        policy.clone(),
        Arc::clone(&grants),
        session,
        project.clone(),
        Arc::clone(&tool_runtime),
        Arc::clone(&pending),
        Arc::clone(&prompts),
    )
    .with_dangerous_override(dangerous_mode);
    let mut resolver = ChildPermissionResolver;
    let mut dispatcher = ProductionToolDispatcher::new(tool_runtime, pending);
    let snapshot = block_on_headless_turn(run_headless_turn_with_max_iterations_and_progress(
        &mut provider,
        &mut gate,
        &mut resolver,
        &mut dispatcher,
        &mut repository,
        cancellation,
        16,
        progress,
        None,
    ))
    .map_err(|_| ChildRunError::Runtime)?
    .map_err(child_run_error)?;

    Ok(snapshot
        .events()
        .iter()
        .filter_map(|event| match event {
            TurnEvent::ProviderPart(MessagePart::Text(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect())
}

struct ChildPermissionResolver;

impl HeadlessPermissionResolver for ChildPermissionResolver {
    fn resolve(
        &mut self,
        _: &HeadlessToolCall,
        _: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send
    {
        std::future::ready(Ok(PermissionDecision::Deny))
    }
}
