//! Running one headless turn: building the provider for the configured
//! backend, resolving the policy and prompt it runs under, and driving it to
//! completion inside the session attempt lifecycle.

use crate::outcome::{HeadlessChatCompletion, HeadlessChatFailure};
use crate::request::HeadlessChatRequest;
use crate::request::{explicit_task_delegation_prompt, preflight_request_media, provider_messages};
use crate::subagents::{interrupted_turn_note, record_tool_result_fact};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use agens_core::{
    BeginSessionAttemptError, HeadlessTurnCancellation, HeadlessTurnError, Message, MessagePart,
    PermissionMode, PermissionSession, TurnEvent, TurnProgressSink,
    run_headless_turn_with_max_iterations_and_progress,
};
use agens_providers::{
    ChatGptResponsesProvider, DiagnosticRef, MediaBlobs, MoonshotProvider, OpenAiFunctionTool,
    OpenAiResponsesProvider, ProgressAwareProvider, ProviderDiagnosticClass,
    ProviderDiagnosticScope, ProviderFailureDetail,
};
use agens_store::{PermissionGrantStore, SessionStore, ToolFactStore, open_media};
use agens_tools::{
    EffectiveCapabilitySet, McpErrorCategory, McpLifecycleState, McpLoadPhase, McpStatusHandle,
    TaskMessageTarget,
};

use agens_agents::{AgentModelCompatibility, agent_catalog};
use agens_bootstrap::Bootstrap;
use agens_bootstrap::effective_max_iterations;
use agens_diagnostics::{
    SafeDiagnosticStore, SessionLifecycle, TurnOutcome, diagnostic_store,
    operation_diagnostics_with_progress, record_parent_terminal, record_session_lifecycle,
};
use agens_dispatch::ProductionToolDispatcher;
use agens_error::{CliError, ExitStatus, cancellation_result};
use agens_permissions::{
    PermissionPrompter, ProductionPermissionGate, ProductionPermissionResolver,
    ProductionPromptAuthorization, permission_policy,
};
use agens_session::attempt::{
    AttemptLifecycleError, active_session_attempts,
    run_session_attempt_lifecycle_with_terminal_writer, write_terminal_attempt,
    write_terminal_attempt_with_history,
};
use agens_session::provider::{
    ProviderKind, ProviderResolutionError, ResolvedProvider, authenticated_sources,
    bootstrap_authentication, resolve_provider_for_model,
};
use agens_session::turns::{
    completed_session_turn_from_events_with_media, completed_session_turn_with_media,
    next_session_metadata,
};
use agens_tool_runtime::block_on_headless_turn;
use agens_tool_runtime::child::TaskMailboxProvider;
use agens_tool_runtime::runtime::production_tool_runtime_for_parent_with_cancellation;
use agens_tool_runtime::task::ProductionTuiTaskRuntime;

#[derive(Default)]
struct PartialTurnRecorder {
    events: Vec<TurnEvent>,
    tool_call_ids: BTreeSet<String>,
    tool_result_ids: BTreeSet<String>,
}

impl PartialTurnRecorder {
    fn observe(&mut self, event: TurnEvent) {
        let accepted = match &event {
            TurnEvent::ProviderPart(MessagePart::ToolCall { id, .. }) => {
                self.tool_call_ids.insert(id.clone())
            }
            TurnEvent::ToolResult(MessagePart::ToolResult { tool_call_id, .. }) => {
                self.tool_result_ids.insert(tool_call_id.clone())
            }
            _ => true,
        };
        if accepted {
            self.events.push(event);
        }
    }

    fn has_partial_history(&self) -> bool {
        self.events.iter().any(|event| {
            matches!(
                event,
                TurnEvent::ProviderPart(_) | TurnEvent::ToolResult { .. }
            )
        })
    }
}

struct HeadlessProviderContext<'a> {
    bootstrap: &'a Bootstrap,
    /// The provider this turn resolved from its own model, rather than a
    /// process-wide setting: one run may reach several providers.
    provider: ProviderKind,
    cancellation: &'a HeadlessTurnCancellation,
    progress: Option<&'a TurnProgressSink>,
    prompter: Box<dyn PermissionPrompter>,
    task_runtime: Option<&'a ProductionTuiTaskRuntime>,
    diagnostic_reference: &'a str,
    include_system_prompt: bool,
    failure_detail: ProviderFailureDetail,
}

/// The provider this turn speaks to, chosen by the model it was given.
///
/// A `provider/model` identifier names it outright; a bare one resolves only
/// when a single authenticated provider serves it. Nothing process-wide takes
/// part, so one run can reach several providers.
fn resolve_turn_provider(
    bootstrap: &Bootstrap,
    model: Option<&str>,
) -> Result<ResolvedProvider, CliError> {
    resolve_provider_for_model(model, &bootstrap_authentication(bootstrap)).map_err(|error| {
        match error {
            // Missing credentials stay an authentication failure rather than
            // becoming a configuration one: the run is configured correctly and
            // the operator has to authenticate, not edit a file.
            ProviderResolutionError::Unauthenticated(provider) => {
                CliError::authentication(provider_authentication_message(provider))
            }
            other => CliError::configuration(other.message()),
        }
    })
}

const fn provider_authentication_message(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::OpenAiApi => "OpenAI API authentication is unavailable",
        ProviderKind::OpenAiChatGpt => "ChatGPT credentials are unavailable or invalid",
        ProviderKind::Moonshot => "Moonshot AI authentication is unavailable",
    }
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

    let requested_model = request
        .model
        .clone()
        .or_else(|| bootstrap.model().map(ToOwned::to_owned));
    let validator = AgentModelCompatibility::for_authenticated(authenticated_sources(
        &bootstrap_authentication(bootstrap),
    ))?;
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
            None => agens_core::prompt::base_system_prompt(
                headless_turn_system_prompt(bootstrap, &agent_catalog_root)?.as_deref(),
            ),
        };
        request.system_prompt = Some(explicit_task_delegation_prompt(&base));
    }

    let diagnostics = operation_diagnostics_with_progress(
        bootstrap,
        ProviderDiagnosticScope::Parent,
        operation_reference,
        progress.cloned(),
    );
    let diagnostic_reference = diagnostics.reference;
    let provider_diagnostics = diagnostics.provider;
    let failure_detail = ProviderFailureDetail::new();
    // Resolved here rather than earlier so a provider failure carries this
    // turn's diagnostic reference like every other terminal failure does.
    let resolved = match resolve_turn_provider(bootstrap, requested_model.as_deref()) {
        Ok(resolved) => resolved,
        Err(error) => {
            // Still a turn that ended: a supervisor waiting on `turn_ended`
            // would otherwise wait forever on the one failure that happens
            // before a provider is even chosen.
            let failure = HeadlessChatFailure::from(error);
            record_session_lifecycle(
                bootstrap,
                &diagnostic_reference,
                ProviderDiagnosticScope::Parent,
                SessionLifecycle::TurnEnded {
                    outcome: TurnOutcome::Failed,
                },
            );
            record_parent_terminal(bootstrap, &diagnostic_reference, &failure.error);
            return Err(
                failure.map_error(|error| error.with_diagnostic_reference(&diagnostic_reference))
            );
        }
    };
    request.model = Some(resolved.model.clone());
    record_session_lifecycle(
        bootstrap,
        &diagnostic_reference,
        ProviderDiagnosticScope::Parent,
        SessionLifecycle::TurnStarted {
            model: &resolved.model,
        },
    );

    let result = match resolved.provider {
        ProviderKind::OpenAiApi => {
            let api_key = bootstrap.api_key_for("openai-api").ok_or_else(|| {
                CliError::authentication("OpenAI API authentication is unavailable")
            })?;
            let base_url = headless_turn_provider_base_url(bootstrap, &agent_catalog_root)?;
            run_production_headless_chat_with_provider(
                request,
                HeadlessProviderContext {
                    bootstrap,
                    provider: resolved.provider,
                    cancellation,
                    progress,
                    prompter,
                    task_runtime,
                    diagnostic_reference: &diagnostic_reference,
                    include_system_prompt: true,
                    failure_detail: failure_detail.clone(),
                },
                move |model, messages, tools, request_config, media_blobs| {
                    build_openai_provider_with_media(
                        api_key,
                        base_url.as_deref(),
                        model,
                        messages,
                        tools,
                        media_blobs,
                    )
                    .map(|provider| {
                        provider
                            .with_parallel_tool_calls(bootstrap.parallel_tool_calls)
                            .with_request_config(request_config)
                            .with_diagnostics(provider_diagnostics)
                            .with_failure_detail(failure_detail.clone())
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
        ProviderKind::Moonshot => {
            let api_key = bootstrap.api_key_for("moonshotai").ok_or_else(|| {
                CliError::authentication("Moonshot AI authentication is unavailable")
            })?;
            let base_url = headless_turn_provider_base_url(bootstrap, &agent_catalog_root)?;
            run_production_headless_chat_with_provider(
                request,
                HeadlessProviderContext {
                    bootstrap,
                    provider: resolved.provider,
                    cancellation,
                    progress,
                    prompter,
                    task_runtime,
                    diagnostic_reference: &diagnostic_reference,
                    include_system_prompt: true,
                    failure_detail: failure_detail.clone(),
                },
                move |model, messages, tools, request_config, media_blobs| {
                    MoonshotProvider::from_api_key_with_messages_and_tools_and_timeout(
                        api_key,
                        base_url.as_deref(),
                        model,
                        messages,
                        tools,
                        agens_providers::DEFAULT_PROVIDER_REQUEST_TIMEOUT,
                    )
                    .map(|provider| {
                        provider
                            .with_parallel_tool_calls(bootstrap.parallel_tool_calls)
                            .with_request_config(request_config)
                            .with_diagnostics(provider_diagnostics)
                            .with_failure_detail(failure_detail.clone())
                            .with_media_blobs(media_blobs)
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
        ProviderKind::OpenAiChatGpt => {
            let credentials_path = bootstrap.paths.credentials.clone();
            let instructions = match request.system_prompt.clone() {
                Some(explicit) => explicit,
                None => agens_core::prompt::base_system_prompt(
                    headless_turn_system_prompt(bootstrap, &agent_catalog_root)?.as_deref(),
                ),
            };
            let base_url = headless_turn_provider_base_url(bootstrap, &agent_catalog_root)?;
            run_production_headless_chat_with_provider(
                request,
                HeadlessProviderContext {
                    bootstrap,
                    provider: resolved.provider,
                    cancellation,
                    progress,
                    prompter,
                    task_runtime,
                    diagnostic_reference: &diagnostic_reference,
                    include_system_prompt: false,
                    failure_detail: failure_detail.clone(),
                },
                move |model, messages, tools, request_config, media_blobs| {
                    build_chatgpt_provider_with_media(
                        &credentials_path,
                        base_url.as_deref(),
                        model,
                        instructions,
                        messages,
                        tools,
                        media_blobs,
                    )
                    .map(|provider| {
                        provider
                            .with_parallel_tool_calls(bootstrap.parallel_tool_calls)
                            .with_request_config(request_config)
                            .with_diagnostics(provider_diagnostics)
                            .with_failure_detail(failure_detail.clone())
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
    };
    record_session_lifecycle(
        bootstrap,
        &diagnostic_reference,
        ProviderDiagnosticScope::Parent,
        SessionLifecycle::TurnEnded {
            outcome: turn_outcome(&result),
        },
    );

    result.map_err(|failure| {
        record_parent_terminal(bootstrap, &diagnostic_reference, &failure.error);
        failure.map_error(|error| error.with_diagnostic_reference(&diagnostic_reference))
    })
}

/// Cancellation is reported as itself rather than folded into failure: a
/// supervisor that cannot tell the two apart will retry work the operator
/// deliberately stopped.
fn turn_outcome<T>(result: &Result<T, HeadlessChatFailure>) -> TurnOutcome {
    match result {
        Ok(_) => TurnOutcome::Completed,
        Err(failure) if failure.error.category == "cancelled" => TurnOutcome::Cancelled,
        Err(_) => TurnOutcome::Failed,
    }
}

/// The tool a reported fact came from. The identity carries no name, but the
/// fact's own shape does: a supervisor is told `bash` failed, not that call
/// `toolu_017` did.
const fn tool_fact_name(facts: &agens_core::ToolResultFacts) -> &'static str {
    match facts {
        agens_core::ToolResultFacts::Write { .. } => "write",
        agens_core::ToolResultFacts::Edit { .. } => "edit",
        agens_core::ToolResultFacts::Bash { .. } => "bash",
        agens_core::ToolResultFacts::Read { .. } => "read",
        agens_core::ToolResultFacts::Search { .. } => "search",
        _ => "unknown",
    }
}

/// `None` for a tool that succeeded.
///
/// A denial stays distinct from a failure: one means the operator said no and
/// the other means the tool broke, and a supervisor that retries the first is
/// overriding a decision rather than recovering from a fault.
const fn tool_failure_class(
    facts: &agens_core::ToolResultFacts,
) -> Option<ProviderDiagnosticClass> {
    let outcome = match facts {
        agens_core::ToolResultFacts::Write { outcome, .. }
        | agens_core::ToolResultFacts::Edit { outcome, .. }
        | agens_core::ToolResultFacts::Bash { outcome, .. }
        | agens_core::ToolResultFacts::Read { outcome, .. }
        | agens_core::ToolResultFacts::Search { outcome, .. } => outcome,
        // The enum is `#[non_exhaustive]`. A fact variant this build does not
        // know reports nothing rather than guessing an outcome for it, so a new
        // tool stays invisible to supervision until it is taught here.
        _ => return None,
    };

    match outcome {
        agens_core::ToolOutcome::Succeeded => None,
        agens_core::ToolOutcome::Failed => Some(ProviderDiagnosticClass::Tool),
        agens_core::ToolOutcome::Denied => Some(ProviderDiagnosticClass::Permission),
        _ => None,
    }
}

/// Wraps a prompter so a supervisor learns the session is waiting on a
/// decision.
///
/// A decorator rather than a change to each `PermissionPrompter`: the
/// implementations live in the terminal, the CLI and the subagent runtime, and
/// none of them has this run's diagnostic store. Blocking is also the one
/// session state nothing else reveals — the process is alive, spending nothing,
/// and producing no output at all.
struct RecordingPrompter {
    inner: Box<dyn PermissionPrompter>,
    store: SafeDiagnosticStore,
    reference: DiagnosticRef,
}

impl PermissionPrompter for RecordingPrompter {
    fn prompt(
        &mut self,
        context: &agens_permissions::PermissionPromptContext,
        cancellation: &agens_core::HeadlessTurnCancellation,
    ) -> Result<agens_permissions::PermissionPromptAnswer, agens_core::HeadlessTurnPortError> {
        self.store.record_session_lifecycle(
            &self.reference,
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::PermissionBlocked {
                tool: agens_core::bare_tool_name(&context.tool_identity).as_ref(),
                access: match context.access {
                    agens_core::ToolAccess::ReadOnly => "read_only",
                    agens_core::ToolAccess::Write => "write",
                },
            },
        );

        self.inner.prompt(context, cancellation)
    }
}

fn build_openai_provider_with_media(
    api_key: String,
    base_url: Option<&str>,
    model: String,
    messages: Vec<Message>,
    tools: Vec<OpenAiFunctionTool>,
    media_blobs: MediaBlobs,
) -> Result<OpenAiResponsesProvider, agens_core::Error> {
    OpenAiResponsesProvider::from_api_key_with_messages_tools_timeout_and_media(
        api_key,
        base_url,
        model,
        messages,
        tools,
        agens_providers::DEFAULT_PROVIDER_REQUEST_TIMEOUT,
        media_blobs,
    )
}

fn build_chatgpt_provider_with_media(
    credentials_path: &std::path::Path,
    base_url: Option<&str>,
    model: String,
    instructions: String,
    messages: Vec<Message>,
    tools: Vec<OpenAiFunctionTool>,
    media_blobs: MediaBlobs,
) -> Result<ChatGptResponsesProvider, agens_core::Error> {
    ChatGptResponsesProvider::from_credentials_with_messages_tools_timeout_auth_and_media(
        credentials_path,
        base_url,
        None,
        model,
        instructions,
        messages,
        tools,
        agens_providers::DEFAULT_PROVIDER_REQUEST_TIMEOUT,
        media_blobs,
    )
}

/// Converts a headless attempt's raw outcome into a `CliError`, always draining the
/// failure-detail handle regardless of whether the attempt succeeded or failed.
///
/// Draining only on the error path would let a handle populated during a successful attempt —
/// for example a mid-stream provider event that was recorded but then recovered, so the overall
/// attempt still completed — sit undrained, since a successful outcome never reaches an error
/// branch to take it. If a later, unrelated attempt then reused the same handle and failed, it
/// would inherit that stale record. Draining unconditionally right here, the moment the outcome
/// is known, closes that window: a successful outcome discards whatever was recorded, and only a
/// genuine failure at this exact point inherits it.
fn attach_recorded_failure_detail<T>(
    outcome: Result<T, HeadlessTurnError>,
    failure_detail: &ProviderFailureDetail,
) -> Result<T, CliError> {
    let recorded_detail = failure_detail.take();
    outcome.map_err(|error| CliError::runtime(error).with_failure_detail(recorded_detail))
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
        None => agens_core::prompt::base_system_prompt(
            headless_turn_system_prompt(bootstrap, project_root)?.as_deref(),
        ),
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
/// The text is composed exclusively from the server name and the closed labels
/// of the failing phase and error category — never from the server's raw error
/// message — so remote-controlled text can never reach a terminal even if a
/// future caller forgets to sanitize its own message.
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
            let (category, phase) = server.last_error().map_or(
                (McpErrorCategory::Unavailable, McpLoadPhase::Connect),
                |error| (error.category(), error.phase()),
            );
            format!(
                "mcp: {} {} ({})",
                server.descriptor().name(),
                phase.failure_summary(),
                category.label()
            )
        })
        .collect()
}

/// Loads durable media blobs for every media id on the request and in its history.
fn load_media_blobs_for_request(
    data_directory: &std::path::Path,
    request: &HeadlessChatRequest,
) -> Result<MediaBlobs, CliError> {
    let mut ids = BTreeSet::new();
    ids.extend(request.media_ids.iter().copied());
    for message in &request.history {
        for part in &message.parts {
            if let MessagePart::Media { media_id, .. } = part {
                ids.insert(*media_id);
            }
        }
    }

    let mut blobs = MediaBlobs::new();
    for media_id in ids {
        let (_mime, path) = open_media(data_directory, media_id)
            .map_err(|error| CliError::storage(format!("media {media_id}: {error}")))?;
        let bytes = std::fs::read(&path)
            .map_err(|error| CliError::storage(format!("media {media_id}: {error}")))?;
        blobs.insert(media_id, bytes);
    }
    Ok(blobs)
}

fn run_production_headless_chat_with_provider<P>(
    request: HeadlessChatRequest,
    context: HeadlessProviderContext<'_>,
    build_provider: impl FnOnce(
        String,
        Vec<Message>,
        Vec<OpenAiFunctionTool>,
        agens_core::RequestConfig,
        MediaBlobs,
    ) -> Result<P, CliError>,
) -> Result<HeadlessChatCompletion, HeadlessChatFailure>
where
    P: ProgressAwareProvider + Send,
{
    let model = request
        .model
        .clone()
        .or_else(|| context.bootstrap.model().map(ToOwned::to_owned))
        .unwrap_or_else(|| context.provider.default_model().to_owned());
    // Capability gate before any provider construction or network I/O.
    preflight_request_media(&model, &request)
        .map_err(|error| HeadlessChatFailure::from(CliError::configuration(error.to_string())))?;
    let session_provider = Some(context.provider.identifier().to_owned());
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
            let runtime = production_tool_runtime_for_parent_with_cancellation(
                context.bootstrap,
                project_root,
                request.skills.as_deref(),
                model.clone(),
                request.request_config.clone(),
                Some(context.diagnostic_reference.to_owned()),
                context.cancellation.adapter_view().cancellation_handle(),
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
    let prompt_diagnostic_reference =
        DiagnosticRef::new(context.diagnostic_reference.to_owned())
            .map_err(|_| CliError::configuration("diagnostic reference is invalid"))?;
    let mut resolver = ProductionPermissionResolver::new(
        Box::new(RecordingPrompter {
            inner: context.prompter,
            store: diagnostic_store(context.bootstrap),
            reference: prompt_diagnostic_reference.clone(),
        }) as Box<dyn PermissionPrompter>,
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
    let partial_events = Arc::new(Mutex::new(PartialTurnRecorder::default()));
    let runtime_partial_events = Arc::clone(&partial_events);
    let terminal_partial_events = Arc::clone(&partial_events);
    let partial_prompt = request.prompt.clone();
    let partial_system_reminder = request.pending_system_reminder.clone();
    let partial_media: Vec<(i64, String)> = request
        .media_ids
        .iter()
        .copied()
        .zip(request.media_mimes.iter().cloned())
        .collect();
    let media_blobs = load_media_blobs_for_request(context.bootstrap.data_directory(), &request)
        .map_err(HeadlessChatFailure::from)?;
    let completion = run_session_attempt_lifecycle_with_terminal_writer(
        active_session_attempts(),
        &mut store,
        metadata,
        request.prompt.clone(),
        request.media_ids.clone(),
        |attempt_key| {
            // Discards whatever an earlier use of this handle left behind before this attempt's
            // own provider is even built. See `attach_recorded_failure_detail`: a successful
            // outcome never drains on its own error path, so without this the next attempt to
            // reuse the handle could otherwise inherit a stale, unrelated record.
            context.failure_detail.take();
            let mut provider = build_provider(
                model,
                provider_messages(&request, context.include_system_prompt),
                provider_tools,
                request.request_config.clone(),
                media_blobs,
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
            let lifecycle_store = diagnostic_store(context.bootstrap);
            let lifecycle_reference = prompt_diagnostic_reference.clone();
            let accepted_partial_events = Arc::clone(&runtime_partial_events);
            let headless_progress: TurnProgressSink = Arc::new(move |event: TurnEvent| {
                if !matches!(event, TurnEvent::ProviderPart(_) | TurnEvent::Usage(_))
                    && let Ok(mut events) = accepted_partial_events.lock()
                {
                    events.observe(event.clone());
                }
                if let TurnEvent::ToolResultFacts { identity, facts } = &event {
                    record_tool_result_fact(&fact_store, identity, facts);
                    if let Some(class) = tool_failure_class(facts) {
                        lifecycle_store.record_session_lifecycle(
                            &lifecycle_reference,
                            ProviderDiagnosticScope::Parent,
                            SessionLifecycle::ToolFailed {
                                tool: tool_fact_name(facts),
                                class,
                            },
                        );
                    }
                }
                if let Some(progress) = &forwarded_progress {
                    progress(event);
                }
            });
            let headless_progress = Some(&headless_progress);
            let streamed_partial_events = Arc::clone(&runtime_partial_events);
            let visible_progress = context.progress.cloned();
            provider = provider.with_progress_sink(Arc::new(move |event| {
                if matches!(event, TurnEvent::ProviderPart(_))
                    && let Ok(mut events) = streamed_partial_events.lock()
                {
                    events.observe(event.clone());
                }
                if let Some(progress) = &visible_progress {
                    progress(event);
                }
            }));
            let mut provider =
                TaskMailboxProvider::new(provider, task_registry.clone(), TaskMessageTarget::Main);
            cancellation_result(context.cancellation)?;
            let turn_outcome = match effective_max_iterations(
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
            }?;
            let snapshot = attach_recorded_failure_detail(turn_outcome, &context.failure_detail)?;
            let turn = completed_session_turn_with_media(
                &request.prompt,
                &partial_media,
                &snapshot,
                request.pending_system_reminder.as_deref(),
            )?;

            Ok((snapshot, turn))
        },
        |store, write| {
            let events = terminal_partial_events
                .lock()
                .map_err(|_| agens_session::attempt::AttemptStoreError)?;
            if !events.has_partial_history() {
                return write_terminal_attempt(store, write, &interrupted_turn_note(&[]));
            }
            let turn = completed_session_turn_from_events_with_media(
                &partial_prompt,
                &partial_media,
                &events.events,
                partial_system_reminder.as_deref(),
            )
            .map_err(|_| agens_session::attempt::AttemptStoreError)?;
            write_terminal_attempt_with_history(store, write, &turn)
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
    use super::{
        PartialTurnRecorder, attach_recorded_failure_detail, build_chatgpt_provider_with_media,
        build_openai_provider_with_media, mcp_failure_notice_lines,
    };
    use agens_core::{HeadlessTurnError, Message, MessagePart, Role, TurnEvent};
    use agens_providers::{MediaBlobs, ProviderFailureDetail};
    use agens_tools::{
        McpErrorCategory, McpLoadPhase, McpRegistry, McpServerDescriptor, McpServerSource,
        McpServerStatus, McpServerTransport, McpStatusHandle,
    };

    #[test]
    fn openai_provider_wiring_supplies_media_before_history_is_encoded() {
        let messages = vec![Message {
            role: Role::User,
            parts: vec![MessagePart::Media {
                media_id: 7,
                mime: "image/png".into(),
            }],
        }];
        let mut media_blobs = MediaBlobs::new();
        media_blobs.insert(7, vec![1, 2, 3]);

        let result = build_openai_provider_with_media(
            "synthetic-key".into(),
            None,
            "gpt-4.1".into(),
            messages,
            Vec::new(),
            media_blobs,
        );

        assert!(
            result.is_ok(),
            "media-aware wiring must encode image history"
        );
    }

    #[test]
    fn chatgpt_provider_wiring_supplies_media_before_history_is_encoded() {
        let messages = vec![Message {
            role: Role::User,
            parts: vec![MessagePart::Media {
                media_id: 7,
                mime: "image/png".into(),
            }],
        }];
        let mut media_blobs = MediaBlobs::new();
        media_blobs.insert(7, vec![1, 2, 3]);

        let result = build_chatgpt_provider_with_media(
            std::path::Path::new("missing-synthetic-credentials.json"),
            None,
            "gpt-5.5".into(),
            "instructions".into(),
            messages,
            Vec::new(),
            media_blobs,
        );
        let Err(error) = result else {
            panic!("missing credentials must still fail authentication");
        };

        assert!(matches!(error, agens_core::Error::Auth(_)));
    }

    #[test]
    fn attach_recorded_failure_detail_carries_the_recorded_text_into_the_cli_error() {
        let failure_detail = ProviderFailureDetail::new();
        failure_detail.record("HTTP 400 rejected model \"gpt-9-missing\"");

        let outcome: Result<(), HeadlessTurnError> = Err(HeadlessTurnError::ProviderRejected);
        let error = attach_recorded_failure_detail(outcome, &failure_detail).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("HTTP 400 rejected model \"gpt-9-missing\"")
        );
    }

    #[test]
    fn attach_recorded_failure_detail_discards_a_successful_outcomes_recorded_detail() {
        let failure_detail = ProviderFailureDetail::new();
        failure_detail.record("recovered mid-stream event, turn still succeeded");

        let outcome: Result<&str, HeadlessTurnError> = Ok("completed");
        let result = attach_recorded_failure_detail(outcome, &failure_detail);

        assert_eq!(result, Ok("completed"));
        assert_eq!(
            failure_detail.take(),
            None,
            "a successful outcome must drain the handle"
        );
    }

    #[test]
    fn a_successful_attempts_stale_detail_never_leaks_into_a_later_unrelated_failure() {
        let failure_detail = ProviderFailureDetail::new();

        // First attempt: a mid-stream event is recorded but the attempt recovers and succeeds
        // overall (the SSE frame-drain recovery WU-3 fixed: an `error` event followed by more
        // valid output can still complete the response).
        failure_detail.record("stale detail from a recovered mid-stream event");
        let first_outcome: Result<(), HeadlessTurnError> = Ok(());
        assert!(attach_recorded_failure_detail(first_outcome, &failure_detail).is_ok());

        // Second, unrelated attempt reuses the same handle and fails for its own reason,
        // recording nothing new of its own.
        let second_outcome: Result<(), HeadlessTurnError> = Err(HeadlessTurnError::ProviderServer);
        let error = attach_recorded_failure_detail(second_outcome, &failure_detail).unwrap_err();

        assert!(
            !error
                .to_string()
                .contains("stale detail from a recovered mid-stream event")
        );
        assert_eq!(error.to_string(), "provider: provider service failed");
    }

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
    fn partial_turn_recorder_keeps_only_the_first_call_and_result_for_each_identity() {
        let call = MessagePart::ToolCall {
            id: "reused".into(),
            name: "native::write".into(),
            input: r#"{"path":"a"}"#.into(),
        };
        let result = MessagePart::ToolResult {
            tool_call_id: "reused".into(),
            content: "wrote a".into(),
            is_error: false,
        };
        let mut recorder = PartialTurnRecorder::default();

        recorder.observe(TurnEvent::ProviderPart(call.clone()));
        recorder.observe(TurnEvent::ToolResult(result.clone()));
        recorder.observe(TurnEvent::ProviderPart(call));
        recorder.observe(TurnEvent::ToolResult(result));

        assert_eq!(recorder.events.len(), 2);
        assert!(recorder.has_partial_history());
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

    /// The discovery reports `production_tool_runtime_for_parent` returns are
    /// not the headless surface: every report is also recorded on the shared
    /// status handle, which is what the parent turn prints to stderr. This
    /// walks a real connect timeout through discovery to prove the two agree.
    #[test]
    fn a_connect_timeout_during_discovery_reaches_the_headless_stderr_notice() {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        use agens_tool_runtime::mcp::ProductionMcpRuntime;
        use agens_tools::{
            McpLimits, McpOperationContext, McpRequest, McpResponse, McpTimeouts, McpTransport,
            McpTransportError, ToolDispatcher,
        };

        struct TimingOutTransport;

        impl McpTransport for TimingOutTransport {
            fn execute(
                &mut self,
                _: McpRequest,
                _: &McpOperationContext,
            ) -> Result<McpResponse, McpTransportError> {
                Err(McpTransportError::TimedOut)
            }

            fn notify(
                &mut self,
                _: McpRequest,
                _: &McpOperationContext,
            ) -> Result<(), McpTransportError> {
                Ok(())
            }

            fn close(&mut self, _: &McpOperationContext) -> Result<(), McpTransportError> {
                Ok(())
            }
        }

        let status = McpStatusHandle::default();
        let mut registry = McpRegistry::with_status_handle(status.clone());
        registry
            .configure_server_with_descriptor(
                descriptor("engram", true),
                || Ok(Box::new(TimingOutTransport)),
                McpTimeouts::new(
                    Duration::from_secs(10),
                    Duration::from_secs(10),
                    Duration::from_millis(200),
                )
                .unwrap(),
                McpLimits::default(),
            )
            .unwrap();

        let mut runtime = ProductionMcpRuntime {
            registry: Arc::new(Mutex::new(registry)),
            dispatcher: Arc::new(Mutex::new(ToolDispatcher::new())),
        };
        let (tools, reports) = runtime.discover_configured_tools().unwrap();

        assert!(tools.is_empty());
        assert!(reports.iter().all(agens_tools::McpServerReport::is_failed));

        let lines = mcp_failure_notice_lines(&status);

        assert_eq!(lines, vec!["mcp: engram failed to connect (timeout)"]);
        assert!(!lines.join("\n").contains("failed to list tools"));
    }

    /// Connect and `tools/list` hold separate budgets, so a server that
    /// answers `initialize` and then blows the list budget must not be
    /// reported as a connect failure: the operator would verify a handshake
    /// that already succeeded instead of widening the list budget.
    #[test]
    fn a_tool_listing_timeout_during_discovery_is_reported_as_a_list_failure() {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        use agens_tool_runtime::mcp::ProductionMcpRuntime;
        use agens_tools::{
            MCP_PROTOCOL_VERSION, McpInitializeResult, McpLimits, McpOperationContext, McpRequest,
            McpResponse, McpTimeouts, McpTransport, McpTransportError, ToolDispatcher,
        };

        #[derive(Default)]
        struct ListTimingOutTransport {
            initialized: bool,
        }

        impl McpTransport for ListTimingOutTransport {
            fn execute(
                &mut self,
                _: McpRequest,
                _: &McpOperationContext,
            ) -> Result<McpResponse, McpTransportError> {
                if self.initialized {
                    return Err(McpTransportError::TimedOut);
                }

                self.initialized = true;
                Ok(McpResponse::Initialized(McpInitializeResult::new(
                    MCP_PROTOCOL_VERSION,
                    serde_json::json!({"tools": {}}),
                )))
            }

            fn notify(
                &mut self,
                _: McpRequest,
                _: &McpOperationContext,
            ) -> Result<(), McpTransportError> {
                Ok(())
            }

            fn close(&mut self, _: &McpOperationContext) -> Result<(), McpTransportError> {
                Ok(())
            }
        }

        let status = McpStatusHandle::default();
        let mut registry = McpRegistry::with_status_handle(status.clone());
        registry
            .configure_server_with_descriptor(
                descriptor("engram", true),
                || Ok(Box::new(ListTimingOutTransport::default())),
                McpTimeouts::new(
                    Duration::from_secs(10),
                    Duration::from_secs(10),
                    Duration::from_millis(200),
                )
                .unwrap(),
                McpLimits::default(),
            )
            .unwrap();

        let mut runtime = ProductionMcpRuntime {
            registry: Arc::new(Mutex::new(registry)),
            dispatcher: Arc::new(Mutex::new(ToolDispatcher::new())),
        };
        let (tools, reports) = runtime.discover_configured_tools().unwrap();

        assert!(tools.is_empty());
        assert!(reports.iter().all(agens_tools::McpServerReport::is_failed));
        assert_eq!(
            mcp_failure_notice_lines(&status),
            vec!["mcp: engram failed to list tools (timeout)"]
        );

        let snapshot = status.snapshot();
        let error = snapshot
            .server("engram")
            .and_then(McpServerStatus::last_error)
            .expect("a failed server keeps its sanitized error");

        assert_eq!(
            error.message(),
            "timeout: tool listing timed out; raise timeout_ms"
        );
        assert_eq!(error.phase(), McpLoadPhase::ListTools);
    }

    #[test]
    fn no_configured_servers_yield_no_lines() {
        let status = McpStatusHandle::default();

        assert!(mcp_failure_notice_lines(&status).is_empty());
    }
}
