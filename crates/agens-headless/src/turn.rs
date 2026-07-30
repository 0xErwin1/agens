//! Running one headless turn: building the provider for the configured
//! backend, resolving the policy and prompt it runs under, and driving it to
//! completion inside the session attempt lifecycle.

use crate::outcome::{HeadlessChatCompletion, HeadlessChatFailure};
use crate::request::HeadlessChatRequest;
use crate::request::{explicit_task_delegation_prompt, provider_messages};
use crate::subagents::{interrupted_turn_note, record_requested_subagent, record_tool_result_fact};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agens_core::{
    BeginSessionAttemptError, HeadlessTurnCancellation, HeadlessTurnError, Message, PermissionMode,
    PermissionSession, TurnEvent, TurnProgressSink,
    run_headless_turn_with_max_iterations_and_progress,
};
use agens_providers::{
    ChatGptResponsesProvider, MoonshotProvider, OpenAiFunctionTool, OpenAiResponsesProvider,
    ProgressAwareProvider, ProviderDiagnosticScope,
};
use agens_store::{PermissionGrantStore, SessionStore, ToolFactStore};
use agens_tools::{
    EffectiveCapabilitySet, McpErrorCategory, McpLifecycleState, McpStatusHandle, TaskMessageTarget,
};

use agens_agents::{AgentModelCompatibility, agent_catalog};
use agens_bootstrap::Bootstrap;
use agens_bootstrap::effective_max_iterations;
use agens_diagnostics::{operation_diagnostics, record_parent_terminal};
use agens_dispatch::ProductionToolDispatcher;
use agens_error::{CliError, ExitStatus, cancellation_result};
use agens_permissions::{
    PermissionPrompter, ProductionPermissionGate, ProductionPermissionResolver,
    ProductionPromptAuthorization, permission_policy,
};
use agens_session::attempt::{
    AttemptLifecycleError, active_session_attempts,
    run_session_attempt_lifecycle_with_terminal_writer, write_terminal_attempt,
};
use agens_session::provider::ProviderKind;
use agens_session::turns::{completed_session_turn, next_session_metadata};
use agens_tool_runtime::block_on_headless_turn;
use agens_tool_runtime::child::TaskMailboxProvider;
use agens_tool_runtime::runtime::production_tool_runtime_for_parent;
use agens_tool_runtime::task::ProductionTuiTaskRuntime;

struct HeadlessProviderContext<'a> {
    bootstrap: &'a Bootstrap,
    cancellation: &'a HeadlessTurnCancellation,
    progress: Option<&'a TurnProgressSink>,
    prompter: Box<dyn PermissionPrompter>,
    task_runtime: Option<&'a ProductionTuiTaskRuntime>,
    diagnostic_reference: &'a str,
    include_system_prompt: bool,
}

pub fn run_production_headless_chat_with_progress(
    mut request: HeadlessChatRequest,
    bootstrap: &Bootstrap,
    cancellation: &HeadlessTurnCancellation,
    progress: Option<&TurnProgressSink>,
    prompter: Box<dyn PermissionPrompter>,
    task_runtime: Option<&ProductionTuiTaskRuntime>,
    operation_reference: Option<&str>,
) -> Result<HeadlessChatCompletion, HeadlessChatFailure> {
    agens_callcount::note_provider_runtime_build();

    let source = bootstrap
        .provider_type()
        .and_then(ProviderKind::parse)
        .map(ProviderKind::source)
        .ok_or_else(|| CliError::configuration("task provider is unavailable"))?;
    let validator = AgentModelCompatibility::for_source(source)?;
    let agent_catalog_root = headless_turn_project_root(bootstrap, task_runtime)?;
    // Only a genuinely new session (no `task_runtime` yet) resolves its own base prompt through
    // `headless_turn_system_prompt`'s explicit/fallback dance below. A resumed TUI turn's
    // `request.system_prompt` is instead the active agent's OWN prompt, already produced by
    // `discover_agent_catalog` with its AGENTS.md instructions appended — appending them again
    // here would duplicate that text in a single request.
    if task_runtime.is_none() {
        request.system_prompt = Some(headless_turn_own_system_prompt(
            bootstrap,
            &agent_catalog_root,
            request.system_prompt.take(),
        )?);
    }
    let has_task = agent_catalog(bootstrap, &agent_catalog_root, &validator)?
        .subagents()
        .any(|agent| agent.mode == agens_core::AgentMode::Subagent);
    if has_task {
        let base = match request.system_prompt.take() {
            Some(explicit) => explicit,
            None => headless_turn_system_prompt(bootstrap, &agent_catalog_root)?
                .unwrap_or_else(|| "You are Agens, a helpful coding agent.".to_owned()),
        };
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
            let api_key = bootstrap.api_key.clone().ok_or_else(|| {
                CliError::authentication("OpenAI API authentication is unavailable")
            })?;
            let base_url = headless_turn_provider_base_url(bootstrap, &agent_catalog_root)?;
            run_production_headless_chat_with_provider(
                request,
                HeadlessProviderContext {
                    bootstrap,
                    cancellation,
                    progress,
                    prompter,
                    task_runtime,
                    diagnostic_reference: &diagnostic_reference,
                    include_system_prompt: true,
                },
                move |model, messages, tools, request_config| {
                    OpenAiResponsesProvider::from_api_key_with_messages_and_tools_and_timeout(
                        api_key,
                        base_url.as_deref(),
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
        Some("moonshotai") => {
            let api_key = bootstrap.api_key.clone().ok_or_else(|| {
                CliError::authentication("Moonshot AI authentication is unavailable")
            })?;
            let base_url = headless_turn_provider_base_url(bootstrap, &agent_catalog_root)?;
            run_production_headless_chat_with_provider(
                request,
                HeadlessProviderContext {
                    bootstrap,
                    cancellation,
                    progress,
                    prompter,
                    task_runtime,
                    diagnostic_reference: &diagnostic_reference,
                    include_system_prompt: true,
                },
                move |model, messages, tools, request_config| {
                    MoonshotProvider::from_api_key_with_messages_and_tools_and_timeout(
                        api_key,
                        base_url.as_deref(),
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
                            "Moonshot AI authentication is unavailable",
                        )
                    })
                },
            )
        }
        Some("openai-chatgpt") => {
            let credentials_path = bootstrap.paths.credentials.clone();
            let instructions = match request.system_prompt.clone() {
                Some(explicit) => explicit,
                None => headless_turn_system_prompt(bootstrap, &agent_catalog_root)?
                    .unwrap_or_else(|| "You are Agens, a helpful coding agent.".to_owned()),
            };
            let base_url = headless_turn_provider_base_url(bootstrap, &agent_catalog_root)?;
            run_production_headless_chat_with_provider(
                request,
                HeadlessProviderContext {
                    bootstrap,
                    cancellation,
                    progress,
                    prompter,
                    task_runtime,
                    diagnostic_reference: &diagnostic_reference,
                    include_system_prompt: false,
                },
                move |model, messages, tools, request_config| {
                    ChatGptResponsesProvider::from_credentials_with_messages_and_tools_and_timeout_and_auth_url(
                        &credentials_path,
                        base_url.as_deref(),
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
            "headless chat requires provider.type = \"openai-api\", \"openai-chatgpt\", or \"moonshotai\"",
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

/// The session root a headless turn's tool dispatch, permission policy, and grant scope must all
/// agree on.
///
/// This function is not exclusive to the headless one-shot `agens chat` command: it is also the
/// TUI's per-turn body (`run_production_headless_chat_with_progress` is called from every TUI
/// turn), so it runs against a resumed session on every turn a resumed session takes. When
/// `task_runtime` is `Some` — always true on that TUI path — the runtime's own recorded root must
/// be reused here, or the permission policy and grant scope computed downstream would apply to
/// the resuming process's root instead of the session's, silently disagreeing with the tool
/// dispatcher `task_runtime` was already built against. Only a genuinely new session (no
/// `task_runtime` yet, including headless one-shot chat, which has no `--resume` flag) falls back
/// to discovering the process's own root.
pub fn headless_turn_project_root(
    bootstrap: &Bootstrap,
    task_runtime: Option<&ProductionTuiTaskRuntime>,
) -> Result<std::path::PathBuf, CliError> {
    match task_runtime {
        Some(task_runtime) => Ok(task_runtime.project_root.clone()),
        None => agens_bootstrap::session_root::SessionRoot::discover_for_new_session(bootstrap)
            .ok_or_else(|| CliError::configuration("native tools require a project root"))
            .map(agens_bootstrap::session_root::SessionRoot::into_path_buf),
    }
}

/// The permission policy a headless turn's tool dispatch must be evaluated against.
///
/// `project_root` may differ from `bootstrap`'s own process-discovered root — it is the session's
/// own recorded root once one exists — so this always re-derives session-scoped configuration
/// through [`agens_bootstrap::session_config::SessionConfig`] rather than reading `bootstrap`'s
/// process-captured `permission_rules` directly, which would silently keep applying rules read
/// from the WRONG root's project configuration.
pub fn headless_turn_permission_policy(
    bootstrap: &Bootstrap,
    project_root: &std::path::Path,
    project: &str,
    mode: PermissionMode,
    tool_runtime: &agens_permissions::SharedToolDispatcher,
    effective_capabilities: Option<&EffectiveCapabilitySet>,
) -> Result<agens_core::PermissionPolicy, CliError> {
    let session_root =
        agens_bootstrap::session_root::SessionRoot::confined_to(project_root.to_path_buf());
    let session_config =
        agens_bootstrap::session_config::SessionConfig::resolve(&session_root, bootstrap)?;
    permission_policy(
        session_config.permission_rules(),
        project,
        mode,
        tool_runtime,
        effective_capabilities,
    )
}

/// The configured system prompt fallback a headless turn must fall back to, re-derived from the
/// session's own recorded root rather than `bootstrap`'s process-captured `agent.system_prompt`.
///
/// `project_root` carries the same "may differ from `bootstrap`'s own process root" caveat as
/// [`headless_turn_permission_policy`]: a wrong root's project configuration here would splice
/// another project's instruction text into this session's turn.
pub fn headless_turn_system_prompt(
    bootstrap: &Bootstrap,
    project_root: &std::path::Path,
) -> Result<Option<String>, CliError> {
    let session_root =
        agens_bootstrap::session_root::SessionRoot::confined_to(project_root.to_path_buf());
    let session_config =
        agens_bootstrap::session_config::SessionConfig::resolve(&session_root, bootstrap)?;
    Ok(session_config.system_prompt().map(ToOwned::to_owned))
}

/// The system prompt a genuinely new headless parent turn (no `task_runtime` yet, i.e. no agent
/// catalog resolution of its own) must send: `explicit`, or [`headless_turn_system_prompt`]'s
/// configured value, or the hardcoded default, followed by this session's own AGENTS.md
/// instruction text.
///
/// `explicit` is appended to as well, not only the fallback: it holds the `--system` CLI flag's
/// raw text for the standalone `chat` command, which replaces the agent's OWN prompt, not the
/// project's instructions — those must still reach the model.
///
/// Re-derives [`agens_bootstrap::session_config::SessionInstructions`] from `project_root`'s own
/// `SessionRoot` on every call, mirroring [`headless_turn_system_prompt`]'s no-caching contract:
/// a wrong root here would splice another project's instruction text into this session's turn.
pub fn headless_turn_own_system_prompt(
    bootstrap: &Bootstrap,
    project_root: &std::path::Path,
    explicit: Option<String>,
) -> Result<String, CliError> {
    let base = match explicit {
        Some(explicit) => explicit,
        None => headless_turn_system_prompt(bootstrap, project_root)?
            .unwrap_or_else(|| "You are Agens, a helpful coding agent.".to_owned()),
    };

    let session_root =
        agens_bootstrap::session_root::SessionRoot::confined_to(project_root.to_path_buf());
    let instructions =
        agens_bootstrap::session_config::SessionInstructions::resolve(&session_root, bootstrap);

    Ok(match instructions.text() {
        Some(text) => format!("{base}\n\n{text}"),
        None => base,
    })
}

/// The provider endpoint a headless turn must send its conversation to, re-derived from the
/// session's own recorded root rather than `bootstrap`'s process-captured `provider.base_url`.
///
/// `project_root` carries the same "may differ from `bootstrap`'s own process root" caveat as
/// [`headless_turn_permission_policy`] and [`headless_turn_system_prompt`]: a wrong root's
/// project configuration here would silently redirect this session's traffic to an endpoint the
/// operator only ever configured for a different project.
pub fn headless_turn_provider_base_url(
    bootstrap: &Bootstrap,
    project_root: &std::path::Path,
) -> Result<Option<String>, CliError> {
    let session_root =
        agens_bootstrap::session_root::SessionRoot::confined_to(project_root.to_path_buf());
    let session_config =
        agens_bootstrap::session_config::SessionConfig::resolve(&session_root, bootstrap)?;
    Ok(session_config.provider_base_url().map(ToOwned::to_owned))
}

/// One sanitized line per enabled server currently `Failed` or `Degraded` on
/// the shared status handle, in the exact form the parent turn's caller
/// prints to stderr.
///
/// The text is composed exclusively from the server name and the error
/// category's closed label — never from the server's raw error message — so
/// remote-controlled text can never reach a terminal even if a future caller
/// forgets to sanitize its own message.
fn mcp_failure_notice_lines(status: &McpStatusHandle) -> Vec<String> {
    status
        .snapshot()
        .servers()
        .iter()
        .filter(|server| server.descriptor().enabled())
        .filter(|server| {
            matches!(
                server.state(),
                McpLifecycleState::Failed | McpLifecycleState::Degraded
            )
        })
        .map(|server| {
            let category = server
                .last_error()
                .map_or(McpErrorCategory::Unavailable, |error| error.category());
            format!(
                "mcp: {} failed to connect ({})",
                server.descriptor().name(),
                category.label()
            )
        })
        .collect()
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
    let project_root = headless_turn_project_root(context.bootstrap, context.task_runtime)?;
    let project_root = project_root.as_path();
    let (provider_tools, tool_runtime) = match context.task_runtime {
        Some(task_runtime) => (
            task_runtime.provider_tools.clone(),
            Arc::clone(&task_runtime.dispatcher),
        ),
        None => {
            let runtime = production_tool_runtime_for_parent(
                context.bootstrap,
                project_root,
                request.skills.as_deref(),
                model.clone(),
                request.request_config.clone(),
                Some(context.diagnostic_reference.to_owned()),
            )?;
            // Discovery for this turn's own registry has already run
            // synchronously inside `production_tool_runtime_for_parent`, so
            // the shared status handle now reflects every server it tried.
            // Only the parent arm reaches this branch — a subagent turn
            // reuses its parent's already-built runtime instead of building
            // its own, so it can never emit a second notice for the same
            // failure.
            for line in mcp_failure_notice_lines(&context.bootstrap.mcp_status) {
                eprintln!("{line}");
            }
            runtime
        }
    };
    let task_registry = context
        .task_runtime
        .map(|runtime| runtime.task_registry.clone());
    let project = project_root.display().to_string();
    let policy = headless_turn_permission_policy(
        context.bootstrap,
        project_root,
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
    let mut repository = agens_core::DiscardCompletedTurnRepository;
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
        context.prompter,
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
    // The evidence ledger gets its own connection: the attempt lifecycle below
    // holds `&mut store` for the whole runtime closure, so a sink installed
    // alongside it cannot also borrow that connection.
    let fact_store = Arc::new(Mutex::new(
        ToolFactStore::open(context.bootstrap.data_directory())
            .map_err(|_| CliError::storage("tool result facts ledger is unavailable"))?,
    ));
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
                if let TurnEvent::ToolResultFacts { identity, facts } = &event {
                    record_tool_result_fact(&fact_store, identity, facts);
                }
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

#[cfg(test)]
mod tests {
    use super::mcp_failure_notice_lines;
    use agens_tools::{
        McpErrorCategory, McpRegistry, McpServerDescriptor, McpServerSource, McpServerTransport,
        McpStatusHandle,
    };

    fn descriptor(name: &str, enabled: bool) -> McpServerDescriptor {
        McpServerDescriptor::new(
            name,
            McpServerSource::Global,
            McpServerTransport::Stdio,
            enabled,
            std::time::Duration::from_millis(500),
            None,
        )
    }

    #[test]
    fn one_failed_enabled_server_yields_one_sanitized_line_and_disabled_ready_servers_yield_none() {
        let status = McpStatusHandle::default();
        let mut registry = McpRegistry::with_status_handle(status.clone());
        registry
            .register_failed_server(
                descriptor("atlas", true),
                McpErrorCategory::Transport,
                "transport: SENTINEL_SECRET must never render",
            )
            .unwrap();
        registry
            .register_disabled_server(descriptor("vault", false))
            .unwrap();

        let lines = mcp_failure_notice_lines(&status);

        assert_eq!(lines, vec!["mcp: atlas failed to connect (transport)"]);
        assert!(!lines.join("\n").contains("SENTINEL_SECRET"));
    }

    #[test]
    fn two_failed_enabled_servers_yield_two_sanitized_lines() {
        let status = McpStatusHandle::default();
        let mut registry = McpRegistry::with_status_handle(status.clone());
        registry
            .register_failed_server(
                descriptor("atlas", true),
                McpErrorCategory::Transport,
                "transport: connection failed",
            )
            .unwrap();
        registry
            .register_failed_server(
                descriptor("engram", true),
                McpErrorCategory::Timeout,
                "timeout: connect timed out",
            )
            .unwrap();

        let mut lines = mcp_failure_notice_lines(&status);
        lines.sort();

        assert_eq!(
            lines,
            vec![
                "mcp: atlas failed to connect (transport)",
                "mcp: engram failed to connect (timeout)",
            ]
        );
    }

    #[test]
    fn no_configured_servers_yield_no_lines() {
        let status = McpStatusHandle::default();

        assert!(mcp_failure_notice_lines(&status).is_empty());
    }
}
