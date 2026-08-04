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
    TurnEvent, TurnProgressSink, TurnProvider, run_isolated_headless_turn_with_progress,
};
use agens_providers::{
    ChatGptResponsesProvider, MoonshotProvider, OpenAiResponsesProvider, ProgressAwareProvider,
    ProviderDiagnosticClass, ProviderDiagnosticEvent, ProviderDiagnosticScope, ProviderDiagnostics,
};
use agens_tools::{
    TaskExecutionId, TaskExecutionRegistry, TaskMessageSource, TaskMessageTarget,
    TaskProviderFailure, TaskRegistryError, TaskRunnerError, TaskTurnRequest,
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
    PermissionPrompter, ProductionPermissionGate, ProductionPermissionResolver,
    ProductionPromptAuthorization, SharedToolDispatcher, configured_permission_rules,
};
use agens_store::PermissionGrantStore;
use std::path::PathBuf;

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
            Self::Runtime | Self::DeclarationRejected(_) => ProviderDiagnosticClass::Runtime,
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
        HeadlessTurnError::MaxIterations => ChildRunError::Runtime,
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
    /// The parent's connected MCP registry, shared so this child dispatches
    /// through the same servers instead of standing up its own.
    pub mcp_registry: Option<Arc<Mutex<agens_tools::McpRegistry>>>,
    /// Builds this execution's prompter onto the parent's surface. `None` is a
    /// delegation with nobody to ask.
    pub permission_prompter: Option<crate::runner::PrompterFactory>,
    /// Builds this execution's `ask_user` port onto the parent's surface.
    pub ask_user_port: Option<crate::runner::AskUserPortFactory>,
    /// Where this execution sits in the delegation chain. The parent turn is
    /// 0, so its own children are 1.
    pub depth: usize,
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
        mcp_registry,
        permission_prompter,
        ask_user_port,
        depth,
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
    // Read from the registry the parent already connected, so the declarations
    // below are resolved against the remote tools this child will actually
    // hold rather than against the natives alone.
    let remote_tools = match mcp_registry.as_ref() {
        Some(registry) => registry
            .lock()
            .map_err(|_| ChildRunError::Runtime)?
            .tools()
            .into_iter()
            .map(|tool| tool.qualified_name.clone())
            .collect::<Vec<_>>(),
        None => Vec::new(),
    };
    let surface = resolve_child_surface(&parent_rules, request.permission_rules(), &remote_tools)
        .map_err(|rejection| {
        record_subagent_surface_rejection(bootstrap, diagnostic_reference, &rejection.message());
        ChildRunError::DeclarationRejected(rejection)
    })?;
    let origin = crate::runner::PromptOrigin {
        execution: execution_id.value(),
        agent: request.agent_name().to_owned(),
    };
    let skills = agens_bootstrap::discover_skill_catalog(bootstrap, project_root)
        .map_err(|_| ChildRunError::Runtime)?;
    let (provider_tools, tool_runtime) = production_child_tool_runtime(
        project_root,
        bootstrap.tool_limits(),
        &surface,
        task_registry.clone(),
        execution_id,
        Some(skills.catalog()),
        mcp_registry.clone(),
        Some(crate::runtime::ChildDelegation {
            bootstrap,
            depth,
            parent: crate::task::TaskParentSelection {
                model: request.model().to_owned(),
                request_config: request.request_config().clone(),
                diagnostic_reference: Some(diagnostic_reference.to_owned()),
            },
            permission_prompter: permission_prompter.clone(),
            ask_user_port: ask_user_port.clone(),
            origin: origin.clone(),
        }),
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
                    agens_providers::DEFAULT_PROVIDER_REQUEST_TIMEOUT,
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
                    permission_prompter: permission_prompter.clone(),
                    grant_store_root: bootstrap.data_directory().to_path_buf(),
                    origin: Some(origin.clone()),
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
                agens_providers::DEFAULT_PROVIDER_REQUEST_TIMEOUT,
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
                    permission_prompter: permission_prompter.clone(),
                    grant_store_root: bootstrap.data_directory().to_path_buf(),
                    origin: Some(origin.clone()),
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
                agens_providers::DEFAULT_PROVIDER_REQUEST_TIMEOUT,
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
                    permission_prompter: permission_prompter.clone(),
                    grant_store_root: bootstrap.data_directory().to_path_buf(),
                    origin: Some(origin.clone()),
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
    provider_rounds: usize,
    check_interval: Option<usize>,
}

const SUBAGENT_SUPERVISION_CHECK: &str = "[subagent supervision advisory]\nAre you still making progress, or do you need help? If you are blocked, contact the parent with the task_message tool using target=\"main\". This check is advisory; continue working if you are making progress.";

impl<P> TaskMailboxProvider<P> {
    pub fn new(
        inner: P,
        registry: Option<TaskExecutionRegistry>,
        target: TaskMessageTarget,
    ) -> Self {
        let check_interval = match target {
            TaskMessageTarget::Execution(_) => registry
                .as_ref()
                .map(|registry| registry.limits().check_interval),
            TaskMessageTarget::Main => None,
        };
        Self {
            inner,
            registry,
            target,
            provider_rounds: 0,
            check_interval,
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
        let mut messages = self
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
        if self.provider_rounds > 0
            && self
                .check_interval
                .is_some_and(|interval| self.provider_rounds.is_multiple_of(interval))
            && let (Some(registry), TaskMessageTarget::Execution(id)) =
                (&self.registry, self.target)
        {
            messages.extend(supervision_advisory(registry, id)?);
        }
        self.inner.queue_user_messages(messages)?;
        self.provider_rounds = self.provider_rounds.saturating_add(1);
        self.inner.next_parts(events, cancellation).await
    }
}

/// Queues one best-effort supervision check. A terminal execution is an expected race with task
/// completion. A message-limit failure can likewise mean another sender filled the still-running
/// execution's mailbox after this provider drained it. Those two advisory losses are intentionally
/// non-fatal; every other registry error indicates a broken execution identity or route invariant.
fn supervision_advisory(
    registry: &TaskExecutionRegistry,
    id: TaskExecutionId,
) -> Result<Vec<Message>, HeadlessTurnPortError> {
    match registry.send_message(
        TaskMessageSource::Main,
        TaskMessageTarget::Execution(id),
        SUBAGENT_SUPERVISION_CHECK.into(),
    ) {
        Ok(()) => Ok(registry
            .drain_messages(TaskMessageTarget::Execution(id))
            .into_iter()
            .map(coordination_message)
            .collect()),
        Err(TaskRegistryError::TerminalExecution | TaskRegistryError::MessageLimit) => {
            Ok(Vec::new())
        }
        Err(
            TaskRegistryError::UnknownExecution
            | TaskRegistryError::ForbiddenRoute
            | TaskRegistryError::InvalidControl,
        ) => Err(HeadlessTurnPortError::Provider),
    }
}

fn coordination_message(message: agens_tools::TaskMessage) -> Message {
    Message {
        role: Role::User,
        parts: vec![MessagePart::Text(format!(
            "[coordination source={} untrusted=true]\n{}",
            task_message_source_label(message.source()),
            message.content(),
        ))],
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
    permission_prompter: Option<crate::runner::PrompterFactory>,
    grant_store_root: PathBuf,
    origin: Option<crate::runner::PromptOrigin>,
}

/// Runs a subagent's isolated turn with no session attempt of its own, so the
/// facts it emits carry no `session_id`/`attempt_id`.
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
        permission_prompter,
        grant_store_root,
        origin,
    } = context;
    let mut provider = TaskMailboxProvider::new(provider, Some(mailbox.registry), mailbox.target);
    let policy = PermissionPolicy::with_safety_predicates(
        PermissionMode::Edit,
        surface.rules.clone(),
        vec![SafetyPredicate::WorktreeEscape, SafetyPredicate::ChatWrite],
    )
    .with_configured_floor(surface.configured_floor.clone());
    let grants = Arc::new(Mutex::new(Vec::new()));
    let session = PermissionSession::new();
    let resolver_session = session;
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
    let mut resolver = match permission_prompter.and_then(|build| {
        PermissionGrantStore::open(&grant_store_root)
            .ok()
            .map(|store| {
                (
                    build(origin.clone().unwrap_or_else(main_thread_origin)),
                    store,
                )
            })
    }) {
        Some((prompter, store)) => {
            ChildPermissionResolver::Prompting(Box::new(ProductionPermissionResolver::new(
                prompter,
                store,
                Arc::clone(&grants),
                Arc::clone(&prompts),
                ProductionPromptAuthorization {
                    policy: policy.clone(),
                    session: resolver_session,
                    project: project.clone(),
                    dispatcher: Arc::clone(&tool_runtime),
                    allowed: Arc::clone(&pending),
                },
            )))
        }
        None => ChildPermissionResolver::Unreachable,
    };
    let mut dispatcher = ProductionToolDispatcher::new(tool_runtime, pending);
    let snapshot = block_on_headless_turn(run_isolated_headless_turn_with_progress(
        &mut provider,
        &mut gate,
        &mut resolver,
        &mut dispatcher,
        &mut repository,
        cancellation,
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

/// The origin a prompt carries when the caller named none. Only tests reach
/// this: a real delegation always names its execution.
fn main_thread_origin() -> crate::runner::PromptOrigin {
    crate::runner::PromptOrigin {
        execution: 0,
        agent: "subagent".into(),
    }
}

/// How a delegated execution answers a call the policy left undecided.
///
/// [`Self::Unreachable`] is the honest answer when nobody is listening: a
/// delegation with no surface behind it cannot ask, so it denies. It is not
/// the honest answer when there IS a surface — the parent is sitting at one,
/// the child runs on a thread of the parent's own process, and the prompt
/// bridge it would use is the same object. Denying there answered a question
/// on the user's behalf that they were available to answer themselves.
enum ChildPermissionResolver {
    Unreachable,
    /// Boxed because the production resolver carries a grant store and a
    /// policy, and this enum is constructed once per turn either way.
    Prompting(Box<ProductionPermissionResolver<Box<dyn PermissionPrompter>>>),
}

impl HeadlessPermissionResolver for ChildPermissionResolver {
    async fn resolve(
        &mut self,
        call: &HeadlessToolCall,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<PermissionDecision, HeadlessTurnPortError> {
        match self {
            Self::Unreachable => Ok(PermissionDecision::Deny),
            Self::Prompting(resolver) => resolver.resolve(call, cancellation).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agens_core::HeadlessPermissionGate;

    /// A prompter that records what it was asked and answers as scripted,
    /// standing in for the person at the parent's terminal.
    struct ScriptedPrompter {
        answer: agens_permissions::PermissionPromptAnswer,
        asked: Arc<Mutex<Vec<String>>>,
    }

    impl PermissionPrompter for ScriptedPrompter {
        fn prompt(
            &mut self,
            context: &agens_tools::PermissionPromptContext,
            _: &HeadlessTurnCancellation,
        ) -> Result<agens_permissions::PermissionPromptAnswer, HeadlessTurnPortError> {
            self.asked
                .lock()
                .expect("the record must be available")
                .push(context.target_identifier.clone());
            Ok(self.answer)
        }
    }

    /// These ports all answer synchronously, so one poll is the whole future.
    fn poll_once<T>(future: impl std::future::Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        let context = &mut std::task::Context::from_waker(std::task::Waker::noop());
        match future.as_mut().poll(context) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => panic!("this fixture's ports complete synchronously"),
        }
    }

    /// The behaviour this whole channel exists for. A delegated execution runs
    /// on a thread of the parent's process, and the parent is sitting at a
    /// surface; answering `Deny` on the user's behalf was inventing a decision
    /// they were available to make. Both directions are pinned, because a
    /// channel that only ever says yes would pass an allow-only assertion.
    #[test]
    fn a_child_with_a_surface_asks_the_person_instead_of_denying_for_them() {
        use agens_permissions::PermissionPromptAnswer;

        let temporary = std::env::temp_dir().join(format!(
            "agens-child-prompt-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&temporary).expect("the store root must exist");

        for (answer, expected) in [
            (PermissionPromptAnswer::AllowOnce, PermissionDecision::Allow),
            (PermissionPromptAnswer::DenyOnce, PermissionDecision::Deny),
        ] {
            let (_tools, dispatcher) = crate::runtime::production_child_tool_runtime(
                &temporary,
                agens_config::ToolLimitSettings::default(),
                &crate::child_catalog::resolve_child_surface(&[], &[], &[]).unwrap(),
                TaskExecutionRegistry::new(),
                agens_tools::TaskExecutionId::from_value(1),
                None,
                None,
                None,
            )
            .expect("the child runtime must build");

            let call = HeadlessToolCall {
                id: "call-1".into(),
                name: "native::write".into(),
                input: r#"{"path":"notes.md","contents":"hi"}"#.into(),
            };
            let prompts = Arc::new(Mutex::new(BTreeMap::new()));
            let asked = Arc::new(Mutex::new(Vec::new()));

            // The gate is what parks a call as a pending prompt; the resolver
            // only answers one, so it has to be driven through the gate the
            // same way a real turn drives it.
            let mut gate = ProductionPermissionGate::new(
                agens_core::PermissionPolicy::new(PermissionMode::Edit, Vec::new()),
                Arc::new(Mutex::new(Vec::new())),
                PermissionSession::new(),
                "project".into(),
                Arc::clone(&dispatcher),
                Arc::new(Mutex::new(BTreeMap::new())),
                Arc::clone(&prompts),
            );
            let cancellation = HeadlessTurnCancellation::new();
            let evaluated =
                poll_once(gate.evaluate(&call, &cancellation)).expect("the gate decides");
            assert_eq!(
                evaluated,
                PermissionDecision::Ask,
                "an unruled write must be the undecided case this channel is about"
            );

            let mut resolver =
                ChildPermissionResolver::Prompting(Box::new(ProductionPermissionResolver::new(
                    Box::new(ScriptedPrompter {
                        answer,
                        asked: Arc::clone(&asked),
                    }) as Box<dyn PermissionPrompter>,
                    PermissionGrantStore::open(&temporary).expect("the grant store must open"),
                    Arc::new(Mutex::new(Vec::new())),
                    Arc::clone(&prompts),
                    ProductionPromptAuthorization {
                        policy: agens_core::PermissionPolicy::new(PermissionMode::Edit, Vec::new()),
                        session: PermissionSession::new(),
                        project: "project".into(),
                        dispatcher: Arc::clone(&dispatcher),
                        allowed: Arc::new(Mutex::new(BTreeMap::new())),
                    },
                )));
            let decision =
                poll_once(resolver.resolve(&call, &cancellation)).expect("the resolver decides");

            assert_eq!(decision, expected, "the person's answer is the decision");
            assert_eq!(
                asked.lock().unwrap().len(),
                1,
                "the question must reach the surface exactly once"
            );
        }

        let mut unreachable = ChildPermissionResolver::Unreachable;
        let call = HeadlessToolCall {
            id: "call-2".into(),
            name: "native::write".into(),
            input: "{}".into(),
        };
        let decision = poll_once(unreachable.resolve(&call, &HeadlessTurnCancellation::new()))
            .expect("the resolver decides");
        assert_eq!(
            decision,
            PermissionDecision::Deny,
            "with nobody to ask, denying is still the only honest answer"
        );

        std::fs::remove_dir_all(&temporary).ok();
    }

    /// Pins the subagent-scope split (`PermissionSession::new()` above, never
    /// `with_temporary_bypass()`): a child never inherits a bypass, so a
    /// session that turned prompting off for itself does not turn it off for
    /// everything it delegates. That is about bypass, not about asking — a
    /// child with a surface behind it now asks, and
    /// [`a_child_with_a_surface_asks_the_person_instead_of_denying_for_them`]
    /// pins that half.
    #[test]
    fn isolated_turn_supervises_a_long_synthetic_conversation_without_stopping_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct IteratingProvider {
            requests: Arc<AtomicUsize>,
            queued: Arc<Mutex<Vec<Message>>>,
        }

        impl TurnProvider for IteratingProvider {
            fn queue_user_messages(
                &mut self,
                messages: Vec<Message>,
            ) -> Result<(), HeadlessTurnPortError> {
                self.queued.lock().unwrap().extend(messages);
                Ok(())
            }

            fn next_parts(
                &mut self,
                _events: &[TurnEvent],
                _cancellation: &HeadlessTurnCancellation,
            ) -> impl std::future::Future<Output = Result<Vec<MessagePart>, HeadlessTurnPortError>> + Send
            {
                let request = self.requests.fetch_add(1, Ordering::SeqCst) + 1;
                let parts = if request <= 34 {
                    vec![MessagePart::ToolCall {
                        id: format!("read-{request}"),
                        name: "native::read".into(),
                        input: r#"{"path":"notes.md"}"#.into(),
                    }]
                } else {
                    vec![MessagePart::Text(
                        "completed after sustained progress".into(),
                    )]
                };

                std::future::ready(Ok(parts))
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
            check_interval: 32,
            max_concurrency: 1,
            max_output_chars: 1_024,
        });
        let execution_id = registry
            .admit(agens_tools::TaskLaunchMode::Foreground)
            .unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let queued = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = agens_tools::ToolDispatcher::new();
        dispatcher
            .register_native("native::read", agens_core::ToolAccess::ReadOnly, ReadTool)
            .unwrap();

        let result = run_isolated_task_turn(
            IteratingProvider {
                requests: Arc::clone(&requests),
                queued: Arc::clone(&queued),
            },
            Arc::new(Mutex::new(dispatcher)),
            IsolatedTaskTurnContext {
                project_root: Path::new("."),
                dangerous_mode: false,
                cancellation: &HeadlessTurnCancellation::new(),
                progress: None,
                surface: &crate::child_catalog::resolve_child_surface(&[], &[], &[]).unwrap(),
                mailbox: TaskMailboxContext {
                    registry,
                    target: TaskMessageTarget::Execution(execution_id),
                },
                permission_prompter: None,
                grant_store_root: std::env::temp_dir(),
                origin: None,
            },
        );

        let Ok(output) = result else {
            panic!("long-running subagent must complete");
        };
        assert_eq!(output, "completed after sustained progress");
        assert_eq!(requests.load(Ordering::SeqCst), 35);
        let queued = queued.lock().unwrap();
        assert_eq!(queued.len(), 1);
        let MessagePart::Text(check) = &queued[0].parts[0] else {
            panic!("supervision check must be text");
        };
        assert!(check.contains("still making progress"), "{check}");
        assert!(check.contains("task_message"), "{check}");
    }

    #[test]
    fn supervision_drops_an_advisory_when_a_live_mailbox_is_concurrently_full() {
        let registry = TaskExecutionRegistry::new();
        let id = registry
            .admit(agens_tools::TaskLaunchMode::Background)
            .unwrap();
        let mut queued = 0;

        loop {
            match registry.send_message(
                TaskMessageSource::User,
                TaskMessageTarget::Execution(id),
                format!("concurrent message {queued}"),
            ) {
                Ok(()) => queued += 1,
                Err(TaskRegistryError::MessageLimit) => break,
                Err(error) => panic!("unexpected mailbox error: {error:?}"),
            }
        }

        assert!(queued > 0);
        assert!(supervision_advisory(&registry, id).unwrap().is_empty());
        assert_eq!(registry.snapshot(id).unwrap().terminal, None);
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
            let mut resolver = ChildPermissionResolver::Unreachable;
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
    fn a_child_call_the_catalog_cannot_serve_says_whether_it_was_denied_or_never_existed() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct UnregisteredToolProbeProvider {
            calls: AtomicUsize,
            name: &'static str,
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
                        name: self.name.into(),
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

        let refusal_of = |name: &'static str| {
            let registry = TaskExecutionRegistry::with_limits(agens_tools::TaskExecutionLimits {
                check_interval: 3,
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
                    name,
                },
                Arc::new(Mutex::new(dispatcher)),
                IsolatedTaskTurnContext {
                    project_root: Path::new("."),
                    dangerous_mode: false,
                    cancellation: &HeadlessTurnCancellation::new(),
                    progress: None,
                    surface: &crate::child_catalog::resolve_child_surface(&[], &[], &[]).unwrap(),
                    mailbox: TaskMailboxContext {
                        registry,
                        target: TaskMessageTarget::Main,
                    },
                    permission_prompter: None,
                    grant_store_root: std::env::temp_dir(),
                    origin: None,
                },
            );

            match result {
                Ok(output) => output,
                Err(_) => panic!("a refused tool call must not fail the turn"),
            }
        };

        let denied = refusal_of("native::write");
        assert!(
            denied.contains("permission denied"),
            "expected a denial naming permission denied, got: {denied}"
        );
        assert!(
            denied.contains("facts=true"),
            "expected denial_facts to accompany the denial, got: {denied}"
        );

        let bare = refusal_of("write");
        assert!(
            bare.contains("permission denied"),
            "a tool is advertised to the model under its bare name, and a call in that spelling \
             is the same denial as in the qualified one, got: {bare}"
        );

        let missing = refusal_of("native::sorcery");
        assert!(
            missing.contains("this session has no tool by that name"),
            "a name no tool answers to must not read as a denial, got: {missing}"
        );
        assert!(
            !missing.contains("permission denied"),
            "nothing denied a call there was no tool to deny, got: {missing}"
        );

        for output in [&denied, &bare, &missing] {
            assert!(
                !output.contains("invalid tool arguments"),
                "a call the catalog cannot serve must never surface as an argument error, got: \
                 {output}"
            );
        }
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
            crate::child_catalog::resolve_child_surface(parent_rules, declarations, &[]).unwrap();
        let registry = TaskExecutionRegistry::with_limits(agens_tools::TaskExecutionLimits {
            check_interval: 3,
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
            None,
            None,
            None,
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
                permission_prompter: None,
                grant_store_root: std::env::temp_dir(),
                origin: None,
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
    /// these" shape written with the wildcard spelled out. `rm*` selects a
    /// strict subset of `*`, so the deny outranks the allow it is written
    /// beside, in either order.
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

    /// `allow bash *` names exactly the calls `allow bash` names, so it must
    /// not reopen a tool an untargeted deny closed. Spelling the wildcard out
    /// is the house style the shipped configuration teaches, which is what
    /// made this the shape that reopened `rm`.
    #[test]
    fn an_explicit_wildcard_allow_cannot_reopen_an_untargeted_deny_in_either_order() {
        for (label, declarations) in [
            ("deny-first", ["deny bash", "allow bash *"]),
            ("allow-first", ["allow bash *", "deny bash"]),
        ] {
            let temporary = agens_fixtures::session_directory(&format!("wildcard-reopen-{label}"));
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
                "{label}: an explicit wildcard allow must not reopen an untargeted deny, \
                 got: {denied_output}"
            );
            assert!(
                victim.exists(),
                "{label}: the denied command must never have run, yet the victim file is gone"
            );

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    /// A `bash` rule names a command, and a shell expression runs several. The
    /// deny has to hold however the command was dressed up — this is the
    /// ordinary-evasion set, not a boundary against an adversary.
    #[test]
    fn a_command_deny_holds_through_the_ordinary_shell_evasions() {
        let temporary = agens_fixtures::session_directory("shell-evasions");
        let project_root = temporary.join("project");
        let victim = project_root.join("probe-victim.txt");
        std::fs::create_dir_all(&project_root).unwrap();

        let rules = declared(&temporary, &["deny bash rm*", "allow bash"]);

        for command in [
            "rm -rf probe-victim.txt",
            "cd . && rm -rf probe-victim.txt",
            "/bin/rm -rf probe-victim.txt",
            "sudo rm -rf probe-victim.txt",
            "echo x | xargs rm probe-victim.txt",
            "bash -c \\\"rm -rf probe-victim.txt\\\"",
            "echo $(rm -rf probe-victim.txt)",
            "\\\\rm -rf probe-victim.txt",
        ] {
            std::fs::write(&victim, "victim").unwrap();

            let denied_output = single_call_turn(
                &project_root,
                &rules,
                &[],
                false,
                "native::bash",
                &format!(r#"{{"command":"{command}"}}"#),
            );

            assert!(
                denied_output.contains("permission denied"),
                "{command} must be denied, got: {denied_output}"
            );
            assert!(
                victim.exists(),
                "{command} must never have run, yet the victim file is gone"
            );
        }

        let allowed_output = single_call_turn(
            &project_root,
            &rules,
            &[],
            false,
            "native::bash",
            r#"{"command":"echo hi && echo there"}"#,
        );
        assert!(
            !allowed_output.contains("permission denied"),
            "a compound command the deny does not name must still run, got: {allowed_output}"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// A path deny names a file, not a spelling of it.
    #[test]
    fn a_path_deny_holds_against_an_equivalent_spelling_of_the_same_path() {
        let temporary = agens_fixtures::session_directory("path-spelling-deny");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let rules = declared(&temporary, &["deny write .env*", "allow write"]);

        for path in ["./.env", ".//.env", "././.env", "./././/.env"] {
            let denied_output = single_call_turn(
                &project_root,
                &rules,
                &[],
                false,
                "native::write",
                &format!(r#"{{"path":"{path}","content":"SECRET=1"}}"#),
            );

            assert!(
                denied_output.contains("permission denied"),
                "{path} must not defeat a path deny, got: {denied_output}"
            );
            assert!(!project_root.join(".env").exists(), "{path} wrote the file");
        }

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// The same file written another way is the same file. A deny on the
    /// directory has to hold against every spelling that reaches it, and the
    /// victim's contents are what proves the call never ran.
    #[test]
    fn a_nested_path_deny_holds_against_every_spelling_that_reaches_the_file() {
        let temporary = agens_fixtures::session_directory("path-spelling-separators");
        let project_root = temporary.join("project");
        let victim = project_root.join("src").join("secret").join("key.txt");
        std::fs::create_dir_all(victim.parent().unwrap()).unwrap();
        std::fs::write(&victim, "victim").unwrap();

        let rules = declared(&temporary, &["allow write **", "deny write src/secret/**"]);

        for path in [
            "src/secret/key.txt",
            "src//secret/key.txt",
            "src///secret/key.txt",
            "./src//secret/key.txt",
            "src/./secret/key.txt",
            "src/.//./secret/key.txt",
            ".//src/secret//key.txt",
        ] {
            let denied_output = single_call_turn(
                &project_root,
                &rules,
                &[],
                false,
                "native::write",
                &format!(r#"{{"path":"{path}","content":"OVERWRITTEN"}}"#),
            );

            assert!(
                denied_output.contains("permission denied"),
                "{path} must be denied, got: {denied_output}"
            );
            assert_eq!(
                std::fs::read_to_string(&victim).unwrap(),
                "victim",
                "{path} reached the file the deny names"
            );
        }

        let allowed_output = single_call_turn(
            &project_root,
            &rules,
            &[],
            false,
            "native::write",
            r#"{"path":"src//main.rs","content":"fn main() {}"}"#,
        );
        assert!(
            !allowed_output.contains("permission denied"),
            "a path the deny does not name must still be written, got: {allowed_output}"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// A configured `ask` is an approval the operator asked for even under
    /// bypass. A child cannot reach the prompt, so it has to refuse — what it
    /// must never do is run the command unattended because a definition said
    /// `allow bash`.
    #[test]
    fn a_configured_ask_refuses_rather_than_running_unattended_in_a_child() {
        let temporary = agens_fixtures::session_directory("configured-ask-child");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let configured = [PermissionRule::global(
            PermissionDecision::Ask,
            PermissionPattern::Exact("native::bash".into()),
            PermissionPattern::glob_for_target_kind(
                "git push*",
                agens_core::PermissionTargetKind::FreeFormText,
            )
            .unwrap(),
        )];
        let rules = declared(&temporary, &["allow bash"]);

        for dangerous_mode in [false, true] {
            let denied_output = single_call_turn(
                &project_root,
                &rules,
                &configured,
                dangerous_mode,
                "native::bash",
                r#"{"command":"git push origin main"}"#,
            );

            assert!(
                denied_output.contains("permission denied"),
                "dangerous_mode={dangerous_mode}: a configured ask must not run unattended, \
                 got: {denied_output}"
            );
        }

        let allowed_output = single_call_turn(
            &project_root,
            &rules,
            &configured,
            false,
            "native::bash",
            r#"{"command":"echo hi"}"#,
        );
        assert!(
            !allowed_output.contains("permission denied"),
            "a command the configured ask does not name must still run, got: {allowed_output}"
        );

        std::fs::remove_dir_all(temporary).unwrap();
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
    /// own configured deny has to outrank it. Held in the configured floor
    /// rather than merged into the declared rule set, because merging the two
    /// would let a narrower declaration such as `allow bash git*` outrank the
    /// configured deny by containment.
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

    /// Resolves the `[permissions]` block of the configuration agens ships as
    /// its example, verbatim, into the rules a delegated child runs under. A
    /// probe written against a hand-built approximation of it would prove
    /// nothing about what an operator who copied that file actually gets.
    fn shipped_configured_rules(project_root: &Path) -> Vec<PermissionRule> {
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../example/config.toml")
            .canonicalize()
            .expect("the shipped example configuration must exist");
        let document = agens_config::parse_toml_document(
            &std::fs::read_to_string(shipped).expect("the shipped example must be readable"),
        )
        .expect("the shipped example must be valid TOML");
        let entries =
            agens_config::extract_permission_rules(&document, &Default::default()).unwrap();

        configured_permission_rules(
            &entries,
            &project_root.display().to_string(),
            |configured| Ok(PermissionPattern::Exact(configured.to_owned())),
        )
        .expect("the shipped rules must resolve")
    }

    /// The whole point of a path deny, against the tool that reports what it
    /// read: the configuration agens ships, a subagent that declares nothing,
    /// and a real secret in a real `.env`. `grep` returns the lines it matched,
    /// so a deny that does not reach it hands the file's contents to the model
    /// however loudly the configuration denies reading that path.
    ///
    /// A call that names the denied file is refused outright, which is the
    /// honest answer to a call whose whole subject a rule denies. A search
    /// rooted above it is a different question, answered per file while it
    /// runs; see
    /// [`a_search_rooted_above_a_denied_file_omits_it_and_still_reports_the_rest`].
    #[test]
    fn the_shipped_configuration_keeps_a_denied_file_out_of_a_grep_result() {
        let temporary = agens_fixtures::session_directory("shipped-config-grep");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(
            project_root.join(".env"),
            "OPENAI_API_KEY=sk-live-probe-do-not-leak\n",
        )
        .unwrap();
        std::fs::write(project_root.join("notes.md"), "OPENAI_API_KEY is set\n").unwrap();

        let configured = shipped_configured_rules(&project_root);

        for arguments in [
            r#"{"pattern":"OPENAI_API_KEY","path":".env"}"#,
            r#"{"pattern":"OPENAI_API_KEY","path":"./.env"}"#,
            r#"{"pattern":"OPENAI_API_KEY","path":".//.env"}"#,
            r#"{"pattern":"OPENAI_API_KEY","path":"./.env/."}"#,
        ] {
            let output = single_call_turn(
                &project_root,
                &[],
                &configured,
                false,
                "native::grep",
                arguments,
            );

            assert!(
                output.contains("permission denied"),
                "{arguments} must be denied under the shipped configuration, got: {output}"
            );
            assert!(
                !output.contains("sk-live-probe-do-not-leak"),
                "{arguments} handed the denied file's contents to the model: {output}"
            );
        }

        let allowed = single_call_turn(
            &project_root,
            &[],
            &configured,
            false,
            "native::grep",
            r#"{"pattern":"OPENAI_API_KEY","path":"notes.md"}"#,
        );
        assert!(
            !allowed.contains("permission denied"),
            "a search of a file no rule names must still run, got: {allowed}"
        );
        assert!(
            allowed.contains("OPENAI_API_KEY is set"),
            "the allowed search must still report the lines it matched, got: {allowed}"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// Lays out a worktree holding three secrets a rule denies and two
    /// ordinary files that match the same pattern, so a probe can tell a
    /// search that omitted the denied files from one that returned nothing.
    fn worktree_with_denied_secrets(project_root: &Path) {
        std::fs::create_dir_all(project_root.join("src/secret")).unwrap();
        std::fs::write(
            project_root.join(".env"),
            "OPENAI_API_KEY=sk-live-root-do-not-leak\n",
        )
        .unwrap();
        std::fs::write(
            project_root.join("src/.env"),
            "OPENAI_API_KEY=sk-live-nested-do-not-leak\n",
        )
        .unwrap();
        std::fs::write(
            project_root.join("src/secret/key"),
            "OPENAI_API_KEY=sk-live-keyfile-do-not-leak\n",
        )
        .unwrap();
        std::fs::write(project_root.join("notes.md"), "OPENAI_API_KEY is set\n").unwrap();
        std::fs::write(
            project_root.join("src/main.rs"),
            "// OPENAI_API_KEY comes from the environment\n",
        )
        .unwrap();
    }

    /// The one line a search adds when it withheld something. A caller is told
    /// that its result is not the whole corpus and nothing more: naming the
    /// files would hand over what the rule withholds, and counting them would
    /// let a caller that can re-root the search locate them by narrowing it.
    const WITHHELD_FILES_NOTICE: &str =
        "[some files were not read: a permission rule denies reading them]";

    fn withheld_notice(output: &str) -> Option<&str> {
        output
            .lines()
            .find(|line| line.starts_with("[some files were not read"))
    }

    const DENIED_SECRETS: &[&str] = &[
        "sk-live-root-do-not-leak",
        "sk-live-nested-do-not-leak",
        "sk-live-keyfile-do-not-leak",
    ];

    /// The rules an operator who copied the shipped example gets, plus the two
    /// that name a secret outside the `**/.env` shape — enough to show the
    /// filter is driven by the rules rather than by anything `.env`-specific.
    fn shipped_rules_denying_the_key_file(project_root: &Path) -> Vec<PermissionRule> {
        let mut rules = shipped_configured_rules(project_root);
        for tool in ["grep", "search"] {
            rules.push(PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::glob(tool).unwrap(),
                PermissionPattern::glob_for_target_kind(
                    "src/secret/**",
                    agens_core::PermissionTargetKind::Path,
                )
                .unwrap(),
            ));
        }
        rules
    }

    /// A search rooted above a denied file must not report what that file
    /// holds, and must still report everything else it matched. Refusing the
    /// whole call instead would make every recursive search under the shipped
    /// configuration useless, which is why the decision cannot be taken on the
    /// root alone.
    #[test]
    fn a_search_rooted_above_a_denied_file_omits_it_and_still_reports_the_rest() {
        let temporary = agens_fixtures::session_directory("rooted-search-filter");
        let project_root = temporary.join("project");
        worktree_with_denied_secrets(&project_root);

        let configured = shipped_rules_denying_the_key_file(&project_root);

        for (arguments, expected) in [
            (r#"{"pattern":"OPENAI_API_KEY"}"#, "OPENAI_API_KEY is set"),
            (
                r#"{"pattern":"OPENAI_API_KEY","path":"."}"#,
                "OPENAI_API_KEY is set",
            ),
            (
                r#"{"pattern":"OPENAI_API_KEY","path":"./"}"#,
                "OPENAI_API_KEY is set",
            ),
            (
                r#"{"pattern":"OPENAI_API_KEY","path":"src"}"#,
                "OPENAI_API_KEY comes from the environment",
            ),
            (
                r#"{"pattern":"OPENAI_API_KEY","path":"./src/."}"#,
                "OPENAI_API_KEY comes from the environment",
            ),
        ] {
            let output = single_call_turn(
                &project_root,
                &[],
                &configured,
                false,
                "native::grep",
                arguments,
            );

            for secret in DENIED_SECRETS {
                assert!(
                    !output.contains(secret),
                    "grep {arguments} reported a denied file's contents: {output}"
                );
            }
            assert!(
                output.contains(expected),
                "grep {arguments} must still report what it is allowed to, got: {output}"
            );
            assert_eq!(
                withheld_notice(&output),
                Some(WITHHELD_FILES_NOTICE),
                "grep {arguments} must say its result is not the whole corpus, \
                 and say it without naming or counting what it withheld: {output}"
            );
        }

        for (path, expected) in [
            (".", "OPENAI_API_KEY is set"),
            ("./", "OPENAI_API_KEY is set"),
            ("src", "OPENAI_API_KEY comes from the environment"),
        ] {
            let arguments = format!(r#"{{"path":"{path}","query":"OPENAI_API_KEY"}}"#);
            let output = single_call_turn(
                &project_root,
                &[],
                &configured,
                false,
                "native::search",
                &arguments,
            );

            for secret in DENIED_SECRETS {
                assert!(
                    !output.contains(secret),
                    "search {arguments} reported a denied file's contents: {output}"
                );
            }
            assert_eq!(
                withheld_notice(&output),
                Some(WITHHELD_FILES_NOTICE),
                "search {arguments} must say its result is not the whole corpus, \
                 and say it without naming or counting what it withheld: {output}"
            );
            assert!(
                output.contains(expected),
                "search {arguments} must still report what it is allowed to, got: {output}"
            );
        }

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// Runs one git command in a probe worktree, with an identity of its own so
    /// the machine's configuration cannot decide whether the probe can commit.
    fn git(root: &Path, arguments: &[&str]) {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "agens")
            .env("GIT_AUTHOR_EMAIL", "agens@example.invalid")
            .env("GIT_COMMITTER_NAME", "agens")
            .env("GIT_COMMITTER_EMAIL", "agens@example.invalid")
            .output()
            .expect("git must be available to probe git_read");

        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The secrets a `git_read diff` could reach, one per place a diff can read
    /// from: the working tree, the index, and history at either end of a
    /// revision range. Each one is a distinct string so a probe that leaks says
    /// which shape leaked it.
    const GIT_READ_SECRETS: &[&str] = &[
        "sk-live-gitread-worktree-do-not-leak",
        "sk-live-gitread-staged-do-not-leak",
        "sk-live-gitread-history-do-not-leak",
    ];

    /// A repository holding a secret in `.env` and ordinary prose in `notes.md`
    /// at every point a diff can be taken from: the `base` branch, `main`, the
    /// index and the working tree. A probe over it can tell a diff that
    /// withheld the secret from one that returned nothing at all.
    fn repository_with_a_denied_secret(project_root: &Path) {
        std::fs::create_dir_all(project_root).unwrap();
        git(project_root, &["init", "--initial-branch=main", "--quiet"]);

        std::fs::write(project_root.join(".env"), "OPENAI_API_KEY=placeholder\n").unwrap();
        std::fs::write(project_root.join("notes.md"), "notes: first\n").unwrap();
        git(project_root, &["add", "-A"]);
        git(project_root, &["commit", "--quiet", "-m", "first"]);
        git(project_root, &["branch", "base"]);

        std::fs::write(
            project_root.join(".env"),
            "OPENAI_API_KEY=sk-live-gitread-history-do-not-leak\n",
        )
        .unwrap();
        std::fs::write(project_root.join("notes.md"), "notes: second\n").unwrap();
        git(project_root, &["add", "-A"]);
        git(project_root, &["commit", "--quiet", "-m", "second"]);

        std::fs::write(
            project_root.join(".env"),
            "OPENAI_API_KEY=sk-live-gitread-staged-do-not-leak\n",
        )
        .unwrap();
        std::fs::write(project_root.join("notes.md"), "notes: third\n").unwrap();
        git(project_root, &["add", "-A"]);

        std::fs::write(
            project_root.join(".env"),
            "OPENAI_API_KEY=sk-live-gitread-worktree-do-not-leak\n",
        )
        .unwrap();
        std::fs::write(project_root.join("notes.md"), "notes: fourth\n").unwrap();
    }

    /// `git_read diff` hands the model the contents of the files it names, so a
    /// path deny that does not reach it is a deny in name only — the shipped
    /// configuration would refuse `read`, `grep` and `search` on `.env` while a
    /// one-word tool swap printed the same bytes.
    ///
    /// Every place a diff reads from is probed, because fixing the working-tree
    /// shape alone would leave the index and every revision range open.
    #[test]
    fn the_shipped_configuration_keeps_a_denied_file_out_of_a_git_read_diff() {
        let temporary = agens_fixtures::session_directory("shipped-config-git-read");
        let project_root = temporary.join("project");
        repository_with_a_denied_secret(&project_root);

        let configured = shipped_configured_rules(&project_root);

        for (arguments, expected) in [
            (r#"{"operation":"diff"}"#, "notes: fourth"),
            (r#"{"operation":"diff","staged":true}"#, "notes: third"),
            (r#"{"operation":"diff","base":"base"}"#, "notes: fourth"),
            (
                r#"{"operation":"diff","base":"base","head":"main"}"#,
                "notes: second",
            ),
        ] {
            let output = single_call_turn(
                &project_root,
                &[],
                &configured,
                false,
                "native::git_read",
                arguments,
            );

            for secret in GIT_READ_SECRETS {
                assert!(
                    !output.contains(secret),
                    "git_read {arguments} handed a denied file's contents to the model: {output}"
                );
            }
            assert!(
                output.contains(expected),
                "git_read {arguments} must still show the files no rule denies, got: {output}"
            );
            assert_eq!(
                withheld_notice(&output),
                Some(WITHHELD_FILES_NOTICE),
                "git_read {arguments} must say its result is not the whole patch, \
                 and say it without naming or counting what it withheld: {output}"
            );
        }

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// The operations that report names or refs rather than contents are left
    /// alone: `status` reports the same paths `list` and `glob` already report
    /// under the shipped configuration, and `log`, `branch_merged` and
    /// `merge_base` report commit subjects and ref names. Filtering them would
    /// cost the operator the notice on a result that never held a file.
    #[test]
    fn the_git_read_operations_that_report_no_file_contents_are_answered_in_full() {
        let temporary = agens_fixtures::session_directory("git-read-nameless-operations");
        let project_root = temporary.join("project");
        repository_with_a_denied_secret(&project_root);

        let configured = shipped_configured_rules(&project_root);

        for (arguments, expected) in [
            (r#"{"operation":"status"}"#, ".env"),
            (r#"{"operation":"log"}"#, "second"),
            (r#"{"operation":"branch_merged","base":"main"}"#, "base"),
            (
                r#"{"operation":"merge_base","base":"base","head":"main"}"#,
                "\n",
            ),
        ] {
            let output = single_call_turn(
                &project_root,
                &[],
                &configured,
                false,
                "native::git_read",
                arguments,
            );

            for secret in GIT_READ_SECRETS {
                assert!(
                    !output.contains(secret),
                    "git_read {arguments} reported a denied file's contents: {output}"
                );
            }
            assert!(
                output.contains(expected),
                "git_read {arguments} must still answer in full, got: {output}"
            );
            assert_eq!(
                withheld_notice(&output),
                None,
                "git_read {arguments} reports no file contents, so it withholds nothing \
                 and must not claim otherwise: {output}"
            );
        }

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// A rename is one change reported under two names, so a rule naming either
    /// name has to withhold the whole of it. Withholding only the name the rule
    /// matched leaves git unable to pair the two sides and printing the file it
    /// was supposed to withhold, in full, as a new file under the other name.
    #[test]
    fn a_diff_that_renames_a_denied_file_reports_neither_side_of_it() {
        let temporary = agens_fixtures::session_directory("git-read-rename");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        git(&project_root, &["init", "--initial-branch=main", "--quiet"]);

        std::fs::write(
            project_root.join(".env"),
            "OPENAI_API_KEY=sk-live-gitread-renamed-do-not-leak\nregion=eu\n",
        )
        .unwrap();
        std::fs::write(project_root.join("notes.md"), "notes: first\n").unwrap();
        git(&project_root, &["add", "-A"]);
        git(&project_root, &["commit", "--quiet", "-m", "first"]);

        git(&project_root, &["mv", ".env", "settings.ini"]);
        std::fs::write(
            project_root.join("settings.ini"),
            "OPENAI_API_KEY=sk-live-gitread-renamed-do-not-leak\nregion=us\n",
        )
        .unwrap();
        std::fs::write(project_root.join("notes.md"), "notes: second\n").unwrap();
        git(&project_root, &["add", "-A"]);

        let output = single_call_turn(
            &project_root,
            &[],
            &shipped_configured_rules(&project_root),
            false,
            "native::git_read",
            r#"{"operation":"diff","staged":true}"#,
        );

        assert!(
            !output.contains("sk-live-gitread-renamed-do-not-leak"),
            "a denied file renamed out of the shape the rule names still leaked: {output}"
        );
        assert!(
            !output.contains("settings.ini"),
            "the other side of the rename must go with it: {output}"
        );
        assert!(
            output.contains("notes: second"),
            "the files no rule denies must still be reported: {output}"
        );
        assert_eq!(
            withheld_notice(&output),
            Some(WITHHELD_FILES_NOTICE),
            "the caller must be told the patch is not the whole patch: {output}"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// The target of a `git_read` call is still the operation it names, so a
    /// rule reading `deny git_read(diff)` denies diffs and nothing else. The
    /// per-file question is asked under the same tool name but against paths,
    /// and the two value spaces do not collide.
    #[test]
    fn a_deny_written_against_a_git_read_operation_still_denies_that_operation() {
        let temporary = agens_fixtures::session_directory("git-read-operation-deny");
        let project_root = temporary.join("project");
        repository_with_a_denied_secret(&project_root);

        let mut configured = shipped_configured_rules(&project_root);
        configured.push(PermissionRule::global(
            PermissionDecision::Deny,
            PermissionPattern::glob("git_read").unwrap(),
            PermissionPattern::glob_for_target_kind(
                "diff",
                agens_core::permission_target_kind_for_tool("git_read"),
            )
            .unwrap(),
        ));

        let denied = single_call_turn(
            &project_root,
            &[],
            &configured,
            false,
            "native::git_read",
            r#"{"operation":"diff"}"#,
        );
        assert!(
            denied.contains("permission denied"),
            "deny git_read(diff) must refuse a diff outright, got: {denied}"
        );

        let allowed = single_call_turn(
            &project_root,
            &[],
            &configured,
            false,
            "native::git_read",
            r#"{"operation":"status"}"#,
        );
        assert!(
            !allowed.contains("permission denied"),
            "deny git_read(diff) must say nothing about any other operation, got: {allowed}"
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

        let surface = resolve_child_surface(&[], &explore.permission_rules, &[]).unwrap();
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
