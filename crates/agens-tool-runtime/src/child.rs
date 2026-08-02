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
    ChatGptResponsesProvider, MoonshotProvider, OpenAiResponsesProvider, ProgressAwareProvider,
    ProviderDiagnosticClass, ProviderDiagnosticEvent, ProviderDiagnosticScope, ProviderDiagnostics,
};
use agens_tools::{
    TaskExecutionRegistry, TaskMessageSource, TaskMessageTarget, TaskProviderFailure,
    TaskRunnerError, TaskTurnRequest,
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
            Self::IterationLimit => Some(SubagentErrorKind::IterationLimit),
            Self::Runtime => Some(SubagentErrorKind::Runtime),
        }
    }

    pub const fn task_runner_error(self) -> TaskRunnerError {
        match self {
            Self::Cancelled => TaskRunnerError::Cancelled,
            Self::TimedOut => TaskRunnerError::TimedOut,
            Self::Authentication => {
                TaskRunnerError::ProviderFailure(TaskProviderFailure::Authentication)
            }
            Self::Context => TaskRunnerError::ProviderFailure(TaskProviderFailure::Context),
            Self::Network => TaskRunnerError::ProviderFailure(TaskProviderFailure::Network),
            Self::Provider | Self::Protocol => {
                TaskRunnerError::ProviderFailure(TaskProviderFailure::Protocol)
            }
            Self::RateLimited => TaskRunnerError::ProviderFailure(TaskProviderFailure::RateLimited),
            Self::Rejected => TaskRunnerError::ProviderFailure(TaskProviderFailure::Rejected),
            Self::Server => TaskRunnerError::ProviderFailure(TaskProviderFailure::Server),
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
        Some("moonshotai") => {
            let api_key = bootstrap.api_key.clone().ok_or(ChildRunError::Runtime)?;
            let base_url = task_provider_base_url(bootstrap, project_root)
                .map_err(|_| ChildRunError::Runtime)?;
            let provider = MoonshotProvider::from_api_key_with_messages_and_tools_and_timeout(
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
fn configured_task_max_iterations(registry: &TaskExecutionRegistry) -> usize {
    registry.limits().max_iterations
}

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
    let max_iterations = configured_task_max_iterations(&mailbox.registry);
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
        max_iterations,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the subagent-scope split (`PermissionSession::new()` above, never
    /// `with_temporary_bypass()`): a model-launched child's own resolver fails closed on every
    /// `Ask`, unconditionally and regardless of which tool or arguments produced it. A future edit
    /// that forwards a session's bypass into `run_production_task` (see `ProductionTaskRunner`'s
    /// doc comment on `with_bypass`) must not make this resolver return anything but `Deny`.
    #[test]
    fn isolated_turn_stops_at_the_task_registrys_configured_iteration_limit() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct IteratingProvider {
            requests: Arc<AtomicUsize>,
        }

        impl TurnProvider for IteratingProvider {
            fn next_parts(
                &mut self,
                _events: &[TurnEvent],
                _cancellation: &HeadlessTurnCancellation,
            ) -> impl std::future::Future<Output = Result<Vec<MessagePart>, HeadlessTurnPortError>> + Send
            {
                let request = self.requests.fetch_add(1, Ordering::SeqCst) + 1;
                std::future::ready(Ok(vec![MessagePart::ToolCall {
                    id: format!("read-{request}"),
                    name: "native::read".into(),
                    input: r#"{"path":"notes.md"}"#.into(),
                }]))
            }
        }

        impl ProgressAwareProvider for IteratingProvider {
            fn with_progress_sink(self, _progress: TurnProgressSink) -> Self {
                self
            }
        }

        struct ReadTool;

        impl agens_tools::DispatchTool for ReadTool {
            fn permission_target(
                &self,
                arguments: &serde_json::Value,
            ) -> Result<String, agens_core::Error> {
                arguments
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| agens_core::Error::Tool("invalid read arguments".into()))
            }

            fn execute(
                &mut self,
                _context: &agens_tools::ToolExecutionContext,
                _arguments: serde_json::Value,
            ) -> Result<agens_tools::ToolOutput, agens_core::Error> {
                Ok(agens_tools::ToolOutput::success("read"))
            }
        }

        let registry = TaskExecutionRegistry::with_limits(agens_tools::TaskExecutionLimits {
            max_iterations: 3,
            max_concurrency: 1,
            max_output_chars: 1_024,
        });
        let requests = Arc::new(AtomicUsize::new(0));
        let mut dispatcher = agens_tools::ToolDispatcher::new();
        dispatcher
            .register_native("native::read", agens_core::ToolAccess::ReadOnly, ReadTool)
            .unwrap();

        let result = run_isolated_task_turn(
            IteratingProvider {
                requests: Arc::clone(&requests),
            },
            Arc::new(Mutex::new(dispatcher)),
            Path::new("."),
            false,
            &HeadlessTurnCancellation::new(),
            None,
            TaskMailboxContext {
                registry,
                target: TaskMessageTarget::Main,
            },
        );

        assert!(matches!(result, Err(ChildRunError::IterationLimit)));
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn child_permission_resolver_fails_closed_on_every_ask_unconditionally() {
        let cancellation = HeadlessTurnCancellation::new();
        for call in [
            HeadlessToolCall {
                id: "call-1".into(),
                name: "native::write".into(),
                input: r#"{"path":"a.txt","content":"x"}"#.into(),
            },
            HeadlessToolCall {
                id: "call-2".into(),
                name: "native::task".into(),
                input: r#"{"agent":"reviewer","description":"probe"}"#.into(),
            },
            HeadlessToolCall {
                id: "call-3".into(),
                name: "native::bash".into(),
                input: "{}".into(),
            },
        ] {
            let mut resolver = ChildPermissionResolver;
            let decision = block_on_headless_turn(resolver.resolve(&call, &cancellation))
                .unwrap()
                .unwrap();
            assert_eq!(
                decision,
                PermissionDecision::Deny,
                "a model-launched child must fail closed on Ask for {}, with no bypass path \
                 available to it",
                call.name
            );
        }
    }

    #[test]
    fn a_call_to_a_tool_absent_from_the_child_catalog_is_denied_honestly_with_facts() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct UnregisteredToolProbeProvider {
            calls: AtomicUsize,
        }

        impl TurnProvider for UnregisteredToolProbeProvider {
            fn next_parts(
                &mut self,
                events: &[TurnEvent],
                _cancellation: &HeadlessTurnCancellation,
            ) -> impl std::future::Future<Output = Result<Vec<MessagePart>, HeadlessTurnPortError>> + Send
            {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);

                let parts = if call == 0 {
                    vec![MessagePart::ToolCall {
                        id: "call-1".into(),
                        name: "native::write".into(),
                        input: r#"{"path":"b.txt","content":"x"}"#.into(),
                    }]
                } else {
                    let content = events
                        .iter()
                        .find_map(|event| match event {
                            TurnEvent::ToolResult(MessagePart::ToolResult { content, .. }) => {
                                Some(content.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    let has_facts = events
                        .iter()
                        .any(|event| matches!(event, TurnEvent::ToolResultFacts { .. }));

                    vec![MessagePart::Text(format!("{content}|facts={has_facts}"))]
                };

                std::future::ready(Ok(parts))
            }
        }

        impl ProgressAwareProvider for UnregisteredToolProbeProvider {
            fn with_progress_sink(self, _progress: TurnProgressSink) -> Self {
                self
            }
        }

        struct ReadTool;

        impl agens_tools::DispatchTool for ReadTool {
            fn permission_target(
                &self,
                arguments: &serde_json::Value,
            ) -> Result<String, agens_core::Error> {
                arguments
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| agens_core::Error::Tool("invalid read arguments".into()))
            }

            fn execute(
                &mut self,
                _context: &agens_tools::ToolExecutionContext,
                _arguments: serde_json::Value,
            ) -> Result<agens_tools::ToolOutput, agens_core::Error> {
                Ok(agens_tools::ToolOutput::success("read"))
            }
        }

        let registry = TaskExecutionRegistry::with_limits(agens_tools::TaskExecutionLimits {
            max_iterations: 3,
            max_concurrency: 1,
            max_output_chars: 1_024,
        });
        let mut dispatcher = agens_tools::ToolDispatcher::new();
        dispatcher
            .register_native("native::read", agens_core::ToolAccess::ReadOnly, ReadTool)
            .unwrap();

        let result = run_isolated_task_turn(
            UnregisteredToolProbeProvider {
                calls: AtomicUsize::new(0),
            },
            Arc::new(Mutex::new(dispatcher)),
            Path::new("."),
            false,
            &HeadlessTurnCancellation::new(),
            None,
            TaskMailboxContext {
                registry,
                target: TaskMessageTarget::Main,
            },
        );

        let output = match result {
            Ok(output) => output,
            Err(_) => panic!("a denied tool call must not fail the turn"),
        };
        assert!(
            output.contains("permission denied"),
            "expected a denial naming permission denied, got: {output}"
        );
        assert!(
            !output.contains("invalid tool arguments"),
            "a call to a tool absent from the catalog must never surface as an argument error, \
             got: {output}"
        );
        assert!(
            output.contains("facts=true"),
            "expected denial_facts to accompany the denial, got: {output}"
        );
    }
}
