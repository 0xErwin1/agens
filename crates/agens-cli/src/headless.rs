//! Production headless chat: builds the provider for the configured backend, runs a single
//! turn to completion under the session attempt lifecycle, and reports the requested-subagent
//! note used when a turn is interrupted before it completes.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agens_core::{
    BeginSessionAttemptError, CompletedTurnRepository, CompletedTurnSnapshot,
    CompletedTurnStoreError, HeadlessTurnCancellation, HeadlessTurnError, Message, MessagePart,
    PermissionMode, PermissionSession, Role, SessionMetadata, TurnEvent, TurnProgressSink,
    run_headless_turn_with_max_iterations_and_progress,
};
use agens_providers::{
    ChatGptResponsesProvider, OpenAiFunctionTool, OpenAiResponsesProvider, ProgressAwareProvider,
    ProviderDiagnosticScope,
};
use agens_store::{PermissionGrantStore, SessionStore};
use agens_tools::{EffectiveCapabilitySet, SkillCatalog, TaskMessageTarget};
use agens_tui::TuiPermissionBridge;

use crate::dispatch::ProductionToolDispatcher;
use crate::error::{CliError, ExitStatus};
use crate::permissions::{
    ProductionPermissionGate, ProductionPermissionPrompter, ProductionPermissionResolver,
    ProductionPromptAuthorization, TtyPermissionPrompter, permission_policy,
};
use crate::session::attempt::{
    AttemptLifecycleError, PartialTurnRecord, active_session_attempts,
    run_session_attempt_lifecycle_with_terminal_writer, write_terminal_attempt,
};
use crate::tools::child::TaskMailboxProvider;
use crate::tools::runtime::production_tool_runtime_for_parent;
use crate::tools::task::ProductionTuiTaskRuntime;
use crate::tui::agents::{TuiAgentModelValidator, tui_agent_catalog};
use crate::tui::provider::TuiProvider;
use crate::turns::{completed_session_turn, next_session_metadata, sanitize_subagent_summary};
use crate::{
    Bootstrap, cancellation_result, effective_max_iterations, operation_diagnostics,
    record_parent_terminal,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadlessChatRequest {
    pub prompt: String,
    pub(crate) history: Vec<Message>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub max_iterations: Option<usize>,
    pub mode: PermissionMode,
    pub dangerously_allow_all: bool,
    pub dangerous_mode: bool,
    pub(crate) request_config: agens_core::RequestConfig,
    pub(crate) session_reasoning_effort: Option<agens_core::ReasoningEffort>,
    pub(crate) session: Option<SessionMetadata>,
    pub(crate) active_agent: Option<String>,
    pub(crate) effective_capabilities: Option<EffectiveCapabilitySet>,
    pub(crate) pending_system_reminder: Option<String>,
    pub(crate) skills: Option<Arc<SkillCatalog>>,
}

const MAX_NOTED_REQUESTED_SUBAGENTS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RequestedSubagent {
    pub(crate) agent: String,
    pub(crate) description: String,
}

/// Describes the turn as interrupted rather than cancelled by the user, because an expired
/// deadline reaches this path with the same terminal status as an explicit cancellation.
pub(crate) fn interrupted_turn_note(requested: &[RequestedSubagent]) -> String {
    let mut note = "[interrupted] The previous turn stopped before this assistant produced a \
                    result. Results of tools it had requested are unavailable, so their effects \
                    are unverified."
        .to_owned();
    if requested.is_empty() {
        return note;
    }

    note.push_str(" Subagents requested in that turn: ");
    note.push_str(
        &requested
            .iter()
            .map(|subagent| format!("{} — \"{}\"", subagent.agent, subagent.description))
            .collect::<Vec<_>>()
            .join("; "),
    );
    note.push('.');
    note
}

pub(crate) fn record_requested_subagent(
    requested: &Mutex<Vec<RequestedSubagent>>,
    event: &TurnEvent,
) {
    let TurnEvent::ToolCallRequested { name, input, .. } = event else {
        return;
    };
    if name != "native::task" {
        return;
    }
    let Some(subagent) = serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|value| {
            Some(RequestedSubagent {
                agent: sanitize_subagent_summary(value.get("agent")?.as_str()?),
                description: sanitize_subagent_summary(value.get("description")?.as_str()?),
            })
        })
    else {
        return;
    };

    if let Ok(mut requested) = requested.lock()
        && requested.len() < MAX_NOTED_REQUESTED_SUBAGENTS
        && !requested.contains(&subagent)
    {
        requested.push(subagent);
    }
}

pub(crate) fn run_production_headless_chat(
    request: HeadlessChatRequest,
    bootstrap: &Bootstrap,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    run_production_headless_chat_with_progress(
        request,
        bootstrap,
        cancellation,
        None,
        None,
        None,
        None,
    )
    .map(|completion| completion.text)
    .map_err(HeadlessChatFailure::into_error)
}

pub(crate) struct HeadlessChatCompletion {
    pub(crate) text: String,
    pub(crate) metadata: SessionMetadata,
    pub(crate) messages: Vec<Message>,
}

/// Failed turn plus any history the attempt already persisted, so the caller can adopt the
/// session the failed attempt belongs to instead of starting a new one on the next turn.
#[derive(Debug)]
pub(crate) struct HeadlessChatFailure {
    pub(crate) error: CliError,
    pub(crate) partial: Option<Box<PartialTurnRecord>>,
}

impl HeadlessChatFailure {
    fn into_error(self) -> CliError {
        self.error
    }

    fn map_error(self, map: impl FnOnce(CliError) -> CliError) -> Self {
        Self {
            error: map(self.error),
            partial: self.partial,
        }
    }
}

impl From<CliError> for HeadlessChatFailure {
    fn from(error: CliError) -> Self {
        Self {
            error,
            partial: None,
        }
    }
}

struct HeadlessProviderContext<'a> {
    bootstrap: &'a Bootstrap,
    cancellation: &'a HeadlessTurnCancellation,
    progress: Option<&'a TurnProgressSink>,
    permission_bridge: Option<TuiPermissionBridge>,
    task_runtime: Option<&'a ProductionTuiTaskRuntime>,
    diagnostic_reference: &'a str,
    include_system_prompt: bool,
}

pub(crate) fn run_production_headless_chat_with_progress(
    mut request: HeadlessChatRequest,
    bootstrap: &Bootstrap,
    cancellation: &HeadlessTurnCancellation,
    progress: Option<&TurnProgressSink>,
    permission_bridge: Option<TuiPermissionBridge>,
    task_runtime: Option<&ProductionTuiTaskRuntime>,
    operation_reference: Option<&str>,
) -> Result<HeadlessChatCompletion, HeadlessChatFailure> {
    #[cfg(test)]
    crate::test_support::note_production_provider_runtime();

    let source = bootstrap
        .provider_type()
        .and_then(TuiProvider::parse)
        .map(TuiProvider::source)
        .ok_or_else(|| CliError::configuration("task provider is unavailable"))?;
    let validator = TuiAgentModelValidator::for_source(source)?;
    let has_task = tui_agent_catalog(bootstrap, &validator)?
        .subagents()
        .any(|agent| agent.mode == agens_core::AgentMode::Subagent);
    if has_task {
        let base = request
            .system_prompt
            .take()
            .or_else(|| bootstrap.system_prompt.clone())
            .unwrap_or_else(|| "You are Agens, a helpful coding agent.".to_owned());
        request.system_prompt = Some(explicit_task_delegation_prompt(&base));
    }

    let diagnostics = operation_diagnostics(
        bootstrap,
        ProviderDiagnosticScope::Parent,
        operation_reference,
    );
    let diagnostic_reference = diagnostics.reference;
    let provider_diagnostics = diagnostics.provider;
    let result = match bootstrap.provider_type() {
        Some("openai-api") => {
            let api_key = bootstrap.openai_api_key.clone().ok_or_else(|| {
                CliError::authentication("OpenAI API authentication is unavailable")
            })?;
            run_production_headless_chat_with_provider(
                request,
                HeadlessProviderContext {
                    bootstrap,
                    cancellation,
                    progress,
                    permission_bridge,
                    task_runtime,
                    diagnostic_reference: &diagnostic_reference,
                    include_system_prompt: true,
                },
                move |model, messages, tools, request_config| {
                    OpenAiResponsesProvider::from_api_key_with_messages_and_tools_and_timeout(
                        api_key,
                        bootstrap.provider_base_url(),
                        model,
                        messages,
                        tools,
                        std::time::Duration::from_secs(120),
                    )
                    .map(|provider| {
                        provider
                            .with_parallel_tool_calls(bootstrap.parallel_tool_calls)
                            .with_request_config(request_config)
                            .with_diagnostics(provider_diagnostics)
                    })
                    .map_err(|error| {
                        provider_construction_error(
                            error,
                            "OpenAI API authentication is unavailable",
                        )
                    })
                },
            )
        }
        Some("openai-chatgpt") => {
            let credentials_path = bootstrap.paths.credentials.clone();
            let instructions = request
                .system_prompt
                .clone()
                .or_else(|| bootstrap.system_prompt.clone())
                .unwrap_or_else(|| "You are Agens, a helpful coding agent.".to_owned());
            run_production_headless_chat_with_provider(
                request,
                HeadlessProviderContext {
                    bootstrap,
                    cancellation,
                    progress,
                    permission_bridge,
                    task_runtime,
                    diagnostic_reference: &diagnostic_reference,
                    include_system_prompt: false,
                },
                move |model, messages, tools, request_config| {
                    ChatGptResponsesProvider::from_credentials_with_messages_and_tools_and_timeout_and_auth_url(
                        &credentials_path,
                        bootstrap.provider_base_url(),
                        None,
                        model,
                        instructions,
                        messages,
                        tools,
                        std::time::Duration::from_secs(120),
                    )
                    .map(|provider| {
                        provider
                            .with_parallel_tool_calls(bootstrap.parallel_tool_calls)
                            .with_request_config(request_config)
                            .with_diagnostics(provider_diagnostics)
                    })
                    .map_err(|error| {
                        provider_construction_error(
                            error,
                            "ChatGPT credentials are unavailable or invalid",
                        )
                    })
                },
            )
        }
        _ => Err(HeadlessChatFailure::from(CliError::configuration(
            "headless chat requires provider.type = \"openai-api\" or \"openai-chatgpt\"",
        ))),
    };
    result.map_err(|failure| {
        record_parent_terminal(bootstrap, &diagnostic_reference, &failure.error);
        failure.map_error(|error| error.with_diagnostic_reference(&diagnostic_reference))
    })
}

/// Keeps a rejected local encode of the resumed history distinguishable from missing or invalid
/// credentials, so malformed persisted history is not reported as an authentication failure.
fn provider_construction_error(error: agens_core::Error, authentication: &str) -> CliError {
    match error {
        agens_core::Error::Auth(_) => CliError::authentication(authentication),
        agens_core::Error::Config(_) => {
            CliError::configuration("provider request could not be configured")
        }
        _ => CliError::new(
            ExitStatus::Failure,
            "provider",
            "session history could not be encoded for the provider request",
        ),
    }
}

fn run_production_headless_chat_with_provider<P>(
    request: HeadlessChatRequest,
    context: HeadlessProviderContext<'_>,
    build_provider: impl FnOnce(
        String,
        Vec<Message>,
        Vec<OpenAiFunctionTool>,
        agens_core::RequestConfig,
    ) -> Result<P, CliError>,
) -> Result<HeadlessChatCompletion, HeadlessChatFailure>
where
    P: ProgressAwareProvider + Send,
{
    let model = request
        .model
        .clone()
        .or_else(|| context.bootstrap.model().map(ToOwned::to_owned))
        .unwrap_or_else(|| match context.bootstrap.provider_type() {
            Some("openai-chatgpt") => "gpt-5.5".to_owned(),
            _ => "gpt-4.1".to_owned(),
        });
    let session_provider = context.bootstrap.provider_type().map(str::to_owned);
    let session_model = model.clone();
    let session_effort = request
        .session_reasoning_effort
        .or_else(|| request.request_config.reasoning_effort());
    let project_root = context
        .bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    let (provider_tools, tool_runtime) = match context.task_runtime {
        Some(task_runtime) => (
            task_runtime.provider_tools.clone(),
            Arc::clone(&task_runtime.dispatcher),
        ),
        None => production_tool_runtime_for_parent(
            context.bootstrap,
            project_root,
            request.skills.as_deref(),
            model.clone(),
            request.request_config.clone(),
            Some(context.diagnostic_reference.to_owned()),
        )?,
    };
    let task_registry = context
        .task_runtime
        .map(|runtime| runtime.task_registry.clone());
    let project = project_root.display().to_string();
    let policy = permission_policy(
        context.bootstrap.permission_rules(),
        &project,
        request.mode,
        &tool_runtime,
        request.effective_capabilities.as_ref(),
    )?;
    let grant_store = PermissionGrantStore::open(context.bootstrap.data_directory())
        .map_err(|_| CliError::storage("permission grants are unavailable"))?;
    let grants = grant_store
        .grants_for_project(&project)
        .map_err(|_| CliError::storage("permission grants are unavailable"))?;
    let grants = Arc::new(Mutex::new(grants));
    let session = if request.dangerously_allow_all {
        PermissionSession::with_temporary_bypass()
    } else {
        PermissionSession::new()
    };
    let pending = Arc::new(Mutex::new(BTreeMap::new()));
    let prompts = Arc::new(Mutex::new(BTreeMap::new()));
    let mut repository = DiscardCompletedTurnRepository;
    let mut gate = ProductionPermissionGate::new(
        policy.clone(),
        Arc::clone(&grants),
        session,
        project.clone(),
        Arc::clone(&tool_runtime),
        Arc::clone(&pending),
        Arc::clone(&prompts),
    );
    let mut resolver = ProductionPermissionResolver::new(
        context.permission_bridge.map_or(
            ProductionPermissionPrompter::Tty(TtyPermissionPrompter),
            ProductionPermissionPrompter::Tui,
        ),
        grant_store,
        grants,
        prompts,
        ProductionPromptAuthorization {
            policy,
            session,
            project,
            dispatcher: Arc::clone(&tool_runtime),
            allowed: Arc::clone(&pending),
        },
    );
    let mut dispatcher = ProductionToolDispatcher::new(tool_runtime, pending);
    let mut store = SessionStore::open(context.bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let metadata = next_session_metadata(
        context.bootstrap,
        &request.prompt,
        request.session.as_ref(),
        request.active_agent.as_deref(),
        session_provider,
        session_model,
        session_effort,
    )?;
    let requested_subagents = Arc::new(Mutex::new(Vec::new()));
    let noted_subagents = Arc::clone(&requested_subagents);
    let completion = run_session_attempt_lifecycle_with_terminal_writer(
        active_session_attempts(),
        &mut store,
        metadata,
        request.prompt.clone(),
        |attempt_key| {
            let mut provider = build_provider(
                model,
                provider_messages(&request, context.include_system_prompt),
                provider_tools,
                request.request_config.clone(),
            )?;
            // Live SSE already emits ProviderPart/Usage through the provider sink.
            // Headless flush_progress would re-send those and double TUI text/tools.
            let forwarded_progress = context.progress.map(|progress| {
                let progress = Arc::clone(progress);
                Arc::new(move |event: TurnEvent| match event {
                    TurnEvent::ProviderPart(_) | TurnEvent::Usage(_) => {}
                    other => progress(other),
                }) as TurnProgressSink
            });
            let headless_progress: TurnProgressSink = Arc::new(move |event: TurnEvent| {
                record_requested_subagent(&requested_subagents, &event);
                if let Some(progress) = &forwarded_progress {
                    progress(event);
                }
            });
            let headless_progress = Some(&headless_progress);
            if let Some(progress) = context.progress {
                provider = provider.with_progress_sink(Arc::clone(progress));
            }
            let mut provider =
                TaskMailboxProvider::new(provider, task_registry.clone(), TaskMessageTarget::Main);
            cancellation_result(context.cancellation)?;
            let snapshot = match effective_max_iterations(
                request.max_iterations,
                context.bootstrap.max_iterations,
            ) {
                Some(max_iterations) => {
                    block_on_headless_turn(run_headless_turn_with_max_iterations_and_progress(
                        &mut provider,
                        &mut gate,
                        &mut resolver,
                        &mut dispatcher,
                        &mut repository,
                        context.cancellation,
                        max_iterations,
                        headless_progress,
                        Some(attempt_key),
                    ))
                }
                None => block_on_headless_turn(agens_core::run_headless_turn_with_progress(
                    &mut provider,
                    &mut gate,
                    &mut resolver,
                    &mut dispatcher,
                    &mut repository,
                    context.cancellation,
                    headless_progress,
                    Some(attempt_key),
                )),
            }?
            .map_err(CliError::runtime)?;
            let turn = completed_session_turn(
                &request.prompt,
                &snapshot,
                request.pending_system_reminder.as_deref(),
            )?;

            Ok((snapshot, turn))
        },
        |store, write| {
            let note = noted_subagents
                .lock()
                .map(|requested| interrupted_turn_note(&requested))
                .unwrap_or_else(|_| interrupted_turn_note(&[]));

            write_terminal_attempt(store, write, &note)
        },
    )
    .map_err(|error| match error {
        AttemptLifecycleError::Begin(BeginSessionAttemptError::AlreadyRunning(_)) => {
            HeadlessChatFailure::from(CliError::runtime(HeadlessTurnError::State))
        }
        AttemptLifecycleError::Begin(BeginSessionAttemptError::Store) => {
            HeadlessChatFailure::from(CliError::storage("session attempt could not be started"))
        }
        AttemptLifecycleError::Runtime { error, partial } => HeadlessChatFailure { error, partial },
    })?;

    let text = completion
        .snapshot
        .events()
        .iter()
        .filter_map(|event| match event {
            agens_core::TurnEvent::ProviderPart(agens_core::MessagePart::Text(text)) => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<String>();

    if text.is_empty() {
        Ok(HeadlessChatCompletion {
            text: "completed".to_owned(),
            metadata: completion.metadata,
            messages: completion.messages,
        })
    } else {
        Ok(HeadlessChatCompletion {
            text,
            metadata: completion.metadata,
            messages: completion.messages,
        })
    }
}

pub(crate) fn provider_messages(
    request: &HeadlessChatRequest,
    include_system_prompt: bool,
) -> Vec<Message> {
    let mut messages = request.history.clone();
    if include_system_prompt
        && request.skills.is_some()
        && let Some(system_prompt) = &request.system_prompt
    {
        messages.insert(
            0,
            Message {
                role: Role::System,
                parts: vec![MessagePart::Text(system_prompt.clone())],
            },
        );
    }
    if let Some(reminder) = &request.pending_system_reminder {
        messages.push(Message {
            role: Role::System,
            parts: vec![MessagePart::Text(reminder.clone())],
        });
    }
    messages.push(Message {
        role: Role::User,
        parts: vec![MessagePart::Text(request.prompt.clone())],
    });
    messages
}

pub(crate) struct DiscardCompletedTurnRepository;

impl CompletedTurnRepository for DiscardCompletedTurnRepository {
    fn persist_completed_turn(
        &mut self,
        _: CompletedTurnSnapshot,
    ) -> impl std::future::Future<Output = Result<(), CompletedTurnStoreError>> + Send {
        std::future::ready(Ok(()))
    }
}

pub(crate) fn block_on_headless_turn<T>(
    future: impl std::future::Future<Output = T>,
) -> Result<T, CliError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| CliError::runtime(HeadlessTurnError::Provider))?;

    Ok(runtime.block_on(future))
}

pub(crate) fn explicit_task_delegation_prompt(base: &str) -> String {
    const INSTRUCTION: &str = "When the user explicitly asks for subagent delegation, use the `task` tool instead of completing the delegated work inline. Use `task_control` to inspect, background, or cancel a live execution and `task_message` to send bounded coordination without waiting for completion.";

    if base.contains(INSTRUCTION) {
        base.to_owned()
    } else {
        format!("{base}\n\n{INSTRUCTION}")
    }
}

#[cfg(test)]
mod tests {
    use agens_core::{CompletedSessionTurn, SessionMessage};

    use super::*;
    use crate::CliDependencies;
    use crate::bootstrap::bootstrap;

    #[test]
    fn primary_task_instruction_requires_explicit_delegation_and_is_idempotent() {
        let prompt = explicit_task_delegation_prompt("Base instructions.");

        assert_eq!(
            prompt,
            "Base instructions.\n\nWhen the user explicitly asks for subagent delegation, use the `task` tool instead of completing the delegated work inline. Use `task_control` to inspect, background, or cancel a live execution and `task_message` to send bounded coordination without waiting for completion."
        );
        assert_eq!(explicit_task_delegation_prompt(&prompt), prompt);
    }

    #[test]
    fn production_resumed_headless_turn_replays_typed_history_and_appends_to_the_same_session() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-resumed-headless-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos()
        ));
        let project_root = temporary.join("project");
        let config_home = temporary.join("config");
        let data_directory = temporary.join("data");
        std::fs::create_dir_all(project_root.join(".git"))
            .expect("project marker should be created");
        std::fs::create_dir_all(config_home.join("agents"))
            .expect("agent directory should be created");
        std::fs::write(
            config_home.join("agents/reviewer.md"),
            "---\nname: reviewer\ndescription: reviewer\nmode: primary\nmodel: gpt-4o\npermissions: []\n---\nYou are reviewer.\n",
        )
        .expect("reviewer agent should be written");

        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("mock provider should bind");
        let address = listener
            .local_addr()
            .expect("mock provider should have an address");
        let worker = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};

            let (mut stream, _) = listener
                .accept()
                .expect("mock provider should accept the resumed request");
            let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .expect("request line should be readable");
            assert_eq!(request_line, "POST /responses HTTP/1.1\r\n");

            let mut content_length = None;
            loop {
                let mut header = String::new();
                reader
                    .read_line(&mut header)
                    .expect("request header should be readable");
                if header == "\r\n" {
                    break;
                }
                if let Some(value) = header.strip_prefix("content-length: ") {
                    content_length = Some(
                        value
                            .trim()
                            .parse::<usize>()
                            .expect("content length should be numeric"),
                    );
                }
            }

            let mut body =
                vec![0_u8; content_length.expect("request should include content length")];
            std::io::Read::read_exact(&mut reader, &mut body)
                .expect("request body should be readable");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"second answer\"}\n\ndata: {\"type\":\"response.completed\"}\n\n")
                .expect("mock response should be written");

            serde_json::from_slice::<serde_json::Value>(&body)
                .expect("resumed provider request should be valid JSON")
        });

        let dependencies = CliDependencies::for_test(
            project_root.clone(),
            Some(temporary.join("home")),
            BTreeMap::from([
                (
                    "AGENS_CONFIG_HOME".to_owned(),
                    config_home.display().to_string(),
                ),
                ("OPENAI_API_KEY".to_owned(), "test-key".to_owned()),
            ]),
            BTreeMap::from([(
                config_home.join("config.toml"),
                format!(
                    "[provider]\ntype = \"openai-api\"\nmodel = \"gpt-4.1\"\nbase_url = \"http://{address}\"\n\n[options]\ndata_dir = \"{}\"\n",
                    data_directory.display()
                ),
            )]),
        );
        let bootstrap = bootstrap(&dependencies).expect("production bootstrap should be valid");
        let initial_messages = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("first input".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::Reasoning("first reasoning".into()),
                    MessagePart::ToolCall {
                        id: "call-history".into(),
                        name: "native::read".into(),
                        input: r#"{"path":"notes.md"}"#.into(),
                    },
                    MessagePart::Text("calling the tool".into()),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "call-history".into(),
                    content: "file contents".into(),
                    is_error: false,
                }],
            },
        ];
        let initial_turn = CompletedSessionTurn::new(
            initial_messages
                .clone()
                .into_iter()
                .map(SessionMessage::try_from)
                .collect::<Result<_, _>>()
                .expect("typed history should be a valid completed turn"),
        )
        .expect("typed history should be a valid completed turn");
        let metadata = SessionMetadata {
            id: 0,
            project: project_root.display().to_string(),
            title: "first input".into(),
            active_agent: "reviewer".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 10,
            updated_at: 10,
            completed_turn_count: 0,
            resumable: false,
        };
        SessionStore::open(&data_directory)
            .expect("session store should open")
            .persist_completed_session_turn(&metadata, &initial_turn)
            .expect("normalized session should persist");

        let mut request = crate::tui::resume::resume_tui_session(
            &bootstrap,
            1,
            &SkillCatalog::default(),
            &crate::tui::provider::TuiCredentialResolver::production(),
        )
        .expect("normalized session should resume")
        .apply_to(HeadlessChatRequest {
            prompt: "second input".into(),
            history: Vec::new(),
            model: None,
            system_prompt: None,
            max_iterations: None,
            mode: PermissionMode::Edit,
            dangerously_allow_all: false,
            dangerous_mode: false,
            request_config: agens_core::RequestConfig::default(),
            session_reasoning_effort: None,
            session: None,
            active_agent: None,
            effective_capabilities: None,
            pending_system_reminder: None,
            skills: None,
        });
        request.pending_system_reminder =
            Some("Agent capabilities expanded: primary -> reviewer.".into());
        let completion = run_production_headless_chat_with_progress(
            request,
            &bootstrap,
            &HeadlessTurnCancellation::new(),
            None,
            None,
            None,
            None,
        )
        .expect("resumed production turn should complete");
        let provider_request = worker.join().expect("mock provider should finish");
        let reopened = SessionStore::open(&data_directory)
            .expect("session store should reopen")
            .load_session_for_resume(1)
            .expect("same session should remain resumable");

        assert_eq!(completion.metadata.id, 1);
        assert_eq!(
            provider_request["input"],
            serde_json::json!([
                {"role": "user", "content": [{"type": "input_text", "text": "first input"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "first reasoning"}]},
                {"type": "function_call", "call_id": "call-history", "name": "native::read", "arguments": "{\"path\":\"notes.md\"}"},
                {"role": "assistant", "content": [{"type": "output_text", "text": "calling the tool"}]},
                {"type": "function_call_output", "call_id": "call-history", "output": "file contents"},
                {"role": "system", "content": [{"type": "input_text", "text": "Agent capabilities expanded: primary -> reviewer."}]},
                {"role": "user", "content": [{"type": "input_text", "text": "second input"}]},
            ])
        );
        assert_eq!(reopened.metadata.id, 1);
        assert_eq!(reopened.metadata.active_agent, "reviewer");
        assert_eq!(reopened.metadata.completed_turn_count, 2);
        assert_eq!(
            reopened
                .messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            vec![
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::System,
                Role::User,
                Role::Assistant
            ]
        );
        assert_eq!(reopened.messages[..3], initial_messages);
        assert_eq!(
            reopened.messages[3].parts,
            vec![MessagePart::Text(
                "Agent capabilities expanded: primary -> reviewer.".into()
            )]
        );
        assert_eq!(
            reopened.messages[4].parts,
            vec![MessagePart::Text("second input".into())]
        );
        assert_eq!(
            reopened.messages[5].parts,
            vec![MessagePart::Text("second answer".into())]
        );

        std::fs::remove_dir_all(temporary).expect("temporary files should be removed");
    }
}
