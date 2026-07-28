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

use crate::tools::runtime::production_child_tool_runtime;
use crate::{Bootstrap, SubagentErrorKind, block_on_headless_turn};
use agens_core::DiscardCompletedTurnRepository;
use agens_diagnostics::diagnostic_store;
use agens_dispatch::ProductionToolDispatcher;
use agens_permissions::{ProductionPermissionGate, SharedToolDispatcher};

#[derive(Clone, Copy)]
pub(crate) enum ChildRunError {
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
    pub(crate) const fn diagnostic_class(self) -> ProviderDiagnosticClass {
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

    pub(crate) const fn tui_kind(self) -> Option<SubagentErrorKind> {
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

    pub(crate) const fn task_runner_error(self) -> TaskRunnerError {
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

pub(crate) struct ProductionTaskExecutionContext<'a> {
    pub(crate) bootstrap: &'a Bootstrap,
    pub(crate) project_root: &'a Path,
    pub(crate) dangerous_mode: bool,
    pub(crate) cancellation: &'a HeadlessTurnCancellation,
    pub(crate) progress: Option<&'a TurnProgressSink>,
    pub(crate) diagnostic_reference: &'a str,
    pub(crate) task_registry: &'a TaskExecutionRegistry,
    pub(crate) execution_id: agens_tools::TaskExecutionId,
}

pub(crate) fn run_production_task(
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
            let api_key = bootstrap
                .openai_api_key
                .clone()
                .ok_or(ChildRunError::Runtime)?;
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
fn task_provider_base_url(
    bootstrap: &Bootstrap,
    project_root: &Path,
) -> Result<Option<String>, agens_error::CliError> {
    let session_root =
        agens_bootstrap::session_root::SessionRoot::confined_to(project_root.to_path_buf());
    let session_config =
        agens_bootstrap::session_config::SessionConfig::resolve(&session_root, bootstrap)?;
    Ok(session_config.provider_base_url().map(ToOwned::to_owned))
}

pub(crate) struct TaskMailboxProvider<P> {
    inner: P,
    registry: Option<TaskExecutionRegistry>,
    target: TaskMessageTarget,
}

impl<P> TaskMailboxProvider<P> {
    pub(crate) fn new(
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

#[cfg(test)]
mod tests {
    use agens_bus::BridgeTx;
    use agens_tools::TaskLaunchMode;

    use super::*;
    use crate::dispatch::launch_selected_tui_task;
    use crate::permission_prompt::{TuiPermissionPrompter, production_tui_permission_bridge};
    use crate::test_support::{tui_session_bootstrap, tui_session_directory};
    use crate::tools::runner::{ProductionTaskRunner, TuiTaskControls, TuiTaskLifecycleBridge};
    use crate::tools::task::production_tui_task_runtime_with_runner;
    use agens_dispatch::TuiSelectedTaskLaunch;
    use agens_session::context::SessionContext;
    use agens_session::context::current_session_timestamp;

    struct RecordingMailboxProvider {
        queued: Arc<Mutex<Vec<Vec<Message>>>>,
    }

    impl TurnProvider for RecordingMailboxProvider {
        fn queue_user_messages(
            &mut self,
            messages: Vec<Message>,
        ) -> Result<(), HeadlessTurnPortError> {
            self.queued.lock().unwrap().push(messages);
            Ok(())
        }

        async fn next_parts(
            &mut self,
            _: &[TurnEvent],
            _: &HeadlessTurnCancellation,
        ) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
            Ok(vec![MessagePart::Text("ok".into())])
        }
    }

    /// A subagent turn confined to root A must not send its conversation to root B's configured
    /// `provider.base_url` — the same confinement shape headless turns get, but for the child
    /// (subagent) provider construction path, which reads its endpoint through
    /// [`task_provider_base_url`] rather than `headless_turn_provider_base_url`.
    #[test]
    fn a_task_runtimes_provider_base_url_is_scoped_to_its_own_root_not_the_bootstraps_process_root()
    {
        use std::collections::BTreeMap;

        use crate::CliDependencies;
        use crate::deps::bootstrap;

        let temporary = std::env::temp_dir().join(format!(
            "agens-task-runtime-provider-base-url-scope-{}",
            std::process::id()
        ));
        let config_home = temporary.join("config");
        let root_b = temporary.join("root-b/project");
        let root_a = temporary.join("root-a/project");

        let mut files = BTreeMap::new();
        files.insert(
            root_b.join(".agens/config.toml"),
            "[provider]\nbase_url = \"https://root-b.invalid/exfiltrate\"\n".to_owned(),
        );

        let bootstrap_from_root_b = bootstrap(&CliDependencies::for_test(
            root_b,
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            files.clone(),
        ))
        .unwrap();

        let base_url = task_provider_base_url(&bootstrap_from_root_b, &root_a).unwrap();

        assert_eq!(
            base_url, None,
            "a provider endpoint configured for a DIFFERENT project root must not silently \
             govern a subagent turn confined to this root"
        );

        files.insert(
            root_a.join(".agens/config.toml"),
            "[provider]\nbase_url = \"https://root-a.invalid/own-endpoint\"\n".to_owned(),
        );
        let bootstrap_from_root_b = bootstrap(&CliDependencies::for_test(
            temporary.join("root-b/project"),
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            files,
        ))
        .unwrap();

        let base_url = task_provider_base_url(&bootstrap_from_root_b, &root_a).unwrap();

        assert_eq!(
            base_url.as_deref(),
            Some("https://root-a.invalid/own-endpoint"),
            "a session's OWN project configuration must still set its subagent turn's provider \
             endpoint"
        );

        std::fs::remove_dir_all(&temporary).ok();
        std::fs::remove_dir_all(bootstrap_from_root_b.data_directory()).ok();
    }

    #[test]
    fn task_mailbox_provider_injects_typed_user_messages_only_at_request_safe_points() {
        let registry = TaskExecutionRegistry::new();
        let id = registry.admit(TaskLaunchMode::Background).unwrap();
        registry
            .send_message(
                TaskMessageSource::Main,
                TaskMessageTarget::Execution(id),
                "first".into(),
            )
            .unwrap();
        let queued = Arc::new(Mutex::new(Vec::new()));
        let mut provider = TaskMailboxProvider::new(
            RecordingMailboxProvider {
                queued: Arc::clone(&queued),
            },
            Some(registry.clone()),
            TaskMessageTarget::Execution(id),
        );
        let cancellation = HeadlessTurnCancellation::new();

        block_on_headless_turn(provider.next_parts(&[], &cancellation))
            .unwrap()
            .unwrap();
        registry
            .send_message(
                TaskMessageSource::User,
                TaskMessageTarget::Execution(id),
                "second".into(),
            )
            .unwrap();
        block_on_headless_turn(provider.next_parts(&[], &cancellation))
            .unwrap()
            .unwrap();

        let queued = queued.lock().unwrap();
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0][0].role, Role::User);
        assert_eq!(
            queued[0][0].parts,
            [MessagePart::Text(
                "[coordination source=main untrusted=true]\nfirst".into()
            )]
        );
        assert_eq!(
            queued[1][0].parts,
            [MessagePart::Text(
                "[coordination source=user untrusted=true]\nsecond".into()
            )]
        );
    }

    #[test]
    fn p1c3_completed_background_subagent_notifies_the_next_main_turn() {
        let temporary = tui_session_directory("subagent-completion-notice");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let (events, _receiver) = BridgeTx::bounded(16);
        let controls = TuiTaskControls::default();
        let session = Arc::new(Mutex::new(SessionContext {
            selected_subagent: Some("reviewer".into()),
            ..SessionContext::fresh()
        }));
        let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, controls.clone())
            .with_session_writer(bootstrap.clone(), Arc::clone(&session));
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
            &agens_tools::SkillCatalog::default(),
            Box::new(TuiPermissionPrompter(production_tui_permission_bridge().0)),
            ProductionTaskRunner::with_progress_probe(
                bootstrap.clone(),
                agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
                Arc::new(Mutex::new(Vec::new())),
                Vec::new(),
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
        let launched_at = current_session_timestamp();

        assert_eq!(
            launch_selected_tui_task(&mut runtime, &session, "review task", true, &cancellation),
            Ok(TuiSelectedTaskLaunch::Dispatched)
        );
        crate::test_support::wait_for(
            "a completed background subagent to persist one durable turn",
            || session.lock().unwrap().identifier,
        );

        let queued = Arc::new(Mutex::new(Vec::new()));
        let mut provider = TaskMailboxProvider::new(
            RecordingMailboxProvider {
                queued: Arc::clone(&queued),
            },
            Some(controls.0.clone()),
            TaskMessageTarget::Main,
        );
        // The notice is posted after the turn is persisted, so the identifier the
        // launch waits on is set strictly earlier. Drain until the notice lands
        // rather than assuming one drain is enough.
        crate::test_support::wait_for("the completed subagent's mailbox notice", || {
            block_on_headless_turn(provider.next_parts(&[], &cancellation))
                .unwrap()
                .unwrap();
            queued
                .lock()
                .unwrap()
                .iter()
                .any(|batch| !batch.is_empty())
                .then_some(())
        });
        // Draining again must add nothing: the notice is delivered once, which is
        // the property the old single-drain assertion was standing in for.
        block_on_headless_turn(provider.next_parts(&[], &cancellation))
            .unwrap()
            .unwrap();

        let queued = queued.lock().unwrap();
        let delivered = queued
            .iter()
            .filter(|batch| !batch.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(delivered.len(), 1, "{queued:?}");
        assert_eq!(delivered[0].len(), 1);
        assert_eq!(delivered[0][0].role, Role::User);
        let [MessagePart::Text(notice)] = delivered[0][0].parts.as_slice() else {
            panic!("a mailbox notice is text: {:?}", delivered[0][0].parts)
        };
        let (label, detail) = notice
            .split_once('\n')
            .expect("mailbox notices are labelled untrusted");
        assert_eq!(label, "[coordination source=subagent:1 untrusted=true]");
        let completed_at = detail
            .split_once("completed_at=")
            .and_then(|(_, tail)| tail.split_whitespace().next())
            .and_then(|value| value.parse::<i64>().ok())
            .expect("the notice states when the subagent finished");
        assert!(completed_at >= launched_at);
        assert_eq!(
            detail,
            format!(
                "subagent #1 (reviewer) finished with state=completed completed_at={completed_at} \
                 (unix seconds). The full result is recorded in this session history; run \
                 task_control action=status id=1 for the recorded outcome."
            )
        );

        drop(queued);
        std::fs::remove_dir_all(temporary).unwrap();
    }
}
