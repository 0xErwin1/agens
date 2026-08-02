//! Isolated child-turn execution for subagents: builds the provider for the
//! configured backend, runs a single turn to completion under a read-only or
//! dangerous-mode tool set, and reports a sanitized, provider-agnostic error.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_core::{
    HeadlessPermissionResolver, HeadlessToolCall, HeadlessTurnCancellation, HeadlessTurnError,
    HeadlessTurnPortError, Message, MessagePart, PermissionDecision, PermissionMode,
    PermissionPattern, PermissionPolicy, PermissionRule, PermissionSession, Role, SafetyPredicate,
    TurnEvent, TurnProgressSink, TurnProvider,
    run_isolated_headless_turn_with_max_iterations_and_progress,
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
use crate::child_catalog::{ChildSurfaceRejection, resolve_child_surface};
use crate::runtime::production_child_tool_runtime;
use agens_bootstrap::Bootstrap;
use agens_bootstrap::session_config::SessionConfig;
use agens_bootstrap::session_root::SessionRoot;
use agens_core::DiscardCompletedTurnRepository;
use agens_core::SubagentErrorKind;
use agens_diagnostics::{diagnostic_store, record_subagent_surface_rejection};
use agens_dispatch::ProductionToolDispatcher;
use agens_error::CliError;
use agens_permissions::{
    ProductionPermissionGate, SharedToolDispatcher, configured_permission_rules,
};

/// Why a delegated child turn ended without a result.
///
/// Every variant but [`Self::DeclarationRejected`] is payload-free on purpose:
/// the parent is told a class, never host detail. A rejected declaration is the
/// exception because the operator wrote the offending name themselves and
/// cannot act on the failure without seeing which one it was.
#[derive(Clone)]
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
    DeclarationRejected(ChildSurfaceRejection),
}

impl ChildRunError {
    pub const fn diagnostic_class(&self) -> ProviderDiagnosticClass {
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
            Self::IterationLimit | Self::Runtime | Self::DeclarationRejected(_) => {
                ProviderDiagnosticClass::Runtime
            }
        }
    }

    pub const fn tui_kind(&self) -> Option<SubagentErrorKind> {
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
            Self::Runtime | Self::DeclarationRejected(_) => Some(SubagentErrorKind::Runtime),
        }
    }

    pub fn task_runner_error(self) -> TaskRunnerError {
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
            Self::DeclarationRejected(rejection) => TaskRunnerError::DeclarationRejected {
                reason: rejection.reason,
                tool: rejection.tool,
            },
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

/// Reads the parent's own configured `[permissions]` rules for `project_root`.
///
/// Resolved through [`SessionConfig`] rather than from the process-captured
/// bootstrap value, so a delegation is bounded by the configuration of the
/// root it actually runs in. The tool pattern keeps its qualified name because
/// the child's dispatcher does not exist yet; the child's own policy resolves
/// names to identities when it evaluates.
fn parent_configured_rules(
    bootstrap: &Bootstrap,
    project_root: &Path,
) -> Result<Vec<PermissionRule>, CliError> {
    let session_config = SessionConfig::resolve(
        &SessionRoot::confined_to(project_root.to_path_buf()),
        bootstrap,
    )?;
    configured_permission_rules(
        session_config.permission_rules(),
        &project_root.display().to_string(),
        |configured| Ok(PermissionPattern::Exact(configured.to_owned())),
    )
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
    let parent_rules = parent_configured_rules(bootstrap, project_root).map_err(|error| {
        record_subagent_surface_rejection(bootstrap, diagnostic_reference, &error.message);
        ChildRunError::Runtime
    })?;
    let surface =
        resolve_child_surface(&parent_rules, request.permission_rules()).map_err(|rejection| {
            record_subagent_surface_rejection(
                bootstrap,
                diagnostic_reference,
                &rejection.message(),
            );
            ChildRunError::DeclarationRejected(rejection)
        })?;
    let (provider_tools, tool_runtime) = production_child_tool_runtime(
        project_root,
        bootstrap.tool_limits(),
        &surface,
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
                IsolatedTaskTurnContext {
                    project_root,
                    dangerous_mode,
                    cancellation,
                    progress,
                    surface: &surface,
                    mailbox: TaskMailboxContext {
                        registry: task_registry.clone(),
                        target: TaskMessageTarget::Execution(execution_id),
                    },
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
                IsolatedTaskTurnContext {
                    project_root,
                    dangerous_mode,
                    cancellation,
                    progress,
                    surface: &surface,
                    mailbox: TaskMailboxContext {
                        registry: task_registry.clone(),
                        target: TaskMessageTarget::Execution(execution_id),
                    },
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
                IsolatedTaskTurnContext {
                    project_root,
                    dangerous_mode,
                    cancellation,
                    progress,
                    surface: &surface,
                    mailbox: TaskMailboxContext {
                        registry: task_registry.clone(),
                        target: TaskMessageTarget::Execution(execution_id),
                    },
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

struct IsolatedTaskTurnContext<'a> {
    project_root: &'a Path,
    dangerous_mode: bool,
    cancellation: &'a HeadlessTurnCancellation,
    progress: Option<&'a TurnProgressSink>,
    surface: &'a crate::child_catalog::ChildToolSurface,
    mailbox: TaskMailboxContext,
}

/// Runs a subagent's isolated turn with no session attempt of its own, so the
/// facts it emits carry no `session_id`/`attempt_id`.
fn configured_task_max_iterations(registry: &TaskExecutionRegistry) -> usize {
    registry.limits().max_iterations
}

fn run_isolated_task_turn<P>(
    provider: P,
    tool_runtime: SharedToolDispatcher,
    context: IsolatedTaskTurnContext<'_>,
) -> Result<String, ChildRunError>
where
    P: ProgressAwareProvider + Send,
{
    let IsolatedTaskTurnContext {
        project_root,
        dangerous_mode,
        cancellation,
        progress,
        surface,
        mailbox,
    } = context;
    let max_iterations = configured_task_max_iterations(&mailbox.registry);
    let mut provider = TaskMailboxProvider::new(provider, Some(mailbox.registry), mailbox.target);
    let mut rules = surface.rules.clone();
    rules.extend(
        ["native::task_control", "native::task_message"]
            .into_iter()
            .map(|tool| {
                PermissionRule::global(
                    PermissionDecision::Allow,
                    PermissionPattern::Exact(tool.into()),
                    PermissionPattern::Any,
                )
            }),
    );
    let policy = PermissionPolicy::with_safety_predicates(
        PermissionMode::Edit,
        rules,
        vec![SafetyPredicate::WorktreeEscape, SafetyPredicate::ChatWrite],
    )
    .with_configured_floor(surface.configured_floor.clone());
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
    let snapshot =
        block_on_headless_turn(run_isolated_headless_turn_with_max_iterations_and_progress(
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
            IsolatedTaskTurnContext {
                project_root: Path::new("."),
                dangerous_mode: false,
                cancellation: &HeadlessTurnCancellation::new(),
                progress: None,
                surface: &crate::child_catalog::resolve_child_surface(&[], &[]).unwrap(),
                mailbox: TaskMailboxContext {
                    registry,
                    target: TaskMessageTarget::Main,
                },
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
            IsolatedTaskTurnContext {
                project_root: Path::new("."),
                dangerous_mode: false,
                cancellation: &HeadlessTurnCancellation::new(),
                progress: None,
                surface: &crate::child_catalog::resolve_child_surface(&[], &[]).unwrap(),
                mailbox: TaskMailboxContext {
                    registry,
                    target: TaskMessageTarget::Main,
                },
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

    struct SingleToolCallProvider {
        calls: std::sync::atomic::AtomicUsize,
        name: String,
        input: String,
    }

    impl TurnProvider for SingleToolCallProvider {
        fn next_parts(
            &mut self,
            events: &[TurnEvent],
            _cancellation: &HeadlessTurnCancellation,
        ) -> impl std::future::Future<Output = Result<Vec<MessagePart>, HeadlessTurnPortError>> + Send
        {
            use std::sync::atomic::Ordering;

            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let parts = if call == 0 {
                vec![MessagePart::ToolCall {
                    id: "call-1".into(),
                    name: self.name.clone(),
                    input: self.input.clone(),
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
                vec![MessagePart::Text(content)]
            };

            std::future::ready(Ok(parts))
        }
    }

    impl ProgressAwareProvider for SingleToolCallProvider {
        fn with_progress_sink(self, _progress: TurnProgressSink) -> Self {
            self
        }
    }

    fn single_call_turn(
        project_root: &Path,
        declarations: &[PermissionRule],
        parent_rules: &[PermissionRule],
        dangerous_mode: bool,
        tool_name: &str,
        input: &str,
    ) -> String {
        let surface =
            crate::child_catalog::resolve_child_surface(parent_rules, declarations).unwrap();
        let registry = TaskExecutionRegistry::with_limits(agens_tools::TaskExecutionLimits {
            max_iterations: 3,
            max_concurrency: 1,
            max_output_chars: 4_096,
        });
        let execution_id = registry
            .admit(agens_tools::TaskLaunchMode::Foreground)
            .unwrap();
        let (_, tool_runtime) = production_child_tool_runtime(
            project_root,
            agens_config::ToolLimitSettings::default(),
            &surface,
            registry.clone(),
            execution_id,
        )
        .unwrap();

        match run_isolated_task_turn(
            SingleToolCallProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
                name: tool_name.to_owned(),
                input: input.to_owned(),
            },
            tool_runtime,
            IsolatedTaskTurnContext {
                project_root,
                dangerous_mode,
                cancellation: &HeadlessTurnCancellation::new(),
                progress: None,
                surface: &surface,
                mailbox: TaskMailboxContext {
                    registry,
                    target: TaskMessageTarget::Main,
                },
            },
        ) {
            Ok(output) => output,
            Err(_) => panic!("a single scripted tool call must not fail the turn"),
        }
    }

    #[test]
    fn an_agent_with_no_declarations_reaches_the_inherited_read_class_tools() {
        let temporary = agens_fixtures::session_directory("no-declarations");
        let project_root = temporary.join("project");

        let output = single_call_turn(
            &project_root,
            &[],
            &[],
            false,
            "native::grep",
            r#"{"pattern":"anything"}"#,
        );

        assert!(
            !output.contains("permission denied"),
            "an inherited read-class tool must not be denied, got: {output}"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_read_only_agent_is_denied_write_regardless_of_dangerous_mode() {
        let read_only_rules = [
            PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::glob("write").unwrap(),
                PermissionPattern::Any,
            ),
            PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::glob("edit").unwrap(),
                PermissionPattern::Any,
            ),
            PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::glob("bash").unwrap(),
                PermissionPattern::Any,
            ),
            PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::glob("webfetch").unwrap(),
                PermissionPattern::Any,
            ),
        ];

        for dangerous_mode in [false, true] {
            let temporary =
                agens_fixtures::session_directory(&format!("read-only-dangerous-{dangerous_mode}"));
            let project_root = temporary.join("project");

            let output = single_call_turn(
                &project_root,
                &read_only_rules,
                &[],
                dangerous_mode,
                "native::write",
                r#"{"path":"denied.txt","content":"x"}"#,
            );

            assert!(
                output.contains("permission denied"),
                "a declared-omitted tool must stay denied with dangerous_mode={dangerous_mode}, \
                 got: {output}"
            );

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    #[test]
    fn a_declared_ask_in_a_child_denies_and_states_the_prompt_was_unreachable() {
        let temporary = agens_fixtures::session_directory("declared-ask-unreachable");
        let project_root = temporary.join("project");

        let output = single_call_turn(
            &project_root,
            &[PermissionRule::global(
                PermissionDecision::Ask,
                PermissionPattern::glob("bash").unwrap(),
                PermissionPattern::Any,
            )],
            &[],
            false,
            "native::bash",
            r#"{"command":"echo should-never-run > ask-marker.txt"}"#,
        );

        assert!(
            output.contains("permission denied"),
            "a declared ask must still deny inside a child, got: {output}"
        );
        assert!(
            output.contains("could not be reached"),
            "the denial must state that the prompt was unreachable in a subagent, \
             not read like a plain policy deny, got: {output}"
        );
        assert!(!project_root.join("ask-marker.txt").exists());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_write_granted_agent_actually_writes_the_file() {
        let temporary = agens_fixtures::session_directory("write-granted");
        let project_root = temporary.join("project");

        let output = single_call_turn(
            &project_root,
            &[
                PermissionRule::global(
                    PermissionDecision::Allow,
                    PermissionPattern::glob("write").unwrap(),
                    PermissionPattern::Any,
                ),
                PermissionRule::global(
                    PermissionDecision::Allow,
                    PermissionPattern::glob("edit").unwrap(),
                    PermissionPattern::Any,
                ),
            ],
            &[],
            false,
            "native::write",
            r#"{"path":"granted.txt","content":"hello from a declared allow"}"#,
        );

        assert!(
            !output.contains("permission denied"),
            "a declared allow write must not be denied, got: {output}"
        );
        assert_eq!(
            std::fs::read_to_string(project_root.join("granted.txt")).unwrap(),
            "hello from a declared allow"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_bash_granted_agent_executes_unattended() {
        let temporary = agens_fixtures::session_directory("bash-granted");
        let project_root = temporary.join("project");

        let output = single_call_turn(
            &project_root,
            &[PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::glob("bash").unwrap(),
                PermissionPattern::Any,
            )],
            &[],
            false,
            "native::bash",
            r#"{"command":"echo marker > bash-marker.txt"}"#,
        );

        assert!(
            !output.contains("permission denied"),
            "a declared allow bash must not be denied, got: {output}"
        );
        assert!(
            project_root.join("bash-marker.txt").exists(),
            "the declared-allow bash call must have actually run"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_target_scoped_bash_deny_blocks_only_the_matching_command_and_survives_dangerous_mode() {
        let declarations = [
            PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::glob("bash").unwrap(),
                PermissionPattern::Any,
            ),
            PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::glob("bash").unwrap(),
                PermissionPattern::glob("rm -rf /**").unwrap(),
            ),
        ];

        for dangerous_mode in [false, true] {
            let temporary = agens_fixtures::session_directory(&format!(
                "targeted-bash-deny-dangerous-{dangerous_mode}"
            ));
            let project_root = temporary.join("project");

            let denied_output = single_call_turn(
                &project_root,
                &declarations,
                &[],
                dangerous_mode,
                "native::bash",
                r#"{"command":"rm -rf /tmp/should-never-run"}"#,
            );

            assert!(
                denied_output.contains("permission denied"),
                "a target-scoped declared deny must survive dangerous_mode={dangerous_mode}, \
                 got: {denied_output}"
            );

            let allowed_output = single_call_turn(
                &project_root,
                &declarations,
                &[],
                dangerous_mode,
                "native::bash",
                r#"{"command":"echo marker > survives.txt"}"#,
            );

            assert!(
                !allowed_output.contains("permission denied"),
                "a non-matching bash command must still run, got: {allowed_output}"
            );
            assert!(
                project_root.join("survives.txt").exists(),
                "the non-matching bash command must have actually run"
            );

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    #[test]
    fn a_target_scoped_deny_with_a_wildcard_tool_pattern_blocks_a_command_containing_a_slash() {
        let declarations = [
            PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::glob("bash").unwrap(),
                PermissionPattern::Any,
            ),
            PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::glob("bas*").unwrap(),
                PermissionPattern::glob("rm*").unwrap(),
            ),
        ];

        let temporary = agens_fixtures::session_directory("targeted-wildcard-bash-deny-slash");
        let project_root = temporary.join("project");
        let victim = project_root.join("nested").join("probe-victim.txt");
        std::fs::create_dir_all(victim.parent().unwrap()).unwrap();
        std::fs::write(&victim, "victim").unwrap();

        let denied_output = single_call_turn(
            &project_root,
            &declarations,
            &[],
            false,
            "native::bash",
            &format!(r#"{{"command":"rm -rf {}"}}"#, victim.display()),
        );

        assert!(
            denied_output.contains("permission denied"),
            "a target-scoped deny whose tool pattern is a wildcard must still deny a command \
             containing a slash, not just a slash-free one, got: {denied_output}"
        );
        assert!(
            victim.exists(),
            "the denied command must never have run, yet the victim file is gone"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_target_scoped_deny_with_a_wildcard_tool_pattern_still_blocks_the_matching_command() {
        let declarations = [
            PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::glob("bash").unwrap(),
                PermissionPattern::Any,
            ),
            PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::glob("bas*").unwrap(),
                PermissionPattern::glob("rm*").unwrap(),
            ),
        ];

        let temporary = agens_fixtures::session_directory("targeted-wildcard-bash-deny");
        let project_root = temporary.join("project");
        let victim = project_root.join("probe-victim.txt");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(&victim, "victim").unwrap();

        let denied_output = single_call_turn(
            &project_root,
            &declarations,
            &[],
            false,
            "native::bash",
            r#"{"command":"rm -rf probe-victim.txt"}"#,
        );

        assert!(
            denied_output.contains("permission denied"),
            "a target-scoped deny whose tool pattern is a wildcard must still deny a matching \
             command, got: {denied_output}"
        );
        assert!(
            victim.exists(),
            "the denied command must never have run, yet the victim file is gone"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_target_scoped_write_deny_generalizes_the_guardrail_beyond_bash() {
        let temporary = agens_fixtures::session_directory("targeted-write-deny");
        let project_root = temporary.join("project");
        let declarations = [
            PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::glob("write").unwrap(),
                PermissionPattern::Any,
            ),
            PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::glob("write").unwrap(),
                PermissionPattern::glob(".env*").unwrap(),
            ),
        ];

        let denied_output = single_call_turn(
            &project_root,
            &declarations,
            &[],
            false,
            "native::write",
            r#"{"path":".env","content":"SECRET=1"}"#,
        );
        assert!(
            denied_output.contains("permission denied"),
            "a target-scoped deny must block a matching write target, got: {denied_output}"
        );
        assert!(!project_root.join(".env").exists());

        let allowed_output = single_call_turn(
            &project_root,
            &declarations,
            &[],
            false,
            "native::write",
            r#"{"path":"notes.md","content":"fine"}"#,
        );
        assert!(
            !allowed_output.contains("permission denied"),
            "a non-matching write target must still be allowed, got: {allowed_output}"
        );
        assert_eq!(
            std::fs::read_to_string(project_root.join("notes.md")).unwrap(),
            "fine"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// Parses declarations through the real agent-markdown grammar, so a probe
    /// exercises exactly the rules an authored definition produces rather than
    /// a hand-built approximation of them.
    fn declared(directory: &Path, declarations: &[&str]) -> Vec<PermissionRule> {
        let global = directory.join("agents-global");
        let project = directory.join("agents-project");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&project).unwrap();

        let entries = declarations
            .iter()
            .map(|entry| format!("  - {entry}\n"))
            .collect::<String>();
        std::fs::write(
            global.join("probe.md"),
            format!(
                "---\nname: probe\ndescription: probe\nmode: all\npermissions:\n{entries}---\nbody\n"
            ),
        )
        .unwrap();

        agens_tools::AgentCatalog::discover(&[], &global, &project)
            .unwrap()
            .catalog()
            .agent("probe")
            .expect("the probe definition must load")
            .permission_rules
            .clone()
    }

    /// `deny bash rm*` beside `allow bash *` is the canonical "bash, except
    /// these" shape written with the wildcard spelled out. Both targets are
    /// globs and therefore equally specific, so only the deny-takes-the-tie
    /// rule keeps the narrow deny from being overtaken by the broad allow.
    #[test]
    fn a_narrow_deny_holds_against_an_explicit_wildcard_allow_in_either_order() {
        for (label, declarations) in [
            ("deny-first", ["deny bash rm*", "allow bash *"]),
            ("allow-first", ["allow bash *", "deny bash rm*"]),
        ] {
            let temporary = agens_fixtures::session_directory(&format!("wildcard-allow-{label}"));
            let project_root = temporary.join("project");
            let victim = project_root.join("probe-victim.txt");
            std::fs::create_dir_all(&project_root).unwrap();
            std::fs::write(&victim, "victim").unwrap();

            let rules = declared(&temporary, &declarations);
            let denied_output = single_call_turn(
                &project_root,
                &rules,
                &[],
                false,
                "native::bash",
                r#"{"command":"rm -rf probe-victim.txt"}"#,
            );

            assert!(
                denied_output.contains("permission denied"),
                "{label}: an explicit wildcard allow must not overtake a narrower deny, \
                 got: {denied_output}"
            );
            assert!(
                victim.exists(),
                "{label}: the denied command must never have run, yet the victim file is gone"
            );

            let allowed_output = single_call_turn(
                &project_root,
                &rules,
                &[],
                false,
                "native::bash",
                r#"{"command":"echo hi"}"#,
            );
            assert!(
                !allowed_output.contains("permission denied"),
                "{label}: the broad allow must still authorize a non-matching command, \
                 got: {allowed_output}"
            );

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    /// "Deny the secrets, allow the tree" is the canonical authoring shape for
    /// a write-scoped agent, and both of its targets are globs.
    #[test]
    fn a_nested_write_deny_holds_against_a_broader_write_allow_in_either_order() {
        for (label, declarations) in [
            (
                "deny-first",
                ["deny write src/secret/**", "allow write src/**"],
            ),
            (
                "allow-first",
                ["allow write src/**", "deny write src/secret/**"],
            ),
        ] {
            let temporary =
                agens_fixtures::session_directory(&format!("nested-write-deny-{label}"));
            let project_root = temporary.join("project");
            std::fs::create_dir_all(project_root.join("src").join("secret")).unwrap();

            let rules = declared(&temporary, &declarations);
            let denied_output = single_call_turn(
                &project_root,
                &rules,
                &[],
                false,
                "native::write",
                r#"{"path":"src/secret/key.txt","content":"SECRET"}"#,
            );

            assert!(
                denied_output.contains("permission denied"),
                "{label}: a broader allow must not overtake a nested deny, got: {denied_output}"
            );
            assert!(
                !project_root
                    .join("src")
                    .join("secret")
                    .join("key.txt")
                    .exists(),
                "{label}: the denied write must never have run, yet the secret file exists"
            );

            let allowed_output = single_call_turn(
                &project_root,
                &rules,
                &[],
                false,
                "native::write",
                r#"{"path":"src/main.rs","content":"fn main() {}"}"#,
            );
            assert!(
                !allowed_output.contains("permission denied"),
                "{label}: a write outside the denied subtree must still be allowed, \
                 got: {allowed_output}"
            );

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    /// "Deny X except for these commands" is the standard allowlist shape. The
    /// untargeted deny must not erase the tool from the catalog, because the
    /// targeted allow outranks it for the calls it names and needs the tool to
    /// still be there to act on.
    #[test]
    fn an_untargeted_deny_leaves_a_targeted_allow_reachable_in_either_order() {
        for (label, declarations) in [
            ("deny-first", ["deny bash", "allow bash git*"]),
            ("allow-first", ["allow bash git*", "deny bash"]),
        ] {
            let temporary =
                agens_fixtures::session_directory(&format!("targeted-allowlist-{label}"));
            let project_root = temporary.join("project");
            std::fs::create_dir_all(&project_root).unwrap();

            let rules = declared(&temporary, &declarations);
            let allowed_output = single_call_turn(
                &project_root,
                &rules,
                &[],
                false,
                "native::bash",
                r#"{"command":"git --version"}"#,
            );
            assert!(
                !allowed_output.contains("permission denied"),
                "{label}: the targeted allow must outrank the untargeted deny, \
                 got: {allowed_output}"
            );

            let denied_output = single_call_turn(
                &project_root,
                &rules,
                &[],
                false,
                "native::bash",
                r#"{"command":"echo hi"}"#,
            );
            assert!(
                denied_output.contains("permission denied"),
                "{label}: everything the targeted allow does not name stays denied, \
                 got: {denied_output}"
            );

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    /// A project-supplied definition can declare `allow bash`, so the parent's
    /// own configured deny has to outrank it. Enforced as a hard safety
    /// predicate rather than as a policy rule, because a declaration would
    /// otherwise be the more specific match and win.
    #[test]
    fn a_configured_deny_outranks_a_declared_allow_inside_the_child() {
        let temporary = agens_fixtures::session_directory("configured-deny-outranks");
        let project_root = temporary.join("project");
        let victim = project_root.join("probe-victim.txt");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(&victim, "victim").unwrap();

        let denied_output = single_call_turn(
            &project_root,
            &[PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::glob("bash").unwrap(),
                PermissionPattern::Any,
            )],
            &[PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::Exact("native::bash".into()),
                PermissionPattern::glob_for_target_kind(
                    "rm*",
                    agens_core::PermissionTargetKind::FreeFormText,
                )
                .unwrap(),
            )],
            false,
            "native::bash",
            r#"{"command":"rm -rf probe-victim.txt"}"#,
        );

        assert!(
            denied_output.contains("permission denied"),
            "a configured deny must survive a declared allow, got: {denied_output}"
        );
        assert!(
            victim.exists(),
            "the denied command must never have run, yet the victim file is gone"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_configured_deny_removes_a_declared_tool_from_the_child_entirely() {
        let temporary = agens_fixtures::session_directory("configured-deny-omits");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let output = single_call_turn(
            &project_root,
            &[],
            &[PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::Exact("native::write".into()),
                PermissionPattern::Any,
            )],
            true,
            "native::write",
            r#"{"path":"notes.md","content":"nope"}"#,
        );

        assert!(
            output.contains("permission denied"),
            "a configured deny must reach the child even under dangerous mode, got: {output}"
        );
        assert!(!project_root.join("notes.md").exists());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_granted_bash_call_can_still_reach_outside_the_project_root() {
        let temporary = agens_fixtures::session_directory("bash-unconfined");
        let project_root = temporary.join("project");

        let output = single_call_turn(
            &project_root,
            &[PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::glob("bash").unwrap(),
                PermissionPattern::Any,
            )],
            &[],
            false,
            "native::bash",
            r#"{"command":"echo escaped > ../outside-marker.txt"}"#,
        );

        assert!(
            !output.contains("permission denied"),
            "a declared allow bash must not be denied, got: {output}"
        );
        assert!(
            temporary.join("outside-marker.txt").exists(),
            "a granted bash call is not confined to the project root: this is the accepted, \
             documented cost of granting bash, not a regression"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn built_in_explore_still_cannot_write_edit_bash_or_fetch_after_inheriting_the_parent_surface()
    {
        let temporary = agens_fixtures::session_directory("explore-narrowing");
        let bootstrap = agens_fixtures::session_bootstrap(&temporary, &[]);
        let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);
        let catalog =
            agens_agents::discover_agent_catalog(&bootstrap, &project_root, None).unwrap();
        let explore = catalog
            .agent("explore")
            .expect("explore is a built-in agent");

        let surface = resolve_child_surface(&[], &explore.permission_rules).unwrap();
        let tool_names = surface
            .tools
            .iter()
            .map(|entry| entry.qualified_name.clone())
            .collect::<Vec<_>>();

        for denied in [
            "native::write",
            "native::edit",
            "native::bash",
            "native::webfetch",
        ] {
            assert!(
                !tool_names.contains(&denied.to_owned()),
                "{denied} must be absent from explore's inherited surface, got: {tool_names:?}"
            );
        }
        assert!(
            tool_names.contains(&"native::read".to_owned()),
            "explore must still inherit the read-class tools it relies on"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }
}
