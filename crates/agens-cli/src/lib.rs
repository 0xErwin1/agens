use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc::Receiver};
use std::time::{SystemTime, UNIX_EPOCH};

use agens_config::{
    ConfigPaths, ConfigPermissionDecision, ConfigPermissionRule, ConfigPermissionScope,
    McpTransport, expand_environment, expand_environment_with_commands, extract_permission_rules,
    mcp_servers, merge_toml_documents, parse_toml_document, resolve_paths, validate_toml_document,
};
use agens_core::{
    AgentDefinition, AttemptKey, BeginSessionAttemptError, CompletedSessionTurn,
    CompletedTurnRepository, CompletedTurnSnapshot, CompletedTurnStoreError,
    HeadlessPermissionGate, HeadlessPermissionResolver, HeadlessToolCall, HeadlessToolDispatcher,
    HeadlessToolOutput, HeadlessTurnCancellation, HeadlessTurnError, HeadlessTurnPortError,
    Message, MessagePart, PermissionDecision, PermissionMode, PermissionPattern, PermissionPolicy,
    PermissionRule, PermissionSession, RecoveryOutcome, RetryBoundary, Role, SessionAttemptStatus,
    SessionMessage, SessionMessageError, SessionMetadata, TurnEvent, TurnProgressSink,
    TurnProvider, TurnState, run_headless_turn_with_max_iterations_and_progress,
};
use agens_providers::chatgpt_login::{
    LoginCancellation, LoginError, remove_provider_entry, upsert_provider_entry,
};
use agens_providers::{
    ChatGptAuthState, ChatGptResponsesProvider, DiagnosticRef, OpenAiFunctionTool,
    OpenAiResponsesProvider, ProgressAwareProvider, ProviderDiagnosticClass,
    ProviderDiagnosticComponent, ProviderDiagnosticEvent, ProviderDiagnosticKind,
    ProviderDiagnosticScope, ProviderDiagnostics, load_chatgpt_auth_state,
};
use agens_store::{
    ModelPreference, PermissionGrantStore, PreferenceStore, SessionCursor, SessionStore,
    StoredSession,
};
#[cfg(test)]
use agens_tools::TaskTerminalState;
use agens_tools::{
    AgentCatalog, AgentModelValidator, AuthorizedToolCall, CommandCatalog, CommandDefinition,
    DispatchTool, EffectiveCapabilitySet, McpEndpointSummary, McpHttpTransport, McpLimits,
    McpRegistry, McpServerDescriptor, McpServerSource, McpServerTransport, McpSseTransport,
    McpStatusHandle, McpStatusSnapshot, McpStdioTransport, McpStdioTransportConfig, McpTimeouts,
    McpTransport as McpTransportPort, McpTransportError, NativeToolCatalog, NativeTools,
    PermissionPromptContext, ReadFileInput, RemoteToolMetadata, SkillCatalog, SkillResourceTool,
    TaskControlTool, TaskExecutionEvent, TaskExecutionLifecycle, TaskExecutionRegistry,
    TaskLaunchMode, TaskMessageSource, TaskMessageTarget, TaskMessageTool,
    TaskModelResolutionError, TaskRunContext, TaskRunner, TaskRunnerError, TaskTool,
    TaskTurnRequest, TaskTurnResult, ToolDispatchRequest, ToolDispatcher, ToolEvaluationOutcome,
    ToolExecutionContext, ToolOutput,
};
use agens_tui::{
    BridgeCancel, BridgeTx, Conversation, DialogEntry, DialogView, DiffLine, DiffLineKind,
    Engine as TuiEngine, PaletteEntry, PaletteEntryKind, SessionDialogCursor, SessionDialogRequest,
    SessionDialogScope, ToolResultState, Tui, TuiExecutionEvent, TuiPermissionBridge,
    TuiPermissionReply, TuiPermissionRequest, TuiPresentation, TuiProviderOutcome,
    TuiRouteCancellation, TuiRouteProgress, TuiRouteRequest, TuiRuntimeEvent, TuiSubagentErrorKind,
    TuiSubagentEvent, TuiSubagentStatus, TuiSubmissionOutcome, TuiSubmitOrigin,
    run_with_default_progress_submit_with_permissions_and_task_controls,
};

mod chatgpt_auth;
mod model_registry;

use chatgpt_auth::{ChatGptAuthCoordinator, ChatGptAuthFlow, ChatGptAuthProgress};

pub use model_registry::{TuiModelSelector, TuiModelSource};

const UNAVAILABLE_MESSAGE: &str = "this command is not implemented yet";
const TUI_ERROR_ACTION: &str = "Correct the command or runtime condition, then retry.";
const DIAGNOSTIC_FILE_LIMIT_BYTES: u64 = 1024 * 1024;
const DIAGNOSTIC_FILE_COUNT_LIMIT: usize = 4;
static DIAGNOSTIC_REFERENCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static DIAGNOSTIC_FILE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
const RESERVED_TUI_COMMANDS: &[&str] = &[
    "agent",
    "connect",
    "disconnect",
    "diagnostics",
    "effort",
    "help",
    "mcp",
    "model",
    "new",
    "provider",
    "quit",
    "resume",
    "select",
    "sessions",
    "subagent",
    "subagents",
];

const TUI_PALETTE_BUILT_INS: &[(&str, &str, &str, Option<&str>)] = &[
    ("connect", "Connect to ChatGPT", "[--device-auth]", None),
    ("disconnect", "Disconnect ChatGPT credentials", "", None),
    (
        "diagnostics",
        "Show sanitized runtime diagnostics",
        "",
        Some("diagnostics"),
    ),
    ("new", "Start a new session", "", None),
    ("sessions", "List saved sessions", "", None),
    ("resume", "Resume a saved session", "<id>", None),
    ("agent", "List or select the primary agent", "[name]", None),
    (
        "provider",
        "Select runtime provider",
        "[name]",
        Some("provider"),
    ),
    ("model", "List or select the model", "[name]", Some("model")),
    (
        "effort",
        "Show or set reasoning effort",
        "[level]",
        Some("effort"),
    ),
    ("help", "Show commands and skills", "", Some("help")),
    ("mcp", "Show configured MCP servers", "", Some("mcp")),
    ("select", "Select a project file", "", Some("select")),
    ("quit", "Exit Agens", "", None),
];

#[derive(Clone)]
struct SafeDiagnosticStore {
    directory: PathBuf,
}

impl SafeDiagnosticStore {
    fn new(data_directory: PathBuf) -> Self {
        Self {
            directory: data_directory.join("diagnostics"),
        }
    }

    fn record(&self, event: &ProviderDiagnosticEvent) {
        let _guard = DIAGNOSTIC_FILE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _ = self.write(event);
    }

    fn write(&self, event: &ProviderDiagnosticEvent) -> std::io::Result<()> {
        ensure_private_diagnostics_directory(&self.directory)?;
        let line = diagnostic_json_line(event)?;
        let active = self.active_path();
        let existing_size = match fs::symlink_metadata(&active) {
            Ok(metadata) if metadata.file_type().is_file() => metadata.len(),
            Ok(_) => {
                return Err(std::io::Error::other(
                    "diagnostics path is not a regular file",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };
        if existing_size.saturating_add(line.len() as u64) > DIAGNOSTIC_FILE_LIMIT_BYTES {
            self.rotate()?;
        }

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(active)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&line)
    }

    fn active_path(&self) -> PathBuf {
        self.directory
            .join(format!("agens-{}.jsonl", std::process::id()))
    }

    fn rotated_path(&self, generation: usize) -> PathBuf {
        self.directory
            .join(format!("agens-{}.{}.jsonl", std::process::id(), generation))
    }

    fn rotate(&self) -> std::io::Result<()> {
        let oldest = self.rotated_path(DIAGNOSTIC_FILE_COUNT_LIMIT - 1);
        match fs::remove_file(oldest) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        for generation in (1..DIAGNOSTIC_FILE_COUNT_LIMIT - 1).rev() {
            let source = self.rotated_path(generation);
            let destination = self.rotated_path(generation + 1);
            match fs::rename(source, destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        match fs::rename(self.active_path(), self.rotated_path(1)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn ensure_private_diagnostics_directory(directory: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        }
        Ok(_) => Err(std::io::Error::other("diagnostics path is not a directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(directory)
        }
        Err(error) => Err(error),
    }
}

fn diagnostic_json_line(event: &ProviderDiagnosticEvent) -> std::io::Result<Vec<u8>> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut line = serde_json::to_vec(&serde_json::json!({
        "timestamp_ms": u64::try_from(timestamp_ms).unwrap_or(u64::MAX),
        "reference": event.reference.as_str(),
        "scope": event.scope.as_str(),
        "component": event.component.as_str(),
        "event": event.event.as_str(),
        "attempt": event.attempt,
        "max_attempts": event.max_attempts,
        "delay_ms": event.delay_ms,
        "status": event.status,
        "class": event.class.map(ProviderDiagnosticClass::as_str),
    }))
    .map_err(std::io::Error::other)?;
    line.push(b'\n');
    Ok(line)
}

struct OperationDiagnostics {
    reference: String,
    provider: ProviderDiagnostics,
}

fn operation_diagnostics(
    bootstrap: &Bootstrap,
    scope: ProviderDiagnosticScope,
    reference: Option<&str>,
) -> OperationDiagnostics {
    let reference = reference.map_or_else(next_diagnostic_reference, str::to_owned);
    let store = SafeDiagnosticStore::new(bootstrap.data_directory().to_path_buf());
    let sink = Arc::new(move |event: ProviderDiagnosticEvent| store.record(&event));
    let provider = ProviderDiagnostics::new(reference.clone(), scope, sink)
        .expect("generated diagnostics references are valid");
    OperationDiagnostics {
        reference,
        provider,
    }
}

fn next_diagnostic_reference() -> String {
    let sequence = DIAGNOSTIC_REFERENCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mixed = timestamp
        .rotate_left(17)
        .wrapping_add(sequence.wrapping_mul(0x9e37_79b9))
        ^ u64::from(std::process::id());
    format!("{:08x}", mixed as u32)
}

fn record_subagent_terminal(
    bootstrap: &Bootstrap,
    reference: &str,
    class: ProviderDiagnosticClass,
) {
    let Ok(reference) = DiagnosticRef::new(reference.to_owned()) else {
        return;
    };
    SafeDiagnosticStore::new(bootstrap.data_directory().to_path_buf()).record(
        &ProviderDiagnosticEvent {
            reference,
            scope: ProviderDiagnosticScope::Subagent,
            component: ProviderDiagnosticComponent::Subagent,
            event: ProviderDiagnosticKind::Terminal,
            attempt: 0,
            max_attempts: 0,
            delay_ms: None,
            status: None,
            class: Some(class),
        },
    );
}

fn record_parent_terminal(bootstrap: &Bootstrap, reference: &str, error: &CliError) {
    if error.message == agens_core::HeadlessTaskTerminal::ModelUnavailable.message() {
        return;
    }
    let class = match error.category {
        "auth" => ProviderDiagnosticClass::Authentication,
        "cancelled" => ProviderDiagnosticClass::Cancelled,
        "provider" => ProviderDiagnosticClass::Provider,
        "timeout" => ProviderDiagnosticClass::Deadline,
        "tool" => ProviderDiagnosticClass::Tool,
        _ => ProviderDiagnosticClass::Runtime,
    };
    let Ok(reference) = DiagnosticRef::new(reference.to_owned()) else {
        return;
    };
    SafeDiagnosticStore::new(bootstrap.data_directory().to_path_buf()).record(
        &ProviderDiagnosticEvent {
            reference,
            scope: ProviderDiagnosticScope::Parent,
            component: ProviderDiagnosticComponent::Responses,
            event: ProviderDiagnosticKind::Terminal,
            attempt: 0,
            max_attempts: 0,
            delay_ms: None,
            status: None,
            class: Some(class),
        },
    );
}

fn record_agent_diagnostic(bootstrap: &Bootstrap, event: ProviderDiagnosticKind) {
    let Ok(reference) = DiagnosticRef::new(next_diagnostic_reference()) else {
        return;
    };
    SafeDiagnosticStore::new(bootstrap.data_directory().to_path_buf()).record(
        &ProviderDiagnosticEvent {
            reference,
            scope: ProviderDiagnosticScope::Parent,
            component: ProviderDiagnosticComponent::Agent,
            event,
            attempt: 0,
            max_attempts: 0,
            delay_ms: None,
            status: None,
            class: Some(ProviderDiagnosticClass::Runtime),
        },
    );
}

type CurrentDirectory = Box<dyn Fn() -> Result<PathBuf, CliError>>;
type HomeDirectory = Box<dyn Fn() -> Option<PathBuf>>;
type Environment = Box<dyn Fn() -> BTreeMap<String, String>>;
type ConfigReader = Box<dyn Fn(&Path) -> Result<Option<String>, CliError>>;
type HeadlessChat = Box<
    dyn Fn(HeadlessChatRequest, &Bootstrap, &HeadlessTurnCancellation) -> Result<String, CliError>,
>;
type TuiLauncher = Box<dyn Fn(&Bootstrap, Option<i64>) -> Result<String, CliError>>;
type AuthLogin = Box<dyn Fn(&Path, bool, &HeadlessTurnCancellation) -> Result<String, CliError>>;

pub struct CliDependencies {
    current_directory: CurrentDirectory,
    home_directory: HomeDirectory,
    environment: Environment,
    read_file: ConfigReader,
    headless_chat: HeadlessChat,
    tui_launcher: TuiLauncher,
    auth_login: AuthLogin,
}

impl CliDependencies {
    pub fn production() -> Self {
        Self {
            current_directory: Box::new(|| {
                std::env::current_dir()
                    .map_err(|_| CliError::configuration("working directory is unavailable"))
            }),
            home_directory: Box::new(|| std::env::var_os("HOME").map(PathBuf::from)),
            environment: Box::new(|| {
                std::env::vars()
                    .filter(|(key, _)| !key.is_empty())
                    .collect()
            }),
            read_file: Box::new(|path| match fs::read_to_string(path) {
                Ok(contents) => Ok(Some(contents)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(_) => Err(CliError::configuration("configuration file is unavailable")),
            }),
            headless_chat: Box::new(run_production_headless_chat),
            tui_launcher: Box::new(run_production_tui),
            auth_login: Box::new(run_production_auth_login),
        }
    }

    pub fn for_test(
        current_directory: PathBuf,
        home_directory: Option<PathBuf>,
        environment: BTreeMap<String, String>,
        files: BTreeMap<PathBuf, String>,
    ) -> Self {
        Self {
            current_directory: Box::new(move || Ok(current_directory.clone())),
            home_directory: Box::new(move || home_directory.clone()),
            environment: Box::new(move || environment.clone()),
            read_file: Box::new(move |path| Ok(files.get(path).cloned())),
            headless_chat: Box::new(|_, _, _| Err(CliError::unavailable(UNAVAILABLE_MESSAGE))),
            tui_launcher: Box::new(|_, _| Err(CliError::unavailable(UNAVAILABLE_MESSAGE))),
            auth_login: Box::new(|_, _, _| Err(CliError::unavailable(UNAVAILABLE_MESSAGE))),
        }
    }

    pub fn with_headless_chat(
        mut self,
        handler: impl Fn(
            HeadlessChatRequest,
            &Bootstrap,
            &HeadlessTurnCancellation,
        ) -> Result<String, CliError>
        + 'static,
    ) -> Self {
        self.headless_chat = Box::new(handler);
        self
    }

    pub fn with_tui_launcher(
        mut self,
        launcher: impl Fn(&Bootstrap, Option<i64>) -> Result<String, CliError> + 'static,
    ) -> Self {
        self.tui_launcher = Box::new(launcher);
        self
    }

    pub fn with_auth_login(
        mut self,
        login: impl Fn(&Path, bool, &HeadlessTurnCancellation) -> Result<String, CliError> + 'static,
    ) -> Self {
        self.auth_login = Box::new(login);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    Success,
    Failure,
    Usage,
    Configuration,
    Authentication,
    Unavailable,
}

impl ExitStatus {
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Usage => 2,
            Self::Configuration => 3,
            Self::Authentication => 4,
            Self::Unavailable => 5,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct CommandResult {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliError {
    status: ExitStatus,
    category: &'static str,
    message: String,
}

impl CliError {
    fn usage(message: impl Into<String>) -> Self {
        Self::new(ExitStatus::Usage, "usage", message)
    }

    fn configuration(message: impl Into<String>) -> Self {
        Self::new(ExitStatus::Configuration, "config", message)
    }

    fn authentication(message: impl Into<String>) -> Self {
        Self::new(ExitStatus::Authentication, "auth", message)
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ExitStatus::Unavailable, "unavailable", message)
    }

    fn storage(message: impl Into<String>) -> Self {
        Self::new(ExitStatus::Failure, "store", message)
    }

    fn runtime(error: HeadlessTurnError) -> Self {
        let (status, category, message) = match error {
            HeadlessTurnError::Cancelled => (
                ExitStatus::Failure,
                "cancelled",
                "headless turn was cancelled",
            ),
            HeadlessTurnError::TimedOut => {
                (ExitStatus::Failure, "timeout", "headless turn timed out")
            }
            HeadlessTurnError::Authentication => (
                ExitStatus::Authentication,
                "auth",
                "ChatGPT credentials are unavailable or invalid",
            ),
            HeadlessTurnError::Provider => {
                (ExitStatus::Failure, "provider", "provider request failed")
            }
            HeadlessTurnError::ProviderRejected => (
                ExitStatus::Failure,
                "provider",
                "ChatGPT request was rejected",
            ),
            HeadlessTurnError::ProviderContext => (
                ExitStatus::Failure,
                "provider",
                "request exceeds the model context window",
            ),
            HeadlessTurnError::ProviderRateLimited => (
                ExitStatus::Failure,
                "provider",
                "ChatGPT request was rate limited",
            ),
            HeadlessTurnError::ProviderServer => {
                (ExitStatus::Failure, "provider", "ChatGPT service failed")
            }
            HeadlessTurnError::ProviderNetwork => {
                (ExitStatus::Failure, "provider", "network request failed")
            }
            HeadlessTurnError::ProviderProtocol => (
                ExitStatus::Failure,
                "provider",
                "ChatGPT response protocol failed",
            ),
            HeadlessTurnError::Permission => (
                ExitStatus::Failure,
                "permission",
                "permission evaluation failed",
            ),
            HeadlessTurnError::PermissionEvaluation => (
                ExitStatus::Failure,
                "permission",
                "permission target could not be evaluated; correct the tool arguments and retry",
            ),
            HeadlessTurnError::PermissionRequired => (
                ExitStatus::Failure,
                "permission",
                "permission approval is required",
            ),
            HeadlessTurnError::Tool => (ExitStatus::Failure, "tool", "tool execution failed"),
            HeadlessTurnError::Store => (
                ExitStatus::Failure,
                "store",
                "completed turn could not be saved",
            ),
            HeadlessTurnError::MaxIterations => (
                ExitStatus::Failure,
                "runtime",
                "headless turn reached the maximum iterations",
            ),
            HeadlessTurnError::State => (
                ExitStatus::Failure,
                "runtime",
                "headless turn entered an invalid state",
            ),
            HeadlessTurnError::TaskTerminal(terminal) => {
                (ExitStatus::Failure, "", terminal.message())
            }
        };
        Self::new(status, category, message)
    }

    fn new(status: ExitStatus, category: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            category,
            message: message.into(),
        }
    }

    fn with_diagnostic_reference(mut self, reference: &str) -> Self {
        self.message.push_str(" [ref: ");
        self.message.push_str(reference);
        self.message.push(']');
        self
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.category.is_empty() {
            return formatter.write_str(&self.message);
        }

        write!(formatter, "{}: {}", self.category, self.message)
    }
}

impl std::error::Error for CliError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadlessChatRequest {
    pub prompt: String,
    history: Vec<Message>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub max_iterations: Option<usize>,
    pub mode: PermissionMode,
    pub dangerously_allow_all: bool,
    pub dangerous_mode: bool,
    request_config: agens_core::RequestConfig,
    session_reasoning_effort: Option<agens_core::ReasoningEffort>,
    session: Option<SessionMetadata>,
    active_agent: Option<String>,
    effective_capabilities: Option<EffectiveCapabilitySet>,
    pending_system_reminder: Option<String>,
    skills: Option<Arc<SkillCatalog>>,
}

pub fn execute<I, S>(arguments: I, dependencies: &CliDependencies) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_owned())
        .collect::<Vec<_>>();

    let cancellation = HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(120));
    execute_strings(arguments, dependencies, &cancellation)
}

pub fn execute_with_cancellation<I, S>(
    arguments: I,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_owned())
        .collect::<Vec<_>>();

    execute_strings(arguments, dependencies, cancellation)
}

pub fn execute_os<I, S>(arguments: I, dependencies: &CliDependencies) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| {
            argument
                .into()
                .into_string()
                .map_err(|_| CliError::usage("command arguments must be valid UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>();

    match arguments {
        Ok(arguments) => {
            let cancellation =
                HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(120));
            execute_strings(arguments, dependencies, &cancellation)
        }
        Err(error) => error_result(&[], error),
    }
}

pub fn execute_os_with_cancellation<I, S>(
    arguments: I,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| {
            argument
                .into()
                .into_string()
                .map_err(|_| CliError::usage("command arguments must be valid UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>();

    match arguments {
        Ok(arguments) => execute_strings(arguments, dependencies, cancellation),
        Err(error) => error_result(&[], error),
    }
}

fn execute_strings(
    arguments: Vec<String>,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> CommandResult {
    match execute_command(&arguments, dependencies, cancellation) {
        Ok(stdout) => CommandResult {
            status: ExitStatus::Success,
            stdout,
            stderr: String::new(),
        },
        Err(error) => error_result(&arguments, error),
    }
}

fn error_result(arguments: &[String], error: CliError) -> CommandResult {
    CommandResult {
        status: error.status,
        stdout: if arguments == ["config", "doctor"] && error.status == ExitStatus::Configuration {
            "Agens config doctor\nStatus:  invalid\n".to_owned()
        } else {
            String::new()
        },
        stderr: format!("error: {error}\n"),
    }
}

fn execute_command(
    arguments: &[String],
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    match arguments {
        [] => run_tui(dependencies, None),
        [resume] if resume == "--resume" => run_tui(dependencies, None),
        [resume, identifier] if resume == "--resume" && identifier.parse::<i64>().is_ok() => {
            run_tui(dependencies, identifier.parse().ok())
        }
        [identifier] if identifier.parse::<i64>().is_ok() => {
            run_tui(dependencies, identifier.parse().ok())
        }
        [command] if is_help(command) => Ok(root_help()),
        [command] if is_version(command) => Ok(format!("agens {}\n", env!("CARGO_PKG_VERSION"))),
        [command, rest @ ..] if command == "config" => run_config(rest, dependencies),
        [command, rest @ ..] if command == "auth" => run_auth(rest, dependencies, cancellation),
        [command, rest @ ..] if command == "chat" => run_chat(rest, dependencies, cancellation),
        [command, rest @ ..] if command == "models" => run_models(rest),
        [command, rest @ ..] if command == "sessions" => run_sessions(rest, dependencies),
        _ => Err(CliError::usage("unknown command; run agens --help")),
    }
}

fn run_config(arguments: &[String], dependencies: &CliDependencies) -> Result<String, CliError> {
    if arguments.iter().any(|argument| is_help(argument)) {
        return Ok("Usage: agens config doctor\n".to_owned());
    }

    match arguments {
        [command] if is_help(command) => Ok("Usage: agens config doctor\n".to_owned()),
        [command] if command == "doctor" => {
            let bootstrap = bootstrap(dependencies)?;
            Ok(format!(
                "Agens config doctor\nGlobal:  {} ({})\nProject: {} ({})\nModel:   {}\nStatus:  valid\n",
                bootstrap.paths.global_config.display(),
                source_status(bootstrap.global_loaded),
                bootstrap.paths.project_config.display(),
                source_status(bootstrap.project_loaded),
                bootstrap.model.as_deref().unwrap_or("-")
            ))
        }
        _ => Err(CliError::usage("config requires the doctor subcommand")),
    }
}

fn run_auth(
    arguments: &[String],
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    if arguments.iter().any(|argument| is_help(argument)) {
        return Ok("Usage: agens auth <status|login|logout>\n".to_owned());
    }

    match arguments {
        [command] if is_help(command) => Ok("Usage: agens auth <status|login|logout>\n".to_owned()),
        [command] if command == "status" => {
            let bootstrap = bootstrap(dependencies)?;
            let state =
                load_chatgpt_auth_state(&bootstrap.paths.credentials, std::time::SystemTime::now())
                    .map_err(|_| {
                        CliError::authentication("ChatGPT credentials are unavailable or invalid")
                    })?;
            let status = match state {
                ChatGptAuthState::Ready => "ready",
                ChatGptAuthState::RefreshRequired => "refresh required",
            };
            Ok(format!("ChatGPT authentication: {status}\n"))
        }
        [command, provider] if command == "status" => {
            let provider = CredentialProvider::parse(provider)?;
            let bootstrap = bootstrap(dependencies)?;
            provider_status(&bootstrap.paths.credentials, provider)
        }
        [command] if command == "login" => run_auth_login(dependencies, false, cancellation),
        [command, flag] if command == "login" && flag == "--device-auth" => {
            run_auth_login(dependencies, true, cancellation)
        }
        [command, subcommand, provider, rest @ ..]
            if command == "login" && subcommand == "api-key" =>
        {
            run_api_key_login(provider, rest, dependencies)
        }
        [command, provider] if command == "logout" => {
            let provider = CredentialProvider::parse(provider)?;
            let bootstrap = bootstrap(dependencies)?;
            let removed =
                remove_provider_entry(&bootstrap.paths.credentials, provider.identifier())
                    .map_err(|_| {
                        CliError::authentication("ChatGPT credentials are unavailable or invalid")
                    })?;
            if removed {
                Ok(format!("Logged out of {}.\n", provider.identifier()))
            } else {
                Ok(format!(
                    "No credentials stored for {}.\n",
                    provider.identifier()
                ))
            }
        }
        _ => Err(CliError::usage("auth requires status, login, or logout")),
    }
}

#[derive(Clone, Copy)]
enum CredentialProvider {
    OpenAiApi,
    OpenAiChatGpt,
}

impl CredentialProvider {
    fn parse(value: &str) -> Result<Self, CliError> {
        match value {
            "openai-api" => Ok(Self::OpenAiApi),
            "openai-chatgpt" => Ok(Self::OpenAiChatGpt),
            _ => Err(CliError::usage("auth provider is unsupported")),
        }
    }

    const fn identifier(self) -> &'static str {
        match self {
            Self::OpenAiApi => "openai-api",
            Self::OpenAiChatGpt => "openai-chatgpt",
        }
    }
}

fn run_api_key_login(
    provider: &str,
    arguments: &[String],
    dependencies: &CliDependencies,
) -> Result<String, CliError> {
    let provider = CredentialProvider::parse(provider)?;
    if !matches!(provider, CredentialProvider::OpenAiApi) {
        return Err(CliError::usage("API-key login supports only openai-api"));
    }

    let supplied_key = parse_api_key_flag(arguments)?;
    let api_key = read_api_key(supplied_key.as_deref())?;
    let bootstrap = bootstrap(dependencies)?;
    upsert_provider_entry(
        &bootstrap.paths.credentials,
        provider.identifier(),
        serde_json::json!({ "api_key": api_key }),
    )
    .map_err(|_| CliError::authentication("API-key credentials could not be saved"))?;

    Ok(format!("Logged in to {}.\n", provider.identifier()))
}

fn parse_api_key_flag(arguments: &[String]) -> Result<Option<String>, CliError> {
    match arguments {
        [] => Ok(None),
        [flag, value] if flag == "--api-key" => {
            let value = value.trim();
            if value.is_empty() {
                return Err(CliError::usage(
                    "auth login api-key requires a non-empty API key",
                ));
            }
            Ok(Some(value.to_owned()))
        }
        _ => Err(CliError::usage(
            "auth login api-key accepts only an optional --api-key value",
        )),
    }
}

fn read_api_key(supplied_key: Option<&str>) -> Result<String, CliError> {
    if std::io::stdin().is_terminal() {
        if supplied_key.is_some() {
            return Err(CliError::usage(
                "auth login api-key does not accept --api-key from a terminal",
            ));
        }
        return read_hidden_tty_api_key();
    }

    match supplied_key {
        Some(key) => Ok(key.to_owned()),
        None => read_stdin_api_key(),
    }
}

#[cfg(unix)]
fn read_hidden_tty_api_key() -> Result<String, CliError> {
    struct EchoGuard(libc::termios);

    impl Drop for EchoGuard {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
            }
        }
    }

    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
        return Err(CliError::authentication("API-key input is unavailable"));
    }
    let original = unsafe { original.assume_init() };
    let _guard = EchoGuard(original);
    let mut hidden = original;
    hidden.c_lflag &= !libc::ECHO;
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &hidden) } != 0 {
        return Err(CliError::authentication("API-key input is unavailable"));
    }

    eprint!("API key: ");
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(|_| CliError::authentication("API-key input is unavailable"))?;
    eprintln!();
    normalize_api_key_input(&input)
}

#[cfg(not(unix))]
fn read_hidden_tty_api_key() -> Result<String, CliError> {
    Err(CliError::authentication("API-key input is unavailable"))
}

fn read_stdin_api_key() -> Result<String, CliError> {
    const MAX_API_KEY_INPUT_BYTES: u64 = 8192;

    let mut input = String::new();
    std::io::stdin()
        .take(MAX_API_KEY_INPUT_BYTES + 1)
        .read_to_string(&mut input)
        .map_err(|_| CliError::authentication("API-key input is unavailable"))?;
    if input.len() as u64 > MAX_API_KEY_INPUT_BYTES {
        return Err(CliError::usage("auth login api-key input is too long"));
    }
    normalize_api_key_input(&input)
}

fn normalize_api_key_input(input: &str) -> Result<String, CliError> {
    let input = input
        .strip_suffix("\r\n")
        .or_else(|| input.strip_suffix('\n'))
        .or_else(|| input.strip_suffix('\r'))
        .unwrap_or(input);
    if input.contains(['\n', '\r']) {
        return Err(CliError::usage(
            "auth login api-key requires exactly one input line",
        ));
    }
    let input = input.trim();
    if input.is_empty() {
        return Err(CliError::usage(
            "auth login api-key requires a non-empty API key",
        ));
    }
    Ok(input.to_owned())
}

fn provider_status(path: &Path, provider: CredentialProvider) -> Result<String, CliError> {
    match provider {
        CredentialProvider::OpenAiApi => {
            let contents = fs::read_to_string(path).map_err(|_| {
                CliError::authentication("OpenAI API credentials are unavailable or invalid")
            })?;
            let ready = serde_json::from_str::<serde_json::Value>(&contents)
                .ok()
                .and_then(|root| root.get(provider.identifier()).cloned())
                .and_then(|entry| entry.get("api_key").cloned())
                .and_then(|key| key.as_str().map(|key| !key.trim().is_empty()))
                .unwrap_or(false);
            if ready {
                Ok("OpenAI API authentication: ready\n".to_owned())
            } else {
                Err(CliError::authentication(
                    "OpenAI API credentials are unavailable or invalid",
                ))
            }
        }
        CredentialProvider::OpenAiChatGpt => {
            let state =
                load_chatgpt_auth_state(path, std::time::SystemTime::now()).map_err(|_| {
                    CliError::authentication("ChatGPT credentials are unavailable or invalid")
                })?;
            let status = match state {
                ChatGptAuthState::Ready => "ready",
                ChatGptAuthState::RefreshRequired => "refresh required",
            };
            Ok(format!("ChatGPT authentication: {status}\n"))
        }
    }
}

fn run_auth_login(
    dependencies: &CliDependencies,
    device_auth: bool,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    if cancellation.is_cancelled() {
        return Err(chatgpt_login_error(LoginError::Cancelled));
    }
    if cancellation.is_expired() {
        return Err(chatgpt_login_error(LoginError::TimedOut));
    }
    let bootstrap = bootstrap(dependencies)?;
    let mut output =
        (dependencies.auth_login)(&bootstrap.paths.credentials, device_auth, cancellation)?;
    output.push_str("Logged in to ChatGPT.\n");
    Ok(output)
}

fn run_production_auth_login(
    path: &Path,
    device_auth: bool,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    let cancellation_view = cancellation.adapter_view();
    let login_cancellation =
        LoginCancellation::from_shared_flag(cancellation_view.cancellation_handle());
    let deadline = cancellation_view
        .deadline()
        .unwrap_or_else(|| std::time::Instant::now() + std::time::Duration::from_secs(600));
    ChatGptAuthCoordinator::production()
        .login(
            path,
            if device_auth {
                ChatGptAuthFlow::Device
            } else {
                ChatGptAuthFlow::Browser
            },
            login_cancellation,
            deadline,
            |progress| match progress {
                ChatGptAuthProgress::BrowserUrl(url) => {
                    let _ = writeln!(std::io::stdout(), "Open {url} to authenticate.");
                    let _ = std::io::stdout().flush();
                }
                ChatGptAuthProgress::DeviceCode {
                    verification_url,
                    user_code,
                } => {
                    let _ = writeln!(
                        std::io::stdout(),
                        "Open {verification_url} and enter code {user_code}."
                    );
                    let _ = std::io::stdout().flush();
                }
            },
        )
        .map_err(|error| CliError::authentication(error.message()))?;
    Ok(String::new())
}

fn chatgpt_login_error(error: LoginError) -> CliError {
    CliError::authentication(error.stage_message())
}

fn run_chat(
    arguments: &[String],
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    if matches!(arguments, [argument] if is_help(argument)) {
        return Ok("Usage: agens chat [flags] <prompt>\n".to_owned());
    }

    let request = parse_chat_request(arguments)?;
    cancellation_result(cancellation)?;
    let bootstrap = bootstrap(dependencies)?;
    let output = (dependencies.headless_chat)(request, &bootstrap, cancellation)?;
    cancellation_result(cancellation)?;

    Ok(format!("{output}\n"))
}

fn run_models(arguments: &[String]) -> Result<String, CliError> {
    if arguments.iter().any(|argument| is_help(argument)) {
        return Ok("Usage: agens models\n".to_owned());
    }

    match arguments {
        [command] if is_help(command) => Ok("Usage: agens models\n".to_owned()),
        [] => model_registry::bundled_openai_models()
            .map(|models| model_registry::format_models(&models))
            .map_err(|_| CliError::unavailable("model registry is unavailable")),
        _ => Err(CliError::usage("models does not accept arguments")),
    }
}

fn run_sessions(arguments: &[String], dependencies: &CliDependencies) -> Result<String, CliError> {
    if arguments.iter().any(|argument| is_help(argument)) {
        return Ok("Usage: agens sessions <list|show|rm>\n".to_owned());
    }

    match arguments {
        [command] if is_help(command) => Ok("Usage: agens sessions <list|show|rm>\n".to_owned()),
        [command] if command == "list" => {
            let bootstrap = bootstrap(dependencies)?;
            let store = SessionStore::open(&bootstrap.data_directory)
                .map_err(|_| CliError::storage("sessions database is unavailable"))?;
            let sessions = store
                .list_sessions()
                .map_err(|_| CliError::storage("saved sessions could not be listed"))?;

            if sessions.is_empty() {
                return Ok("No saved sessions.\n".to_owned());
            }

            let rows = sessions
                .iter()
                .map(|session| {
                    format!(
                        "{}\t{}\t{}\t{}\t{}",
                        session.id,
                        session.project,
                        session.title,
                        session.active_agent,
                        session.completed_turn_count
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(format!("ID\tPROJECT\tTITLE\tAGENT\tTURNS\n{rows}\n"))
        }
        [command, identifier] if command == "show" => {
            let identifier = identifier
                .parse::<i64>()
                .map_err(|_| CliError::usage("sessions show requires a numeric id"))?;
            let bootstrap = bootstrap(dependencies)?;
            let store = SessionStore::open(&bootstrap.data_directory)
                .map_err(|_| CliError::storage("sessions database is unavailable"))?;
            let session = store
                .load_session_for_resume(identifier)
                .map_err(|_| CliError::storage("saved session is unavailable"))?;
            Ok(format!(
                "Session {identifier}: project={} title={} agent={} turns={} messages={}\n",
                session.metadata.project,
                session.metadata.title,
                session.metadata.active_agent,
                session.metadata.completed_turn_count,
                session.messages.len()
            ))
        }
        [command, identifier] if command == "rm" => {
            let identifier = identifier
                .parse::<i64>()
                .map_err(|_| CliError::usage("sessions rm requires a numeric id"))?;
            let bootstrap = bootstrap(dependencies)?;
            let mut store = SessionStore::open(&bootstrap.data_directory)
                .map_err(|_| CliError::storage("sessions database is unavailable"))?;
            store
                .delete_session(identifier)
                .map_err(|_| CliError::storage("saved session could not be removed"))?;
            Ok(format!("Removed session {identifier}.\n"))
        }
        _ => Err(CliError::usage("sessions requires list, show, or rm")),
    }
}

fn run_tui(dependencies: &CliDependencies, resume: Option<i64>) -> Result<String, CliError> {
    let bootstrap = bootstrap(dependencies)?;
    let output = (dependencies.tui_launcher)(&bootstrap, resume)?;
    Ok(format!("{output}\n"))
}

struct ProductionTuiEngine {
    cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
}

#[allow(dead_code)]
#[derive(Default)]
struct AttemptActivityRegistry {
    active: Mutex<Vec<AttemptKey>>,
}

static ACTIVE_SESSION_ATTEMPTS: OnceLock<AttemptActivityRegistry> = OnceLock::new();

fn active_session_attempts() -> &'static AttemptActivityRegistry {
    ACTIVE_SESSION_ATTEMPTS.get_or_init(AttemptActivityRegistry::default)
}

#[allow(dead_code)]
impl AttemptActivityRegistry {
    fn begin_and_register(
        &self,
        store: &mut SessionStore,
        metadata: &SessionMetadata,
        prompt: String,
    ) -> Result<agens_core::SessionAttemptSummary, BeginSessionAttemptError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| BeginSessionAttemptError::Store)?;
        let attempt = store.begin_session_attempt(metadata, prompt)?;
        active.push(attempt.key());
        Ok(attempt)
    }

    fn contains(&self, key: AttemptKey) -> bool {
        self.active.lock().is_ok_and(|active| active.contains(&key))
    }

    fn unregister(&self, key: AttemptKey) {
        if let Ok(mut active) = self.active.lock()
            && let Some(index) = active.iter().position(|active_key| *active_key == key)
        {
            active.remove(index);
        }
    }

    fn recover_running_attempt(
        &self,
        store: &mut SessionStore,
        key: AttemptKey,
        finished_at: i64,
    ) -> Result<Option<RecoveryOutcome>, ()> {
        let active = self.active.lock().map_err(|_| ())?;
        if active.contains(&key) {
            return Ok(None);
        }

        store
            .recover_running_attempt(key, finished_at)
            .map(Some)
            .map_err(|_| ())
    }
}

struct RegisteredAttempt<'a> {
    registry: &'a AttemptActivityRegistry,
    key: AttemptKey,
}

impl Drop for RegisteredAttempt<'_> {
    fn drop(&mut self) {
        self.registry.unregister(self.key);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AttemptLifecycleError {
    Begin(BeginSessionAttemptError),
    Runtime {
        error: CliError,
        partial: Option<Box<PartialTurnRecord>>,
    },
}

impl AttemptLifecycleError {
    fn runtime(error: CliError) -> Self {
        Self::Runtime {
            error,
            partial: None,
        }
    }
}

/// History persisted for an attempt that ended without a completed turn, carried out of the
/// failing path so the caller can keep owning the same session instead of minting a new one.
#[derive(Clone, PartialEq, Eq)]
struct PartialTurnRecord {
    metadata: SessionMetadata,
    messages: Vec<Message>,
}

impl fmt::Debug for PartialTurnRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PartialTurnRecord")
            .field("session", &self.metadata.id)
            .field("messages", &self.messages.len())
            .finish()
    }
}

#[derive(Debug)]
struct SessionAttemptCompletion {
    snapshot: CompletedTurnSnapshot,
    metadata: SessionMetadata,
    messages: Vec<Message>,
}

#[allow(dead_code)]
enum ExplicitAttemptRecoveryOutcome {
    LocallyActive,
    Stale,
    Recovered(Box<SessionAttemptCompletion>),
}

impl fmt::Debug for ExplicitAttemptRecoveryOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = match self {
            Self::LocallyActive => "LocallyActive",
            Self::Stale => "Stale",
            Self::Recovered(_) => "Recovered",
        };

        formatter.write_str(status)
    }
}

#[allow(dead_code)]
fn recover_session_attempt_lifecycle(
    registry: &AttemptActivityRegistry,
    store: &mut SessionStore,
    key: AttemptKey,
    finished_at: i64,
    runtime: impl FnOnce(
        Vec<Message>,
        &str,
        &SessionMetadata,
    ) -> Result<(CompletedTurnSnapshot, CompletedSessionTurn), CliError>,
) -> Result<ExplicitAttemptRecoveryOutcome, AttemptLifecycleError> {
    let Some(recovery) = registry
        .recover_running_attempt(store, key, finished_at)
        .map_err(|_| {
            AttemptLifecycleError::runtime(CliError::storage("attempt recovery failed"))
        })?
    else {
        return Ok(ExplicitAttemptRecoveryOutcome::LocallyActive);
    };
    if recovery == RecoveryOutcome::Stale {
        return Ok(ExplicitAttemptRecoveryOutcome::Stale);
    }

    let boundary = store
        .load_retry_boundary(key)
        .map_err(|_| AttemptLifecycleError::runtime(CliError::storage("attempt recovery failed")))?
        .ok_or_else(|| {
            AttemptLifecycleError::runtime(CliError::storage("attempt recovery failed"))
        })?;
    let stored = store
        .load_session_for_resume(key.session_id())
        .map_err(|_| {
            AttemptLifecycleError::runtime(CliError::storage("attempt recovery failed"))
        })?;
    let metadata = stored.metadata;
    let runtime_metadata = metadata.clone();
    let history = stored.messages;
    let prompt = boundary.prompt().to_owned();
    let completion =
        run_session_attempt_lifecycle(registry, store, metadata, prompt.clone(), || {
            runtime(history, &prompt, &runtime_metadata)
        })?;

    Ok(ExplicitAttemptRecoveryOutcome::Recovered(Box::new(
        completion,
    )))
}

fn run_session_attempt_lifecycle(
    registry: &AttemptActivityRegistry,
    store: &mut SessionStore,
    metadata: SessionMetadata,
    prompt: String,
    runtime: impl FnOnce() -> Result<(CompletedTurnSnapshot, CompletedSessionTurn), CliError>,
) -> Result<SessionAttemptCompletion, AttemptLifecycleError> {
    run_session_attempt_lifecycle_with_terminal_writer(
        registry,
        store,
        metadata,
        prompt,
        runtime,
        |store, write| write_terminal_attempt(store, write, &interrupted_turn_note(&[])),
    )
}

/// Terminal state of an attempt whose runtime failed, handed to the writer that records it.
struct TerminalAttemptWrite<'a> {
    key: AttemptKey,
    status: agens_core::SessionAttemptStatus,
    metadata: &'a SessionMetadata,
    prompt: &'a str,
    finished_at: i64,
}

fn run_session_attempt_lifecycle_with_terminal_writer(
    registry: &AttemptActivityRegistry,
    store: &mut SessionStore,
    mut metadata: SessionMetadata,
    prompt: String,
    runtime: impl FnOnce() -> Result<(CompletedTurnSnapshot, CompletedSessionTurn), CliError>,
    terminal_writer: impl FnOnce(
        &mut SessionStore,
        TerminalAttemptWrite<'_>,
    ) -> Result<Option<PartialTurnRecord>, ()>,
) -> Result<SessionAttemptCompletion, AttemptLifecycleError> {
    let attempt = registry
        .begin_and_register(store, &metadata, prompt.clone())
        .map_err(AttemptLifecycleError::Begin)?;
    let _registered = RegisteredAttempt {
        registry,
        key: attempt.key(),
    };
    metadata.id = attempt.key().session_id();

    let (snapshot, turn) = match runtime() {
        Ok(completion) => completion,
        Err(error) => {
            let partial = terminal_writer(
                store,
                TerminalAttemptWrite {
                    key: attempt.key(),
                    status: attempt_failure_status(&error),
                    metadata: &metadata,
                    prompt: &prompt,
                    finished_at: current_session_timestamp(),
                },
            )
            .ok()
            .flatten();

            return Err(AttemptLifecycleError::Runtime {
                error,
                partial: partial.map(Box::new),
            });
        }
    };

    match store
        .persist_completed_session_attempt(
            attempt.key(),
            &metadata,
            &turn,
            current_session_timestamp(),
        )
        .map_err(|error| {
            CliError::storage(format!("completed session could not be saved: {error}"))
        })
        .map_err(AttemptLifecycleError::runtime)?
    {
        agens_core::AttemptFinishOutcome::Finished => {}
        agens_core::AttemptFinishOutcome::Stale => {
            return Err(AttemptLifecycleError::runtime(CliError::storage(
                "completed session could not be saved",
            )));
        }
    }

    let stored = store
        .load_session_for_resume(metadata.id)
        .map_err(|_| CliError::storage("completed session could not be loaded"))
        .map_err(AttemptLifecycleError::runtime)?;

    Ok(SessionAttemptCompletion {
        snapshot,
        metadata: stored.metadata,
        messages: stored.messages,
    })
}

fn attempt_failure_status(error: &CliError) -> agens_core::SessionAttemptStatus {
    match error.category {
        "cancelled" | "timeout" => agens_core::SessionAttemptStatus::Cancelled,
        "auth" | "provider" => agens_core::SessionAttemptStatus::ProviderError,
        _ => agens_core::SessionAttemptStatus::Failed,
    }
}

/// Records an interrupted attempt (explicit cancellation or an expired deadline) as history, so
/// the next turn keeps the prompt and knows the turn stopped early. Every other failure keeps its
/// retained retry prompt instead, because its recovery path replays that prompt rather than
/// continuing the conversation.
fn write_terminal_attempt(
    store: &mut SessionStore,
    write: TerminalAttemptWrite<'_>,
    note: &str,
) -> Result<Option<PartialTurnRecord>, ()> {
    if write.status != agens_core::SessionAttemptStatus::Cancelled {
        return store
            .finish_session_attempt(write.key, write.status, write.finished_at)
            .map(|_| None)
            .map_err(|_| ());
    }

    let turn = interrupted_session_turn(write.prompt, note).map_err(|_| ())?;
    let outcome = store
        .persist_partial_session_attempt(
            write.key,
            write.metadata,
            &turn,
            write.status,
            write.finished_at,
        )
        .map_err(|_| ())?;
    if outcome == agens_core::AttemptFinishOutcome::Stale {
        return Ok(None);
    }

    let stored = store
        .load_session_for_resume(write.metadata.id)
        .map_err(|_| ())?;

    Ok(Some(PartialTurnRecord {
        metadata: stored.metadata,
        messages: stored.messages,
    }))
}

/// Keeps the interrupted turn to the prompt and a plain assistant note: a tool call that never
/// answered must not gain a fabricated result, because the tool may already have changed the
/// project and claiming otherwise would assert something unverified.
fn interrupted_session_turn(
    prompt: &str,
    note: &str,
) -> Result<CompletedSessionTurn, SessionMessageError> {
    let messages = [
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text(prompt.to_owned())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text(note.to_owned())],
        },
    ]
    .into_iter()
    .map(SessionMessage::try_from)
    .collect::<Result<Vec<_>, _>>()?;

    CompletedSessionTurn::new(messages).map_err(|_| SessionMessageError::EmptyParts)
}

const MAX_NOTED_REQUESTED_SUBAGENTS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestedSubagent {
    agent: String,
    description: String,
}

/// Describes the turn as interrupted rather than cancelled by the user, because an expired
/// deadline reaches this path with the same terminal status as an explicit cancellation.
fn interrupted_turn_note(requested: &[RequestedSubagent]) -> String {
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

fn record_requested_subagent(requested: &Mutex<Vec<RequestedSubagent>>, event: &TurnEvent) {
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

struct TuiMetricsPublisher {
    bridge: BridgeTx<TuiRuntimeEvent>,
    cancellation: BridgeCancel,
    model_id: String,
    turn_started_at: Option<std::time::Instant>,
    tools: BTreeMap<String, (String, std::time::Instant)>,
}

impl TuiMetricsPublisher {
    fn new(
        bridge: BridgeTx<TuiRuntimeEvent>,
        cancellation: BridgeCancel,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            bridge,
            cancellation,
            model_id: model_id.into(),
            turn_started_at: None,
            tools: BTreeMap::new(),
        }
    }

    fn observe(&mut self, event: &TurnEvent) {
        let now = std::time::Instant::now();
        let completed_tool = match event {
            TurnEvent::ToolResult(MessagePart::ToolResult { tool_call_id, .. }) => {
                self.tools.remove(tool_call_id)
            }
            _ => None,
        };
        let metric = match event {
            TurnEvent::StateChanged(TurnState::Requesting) => {
                if self.turn_started_at.is_none() {
                    self.turn_started_at = Some(now);
                    Some(TuiRuntimeEvent::TurnStarted)
                } else {
                    None
                }
            }
            TurnEvent::StateChanged(
                TurnState::Completed | TurnState::Cancelled | TurnState::Failed,
            ) => None,
            TurnEvent::Usage(usage) => {
                let mut usage = usage.clone();
                if usage.context_window.is_none() {
                    usage.context_window = model_registry::context_window_for(&self.model_id);
                }
                Some(TuiRuntimeEvent::Usage(usage))
            }
            TurnEvent::ToolCallRequested { id, name, input } => {
                self.tools.insert(id.clone(), (name.clone(), now));
                Some(TuiRuntimeEvent::ToolStarted {
                    call_id: id.clone(),
                    name: name.clone(),
                    input: sanitize_tui_metric(input),
                    parsed: agens_core::ToolInput::parse(name, input),
                })
            }
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id,
                is_error,
                ..
            }) => {
                let duration = completed_tool
                    .as_ref()
                    .map(|(_, started)| now.duration_since(*started));
                Some(TuiRuntimeEvent::ToolEnded {
                    call_id: tool_call_id.clone(),
                    duration,
                    result: if *is_error {
                        ToolResultState::Failure
                    } else {
                        ToolResultState::Success
                    },
                })
            }
            TurnEvent::ProviderPart(_) | TurnEvent::StateChanged(_) => None,
            TurnEvent::ToolResult(_) => None,
        };

        if let Some(event) = metric {
            let _ = self.bridge.publish(event, &self.cancellation, None);
        }

        if let TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id,
            content,
            is_error: false,
        }) = event
            && completed_tool
                .as_ref()
                .is_some_and(|(name, _)| name.ends_with("::edit"))
        {
            let lines = parse_edit_diff(&sanitize_tui_metric(content));
            if !lines.is_empty() {
                let _ = self.bridge.publish(
                    TuiRuntimeEvent::Diff {
                        call_id: tool_call_id.clone(),
                        lines,
                    },
                    &self.cancellation,
                    None,
                );
            }
        }
    }

    fn finish(&mut self, result: Result<(), &CliError>) {
        let status = match result {
            Ok(()) => TurnState::Completed,
            Err(error) if error.category == "cancelled" => TurnState::Cancelled,
            Err(_) => TurnState::Failed,
        };
        let duration = self.turn_started_at.take().map(|started| started.elapsed());
        let _ = self.bridge.publish(
            TuiRuntimeEvent::TurnEnded { status, duration },
            &self.cancellation,
            None,
        );
    }
}

fn finish_tui_metrics<T>(metrics: &Arc<Mutex<TuiMetricsPublisher>>, result: &Result<T, CliError>) {
    if let Ok(mut metrics) = metrics.lock() {
        metrics.finish(result.as_ref().map(|_| ()));
    }
}

fn sanitize_tui_metric(value: &str) -> String {
    if contains_sensitive_marker(value) {
        "[redacted]".to_owned()
    } else {
        value.to_owned()
    }
}

fn parse_edit_diff(diff: &str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let mut old_number = 0;
    let mut new_number = 0;

    for line in diff.lines() {
        if let Some((old, new)) = parse_diff_hunk(line) {
            old_number = old;
            new_number = new;
        } else if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        } else if let Some(text) = line.strip_prefix('-') {
            lines.push(DiffLine::new(old_number, DiffLineKind::Removed, text));
            old_number += 1;
        } else if let Some(text) = line.strip_prefix('+') {
            lines.push(DiffLine::new(new_number, DiffLineKind::Added, text));
            new_number += 1;
        } else if line.starts_with(' ') {
            old_number += 1;
            new_number += 1;
        }
    }

    lines
}

fn parse_diff_hunk(line: &str) -> Option<(u32, u32)> {
    let ranges = line.strip_prefix("@@ -")?.strip_suffix(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    Some((
        old.split_once(',')?.0.parse().ok()?,
        new.split_once(',')?.0.parse().ok()?,
    ))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TuiSessionContext {
    identifier: Option<i64>,
    metadata: Option<SessionMetadata>,
    messages: Vec<Message>,
    restored_history: Vec<Conversation>,
    active_agent: Option<ActiveAgentRuntime>,
    pending_system_reminder: Option<String>,
    selection: Option<TuiModelSelector>,
    provider: Option<TuiProvider>,
    chatgpt_unavailable: bool,
    resume_error: Option<String>,
    resume_notice: Option<String>,
    agent_correction_pending: bool,
    resume_draft: Option<ResumeDraft>,
    selected_subagent: Option<String>,
    dangerous_mode: bool,
    running: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct ResumeDraft(String);

impl ResumeDraft {
    fn new(prompt: String) -> Self {
        Self(prompt)
    }

    fn into_inner(self) -> String {
        self.0
    }
}

impl std::ops::Deref for ResumeDraft {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Debug for ResumeDraft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ResumeDraft([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CompletedSubagentTurn {
    id: u64,
    agent: String,
    task: String,
    final_result: String,
    tool_uses: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TuiSessionMutationError {
    Busy,
}

fn reset_tui_session(context: &mut TuiSessionContext) -> Result<(), TuiSessionMutationError> {
    if context.running {
        return Err(TuiSessionMutationError::Busy);
    }

    *context = TuiSessionContext::fresh();
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveAgentRuntime {
    name: String,
    model: Option<String>,
    system_prompt: String,
    capabilities: EffectiveCapabilitySet,
}
impl ActiveAgentRuntime {
    fn build(
        agent: &AgentDefinition,
        inherited_model: Option<&str>,
        project: &str,
        dispatcher: &ToolDispatcher,
        validator: &dyn AgentModelValidator,
    ) -> Result<Self, AgentRotationError> {
        if agent
            .model
            .as_deref()
            .is_some_and(|model| validator.validate_model(model).is_err())
        {
            return Err(AgentRotationError::ModelUnavailable);
        }
        let model = agent
            .model
            .as_deref()
            .or(inherited_model)
            .map(str::to_owned);
        Ok(Self {
            name: agent.name.clone(),
            model,
            system_prompt: agent.system_prompt.clone(),
            capabilities: EffectiveCapabilitySet::from_agent(agent, project, dispatcher),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentRotationError {
    Busy,
    ModelUnavailable,
    Persistence,
}
fn rotate_active_agent(
    context: &mut TuiSessionContext,
    candidate: &AgentDefinition,
    inherited_model: Option<&str>,
    project: &str,
    dispatcher: &ToolDispatcher,
    validator: &dyn AgentModelValidator,
    store: Option<&mut SessionStore>,
) -> Result<(), AgentRotationError> {
    if context.running {
        return Err(AgentRotationError::Busy);
    }
    let next =
        ActiveAgentRuntime::build(candidate, inherited_model, project, dispatcher, validator)?;
    let reminder = context.active_agent.as_ref().and_then(|current| {
        next.capabilities
            .is_expansion_from(&current.capabilities)
            .then(|| {
                format!(
                    "Agent capabilities expanded: {} -> {}.",
                    current.name, next.name
                )
            })
    });

    let metadata = match (&context.metadata, store) {
        (Some(metadata), Some(store)) => {
            let mut metadata = metadata.clone();
            metadata.active_agent = next.name.clone();
            metadata.updated_at = session_timestamp().ok_or(AgentRotationError::Persistence)?;
            store
                .update_session(&metadata)
                .map_err(|_| AgentRotationError::Persistence)?;
            Some(metadata)
        }
        (Some(_), None) => return Err(AgentRotationError::Persistence),
        (None, _) => None,
    };

    context.active_agent = Some(next);
    context.metadata = metadata;
    if reminder.is_some() {
        context.pending_system_reminder = reminder;
    }

    Ok(())
}

fn session_timestamp() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
}

fn current_session_timestamp() -> i64 {
    session_timestamp().unwrap_or_default()
}

fn parse_recovery_action(action_id: &str) -> Option<AttemptKey> {
    let mut parts = action_id.split(':');
    let (Some("session"), Some("recover"), Some(session_id), Some(attempt_id), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };

    AttemptKey::new(session_id.parse().ok()?, attempt_id.parse().ok()?).ok()
}

fn session_dialog_entry(
    session: &StoredSession,
    current_session: Option<i64>,
    all_projects: bool,
    now: i64,
) -> DialogEntry {
    let metadata = &session.metadata;
    let age = session_relative_age(metadata.updated_at, now);
    let turns = if metadata.completed_turn_count == 1 {
        "1 turn".to_owned()
    } else {
        format!("{} turns", metadata.completed_turn_count)
    };
    let current = (current_session == Some(metadata.id)).then_some(" · current");
    let root = all_projects.then(|| format!(" · root={}", compact_session_root(&metadata.project)));
    let attempt_status = session
        .latest_attempt
        .as_ref()
        .map(|attempt| {
            format!(
                " · Attempt: {}",
                session_attempt_status_label(attempt.status())
            )
        })
        .unwrap_or_default();
    let row_detail = format!("{turns} · {age}");
    let selected_detail = format!(
        "Turns: {} · Agent: {}{}\nProvider: {} · Model: {}\nEffort: {} · Updated: {} ({}){} · ID: {} · {}{}",
        metadata.completed_turn_count,
        metadata.active_agent,
        current.unwrap_or_default(),
        metadata.provider_id.as_deref().unwrap_or("current runtime"),
        metadata.model_id.as_deref().unwrap_or("current runtime"),
        metadata
            .reasoning_effort
            .map(agens_core::ReasoningEffort::as_str)
            .unwrap_or_else(|| {
                if metadata.provider_id.is_some() || metadata.model_id.is_some() {
                    "Default"
                } else {
                    "current runtime"
                }
            }),
        metadata.updated_at,
        age,
        root.as_deref().unwrap_or_default(),
        metadata.id,
        metadata.title,
        attempt_status,
    );

    DialogEntry::action_with_metadata(
        format!("#{} {}", metadata.id, metadata.title),
        row_detail,
        format!(
            "{} {} {} {}",
            metadata.id, metadata.title, metadata.project, metadata.active_agent
        ),
        selected_detail,
        format!("session:{}", metadata.id),
    )
}

fn session_attempt_status_label(status: agens_core::SessionAttemptStatus) -> &'static str {
    match status {
        agens_core::SessionAttemptStatus::Running => "running",
        agens_core::SessionAttemptStatus::Completed => "completed",
        agens_core::SessionAttemptStatus::Cancelled => "cancelled",
        agens_core::SessionAttemptStatus::Failed => "failed",
        agens_core::SessionAttemptStatus::ProviderError => "provider error",
        agens_core::SessionAttemptStatus::Interrupted => "interrupted",
    }
}

fn resume_retry_notice(status: SessionAttemptStatus) -> Option<&'static str> {
    match status {
        SessionAttemptStatus::Cancelled
        | SessionAttemptStatus::Interrupted
        | SessionAttemptStatus::Failed
        | SessionAttemptStatus::ProviderError => {
            Some("Recovered failed prompt · Enter retry · Esc discard")
        }
        SessionAttemptStatus::Running | SessionAttemptStatus::Completed => None,
    }
}

fn recovery_confirmation_dialog(
    metadata: &SessionMetadata,
    attempt: &agens_core::SessionAttemptSummary,
    refusal: Option<&str>,
) -> DialogView {
    let mut help = format!(
        "Session: {} · ID: {}\nStatus: running\nStarted: {}\nThis may invalidate an attempt still running in another process.",
        metadata.title,
        metadata.id,
        attempt.started_at(),
    );
    if let Some(refusal) = refusal {
        help.push('\n');
        help.push_str(refusal);
    }

    DialogView::selection(
        "Recover interrupted attempt",
        Some(help),
        vec![
            DialogEntry::action(
                "Recover interrupted attempt",
                format!(
                    "session:recover:{}:{}",
                    attempt.key().session_id(),
                    attempt.key().attempt_id()
                ),
            ),
            DialogEntry::cancel("Cancel"),
        ],
    )
}

fn compact_session_root(root: &str) -> String {
    const MAX_CHARS: usize = 30;
    let character_count = root.chars().count();
    if character_count <= MAX_CHARS {
        return root.into();
    }

    format!(
        "...{}",
        root.chars()
            .skip(character_count - MAX_CHARS)
            .collect::<String>()
    )
}

fn session_relative_age(updated_at: i64, now: i64) -> String {
    let age = now.saturating_sub(updated_at);
    match age {
        ..=0 => "now".into(),
        1..=59 => format!("{age}s ago"),
        60..=3_599 => format!("{}m ago", age / 60),
        3_600..=86_399 => format!("{}h ago", age / 3_600),
        _ => format!("{}d ago", age / 86_400),
    }
}

impl TuiSessionContext {
    fn fresh() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn resumed(
        identifier: i64,
        metadata: SessionMetadata,
        messages: Vec<Message>,
        active_agent: ActiveAgentRuntime,
    ) -> Self {
        Self {
            identifier: Some(identifier),
            metadata: Some(metadata),
            messages,
            restored_history: Vec::new(),
            active_agent: Some(active_agent),
            pending_system_reminder: None,
            selection: None,
            provider: None,
            chatgpt_unavailable: false,
            resume_error: None,
            resume_notice: None,
            agent_correction_pending: false,
            resume_draft: None,
            selected_subagent: None,
            dangerous_mode: false,
            running: false,
        }
    }

    fn restored(
        identifier: i64,
        metadata: SessionMetadata,
        messages: Vec<Message>,
        restored_history: Vec<Conversation>,
    ) -> Self {
        Self {
            identifier: Some(identifier),
            metadata: Some(metadata),
            messages,
            restored_history,
            active_agent: None,
            pending_system_reminder: None,
            selection: None,
            provider: None,
            chatgpt_unavailable: false,
            resume_error: None,
            resume_notice: None,
            agent_correction_pending: false,
            resume_draft: None,
            selected_subagent: None,
            dangerous_mode: false,
            running: false,
        }
    }

    fn note(&self) -> String {
        if let Some(notice) = &self.resume_notice {
            return notice.clone();
        }
        if let Some(error) = &self.resume_error {
            return error.clone();
        }
        let identifier = self
            .identifier
            .expect("resumed TUI session context always has an identifier");
        let metadata = self
            .metadata
            .as_ref()
            .expect("resumed TUI session context always has metadata");
        format!(
            "Resumed session {identifier}: agent={} turns={}",
            metadata.active_agent, metadata.completed_turn_count
        )
    }

    fn apply_to(&self, mut request: HeadlessChatRequest) -> HeadlessChatRequest {
        request.dangerous_mode = self.dangerous_mode;
        if self.identifier.is_some() {
            request.history = self.messages.clone();
            request.session = self.metadata.clone();
        }

        let selected_model = self.selection.as_ref().map(|selection| {
            request.model = Some(selection.model().to_owned());
            request.request_config = selection.request_config().clone();
            request.session_reasoning_effort = selection.reasoning_effort_value();
            selection.model()
        });
        if let Some(agent) = &self.active_agent {
            let overrides_selection = selected_model.is_some_and(|selected| {
                agent
                    .model
                    .as_deref()
                    .is_some_and(|model| model != selected)
            });
            if request.model.is_none() || overrides_selection {
                request.model = agent.model.clone();
            }
            if overrides_selection {
                request.request_config = Default::default();
                request.session_reasoning_effort = None;
            }
            request
                .system_prompt
                .get_or_insert_with(|| agent.system_prompt.clone());
            request.active_agent = Some(agent.name.clone());
            request.effective_capabilities = Some(agent.capabilities.clone());
        }
        request.pending_system_reminder = self.pending_system_reminder.clone();

        request
    }
}

#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TuiProvider {
    OpenAiApi,
    OpenAiChatGpt,
}

impl TuiProvider {
    const ALL: [Self; 2] = [Self::OpenAiChatGpt, Self::OpenAiApi];

    const fn identifier(self) -> &'static str {
        ["openai-api", "openai-chatgpt"][self as usize]
    }

    const fn label(self) -> &'static str {
        ["OpenAI API", "ChatGPT subscription"][self as usize]
    }

    const fn source(self) -> TuiModelSource {
        [
            TuiModelSource::OpenAiApi,
            TuiModelSource::ChatGptSubscription,
        ][self as usize]
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.identifier() == value)
    }
}

#[repr(usize)]
#[derive(Clone, Copy)]
enum TuiProviderStatus {
    Ready,
    RefreshRequired,
    ConnectRequired,
    CredentialRequired,
}

impl TuiProviderStatus {
    const fn label(self) -> &'static str {
        [
            "ready",
            "refresh required",
            "connect required",
            "credential required",
        ][self as usize]
    }

    const fn available(self) -> bool {
        matches!(self, Self::Ready | Self::RefreshRequired)
    }
}

#[derive(Clone)]
struct TuiCredentialResolver {
    environment: Arc<dyn Fn() -> BTreeMap<String, String> + Send + Sync>,
}

impl TuiCredentialResolver {
    fn production() -> Self {
        Self {
            environment: Arc::new(|| std::env::vars().collect()),
        }
    }

    #[cfg(test)]
    fn with_environment(environment: BTreeMap<String, String>) -> Self {
        Self::with_environment_resolver(move || environment.clone())
    }

    #[cfg(test)]
    fn with_environment_resolver(
        resolve: impl Fn() -> BTreeMap<String, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            environment: Arc::new(resolve),
        }
    }

    fn api_key(&self, path: &Path) -> Option<String> {
        let credentials = fs::read_to_string(path).ok();
        openai_api_key(credentials.as_deref(), &(self.environment)())
    }

    fn status(&self, path: &Path, provider: TuiProvider) -> TuiProviderStatus {
        match provider {
            TuiProvider::OpenAiChatGpt => {
                match load_chatgpt_auth_state(path, std::time::SystemTime::now()) {
                    Ok(ChatGptAuthState::Ready) => TuiProviderStatus::Ready,
                    Ok(ChatGptAuthState::RefreshRequired) => TuiProviderStatus::RefreshRequired,
                    Err(_) => TuiProviderStatus::ConnectRequired,
                }
            }
            TuiProvider::OpenAiApi => {
                if self.api_key(path).is_some() {
                    TuiProviderStatus::Ready
                } else {
                    TuiProviderStatus::CredentialRequired
                }
            }
        }
    }
}

impl TuiEngine for ProductionTuiEngine {
    fn cancel(&mut self) {
        if let Ok(cancellation) = self.cancellation.lock()
            && let Some(cancellation) = cancellation.as_ref()
        {
            cancellation.cancel();
        }
    }
}

#[derive(Clone)]
struct TuiRuntimeRouter {
    bootstrap: Arc<Mutex<Bootstrap>>,
    session: Arc<Mutex<TuiSessionContext>>,
    cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
    auth: ChatGptAuthCoordinator,
    credentials: TuiCredentialResolver,
    commands: Arc<CommandCatalog>,
    skills: Arc<SkillCatalog>,
    palette: Arc<[PaletteEntry]>,
    mcp_status: McpStatusHandle,
    _mcp_registry: Arc<Mutex<McpRegistry>>,
    clock: fn() -> i64,
    credential_restorer: Arc<CredentialRestorer>,
}

type CredentialRestorer =
    dyn Fn(&Path, ChatGptCredentialSnapshot) -> Result<(), CliError> + Send + Sync;

impl TuiRuntimeRouter {
    fn new(
        bootstrap: Bootstrap,
        session: Arc<Mutex<TuiSessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
    ) -> Self {
        Self::with_auth_coordinator(
            bootstrap,
            session,
            cancellation,
            commands,
            skills,
            ChatGptAuthCoordinator::production(),
        )
    }

    fn with_auth_coordinator(
        mut bootstrap: Bootstrap,
        session: Arc<Mutex<TuiSessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
        auth: ChatGptAuthCoordinator,
    ) -> Self {
        let has_subagents = session.lock().is_ok_and(|context| {
            tui_subagent_catalog(&bootstrap, &context)
                .is_ok_and(|mut agents| agents.next().is_some())
        });
        let palette = resolved_tui_palette(&commands, &skills, has_subagents).into();
        let project_root = bootstrap.project_root.as_deref().unwrap_or(Path::new("."));
        let registry = Arc::new(Mutex::new(load_configured_mcp_registry(
            &bootstrap,
            project_root,
        )));
        let mcp_status = registry
            .lock()
            .expect("new MCP registry lock")
            .status_handle();
        bootstrap.mcp_status = Some(mcp_status.clone());
        Self {
            bootstrap: Arc::new(Mutex::new(bootstrap)),
            session,
            cancellation,
            auth,
            credentials: TuiCredentialResolver::production(),
            commands,
            skills,
            palette,
            mcp_status,
            _mcp_registry: registry,
            clock: current_session_timestamp,
            credential_restorer: Arc::new(restore_chatgpt_credentials),
        }
    }

    #[cfg(test)]
    fn with_credential_restorer(
        mut self,
        restore: impl Fn(&Path, ChatGptCredentialSnapshot) -> Result<(), CliError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.credential_restorer = Arc::new(restore);
        self
    }

    #[cfg(test)]
    fn with_credential_resolver(
        bootstrap: Bootstrap,
        session: Arc<Mutex<TuiSessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
        credentials: TuiCredentialResolver,
    ) -> Self {
        let mut router = Self::new(bootstrap, session, cancellation, commands, skills);
        router.credentials = credentials;
        router
    }

    #[cfg(test)]
    fn with_clock(
        bootstrap: Bootstrap,
        session: Arc<Mutex<TuiSessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
        clock: fn() -> i64,
    ) -> Self {
        let mut router = Self::new(bootstrap, session, cancellation, commands, skills);
        router.clock = clock;
        router
    }

    #[cfg(test)]
    fn route(&self, input: String) -> TuiSubmissionOutcome {
        let (progress, _) = std::sync::mpsc::channel();
        self.route_with_progress(input, progress)
    }

    #[cfg(test)]
    fn route_with_progress(
        &self,
        input: String,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) -> TuiSubmissionOutcome {
        self.route_with_progress_cancellable(input, progress, TuiRouteCancellation::new())
    }

    fn route_with_progress_cancellable(
        &self,
        input: String,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
        cancellation: TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        let command = input.trim();
        let auth = match command {
            "/connect --device-auth" => Some(self.connect(ChatGptAuthFlow::Device, progress)),
            _ => None,
        };
        if let Some(result) = auth {
            return auth_route_outcome(result);
        }
        self.resolve_with_cancellation(input, &cancellation)
            .unwrap_or_else(|error| TuiSubmissionOutcome::LocalActionableError {
                message: error.to_string(),
                action: TUI_ERROR_ACTION.into(),
            })
    }

    #[cfg(test)]
    fn route_request(
        &self,
        request: TuiRouteRequest,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) -> TuiSubmissionOutcome {
        self.route_request_with_cancellation(request, progress, TuiRouteCancellation::new())
    }

    fn route_request_with_cancellation(
        &self,
        request: TuiRouteRequest,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
        cancellation: TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        let result = match request {
            TuiRouteRequest::Input(input) => {
                return self.route_with_progress_cancellable(input, progress, cancellation);
            }
            TuiRouteRequest::OpenDialog(route_id) => self.open_dialog(&route_id),
            TuiRouteRequest::SessionPage(request) => {
                return self.session_dialog_outcome(request);
            }
            TuiRouteRequest::DialogAction(action_id) => {
                return self.route_dialog_action_with_cancellation(
                    &action_id,
                    progress,
                    &cancellation,
                );
            }
        };
        result.unwrap_or_else(|error| TuiSubmissionOutcome::LocalActionableError {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
        })
    }

    fn open_dialog(&self, route_id: &str) -> Result<TuiSubmissionOutcome, CliError> {
        let bootstrap = self.bootstrap()?;
        let dialog = match route_id {
            "dangerous" => return self.toggle_dangerous_mode(),
            "connect" => DialogView::selection(
                "Connect to ChatGPT",
                Some("Choose an authentication flow"),
                vec![
                    DialogEntry::action("Browser", "connect:browser"),
                    DialogEntry::action("Device Code", "connect:device"),
                ],
            ),
            "disconnect" => DialogView::selection(
                "Disconnect from ChatGPT",
                Some("Remove stored ChatGPT credentials?"),
                vec![
                    DialogEntry::action("Disconnect", "disconnect:confirm"),
                    DialogEntry::cancel("Cancel"),
                ],
            ),
            "diagnostics" => diagnostics_dialog(bootstrap.data_directory()),
            "provider" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                let current = current_tui_provider(&bootstrap, &context);
                let entries = TuiProvider::ALL
                    .into_iter()
                    .filter_map(|provider| {
                        let status = self
                            .credentials
                            .status(&bootstrap.paths.credentials, provider);
                        status.available().then(|| {
                            let label = if Some(provider) == current {
                                format!("{} (current)", provider.label())
                            } else {
                                provider.label().to_owned()
                            };
                            DialogEntry::action_with_detail(
                                label,
                                Some(status.label()),
                                format!("provider:{}", provider.identifier()),
                            )
                        })
                    })
                    .collect();
                let help = current.map_or_else(
                    || "Current: not configured".to_owned(),
                    |provider| {
                        let status = self
                            .credentials
                            .status(&bootstrap.paths.credentials, provider);
                        let remediation = matches!(status, TuiProviderStatus::ConnectRequired)
                            .then_some(" · run /connect")
                            .unwrap_or_default();
                        format!(
                            "Current: {} · {}{remediation}",
                            provider.label(),
                            status.label()
                        )
                    },
                );
                DialogView::selection("Choose provider", Some(help), entries)
            }
            "model" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                let current = context
                    .selection
                    .as_ref()
                    .map(TuiModelSelector::model)
                    .or_else(|| bootstrap.model())
                    .unwrap_or_else(|| default_model(&bootstrap))
                    .to_owned();
                let source = tui_model_source(&bootstrap, &context);
                drop(context);
                let selector = TuiModelSelector::for_source(current.clone(), source);
                let values = selector.models().map_err(CliError::unavailable)?;
                let selected = values
                    .iter()
                    .position(|model| model.id == current)
                    .unwrap_or_default();
                let entries = values
                    .into_iter()
                    .map(|model| {
                        let label = if model.id == current {
                            format!("{} (current)", model.id)
                        } else {
                            model.id.clone()
                        };
                        DialogEntry::action_with_detail(
                            label,
                            Some(format_model_metadata(&model)),
                            format!("model:{}", model.id),
                        )
                    })
                    .collect();
                DialogView::selection(
                    "Choose model",
                    Some(format!("Source: {}", selector.source_label())),
                    entries,
                )
                .with_selected(selected)
                .with_identifier_query_action(
                    "Use ",
                    " (unverified metadata)",
                    "model-custom:",
                    64,
                )
            }
            "effort" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                let model = context
                    .selection
                    .as_ref()
                    .map(TuiModelSelector::model)
                    .or_else(|| bootstrap.model())
                    .unwrap_or_else(|| default_model(&bootstrap));
                let selector = context.selection.clone().unwrap_or_else(|| {
                    TuiModelSelector::for_source(model, tui_model_source(&bootstrap, &context))
                });
                let current = selector.reasoning_effort().unwrap_or("default");
                let values = selector.reasoning_effort_values();
                let help = selector.reasoning_effort_default().map_or_else(
                    || format!("Model: {model}"),
                    |effort| format!("Model: {model} · Default: {effort}"),
                );
                let selected = values
                    .iter()
                    .position(|effort| *effort == current)
                    .unwrap_or_default();
                let entries = values
                    .into_iter()
                    .map(|effort| {
                        let name = if effort == "default" {
                            "Default"
                        } else {
                            effort
                        };
                        let label = if effort == current {
                            format!("{name} (current)")
                        } else {
                            name.to_owned()
                        };
                        DialogEntry::action(label, format!("effort:{effort}"))
                    })
                    .collect();
                DialogView::selection("Choose effort", Some(help), entries).with_selected(selected)
            }
            "help" => DialogView::selection(
                "Commands and skills",
                Some(render_tui_help(&self.palette)),
                Vec::new(),
            ),
            "mcp" => mcp_status_dialog(self.mcp_status.snapshot()),
            "select" => {
                let entries = tui_select_candidates(&bootstrap)?
                    .into_iter()
                    .map(|path| DialogEntry::safe_action(&path, format!("select:{path}")))
                    .collect();
                return Ok(TuiSubmissionOutcome::SafeDialog(
                    DialogView::selection(
                        "Select project file",
                        Some("Choose one approved file"),
                        entries,
                    )
                    .with_empty_message("No approved project files are available.")
                    .with_cancellation_action("select:cancel"),
                ));
            }
            "sessions" => {
                return Ok(self.session_dialog_outcome(SessionDialogRequest::initial()));
            }
            "agent" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                let catalog = tui_agent_catalog_for_context(&bootstrap, &context)?;
                let current = context
                    .active_agent
                    .as_ref()
                    .map(|agent| agent.name.as_str())
                    .or_else(|| {
                        context
                            .metadata
                            .as_ref()
                            .map(|metadata| metadata.active_agent.as_str())
                    })
                    .unwrap_or("primary");
                let entries = catalog
                    .primary_or_all()
                    .map(|agent| {
                        let label = if agent.name == current {
                            format!("{} (current)", agent.name)
                        } else {
                            agent.name.clone()
                        };
                        DialogEntry::action(label, format!("agent:{}", agent.name))
                    })
                    .collect();
                DialogView::selection("Choose agent", Some("Eligible primary agents"), entries)
            }
            "subagent" => {
                let context = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?;
                let entries = tui_subagent_catalog(&bootstrap, &context)?
                    .map(|agent| {
                        DialogEntry::action(&agent.name, format!("subagent:{}", agent.name))
                    })
                    .collect();
                DialogView::selection("Choose subagent", Some("Eligible subagents"), entries)
                    .with_empty_message("No eligible subagents are available.")
            }
            _ => return Err(CliError::usage("TUI dialog is unavailable")),
        };
        if route_id == "subagent" {
            Ok(TuiSubmissionOutcome::SafeDialog(dialog))
        } else {
            Ok(TuiSubmissionOutcome::Dialog(dialog))
        }
    }

    fn session_dialog_outcome(&self, request: SessionDialogRequest) -> TuiSubmissionOutcome {
        let fallback_request = request.clone();
        match self.load_session_dialog(request) {
            Ok(dialog) => TuiSubmissionOutcome::Dialog(dialog),
            Err(_) => TuiSubmissionOutcome::Dialog(DialogView::sessions_error(
                fallback_request,
                "Saved sessions could not be loaded.",
            )),
        }
    }

    fn load_session_dialog(&self, request: SessionDialogRequest) -> Result<DialogView, CliError> {
        let bootstrap = self.bootstrap()?;
        let project = tui_project_identifier(&bootstrap)?;
        let project = match request.scope() {
            SessionDialogScope::CurrentProject => Some(project.as_str()),
            SessionDialogScope::AllProjects => None,
        };
        let cursor = request
            .cursor()
            .map(|cursor| SessionCursor::new(cursor.updated_at(), cursor.id()));
        let store = SessionStore::open(bootstrap.data_directory())
            .map_err(|_| CliError::storage("sessions database is unavailable"))?;
        let page = store
            .list_session_page(project, request.query(), cursor, 64)
            .map_err(|_| CliError::storage("saved sessions could not be listed"))?;
        let current_session = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?
            .identifier;
        let now = (self.clock)();
        let show_project = request.scope() == SessionDialogScope::AllProjects;
        let entries = page
            .sessions
            .iter()
            .map(|session| session_dialog_entry(session, current_session, show_project, now))
            .collect();
        let next_cursor = page
            .next_cursor
            .map(|cursor| SessionDialogCursor::new(cursor.updated_at(), cursor.id()));

        Ok(DialogView::sessions_page(entries, request, next_cursor))
    }

    #[cfg(test)]
    fn route_dialog_action(
        &self,
        action_id: &str,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) -> TuiSubmissionOutcome {
        self.route_dialog_action_with_cancellation(
            action_id,
            progress,
            &TuiRouteCancellation::new(),
        )
    }

    fn route_dialog_action_with_cancellation(
        &self,
        action_id: &str,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
        cancellation: &TuiRouteCancellation,
    ) -> TuiSubmissionOutcome {
        match action_id {
            "connect:browser" => {
                return auth_route_outcome(self.connect(ChatGptAuthFlow::Browser, progress));
            }
            "connect:device" => {
                return auth_route_outcome(self.connect(ChatGptAuthFlow::Device, progress));
            }
            "disconnect:confirm" => return auth_route_outcome(self.disconnect()),
            _ => {}
        }
        let result =
            (|| {
                let bootstrap = self.bootstrap()?;
                if action_id == "select:cancel" {
                    return Ok(TuiSubmissionOutcome::SelectionCancelled);
                }
                if let Some(path) = action_id.strip_prefix("select:") {
                    return selected_tui_file(&bootstrap, path).map(|path| {
                        TuiSubmissionOutcome::SelectionInfo(format!("Selected file: {path}"))
                    });
                }
                if let Some(key) = parse_recovery_action(action_id) {
                    return self.recover_tui_session_attempt(&bootstrap, key);
                }
                if let Some(identifier) = action_id.strip_prefix("session:") {
                    let expected = self
                        .session
                        .lock()
                        .map_err(|_| CliError::storage("TUI session is unavailable"))?
                        .clone();
                    let identifier = identifier
                        .parse()
                        .map_err(|_| CliError::usage("session action is invalid"))?;
                    let stored = load_tui_session_for_resume(&bootstrap, identifier)?;
                    if stored.metadata.project != tui_project_identifier(&bootstrap)? {
                        return Err(CliError::storage("saved session is unavailable"));
                    }
                    if let Some(attempt) = stored.latest_attempt.as_ref().filter(|attempt| {
                        attempt.status() == agens_core::SessionAttemptStatus::Running
                    }) {
                        return Ok(TuiSubmissionOutcome::Dialog(recovery_confirmation_dialog(
                            &stored.metadata,
                            attempt,
                            None,
                        )));
                    }
                    let resumed = prepare_loaded_tui_session_resume(
                        &bootstrap,
                        identifier,
                        stored,
                        &self.credentials,
                    )?;
                    return commit_tui_session_resume(
                        &bootstrap,
                        &self.session,
                        &expected,
                        resumed,
                        cancellation,
                    );
                }
                let message = if let Some(model) = action_id.strip_prefix("model:") {
                    apply_tui_model(&bootstrap, model, &self.session)?
                } else if let Some(model) = action_id.strip_prefix("model-custom:") {
                    apply_tui_unverified_model(&bootstrap, model, &self.session)?
                } else if let Some(provider) = action_id.strip_prefix("provider:") {
                    self.apply_provider(&bootstrap, provider)?
                } else if let Some(effort) = action_id.strip_prefix("effort:") {
                    apply_tui_effort(&bootstrap, effort, &self.session)?
                } else if let Some(agent) = action_id.strip_prefix("agent:") {
                    rotate_tui_agent(&bootstrap, agent, &self.session, &self.skills)?
                } else if let Some(agent) = action_id.strip_prefix("subagent:") {
                    select_tui_subagent(&bootstrap, agent, &self.session)?
                } else {
                    return Err(CliError::usage("TUI dialog action is unavailable"));
                };
                Ok(TuiSubmissionOutcome::ContextChanged {
                    message,
                    presentation: self.presentation()?,
                })
            })();
        match result {
            Ok(outcome) => outcome,
            Err(error) if action_id.starts_with("select:") => {
                TuiSubmissionOutcome::SelectionError {
                    message: error.to_string(),
                    action: TUI_ERROR_ACTION.into(),
                }
            }
            Err(error) => TuiSubmissionOutcome::LocalActionableError {
                message: error.to_string(),
                action: TUI_ERROR_ACTION.into(),
            },
        }
    }

    fn recover_tui_session_attempt(
        &self,
        bootstrap: &Bootstrap,
        key: AttemptKey,
    ) -> Result<TuiSubmissionOutcome, CliError> {
        let mut store = SessionStore::open(bootstrap.data_directory())
            .map_err(|_| CliError::storage("sessions database is unavailable"))?;
        let stored = store
            .load_session_for_resume(key.session_id())
            .map_err(|_| CliError::storage("saved session is unavailable"))?;
        if stored.metadata.project != tui_project_identifier(bootstrap)? {
            return Err(CliError::storage("saved session is unavailable"));
        }
        let Some(attempt) = stored.latest_attempt.as_ref().filter(|attempt| {
            attempt.key() == key && attempt.status() == agens_core::SessionAttemptStatus::Running
        }) else {
            return self.open_dialog("sessions");
        };

        let recovery = active_session_attempts()
            .recover_running_attempt(&mut store, key, current_session_timestamp())
            .map_err(|_| CliError::storage("attempt recovery failed"))?;
        let Some(recovery) = recovery else {
            return Ok(TuiSubmissionOutcome::Dialog(recovery_confirmation_dialog(
                &stored.metadata,
                attempt,
                Some("Recovery was refused because this attempt is active in this process."),
            )));
        };
        if recovery == RecoveryOutcome::Stale {
            return self.open_dialog("sessions");
        }

        let boundary = store
            .load_retry_boundary(key)
            .map_err(|_| CliError::storage("attempt recovery failed"))?
            .ok_or_else(|| CliError::storage("attempt recovery failed"))?;
        drop(store);

        let mut resumed =
            resume_tui_session(bootstrap, key.session_id(), &self.skills, &self.credentials)?;
        persist_pending_agent_correction(bootstrap, &mut resumed);
        let prompt = boundary.prompt().to_owned();
        *self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))? = resumed;

        Ok(TuiSubmissionOutcome::ProviderTurn {
            display: "Retrying recovered attempt.".into(),
            prompt,
        })
    }

    fn palette_entries(&self) -> &[PaletteEntry] {
        &self.palette
    }

    #[cfg(test)]
    fn resolve(&self, input: String) -> Result<TuiSubmissionOutcome, CliError> {
        self.resolve_with_cancellation(input, &TuiRouteCancellation::new())
    }

    fn resolve_with_cancellation(
        &self,
        input: String,
        cancellation: &TuiRouteCancellation,
    ) -> Result<TuiSubmissionOutcome, CliError> {
        if !input.starts_with('/') {
            return Ok(TuiSubmissionOutcome::ProviderTurn {
                display: input.clone(),
                prompt: input,
            });
        }

        let command = input.trim();
        let invocation = command
            .strip_prefix('/')
            .expect("slash command input was checked");
        let name_end = invocation
            .find(char::is_whitespace)
            .unwrap_or(invocation.len());
        let (name, arguments) = invocation.split_at(name_end);
        let arguments = arguments.trim();
        let bootstrap = self.bootstrap()?;
        let outcome = match command {
            "/dangerous" => return self.toggle_dangerous_mode(),
            "/help" => self.open_dialog("help")?,
            "/mcp" => self.open_dialog("mcp")?,
            "/select" => self.open_dialog("select")?,
            "/quit" => TuiSubmissionOutcome::Quit,
            "/sessions" | "/resume" => self.open_dialog("sessions")?,
            "/connect" => self.open_dialog("connect")?,
            "/disconnect" => self.open_dialog("disconnect")?,
            "/diagnostics" => self.open_dialog("diagnostics")?,
            "/provider" => self.open_dialog("provider")?,
            command if command.starts_with("/provider ") => TuiSubmissionOutcome::ContextChanged {
                message: self.apply_provider(&bootstrap, &command[10..])?,
                presentation: self.presentation()?,
            },
            "/new" => {
                let mut session = self.session.lock().map_err(|_| {
                    CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
                })?;
                reset_tui_session(&mut session)
                    .map_err(|_| CliError::runtime(HeadlessTurnError::State))?;
                drop(session);
                TuiSubmissionOutcome::ResetSucceeded {
                    message: "Started a new session.".into(),
                    presentation: self.presentation()?,
                }
            }
            command if command.starts_with("/resume ") => {
                let expected = self
                    .session
                    .lock()
                    .map_err(|_| CliError::storage("TUI session is unavailable"))?
                    .clone();
                if expected.running {
                    return Err(CliError::runtime(HeadlessTurnError::State));
                }
                let identifier = command[8..]
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| CliError::usage("/resume requires a numeric session id"))?;
                let resumed =
                    resume_tui_session(&bootstrap, identifier, &self.skills, &self.credentials)?;
                commit_tui_session_resume(
                    &bootstrap,
                    &self.session,
                    &expected,
                    resumed,
                    cancellation,
                )?
            }
            command if command.starts_with("/agent ") => TuiSubmissionOutcome::ContextChanged {
                message: rotate_tui_agent(&bootstrap, &command[7..], &self.session, &self.skills)?,
                presentation: self.presentation()?,
            },
            "/agent" => self.open_dialog("agent")?,
            command if command.starts_with("/subagent ") => TuiSubmissionOutcome::ContextChanged {
                message: select_tui_subagent(&bootstrap, &command[10..], &self.session)?,
                presentation: self.presentation()?,
            },
            "/subagent" => self.open_dialog("subagent")?,
            "/subagents" => TuiSubmissionOutcome::TranscriptDialog,
            "/model" => self.open_dialog("model")?,
            command if command.starts_with("/model ") => TuiSubmissionOutcome::ContextChanged {
                message: select_tui_model(&bootstrap, command, &self.session)?,
                presentation: self.presentation()?,
            },
            "/effort" => self.open_dialog("effort")?,
            command if command.starts_with("/effort ") => TuiSubmissionOutcome::ContextChanged {
                message: select_tui_effort(&bootstrap, command, &self.session)?,
                presentation: self.presentation()?,
            },
            _ if RESERVED_TUI_COMMANDS.contains(&name) => {
                return Err(CliError::usage(format!("unknown TUI command: {command}")));
            }
            _ => match self.commands.command(name) {
                Some(command) => TuiSubmissionOutcome::ProviderTurn {
                    display: input.clone(),
                    prompt: command.expand(arguments),
                },
                None => match self.skills.skill(name) {
                    Some(skill) => TuiSubmissionOutcome::ProviderTurn {
                        display: input.clone(),
                        prompt: format!(
                            "## Skill: {}\n{}\n\n## User arguments\n{}",
                            skill.name(),
                            skill.load_instructions().map_err(|_| {
                                CliError::usage(format!("skill /{name} is unavailable"))
                            })?,
                            arguments
                        ),
                    },
                    None => {
                        return Err(CliError::usage(format!("unknown TUI command: {command}")));
                    }
                },
            },
        };
        Ok(outcome)
    }

    fn presentation(&self) -> Result<TuiPresentation, CliError> {
        let bootstrap = self.bootstrap()?;
        let session = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        Ok(tui_session_presentation(&bootstrap, &session))
    }

    fn toggle_dangerous_mode(&self) -> Result<TuiSubmissionOutcome, CliError> {
        let enabled = {
            let mut session = self
                .session
                .lock()
                .map_err(|_| CliError::storage("TUI session is unavailable"))?;
            session.dangerous_mode = !session.dangerous_mode;
            session.dangerous_mode
        };

        Ok(TuiSubmissionOutcome::ContextChanged {
            message: format!("Dangerous mode: {}.", if enabled { "on" } else { "off" }),
            presentation: self.presentation()?,
        })
    }

    fn bootstrap(&self) -> Result<Bootstrap, CliError> {
        self.bootstrap
            .lock()
            .map(|bootstrap| bootstrap.clone())
            .map_err(|_| CliError::storage("TUI provider state is unavailable"))
    }

    fn turn_bootstrap(&self) -> Result<Bootstrap, CliError> {
        let mut bootstrap = self.bootstrap()?;
        let context = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        if context.chatgpt_unavailable {
            return Err(CliError::authentication(
                "ChatGPT credentials are unavailable; run /connect",
            ));
        }
        let provider = current_tui_provider(&bootstrap, &context)
            .ok_or_else(|| CliError::configuration("TUI provider is unavailable"))?;
        if let Some(selection) = &context.selection {
            bootstrap.model = Some(selection.model().to_owned());
        }
        drop(context);

        bootstrap.provider_type = Some(provider.identifier().into());
        bootstrap.openai_api_key = match provider {
            TuiProvider::OpenAiApi => Some(
                self.credentials
                    .api_key(&bootstrap.paths.credentials)
                    .ok_or_else(|| {
                        CliError::authentication("OpenAI API authentication is unavailable")
                    })?,
            ),
            TuiProvider::OpenAiChatGpt => {
                if !self
                    .credentials
                    .status(&bootstrap.paths.credentials, provider)
                    .available()
                {
                    return Err(CliError::authentication(
                        "ChatGPT credentials are unavailable or invalid; run /connect",
                    ));
                }
                None
            }
        };
        Ok(bootstrap)
    }

    fn task_parent_request_config(&self) -> Result<agens_core::RequestConfig, CliError> {
        self.session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))
            .map(|context| {
                context
                    .selection
                    .as_ref()
                    .map(|selection| selection.request_config().clone())
                    .unwrap_or_default()
            })
    }

    fn connect(
        &self,
        flow: ChatGptAuthFlow,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) -> Result<String, AuthRouteError> {
        let path = self
            .bootstrap()
            .map_err(AuthRouteError::Runtime)?
            .paths
            .credentials;
        let credentials_before =
            snapshot_chatgpt_credentials(&path).map_err(AuthRouteError::Runtime)?;
        let runtime_before = self
            .session
            .lock()
            .map_err(|_| AuthRouteError::Runtime(CliError::storage("TUI session is unavailable")))?
            .clone();
        let operation =
            HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(600));
        *self.cancellation.lock().map_err(|_| {
            AuthRouteError::Runtime(CliError::storage("TUI cancellation is unavailable"))
        })? = Some(operation.clone());
        let view = operation.adapter_view();
        let result = self.auth.login(
            &path,
            flow,
            LoginCancellation::from_shared_flag(view.cancellation_handle()),
            view.deadline()
                .expect("authentication has a fixed deadline"),
            move |event| {
                let event = match event {
                    ChatGptAuthProgress::BrowserUrl(url) => TuiRouteProgress::BrowserUrl(url),
                    ChatGptAuthProgress::DeviceCode {
                        verification_url,
                        user_code,
                    } => TuiRouteProgress::DeviceCode {
                        verification_url,
                        user_code,
                    },
                };
                let _ = progress.send(event);
            },
        );
        if let Ok(mut active) = self.cancellation.lock() {
            *active = None;
        }
        result.map_err(AuthRouteError::Auth)?;
        if let Err(error) = self.reconcile_provider(true) {
            if (self.credential_restorer)(&path, credentials_before).is_err() {
                self.mark_chatgpt_unavailable()
                    .map_err(AuthRouteError::Runtime)?;
                return Err(AuthRouteError::Runtime(CliError::storage(
                    "ChatGPT credential recovery failed",
                )));
            }
            *self.session.lock().map_err(|_| {
                AuthRouteError::Runtime(CliError::storage("TUI session is unavailable"))
            })? = runtime_before;
            return Err(AuthRouteError::Runtime(error));
        }
        Ok("Connected to ChatGPT.".into())
    }

    fn disconnect(&self) -> Result<String, AuthRouteError> {
        let path = self
            .bootstrap()
            .map_err(AuthRouteError::Runtime)?
            .paths
            .credentials;
        let removed = self.auth.disconnect(&path).map_err(AuthRouteError::Auth)?;
        if removed {
            if let Err(error) = self.reconcile_provider(false) {
                self.mark_chatgpt_unavailable()
                    .map_err(AuthRouteError::Runtime)?;
                return Err(AuthRouteError::Runtime(error));
            }
            Ok("Disconnected from ChatGPT.".into())
        } else {
            Ok("No ChatGPT credentials were stored.".into())
        }
    }

    fn reconcile_provider(&self, connected: bool) -> Result<(), CliError> {
        let bootstrap = self.bootstrap()?;
        match bootstrap.provider_source {
            ProviderSource::Auto => {
                let provider = if connected {
                    "openai-chatgpt".to_owned()
                } else {
                    let credentials = fs::read_to_string(&bootstrap.paths.credentials).ok();
                    resolve_provider_type(
                        None,
                        credentials.as_deref(),
                        &(self.credentials.environment)(),
                    )
                    .ok_or_else(|| {
                        CliError::authentication(
                            "ChatGPT credentials are unavailable; run /connect",
                        )
                    })?
                };
                self.apply_provider(&bootstrap, &provider)?;
            }
            ProviderSource::ExplicitChatGpt if connected => {
                self.apply_provider(&bootstrap, "openai-chatgpt")?;
            }
            ProviderSource::ExplicitChatGpt => self.mark_chatgpt_unavailable()?,
            ProviderSource::ExplicitOther => {}
        }
        Ok(())
    }

    fn mark_chatgpt_unavailable(&self) -> Result<(), CliError> {
        let mut context = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        context.provider = None;
        context.chatgpt_unavailable = true;
        context.active_agent = None;
        Ok(())
    }

    fn apply_provider(&self, bootstrap: &Bootstrap, provider: &str) -> Result<String, CliError> {
        let provider = TuiProvider::parse(provider)
            .ok_or_else(|| CliError::usage("provider is not implemented"))?;
        let mut context = self
            .session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        if context.running {
            return Err(CliError::runtime(HeadlessTurnError::State));
        }
        let status = self
            .credentials
            .status(&bootstrap.paths.credentials, provider);
        if !status.available() {
            let message = if provider == TuiProvider::OpenAiChatGpt {
                "ChatGPT subscription requires connection; run /connect"
            } else {
                "OpenAI API credentials are unavailable"
            };
            return Err(CliError::authentication(message));
        }

        let current_model = effective_tui_model(bootstrap, &context);
        let previous_effort = context
            .selection
            .as_ref()
            .and_then(TuiModelSelector::reasoning_effort);
        let mut next = TuiModelSelector::for_source(&current_model, provider.source());
        let compatible = next
            .model_values()
            .map_err(CliError::unavailable)?
            .iter()
            .any(|model| model == &current_model);
        let label = provider.label();
        let message = if compatible {
            let reset_effort =
                previous_effort.is_some_and(|effort| next.apply_reasoning_effort(effort).is_err());
            if reset_effort {
                format!(
                    "Provider: {label}. Model retained: {current_model}. Reasoning effort reset to Default."
                )
            } else {
                format!("Provider: {label}. Model retained: {current_model}.")
            }
        } else {
            let previous = current_model.clone();
            let default = ["gpt-4.1", "gpt-5.5"][provider as usize];
            next = TuiModelSelector::for_source(default, provider.source());
            format!(
                "Provider: {label}. Model reset to {default} and reasoning effort reset to Default because {previous} is unavailable."
            )
        };
        apply_tui_selection(bootstrap, &mut context, provider, next)?;
        context.chatgpt_unavailable = false;
        context.resume_error = None;
        Ok(message)
    }
}

enum AuthRouteError {
    Auth(chatgpt_auth::ChatGptAuthError),
    Runtime(CliError),
}

fn auth_route_outcome(result: Result<String, AuthRouteError>) -> TuiSubmissionOutcome {
    match result {
        Ok(message) => TuiSubmissionOutcome::LocalInfo(message),
        Err(AuthRouteError::Auth(error)) => TuiSubmissionOutcome::LocalActionableError {
            message: error.message().into(),
            action: error.action().into(),
        },
        Err(AuthRouteError::Runtime(error)) => TuiSubmissionOutcome::LocalActionableError {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
        },
    }
}

fn tui_provider_outcome(result: Result<String, CliError>) -> TuiProviderOutcome {
    match result {
        Ok(output) => TuiProviderOutcome::Completed(output),
        Err(error) if error.category == "cancelled" => TuiProviderOutcome::Cancelled {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
        },
        Err(error) if error.message == "request exceeds the model context window" => {
            TuiProviderOutcome::Failed {
                message: error.to_string(),
                action: "Start a new session or shorten the prompt, then retry.".into(),
            }
        }
        Err(error) if error.message == "network request failed" => TuiProviderOutcome::Failed {
            message: error.to_string(),
            action: "Check the network connection, then retry.".into(),
        },
        Err(error) => TuiProviderOutcome::Failed {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
        },
    }
}

fn start_tui_commands<E: TuiEngine>(
    tui: &mut Tui<E>,
    bootstrap: &Bootstrap,
) -> Result<Arc<CommandCatalog>, CliError> {
    let global_root = bootstrap
        .paths
        .global_config
        .parent()
        .ok_or_else(|| CliError::configuration("global command root is unavailable"))?
        .join("commands");
    let project_root = bootstrap
        .paths
        .project_config
        .parent()
        .ok_or_else(|| CliError::configuration("project command root is unavailable"))?
        .join("commands");
    let built_ins = RESERVED_TUI_COMMANDS
        .iter()
        .map(|name| {
            CommandDefinition::new(*name, "Reserved TUI command", *name)
                .expect("reserved TUI command names are valid")
        })
        .collect::<Vec<_>>();
    let discovery = CommandCatalog::discover(&built_ins, global_root, project_root)
        .map_err(CliError::configuration)?;

    for diagnostic in discovery.diagnostics() {
        tui.add_diagnostic(format!(
            "Command diagnostic ({}): {}",
            diagnostic.path().display(),
            diagnostic.message()
        ));
    }
    for name in discovery.shadowed() {
        tui.add_diagnostic(format!(
            "Command /{name} has multiple definitions; applied source precedence."
        ));
    }

    Ok(Arc::new(discovery.catalog().clone()))
}

fn start_tui_skills<E: TuiEngine>(
    tui: &mut Tui<E>,
    bootstrap: &Bootstrap,
) -> Result<Arc<SkillCatalog>, CliError> {
    let discovery = discover_skill_catalog(bootstrap)?;
    for diagnostic in discovery.diagnostics() {
        tui.add_diagnostic(format!(
            "Skill diagnostic ({}): {}",
            diagnostic.path().display(),
            diagnostic.message()
        ));
    }
    for shadow in discovery.shadowed() {
        tui.add_diagnostic(format!(
            "Skill /{} has multiple definitions; applied source precedence.",
            shadow.name()
        ));
    }

    Ok(Arc::new(discovery.catalog().clone()))
}

fn discover_skill_catalog(bootstrap: &Bootstrap) -> Result<agens_tools::SkillDiscovery, CliError> {
    SkillCatalog::discover(
        bootstrap.paths.global_config.with_file_name("skills"),
        bootstrap.paths.project_config.with_file_name("skills"),
    )
    .map_err(|_| CliError::configuration("skill catalog is unavailable"))
}

fn parent_skill_system_prompt(base: &str, skills: &SkillCatalog) -> String {
    if skills.is_empty() {
        return base.to_owned();
    }

    let metadata = skills
        .skills()
        .map(|skill| format!("- {}: {}", skill.name(), skill.description()))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{base}\n\n## Available skills\nUse the `skill` tool to load instructions or declared resources only when needed.\n{metadata}"
    )
}

fn explicit_task_delegation_prompt(base: &str) -> String {
    const INSTRUCTION: &str = "When the user explicitly asks for subagent delegation, use the `task` tool instead of completing the delegated work inline. Use `task_control` to inspect, background, or cancel a live execution and `task_message` to send bounded coordination without waiting for completion.";

    if base.contains(INSTRUCTION) {
        base.to_owned()
    } else {
        format!("{base}\n\n{INSTRUCTION}")
    }
}

fn report_tui_extension_collisions<E: TuiEngine>(
    tui: &mut Tui<E>,
    commands: &CommandCatalog,
    skills: &SkillCatalog,
) {
    for skill in skills
        .skills()
        .filter(|skill| commands.command(skill.name()).is_some())
    {
        tui.add_diagnostic(format!(
            "Skill /{} is shadowed by a command; command routing wins.",
            skill.name()
        ));
    }
}

fn resolved_tui_palette(
    commands: &CommandCatalog,
    skills: &SkillCatalog,
    has_subagents: bool,
) -> Vec<PaletteEntry> {
    let mut entries = TUI_PALETTE_BUILT_INS
        .iter()
        .map(|(name, description, hint, dialog_id)| {
            let entry = PaletteEntry::new(*name, *description, *hint, PaletteEntryKind::BuiltIn);
            let dialog_id = dialog_id.or(match *name {
                "connect" | "disconnect" | "agent" => Some(*name),
                "sessions" | "resume" => Some("sessions"),
                _ => None,
            });
            dialog_id.map_or(entry.clone(), |route| entry.with_dialog(route))
        })
        .collect::<Vec<_>>();
    if has_subagents {
        entries.push(
            PaletteEntry::new(
                "subagent",
                "Choose an eligible configured subagent",
                "[name]",
                PaletteEntryKind::BuiltIn,
            )
            .with_dialog("subagent"),
        );
        entries.push(PaletteEntry::new(
            "subagents",
            "Inspect current-session subagent transcripts",
            "",
            PaletteEntryKind::BuiltIn,
        ));
    }
    let mut custom_commands = commands
        .iter()
        .filter(|command| !RESERVED_TUI_COMMANDS.contains(&command.name()))
        .collect::<Vec<_>>();
    custom_commands.sort_by_key(|command| command.name());
    entries.extend(custom_commands.into_iter().map(|command| {
        PaletteEntry::new(
            command.name(),
            command.description(),
            "[arguments]",
            PaletteEntryKind::Command,
        )
    }));
    let mut resolved_skills = skills
        .skills()
        .filter(|skill| {
            !RESERVED_TUI_COMMANDS.contains(&skill.name())
                && commands.command(skill.name()).is_none()
        })
        .collect::<Vec<_>>();
    resolved_skills.sort_by_key(|skill| skill.name());
    entries.extend(resolved_skills.into_iter().map(|skill| {
        PaletteEntry::new(
            skill.name(),
            skill.description(),
            "[arguments]",
            PaletteEntryKind::Skill,
        )
    }));
    entries
}

fn render_tui_help(entries: &[PaletteEntry]) -> String {
    let surface = entries
        .iter()
        .map(|entry| {
            format!(
                "/{} {}  [{}] {}",
                entry.name(),
                entry.argument_hint(),
                entry.kind().label(),
                entry.description()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("Available commands and skills:\n{surface}")
}

fn mcp_status_dialog(snapshot: McpStatusSnapshot) -> DialogView {
    let entries = snapshot
        .servers()
        .iter()
        .map(|server| {
            let descriptor = server.descriptor();
            let transport = format!("{:?}", descriptor.transport()).to_lowercase();
            let state = format!("{:?}", server.state()).to_lowercase();
            let enabled = if descriptor.enabled() { "enabled" } else { "disabled" };
            let source = format!("{:?}", descriptor.source()).to_lowercase();
            let tools = server.tool_names().join(", ");
            let endpoint = descriptor.endpoint().map_or("not configured", McpEndpointSummary::as_str);
            let error = server.last_error().map_or_else(
                || "none".into(),
                |error| format!("{}: {}", format!("{:?}", error.category()).to_lowercase(), error.message()),
            );
            DialogEntry::read_only(
                format!("{}  {transport}  {enabled}/{state}  {} tools", descriptor.name(), server.tool_count()),
                format!("{} {transport} {state} {tools}", descriptor.name()),
                format!(
                    "Source: {source}\nEndpoint: {endpoint}\nTimeout: {}ms\nTools: {}\nLast error: {error}",
                    descriptor.timeout().as_millis(),
                    if tools.is_empty() { "none" } else { &tools },
                ),
            )
        })
        .collect();
    DialogView::read_only("MCP servers", None::<&str>, entries, "mcp")
        .with_empty_message("No MCP servers configured.")
}

fn diagnostics_dialog(data_directory: &Path) -> DialogView {
    let directory = data_directory.join("diagnostics");
    let safe_directory =
        fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.file_type().is_dir());
    let mut files = match safe_directory.then(|| fs::read_dir(&directory)) {
        Some(Ok(entries)) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                is_diagnostic_file_name(&name).then_some((name, entry.path()))
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut entries = Vec::new();
    for (name, path) in files {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() || metadata.len() > DIAGNOSTIC_FILE_LIMIT_BYTES {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let relative_path = format!("diagnostics/{name}");
        entries.extend(content.lines().filter_map(|line| {
            let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
            safe_diagnostic_entry(&value, &relative_path)
        }));
    }

    DialogView::read_only(
        "Runtime diagnostics",
        Some("Sanitized local events"),
        entries,
        "diagnostics",
    )
    .with_empty_message("No runtime diagnostics are available.")
}

fn is_diagnostic_file_name(name: &str) -> bool {
    let Some(identifier) = name
        .strip_prefix("agens-")
        .and_then(|name| name.strip_suffix(".jsonl"))
    else {
        return false;
    };
    let mut parts = identifier.split('.');
    let Some(process) = parts.next() else {
        return false;
    };
    if process.is_empty() || !process.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match (parts.next(), parts.next()) {
        (None, None) => true,
        (Some(generation), None) => matches!(generation, "1" | "2" | "3"),
        _ => false,
    }
}

fn safe_diagnostic_entry(value: &serde_json::Value, relative_path: &str) -> Option<DialogEntry> {
    let object = value.as_object()?;
    let timestamp = object.get("timestamp_ms")?.as_u64()?;
    let reference = object.get("reference")?.as_str()?;
    DiagnosticRef::new(reference.to_owned()).ok()?;
    let scope =
        allowlisted_diagnostic_value(object.get("scope")?.as_str()?, &["parent", "subagent"])?;
    let component = allowlisted_diagnostic_value(
        object.get("component")?.as_str()?,
        &["responses", "oauth_refresh", "subagent", "agent"],
    )?;
    let event = allowlisted_diagnostic_value(
        object.get("event")?.as_str()?,
        &[
            "attempt",
            "retry_scheduled",
            "terminal",
            "agent_unavailable",
            "agent_fallback",
        ],
    )?;
    let attempt = object
        .get("attempt")?
        .as_u64()
        .filter(|attempt| *attempt <= 3)?;
    let max_attempts = object
        .get("max_attempts")?
        .as_u64()
        .filter(|attempts| *attempts <= 3)?;
    let delay = optional_bounded_u64(object.get("delay_ms"), 5_000)?;
    let status = optional_bounded_u64(object.get("status"), 599)?;
    let class = match object.get("class") {
        Some(serde_json::Value::String(class)) => Some(allowlisted_diagnostic_value(
            class,
            &[
                "authentication",
                "cancelled",
                "context",
                "deadline",
                "model_unavailable",
                "network",
                "provider",
                "protocol",
                "rate_limited",
                "rejected",
                "runtime",
                "server",
                "tool",
            ],
        )?),
        Some(serde_json::Value::Null) | None => None,
        Some(_) => return None,
    };
    let class_label = class.unwrap_or("success");
    let status_label = status.map_or_else(|| "none".into(), |status| status.to_string());
    let delay_label = delay.map_or_else(|| "none".into(), |delay| format!("{delay}ms"));
    let label = format!("[ref: {reference}] {scope} · {component} · {event} · {class_label}");
    let detail = format!(
        "Source: {relative_path}\nTimestamp: {timestamp}\nAttempt: {attempt}/{max_attempts}\nHTTP status: {status_label}\nRetry delay: {delay_label}"
    );
    Some(DialogEntry::read_only(
        label.clone(),
        format!("{reference} {scope} {component} {event} {class_label}"),
        detail,
    ))
}

fn allowlisted_diagnostic_value<'a>(value: &'a str, allowed: &[&str]) -> Option<&'a str> {
    allowed.contains(&value).then_some(value)
}

fn optional_bounded_u64(value: Option<&serde_json::Value>, maximum: u64) -> Option<Option<u64>> {
    match value {
        Some(serde_json::Value::Number(number)) => {
            Some(Some(number.as_u64().filter(|value| *value <= maximum)?))
        }
        Some(serde_json::Value::Null) | None => Some(None),
        Some(_) => None,
    }
}

fn configure_tui_project_identity(tui: &mut Tui<ProductionTuiEngine>, bootstrap: &Bootstrap) {
    if let Some(project_root) = bootstrap.project_root() {
        tui.set_project(project_root.display().to_string());
    }
}

fn run_production_tui(bootstrap: &Bootstrap, resume: Option<i64>) -> Result<String, CliError> {
    let cancellation = Arc::new(Mutex::new(None));
    let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
    let task_controls = TuiTaskControls::default();
    let engine = ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    };
    let mut tui = Tui::new(engine);
    configure_tui_project_identity(&mut tui, bootstrap);
    tui.set_collapse_thinking(bootstrap.collapse_thinking);
    let skills = start_tui_skills(&mut tui, bootstrap)?;
    if let Some(identifier) = resume {
        let mut resumed = resume_tui_session(
            bootstrap,
            identifier,
            &skills,
            &TuiCredentialResolver::production(),
        )?;
        persist_pending_agent_correction(bootstrap, &mut resumed);
        let presentation = tui_session_presentation(bootstrap, &resumed);
        let message = resumed.note();
        let draft = resumed.resume_draft.take().map(ResumeDraft::into_inner);
        let resume_error = resumed.resume_error.clone();
        resumed.resume_notice = None;
        tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
            message,
            presentation,
            history: std::mem::take(&mut resumed.restored_history),
            draft,
            resume_error,
        });
        for event in resumed_subagent_cards(&resumed.messages) {
            tui.apply_runtime_event(event);
        }
        *session.lock().map_err(|_| {
            CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
        })? = resumed;
    } else {
        let mut context = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        let notice = seed_remembered_tui_selection(bootstrap, &mut context);
        tui.apply_presentation(tui_session_presentation(bootstrap, &context));
        drop(context);
        if let Some(notice) = notice {
            tui.add_info(notice);
        }
    }

    let commands = start_tui_commands(&mut tui, bootstrap)?;
    report_tui_extension_collisions(&mut tui, &commands, &skills);
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        session,
        Arc::clone(&cancellation),
        commands,
        Arc::clone(&skills),
    );
    tui.set_palette_entries(router.palette_entries().to_vec());
    let route_router = router.clone();
    let (permission_bridge, permission_requests) = production_tui_permission_bridge();
    let transition_controls = task_controls.clone();
    let cancel_controls = task_controls.clone();
    let message_controls = task_controls.clone();
    let submit_task_controls = task_controls.clone();
    let prompt_bridge = permission_bridge.clone();
    let tui_result = run_with_default_progress_submit_with_permissions_and_task_controls(
        &mut tui,
        move |request, progress, cancellation| {
            route_router.route_request_with_cancellation(request, progress, cancellation)
        },
        move |prompt, origin, progress, metrics| {
            let task_events = metrics.clone();
            let turn_cancellation =
                HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(120));
            let Ok(mut active) = cancellation.lock() else {
                return tui_provider_outcome(Err(CliError::new(
                    ExitStatus::Failure,
                    "ui",
                    "TUI cancellation is unavailable",
                )));
            };
            *active = Some(turn_cancellation.clone());
            drop(active);

            let model_id = router
                .presentation()
                .map(|presentation| presentation.model().to_owned())
                .unwrap_or_default();
            let metrics = Arc::new(Mutex::new(TuiMetricsPublisher::new(
                metrics,
                BridgeCancel::new(),
                model_id,
            )));
            let metrics_progress = Arc::clone(&metrics);
            let sink: TurnProgressSink = Arc::new(move |event| {
                if let Ok(mut metrics) = metrics_progress.lock() {
                    metrics.observe(&event);
                }
                let _ = progress.send(event);
            });
            let runtime_bootstrap = match router.turn_bootstrap() {
                Ok(bootstrap) => bootstrap,
                Err(error) => return tui_provider_outcome(Err(error)),
            };
            let task_parent_request_config = match router.task_parent_request_config() {
                Ok(config) => config,
                Err(error) => return tui_provider_outcome(Err(error)),
            };
            let task_diagnostic_reference = next_diagnostic_reference();
            let lifecycle_bridge =
                TuiTaskLifecycleBridge::new(task_events, submit_task_controls.clone())
                    .with_session_writer(runtime_bootstrap.clone(), Arc::clone(&router.session));
            let mut task_runtime = match production_tui_task_runtime(
                &runtime_bootstrap,
                &router.skills,
                prompt_bridge.clone(),
                lifecycle_bridge.clone(),
                task_parent_request_config.clone(),
                task_diagnostic_reference.clone(),
            ) {
                Ok(runtime) => runtime,
                Err(error) => return tui_provider_outcome(Err(error)),
            };
            if let Err(error) = ensure_active_tui_agent_runtime(
                &runtime_bootstrap,
                &router.session,
                &task_runtime.dispatcher,
            ) {
                return tui_provider_outcome(Err(error));
            }
            let selected_launch = if origin_launches_selected_subagent(origin) {
                selected_tui_task_skips_parent(
                    launch_selected_tui_task(
                        &mut task_runtime,
                        &router.session,
                        &prompt,
                        matches!(origin, TuiSubmitOrigin::Background),
                        &turn_cancellation,
                    ),
                    &lifecycle_bridge,
                )
            } else {
                Ok(false)
            };
            match selected_launch {
                Ok(true) => return TuiProviderOutcome::Backgrounded,
                Ok(false) => {}
                Err(error) => return tui_provider_outcome(Err(error)),
            }
            let result = run_tui_prompt_with(
                &runtime_bootstrap,
                &prompt,
                &router.session,
                Some(Arc::clone(&router.skills)),
                |request| {
                    let project_root = runtime_bootstrap.project_root().ok_or_else(|| {
                        CliError::configuration("native tools require a project root")
                    })?;
                    let task_runtime = production_tui_task_runtime_with_runner_and_parent_config(
                        &runtime_bootstrap,
                        &router.skills,
                        prompt_bridge.clone(),
                        ProductionTaskRunner::new(
                            runtime_bootstrap.clone(),
                            project_root.to_path_buf(),
                        )
                        .with_lifecycle_bridge(lifecycle_bridge.clone())
                        .with_dangerous_mode(request.dangerous_mode),
                        task_parent_request_config.clone(),
                        Some(task_diagnostic_reference.clone()),
                    )?;
                    run_production_headless_chat_with_progress(
                        request,
                        &runtime_bootstrap,
                        &turn_cancellation,
                        Some(&sink),
                        Some(prompt_bridge.clone()),
                        Some(&task_runtime),
                        Some(&task_diagnostic_reference),
                    )
                },
            );

            finish_tui_metrics(&metrics, &result);

            if let Ok(mut active) = cancellation.lock() {
                *active = None;
            }

            tui_provider_outcome(result)
        },
        move |id| transition_controls.transition_to_background(id),
        move |id| {
            cancel_controls
                .0
                .cancel(agens_tools::TaskExecutionId::from_value(id))
        },
        move |id, message| {
            message_controls
                .0
                .send_message(
                    TaskMessageSource::User,
                    TaskMessageTarget::Execution(agens_tools::TaskExecutionId::from_value(id)),
                    message,
                )
                .is_ok()
        },
        Some((permission_bridge, permission_requests)),
    );
    task_controls.0.cancel_all();
    let _ = task_controls
        .0
        .wait_for_idle(std::time::Duration::from_secs(2));
    tui_result.map_err(|_| CliError::new(ExitStatus::Failure, "ui", "terminal UI failed"))?;

    Ok(String::new())
}

#[cfg(test)]
fn run_tui_prompt(
    bootstrap: &Bootstrap,
    prompt: &str,
    cancellation: &HeadlessTurnCancellation,
    session: &Arc<Mutex<TuiSessionContext>>,
    progress: Option<&TurnProgressSink>,
) -> Result<String, CliError> {
    match prompt.trim() {
        command if command.starts_with('/') => {
            let router = TuiRuntimeRouter::new(
                bootstrap.clone(),
                Arc::clone(session),
                Arc::new(Mutex::new(None)),
                Arc::new(CommandCatalog::default()),
                Arc::new(SkillCatalog::default()),
            );
            match router.resolve(command.to_owned())? {
                TuiSubmissionOutcome::LocalInfo(message)
                | TuiSubmissionOutcome::SelectionInfo(message)
                | TuiSubmissionOutcome::ResetSucceeded { message, .. }
                | TuiSubmissionOutcome::ContextChanged { message, .. }
                | TuiSubmissionOutcome::SessionResumed { message, .. } => Ok(message),
                TuiSubmissionOutcome::ProviderTurn { .. }
                | TuiSubmissionOutcome::LocalActionableError { .. }
                | TuiSubmissionOutcome::Dialog(_)
                | TuiSubmissionOutcome::SafeDialog(_)
                | TuiSubmissionOutcome::TranscriptDialog
                | TuiSubmissionOutcome::SelectionCancelled
                | TuiSubmissionOutcome::RouteCancelled
                | TuiSubmissionOutcome::SelectionError { .. } => {
                    unreachable!("slash routing returns a local result or CLI error")
                }
                TuiSubmissionOutcome::Quit => Ok(String::new()),
            }
        }
        prompt => run_tui_prompt_with(bootstrap, prompt, session, None, |request| {
            run_production_headless_chat_with_progress(
                request,
                bootstrap,
                cancellation,
                progress,
                None,
                None,
                None,
            )
        }),
    }
}

fn run_tui_prompt_with(
    bootstrap: &Bootstrap,
    prompt: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
    skills: Option<Arc<SkillCatalog>>,
    run: impl FnOnce(HeadlessChatRequest) -> Result<HeadlessChatCompletion, HeadlessChatFailure>,
) -> Result<String, CliError> {
    let prompt = expand_tui_file_reference(bootstrap, prompt)?;
    let request = {
        let mut session = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        if session.running {
            return Err(CliError::runtime(HeadlessTurnError::State));
        }
        session.running = true;
        let mut request = session.apply_to(HeadlessChatRequest {
            prompt,
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
            skills: skills.clone(),
        });
        if let Some(skills) = skills {
            let base = request
                .system_prompt
                .take()
                .or_else(|| bootstrap.system_prompt.clone())
                .unwrap_or_else(|| "You are Agens, a helpful coding agent.".into());
            request.system_prompt = Some(parent_skill_system_prompt(&base, &skills));
        }
        request
    };
    let consumed_reminder = request.pending_system_reminder.is_some();
    let completion = run(request);
    let mut session = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    session.running = false;
    complete_tui_turn(&mut session, completion, consumed_reminder)
}

pub fn tui_file_candidates(bootstrap: &Bootstrap) -> Result<Vec<String>, CliError> {
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    NativeTools::open(project_root)
        .map_err(|_| CliError::configuration("native tools are unavailable"))?
        .tui_file_candidates(100)
        .map_err(|output| CliError::new(ExitStatus::Failure, "file", output.content))
}

fn selected_tui_file(bootstrap: &Bootstrap, selection: &str) -> Result<String, CliError> {
    if selection.is_empty() || selection.chars().count() > 121 {
        return Err(CliError::usage("selected file is invalid"));
    }

    tui_select_candidates(bootstrap)?
        .into_iter()
        .find(|candidate| candidate == selection)
        .ok_or_else(|| CliError::usage("selected file is unavailable"))
}

fn tui_select_candidates(bootstrap: &Bootstrap) -> Result<Vec<String>, CliError> {
    Ok(tui_file_candidates(bootstrap)?
        .into_iter()
        .filter(|path| path.chars().count() <= 121)
        .take(64)
        .collect())
}

fn expand_tui_file_reference(bootstrap: &Bootstrap, prompt: &str) -> Result<String, CliError> {
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    let tools = NativeTools::open(project_root)
        .map_err(|_| CliError::configuration("native tools are unavailable"))?;
    let mut expanded = String::with_capacity(prompt.len());

    for segment in prompt.split_inclusive(char::is_whitespace) {
        let token = segment.trim_end_matches(char::is_whitespace);
        let whitespace = &segment[token.len()..];
        if let Some(path) = token.strip_prefix('@').filter(|path| !path.is_empty()) {
            let output = tools
                .read_file(ReadFileInput::new(path))
                .map_err(|_| CliError::new(ExitStatus::Failure, "file", "read failed"))?;
            if output.is_error {
                return Err(CliError::new(ExitStatus::Failure, "file", output.content));
            }
            expanded.push_str(&format!(
                "<file path=\"{path}\">\n{}\n</file>",
                output.content
            ));
        } else {
            expanded.push_str(token);
        }
        expanded.push_str(whitespace);
    }

    Ok(expanded)
}

fn complete_tui_turn(
    session: &mut TuiSessionContext,
    completion: Result<HeadlessChatCompletion, HeadlessChatFailure>,
    consumed_reminder: bool,
) -> Result<String, CliError> {
    let completion = match completion {
        Ok(completion) => completion,
        Err(failure) => {
            if let Some(partial) = failure.partial {
                session.identifier = Some(partial.metadata.id);
                session.metadata = Some(partial.metadata);
                adopt_turn_history(session, partial.messages);
            }

            return Err(failure.error);
        }
    };
    session.identifier = Some(completion.metadata.id);
    session.metadata = Some(completion.metadata);
    adopt_turn_history(session, completion.messages);
    if consumed_reminder {
        session.pending_system_reminder = None;
    }
    Ok(completion.text)
}

/// A background subagent turn can be persisted after the foreground turn reloaded the session, so
/// adopting the turn's history alone would drop that turn from the in-process request history for
/// the rest of the process even though the store keeps it.
fn adopt_turn_history(session: &mut TuiSessionContext, history: Vec<Message>) {
    let preserved = missing_subagent_turns(&session.messages, &history);
    session.messages = history;
    session.messages.extend(preserved);
}

fn missing_subagent_turns(previous: &[Message], history: &[Message]) -> Vec<Message> {
    let known = history
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(subagent_call_id)
        .collect::<BTreeSet<_>>();

    previous
        .windows(3)
        .filter(|window| {
            let [user, assistant, tool] = window else {
                return false;
            };
            let Some(call_id) = assistant.parts.iter().find_map(subagent_call_id) else {
                return false;
            };

            user.role == Role::User
                && !known.contains(call_id)
                && tool.parts.iter().any(|part| match part {
                    MessagePart::ToolResult { tool_call_id, .. } => tool_call_id == call_id,
                    _ => false,
                })
        })
        .flatten()
        .cloned()
        .collect()
}

fn subagent_call_id(part: &MessagePart) -> Option<&str> {
    match part {
        MessagePart::ToolCall { id, .. } if id.starts_with(SUBAGENT_CALL_ID_PREFIX) => Some(id),
        _ => None,
    }
}

fn current_tui_provider(bootstrap: &Bootstrap, context: &TuiSessionContext) -> Option<TuiProvider> {
    if context.chatgpt_unavailable {
        return None;
    }
    if context.resume_error.is_some()
        && context
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.provider_id.is_some())
        && context.provider.is_none()
    {
        return None;
    }
    context
        .provider
        .or_else(|| bootstrap.provider_type().and_then(TuiProvider::parse))
}

fn effective_tui_model(bootstrap: &Bootstrap, context: &TuiSessionContext) -> String {
    context
        .selection
        .as_ref()
        .map(TuiModelSelector::model)
        .or_else(|| {
            context
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.model_id.as_deref())
        })
        .or_else(|| bootstrap.model())
        .unwrap_or_else(|| default_model(bootstrap))
        .to_owned()
}

fn tui_session_presentation(bootstrap: &Bootstrap, session: &TuiSessionContext) -> TuiPresentation {
    let model = effective_tui_model(bootstrap, session);
    let provider = session
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.provider_id.as_deref())
        .or_else(|| current_tui_provider(bootstrap, session).map(TuiProvider::identifier))
        .unwrap_or_else(|| bootstrap.provider_type().unwrap_or("provider"));
    let label = session
        .identifier
        .map_or_else(|| "new session".into(), |id| format!("session #{id}"));
    let effort = session
        .selection
        .as_ref()
        .and_then(|selection| {
            selection
                .reasoning_effort()
                .or_else(|| selection.reasoning_effort_default())
        })
        .or_else(|| {
            TuiModelSelector::for_source(&model, tui_model_source(bootstrap, session))
                .reasoning_effort_default()
        });
    let mut presentation = TuiPresentation::new(provider, &model, label)
        .with_context_window(model_registry::context_window_for(&model))
        .with_dangerous_mode(session.dangerous_mode);
    if let Some(effort) = effort {
        presentation = presentation.with_effort(effort);
    }
    presentation
}

enum ChatGptCredentialSnapshot {
    Absent,
    Present(serde_json::Value),
}

fn snapshot_chatgpt_credentials(path: &Path) -> Result<ChatGptCredentialSnapshot, CliError> {
    match fs::read_to_string(path) {
        Ok(credentials) => serde_json::from_str::<serde_json::Value>(&credentials)
            .ok()
            .and_then(|root| root.get("openai-chatgpt").cloned())
            .map_or(Ok(ChatGptCredentialSnapshot::Absent), |entry| {
                Ok(ChatGptCredentialSnapshot::Present(entry))
            }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound || path.is_dir() => {
            Ok(ChatGptCredentialSnapshot::Absent)
        }
        Err(_) => Err(CliError::storage("ChatGPT credentials could not be read")),
    }
}

fn restore_chatgpt_credentials(
    path: &Path,
    snapshot: ChatGptCredentialSnapshot,
) -> Result<(), CliError> {
    remove_provider_entry(path, "openai-chatgpt")
        .map_err(|_| CliError::storage("ChatGPT credential recovery failed"))?;

    if let ChatGptCredentialSnapshot::Present(entry) = snapshot {
        upsert_provider_entry(path, "openai-chatgpt", entry)
            .map_err(|_| CliError::storage("ChatGPT credential recovery failed"))?;
    }

    Ok(())
}

fn apply_tui_selection(
    bootstrap: &Bootstrap,
    context: &mut TuiSessionContext,
    provider: TuiProvider,
    selector: TuiModelSelector,
) -> Result<(), CliError> {
    if let Some(mut metadata) = context.metadata.clone() {
        metadata.provider_id = Some(provider.identifier().into());
        metadata.model_id = Some(selector.model().into());
        metadata.reasoning_effort = selector.reasoning_effort_value();
        SessionStore::open(bootstrap.data_directory())
            .and_then(|mut store| store.update_session_selection(&metadata))
            .map_err(|_| CliError::storage("session selection could not be saved"))?;
        context.metadata = Some(metadata);
    }
    PreferenceStore::open(bootstrap.data_directory())
        .and_then(|mut store| {
            store.remember_model(&ModelPreference::new(
                selector.model(),
                selector.reasoning_effort_value(),
            ))
        })
        .map_err(|_| CliError::storage("model preference could not be saved"))?;
    context.provider = Some(provider);
    context.selection = Some(selector);
    context.active_agent = None;
    Ok(())
}

/// Resolves the model for a fresh session: a CLI flag or configured model first, then the last
/// remembered selection, then the hardcoded default.
///
/// A model written into configuration by hand is a deliberate statement, so a terminal pick never
/// silently overrides it. Returns the notice the user must see when a remembered selection cannot
/// be honored, because falling back to a different model without saying so is indistinguishable
/// from the preference being ignored.
fn seed_remembered_tui_selection(
    bootstrap: &Bootstrap,
    context: &mut TuiSessionContext,
) -> Option<String> {
    if bootstrap.model().is_some() {
        return None;
    }

    let preference = match PreferenceStore::open(bootstrap.data_directory())
        .and_then(|store| store.remembered_model())
    {
        Ok(Some(preference)) => preference,
        Ok(None) => return None,
        Err(_) => return Some("Remembered model selection could not be read.".to_owned()),
    };
    let source = tui_model_source(bootstrap, context);
    let default = default_model(bootstrap);
    let mut selector = TuiModelSelector::for_source(default, source);
    if selector.apply_model(preference.model()).is_err() {
        return Some(format!(
            "Remembered model {} is unavailable for {}; using {default}.",
            preference.model(),
            source.label()
        ));
    }

    let dropped_effort = preference
        .reasoning_effort()
        .is_some_and(|effort| selector.apply_reasoning_effort(effort.as_str()).is_err());
    let notice = dropped_effort.then(|| {
        format!(
            "Remembered reasoning effort is unsupported by {}; using Default.",
            preference.model()
        )
    });
    context.selection = Some(selector);
    notice
}

fn tui_model_source(bootstrap: &Bootstrap, context: &TuiSessionContext) -> TuiModelSource {
    current_tui_provider(bootstrap, context)
        .unwrap_or(TuiProvider::OpenAiApi)
        .source()
}

fn format_model_metadata(model: &model_registry::ModelMetadata) -> String {
    let context = model
        .context
        .map(format_token_count)
        .unwrap_or_else(|| "?".into());
    let output = model
        .output
        .map(format_token_count)
        .unwrap_or_else(|| "?".into());
    let reasoning = match model.reasoning {
        Some(true) => "reasoning",
        Some(false) => "no reasoning",
        None => "reasoning unknown",
    };
    format!("{context} context | {output} output | {reasoning}")
}

fn format_token_count(tokens: u64) -> String {
    if tokens.is_multiple_of(1_000) {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn select_tui_model(
    bootstrap: &Bootstrap,
    command: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let model = command.strip_prefix("/model").unwrap_or_default().trim();
    if model.is_empty() {
        let context = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        let selector =
            TuiModelSelector::for_source("gpt-4.1", tui_model_source(bootstrap, &context));
        let values = selector
            .model_values()
            .map_err(CliError::unavailable)?
            .join(", ");
        let current = context
            .selection
            .as_ref()
            .map(|selection| selection.model())
            .or_else(|| bootstrap.model())
            .unwrap_or_else(|| default_model(bootstrap));
        return Ok(format!("Model: {current}. Available: {values}."));
    }

    apply_tui_model(bootstrap, model, session)
}

fn apply_tui_model(
    bootstrap: &Bootstrap,
    model: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let mut context = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    let mut selector = context.selection.clone().unwrap_or_else(|| {
        TuiModelSelector::for_source(model, tui_model_source(bootstrap, &context))
    });
    let previous_effort = selector.reasoning_effort();
    selector
        .apply_model(model)
        .map_err(CliError::configuration)?;
    let reset_effort = previous_effort.filter(|_| selector.reasoning_effort().is_none());
    let provider = current_tui_provider(bootstrap, &context)
        .ok_or_else(|| CliError::configuration("TUI provider is unavailable"))?;
    apply_tui_selection(bootstrap, &mut context, provider, selector)?;
    Ok(reset_effort.map_or_else(
        || format!("Model: {model}."),
        |effort| {
            format!(
                "Model: {model}. Reasoning effort reset to Default because {effort} is unsupported."
            )
        },
    ))
}

fn apply_tui_unverified_model(
    bootstrap: &Bootstrap,
    model: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let mut context = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    let mut selector = context.selection.clone().unwrap_or_else(|| {
        TuiModelSelector::for_source(model, tui_model_source(bootstrap, &context))
    });
    let reset_effort = selector.reasoning_effort().is_some();
    selector
        .apply_unverified_model(model)
        .map_err(CliError::configuration)?;
    let provider = current_tui_provider(bootstrap, &context)
        .ok_or_else(|| CliError::configuration("TUI provider is unavailable"))?;
    apply_tui_selection(bootstrap, &mut context, provider, selector)?;

    Ok(if reset_effort {
        format!("Model: {model} (unverified metadata). Reasoning effort reset to Default.")
    } else {
        format!("Model: {model} (unverified metadata).")
    })
}

fn select_tui_effort(
    bootstrap: &Bootstrap,
    command: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let effort = command.strip_prefix("/effort").unwrap_or_default().trim();
    let context = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    if effort.is_empty() {
        let current = context
            .selection
            .as_ref()
            .and_then(|selection| selection.reasoning_effort())
            .unwrap_or("default");
        return Ok(format!("Reasoning effort: {current}."));
    }

    drop(context);
    apply_tui_effort(bootstrap, effort, session)
}

fn apply_tui_effort(
    bootstrap: &Bootstrap,
    effort: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let mut context = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    let model = context
        .selection
        .as_ref()
        .map(|selection| selection.model())
        .or_else(|| bootstrap.model())
        .unwrap_or_else(|| default_model(bootstrap));
    let mut selector = TuiModelSelector::for_source(model, tui_model_source(bootstrap, &context));
    selector
        .apply_reasoning_effort(effort)
        .map_err(CliError::configuration)?;
    let provider = current_tui_provider(bootstrap, &context)
        .ok_or_else(|| CliError::configuration("TUI provider is unavailable"))?;
    apply_tui_selection(bootstrap, &mut context, provider, selector)?;
    let effort = if effort == "default" {
        "Default"
    } else {
        effort
    };
    Ok(format!("Reasoning effort: {effort}."))
}

fn rotate_tui_agent(
    bootstrap: &Bootstrap,
    name: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
    skills: &SkillCatalog,
) -> Result<String, CliError> {
    let validator = {
        let context = session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        TuiAgentModelValidator::for_context(bootstrap, &context)?
    };
    let catalog = tui_agent_catalog(bootstrap, &validator)?;
    if session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?
        .running
    {
        return Err(CliError::runtime(HeadlessTurnError::State));
    }
    let agent = catalog
        .agent(name.trim())
        .filter(|agent| agent.mode != agens_core::AgentMode::Subagent)
        .ok_or_else(|| CliError::usage("/agent requires an available primary agent"))?
        .clone();
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    let (_, dispatcher) = production_tool_runtime(bootstrap, project_root, Some(skills))?;
    ensure_active_tui_agent_runtime(bootstrap, session, &dispatcher)?;
    let dispatcher = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is unavailable"))?;
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    if context.running {
        return Err(CliError::runtime(HeadlessTurnError::State));
    }
    let inherited_model = effective_tui_model(bootstrap, &context);
    let mut store = context
        .metadata
        .is_some()
        .then(|| SessionStore::open(bootstrap.data_directory()))
        .transpose()
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    rotate_active_agent(
        &mut context,
        &agent,
        Some(&inherited_model),
        &project_root.display().to_string(),
        &dispatcher,
        &validator,
        store.as_mut(),
    )
    .map_err(agent_rotation_error)?;
    Ok(format!("Active agent: {}.", agent.name))
}

#[cfg(test)]
fn list_tui_agents(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<TuiSessionContext>>,
    mode: agens_core::AgentMode,
) -> Result<String, CliError> {
    let context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    let catalog = tui_agent_catalog_for_context(bootstrap, &context)?;
    let current = match mode {
        agens_core::AgentMode::Primary => context
            .active_agent
            .as_ref()
            .map(|agent| agent.name.as_str()),
        agens_core::AgentMode::Subagent => context.selected_subagent.as_deref(),
        agens_core::AgentMode::All => None,
    }
    .unwrap_or("none");
    let agents = match mode {
        agens_core::AgentMode::Primary => catalog
            .primary_or_all()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>(),
        agens_core::AgentMode::Subagent => catalog
            .subagents()
            .filter(|agent| agent.mode == agens_core::AgentMode::Subagent)
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>(),
        agens_core::AgentMode::All => unreachable!("TUI selectors do not expose all-mode agents"),
    };
    let label = if mode == agens_core::AgentMode::Subagent {
        "Subagent"
    } else {
        "Active agent"
    };
    if agents.is_empty() {
        return Ok(format!("{label}: none."));
    }

    Ok(format!(
        "{label}: {current}. Available: {}.",
        agents.join(", ")
    ))
}

fn select_tui_subagent(
    bootstrap: &Bootstrap,
    name: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let snapshot = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?
        .clone();
    let agents = tui_subagent_catalog(bootstrap, &snapshot)?.collect::<Vec<_>>();
    if agents.is_empty() {
        return Err(CliError::usage("No eligible subagents are available."));
    }
    let agent = agents
        .into_iter()
        .find(|agent| agent.name == name.trim())
        .ok_or_else(|| CliError::usage("/subagent requires an available subagent"))?;
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    if context.running {
        return Err(CliError::runtime(HeadlessTurnError::State));
    }
    context.selected_subagent = Some(agent.name.clone());
    Ok(format!("Subagent: {}.", agent.name))
}

fn tui_subagent_catalog(
    bootstrap: &Bootstrap,
    context: &TuiSessionContext,
) -> Result<impl Iterator<Item = AgentDefinition>, CliError> {
    if bootstrap
        .provider_type()
        .and_then(TuiProvider::parse)
        .is_none()
    {
        return Ok(Vec::new().into_iter());
    }

    let agents = tui_agent_catalog_for_context(bootstrap, context)?
        .subagents()
        .filter(|agent| agent.mode == agens_core::AgentMode::Subagent)
        .cloned()
        .collect::<Vec<_>>();
    Ok(agents.into_iter())
}

fn tui_agent_catalog(
    bootstrap: &Bootstrap,
    validator: &dyn AgentModelValidator,
) -> Result<AgentCatalog, CliError> {
    discover_tui_agent_catalog(bootstrap, Some(validator))
}

fn tui_agent_catalog_for_context(
    bootstrap: &Bootstrap,
    context: &TuiSessionContext,
) -> Result<AgentCatalog, CliError> {
    let validator = TuiAgentModelValidator::for_context(bootstrap, context)?;
    tui_agent_catalog(bootstrap, &validator)
}

fn tui_task_agent_catalog(bootstrap: &Bootstrap) -> Result<AgentCatalog, CliError> {
    discover_tui_agent_catalog(bootstrap, None)
}

fn discover_tui_agent_catalog(
    bootstrap: &Bootstrap,
    validator: Option<&dyn AgentModelValidator>,
) -> Result<AgentCatalog, CliError> {
    let primary = AgentDefinition {
        name: "primary".into(),
        description: "Default interactive agent".into(),
        mode: agens_core::AgentMode::Primary,
        model: None,
        system_prompt: bootstrap
            .system_prompt
            .clone()
            .unwrap_or_else(|| "You are Agens, a helpful coding agent.".into()),
        permission_rules: Vec::new(),
        skills: Vec::new(),
    };
    let explore = AgentDefinition {
        name: "explore".into(),
        description: "Explore the codebase without modifying files".into(),
        mode: agens_core::AgentMode::Subagent,
        model: None,
        system_prompt: "You are the read-only exploration subagent. Inspect the codebase without modifying files and return concise, grounded findings."
            .into(),
        permission_rules: Vec::new(),
        skills: Vec::new(),
    };
    let general = AgentDefinition {
        name: "general".into(),
        description: "Handle a general delegated coding task".into(),
        mode: agens_core::AgentMode::Subagent,
        model: None,
        system_prompt: "You are the general-purpose subagent. Complete the delegated task with the available native tools and return a concise result."
            .into(),
        permission_rules: Vec::new(),
        skills: Vec::new(),
    };
    let global = bootstrap.paths.global_config.with_file_name("agents");
    let project = bootstrap.paths.project_config.with_file_name("agents");
    let built_ins = [primary, explore, general];
    let discovery = match validator {
        Some(validator) => {
            AgentCatalog::discover_with_model_validator(&built_ins, &global, &project, validator)
        }
        None => AgentCatalog::discover(&built_ins, &global, &project),
    };
    discovery
        .map(|discovery| discovery.catalog().clone())
        .map_err(|_| CliError::configuration("agent catalog is unavailable"))
}

fn agent_rotation_error(error: AgentRotationError) -> CliError {
    match error {
        AgentRotationError::Busy => CliError::runtime(HeadlessTurnError::State),
        AgentRotationError::ModelUnavailable => {
            CliError::configuration("agent model is unavailable")
        }
        AgentRotationError::Persistence => CliError::storage("active agent could not be saved"),
    }
}

#[derive(Clone)]
struct TuiAgentModelValidator {
    available: Arc<BTreeSet<String>>,
}

impl TuiAgentModelValidator {
    fn for_source(source: TuiModelSource) -> Result<Self, CliError> {
        let available = TuiModelSelector::for_source("gpt-4.1", source)
            .model_values()
            .map_err(CliError::unavailable)?
            .into_iter()
            .collect();
        Ok(Self {
            available: Arc::new(available),
        })
    }

    fn for_context(bootstrap: &Bootstrap, context: &TuiSessionContext) -> Result<Self, CliError> {
        Self::for_source(tui_model_source(bootstrap, context))
    }
}

impl AgentModelValidator for TuiAgentModelValidator {
    fn validate_model(&self, model: &str) -> Result<(), agens_tools::AgentModelValidationError> {
        self.available
            .contains(model)
            .then_some(())
            .ok_or(agens_tools::AgentModelValidationError::Unavailable)
    }
}

#[cfg(test)]
struct BundledModelValidator;

#[cfg(test)]
impl AgentModelValidator for BundledModelValidator {
    fn validate_model(&self, model: &str) -> Result<(), agens_tools::AgentModelValidationError> {
        [
            TuiModelSource::OpenAiApi,
            TuiModelSource::ChatGptSubscription,
        ]
        .into_iter()
        .any(|source| {
            TuiModelSelector::for_source(model, source)
                .model_values()
                .is_ok_and(|models| models.iter().any(|candidate| candidate == model))
        })
        .then_some(())
        .ok_or(agens_tools::AgentModelValidationError::Unavailable)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PersistedAgentResolution {
    agent: AgentDefinition,
    fallback_from: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PersistedAgentResolutionError {
    Model,
    Agent,
    Primary,
}

fn resolve_persisted_active_agent(
    name: &str,
    catalog: &AgentCatalog,
    unvalidated_catalog: &AgentCatalog,
    validator: &dyn AgentModelValidator,
) -> Result<PersistedAgentResolution, PersistedAgentResolutionError> {
    let unvalidated = unvalidated_catalog.agent(name);
    if unvalidated
        .and_then(|agent| agent.model.as_deref())
        .is_some_and(|model| validator.validate_model(model).is_err())
    {
        return Err(PersistedAgentResolutionError::Model);
    }
    if let Some(agent) = catalog
        .agent(name)
        .filter(|agent| agent.mode != agens_core::AgentMode::Subagent)
    {
        return Ok(PersistedAgentResolution {
            agent: agent.clone(),
            fallback_from: None,
        });
    }
    if name == "primary" {
        return Err(PersistedAgentResolutionError::Primary);
    }
    if unvalidated.is_some() {
        return Err(PersistedAgentResolutionError::Agent);
    }

    let unvalidated_primary = unvalidated_catalog
        .agent("primary")
        .ok_or(PersistedAgentResolutionError::Primary)?;
    if unvalidated_primary.mode == agens_core::AgentMode::Subagent {
        return Err(PersistedAgentResolutionError::Primary);
    }
    if unvalidated_primary
        .model
        .as_deref()
        .is_some_and(|model| validator.validate_model(model).is_err())
    {
        return Err(PersistedAgentResolutionError::Model);
    }
    let primary = catalog
        .agent("primary")
        .filter(|agent| agent.mode != agens_core::AgentMode::Subagent)
        .ok_or(PersistedAgentResolutionError::Primary)?;
    Ok(PersistedAgentResolution {
        agent: primary.clone(),
        fallback_from: Some(name.to_owned()),
    })
}

fn persisted_agent_resolution_error(error: PersistedAgentResolutionError) -> CliError {
    match error {
        PersistedAgentResolutionError::Model => {
            CliError::configuration("agent model is unavailable")
        }
        PersistedAgentResolutionError::Agent => {
            CliError::configuration("active agent is unavailable")
        }
        PersistedAgentResolutionError::Primary => {
            CliError::configuration("primary agent is unavailable")
        }
    }
}

fn reconcile_persisted_active_agent(
    bootstrap: &Bootstrap,
    context: &mut TuiSessionContext,
) -> Result<AgentDefinition, CliError> {
    let name = context
        .metadata
        .as_ref()
        .map(|metadata| metadata.active_agent.clone())
        .unwrap_or_else(|| "primary".into());
    let validator = TuiAgentModelValidator::for_context(bootstrap, context)?;
    let catalog = tui_agent_catalog(bootstrap, &validator)?;
    let unvalidated_catalog = tui_task_agent_catalog(bootstrap)?;
    let resolution =
        resolve_persisted_active_agent(&name, &catalog, &unvalidated_catalog, &validator).map_err(
            |error| {
                record_agent_diagnostic(bootstrap, ProviderDiagnosticKind::AgentUnavailable);
                persisted_agent_resolution_error(error)
            },
        )?;

    let Some(stale_name) = resolution.fallback_from.as_deref() else {
        return Ok(resolution.agent);
    };
    if let Some(metadata) = context.metadata.as_mut() {
        metadata.active_agent = "primary".into();
        context.agent_correction_pending = true;
    }
    context.resume_notice = Some(format!(
        "Agent '{stale_name}' is unavailable; resumed with primary."
    ));
    record_agent_diagnostic(bootstrap, ProviderDiagnosticKind::AgentFallback);

    Ok(resolution.agent)
}

fn persist_pending_agent_correction(bootstrap: &Bootstrap, context: &mut TuiSessionContext) {
    if !context.agent_correction_pending {
        return;
    }
    context.agent_correction_pending = false;

    let Some(metadata) = context.metadata.as_mut() else {
        return;
    };
    let mut corrected = metadata.clone();
    corrected.updated_at = current_session_timestamp();
    if SessionStore::open(bootstrap.data_directory())
        .and_then(|mut store| store.update_session(&corrected))
        .is_ok()
    {
        *metadata = corrected;
    }
}

#[derive(Clone)]
struct TaskModelValidator {
    available: Arc<BTreeSet<String>>,
}

impl TaskModelValidator {
    fn new(models: &[String]) -> Self {
        Self {
            available: Arc::new(models.iter().cloned().collect()),
        }
    }
}

impl AgentModelValidator for TaskModelValidator {
    fn validate_model(&self, model: &str) -> Result<(), agens_tools::AgentModelValidationError> {
        self.available
            .contains(model)
            .then_some(())
            .ok_or(agens_tools::AgentModelValidationError::Unavailable)
    }
}

fn task_model_catalog(bootstrap: &Bootstrap) -> Result<Vec<String>, CliError> {
    let source = bootstrap
        .provider_type()
        .and_then(TuiProvider::parse)
        .map(TuiProvider::source)
        .ok_or_else(|| CliError::configuration("task provider is unavailable"))?;
    TuiModelSelector::for_source(default_model(bootstrap), source)
        .model_values()
        .map_err(CliError::unavailable)
}

#[cfg(test)]
fn list_tui_sessions(bootstrap: &Bootstrap) -> Result<String, CliError> {
    let project = tui_project_identifier(bootstrap)?;
    let store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let sessions = store
        .list_sessions()
        .map_err(|_| CliError::storage("saved sessions could not be listed"))?
        .into_iter()
        .filter(|session| session.project == project)
        .collect::<Vec<_>>();

    if sessions.is_empty() {
        return Ok("No saved sessions.".to_owned());
    }

    Ok(sessions
        .iter()
        .map(|session| format!("{}\t{} event(s)", session.id, session.completed_turn_count))
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
thread_local! {
    static TUI_RESUME_LOAD_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TUI_RESUME_PROJECTION_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PRODUCTION_TOOL_RUNTIME_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PRODUCTION_PROVIDER_RUNTIME_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct LoadedTuiSessionResume {
    session: StoredSession,
    retry_boundary: Option<RetryBoundary>,
}

impl std::ops::Deref for LoadedTuiSessionResume {
    type Target = StoredSession;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

#[cfg(test)]
fn reset_tui_resume_test_counters() {
    TUI_RESUME_LOAD_CALLS.with(|calls| calls.set(0));
    TUI_RESUME_PROJECTION_CALLS.with(|calls| calls.set(0));
    PRODUCTION_TOOL_RUNTIME_CALLS.with(|calls| calls.set(0));
    PRODUCTION_PROVIDER_RUNTIME_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
fn tui_resume_test_counters() -> (usize, usize, usize, usize) {
    (
        TUI_RESUME_LOAD_CALLS.with(std::cell::Cell::get),
        TUI_RESUME_PROJECTION_CALLS.with(std::cell::Cell::get),
        PRODUCTION_TOOL_RUNTIME_CALLS.with(std::cell::Cell::get),
        PRODUCTION_PROVIDER_RUNTIME_CALLS.with(std::cell::Cell::get),
    )
}

fn resume_tui_session(
    bootstrap: &Bootstrap,
    identifier: i64,
    _skills: &SkillCatalog,
    credentials: &TuiCredentialResolver,
) -> Result<TuiSessionContext, CliError> {
    let session = load_tui_session_for_resume(bootstrap, identifier)?;
    prepare_loaded_tui_session_resume(bootstrap, identifier, session, credentials)
}

fn load_tui_session_for_resume(
    bootstrap: &Bootstrap,
    identifier: i64,
) -> Result<LoadedTuiSessionResume, CliError> {
    #[cfg(test)]
    TUI_RESUME_LOAD_CALLS.with(|calls| calls.set(calls.get() + 1));

    let store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let session = store
        .load_session_for_resume(identifier)
        .map_err(|_| CliError::storage("saved session is unavailable"))?;
    let retry_boundary = session
        .latest_attempt
        .as_ref()
        .filter(|attempt| resume_retry_notice(attempt.status()).is_some())
        .map(|attempt| {
            store
                .load_retry_boundary(attempt.key())
                .map_err(|_| CliError::storage("saved session is unavailable"))
        })
        .transpose()?
        .flatten();
    if session.metadata.completed_turn_count == 0
        && session
            .latest_attempt
            .as_ref()
            .is_none_or(|attempt| attempt.status() != SessionAttemptStatus::Running)
        && retry_boundary.is_none()
    {
        return Err(CliError::storage("saved session is unavailable"));
    }
    Ok(LoadedTuiSessionResume {
        session,
        retry_boundary,
    })
}

fn prepare_loaded_tui_session_resume(
    bootstrap: &Bootstrap,
    identifier: i64,
    loaded: LoadedTuiSessionResume,
    credentials: &TuiCredentialResolver,
) -> Result<TuiSessionContext, CliError> {
    let LoadedTuiSessionResume {
        session,
        retry_boundary,
    } = loaded;
    if session.metadata.project != tui_project_identifier(bootstrap)? {
        return Err(CliError::storage("saved session is unavailable"));
    }
    #[cfg(test)]
    TUI_RESUME_PROJECTION_CALLS.with(|calls| calls.set(calls.get() + 1));
    let restored_history =
        Conversation::from_messages_with_parser(&session.messages, |name, input| {
            let bare = name
                .strip_prefix("native::")
                .or_else(|| name.strip_prefix("mcp::"))
                .unwrap_or(name);
            agens_core::ToolInput::parse(bare, input)
        })
        .map_err(|_| CliError::storage("saved session is unavailable"))?;
    let saved_provider = session.metadata.provider_id.as_deref();
    let provider = saved_provider.and_then(TuiProvider::parse);
    let selection_provider =
        provider.or_else(|| bootstrap.provider_type().and_then(TuiProvider::parse));
    let selection = match (session.metadata.model_id.as_deref(), selection_provider) {
        (Some(model), Some(provider)) => {
            let mut selector = TuiModelSelector::for_source(model, provider.source());
            if selector.apply_model(model).is_err() {
                selector
                    .apply_unverified_model(model)
                    .map_err(|_| CliError::storage("saved session selection is unavailable"))?;
            }
            if let Some(effort) = session.metadata.reasoning_effort {
                selector
                    .apply_reasoning_effort(effort.as_str())
                    .map_err(|_| CliError::storage("saved session selection is unavailable"))?;
            }
            Some(selector)
        }
        _ => None,
    };
    let resume_error = saved_provider
        .filter(|_| {
            provider.is_none_or(|provider| {
                !credentials
                    .status(&bootstrap.paths.credentials, provider)
                    .available()
            })
        })
        .map(|_| "connect or choose provider".to_owned());
    let mut context = TuiSessionContext::restored(
        identifier,
        session.metadata,
        session.messages,
        restored_history,
    );
    context.provider = provider;
    context.selection = selection;
    context.resume_error = resume_error;
    if let Some(boundary) = retry_boundary {
        let status = session
            .latest_attempt
            .as_ref()
            .map(agens_core::SessionAttemptSummary::status)
            .ok_or_else(|| CliError::storage("saved session is unavailable"))?;
        context.resume_notice = resume_retry_notice(status).map(str::to_owned);
        context.resume_draft = Some(ResumeDraft::new(boundary.prompt().to_owned()));
    }
    reconcile_persisted_active_agent(bootstrap, &mut context)?;
    Ok(context)
}

fn commit_tui_session_resume(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<TuiSessionContext>>,
    expected: &TuiSessionContext,
    mut resumed: TuiSessionContext,
    cancellation: &TuiRouteCancellation,
) -> Result<TuiSubmissionOutcome, CliError> {
    let presentation = tui_session_presentation(bootstrap, &resumed);
    let message = resumed.note();
    let history = std::mem::take(&mut resumed.restored_history);
    let draft = resumed.resume_draft.take().map(ResumeDraft::into_inner);
    let resume_error = resumed.resume_error.clone();
    resumed.resume_notice = None;
    if cancellation.is_cancelled() {
        return Ok(TuiSubmissionOutcome::RouteCancelled);
    }

    let mut current = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    if current.running {
        return Err(CliError::runtime(HeadlessTurnError::State));
    }
    if *current != *expected || !cancellation.try_commit() {
        return Ok(TuiSubmissionOutcome::RouteCancelled);
    }
    persist_pending_agent_correction(bootstrap, &mut resumed);
    *current = resumed;

    Ok(TuiSubmissionOutcome::SessionResumed {
        message,
        presentation,
        history,
        draft,
        resume_error,
    })
}

const MAX_RESTORED_SUBAGENT_TOOL_USES: usize = 256;

fn resumed_subagent_cards(messages: &[Message]) -> Vec<TuiRuntimeEvent> {
    let mut restored = Vec::new();
    let mut seen = BTreeSet::new();

    for window in messages.windows(3) {
        let [user, assistant, tool] = window else {
            continue;
        };
        let [MessagePart::Text(task)] = user.parts.as_slice() else {
            continue;
        };
        let [
            MessagePart::ToolCall { id, name, input },
            MessagePart::Reasoning(reasoning),
        ] = assistant.parts.as_slice()
        else {
            continue;
        };
        let [
            MessagePart::ToolResult {
                tool_call_id,
                content: final_result,
                is_error: false,
            },
        ] = tool.parts.as_slice()
        else {
            continue;
        };
        let Some(id) = id
            .strip_prefix("subagent:")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|id| *id > 0)
        else {
            continue;
        };
        let Some((agent, description)) = (name == "native::task")
            .then(|| serde_json::from_str::<serde_json::Value>(input).ok())
            .flatten()
            .and_then(|value| {
                Some((
                    value.get("agent")?.as_str()?.to_owned(),
                    value.get("description")?.as_str()?.to_owned(),
                ))
            })
        else {
            continue;
        };
        let Some(tool_uses) = reasoning
            .strip_suffix(" tool uses")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|tool_uses| *tool_uses <= MAX_RESTORED_SUBAGENT_TOOL_USES)
        else {
            continue;
        };
        if task.is_empty()
            || agent.is_empty()
            || description != *task
            || *tool_call_id != format!("subagent:{id}")
            || !seen.insert(id)
        {
            continue;
        }

        restored.push(TuiRuntimeEvent::RestoredCompletedSubagent {
            id,
            agent: sanitize_subagent_summary(&agent),
            task_summary: sanitize_subagent_summary(task),
            final_result: sanitize_subagent_summary(final_result),
            tool_uses,
        });
    }

    restored
}

fn tui_project_identifier(bootstrap: &Bootstrap) -> Result<String, CliError> {
    bootstrap
        .project_root()
        .map(|project| project.display().to_string())
        .ok_or_else(|| CliError::configuration("TUI sessions require a project root"))
}

fn ensure_active_tui_agent_runtime(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<TuiSessionContext>>,
    dispatcher: &SharedToolDispatcher,
) -> Result<(), CliError> {
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    let dispatcher = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is unavailable"))?;
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    if context.active_agent.is_some() {
        return Ok(());
    }
    let agent = reconcile_persisted_active_agent(bootstrap, &mut context)?;
    let validator = TuiAgentModelValidator::for_context(bootstrap, &context)?;
    let inherited_model = effective_tui_model(bootstrap, &context);
    let active_agent = ActiveAgentRuntime::build(
        &agent,
        Some(&inherited_model),
        &project_root.display().to_string(),
        &dispatcher,
        &validator,
    )
    .map_err(agent_rotation_error)?;
    persist_pending_agent_correction(bootstrap, &mut context);
    context.active_agent = Some(active_agent);
    Ok(())
}

fn parse_chat_request(arguments: &[String]) -> Result<HeadlessChatRequest, CliError> {
    let mut request = HeadlessChatRequest {
        prompt: String::new(),
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
    };
    let mut index = 0;

    while let Some(argument) = arguments.get(index) {
        match argument.as_str() {
            "--model" => {
                request.model = Some(required_flag_value(arguments, &mut index, "--model")?)
            }
            "--system" => {
                request.system_prompt =
                    Some(required_flag_value(arguments, &mut index, "--system")?)
            }
            "--max-iterations" => {
                let value = required_flag_value(arguments, &mut index, "--max-iterations")?;
                let parsed = value
                    .parse::<usize>()
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or_else(|| CliError::usage("chat --max-iterations must be >= 1"))?;
                request.max_iterations = Some(parsed);
            }
            "--mode" => {
                let value = required_flag_value(arguments, &mut index, "--mode")?;
                request.mode = match value.as_str() {
                    "edit" => PermissionMode::Edit,
                    "chat" => PermissionMode::Chat,
                    _ => return Err(CliError::usage("chat --mode must be chat or edit")),
                };
            }
            "--dangerously-allow-all" => request.dangerously_allow_all = true,
            argument if argument.starts_with('-') => {
                return Err(CliError::usage("chat received an unknown flag"));
            }
            prompt if request.prompt.is_empty() && !prompt.trim().is_empty() => {
                request.prompt = prompt.trim().to_owned();
            }
            _ => return Err(CliError::usage("chat accepts one prompt argument")),
        }
        index += 1;
    }

    if request.prompt.is_empty() {
        return Err(CliError::usage("chat requires a prompt argument"));
    }

    Ok(request)
}

fn required_flag_value(
    arguments: &[String],
    index: &mut usize,
    flag: &str,
) -> Result<String, CliError> {
    *index += 1;
    arguments
        .get(*index)
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .ok_or_else(|| CliError::usage(format!("chat {flag} requires a value")))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProviderSource {
    Auto,
    ExplicitChatGpt,
    ExplicitOther,
}
pub struct Bootstrap {
    paths: ConfigPaths,
    global_loaded: bool,
    project_loaded: bool,
    model: Option<String>,
    provider_type: Option<String>,
    provider_source: ProviderSource,
    provider_base_url: Option<String>,
    system_prompt: Option<String>,
    max_iterations: Option<usize>,
    parallel_tool_calls: bool,
    collapse_thinking: bool,
    openai_api_key: Option<String>,
    data_directory: PathBuf,
    project_root: Option<PathBuf>,
    mcp_servers: Vec<agens_config::McpServerConfig>,
    mcp_status: Option<McpStatusHandle>,
    permission_rules: Vec<ConfigPermissionRule>,
}

impl Clone for Bootstrap {
    fn clone(&self) -> Self {
        Self {
            paths: ConfigPaths {
                global_config: self.paths.global_config.clone(),
                credentials: self.paths.credentials.clone(),
                project_config: self.paths.project_config.clone(),
            },
            global_loaded: self.global_loaded,
            project_loaded: self.project_loaded,
            model: self.model.clone(),
            provider_type: self.provider_type.clone(),
            provider_source: self.provider_source,
            provider_base_url: self.provider_base_url.clone(),
            system_prompt: self.system_prompt.clone(),
            max_iterations: self.max_iterations,
            parallel_tool_calls: self.parallel_tool_calls,
            collapse_thinking: self.collapse_thinking,
            openai_api_key: self.openai_api_key.clone(),
            data_directory: self.data_directory.clone(),
            project_root: self.project_root.clone(),
            mcp_servers: self.mcp_servers.clone(),
            mcp_status: self.mcp_status.clone(),
            permission_rules: self.permission_rules.clone(),
        }
    }
}

impl Bootstrap {
    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn provider_type(&self) -> Option<&str> {
        self.provider_type.as_deref()
    }

    pub fn provider_base_url(&self) -> Option<&str> {
        self.provider_base_url.as_deref()
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }

    fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    fn permission_rules(&self) -> &[ConfigPermissionRule] {
        &self.permission_rules
    }

    pub fn mcp_transports(
        &self,
    ) -> Result<Vec<(String, McpStdioTransport, std::time::Duration)>, CliError> {
        let project_root = self
            .project_root
            .as_deref()
            .ok_or_else(|| CliError::configuration("MCP project root is unavailable"))?;
        self.mcp_servers
            .iter()
            .filter(|server| !server.disabled && server.transport == McpTransport::Stdio)
            .map(|server| {
                let transport = McpStdioTransport::spawn(McpStdioTransportConfig {
                    command: server
                        .command
                        .clone()
                        .expect("stdio MCP commands are validated"),
                    args: server.args.clone(),
                    environment: server.environment.clone(),
                    project_root: server
                        .cwd
                        .clone()
                        .unwrap_or_else(|| project_root.to_path_buf()),
                })
                .map_err(|_| CliError::configuration("MCP server configuration is unavailable"))?;
                Ok((
                    server.name.clone(),
                    transport,
                    std::time::Duration::from_millis(server.timeout_ms),
                ))
            })
            .collect()
    }
}

pub fn bootstrap(dependencies: &CliDependencies) -> Result<Bootstrap, CliError> {
    let current_directory = (dependencies.current_directory)()?;
    let home_directory = (dependencies.home_directory)();
    let environment = (dependencies.environment)();
    let project_root = discover_project_root(&current_directory);
    let config_root = project_root.as_deref().unwrap_or(&current_directory);
    let paths = resolve_paths(config_root, home_directory.as_deref(), &environment);
    let (global, global_loaded) = load_toml(&paths.global_config, "global", dependencies)?;
    let (project, project_loaded) = load_toml(&paths.project_config, "project", dependencies)?;
    if project.contains_key("mcp") {
        return Err(CliError::configuration(
            "project configuration cannot define MCP servers",
        ));
    }
    let permission_rules = extract_permission_rules(&global, &project)
        .map_err(|_| CliError::configuration("permission configuration is invalid"))?;
    let global = expand_global_mcp(global, &environment)?;
    let document = merge_toml_documents(global, project);
    let document = expand_document(document, &environment)?;

    let mcp_servers = mcp_servers(&document)
        .map_err(|_| CliError::configuration("MCP server configuration is invalid"))?;
    let credentials = (dependencies.read_file)(&paths.credentials)?;
    let configured_provider = string_value(&document, &["provider", "type"]);
    let provider_source = match configured_provider.as_deref() {
        None => ProviderSource::Auto,
        Some("openai-chatgpt") => ProviderSource::ExplicitChatGpt,
        Some(_) => ProviderSource::ExplicitOther,
    };
    let provider_type =
        resolve_provider_type(configured_provider, credentials.as_deref(), &environment);
    Ok(Bootstrap {
        model: string_value(&document, &["provider", "model"]),
        provider_type,
        provider_source,
        provider_base_url: string_value(&document, &["provider", "base_url"]),
        system_prompt: string_value(&document, &["agent", "system_prompt"]),
        max_iterations: document
            .get("agent")
            .and_then(toml::Value::as_table)
            .and_then(|agent| agent.get("max_iterations"))
            .and_then(toml::Value::as_integer)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0),
        parallel_tool_calls: document
            .get("agent")
            .and_then(toml::Value::as_table)
            .and_then(|agent| agent.get("parallel_tool_calls"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(true),
        collapse_thinking: document
            .get("ui")
            .and_then(toml::Value::as_table)
            .and_then(|ui| ui.get("collapse_thinking"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(false),
        openai_api_key: openai_api_key(credentials.as_deref(), &environment),
        data_directory: data_directory(&document, home_directory.as_deref(), &environment),
        project_root,
        mcp_servers,
        mcp_status: None,
        permission_rules,
        paths,
        global_loaded,
        project_loaded,
    })
}

fn run_production_headless_chat(
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

struct HeadlessChatCompletion {
    text: String,
    metadata: SessionMetadata,
    messages: Vec<Message>,
}

/// Failed turn plus any history the attempt already persisted, so the caller can adopt the
/// session the failed attempt belongs to instead of starting a new one on the next turn.
#[derive(Debug)]
struct HeadlessChatFailure {
    error: CliError,
    partial: Option<Box<PartialTurnRecord>>,
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

fn run_production_headless_chat_with_progress(
    mut request: HeadlessChatRequest,
    bootstrap: &Bootstrap,
    cancellation: &HeadlessTurnCancellation,
    progress: Option<&TurnProgressSink>,
    permission_bridge: Option<TuiPermissionBridge>,
    task_runtime: Option<&ProductionTuiTaskRuntime>,
    operation_reference: Option<&str>,
) -> Result<HeadlessChatCompletion, HeadlessChatFailure> {
    #[cfg(test)]
    PRODUCTION_PROVIDER_RUNTIME_CALLS.with(|calls| calls.set(calls.get() + 1));

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
        || {
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
            let snapshot = match request.max_iterations.or(context.bootstrap.max_iterations) {
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

fn provider_messages(request: &HeadlessChatRequest, include_system_prompt: bool) -> Vec<Message> {
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

struct DiscardCompletedTurnRepository;

impl CompletedTurnRepository for DiscardCompletedTurnRepository {
    fn persist_completed_turn(
        &mut self,
        _: CompletedTurnSnapshot,
    ) -> impl std::future::Future<Output = Result<(), CompletedTurnStoreError>> + Send {
        std::future::ready(Ok(()))
    }
}

fn next_session_metadata(
    bootstrap: &Bootstrap,
    title: &str,
    resumed: Option<&SessionMetadata>,
    active_agent: Option<&str>,
    provider_id: Option<String>,
    model_id: String,
    reasoning_effort: Option<agens_core::ReasoningEffort>,
) -> Result<SessionMetadata, CliError> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| CliError::storage("session clock is unavailable"))?
        .as_secs() as i64;

    if let Some(metadata) = resumed {
        return Ok(SessionMetadata {
            updated_at: timestamp,
            provider_id,
            model_id: Some(model_id),
            reasoning_effort,
            ..metadata.clone()
        });
    }

    Ok(SessionMetadata {
        id: 0,
        project: bootstrap
            .project_root()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "default".to_owned()),
        title: title.to_owned(),
        active_agent: active_agent.unwrap_or("primary").to_owned(),
        provider_id,
        model_id: Some(model_id),
        reasoning_effort,
        created_at: timestamp,
        updated_at: timestamp,
        completed_turn_count: 0,
        resumable: false,
    })
}

fn completed_session_turn(
    prompt: &str,
    snapshot: &CompletedTurnSnapshot,
    pending_system_reminder: Option<&str>,
) -> Result<CompletedSessionTurn, CliError> {
    completed_session_turn_from_events(prompt, snapshot.events(), pending_system_reminder)
}

fn completed_session_turn_from_events(
    prompt: &str,
    events: &[TurnEvent],
    pending_system_reminder: Option<&str>,
) -> Result<CompletedSessionTurn, CliError> {
    let mut messages = pending_system_reminder
        .map(|reminder| Message {
            role: Role::System,
            parts: vec![MessagePart::Text(reminder.to_owned())],
        })
        .into_iter()
        .collect::<Vec<_>>();
    messages.push(Message {
        role: Role::User,
        parts: vec![MessagePart::Text(prompt.to_owned())],
    });
    let mut role = None;
    let mut parts = Vec::new();
    for event in events {
        let (next_role, part) = match event {
            TurnEvent::ProviderPart(part) => (Role::Assistant, part),
            TurnEvent::ToolResult(part) => (Role::Tool, part),
            TurnEvent::StateChanged(_)
            | TurnEvent::Usage(_)
            | TurnEvent::ToolCallRequested { .. } => continue,
        };
        if role != Some(next_role) {
            if let Some(role) = role {
                flush_parts(&mut messages, role, &mut parts);
            }
            role = Some(next_role);
        }
        parts.push(part.clone());
    }
    if let Some(role) = role {
        flush_parts(&mut messages, role, &mut parts);
    }

    let messages = messages
        .into_iter()
        .map(SessionMessage::try_from)
        .collect::<Result<_, _>>()
        .map_err(|_| CliError::storage("completed session could not be encoded"))?;
    CompletedSessionTurn::new(messages)
        .map_err(|_| CliError::storage("completed session could not be encoded"))
}

fn completed_subagent_session_turn(
    turn: &CompletedSubagentTurn,
    call_id: &str,
) -> Result<CompletedSessionTurn, CliError> {
    let call_id = call_id.to_owned();
    let agent = sanitize_subagent_summary(&turn.agent);
    let task = sanitize_subagent_summary(&turn.task);
    let final_result = sanitize_subagent_result_for_persistence(&turn.final_result);
    let input = serde_json::json!({
        "agent": agent,
        "description": task,
    })
    .to_string();
    let messages = vec![
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text(task)],
        },
        Message {
            role: Role::Assistant,
            parts: vec![
                MessagePart::ToolCall {
                    id: call_id.clone(),
                    name: "native::task".into(),
                    input,
                },
                MessagePart::Reasoning(format!("{} tool uses", turn.tool_uses)),
            ],
        },
        Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: call_id,
                content: final_result,
                is_error: false,
            }],
        },
    ];
    let messages = messages
        .into_iter()
        .map(SessionMessage::try_from)
        .collect::<Result<_, _>>()
        .map_err(|_| CliError::storage("completed session could not be encoded"))?;
    CompletedSessionTurn::new(messages)
        .map_err(|_| CliError::storage("completed session could not be encoded"))
}

const SUBAGENT_CALL_ID_PREFIX: &str = "subagent:";
const MAX_SUBAGENT_SUMMARY_CHARS: usize = 256;
const MAX_PERSISTED_SUBAGENT_RESULT_CHARS: usize = 65_536;
const SUBAGENT_RESULT_TRUNCATION_MARKER: &str =
    "\n[truncated: only the first 65536 characters of this subagent result were persisted]";
const CREDENTIAL_MARKERS: [&str; 5] = ["api_key", "authorization", "password", "secret", "token"];

/// A subagent tool-call id must be unique inside the session, not merely inside the process:
/// execution ids restart at one in every process, so a resumed session would otherwise persist a
/// duplicate call id and make the whole history unencodable for the provider.
fn next_subagent_call_id(history: &[Message]) -> String {
    let highest = history
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| match part {
            MessagePart::ToolCall { id, .. } => id.strip_prefix(SUBAGENT_CALL_ID_PREFIX),
            _ => None,
        })
        .filter_map(|value| value.parse::<u64>().ok())
        .max()
        .unwrap_or(0);

    format!("{SUBAGENT_CALL_ID_PREFIX}{}", highest.saturating_add(1))
}

fn sanitize_subagent_summary(value: &str) -> String {
    if contains_credential_marker(value) {
        "[redacted]".into()
    } else {
        value.chars().take(MAX_SUBAGENT_SUMMARY_CHARS).collect()
    }
}

/// The persisted result is the model's only durable record of a background subagent's work, so it
/// keeps the same budget the foreground task path allows and every removal stays visible: silent
/// truncation or a wholesale replacement would make the model reason over a fragment it cannot see.
fn sanitize_subagent_result_for_persistence(value: &str) -> String {
    let redacted = redact_credential_lines(value);
    let mut bounded = redacted
        .chars()
        .take(MAX_PERSISTED_SUBAGENT_RESULT_CHARS)
        .collect::<String>();
    if redacted.chars().count() > MAX_PERSISTED_SUBAGENT_RESULT_CHARS {
        bounded.push_str(SUBAGENT_RESULT_TRUNCATION_MARKER);
    }
    bounded
}

fn redact_credential_lines(value: &str) -> String {
    value
        .lines()
        .map(|line| {
            if contains_credential_marker(line) {
                format!(
                    "[withheld: {} characters matched a credential pattern]",
                    line.chars().count()
                )
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn contains_credential_marker(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    CREDENTIAL_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

fn persist_completed_subagent_turn(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<TuiSessionContext>>,
    turn: CompletedSubagentTurn,
) -> Result<(), CliError> {
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    let provider = context.provider.map(|provider| match provider {
        TuiProvider::OpenAiApi => "openai-api".to_owned(),
        TuiProvider::OpenAiChatGpt => "openai-chatgpt".to_owned(),
    });
    let model = context
        .selection
        .as_ref()
        .map(|selection| selection.model().to_owned())
        .or_else(|| bootstrap.model().map(ToOwned::to_owned))
        .unwrap_or_else(|| default_model(bootstrap).to_owned());
    let active_agent = context
        .active_agent
        .as_ref()
        .map(|agent| agent.name.as_str());
    let metadata = next_session_metadata(
        bootstrap,
        &turn.task,
        context.metadata.as_ref(),
        active_agent,
        provider,
        model,
        None,
    )?;
    let mut store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let persisted_history = context
        .identifier
        .and_then(|identifier| store.load_session_for_resume(identifier).ok())
        .map(|session| session.messages);
    let call_id = next_subagent_call_id(persisted_history.as_deref().unwrap_or(&context.messages));
    let metadata = store
        .persist_completed_session_turn(
            &metadata,
            &completed_subagent_session_turn(&turn, &call_id)?,
        )
        .map_err(|_| CliError::storage("completed session could not be saved"))?;
    let messages = store
        .load_session_for_resume(metadata.id)
        .map_err(|_| CliError::storage("completed session could not be loaded"))?
        .messages;
    context.identifier = Some(metadata.id);
    context.metadata = Some(metadata);
    context.messages = messages;
    Ok(())
}

fn flush_parts(messages: &mut Vec<Message>, role: Role, parts: &mut Vec<MessagePart>) {
    if !parts.is_empty() {
        messages.push(Message {
            role,
            parts: std::mem::take(parts),
        });
    }
}

fn production_tool_runtime(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    production_tool_runtime_with_task_runner(
        bootstrap,
        project_root,
        skills,
        ProductionTaskRunner::new(bootstrap.clone(), project_root.to_path_buf()),
    )
}

fn production_tool_runtime_for_parent(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    parent_model: String,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    production_tool_runtime_with_parent_task_runner(
        bootstrap,
        project_root,
        skills,
        parent_model,
        parent_request_config,
        model_resolution_reference,
        ProductionTaskRunner::new(bootstrap.clone(), project_root.to_path_buf()),
    )
}

fn production_tool_runtime_with_task_runner<R: TaskRunner>(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    task_runner: R,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    let parent_model = bootstrap
        .model()
        .unwrap_or_else(|| default_model(bootstrap))
        .to_owned();
    production_tool_runtime_with_parent_task_runner(
        bootstrap,
        project_root,
        skills,
        parent_model,
        agens_core::RequestConfig::default(),
        None,
        task_runner,
    )
}

fn production_tool_runtime_with_parent_task_runner<R: TaskRunner>(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    parent_model: String,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
    task_runner: R,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    #[cfg(test)]
    PRODUCTION_TOOL_RUNTIME_CALLS.with(|calls| calls.set(calls.get() + 1));

    let native_catalog = Arc::new(Mutex::new(NativeToolCatalog::new(
        NativeTools::open(project_root)
            .map_err(|_| CliError::configuration("native tools are unavailable"))?,
    )));
    let mcp_registry = Arc::new(Mutex::new(load_configured_mcp_registry(
        bootstrap,
        project_root,
    )));
    let mut dispatcher = ToolDispatcher::new();
    let mut provider_tools = BTreeMap::new();
    let discovered_skills;
    let skills = match skills {
        Some(skills) => skills,
        None => {
            discovered_skills = discover_skill_catalog(bootstrap)?.catalog().clone();
            &discovered_skills
        }
    };

    for metadata in NativeToolCatalog::metadata() {
        let model_name = native_model_tool_name(&metadata.qualified_name)?;
        provider_tools.insert(
            model_name.clone(),
            OpenAiFunctionTool::new(model_name, metadata.description, metadata.input_schema)
                .map_err(|_| CliError::configuration("native tools are unavailable"))?,
        );
        dispatcher
            .register_native(
                metadata.qualified_name.clone(),
                metadata.access,
                RegisteredNativeTool {
                    name: metadata.qualified_name,
                    catalog: Arc::clone(&native_catalog),
                },
            )
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    }

    provider_tools.insert(
        "skill".into(),
        OpenAiFunctionTool::new(
            "skill",
            "Load selected skill instructions or a declared reference, script, or asset as text",
            SkillResourceTool::input_schema(),
        )
        .map_err(|_| CliError::configuration("skill tool is unavailable"))?,
    );
    dispatcher
        .register_native(
            "native::skill",
            agens_core::ToolAccess::ReadOnly,
            SkillResourceTool::new(skills.clone()),
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;

    register_production_task_tool(
        bootstrap,
        skills,
        &mut dispatcher,
        &mut provider_tools,
        TaskParentSelection {
            model: parent_model,
            request_config: parent_request_config,
            diagnostic_reference: model_resolution_reference,
        },
        task_runner,
    )?;

    let mut runtime = ProductionMcpRuntime {
        registry: mcp_registry,
        dispatcher: Arc::new(Mutex::new(dispatcher)),
    };
    let remote_tools = runtime.discover_configured_tools()?;

    for metadata in remote_tools {
        let model_name = mcp_model_tool_name(&metadata);
        provider_tools.insert(
            model_name.clone(),
            remote_function_tool(&metadata, model_name)?,
        );
    }

    Ok((provider_tools.into_values().collect(), runtime.dispatcher))
}

struct ProductionTuiTaskRuntime {
    provider_tools: Vec<OpenAiFunctionTool>,
    dispatcher: SharedToolDispatcher,
    task_registry: TaskExecutionRegistry,
    #[allow(dead_code)]
    authorized: AuthorizedNativeTaskRuntime<ProductionPermissionPrompter>,
}

struct TaskParentSelection {
    model: String,
    request_config: agens_core::RequestConfig,
    diagnostic_reference: Option<String>,
}

fn production_tui_task_runtime(
    bootstrap: &Bootstrap,
    skills: &SkillCatalog,
    permission_bridge: TuiPermissionBridge,
    lifecycle_bridge: TuiTaskLifecycleBridge,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: String,
) -> Result<ProductionTuiTaskRuntime, CliError> {
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    production_tui_task_runtime_with_runner_and_parent_config(
        bootstrap,
        skills,
        permission_bridge,
        ProductionTaskRunner::new(bootstrap.clone(), project_root.to_path_buf())
            .with_lifecycle_bridge(lifecycle_bridge),
        parent_request_config,
        Some(model_resolution_reference),
    )
}

#[cfg(test)]
fn production_tui_task_runtime_with_runner(
    bootstrap: &Bootstrap,
    skills: &SkillCatalog,
    permission_bridge: TuiPermissionBridge,
    task_runner: ProductionTaskRunner,
) -> Result<ProductionTuiTaskRuntime, CliError> {
    production_tui_task_runtime_with_runner_and_parent_config(
        bootstrap,
        skills,
        permission_bridge,
        task_runner,
        agens_core::RequestConfig::default(),
        None,
    )
}

fn production_tui_task_runtime_with_runner_and_parent_config(
    bootstrap: &Bootstrap,
    skills: &SkillCatalog,
    permission_bridge: TuiPermissionBridge,
    task_runner: ProductionTaskRunner,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
) -> Result<ProductionTuiTaskRuntime, CliError> {
    let task_registry = task_runner.execution_registry().unwrap_or_default();
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    let parent_model = bootstrap
        .model()
        .unwrap_or_else(|| default_model(bootstrap))
        .to_owned();
    let (provider_tools, dispatcher) = production_tool_runtime_with_parent_task_runner(
        bootstrap,
        project_root,
        Some(skills),
        parent_model,
        parent_request_config,
        model_resolution_reference,
        task_runner,
    )?;
    let project = project_root.display().to_string();
    let policy = permission_policy(
        bootstrap.permission_rules(),
        &project,
        PermissionMode::Edit,
        &dispatcher,
        None,
    )?;
    let grant_store = PermissionGrantStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("permission grants are unavailable"))?;
    let grants = grant_store
        .grants_for_project(&project)
        .map_err(|_| CliError::storage("permission grants are unavailable"))?;
    let grants = Arc::new(Mutex::new(grants));
    let session = PermissionSession::new();
    let pending = Arc::new(Mutex::new(BTreeMap::new()));
    let prompts = Arc::new(Mutex::new(BTreeMap::new()));
    let gate = ProductionPermissionGate::new(
        policy.clone(),
        Arc::clone(&grants),
        session,
        project.clone(),
        Arc::clone(&dispatcher),
        Arc::clone(&pending),
        Arc::clone(&prompts),
    );
    let resolver = ProductionPermissionResolver::new(
        ProductionPermissionPrompter::Tui(permission_bridge),
        grant_store,
        grants,
        prompts,
        ProductionPromptAuthorization {
            policy,
            session: PermissionSession::new(),
            project,
            dispatcher: Arc::clone(&dispatcher),
            allowed: Arc::clone(&pending),
        },
    );

    Ok(ProductionTuiTaskRuntime {
        provider_tools,
        dispatcher: Arc::clone(&dispatcher),
        task_registry,
        authorized: AuthorizedNativeTaskRuntime {
            gate,
            resolver,
            dispatcher: ProductionToolDispatcher::new(dispatcher, pending),
            next_call_id: 0,
        },
    })
}

fn register_production_task_tool<R: TaskRunner>(
    bootstrap: &Bootstrap,
    skills: &SkillCatalog,
    dispatcher: &mut ToolDispatcher,
    provider_tools: &mut BTreeMap<String, OpenAiFunctionTool>,
    parent: TaskParentSelection,
    task_runner: R,
) -> Result<(), CliError> {
    let available_models = task_model_catalog(bootstrap)?;
    let validator = TaskModelValidator::new(&available_models);
    let agents = tui_task_agent_catalog(bootstrap)?;
    if !agents
        .subagents()
        .any(|agent| agent.mode == agens_core::AgentMode::Subagent)
    {
        return Ok(());
    }

    let diagnostic_bootstrap = bootstrap.clone();
    let task = TaskTool::from_catalogs_with_parent_config(
        agents,
        skills.clone(),
        parent.model,
        parent.request_config,
        available_models,
        validator,
        task_runner,
    )
    .with_model_resolution_diagnostics(move |error| match error {
        TaskModelResolutionError::ModelUnavailable => {
            let reference = parent
                .diagnostic_reference
                .clone()
                .unwrap_or_else(next_diagnostic_reference);
            record_subagent_terminal(
                &diagnostic_bootstrap,
                &reference,
                ProviderDiagnosticClass::ModelUnavailable,
            );
            Some(reference)
        }
    });
    let input_schema = task.catalog_input_schema();
    let task_registry = task.execution_registry().clone();

    provider_tools.insert(
        "task".into(),
        OpenAiFunctionTool::new(
            "task",
            "Dispatch an isolated eligible subagent task in the foreground or background",
            input_schema,
        )
        .map_err(|_| CliError::configuration("task tool is unavailable"))?,
    );
    dispatcher
        .register_native("native::task", agens_core::ToolAccess::Write, task)
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;

    register_task_coordination_tools(
        dispatcher,
        provider_tools,
        task_registry,
        TaskMessageSource::Main,
    )
}

fn register_task_coordination_tools(
    dispatcher: &mut ToolDispatcher,
    provider_tools: &mut BTreeMap<String, OpenAiFunctionTool>,
    registry: TaskExecutionRegistry,
    source: TaskMessageSource,
) -> Result<(), CliError> {
    provider_tools.insert(
        "task_control".into(),
        OpenAiFunctionTool::new(
            "task_control",
            "Inspect, background, or cancel a live subagent execution",
            TaskControlTool::input_schema(),
        )
        .map_err(|_| CliError::configuration("task control tool is unavailable"))?,
    );
    provider_tools.insert(
        "task_message".into(),
        OpenAiFunctionTool::new(
            "task_message",
            "Queue a bounded coordination message for a live subagent or the main agent",
            TaskMessageTool::input_schema(),
        )
        .map_err(|_| CliError::configuration("task message tool is unavailable"))?,
    );
    dispatcher
        .register_native(
            "native::task_control",
            agens_core::ToolAccess::Write,
            TaskControlTool::new(registry.clone(), source),
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    dispatcher
        .register_native(
            "native::task_message",
            agens_core::ToolAccess::Write,
            TaskMessageTool::new(registry, source),
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))
}

fn default_model(bootstrap: &Bootstrap) -> &'static str {
    match bootstrap.provider_type() {
        Some("openai-chatgpt") => "gpt-5.5",
        _ => "gpt-4.1",
    }
}

struct ProductionTaskRunner {
    bootstrap: Bootstrap,
    project_root: PathBuf,
    dangerous_mode: bool,
    lifecycle_bridge: Option<TuiTaskLifecycleBridge>,
    task_registry: Option<TaskExecutionRegistry>,
    #[cfg(test)]
    probe: Option<ProductionTaskProbe>,
    #[cfg(test)]
    progress_probe: Option<Vec<TurnEvent>>,
    #[cfg(test)]
    failure_probe: Option<TestTaskFailure>,
}

#[cfg(test)]
type ProductionTaskProbe = Arc<
    Mutex<
        Vec<(
            agens_tools::TaskExecutionId,
            TaskLaunchMode,
            String,
            Option<agens_core::ReasoningEffort>,
        )>,
    >,
>;

#[cfg(test)]
struct TestTaskFailure {
    error: ChildRunError,
    _source_detail: String,
}

#[derive(Clone, Default)]
struct TuiTaskControls(TaskExecutionRegistry);

impl TuiTaskControls {
    fn transition_to_background(&self, id: u64) -> bool {
        self.0
            .transition_to_background(agens_tools::TaskExecutionId::from_value(id))
    }
}
#[derive(Clone)]
struct TuiTaskLifecycleBridge {
    events: BridgeTx<TuiRuntimeEvent>,
    controls: TuiTaskControls,
    lifecycle: Arc<Mutex<Option<TaskExecutionLifecycle>>>,
    terminal_results: Arc<Mutex<BTreeMap<u64, String>>>,
    completed_turns: Arc<Mutex<BTreeMap<u64, CompletedSubagentTurn>>>,
    persist_completed: Option<Arc<dyn Fn(CompletedSubagentTurn) -> bool + Send + Sync>>,
}

impl TuiTaskLifecycleBridge {
    fn new(events: BridgeTx<TuiRuntimeEvent>, controls: TuiTaskControls) -> Self {
        Self {
            events,
            controls,
            lifecycle: Arc::new(Mutex::new(None)),
            terminal_results: Arc::new(Mutex::new(BTreeMap::new())),
            completed_turns: Arc::new(Mutex::new(BTreeMap::new())),
            persist_completed: None,
        }
    }

    fn with_session_writer(
        mut self,
        bootstrap: Bootstrap,
        session: Arc<Mutex<TuiSessionContext>>,
    ) -> Self {
        let events = self.events.clone();
        self.persist_completed = Some(Arc::new(move |turn: CompletedSubagentTurn| {
            let id = turn.id;
            if persist_completed_subagent_turn(&bootstrap, &session, turn).is_err() {
                let _ = events.publish(
                    TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::error(
                        id,
                        TuiSubagentErrorKind::Runtime,
                    )),
                    &BridgeCancel::new(),
                    None,
                );
                return false;
            }
            true
        }));
        self
    }

    fn mode(&self) -> Option<TaskLaunchMode> {
        let lifecycle = self.lifecycle.lock().ok()?;
        Some(lifecycle.as_ref()?.mode())
    }

    fn observe(&self, request: &TaskTurnRequest, lifecycle: TaskExecutionLifecycle) {
        let id = lifecycle.id().value();
        if let Ok(mut current) = self.lifecycle.lock() {
            *current = Some(lifecycle.clone());
        }
        let presentation = match lifecycle.mode() {
            TaskLaunchMode::Foreground => agens_tui::TuiExecutionState::ForegroundRunning,
            TaskLaunchMode::Background => agens_tui::TuiExecutionState::BackgroundRunning,
        };
        let agent = request.agent_name().to_owned();
        if let Ok(mut turns) = self.completed_turns.lock() {
            turns.insert(
                id,
                CompletedSubagentTurn {
                    id,
                    agent: agent.clone(),
                    task: request.description().to_owned(),
                    final_result: String::new(),
                    tool_uses: 0,
                },
            );
        }
        self.publish(TuiRuntimeEvent::TaskExecution {
            agent: agent.clone(),
            event: match lifecycle.mode() {
                TaskLaunchMode::Foreground => TuiExecutionEvent::ForegroundStarted { id },
                TaskLaunchMode::Background => TuiExecutionEvent::BackgroundStarted { id },
            },
        });
        self.publish(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::started(id, &agent, request.description(), presentation),
        ));
        let events = self.events.clone();
        let registry = self.controls.0.clone();
        let terminal_results = Arc::clone(&self.terminal_results);
        let completed_turns = Arc::clone(&self.completed_turns);
        let persist_completed = self.persist_completed.clone();
        std::thread::spawn(move || {
            let mut seen = 1;
            loop {
                let lifecycle_events = lifecycle.events();
                for event in &lifecycle_events[seen..] {
                    let event = match *event {
                        TaskExecutionEvent::Admitted(_, TaskLaunchMode::Foreground) => {
                            TuiExecutionEvent::ForegroundStarted { id }
                        }
                        TaskExecutionEvent::Admitted(_, TaskLaunchMode::Background) => {
                            TuiExecutionEvent::BackgroundStarted { id }
                        }
                        TaskExecutionEvent::Backgrounded(_) => {
                            TuiExecutionEvent::Backgrounded { id }
                        }
                        TaskExecutionEvent::Completed(_) => TuiExecutionEvent::Completed { id },
                        TaskExecutionEvent::Failed(_) => TuiExecutionEvent::Failed { id },
                        TaskExecutionEvent::Cancelled(_) => TuiExecutionEvent::Cancelled { id },
                    };
                    let _ = events.publish(
                        TuiRuntimeEvent::TaskExecution {
                            agent: agent.clone(),
                            event,
                        },
                        &BridgeCancel::new(),
                        None,
                    );
                    if matches!(
                        event,
                        TuiExecutionEvent::Completed { .. }
                            | TuiExecutionEvent::Failed { .. }
                            | TuiExecutionEvent::Cancelled { .. }
                    ) {
                        let (status, fallback) = match event {
                            TuiExecutionEvent::Completed { .. } => {
                                (TuiSubagentStatus::Success, "completed")
                            }
                            TuiExecutionEvent::Failed { .. } => {
                                (TuiSubagentStatus::Failure, "failed")
                            }
                            TuiExecutionEvent::Cancelled { .. } => {
                                (TuiSubagentStatus::Cancelled, "cancelled")
                            }
                            _ => unreachable!("terminal event was matched above"),
                        };
                        let final_result = terminal_results
                            .lock()
                            .ok()
                            .and_then(|mut results| results.remove(&id))
                            .unwrap_or_else(|| fallback.into());
                        let _ = events.publish(
                            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::terminal(
                                id,
                                status,
                                final_result,
                            )),
                            &BridgeCancel::new(),
                            None,
                        );
                        let completed_turn = completed_turns
                            .lock()
                            .ok()
                            .and_then(|mut turns| turns.remove(&id));
                        let persisted = matches!(event, TuiExecutionEvent::Completed { .. })
                            && match (completed_turn, &persist_completed) {
                                (Some(turn), Some(persist)) => persist(turn),
                                _ => false,
                            };
                        notify_main_of_terminal_subagent(
                            &registry, &events, id, &agent, fallback, persisted,
                        );
                        return;
                    }
                }
                seen = lifecycle_events.len();
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
    }

    fn observe_progress(&self, id: u64, event: TurnEvent) {
        if matches!(event, TurnEvent::ToolCallRequested { .. })
            && let Ok(mut turns) = self.completed_turns.lock()
            && let Some(turn) = turns.get_mut(&id)
        {
            turn.tool_uses += 1;
        }
        let event = match event {
            TurnEvent::ProviderPart(MessagePart::Reasoning(delta)) => {
                TuiSubagentEvent::reasoning(id, delta)
            }
            TurnEvent::ProviderPart(MessagePart::Text(delta)) => TuiSubagentEvent::text(id, delta),
            TurnEvent::ToolCallRequested {
                id: call_id,
                name,
                input,
            } => {
                // Parse the sanitized input so a redacted secret never
                // survives inside `parsed`'s `Other { raw, .. }` fallback.
                let sanitized_input = sanitize_tui_metric(&input);
                let parsed = agens_core::ToolInput::parse(&name, &sanitized_input);
                TuiSubagentEvent::tool_call_with_parsed(id, call_id, name, input, parsed)
            }
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id,
                content,
                is_error,
            }) => TuiSubagentEvent::tool_result(id, tool_call_id, content, is_error),
            _ => return,
        };
        self.publish(TuiRuntimeEvent::SubagentExecution(event));
    }

    fn record_terminal_result(&self, id: u64, result: Result<&str, &TaskRunnerError>) {
        let final_result = match result {
            Ok(result) => result.into(),
            Err(TaskRunnerError::Cancelled) => "cancelled".into(),
            Err(_) => "failed".into(),
        };
        if let Ok(mut results) = self.terminal_results.lock() {
            results.insert(id, final_result);
        }
        if let Ok(result) = result
            && let Ok(mut turns) = self.completed_turns.lock()
            && let Some(turn) = turns.get_mut(&id)
        {
            turn.final_result = result.into();
        }
    }

    fn publish(&self, event: TuiRuntimeEvent) {
        let _ = self.events.publish(event, &BridgeCancel::new(), None);
    }
}

/// A finished background subagent is otherwise only shown to the user, never told to the model.
/// The notice stays a pointer rather than the payload: the persisted turn remains the source of
/// truth, and the bounded untrusted mailbox delivers this on the next main turn without starting
/// one.
fn notify_main_of_terminal_subagent(
    registry: &TaskExecutionRegistry,
    events: &BridgeTx<TuiRuntimeEvent>,
    id: u64,
    agent: &str,
    state: &str,
    persisted: bool,
) {
    let record = if persisted {
        "The full result is recorded in this session history"
    } else {
        "No durable result was recorded"
    };
    let notice = format!(
        "subagent #{id} ({agent}) finished with state={state} completed_at={} (unix seconds). \
         {record}; run task_control action=status id={id} for the recorded outcome.",
        current_session_timestamp()
    );

    if registry
        .notify_main(agens_tools::TaskExecutionId::from_value(id), notice)
        .is_err()
    {
        let _ = events.publish(
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::error(
                id,
                TuiSubagentErrorKind::Runtime,
            )),
            &BridgeCancel::new(),
            None,
        );
    }
}

impl ProductionTaskRunner {
    fn new(bootstrap: Bootstrap, project_root: PathBuf) -> Self {
        Self {
            bootstrap,
            project_root,
            dangerous_mode: false,
            lifecycle_bridge: None,
            task_registry: None,
            #[cfg(test)]
            probe: None,
            #[cfg(test)]
            progress_probe: None,
            #[cfg(test)]
            failure_probe: None,
        }
    }

    fn with_lifecycle_bridge(mut self, lifecycle_bridge: TuiTaskLifecycleBridge) -> Self {
        self.task_registry = Some(lifecycle_bridge.controls.0.clone());
        self.lifecycle_bridge = Some(lifecycle_bridge);
        self
    }

    fn with_dangerous_mode(mut self, dangerous_mode: bool) -> Self {
        self.dangerous_mode = dangerous_mode;
        self
    }
    #[cfg(test)]
    fn with_probe(bootstrap: Bootstrap, project_root: PathBuf, probe: ProductionTaskProbe) -> Self {
        Self {
            bootstrap,
            project_root,
            dangerous_mode: false,
            lifecycle_bridge: None,
            task_registry: None,
            probe: Some(probe),
            progress_probe: None,
            failure_probe: None,
        }
    }

    #[cfg(test)]
    fn with_progress_probe(
        bootstrap: Bootstrap,
        project_root: PathBuf,
        probe: ProductionTaskProbe,
        progress: Vec<TurnEvent>,
    ) -> Self {
        Self {
            bootstrap,
            project_root,
            dangerous_mode: false,
            lifecycle_bridge: None,
            task_registry: None,
            probe: Some(probe),
            progress_probe: Some(progress),
            failure_probe: None,
        }
    }

    #[cfg(test)]
    fn with_failure_probe(
        bootstrap: Bootstrap,
        project_root: PathBuf,
        error: ChildRunError,
        source_detail: &str,
    ) -> Self {
        Self {
            bootstrap,
            project_root,
            dangerous_mode: false,
            lifecycle_bridge: None,
            task_registry: None,
            probe: None,
            progress_probe: None,
            failure_probe: Some(TestTaskFailure {
                error,
                _source_detail: source_detail.into(),
            }),
        }
    }
}

impl TaskRunner for ProductionTaskRunner {
    fn execution_registry(&self) -> Option<TaskExecutionRegistry> {
        self.task_registry.clone()
    }

    fn run(
        &self,
        request: TaskTurnRequest,
        context: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        if let (Some(lifecycle_bridge), Some(execution)) =
            (&self.lifecycle_bridge, context.execution())
        {
            lifecycle_bridge.observe(&request, execution.clone());
        }
        #[cfg(test)]
        if let Some(probe) = &self.probe {
            let execution = context
                .execution()
                .expect("registered task has execution context");
            probe.lock().expect("task probe lock").push((
                execution.id(),
                execution.mode(),
                request.model().to_owned(),
                request.request_config().reasoning_effort(),
            ));
            if let (Some(lifecycle_bridge), Some(execution), Some(progress)) = (
                &self.lifecycle_bridge,
                context.execution(),
                &self.progress_probe,
            ) {
                for event in progress.iter().cloned() {
                    lifecycle_bridge.observe_progress(execution.id().value(), event);
                }
            }
            if self.progress_probe.is_some() {
                let result = TaskTurnResult {
                    output: "probe".into(),
                    iterations: 1,
                };
                if let Some(lifecycle_bridge) = &self.lifecycle_bridge {
                    lifecycle_bridge
                        .record_terminal_result(execution.id().value(), Ok(&result.output));
                }
                return Ok(result);
            }
            if self.lifecycle_bridge.is_some() {
                while !context
                    .cancellation
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    if context
                        .execution()
                        .is_some_and(|execution| execution.mode() == TaskLaunchMode::Background)
                    {
                        return Ok(TaskTurnResult {
                            output: "probe".into(),
                            iterations: 1,
                        });
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                return Err(TaskRunnerError::Cancelled);
            }
            return Ok(TaskTurnResult {
                output: "probe".into(),
                iterations: 1,
            });
        }
        let cancellation = HeadlessTurnCancellation::with_cancellation_and_deadline(
            Arc::clone(&context.cancellation),
            None,
        );
        let progress = self.lifecycle_bridge.as_ref().zip(context.execution()).map(
            |(lifecycle_bridge, execution)| {
                let lifecycle_bridge = lifecycle_bridge.clone();
                let id = execution.id().value();
                Arc::new(move |event| lifecycle_bridge.observe_progress(id, event))
                    as TurnProgressSink
            },
        );
        let diagnostic_reference = next_diagnostic_reference();
        #[cfg(test)]
        let diagnostic_reference = if self.failure_probe.is_some() {
            "abc12345".into()
        } else {
            diagnostic_reference
        };
        #[cfg(test)]
        let result = self
            .failure_probe
            .as_ref()
            .map(|failure| Err(failure.error))
            .unwrap_or_else(|| {
                run_production_task(
                    request,
                    ProductionTaskExecutionContext {
                        bootstrap: &self.bootstrap,
                        project_root: &self.project_root,
                        dangerous_mode: self.dangerous_mode,
                        cancellation: &cancellation,
                        progress: progress.as_ref(),
                        diagnostic_reference: &diagnostic_reference,
                        task_registry: context.execution_registry(),
                        execution_id: context.execution().expect("registered task execution").id(),
                    },
                )
            });
        #[cfg(not(test))]
        let result = run_production_task(
            request,
            ProductionTaskExecutionContext {
                bootstrap: &self.bootstrap,
                project_root: &self.project_root,
                dangerous_mode: self.dangerous_mode,
                cancellation: &cancellation,
                progress: progress.as_ref(),
                diagnostic_reference: &diagnostic_reference,
                task_registry: context.execution_registry(),
                execution_id: context.execution().expect("registered task execution").id(),
            },
        );
        if let Err(error) = &result {
            record_subagent_terminal(
                &self.bootstrap,
                &diagnostic_reference,
                error.diagnostic_class(),
            );
        }
        if let (Some(lifecycle_bridge), Some(execution), Err(error)) =
            (&self.lifecycle_bridge, context.execution(), &result)
            && let Some(kind) = error.tui_kind()
        {
            lifecycle_bridge.publish(TuiRuntimeEvent::SubagentExecution(
                TuiSubagentEvent::error_with_reference(
                    execution.id().value(),
                    kind,
                    &diagnostic_reference,
                ),
            ));
        }
        let result = result.map_err(ChildRunError::task_runner_error);
        if let (Some(lifecycle_bridge), Some(execution)) =
            (&self.lifecycle_bridge, context.execution())
        {
            lifecycle_bridge.record_terminal_result(
                execution.id().value(),
                result.as_ref().map(String::as_str),
            );
        }
        result.map(|output| TaskTurnResult {
            output,
            iterations: 1,
        })
    }
}

#[cfg(test)]
fn map_task_turn_error(error: HeadlessTurnError) -> TaskRunnerError {
    match error {
        HeadlessTurnError::Cancelled => TaskRunnerError::Cancelled,
        HeadlessTurnError::TimedOut => TaskRunnerError::TimedOut,
        HeadlessTurnError::Provider
        | HeadlessTurnError::ProviderRejected
        | HeadlessTurnError::ProviderRateLimited
        | HeadlessTurnError::ProviderServer
        | HeadlessTurnError::ProviderProtocol => TaskRunnerError::ProviderFailure,
        HeadlessTurnError::MaxIterations => TaskRunnerError::IterationLimit,
        _ => TaskRunnerError::ChildFailure,
    }
}

#[derive(Clone, Copy)]
enum ChildRunError {
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
    const fn diagnostic_class(self) -> ProviderDiagnosticClass {
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

    const fn tui_kind(self) -> Option<TuiSubagentErrorKind> {
        match self {
            Self::Cancelled | Self::TimedOut => None,
            Self::Authentication => Some(TuiSubagentErrorKind::Authentication),
            Self::Context => Some(TuiSubagentErrorKind::Context),
            Self::Network => Some(TuiSubagentErrorKind::Network),
            Self::Provider => Some(TuiSubagentErrorKind::Provider),
            Self::Protocol => Some(TuiSubagentErrorKind::Protocol),
            Self::RateLimited => Some(TuiSubagentErrorKind::RateLimited),
            Self::Rejected => Some(TuiSubagentErrorKind::Rejected),
            Self::Server => Some(TuiSubagentErrorKind::Server),
            Self::Tool => Some(TuiSubagentErrorKind::Tool),
            Self::IterationLimit | Self::Runtime => Some(TuiSubagentErrorKind::Runtime),
        }
    }

    const fn task_runner_error(self) -> TaskRunnerError {
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

struct ProductionTaskExecutionContext<'a> {
    bootstrap: &'a Bootstrap,
    project_root: &'a Path,
    dangerous_mode: bool,
    cancellation: &'a HeadlessTurnCancellation,
    progress: Option<&'a TurnProgressSink>,
    diagnostic_reference: &'a str,
    task_registry: &'a TaskExecutionRegistry,
    execution_id: agens_tools::TaskExecutionId,
}

fn run_production_task(
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
        dangerous_mode,
        task_registry.clone(),
        execution_id,
    )
    .map_err(|_| ChildRunError::Runtime)?;
    let diagnostic_store = SafeDiagnosticStore::new(bootstrap.data_directory().to_path_buf());
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
            let provider =
                OpenAiResponsesProvider::from_api_key_with_messages_and_tools_and_timeout(
                    api_key,
                    bootstrap.provider_base_url(),
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
            let provider = ChatGptResponsesProvider::from_credentials_with_messages_and_tools_and_timeout_and_auth_url(
                &bootstrap.paths.credentials,
                bootstrap.provider_base_url(),
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

struct TaskMailboxProvider<P> {
    inner: P,
    registry: Option<TaskExecutionRegistry>,
    target: TaskMessageTarget,
}

impl<P> TaskMailboxProvider<P> {
    fn new(inner: P, registry: Option<TaskExecutionRegistry>, target: TaskMessageTarget) -> Self {
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

fn production_read_only_tool_runtime(
    project_root: &Path,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    let catalog = Arc::new(Mutex::new(NativeToolCatalog::new(
        NativeTools::open(project_root)
            .map_err(|_| CliError::configuration("native tools are unavailable"))?,
    )));
    let metadata = NativeToolCatalog::metadata()
        .into_iter()
        .find(|metadata| metadata.qualified_name == "native::read")
        .ok_or_else(|| CliError::configuration("native read tool is unavailable"))?;
    let name = native_model_tool_name(&metadata.qualified_name)?;
    let tool = OpenAiFunctionTool::new(name.clone(), metadata.description, metadata.input_schema)
        .map_err(|_| CliError::configuration("native tools are unavailable"))?;
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::read",
            metadata.access,
            RegisteredNativeTool {
                name: "native::read".into(),
                catalog,
            },
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;

    Ok((vec![tool], Arc::new(Mutex::new(dispatcher))))
}

const DANGEROUS_CHILD_NATIVE_TOOLS: [&str; 9] = [
    "native::read",
    "native::list",
    "native::search",
    "native::glob",
    "native::grep",
    "native::write",
    "native::edit",
    "native::bash",
    "native::webfetch",
];

fn production_dangerous_child_tool_runtime(
    project_root: &Path,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    let catalog = Arc::new(Mutex::new(NativeToolCatalog::new(
        NativeTools::open(project_root)
            .map_err(|_| CliError::configuration("native tools are unavailable"))?,
    )));
    let metadata = NativeToolCatalog::metadata();
    let mut provider_tools = Vec::with_capacity(DANGEROUS_CHILD_NATIVE_TOOLS.len());
    let mut dispatcher = ToolDispatcher::new();

    for name in DANGEROUS_CHILD_NATIVE_TOOLS {
        let metadata = metadata
            .iter()
            .find(|metadata| metadata.qualified_name == name)
            .ok_or_else(|| CliError::configuration("dangerous child native tool is unavailable"))?;
        let model_name = native_model_tool_name(&metadata.qualified_name)?;
        provider_tools.push(
            OpenAiFunctionTool::new(
                model_name,
                metadata.description.clone(),
                metadata.input_schema.clone(),
            )
            .map_err(|_| CliError::configuration("native tools are unavailable"))?,
        );
        dispatcher
            .register_native(
                metadata.qualified_name.clone(),
                metadata.access,
                RegisteredNativeTool {
                    name: metadata.qualified_name.clone(),
                    catalog: Arc::clone(&catalog),
                },
            )
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    }

    Ok((provider_tools, Arc::new(Mutex::new(dispatcher))))
}

fn production_child_tool_runtime(
    project_root: &Path,
    dangerous_mode: bool,
    task_registry: TaskExecutionRegistry,
    execution_id: agens_tools::TaskExecutionId,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    let (mut provider_tools, dispatcher) = if dangerous_mode {
        production_dangerous_child_tool_runtime(project_root)
    } else {
        production_read_only_tool_runtime(project_root)
    }?;
    provider_tools.push(
        OpenAiFunctionTool::new(
            "task_control",
            "Inspect, background, or cancel this subagent execution",
            TaskControlTool::input_schema(),
        )
        .map_err(|_| CliError::configuration("task control tool is unavailable"))?,
    );
    provider_tools.push(
        OpenAiFunctionTool::new(
            "task_message",
            "Queue a bounded coordination message for the main agent",
            TaskMessageTool::input_schema(),
        )
        .map_err(|_| CliError::configuration("task message tool is unavailable"))?,
    );
    let mut dispatcher_guard = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is unavailable"))?;
    dispatcher_guard
        .register_native(
            "native::task_control",
            agens_core::ToolAccess::Write,
            TaskControlTool::new(
                task_registry.clone(),
                TaskMessageSource::Execution(execution_id),
            ),
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    dispatcher_guard
        .register_native(
            "native::task_message",
            agens_core::ToolAccess::Write,
            TaskMessageTool::new(task_registry, TaskMessageSource::Execution(execution_id)),
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    drop(dispatcher_guard);

    Ok((provider_tools, dispatcher))
}

struct ProductionMcpRuntime {
    registry: Arc<Mutex<McpRegistry>>,
    dispatcher: SharedToolDispatcher,
}

impl ProductionMcpRuntime {
    fn discover_configured_tools(&mut self) -> Result<Vec<RemoteToolMetadata>, CliError> {
        let servers = self
            .registry
            .lock()
            .map_err(|_| CliError::configuration("MCP tools are unavailable"))?
            .configured_server_names();

        for server in servers {
            let _ = self.discover_server(&server)?;
        }

        self.tools()
    }

    fn discover_server(&mut self, server: &str) -> Result<agens_tools::McpServerReport, CliError> {
        let mut dispatcher = self
            .dispatcher
            .lock()
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| CliError::configuration("MCP tools are unavailable"))?;
        let report = registry.discover_server(server);
        if !report.is_failed() {
            synchronize_server_dispatcher(&mut dispatcher, &registry, &self.registry, server)?;
        }
        Ok(report)
    }

    #[allow(dead_code)]
    fn reload_server(&mut self, server: &str) -> Result<agens_tools::McpServerReport, CliError> {
        let mut dispatcher = self
            .dispatcher
            .lock()
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| CliError::configuration("MCP tools are unavailable"))?;
        let report = registry.reload_server(server);
        if !report.is_failed() {
            synchronize_server_dispatcher(&mut dispatcher, &registry, &self.registry, server)?;
        }
        Ok(report)
    }

    #[allow(dead_code)]
    fn diagnostics(&self) -> Result<Vec<agens_tools::McpServerDiagnostic>, CliError> {
        Ok(self
            .registry
            .lock()
            .map_err(|_| CliError::configuration("MCP tools are unavailable"))?
            .diagnostics()
            .into_iter()
            .cloned()
            .collect())
    }

    fn tools(&self) -> Result<Vec<RemoteToolMetadata>, CliError> {
        Ok(self
            .registry
            .lock()
            .map_err(|_| CliError::configuration("MCP tools are unavailable"))?
            .tools()
            .into_iter()
            .cloned()
            .collect())
    }
}

fn synchronize_server_dispatcher(
    dispatcher: &mut ToolDispatcher,
    registry: &McpRegistry,
    shared_registry: &Arc<Mutex<McpRegistry>>,
    server: &str,
) -> Result<(), CliError> {
    let tools = registry
        .tools()
        .into_iter()
        .filter(|tool| tool.server_name == server)
        .cloned()
        .collect::<Vec<_>>();

    dispatcher.remove_mcp_server(server);
    for metadata in tools {
        dispatcher
            .register_mcp(
                &metadata,
                RegisteredMcpTool {
                    name: metadata.qualified_name.clone(),
                    registry: Arc::clone(shared_registry),
                },
            )
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    }
    Ok(())
}

fn load_configured_mcp_registry(bootstrap: &Bootstrap, project_root: &Path) -> McpRegistry {
    let mut registry = bootstrap
        .mcp_status
        .clone()
        .map_or_else(McpRegistry::new, McpRegistry::with_status_handle);

    for server in &bootstrap.mcp_servers {
        let descriptor = mcp_server_descriptor(server);
        if server.disabled {
            let _ = registry.register_disabled_server(descriptor);
            continue;
        }
        let timeout = std::time::Duration::from_millis(server.timeout_ms);
        let Ok(timeouts) = McpTimeouts::new(timeout, timeout, timeout) else {
            continue;
        };

        let server = server.clone();
        let project_root = project_root.to_path_buf();
        let _ = registry.configure_server_with_descriptor(
            descriptor,
            move || configured_mcp_transport(&server, &project_root),
            timeouts,
            McpLimits::default(),
        );
    }

    registry
}

fn mcp_server_descriptor(server: &agens_config::McpServerConfig) -> McpServerDescriptor {
    let transport = match server.transport {
        McpTransport::Stdio => McpServerTransport::Stdio,
        McpTransport::Http => McpServerTransport::Http,
        McpTransport::Sse => McpServerTransport::Sse,
    };
    let endpoint = match server.transport {
        McpTransport::Stdio => server.command.as_ref().map(McpEndpointSummary::stdio),
        McpTransport::Http | McpTransport::Sse => server
            .url
            .as_deref()
            .and_then(|url| McpEndpointSummary::remote(url).ok()),
    };
    McpServerDescriptor::new(
        &server.name,
        McpServerSource::Global,
        transport,
        !server.disabled,
        std::time::Duration::from_millis(server.timeout_ms),
        endpoint,
    )
}

fn configured_mcp_transport(
    server: &agens_config::McpServerConfig,
    project_root: &Path,
) -> Result<Box<dyn McpTransportPort>, McpTransportError> {
    match server.transport {
        McpTransport::Stdio => McpStdioTransport::spawn(McpStdioTransportConfig {
            command: server
                .command
                .clone()
                .expect("stdio MCP commands are validated"),
            args: server.args.clone(),
            environment: server.environment.clone(),
            project_root: server
                .cwd
                .clone()
                .unwrap_or_else(|| project_root.to_path_buf()),
        })
        .map(|transport| Box::new(transport) as Box<dyn McpTransportPort>),
        McpTransport::Http => McpHttpTransport::new(
            server.url.clone().expect("HTTP MCP URLs are validated"),
            server.headers.clone(),
            server.max_retries,
        )
        .map(|transport| Box::new(transport) as Box<dyn McpTransportPort>),
        McpTransport::Sse => McpSseTransport::new(
            server.url.clone().expect("SSE MCP URLs are validated"),
            server.headers.clone(),
            server.max_retries,
        )
        .map(|transport| Box::new(transport) as Box<dyn McpTransportPort>),
    }
}

fn native_model_tool_name(qualified_name: &str) -> Result<String, CliError> {
    qualified_name
        .strip_prefix("native::")
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| CliError::configuration("native tool metadata is invalid"))
}

fn mcp_model_tool_name(metadata: &RemoteToolMetadata) -> String {
    format!("{}_{}", metadata.server_name, metadata.tool_name)
}

fn remote_function_tool(
    metadata: &RemoteToolMetadata,
    model_name: String,
) -> Result<OpenAiFunctionTool, CliError> {
    OpenAiFunctionTool::new(
        model_name,
        metadata
            .description
            .clone()
            .unwrap_or_else(|| "MCP tool".to_owned()),
        metadata.input_schema.clone(),
    )
    .map_err(|_| CliError::configuration("MCP tool metadata is invalid"))
}

#[derive(Debug, PartialEq, Eq)]
enum NativePermissionTarget {
    Command(String),
    Path(String),
    Pattern(String),
    Url(String),
}

#[derive(Debug, PartialEq, Eq)]
enum NativePermissionTargetError {
    UnknownTool,
    ArgumentsNotObject,
    InvalidField(&'static str),
    FieldTooLong(&'static str),
}

impl fmt::Display for NativePermissionTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTool => formatter.write_str("unknown native tool"),
            Self::ArgumentsNotObject => {
                formatter.write_str("native tool arguments must be an object")
            }
            Self::InvalidField(field) => write!(formatter, "native tool {field} is invalid"),
            Self::FieldTooLong(field) => {
                write!(formatter, "native tool {field} exceeds size limit")
            }
        }
    }
}

impl NativePermissionTarget {
    fn parse(
        tool: &str,
        arguments: &serde_json::Value,
    ) -> Result<Self, NativePermissionTargetError> {
        let arguments = arguments
            .as_object()
            .ok_or(NativePermissionTargetError::ArgumentsNotObject)?;

        let field = |field| native_permission_target_field(arguments, field);

        match tool {
            "native::bash" => field("command").map(Self::Command),
            "native::read" | "native::write" | "native::edit" | "native::list"
            | "native::search" => field("path").map(Self::Path),
            "native::glob" => field("pattern").map(Self::Pattern),
            "native::grep" => {
                if arguments.contains_key("path") {
                    field("path")?;
                }

                field("pattern").map(Self::Pattern)
            }
            "native::webfetch" => field("url").map(Self::Url),
            _ => Err(NativePermissionTargetError::UnknownTool),
        }
    }

    fn into_value(self) -> String {
        match self {
            Self::Command(value) | Self::Path(value) | Self::Pattern(value) | Self::Url(value) => {
                value
            }
        }
    }
}

fn native_permission_target_field(
    arguments: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<String, NativePermissionTargetError> {
    let value = arguments
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or(NativePermissionTargetError::InvalidField(field))?;

    if value.trim().is_empty() {
        return Err(NativePermissionTargetError::InvalidField(field));
    }

    if value.len() > agens_core::MAX_PERMISSION_TARGET_BYTES {
        return Err(NativePermissionTargetError::FieldTooLong(field));
    }

    Ok(value.to_owned())
}

trait ParseToolInput: Sized {
    fn parse(name: &str, raw: &str) -> Self;
}

impl ParseToolInput for agens_core::ToolInput {
    fn parse(name: &str, raw: &str) -> Self {
        let fallback = || Self::Other {
            name: name.to_owned(),
            raw: raw.to_owned(),
        };

        let Ok(serde_json::Value::Object(arguments)) = serde_json::from_str(raw) else {
            return fallback();
        };

        let field = |field| native_permission_target_field(&arguments, field).ok();

        match name {
            "read" => field("path").map(|path| Self::Read { path }),
            "write" => field("path").map(|path| Self::Write { path }),
            "edit" => field("path").map(|path| Self::Edit { path }),
            "list" => field("path").map(|path| Self::List { path }),
            "search" => field("path").map(|path| Self::Search { path }),
            "glob" => field("pattern").map(|pattern| Self::Glob {
                pattern,
                path: field("path"),
            }),
            "grep" => field("pattern").map(|pattern| Self::Grep {
                pattern,
                path: field("path"),
            }),
            "bash" => field("command").map(|command| Self::Bash { command }),
            "webfetch" => field("url").map(|url| Self::WebFetch { url }),
            "skill" => field("skill").map(|skill| Self::Skill { skill }),
            _ => None,
        }
        .unwrap_or_else(fallback)
    }
}

struct RegisteredNativeTool {
    name: String,
    catalog: Arc<Mutex<NativeToolCatalog>>,
}

impl DispatchTool for RegisteredNativeTool {
    fn permission_target(
        &self,
        arguments: &serde_json::Value,
    ) -> Result<String, agens_core::Error> {
        NativePermissionTarget::parse(&self.name, arguments)
            .map(NativePermissionTarget::into_value)
            .map_err(|error| agens_core::Error::Tool(error.to_string()))
    }

    fn execute(
        &mut self,
        context: &ToolExecutionContext,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, agens_core::Error> {
        self.catalog
            .lock()
            .map_err(|_| agens_core::Error::Tool("native tool catalog is unavailable".into()))?
            .execute(&self.name, arguments, context)
    }
}

struct RegisteredMcpTool {
    name: String,
    registry: Arc<Mutex<McpRegistry>>,
}

impl DispatchTool for RegisteredMcpTool {
    fn permission_target(&self, _: &serde_json::Value) -> Result<String, agens_core::Error> {
        Ok(self.name.clone())
    }

    fn execute(
        &mut self,
        context: &ToolExecutionContext,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, agens_core::Error> {
        self.registry
            .lock()
            .map_err(|_| agens_core::Error::Tool("MCP tool registry is unavailable".into()))?
            .call_tool(&self.name, arguments, context)
    }
}

fn cancellation_result(cancellation: &HeadlessTurnCancellation) -> Result<(), CliError> {
    if cancellation.is_cancelled() {
        return Err(CliError::runtime(HeadlessTurnError::Cancelled));
    }
    if cancellation.is_expired() {
        return Err(CliError::runtime(HeadlessTurnError::TimedOut));
    }
    Ok(())
}

struct AllowedNativeCall {
    name: String,
    input: String,
    handle: AuthorizedToolCall,
}

type SharedToolDispatcher = Arc<Mutex<ToolDispatcher>>;
type SharedProjectPermissionGrants = Arc<Mutex<Vec<agens_core::ProjectPermissionGrant>>>;
type PendingPermissionPrompts = Arc<Mutex<BTreeMap<String, PermissionPromptContext>>>;

struct ProductionPermissionGate {
    policy: PermissionPolicy,
    grants: SharedProjectPermissionGrants,
    session: PermissionSession,
    project: String,
    dispatcher: SharedToolDispatcher,
    allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
    prompts: PendingPermissionPrompts,
    dangerous_override: bool,
}

impl ProductionPermissionGate {
    fn new(
        policy: PermissionPolicy,
        grants: SharedProjectPermissionGrants,
        session: PermissionSession,
        project: String,
        dispatcher: SharedToolDispatcher,
        allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
        prompts: PendingPermissionPrompts,
    ) -> Self {
        Self {
            policy,
            grants,
            session,
            project,
            dispatcher,
            allowed,
            prompts,
            dangerous_override: false,
        }
    }

    fn with_dangerous_override(mut self, dangerous_override: bool) -> Self {
        self.dangerous_override = dangerous_override;
        self
    }
}

fn is_dangerous_child_native_tool(name: &str) -> bool {
    DANGEROUS_CHILD_NATIVE_TOOLS.iter().any(|registered| {
        name == *registered || name == registered.strip_prefix("native::").unwrap_or_default()
    })
}

impl HeadlessPermissionGate for ProductionPermissionGate {
    fn evaluate(
        &mut self,
        call: &HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send
    {
        let result = self
            .grants
            .lock()
            .map_err(|_| HeadlessTurnPortError::Permission)
            .and_then(|grants| {
                self.dispatcher
                    .lock()
                    .map_err(|_| HeadlessTurnPortError::Permission)
                    .and_then(|dispatcher| {
                        dispatcher
                            .evaluate_with_policy_override(
                                &self.policy,
                                &grants,
                                &self.session,
                                ToolDispatchRequest::new(
                                    &self.project,
                                    &call.name,
                                    parse_tool_input(call)?,
                                ),
                                self.dangerous_override
                                    && is_dangerous_child_native_tool(&call.name),
                            )
                            .map_err(|_| HeadlessTurnPortError::Permission)
                    })
            })
            .and_then(|outcome| match outcome {
                ToolEvaluationOutcome::Authorized(handle) => self
                    .allowed
                    .lock()
                    .map_err(|_| HeadlessTurnPortError::Permission)
                    .map(|mut allowed| {
                        allowed.insert(
                            call.id.clone(),
                            AllowedNativeCall {
                                name: call.name.clone(),
                                input: call.input.clone(),
                                handle,
                            },
                        );
                        PermissionDecision::Allow
                    }),
                ToolEvaluationOutcome::Denied => Ok(PermissionDecision::Deny),
                ToolEvaluationOutcome::PromptRequired(context) => self
                    .prompts
                    .lock()
                    .map_err(|_| HeadlessTurnPortError::Permission)
                    .map(|mut prompts| {
                        prompts.insert(call.id.clone(), context);
                        PermissionDecision::Ask
                    }),
            });
        std::future::ready(result)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermissionPromptAnswer {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
    Cancel,
}

trait PermissionPrompter: Send {
    fn prompt(
        &mut self,
        context: &PermissionPromptContext,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError>;
}

struct TtyPermissionPrompter;

impl PermissionPrompter for TtyPermissionPrompter {
    fn prompt(
        &mut self,
        context: &PermissionPromptContext,
        _: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
        if !std::io::stdin().is_terminal() {
            return Ok(PermissionPromptAnswer::DenyOnce);
        }

        eprint!("{}", render_permission_prompt(context));
        std::io::stderr()
            .flush()
            .map_err(|_| HeadlessTurnPortError::Permission)?;

        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|_| HeadlessTurnPortError::Permission)?;

        Ok(parse_permission_prompt_answer(&answer).unwrap_or(PermissionPromptAnswer::DenyOnce))
    }
}

enum ProductionPermissionPrompter {
    Tty(TtyPermissionPrompter),
    Tui(TuiPermissionBridge),
}

fn production_tui_permission_bridge() -> (TuiPermissionBridge, Receiver<TuiPermissionRequest>) {
    TuiPermissionBridge::channel()
}

impl PermissionPrompter for ProductionPermissionPrompter {
    fn prompt(
        &mut self,
        context: &PermissionPromptContext,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
        match self {
            Self::Tty(prompt) => prompt.prompt(context, cancellation),
            Self::Tui(bridge) => match bridge.wait_for_reply(
                context.qualified_tool_name.clone(),
                render_permission_prompt(context),
                cancellation,
            ) {
                TuiPermissionReply::AllowOnce => Ok(PermissionPromptAnswer::AllowOnce),
                TuiPermissionReply::AllowAlways => Ok(PermissionPromptAnswer::AllowAlways),
                TuiPermissionReply::DenyOnce => Ok(PermissionPromptAnswer::DenyOnce),
                TuiPermissionReply::DenyAlways => Ok(PermissionPromptAnswer::DenyAlways),
                TuiPermissionReply::Cancelled => Err(HeadlessTurnPortError::Cancelled),
                TuiPermissionReply::DeadlineExpired => Err(HeadlessTurnPortError::TimedOut),
            },
        }
    }
}

struct ProductionPermissionResolver<P> {
    prompt: P,
    grant_store: PermissionGrantStore,
    grants: SharedProjectPermissionGrants,
    prompts: PendingPermissionPrompts,
    authorization: ProductionPromptAuthorization,
}

struct ProductionPromptAuthorization {
    policy: PermissionPolicy,
    session: PermissionSession,
    project: String,
    dispatcher: SharedToolDispatcher,
    allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
}

impl<P> ProductionPermissionResolver<P> {
    fn new(
        prompt: P,
        grant_store: PermissionGrantStore,
        grants: SharedProjectPermissionGrants,
        prompts: PendingPermissionPrompts,
        authorization: ProductionPromptAuthorization,
    ) -> Self {
        Self {
            prompt,
            grant_store,
            grants,
            prompts,
            authorization,
        }
    }

    fn authorize_prompted_allow(
        &self,
        call: &HeadlessToolCall,
        ephemeral_grant: Option<agens_core::ProjectPermissionGrant>,
    ) -> Result<PermissionDecision, HeadlessTurnPortError> {
        let mut grants = self
            .grants
            .lock()
            .map_err(|_| HeadlessTurnPortError::Permission)?
            .clone();
        if let Some(grant) = ephemeral_grant {
            grants.push(grant);
        }

        let outcome = self
            .authorization
            .dispatcher
            .lock()
            .map_err(|_| HeadlessTurnPortError::Permission)?
            .evaluate(
                &self.authorization.policy,
                &grants,
                &self.authorization.session,
                ToolDispatchRequest::new(
                    &self.authorization.project,
                    &call.name,
                    parse_tool_input(call)?,
                ),
            )
            .map_err(|_| HeadlessTurnPortError::Permission)?;

        match outcome {
            ToolEvaluationOutcome::Authorized(handle) => self
                .authorization
                .allowed
                .lock()
                .map_err(|_| HeadlessTurnPortError::Permission)
                .map(|mut allowed| {
                    allowed.insert(
                        call.id.clone(),
                        AllowedNativeCall {
                            name: call.name.clone(),
                            input: call.input.clone(),
                            handle,
                        },
                    );
                    PermissionDecision::Allow
                }),
            ToolEvaluationOutcome::Denied => Ok(PermissionDecision::Deny),
            ToolEvaluationOutcome::PromptRequired(_) => Err(HeadlessTurnPortError::Permission),
        }
    }
}

impl<P: PermissionPrompter> HeadlessPermissionResolver for ProductionPermissionResolver<P> {
    fn resolve(
        &mut self,
        call: &HeadlessToolCall,
        cancellation: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send
    {
        let result = (|| {
            if cancellation.is_cancelled() {
                return Err(HeadlessTurnPortError::Cancelled);
            }
            if cancellation.is_expired() {
                return Err(HeadlessTurnPortError::TimedOut);
            }

            let context = self
                .prompts
                .lock()
                .map_err(|_| HeadlessTurnPortError::Permission)?
                .remove(&call.id)
                .ok_or(HeadlessTurnPortError::Permission)?;
            let answer = self.prompt.prompt(&context, cancellation)?;

            if cancellation.is_cancelled() || answer == PermissionPromptAnswer::Cancel {
                return Err(HeadlessTurnPortError::Cancelled);
            }
            if cancellation.is_expired() {
                return Err(HeadlessTurnPortError::TimedOut);
            }

            let decision = match answer {
                PermissionPromptAnswer::AllowOnce => {
                    let grant = agens_core::ProjectPermissionGrant::allow(
                        context.project_id,
                        PermissionPattern::Exact(context.qualified_tool_name),
                        PermissionPattern::Exact(context.target_identifier),
                    );
                    self.authorize_prompted_allow(call, Some(grant))?
                }
                PermissionPromptAnswer::DenyOnce => PermissionDecision::Deny,
                PermissionPromptAnswer::AllowAlways | PermissionPromptAnswer::DenyAlways => {
                    let decision = if answer == PermissionPromptAnswer::AllowAlways {
                        PermissionDecision::Allow
                    } else {
                        PermissionDecision::Deny
                    };
                    let grant = agens_core::ProjectPermissionGrant::new(
                        context.project_id,
                        decision,
                        PermissionPattern::Exact(context.qualified_tool_name),
                        PermissionPattern::Exact(context.target_identifier),
                    );
                    self.grant_store
                        .append_grants(std::slice::from_ref(&grant))
                        .map_err(|_| HeadlessTurnPortError::Permission)?;
                    self.grants
                        .lock()
                        .map_err(|_| HeadlessTurnPortError::Permission)?
                        .push(grant);
                    if decision == PermissionDecision::Allow {
                        self.authorize_prompted_allow(call, None)?
                    } else {
                        decision
                    }
                }
                PermissionPromptAnswer::Cancel => unreachable!(),
            };
            Ok(decision)
        })();
        std::future::ready(result)
    }
}

struct ProductionToolDispatcher {
    dispatcher: SharedToolDispatcher,
    allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
}

impl ProductionToolDispatcher {
    fn new(
        dispatcher: SharedToolDispatcher,
        allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
    ) -> Self {
        Self {
            dispatcher,
            allowed,
        }
    }
}

impl HeadlessToolDispatcher for ProductionToolDispatcher {
    fn dispatch(
        &mut self,
        call: HeadlessToolCall,
        cancellation: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<HeadlessToolOutput, HeadlessTurnPortError>> + Send
    {
        let allowed = self
            .allowed
            .lock()
            .map_err(|_| HeadlessTurnPortError::Tool)
            .and_then(|mut allowed| {
                let allowed_call = allowed.get(&call.id).ok_or(HeadlessTurnPortError::Tool)?;

                if allowed_call.name != call.name || allowed_call.input != call.input {
                    return Err(HeadlessTurnPortError::Tool);
                }

                allowed.remove(&call.id).ok_or(HeadlessTurnPortError::Tool)
            });
        let output = allowed
            .and_then(|allowed| {
                self.dispatcher
                    .lock()
                    .map_err(|_| HeadlessTurnPortError::Tool)?
                    .execute(
                        allowed.handle,
                        &ToolExecutionContext::from_headless_adapter(cancellation.adapter_view()),
                    )
                    .map_err(headless_tool_error)
            })
            .and_then(|output| {
                if let Some(terminal) = output.terminal() {
                    return Err(HeadlessTurnPortError::TaskTerminal(terminal));
                }
                let content = if output.is_error {
                    sanitized_native_tool_failure(&output.content)
                } else {
                    output.content
                };
                Ok(HeadlessToolOutput {
                    content,
                    is_error: output.is_error,
                })
            });
        std::future::ready(output)
    }
}

fn sanitized_native_tool_failure(content: &str) -> String {
    let Some((tool, reason)) = content.split_once(": ") else {
        return "tool execution failed".to_owned();
    };
    if !matches!(
        tool,
        "read"
            | "list"
            | "search"
            | "glob"
            | "grep"
            | "write"
            | "edit"
            | "bash"
            | "webfetch"
            | "file picker"
    ) {
        return "tool execution failed".to_owned();
    }

    let safe_reason = matches!(
        reason,
        "operation timed out" | "cancelled" | "invalid regex" | "invalid glob pattern"
    ) || [
        ("entry limit of ", " exceeded"),
        ("result limit of ", " exceeded"),
        ("traversal depth limit of ", " exceeded"),
    ]
    .into_iter()
    .any(|(prefix, suffix)| {
        reason
            .strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(suffix))
            .is_some_and(|value| {
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
            })
    });
    if safe_reason {
        format!("{tool}: {reason}")
    } else if reason.contains("outside project root")
        || reason.contains("traversal is not allowed")
        || reason.contains("must be a non-empty relative path")
    {
        format!("{tool}: path validation failed")
    } else {
        "tool execution failed".to_owned()
    }
}

struct TaskLaunchRequest<'a> {
    agent: &'a str,
    description: &'a str,
    background: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum TuiSelectedTaskLaunch {
    NotSelected,
    Dispatched,
    Rejected(TaskLaunchOutcome),
}

#[derive(Debug, PartialEq, Eq)]
enum TaskLaunchOutcome {
    Dispatched(HeadlessToolOutput),
    RejectedEmptyInput,
    RejectedCancelled,
    Denied,
}

struct AuthorizedNativeTaskRuntime<P> {
    gate: ProductionPermissionGate,
    resolver: ProductionPermissionResolver<P>,
    dispatcher: ProductionToolDispatcher,
    next_call_id: u64,
}

impl<P: PermissionPrompter> AuthorizedNativeTaskRuntime<P> {
    fn launch(
        &mut self,
        request: TaskLaunchRequest<'_>,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<TaskLaunchOutcome, HeadlessTurnPortError> {
        if request.agent.trim().is_empty() || request.description.trim().is_empty() {
            return Ok(TaskLaunchOutcome::RejectedEmptyInput);
        }
        if cancellation.is_cancelled() {
            return Ok(TaskLaunchOutcome::RejectedCancelled);
        }
        if cancellation.is_expired() {
            return Err(HeadlessTurnPortError::TimedOut);
        }

        self.next_call_id += 1;
        let call = HeadlessToolCall {
            id: format!("tui-task-{}", self.next_call_id),
            name: "native::task".into(),
            input: serde_json::json!({
                "agent": request.agent,
                "description": request.description,
                "background": request.background,
            })
            .to_string(),
        };
        let decision = poll_permission_port(self.gate.evaluate(&call, cancellation))?;
        let decision = if decision == PermissionDecision::Ask {
            poll_permission_port(self.resolver.resolve(&call, cancellation))?
        } else {
            decision
        };

        if decision == PermissionDecision::Deny {
            return Ok(TaskLaunchOutcome::Denied);
        }

        poll_permission_port(self.dispatcher.dispatch(call, cancellation))
            .map(TaskLaunchOutcome::Dispatched)
    }
}

fn launch_selected_tui_task(
    runtime: &mut ProductionTuiTaskRuntime,
    session: &Arc<Mutex<TuiSessionContext>>,
    description: &str,
    background: bool,
    cancellation: &HeadlessTurnCancellation,
) -> Result<TuiSelectedTaskLaunch, CliError> {
    let agent = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?
        .selected_subagent
        .take();
    let Some(agent) = agent else {
        return Ok(TuiSelectedTaskLaunch::NotSelected);
    };

    match runtime.authorized.launch(
        TaskLaunchRequest {
            agent: &agent,
            description,
            background,
        },
        cancellation,
    ) {
        Ok(TaskLaunchOutcome::Dispatched(output)) if !output.is_error => {
            Ok(TuiSelectedTaskLaunch::Dispatched)
        }
        Ok(TaskLaunchOutcome::Dispatched(_)) if cancellation.is_cancelled() => {
            Err(CliError::runtime(HeadlessTurnError::Cancelled))
        }
        Ok(TaskLaunchOutcome::Dispatched(_)) if cancellation.is_expired() => {
            Err(CliError::runtime(HeadlessTurnError::TimedOut))
        }
        Ok(outcome) => Ok(TuiSelectedTaskLaunch::Rejected(outcome)),
        Err(HeadlessTurnPortError::Cancelled) => {
            Err(CliError::runtime(HeadlessTurnError::Cancelled))
        }
        Err(HeadlessTurnPortError::TimedOut) => Err(CliError::runtime(HeadlessTurnError::TimedOut)),
        Err(_) => Err(CliError::runtime(HeadlessTurnError::Tool)),
    }
}

/// A subagent armed for the user's next prompt must survive a turn the user never submitted, so a
/// runtime-scheduled turn leaves the arming in place and runs the main agent instead.
fn origin_launches_selected_subagent(origin: TuiSubmitOrigin) -> bool {
    match origin {
        TuiSubmitOrigin::User | TuiSubmitOrigin::Background => true,
        TuiSubmitOrigin::SubagentCompletion => false,
    }
}

fn selected_tui_task_skips_parent(
    launch: Result<TuiSelectedTaskLaunch, CliError>,
    lifecycle: &TuiTaskLifecycleBridge,
) -> Result<bool, CliError> {
    match launch? {
        TuiSelectedTaskLaunch::NotSelected => Ok(false),
        TuiSelectedTaskLaunch::Dispatched => {
            Ok(lifecycle.mode() == Some(TaskLaunchMode::Background))
        }
        TuiSelectedTaskLaunch::Rejected(outcome) => Err(selected_task_launch_error(outcome)),
    }
}

fn selected_task_launch_error(outcome: TaskLaunchOutcome) -> CliError {
    match outcome {
        TaskLaunchOutcome::RejectedEmptyInput => CliError::usage("subagent task is empty"),
        TaskLaunchOutcome::RejectedCancelled => CliError::runtime(HeadlessTurnError::Cancelled),
        TaskLaunchOutcome::Denied => CliError::runtime(HeadlessTurnError::Permission),
        TaskLaunchOutcome::Dispatched(_) => CliError::runtime(HeadlessTurnError::Tool),
    }
}

#[allow(dead_code)]
fn poll_permission_port<T>(
    future: impl std::future::Future<Output = Result<T, HeadlessTurnPortError>>,
) -> Result<T, HeadlessTurnPortError> {
    let mut future = std::pin::pin!(future);
    let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

    match future.as_mut().poll(context) {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => Err(HeadlessTurnPortError::Permission),
    }
}

fn headless_tool_error(error: agens_core::Error) -> HeadlessTurnPortError {
    match error {
        agens_core::Error::Cancelled => HeadlessTurnPortError::Cancelled,
        agens_core::Error::Tool(message) if message == "mcp operation timed out" => {
            HeadlessTurnPortError::TimedOut
        }
        agens_core::Error::Tool(_) | agens_core::Error::Extension(_) => HeadlessTurnPortError::Tool,
        _ => HeadlessTurnPortError::Tool,
    }
}

fn permission_policy(
    rules: &[ConfigPermissionRule],
    project: &str,
    mode: PermissionMode,
    dispatcher: &SharedToolDispatcher,
    effective_capabilities: Option<&EffectiveCapabilitySet>,
) -> Result<PermissionPolicy, CliError> {
    let mut rules = rules
        .iter()
        .map(|rule| {
            let decision = match rule.decision {
                ConfigPermissionDecision::Allow => PermissionDecision::Allow,
                ConfigPermissionDecision::Deny => PermissionDecision::Deny,
            };
            let configured = configured_tool_name(&rule.tool_pattern)?;
            let tool = dispatcher
                .lock()
                .map_err(|_| CliError::configuration("tool catalog is invalid"))?
                .canonical_identity(&configured)
                .map(|identity| PermissionPattern::Exact(identity.as_str().to_owned()))
                .ok_or_else(|| CliError::configuration("permission configuration is invalid"))?;
            let target = match &rule.target_pattern {
                Some(pattern) => PermissionPattern::glob(pattern.clone())
                    .map_err(|_| CliError::configuration("permission configuration is invalid"))?,
                None => PermissionPattern::Any,
            };
            Ok(match rule.scope {
                ConfigPermissionScope::Global => PermissionRule::global(decision, tool, target),
                ConfigPermissionScope::Project => {
                    PermissionRule::project(project, decision, tool, target)
                }
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    if let Some(capabilities) = effective_capabilities {
        rules.extend(capabilities.permission_rules());
    }
    Ok(PermissionPolicy::new(mode, rules))
}

fn configured_tool_name(name: &str) -> Result<String, CliError> {
    match name {
        "read" => Ok("native::read".to_owned()),
        "write" | "edit" => Ok("native::write".to_owned()),
        "list" => Ok("native::list".to_owned()),
        "search" => Ok("native::search".to_owned()),
        "bash" => Ok("native::bash".to_owned()),
        name => Ok(name.to_owned()),
    }
}

fn parse_tool_input(call: &HeadlessToolCall) -> Result<serde_json::Value, HeadlessTurnPortError> {
    serde_json::from_str(&call.input).map_err(|_| HeadlessTurnPortError::Permission)
}

fn parse_permission_prompt_answer(value: &str) -> Option<PermissionPromptAnswer> {
    match value.trim().to_ascii_lowercase().as_str() {
        "a" | "allow-once" | "allow once" => Some(PermissionPromptAnswer::AllowOnce),
        "always" | "allow-always" | "allow always" => Some(PermissionPromptAnswer::AllowAlways),
        "d" | "deny-once" | "deny once" => Some(PermissionPromptAnswer::DenyOnce),
        "deny-always" | "deny always" => Some(PermissionPromptAnswer::DenyAlways),
        "c" | "cancel" => Some(PermissionPromptAnswer::Cancel),
        _ => None,
    }
}

fn render_permission_prompt(context: &PermissionPromptContext) -> String {
    format!(
        "Permission required for {} ({:?})\nTarget: {}\n[a]llow once, allow [always], [d]eny once, deny [always], or [c]ancel: ",
        context.qualified_tool_name,
        context.access,
        sanitize_permission_target(&context.qualified_tool_name, &context.target_identifier),
    )
}

fn sanitize_permission_target(tool: &str, target: &str) -> String {
    if tool == "native::bash" {
        return "[command redacted]".into();
    }

    if serde_json::from_str::<serde_json::Value>(target).is_ok() {
        return "[redacted]".into();
    }

    if let Some((scheme, remainder)) = target.split_once("://") {
        let remainder = remainder.split(['?', '#']).next().unwrap_or_default();
        let (authority, path) = remainder.split_once('/').unwrap_or((remainder, ""));
        let authority = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        return format!("{scheme}://{authority}/{path}");
    }

    if contains_sensitive_marker(target) {
        return "[redacted]".into();
    }

    target.to_owned()
}

fn contains_sensitive_marker(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    ["api_key", "authorization", "password", "secret", "token"]
        .iter()
        .any(|marker| value.contains(marker))
}

fn block_on_headless_turn<T>(future: impl std::future::Future<Output = T>) -> Result<T, CliError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| CliError::runtime(HeadlessTurnError::Provider))?;

    Ok(runtime.block_on(future))
}

fn load_toml(
    path: &Path,
    scope: &str,
    dependencies: &CliDependencies,
) -> Result<(toml::Table, bool), CliError> {
    let Some(contents) = (dependencies.read_file)(path)? else {
        return Ok((toml::Table::new(), false));
    };

    let document = parse_toml_document(&contents)
        .map_err(|_| CliError::configuration(format!("{scope} configuration is invalid")))?;
    validate_toml_document(&document)
        .map_err(|_| CliError::configuration(format!("{scope} configuration is invalid")))?;

    Ok((document, true))
}

fn discover_project_root(current_directory: &Path) -> Option<PathBuf> {
    let mut current = fs::canonicalize(current_directory).ok()?;

    loop {
        if current.join(".git").exists() {
            return Some(current);
        }

        let parent = current.parent().map(Path::to_path_buf);
        match parent {
            Some(parent) if parent != current => current = parent,
            _ => return None,
        }
    }
}

fn expand_document(
    mut document: toml::Table,
    environment: &BTreeMap<String, String>,
) -> Result<toml::Table, CliError> {
    for (section, field) in [("options", "data_dir"), ("provider", "base_url")] {
        if let Some(table) = document
            .get_mut(section)
            .and_then(toml::Value::as_table_mut)
        {
            expand_string_field(table, field, environment)?;
        }
    }
    Ok(document)
}

fn expand_global_mcp(
    mut document: toml::Table,
    environment: &BTreeMap<String, String>,
) -> Result<toml::Table, CliError> {
    if let Some(servers) = document.get_mut("mcp").and_then(toml::Value::as_table_mut) {
        for server in servers
            .iter_mut()
            .filter_map(|(_, value)| value.as_table_mut())
        {
            if server
                .get("disabled")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            for field in ["command", "cwd", "url"] {
                expand_mcp_string_field(server, field, environment)?;
            }
            for field in ["env", "headers"] {
                if let Some(values) = server.get_mut(field).and_then(toml::Value::as_table_mut) {
                    for (_, value) in values.iter_mut() {
                        expand_mcp_value_in_place(value, environment)?;
                    }
                }
            }
            if let Some(args) = server.get_mut("args").and_then(toml::Value::as_array_mut) {
                for value in args {
                    expand_mcp_value_in_place(value, environment)?;
                }
            }
        }
    }
    Ok(document)
}

fn resolve_provider_type(
    configured: Option<String>,
    credentials: Option<&str>,
    environment: &BTreeMap<String, String>,
) -> Option<String> {
    if matches!(configured.as_deref(), Some("openai-api" | "openai-chatgpt")) {
        return configured;
    }
    let credentials =
        credentials.and_then(|contents| serde_json::from_str::<serde_json::Value>(contents).ok());
    let chatgpt = credentials
        .as_ref()
        .and_then(|credentials| credentials.get("openai-chatgpt"));
    if chatgpt.is_some_and(|entry| {
        ["access_token", "refresh_token", "account_id", "expires_at"]
            .iter()
            .all(|field| {
                entry
                    .get(*field)
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            })
    }) {
        return Some("openai-chatgpt".to_owned());
    }
    if credentials
        .as_ref()
        .and_then(|credentials| credentials.get("openai-api"))
        .and_then(|entry| entry.get("api_key"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| !value.is_empty())
        || environment
            .get("OPENAI_API_KEY")
            .is_some_and(|value| !value.is_empty())
    {
        return Some("openai-api".to_owned());
    }
    None
}

fn openai_api_key(
    credentials: Option<&str>,
    environment: &BTreeMap<String, String>,
) -> Option<String> {
    environment
        .get("OPENAI_API_KEY")
        .filter(|key| !key.is_empty())
        .cloned()
        .or_else(|| {
            credentials
                .and_then(|contents| serde_json::from_str::<serde_json::Value>(contents).ok())
                .and_then(|credentials| {
                    credentials
                        .get("openai-api")?
                        .get("api_key")?
                        .as_str()
                        .filter(|key| !key.is_empty())
                        .map(ToOwned::to_owned)
                })
        })
}

fn expand_value_in_place(
    value: &mut toml::Value,
    environment: &BTreeMap<String, String>,
) -> Result<(), CliError> {
    if let Some(raw) = value.as_str() {
        *value =
            toml::Value::String(expand_environment(raw, environment).map_err(|_| {
                CliError::configuration("configuration environment expansion failed")
            })?);
    }
    Ok(())
}

fn expand_mcp_value_in_place(
    value: &mut toml::Value,
    environment: &BTreeMap<String, String>,
) -> Result<(), CliError> {
    if let Some(raw) = value.as_str() {
        *value =
            toml::Value::String(expand_environment_with_commands(raw, environment).map_err(
                |_| CliError::configuration("configuration environment expansion failed"),
            )?);
    }
    Ok(())
}

fn expand_string_field(
    table: &mut toml::Table,
    field: &str,
    environment: &BTreeMap<String, String>,
) -> Result<(), CliError> {
    if let Some(value) = table.get_mut(field) {
        expand_value_in_place(value, environment)?;
    }
    Ok(())
}

fn expand_mcp_string_field(
    table: &mut toml::Table,
    field: &str,
    environment: &BTreeMap<String, String>,
) -> Result<(), CliError> {
    if let Some(value) = table.get_mut(field) {
        expand_mcp_value_in_place(value, environment)?;
    }
    Ok(())
}

fn string_value(document: &toml::Table, path: &[&str]) -> Option<String> {
    let mut value = document.get(*path.first()?)?;

    for key in &path[1..] {
        value = value.as_table()?.get(*key)?;
    }

    value.as_str().map(ToOwned::to_owned)
}

fn data_directory(
    document: &toml::Table,
    home_directory: Option<&Path>,
    environment: &BTreeMap<String, String>,
) -> PathBuf {
    string_value(document, &["options", "data_dir"])
        .filter(|directory| !directory.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            environment
                .get("XDG_DATA_HOME")
                .filter(|directory| !directory.is_empty())
                .map(PathBuf::from)
                .or_else(|| home_directory.map(|directory| directory.join(".local/share")))
                .unwrap_or_else(|| PathBuf::from(".local/share"))
                .join("agens")
        })
}

fn source_status(loaded: bool) -> &'static str {
    if loaded { "loaded" } else { "missing" }
}

fn is_help(argument: &str) -> bool {
    matches!(argument, "--help" | "-h" | "help")
}

fn is_version(argument: &str) -> bool {
    matches!(argument, "--version" | "-V" | "version")
}

fn root_help() -> String {
    format!(
        "Agens is a coding agent CLI\n\nUsage: agens <command>\n\nCommands:\n  auth      inspect supported authentication\n  chat      run a headless agent turn\n  config    inspect configuration\n  models    list provider models\n  sessions  inspect completed turns\n\nVersion: {}\n",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agens_core::{
        AgentDefinition, AgentMode, CompletedTurnRepository, CompletedTurnSnapshot,
        Error as ToolError, PermissionRule, ToolAccess, TurnProvider, TurnState, Usage,
    };
    use agens_tui::{Action, Event, Key};
    use rusqlite::Connection;

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
    fn subagent_message_and_cancellation_leave_the_primary_agent_unchanged() {
        let registry = TaskExecutionRegistry::new();
        let id = registry.admit(TaskLaunchMode::Background).unwrap();
        let dispatcher = rotation_dispatcher();
        let primary = rotation_agent("primary", None, false);
        let active = ActiveAgentRuntime::build(
            &primary,
            Some("gpt-5.5"),
            "project",
            &dispatcher,
            &BundledModelValidator,
        )
        .unwrap();
        let session = TuiSessionContext {
            active_agent: Some(active),
            ..TuiSessionContext::fresh()
        };

        registry
            .send_message(
                TaskMessageSource::User,
                TaskMessageTarget::Execution(id),
                "continue".into(),
            )
            .unwrap();
        assert!(registry.cancel(id));

        assert_eq!(
            session
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );
        assert_eq!(
            session
                .active_agent
                .as_ref()
                .and_then(|agent| agent.model.as_deref()),
            Some("gpt-5.5")
        );
    }

    #[test]
    fn production_task_error_mapping_reserves_provider_for_provider_failures() {
        assert_eq!(
            map_task_turn_error(HeadlessTurnError::MaxIterations),
            TaskRunnerError::IterationLimit
        );
        assert_eq!(
            map_task_turn_error(HeadlessTurnError::Provider),
            TaskRunnerError::ProviderFailure
        );
        assert_eq!(
            map_task_turn_error(HeadlessTurnError::Tool),
            TaskRunnerError::ChildFailure
        );
    }

    #[test]
    fn native_permission_target_projects_each_registered_tool_to_its_canonical_field() {
        let cases = [
            (
                "native::bash",
                serde_json::json!({"command": "git status"}),
                NativePermissionTarget::Command("git status".into()),
            ),
            (
                "native::read",
                serde_json::json!({"path": "notes.md"}),
                NativePermissionTarget::Path("notes.md".into()),
            ),
            (
                "native::write",
                serde_json::json!({"path": "notes.md", "content": "body"}),
                NativePermissionTarget::Path("notes.md".into()),
            ),
            (
                "native::edit",
                serde_json::json!({"path": "notes.md", "old": "old", "new": "new"}),
                NativePermissionTarget::Path("notes.md".into()),
            ),
            (
                "native::list",
                serde_json::json!({"path": "src"}),
                NativePermissionTarget::Path("src".into()),
            ),
            (
                "native::search",
                serde_json::json!({"path": "src", "query": "permission"}),
                NativePermissionTarget::Path("src".into()),
            ),
            (
                "native::glob",
                serde_json::json!({"pattern": "src/**/*.rs"}),
                NativePermissionTarget::Pattern("src/**/*.rs".into()),
            ),
            (
                "native::grep",
                serde_json::json!({"pattern": "permission"}),
                NativePermissionTarget::Pattern("permission".into()),
            ),
            (
                "native::webfetch",
                serde_json::json!({"url": "https://example.test/docs"}),
                NativePermissionTarget::Url("https://example.test/docs".into()),
            ),
        ];

        for (tool, arguments, expected) in cases {
            assert_eq!(
                NativePermissionTarget::parse(tool, &arguments),
                Ok(expected)
            );
        }
    }

    #[test]
    fn native_permission_target_keeps_grep_path_separate_from_its_pattern() {
        assert_eq!(
            NativePermissionTarget::parse(
                "native::grep",
                &serde_json::json!({"pattern": "TODO", "path": "crates/agens-cli"}),
            ),
            Ok(NativePermissionTarget::Pattern("TODO".into()))
        );
    }

    #[test]
    fn native_permission_target_rejects_invalid_target_fields_for_every_registered_tool() {
        let too_long = "x".repeat(agens_core::MAX_PERMISSION_TARGET_BYTES + 1);

        for (tool, field) in [
            ("native::bash", "command"),
            ("native::read", "path"),
            ("native::write", "path"),
            ("native::edit", "path"),
            ("native::list", "path"),
            ("native::search", "path"),
            ("native::glob", "pattern"),
            ("native::grep", "pattern"),
            ("native::webfetch", "url"),
        ] {
            assert_eq!(
                NativePermissionTarget::parse(tool, &serde_json::json!({})),
                Err(NativePermissionTargetError::InvalidField(field))
            );

            for (value, expected) in [
                (
                    serde_json::json!(1),
                    NativePermissionTargetError::InvalidField(field),
                ),
                (
                    serde_json::json!(""),
                    NativePermissionTargetError::InvalidField(field),
                ),
                (
                    serde_json::json!(too_long.clone()),
                    NativePermissionTargetError::FieldTooLong(field),
                ),
            ] {
                let arguments = serde_json::Value::Object(serde_json::Map::from_iter([(
                    field.to_owned(),
                    value,
                )]));

                assert_eq!(
                    NativePermissionTarget::parse(tool, &arguments),
                    Err(expected)
                );
            }
        }

        for (value, expected) in [
            (
                serde_json::json!(1),
                NativePermissionTargetError::InvalidField("path"),
            ),
            (
                serde_json::json!(""),
                NativePermissionTargetError::InvalidField("path"),
            ),
            (
                serde_json::json!(too_long),
                NativePermissionTargetError::FieldTooLong("path"),
            ),
        ] {
            assert_eq!(
                NativePermissionTarget::parse(
                    "native::grep",
                    &serde_json::json!({"pattern": "TODO", "path": value}),
                ),
                Err(expected)
            );
        }

        assert_eq!(
            NativePermissionTarget::parse("native::glob", &serde_json::json!([])),
            Err(NativePermissionTargetError::ArgumentsNotObject)
        );
        assert_eq!(
            NativePermissionTarget::parse(
                "native::unknown",
                &serde_json::json!({"path": "notes.md"}),
            ),
            Err(NativePermissionTargetError::UnknownTool)
        );
    }

    #[test]
    fn tool_input_parses_every_native_tool_into_its_typed_kind() {
        let cases = [
            (
                "read",
                serde_json::json!({"path": "notes.md"}),
                agens_core::ToolInput::Read {
                    path: "notes.md".into(),
                },
            ),
            (
                "write",
                serde_json::json!({"path": "notes.md", "content": "body"}),
                agens_core::ToolInput::Write {
                    path: "notes.md".into(),
                },
            ),
            (
                "edit",
                serde_json::json!({"path": "notes.md", "old": "old", "new": "new"}),
                agens_core::ToolInput::Edit {
                    path: "notes.md".into(),
                },
            ),
            (
                "list",
                serde_json::json!({"path": "src"}),
                agens_core::ToolInput::List { path: "src".into() },
            ),
            (
                "search",
                serde_json::json!({"path": "src", "query": "permission"}),
                agens_core::ToolInput::Search { path: "src".into() },
            ),
            (
                "glob",
                serde_json::json!({"pattern": "src/**/*.rs"}),
                agens_core::ToolInput::Glob {
                    pattern: "src/**/*.rs".into(),
                    path: None,
                },
            ),
            (
                "grep",
                serde_json::json!({"pattern": "TODO", "path": "crates/agens-cli"}),
                agens_core::ToolInput::Grep {
                    pattern: "TODO".into(),
                    path: Some("crates/agens-cli".into()),
                },
            ),
            (
                "bash",
                serde_json::json!({"command": "git status"}),
                agens_core::ToolInput::Bash {
                    command: "git status".into(),
                },
            ),
            (
                "webfetch",
                serde_json::json!({"url": "https://example.test/docs"}),
                agens_core::ToolInput::WebFetch {
                    url: "https://example.test/docs".into(),
                },
            ),
            (
                "skill",
                serde_json::json!({"skill": "shared"}),
                agens_core::ToolInput::Skill {
                    skill: "shared".into(),
                },
            ),
        ];

        for (name, arguments, expected) in cases {
            let raw = arguments.to_string();
            assert_eq!(agens_core::ToolInput::parse(name, &raw), expected);
        }
    }

    #[test]
    fn tool_input_degrades_unknown_and_mcp_tools_to_other_without_erroring() {
        let raw = serde_json::json!({"foo": "bar"}).to_string();
        assert_eq!(
            agens_core::ToolInput::parse("mcp_server_tool", &raw),
            agens_core::ToolInput::Other {
                name: "mcp_server_tool".into(),
                raw: raw.clone(),
            }
        );

        let malformed = "{not json";
        assert_eq!(
            agens_core::ToolInput::parse("read", malformed),
            agens_core::ToolInput::Other {
                name: "read".into(),
                raw: malformed.into(),
            }
        );

        let missing_field = serde_json::json!({}).to_string();
        assert_eq!(
            agens_core::ToolInput::parse("read", &missing_field),
            agens_core::ToolInput::Other {
                name: "read".into(),
                raw: missing_field.clone(),
            }
        );
    }

    struct RotationTool;

    impl DispatchTool for RotationTool {
        fn execute(
            &mut self,
            _: &ToolExecutionContext,
            _: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::success("unused"))
        }
    }

    fn rotation_agent(name: &str, model: Option<&str>, allow_read: bool) -> AgentDefinition {
        AgentDefinition {
            name: name.into(),
            description: format!("{name} agent"),
            mode: AgentMode::Primary,
            model: model.map(str::to_owned),
            system_prompt: format!("You are {name}."),
            permission_rules: allow_read
                .then(|| {
                    PermissionRule::global(
                        PermissionDecision::Allow,
                        PermissionPattern::glob("native::read").unwrap(),
                        PermissionPattern::Any,
                    )
                })
                .into_iter()
                .collect(),
            skills: Vec::new(),
        }
    }

    fn rotation_dispatcher() -> ToolDispatcher {
        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register_native("native::read", ToolAccess::ReadOnly, RotationTool)
            .unwrap();
        dispatcher
    }

    #[test]
    fn idle_agent_rotation_restores_runtime_and_queues_expansion_reminders_atomically() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-agent-rotation-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dispatcher = rotation_dispatcher();
        let primary = rotation_agent("primary", Some("gpt-4.1"), false);
        let reviewer = rotation_agent("reviewer", Some("gpt-4o"), true);
        let mut store = SessionStore::open(&temporary).unwrap();
        let metadata = SessionMetadata {
            id: 0,
            project: "project".into(),
            title: "title".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
        };
        let turn = CompletedSessionTurn::new(vec![
            SessionMessage::try_from(Message {
                role: Role::User,
                parts: vec![MessagePart::Text("first".into())],
            })
            .unwrap(),
        ])
        .unwrap();
        let metadata = store
            .persist_completed_session_turn(&metadata, &turn)
            .unwrap();
        let primary_runtime = ActiveAgentRuntime::build(
            &primary,
            None,
            "project",
            &dispatcher,
            &BundledModelValidator,
        )
        .unwrap();
        let mut context =
            TuiSessionContext::resumed(1, metadata.clone(), Vec::new(), primary_runtime);
        let original = context.clone();
        context.running = true;
        let busy_original = context.clone();

        let busy = rotate_active_agent(
            &mut context,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
        );
        assert_eq!(busy, Err(AgentRotationError::Busy));
        assert_eq!(context, busy_original);
        context.running = false;
        assert_eq!(
            SessionStore::open(&temporary)
                .unwrap()
                .load_session_for_resume(1)
                .unwrap()
                .metadata
                .active_agent,
            "primary"
        );

        let mut conflicting = metadata.clone();
        conflicting.title = "changed elsewhere".into();
        conflicting.updated_at = 2;
        let conflicting = store
            .persist_completed_session_turn(&conflicting, &turn)
            .unwrap();
        let rollback = rotate_active_agent(
            &mut context,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
        );
        assert_eq!(rollback, Err(AgentRotationError::Persistence));
        assert_eq!(context, original);

        context.metadata = Some(conflicting);
        rotate_active_agent(
            &mut context,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
        )
        .unwrap();
        assert_eq!(
            context.pending_system_reminder.as_deref(),
            Some("Agent capabilities expanded: primary -> reviewer.")
        );

        let request = context.apply_to(HeadlessChatRequest {
            prompt: "next".into(),
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
        assert_eq!(request.active_agent.as_deref(), Some("reviewer"));
        assert_eq!(request.model.as_deref(), Some("gpt-4o"));
        assert_eq!(request.system_prompt.as_deref(), Some("You are reviewer."));
        assert_eq!(
            request.effective_capabilities,
            context
                .active_agent
                .as_ref()
                .map(|agent| agent.capabilities.clone())
        );
        assert_eq!(
            provider_messages(&request, false),
            vec![
                Message {
                    role: Role::System,
                    parts: vec![MessagePart::Text(
                        "Agent capabilities expanded: primary -> reviewer.".into(),
                    )],
                },
                Message {
                    role: Role::User,
                    parts: vec![MessagePart::Text("next".into())],
                },
            ]
        );

        rotate_active_agent(
            &mut context,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
        )
        .unwrap();
        assert_eq!(
            context.pending_system_reminder.as_deref(),
            Some("Agent capabilities expanded: primary -> reviewer.")
        );

        let policy = permission_policy(
            &[],
            "project",
            PermissionMode::Edit,
            &Arc::new(Mutex::new(rotation_dispatcher())),
            request.effective_capabilities.as_ref(),
        )
        .unwrap();
        assert!(matches!(
            rotation_dispatcher()
                .evaluate(
                    &policy,
                    &[],
                    &PermissionSession::new(),
                    ToolDispatchRequest::new(
                        "project",
                        "native::read",
                        serde_json::json!({"target":"file"})
                    ),
                )
                .unwrap(),
            ToolEvaluationOutcome::Authorized(_)
        ));

        let snapshot = CompletedTurnSnapshot::from_persisted_events(vec![
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("answer".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ])
        .unwrap();
        let turn = completed_session_turn(
            "next",
            &snapshot,
            request.pending_system_reminder.as_deref(),
        )
        .unwrap();
        let persisted = store
            .persist_completed_session_turn(context.metadata.as_ref().unwrap(), &turn)
            .unwrap();
        context.metadata = Some(persisted);
        context.pending_system_reminder = None;
        let reopened = SessionStore::open(&temporary)
            .unwrap()
            .load_session_for_resume(1)
            .unwrap();
        assert_eq!(reopened.metadata.active_agent, "reviewer");
        let reminder = reopened
            .messages
            .iter()
            .find(|message| message.role == Role::System)
            .unwrap();
        assert_eq!(
            reminder.parts,
            vec![MessagePart::Text(
                "Agent capabilities expanded: primary -> reviewer.".into()
            )]
        );
        assert!(context.pending_system_reminder.is_none());

        let mut no_expansion = TuiSessionContext::resumed(
            1,
            reopened.metadata,
            reopened.messages,
            context.active_agent.clone().unwrap(),
        );
        no_expansion.metadata = None;
        rotate_active_agent(
            &mut no_expansion,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            None,
        )
        .unwrap();
        assert!(no_expansion.pending_system_reminder.is_none());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn completed_tui_turn_clears_reminders_only_after_successful_persistence() {
        let metadata = SessionMetadata {
            id: 1,
            project: "project".into(),
            title: "title".into(),
            active_agent: "reviewer".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 2,
            completed_turn_count: 2,
            resumable: true,
        };
        let mut context = TuiSessionContext::fresh();
        context.pending_system_reminder = Some("reminder".into());

        assert_eq!(
            complete_tui_turn(
                &mut context,
                Ok(HeadlessChatCompletion {
                    text: "answer".into(),
                    metadata: metadata.clone(),
                    messages: Vec::new(),
                }),
                true,
            )
            .unwrap(),
            "answer"
        );
        assert_eq!(context.metadata, Some(metadata));
        assert!(context.pending_system_reminder.is_none());

        context.pending_system_reminder = Some("reminder".into());
        assert!(
            complete_tui_turn(&mut context, Err(CliError::storage("failed").into()), true).is_err()
        );
        assert_eq!(context.pending_system_reminder.as_deref(), Some("reminder"));
    }

    #[test]
    fn completed_session_turn_ignores_usage_without_changing_output_history_order() {
        let events = [
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::ProviderPart(MessagePart::Text("before usage".into())),
            TurnEvent::Usage(Usage {
                input_tokens: Some(5),
                output_tokens: Some(3),
                total_tokens: Some(8),
                context_window: Some(16),
            }),
            TurnEvent::ProviderPart(MessagePart::Reasoning("after usage".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ];

        let turn = completed_session_turn_from_events("prompt", &events, None)
            .expect("completed session turn should exclude presentation usage");

        assert_eq!(
            turn.messages(),
            &[
                Message {
                    role: Role::User,
                    parts: vec![MessagePart::Text("prompt".into())],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        MessagePart::Text("before usage".into()),
                        MessagePart::Reasoning("after usage".into()),
                    ],
                },
            ]
        );
    }

    #[test]
    fn completed_session_turn_keeps_role_boundaries_around_usage() {
        let events = [
            TurnEvent::ProviderPart(MessagePart::Text("before tool".into())),
            TurnEvent::Usage(Usage::default()),
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "tool output".into(),
                is_error: false,
            }),
            TurnEvent::Usage(Usage {
                input_tokens: None,
                output_tokens: Some(0),
                total_tokens: None,
                context_window: None,
            }),
            TurnEvent::ProviderPart(MessagePart::Text("after tool".into())),
        ];

        let turn = completed_session_turn_from_events("prompt", &events, None)
            .expect("completed session turn should exclude presentation usage");

        assert_eq!(
            turn.messages(),
            &[
                Message {
                    role: Role::User,
                    parts: vec![MessagePart::Text("prompt".into())],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![MessagePart::Text("before tool".into())],
                },
                Message {
                    role: Role::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_call_id: "call-1".into(),
                        content: "tool output".into(),
                        is_error: false,
                    }],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![MessagePart::Text("after tool".into())],
                },
            ]
        );
    }

    #[test]
    fn p1c1_completed_subagent_turn_redacts_and_bounds_durable_content() {
        let turn = CompletedSubagentTurn {
            id: 1,
            agent: "reviewer".into(),
            task: format!("authorization {}", "x".repeat(300)),
            final_result: "token=result".into(),
            tool_uses: 1,
        };

        let messages = completed_subagent_session_turn(&turn, "subagent:1")
            .unwrap()
            .messages()
            .to_vec();

        assert_eq!(
            messages[0].parts,
            vec![MessagePart::Text("[redacted]".into())]
        );
        assert_eq!(
            messages[2].parts,
            vec![MessagePart::ToolResult {
                tool_call_id: "subagent:1".into(),
                content: "[withheld: 12 characters matched a credential pattern]".into(),
                is_error: false,
            }]
        );
    }

    #[test]
    fn p1c4_completing_a_turn_keeps_a_subagent_turn_persisted_mid_flight() {
        let subagent_turn = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("review the patch".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::ToolCall {
                        id: "subagent:1".into(),
                        name: "native::task".into(),
                        input: r#"{"agent":"reviewer","description":"review the patch"}"#.into(),
                    },
                    MessagePart::Reasoning("3 tool uses".into()),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "subagent:1".into(),
                    content: "approved".into(),
                    is_error: false,
                }],
            },
        ];
        let foreground_turn = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("summarize the patch".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text("summary".into())],
            },
        ];
        let mut session = TuiSessionContext {
            identifier: Some(7),
            messages: subagent_turn.clone(),
            ..TuiSessionContext::fresh()
        };
        let completion = HeadlessChatCompletion {
            text: "summary".into(),
            metadata: SessionMetadata {
                id: 7,
                project: "project".into(),
                title: "conversation".into(),
                active_agent: "primary".into(),
                provider_id: None,
                model_id: None,
                reasoning_effort: None,
                created_at: 1,
                updated_at: 1,
                completed_turn_count: 1,
                resumable: true,
            },
            messages: foreground_turn.clone(),
        };

        assert_eq!(
            complete_tui_turn(&mut session, Ok(completion), false).unwrap(),
            "summary"
        );

        let mut expected = foreground_turn;
        expected.extend(subagent_turn);
        assert_eq!(session.messages, expected);
    }

    #[test]
    fn p1c1_persisted_subagent_result_stays_bounded_and_marks_every_loss() {
        let subagent_turn = |final_result: String| CompletedSubagentTurn {
            id: 1,
            agent: "reviewer".into(),
            task: "review the patch".into(),
            final_result,
            tool_uses: 1,
        };
        let persisted_result = |turn: &CompletedSubagentTurn| {
            let messages = completed_subagent_session_turn(turn, "subagent:1")
                .unwrap()
                .messages()
                .to_vec();
            match &messages[2].parts[0] {
                MessagePart::ToolResult { content, .. } => content.clone(),
                part => panic!("subagent turns persist a tool result: {part:?}"),
            }
        };

        let long = persisted_result(&subagent_turn("a".repeat(70_000)));
        assert!(long.starts_with(&"a".repeat(MAX_PERSISTED_SUBAGENT_RESULT_CHARS)));
        assert!(long.ends_with(SUBAGENT_RESULT_TRUNCATION_MARKER));
        assert_eq!(
            long.chars().count(),
            MAX_PERSISTED_SUBAGENT_RESULT_CHARS + SUBAGENT_RESULT_TRUNCATION_MARKER.chars().count()
        );

        let bounded = persisted_result(&subagent_turn("a".repeat(300)));
        assert_eq!(bounded, "a".repeat(300));

        let with_secret = persisted_result(&subagent_turn(
            "usable finding\napi_key=abcd\ntrailing finding".into(),
        ));
        assert_eq!(
            with_secret,
            "usable finding\n[withheld: 12 characters matched a credential pattern]\ntrailing finding"
        );

        let only_secret = persisted_result(&subagent_turn("token=abcd".into()));
        assert_eq!(
            only_secret,
            "[withheld: 10 characters matched a credential pattern]"
        );
    }

    #[test]
    fn p1c1_persisted_subagent_call_ids_stay_unique_when_execution_ids_restart() {
        let temporary = tui_session_directory("subagent-call-id-uniqueness");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let turn = |final_result: &str| CompletedSubagentTurn {
            id: 1,
            agent: "reviewer".into(),
            task: "review the patch".into(),
            final_result: final_result.into(),
            tool_uses: 1,
        };

        persist_completed_subagent_turn(&bootstrap, &session, turn("first")).unwrap();
        persist_completed_subagent_turn(&bootstrap, &session, turn("second")).unwrap();

        let messages = session.lock().unwrap().messages.clone();
        let call_ids = messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                MessagePart::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            call_ids,
            vec!["subagent:1".to_owned(), "subagent:2".to_owned()]
        );
        agens_providers::encode_openai_response_request_with_messages("gpt-4.1", &messages, &[])
            .expect("a resumed subagent history must encode for the provider");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn p1c2_resume_parser_restores_only_complete_standard_subagent_turns() {
        let messages = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("review the patch".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::ToolCall {
                        id: "subagent:42".into(),
                        name: "native::task".into(),
                        input: r#"{"agent":"reviewer","description":"review the patch"}"#.into(),
                    },
                    MessagePart::Reasoning("3 tool uses".into()),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "subagent:42".into(),
                    content: "approved".into(),
                    is_error: false,
                }],
            },
        ];

        assert_eq!(
            resumed_subagent_cards(&messages),
            vec![TuiRuntimeEvent::RestoredCompletedSubagent {
                id: 42,
                agent: "reviewer".into(),
                task_summary: "review the patch".into(),
                final_result: "approved".into(),
                tool_uses: 3,
            }]
        );

        let mut duplicate = messages.clone();
        duplicate.extend(messages.clone());
        assert_eq!(resumed_subagent_cards(&duplicate).len(), 1);

        let mut failed = messages;
        failed[2].parts = vec![MessagePart::ToolResult {
            tool_call_id: "subagent:42".into(),
            content: "failed".into(),
            is_error: true,
        }];
        assert!(resumed_subagent_cards(&failed).is_empty());

        let mut malformed = duplicate[..3].to_vec();
        malformed[1].parts[0] = MessagePart::ToolCall {
            id: "subagent:43".into(),
            name: "native::task".into(),
            input: "not json".into(),
        };
        assert!(resumed_subagent_cards(&malformed).is_empty());

        let incomplete = duplicate[..2].to_vec();
        assert!(resumed_subagent_cards(&incomplete).is_empty());

        let mut transient = duplicate[..3].to_vec();
        transient[2].parts = vec![MessagePart::ToolResult {
            tool_call_id: "subagent:43".into(),
            content: "cancelled".into(),
            is_error: true,
        }];
        assert!(resumed_subagent_cards(&transient).is_empty());
    }

    #[test]
    fn p1c1_terminal_observer_excludes_non_completed_matrix() {
        for (label, terminal) in [
            ("cancelled", Some(TaskTerminalState::Cancelled)),
            ("timed-out", Some(TaskTerminalState::Failed)),
            ("incomplete", None),
            ("failed", Some(TaskTerminalState::Failed)),
        ] {
            let temporary = tui_session_directory(&format!("p1c1-{label}"));
            let bootstrap = tui_session_bootstrap(
                &temporary,
                &[(
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                )],
            );
            let (events, _receiver) = BridgeTx::bounded(8);
            let controls = TuiTaskControls::default();
            let session = Arc::new(Mutex::new(TuiSessionContext {
                selected_subagent: Some("reviewer".into()),
                ..TuiSessionContext::fresh()
            }));
            let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, controls.clone())
                .with_session_writer(bootstrap.clone(), Arc::clone(&session));
            let mut runtime = production_tui_task_runtime_with_runner(
                &bootstrap,
                &SkillCatalog::default(),
                production_tui_permission_bridge().0,
                ProductionTaskRunner::with_probe(
                    bootstrap.clone(),
                    bootstrap.project_root().unwrap().to_path_buf(),
                    Arc::new(Mutex::new(Vec::new())),
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
            let worker_session = Arc::clone(&session);
            let worker_cancellation = cancellation.clone();
            let worker = std::thread::spawn(move || {
                launch_selected_tui_task(
                    &mut runtime,
                    &worker_session,
                    "review task",
                    false,
                    &worker_cancellation,
                )
            });
            let lifecycle = (0..100)
                .find_map(|_| {
                    let lifecycle = controls
                        .0
                        .lifecycle(agens_tools::TaskExecutionId::from_value(1));
                    if lifecycle.is_none() {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    lifecycle
                })
                .expect("running task should be observed");

            assert!(session.lock().unwrap().identifier.is_none());
            assert!(lifecycle.transition_to_background());
            assert!(session.lock().unwrap().identifier.is_none());
            if let Some(terminal) = terminal {
                assert!(lifecycle.finish(terminal));
            }
            if label == "failed" {
                assert!(!lifecycle.finish(TaskTerminalState::Completed));
            }

            cancellation.cancel();
            let _ = worker.join().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));

            assert!(session.lock().unwrap().identifier.is_none());
            assert!(
                SessionStore::open(bootstrap.data_directory())
                    .unwrap()
                    .list_sessions()
                    .unwrap()
                    .is_empty()
            );

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    mod model_registry {
        use super::*;

        #[test]
        fn parses_tolerant_snapshot_filters_and_sorts_models() {
            let snapshot = br#"{
                "source": "https://models.dev",
                "revision": "test",
                "models": [
                    {"id":"z-model","name":"Z","context":4,"input_price":1.5,"output_price":2.5,"supported":true,"future":true},
                    {"id":"a-model","supported":true},
                    {"id":"unsupported","supported":false},
                    {"name":"missing-id","supported":true}
                ]
            }"#;

            let models = crate::model_registry::parse_models(snapshot).expect("snapshot parses");

            assert_eq!(models.len(), 2);
            assert_eq!(models[0].id, "a-model");
            assert_eq!(models[0].name, None);
            assert_eq!(models[0].context, None);
            assert_eq!(models[0].input_price, None);
            assert_eq!(models[0].output_price, None);
            assert_eq!(models[1].id, "z-model");
        }

        #[test]
        fn validates_bundled_snapshot_checksum_and_schema() {
            let models =
                crate::model_registry::bundled_openai_models().expect("bundled snapshot is valid");

            assert_eq!(
                crate::model_registry::bundled_snapshot_checksum(),
                "75086c4979636664367c3031c023b20479fb66296b197fe612b2b624696b5984"
            );
            assert_eq!(
                models.first().map(|model| model.id.as_str()),
                Some("gpt-4.1")
            );
            assert_eq!(
                models.last().map(|model| model.id.as_str()),
                Some("o4-mini")
            );
        }

        #[test]
        fn rejects_snapshot_schema_without_a_model_collection() {
            let result = crate::model_registry::parse_models(
                br#"{"source":"https://models.dev","revision":"test"}"#,
            );

            assert!(result.is_err());
        }

        #[test]
        fn formats_four_columns_and_an_explicit_empty_result() {
            let output = crate::model_registry::format_models(&[
                crate::model_registry::ModelMetadata {
                    id: "missing".to_owned(),
                    name: None,
                    context: None,
                    output: None,
                    reasoning: None,
                    input_price: None,
                    output_price: Some(0.6),
                },
                crate::model_registry::ModelMetadata {
                    id: "known".to_owned(),
                    name: Some("Known".to_owned()),
                    context: Some(128000),
                    output: None,
                    reasoning: None,
                    input_price: Some(2.5),
                    output_price: Some(10.0),
                },
            ]);

            assert_eq!(
                output,
                "ID\tNAME\tCONTEXT\tPRICE\nmissing\t-\t-\t-/$0.60\nknown\tKnown\t128000\t$2.50/$10.00\n"
            );
            assert_eq!(
                crate::model_registry::format_models(&[]),
                "No supported models.\n"
            );
        }

        #[test]
        fn context_window_for_returns_registry_value_or_none() {
            assert_eq!(
                crate::model_registry::context_window_for("gpt-4.1"),
                Some(1_047_576)
            );
            assert_eq!(
                crate::model_registry::context_window_for("gpt-5.5"),
                Some(272_000)
            );
            assert_eq!(
                crate::model_registry::context_window_for("not-a-real-model-xyz"),
                None
            );
        }

        #[test]
        fn models_command_uses_the_bundled_registry() {
            let result = execute_strings(
                vec!["models".to_owned()],
                &CliDependencies::for_test(
                    PathBuf::from("/workspace"),
                    None,
                    BTreeMap::new(),
                    BTreeMap::new(),
                ),
                &HeadlessTurnCancellation::new(),
            );

            assert_eq!(result.status, ExitStatus::Success);
            assert_eq!(
                result.stdout,
                "ID\tNAME\tCONTEXT\tPRICE\ngpt-4.1\tGPT-4.1\t1047576\t$2.00/$8.00\ngpt-4.1-mini\tGPT-4.1 mini\t1047576\t$0.40/$1.60\ngpt-4.1-nano\tGPT-4.1 nano\t1047576\t$0.10/$0.40\ngpt-4o\tGPT-4o\t128000\t$2.50/$10.00\ngpt-4o-mini\tGPT-4o mini\t128000\t$0.15/$0.60\no3\to3\t200000\t$2.00/$8.00\no4-mini\to4-mini\t200000\t$1.10/$4.40\n"
            );
        }
    }

    #[test]
    fn fresh_tui_presentation_projects_resolved_model_effort_and_context() {
        let known_root = tui_session_directory("fresh-presentation-known");
        let known_bootstrap =
            tui_session_bootstrap_for_provider(&known_root, &[], "openai-api", "gpt-5.6-sol");
        let mut known_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        known_tui.apply_presentation(tui_session_presentation(
            &known_bootstrap,
            &TuiSessionContext::fresh(),
        ));
        configure_tui_project_identity(&mut known_tui, &known_bootstrap);
        let known = render_tui_test_backend(&known_tui, 140, 14);

        assert!(
            known.contains("gpt-5.6-sol · medium · 0/1.1m (0%)"),
            "{known:?}"
        );
        assert!(!known.contains("model · default · ctx —"), "{known:?}");

        let unknown_root = tui_session_directory("fresh-presentation-unknown");
        let unknown_bootstrap =
            tui_session_bootstrap_for_provider(&unknown_root, &[], "openai-api", "gpt-future-1");
        let mut unknown_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        unknown_tui.apply_presentation(tui_session_presentation(
            &unknown_bootstrap,
            &TuiSessionContext::fresh(),
        ));
        let unknown = render_tui_test_backend(&unknown_tui, 140, 14);

        assert!(
            unknown.contains("gpt-future-1 · effort — · ctx —"),
            "{unknown:?}"
        );
        assert!(
            !unknown.contains("gpt-future-1 · effort — · 0/"),
            "{unknown:?}"
        );

        std::fs::remove_dir_all(known_root).unwrap();
        std::fs::remove_dir_all(unknown_root).unwrap();
    }

    #[test]
    fn tui_session_reset_refuses_running_mutation_without_state_change() {
        let mut context = TuiSessionContext::fresh();
        context.identifier = Some(7);
        context.running = true;
        let original = context.clone();

        assert_eq!(
            reset_tui_session(&mut context),
            Err(TuiSessionMutationError::Busy)
        );
        assert_eq!(context, original);
    }

    #[test]
    fn tui_session_reset_clears_resumed_state_when_idle() {
        let mut context = TuiSessionContext::fresh();
        context.identifier = Some(7);
        context.metadata = Some(SessionMetadata {
            id: 7,
            project: "project".into(),
            title: "conversation".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 1,
            resumable: true,
        });
        context.messages = vec![Message {
            role: Role::User,
            parts: vec![MessagePart::Text("previous request".into())],
        }];
        context.selected_subagent = Some("reviewer".into());

        reset_tui_session(&mut context).expect("idle reset should synchronize the backend state");

        assert_eq!(context, TuiSessionContext::fresh());
    }

    #[test]
    fn dangerous_mode_is_visible_press_once_and_next_turn_only() {
        let temporary = tui_session_directory("dangerous-mode");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        tui.set_presentation("openai-api", "gpt-4.1", "new session");

        assert!(!render_tui_test_backend(&tui, 120, 24).contains("agens safe"));

        let Action::OpenDialog(route_id) = tui.handle(Event::Key(Key::CtrlShiftD)) else {
            panic!("Ctrl+Shift+D should route through the dangerous-mode router path");
        };
        assert_eq!(route_id, "dangerous");
        assert!(
            tui.apply_submission_outcome(router.route_request(
                TuiRouteRequest::OpenDialog(route_id),
                std::sync::mpsc::channel().0,
            ))
            .is_none()
        );
        assert!(session.lock().unwrap().dangerous_mode);
        assert!(render_tui_test_backend(&tui, 120, 24).contains("danger"));

        assert!(
            tui.apply_submission_outcome(router.route("/dangerous".into()))
                .is_none()
        );
        assert!(!session.lock().unwrap().dangerous_mode);
        assert!(!render_tui_test_backend(&tui, 120, 24).contains("agens safe"));

        tui.apply_submission_outcome(router.route("/dangerous".into()));
        let result = run_tui_prompt_with(&bootstrap, "next request", &session, None, |request| {
            assert!(request.dangerous_mode);
            assert!(matches!(
                router.route("/dangerous".into()),
                TuiSubmissionOutcome::ContextChanged { .. }
            ));
            assert!(request.dangerous_mode);
            Ok(HeadlessChatCompletion {
                text: "captured".into(),
                metadata: SessionMetadata {
                    id: 1,
                    project: "project".into(),
                    title: "captured".into(),
                    active_agent: "primary".into(),
                    provider_id: None,
                    model_id: None,
                    reasoning_effort: None,
                    created_at: 1,
                    updated_at: 1,
                    completed_turn_count: 1,
                    resumable: true,
                },
                messages: Vec::new(),
            })
        });
        assert!(result.is_ok());
        assert!(!session.lock().unwrap().dangerous_mode);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn dangerous_child_catalog_is_exact_and_never_recursive() {
        let temporary = tui_session_directory("dangerous-child-catalog");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let (provider_tools, dispatcher) =
            production_dangerous_child_tool_runtime(&project_root).unwrap();
        let provider_names = provider_tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        assert_eq!(
            provider_names,
            [
                "read", "list", "search", "glob", "grep", "write", "edit", "bash", "webfetch",
            ]
        );

        let dispatcher = dispatcher.lock().unwrap();
        for name in [
            "read", "list", "search", "glob", "grep", "write", "edit", "bash", "webfetch",
        ] {
            assert_eq!(
                dispatcher.canonical_identity(name),
                dispatcher.canonical_identity(&format!("native::{name}")),
                "{name} must have one native dispatcher identity"
            );
        }
        for rejected in [
            "task",
            "native::task",
            "skill",
            "native::skill",
            "files::first",
            "native::unregistered",
        ] {
            assert_eq!(
                dispatcher.canonical_identity(rejected),
                None,
                "{rejected} must be rejected before execution"
            );
            assert!(
                dispatcher
                    .evaluate(
                        &PermissionPolicy::new(
                            PermissionMode::Edit,
                            vec![PermissionRule::global(
                                PermissionDecision::Allow,
                                PermissionPattern::Any,
                                PermissionPattern::Any,
                            )],
                        ),
                        &[],
                        &PermissionSession::new(),
                        ToolDispatchRequest::new("project", rejected, serde_json::json!({})),
                    )
                    .is_err(),
                "{rejected} must fail dispatcher evaluation before execution"
            );
        }
        drop(dispatcher);

        let task_registry = TaskExecutionRegistry::new();
        let execution_id = task_registry.admit(TaskLaunchMode::Foreground).unwrap();
        let (mode_off_tools, mode_off_dispatcher) =
            production_child_tool_runtime(&project_root, false, task_registry, execution_id)
                .unwrap();
        assert_eq!(
            mode_off_tools
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>(),
            ["read", "task_control", "task_message"]
        );
        assert!(
            mode_off_dispatcher
                .lock()
                .unwrap()
                .canonical_identity("native::read")
                .is_some()
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn dangerous_override_never_precedes_hard_safety_or_reuses_authorization() {
        let ordinary_deny = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::Exact("native::write".into()),
                PermissionPattern::Any,
            )],
        );
        let ordinary = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "dangerous-ordinary-deny",
                Vec::new(),
                vec![native_batch_call(
                    "ordinary",
                    "native::write",
                    serde_json::json!({"path":"notes.md","content":"allowed"}),
                )],
            )
            .with_policy(ordinary_deny)
            .with_dangerous_override(),
        );
        assert!(ordinary.result.is_ok());
        assert!(ordinary.prompts.is_empty());
        assert_eq!(ordinary.executions, ["notes.md"]);

        let hard_global_deny = PermissionPolicy::with_safety_predicates(
            PermissionMode::Edit,
            Vec::new(),
            vec![agens_core::SafetyPredicate::GlobalDeny(Box::new(
                agens_core::GlobalDenyPredicate {
                    tool: PermissionPattern::Exact("native::write".into()),
                    target: PermissionPattern::Any,
                },
            ))],
        );
        let global = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "dangerous-global-deny",
                Vec::new(),
                vec![native_batch_call(
                    "global",
                    "native::write",
                    serde_json::json!({"path":"blocked.md","content":"blocked"}),
                )],
            )
            .with_policy(hard_global_deny)
            .with_dangerous_override(),
        );
        assert!(global.result.is_ok());
        assert!(global.prompts.is_empty());
        assert!(global.executions.is_empty());

        let chat = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "dangerous-chat-write",
                Vec::new(),
                vec![native_batch_call(
                    "chat",
                    "native::write",
                    serde_json::json!({"path":"blocked.md","content":"blocked"}),
                )],
            )
            .with_policy(PermissionPolicy::new(PermissionMode::Chat, Vec::new()))
            .with_dangerous_override(),
        );
        assert!(chat.result.is_ok());
        assert!(chat.prompts.is_empty());
        assert!(chat.executions.is_empty());

        for (name, input) in [
            ("native::write", "{malformed"),
            (
                "native::task",
                r#"{"agent":"worker","description":"recursive"}"#,
            ),
            ("mcp::server::tool", r#"{}"#),
            ("native::unregistered", r#"{}"#),
        ] {
            let rejected = run_production_batch_with_policy(
                ProductionBatchInput::new(
                    "dangerous-invalid",
                    Vec::new(),
                    vec![MessagePart::ToolCall {
                        id: "rejected".into(),
                        name: name.into(),
                        input: input.into(),
                    }],
                )
                .with_dangerous_override(),
            );
            assert_eq!(
                rejected.result,
                Err(HeadlessTurnError::PermissionEvaluation),
                "{name} must be rejected before policy bypass"
            );
            assert!(rejected.prompts.is_empty());
            assert!(rejected.executions.is_empty());
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
        dispatcher
            .lock()
            .unwrap()
            .register_native(
                "native::write",
                ToolAccess::Write,
                BatchTool {
                    name: "native::write".into(),
                    calls: Arc::clone(&calls),
                    cancellation: None,
                },
            )
            .unwrap();
        let allowed = Arc::new(Mutex::new(BTreeMap::new()));
        let mut gate = ProductionPermissionGate::new(
            PermissionPolicy::new(PermissionMode::Edit, Vec::new()),
            Arc::new(Mutex::new(Vec::new())),
            PermissionSession::new(),
            "project".into(),
            Arc::clone(&dispatcher),
            Arc::clone(&allowed),
            Arc::new(Mutex::new(BTreeMap::new())),
        )
        .with_dangerous_override(true);
        let mut tool_dispatcher = ProductionToolDispatcher::new(dispatcher, allowed);
        let call = HeadlessToolCall {
            id: "once".into(),
            name: "native::write".into(),
            input: r#"{"path":"once.md","content":"once"}"#.into(),
        };
        let cancellation = HeadlessTurnCancellation::default();

        assert_eq!(
            poll_permission_port(gate.evaluate(&call, &cancellation)),
            Ok(PermissionDecision::Allow)
        );
        assert!(
            poll_permission_port(tool_dispatcher.dispatch(call.clone(), &cancellation)).is_ok()
        );
        assert_eq!(
            poll_permission_port(tool_dispatcher.dispatch(call, &cancellation)),
            Err(HeadlessTurnPortError::Tool)
        );
        assert_eq!(*calls.lock().unwrap(), ["once.md"]);

        let oversized = "x".repeat(agens_core::MAX_PERMISSION_TARGET_BYTES + 1);
        let oversized_call = HeadlessToolCall {
            id: "oversized".into(),
            name: "native::write".into(),
            input: serde_json::json!({"path": oversized, "content": "blocked"}).to_string(),
        };
        assert_eq!(
            poll_permission_port(gate.evaluate(&oversized_call, &cancellation)),
            Err(HeadlessTurnPortError::Permission)
        );

        let temporary = tui_session_directory("dangerous-confined-write");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let (_, dispatcher) = production_dangerous_child_tool_runtime(&project_root).unwrap();
        let allowed = Arc::new(Mutex::new(BTreeMap::new()));
        let mut gate = ProductionPermissionGate::new(
            PermissionPolicy::new(PermissionMode::Edit, Vec::new()),
            Arc::new(Mutex::new(Vec::new())),
            PermissionSession::new(),
            "project".into(),
            Arc::clone(&dispatcher),
            Arc::clone(&allowed),
            Arc::new(Mutex::new(BTreeMap::new())),
        )
        .with_dangerous_override(true);
        let mut tool_dispatcher = ProductionToolDispatcher::new(dispatcher, allowed);
        let escape = HeadlessToolCall {
            id: "escape".into(),
            name: "native::write".into(),
            input: r#"{"path":"../escape.txt","content":"blocked"}"#.into(),
        };

        assert_eq!(
            poll_permission_port(gate.evaluate(&escape, &cancellation)),
            Ok(PermissionDecision::Allow)
        );
        assert!(
            poll_permission_port(tool_dispatcher.dispatch(escape, &cancellation))
                .expect("confined dispatcher should return a sanitized tool failure")
                .is_error
        );
        assert!(!temporary.join("escape.txt").exists());
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn production_tui_project_identity_uses_the_canonical_current_project_for_new_and_resumed_sessions()
     {
        let temporary =
            std::env::temp_dir().join(format!("agens-u18-project-header-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temporary);
        let project_root = temporary.join("non-agens-project");
        std::fs::create_dir_all(project_root.join(".git")).unwrap();
        let config_home = temporary.join("config");
        let project_bootstrap = bootstrap(&CliDependencies::for_test(
            project_root.clone(),
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            BTreeMap::from([(
                config_home.join("config.toml"),
                format!(
                    "[options]\ndata_dir = \"{}\"\n",
                    temporary.join("data").display()
                ),
            )]),
        ))
        .unwrap();
        let project = project_root.display().to_string();
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        configure_tui_project_identity(&mut tui, &project_bootstrap);
        assert_eq!(tui.view().project, project);
        tui.set_presentation("openai-api", "gpt-4.1", "new session");
        let new_session_header = render_tui_test_backend(&tui, 120, 24);
        assert!(
            new_session_header.contains("non-agens-project"),
            "{new_session_header:?}"
        );

        tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
            message: "Resumed session 7".into(),
            presentation: TuiPresentation::new("openai-api", "gpt-4.1", "session #7"),
            history: Vec::new(),
            draft: None,
            resume_error: None,
        });
        let resumed_session_header = render_tui_test_backend(&tui, 120, 24);
        assert!(
            resumed_session_header.contains("non-agens-project"),
            "{resumed_session_header:?}"
        );

        let no_project_directory = temporary.join("no-project");
        std::fs::create_dir_all(&no_project_directory).unwrap();
        let no_project_bootstrap = bootstrap(&CliDependencies::for_test(
            no_project_directory,
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            BTreeMap::new(),
        ))
        .unwrap();
        let mut fallback_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        configure_tui_project_identity(&mut fallback_tui, &no_project_bootstrap);
        assert_eq!(fallback_tui.view().project, "agens");
        let fallback_render = render_tui_test_backend(&fallback_tui, 120, 24);
        // Project basename lives in the operational footer (not "project …" header chrome).
        assert!(fallback_render.contains("agens"), "{fallback_render:?}");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_list_filters_current_project_and_resume_preserves_typed_history() {
        let temporary = tui_session_directory("filter-resume");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let current = persist_tui_session(&mut store, &tui_project(&temporary), "current");
        persist_tui_session(
            &mut store,
            &temporary.join("other").display().to_string(),
            "other",
        );

        assert_eq!(list_tui_sessions(&bootstrap).unwrap(), "1\t1 event(s)");

        reset_tui_resume_test_counters();
        let resumed = resume_tui_session(
            &bootstrap,
            current.id,
            &SkillCatalog::default(),
            &TuiCredentialResolver::production(),
        )
        .unwrap();
        assert_eq!(resumed.identifier, Some(current.id));
        assert_eq!(resumed.metadata, Some(current));
        assert_eq!(resumed.messages, tui_session_messages());
        assert!(resumed.active_agent.is_none());
        assert_eq!(resumed.restored_history.len(), 1);
        assert_eq!(tui_resume_test_counters(), (1, 1, 0, 0));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    fn session_attempt_count(store: &SessionStore) -> i64 {
        Connection::open(store.database_path())
            .unwrap()
            .query_row("SELECT COUNT(*) FROM session_attempts", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn assert_restored_retry_draft_ui(outcome: TuiSubmissionOutcome, retry_prompt: &str) {
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        assert!(tui.begin_session_load());
        assert!(tui.apply_submission_outcome(outcome).is_none());
        let view = tui.view();
        assert_eq!(view.input, retry_prompt);
        assert_eq!(view.focus, agens_tui::TranscriptFocus::Composer);
        assert!(view.following_bottom);
        assert_eq!(
            view.status,
            Some("Recovered failed prompt · Enter retry · Esc discard")
        );
        assert!(view.completed_conversations.is_empty());
        assert!(!view.running);
        let rendered = render_tui_test_backend(&tui, 120, 24);
        assert!(rendered.contains(retry_prompt), "{rendered:?}");
        assert!(
            rendered.contains("Recovered failed prompt · Enter retry · Esc discard"),
            "{rendered:?}"
        );
    }

    #[test]
    fn zero_turn_failed_tui_resume_restores_draft_without_runtime_or_attempt_creation() {
        let temporary = tui_session_directory("failed-draft-resume");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = SessionMetadata {
            id: 0,
            project: tui_project(&temporary),
            title: "failed".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 10,
            updated_at: 20,
            completed_turn_count: 0,
            resumable: false,
        };
        let retry_prompt = "retry exact café 🙂";
        let attempt = store
            .begin_session_attempt(&metadata, retry_prompt.into())
            .unwrap();
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::Failed, 30)
            .unwrap();
        let attempt_count = session_attempt_count(&store);
        drop(store);

        reset_tui_resume_test_counters();
        let loaded = load_tui_session_for_resume(&bootstrap, attempt.key().session_id()).unwrap();
        assert_eq!(
            loaded.retry_boundary.as_ref().map(RetryBoundary::prompt),
            Some(retry_prompt)
        );
        let prepared = prepare_loaded_tui_session_resume(
            &bootstrap,
            attempt.key().session_id(),
            loaded,
            &TuiCredentialResolver::production(),
        )
        .unwrap();
        assert_eq!(prepared.resume_draft.as_deref(), Some(retry_prompt));
        assert!(!format!("{prepared:?}").contains(retry_prompt));
        assert_eq!(
            prepared.note(),
            "Recovered failed prompt · Enter retry · Esc discard"
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let expected = session.lock().unwrap().clone();
        let outcome = commit_tui_session_resume(
            &bootstrap,
            &session,
            &expected,
            prepared,
            &TuiRouteCancellation::new(),
        )
        .unwrap();
        assert!(session.lock().unwrap().resume_draft.is_none());
        assert_restored_retry_draft_ui(outcome.clone(), retry_prompt);
        let TuiSubmissionOutcome::SessionResumed {
            message,
            history,
            draft,
            ..
        } = outcome
        else {
            panic!("expected resumed outcome");
        };
        assert_eq!(
            message,
            "Recovered failed prompt · Enter retry · Esc discard"
        );
        assert!(history.is_empty());
        assert_eq!(draft.as_deref(), Some(retry_prompt));
        assert_eq!(tui_resume_test_counters(), (1, 1, 0, 0));

        let reopened = SessionStore::open(bootstrap.data_directory()).unwrap();
        let unchanged_attempt_count = session_attempt_count(&reopened);
        assert_eq!(unchanged_attempt_count, attempt_count);
        assert_eq!(
            reopened
                .load_session_for_resume(attempt.key().session_id())
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            SessionAttemptStatus::Failed
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn completed_history_resume_adds_failed_draft_without_duplicate_user_message() {
        let temporary = tui_session_directory("history-failed-draft");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "history");
        let retry_prompt = "failed next prompt";
        let attempt = store
            .begin_session_attempt(&metadata, retry_prompt.into())
            .unwrap();
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::ProviderError, 40)
            .unwrap();
        drop(store);

        let loaded = load_tui_session_for_resume(&bootstrap, metadata.id).unwrap();
        let prepared = prepare_loaded_tui_session_resume(
            &bootstrap,
            metadata.id,
            loaded,
            &TuiCredentialResolver::production(),
        )
        .unwrap();
        assert_eq!(prepared.messages, tui_session_messages());
        assert_eq!(prepared.restored_history.len(), 1);
        assert_eq!(prepared.resume_draft.as_deref(), Some(retry_prompt));
        assert_eq!(
            prepared.note(),
            "Recovered failed prompt · Enter retry · Esc discard"
        );
        assert!(
            prepared
                .messages
                .iter()
                .all(|message| message.role != Role::User
                    || message.parts != [MessagePart::Text(retry_prompt.into())])
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn completed_resume_without_retry_draft_and_cancelled_timeout_taxonomy_stay_explicit() {
        let temporary = tui_session_directory("completed-no-draft");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "completed");
        drop(store);

        let loaded = load_tui_session_for_resume(&bootstrap, metadata.id).unwrap();
        assert!(loaded.retry_boundary.is_none());
        let prepared = prepare_loaded_tui_session_resume(
            &bootstrap,
            metadata.id,
            loaded,
            &TuiCredentialResolver::production(),
        )
        .unwrap();
        assert!(prepared.resume_draft.is_none());
        assert!(prepared.note().starts_with("Resumed session"));
        assert_eq!(
            resume_retry_notice(SessionAttemptStatus::Cancelled),
            Some("Recovered failed prompt · Enter retry · Esc discard")
        );
        assert_eq!(
            attempt_failure_status(&CliError::runtime(HeadlessTurnError::TimedOut)),
            SessionAttemptStatus::Cancelled
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_resume_commit_discards_cancelled_stale_and_invalid_preparations() {
        let temporary = tui_session_directory("atomic-resume");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "atomic");
        let attempt = store
            .begin_session_attempt(&metadata, "atomic preserved draft".into())
            .unwrap();
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::Failed, 30)
            .unwrap();
        drop(store);
        let credentials = TuiCredentialResolver::production();
        let prepared = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &credentials,
        )
        .unwrap();
        assert_eq!(
            prepared.resume_draft.as_deref(),
            Some("atomic preserved draft")
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let original = session.lock().unwrap().clone();

        let cancelled = TuiRouteCancellation::new();
        cancelled.cancel();
        assert_eq!(
            commit_tui_session_resume(
                &bootstrap,
                &session,
                &original,
                prepared.clone(),
                &cancelled,
            )
            .unwrap(),
            TuiSubmissionOutcome::RouteCancelled
        );
        assert_eq!(*session.lock().unwrap(), original);

        session.lock().unwrap().dangerous_mode = true;
        let newer = session.lock().unwrap().clone();
        assert_eq!(
            commit_tui_session_resume(
                &bootstrap,
                &session,
                &original,
                prepared.clone(),
                &TuiRouteCancellation::new(),
            )
            .unwrap(),
            TuiSubmissionOutcome::RouteCancelled
        );
        assert_eq!(*session.lock().unwrap(), newer);

        *session.lock().unwrap() = original.clone();
        let accepted = TuiRouteCancellation::new();
        assert!(matches!(
            commit_tui_session_resume(&bootstrap, &session, &original, prepared, &accepted,)
                .unwrap(),
            TuiSubmissionOutcome::SessionResumed { .. }
        ));
        assert!(!accepted.cancel());
        let committed = session.lock().unwrap();
        assert_eq!(committed.identifier, Some(metadata.id));
        assert_eq!(committed.messages, tui_session_messages());
        assert!(committed.restored_history.is_empty());
        drop(committed);

        let mut invalid = load_tui_session_for_resume(&bootstrap, metadata.id).unwrap();
        invalid.session.messages = vec![Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("orphan".into())],
        }];
        let before_error = session.lock().unwrap().clone();
        assert!(
            prepare_loaded_tui_session_resume(&bootstrap, metadata.id, invalid, &credentials,)
                .is_err()
        );
        assert_eq!(*session.lock().unwrap(), before_error);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn first_runtime_materialization_after_resume_preserves_permission_denial() {
        let temporary = tui_session_directory("lazy-resume-runtime");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "lazy");
        drop(store);
        let skills = SkillCatalog::default();
        reset_tui_resume_test_counters();
        let resumed = resume_tui_session(
            &bootstrap,
            metadata.id,
            &skills,
            &TuiCredentialResolver::production(),
        )
        .unwrap();
        assert_eq!(tui_resume_test_counters(), (1, 1, 0, 0));
        let session = Arc::new(Mutex::new(resumed));
        let (permission_bridge, _) = TuiPermissionBridge::channel();
        let (events, _) = BridgeTx::bounded(8);
        let runtime = production_tui_task_runtime(
            &bootstrap,
            &skills,
            permission_bridge,
            TuiTaskLifecycleBridge::new(events, TuiTaskControls::default()),
            agens_core::RequestConfig::default(),
            "abc12345".to_owned(),
        )
        .unwrap();
        ensure_active_tui_agent_runtime(&bootstrap, &session, &runtime.dispatcher).unwrap();
        assert_eq!(tui_resume_test_counters(), (1, 1, 1, 0));
        assert_eq!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );

        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let outcome = runtime
            .dispatcher
            .lock()
            .unwrap()
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new(
                    tui_project(&temporary),
                    "task",
                    serde_json::json!({"agent":"explore","description":"inspect"}),
                ),
            )
            .unwrap();
        assert!(matches!(outcome, ToolEvaluationOutcome::Denied));
        assert_eq!(tui_resume_test_counters(), (1, 1, 1, 0));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn resumed_primary_inherits_every_effective_pinned_model_and_compatible_effort() {
        for provider in ["openai-api", "openai-chatgpt"] {
            for model in [
                "gpt-5.5",
                "gpt-5.6",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
            ] {
                let temporary =
                    tui_session_directory(&format!("resume-primary-{provider}-{model}"));
                let bootstrap =
                    tui_session_bootstrap_for_provider(&temporary, &[], provider, model);
                let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
                let mut metadata =
                    persist_tui_session(&mut store, &tui_project(&temporary), "inherited");
                metadata.provider_id = Some(provider.into());
                metadata.model_id = Some(model.into());
                metadata.reasoning_effort = Some(agens_core::ReasoningEffort::High);
                store.update_session_selection(&metadata).unwrap();
                drop(store);

                let resumed = resume_tui_session(
                    &bootstrap,
                    metadata.id,
                    &SkillCatalog::default(),
                    &TuiCredentialResolver::production(),
                )
                .unwrap();
                assert!(resumed.active_agent.is_none());
                let session = Arc::new(Mutex::new(resumed));
                let dispatcher = Arc::new(Mutex::new(rotation_dispatcher()));

                ensure_active_tui_agent_runtime(&bootstrap, &session, &dispatcher).unwrap();

                let context = session.lock().unwrap();
                let active = context.active_agent.as_ref().unwrap();
                assert_eq!(active.name, "primary", "{provider} {model}");
                assert_eq!(active.model.as_deref(), Some(model), "{provider} {model}");
                let request =
                    context.apply_to(parse_chat_request(&["first submission".into()]).unwrap());
                assert_eq!(request.model.as_deref(), Some(model), "{provider} {model}");
                assert_eq!(
                    request.request_config.reasoning_effort(),
                    Some(agens_core::ReasoningEffort::High),
                    "{provider} {model}"
                );
                drop(context);

                std::fs::remove_dir_all(temporary).unwrap();
            }
        }
    }

    fn remember(bootstrap: &Bootstrap, model: &str, effort: Option<agens_core::ReasoningEffort>) {
        PreferenceStore::open(bootstrap.data_directory())
            .unwrap()
            .remember_model(&ModelPreference::new(model, effort))
            .unwrap();
    }

    #[test]
    fn a_new_session_inherits_the_remembered_model_and_its_effort() {
        let temporary = tui_session_directory("remembered-selection-fresh");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.model = None;
        remember(
            &bootstrap,
            "gpt-5.5",
            Some(agens_core::ReasoningEffort::High),
        );
        let mut context = TuiSessionContext::fresh();

        assert_eq!(
            seed_remembered_tui_selection(&bootstrap, &mut context),
            None
        );

        assert_eq!(effective_tui_model(&bootstrap, &context), "gpt-5.5");
        let request = context.apply_to(parse_chat_request(&["work".into()]).unwrap());
        assert_eq!(request.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(
            request.session_reasoning_effort,
            Some(agens_core::ReasoningEffort::High)
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_configured_or_flagged_model_outranks_the_remembered_one() {
        let temporary = tui_session_directory("remembered-selection-outranked");
        let configured = tui_session_bootstrap(&temporary, &[]);
        remember(
            &configured,
            "gpt-5.5",
            Some(agens_core::ReasoningEffort::High),
        );
        let mut context = TuiSessionContext::fresh();

        assert_eq!(
            seed_remembered_tui_selection(&configured, &mut context),
            None
        );
        assert!(context.selection.is_none());
        assert_eq!(effective_tui_model(&configured, &context), "gpt-4.1");

        // A model flag reaches the same resolved slot as a configured model, so it outranks the
        // remembered pick through the same branch.
        let mut flagged = configured.clone();
        flagged.model = Some("o3".into());
        let mut context = TuiSessionContext::fresh();

        assert_eq!(seed_remembered_tui_selection(&flagged, &mut context), None);
        assert!(context.selection.is_none());
        assert_eq!(effective_tui_model(&flagged, &context), "o3");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn an_unavailable_remembered_model_falls_back_visibly() {
        let temporary = tui_session_directory("remembered-selection-unavailable");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.model = None;
        remember(&bootstrap, "gpt-5.4", None);
        let mut context = TuiSessionContext::fresh();

        assert_eq!(
            seed_remembered_tui_selection(&bootstrap, &mut context),
            Some(
                "Remembered model gpt-5.4 is unavailable for OpenAI API; using gpt-4.1.".to_owned()
            )
        );
        assert!(context.selection.is_none());
        assert_eq!(effective_tui_model(&bootstrap, &context), "gpt-4.1");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn an_effort_the_remembered_model_lost_falls_back_visibly() {
        let temporary = tui_session_directory("remembered-selection-effort");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.model = None;
        remember(
            &bootstrap,
            "gpt-4.1",
            Some(agens_core::ReasoningEffort::High),
        );
        let mut context = TuiSessionContext::fresh();

        assert_eq!(
            seed_remembered_tui_selection(&bootstrap, &mut context),
            Some(
                "Remembered reasoning effort is unsupported by gpt-4.1; using Default.".to_owned()
            )
        );
        let selection = context.selection.as_ref().unwrap();
        assert_eq!(selection.model(), "gpt-4.1");
        assert_eq!(selection.reasoning_effort(), None);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn choosing_a_model_and_an_effort_remembers_both_for_the_next_session() {
        let temporary = tui_session_directory("remembered-selection-write");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.model = None;
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));

        apply_tui_model(&bootstrap, "gpt-5.5", &session).unwrap();
        apply_tui_effort(&bootstrap, "high", &session).unwrap();

        let remembered = PreferenceStore::open(bootstrap.data_directory())
            .unwrap()
            .remembered_model()
            .unwrap()
            .unwrap();
        assert_eq!(remembered.model(), "gpt-5.5");
        assert_eq!(
            remembered.reasoning_effort(),
            Some(agens_core::ReasoningEffort::High)
        );

        let mut context = TuiSessionContext::fresh();
        assert_eq!(
            seed_remembered_tui_selection(&bootstrap, &mut context),
            None
        );
        assert_eq!(effective_tui_model(&bootstrap, &context), "gpt-5.5");
        assert_eq!(
            context
                .selection
                .as_ref()
                .and_then(TuiModelSelector::reasoning_effort),
            Some("high")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn model_switch_invalidates_and_rematerializes_inherited_primary_without_stale_model() {
        let temporary = tui_session_directory("active-agent-model-switch");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let dispatcher = Arc::new(Mutex::new(rotation_dispatcher()));
        ensure_active_tui_agent_runtime(&bootstrap, &session, &dispatcher).unwrap();
        assert_eq!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .and_then(|agent| agent.model.as_deref()),
            Some("gpt-5.5")
        );

        apply_tui_model(&bootstrap, "gpt-5.6-sol", &session).unwrap();
        assert!(session.lock().unwrap().active_agent.is_none());
        ensure_active_tui_agent_runtime(&bootstrap, &session, &dispatcher).unwrap();

        let context = session.lock().unwrap();
        assert_eq!(
            context
                .active_agent
                .as_ref()
                .and_then(|agent| agent.model.as_deref()),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            context
                .selection
                .as_ref()
                .unwrap()
                .reasoning_effort_default(),
            Some("medium")
        );
        drop(context);
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn stale_persisted_agent_falls_back_to_primary_warns_and_persists_correction() {
        let temporary = tui_session_directory("stale-active-agent-fallback");
        let stale_definition = "---\nname: retired\ndescription: retired\nmode: primary\npermissions:\n  - allow native::read\n---\nRetired work.\n";
        let bootstrap = tui_session_bootstrap(&temporary, &[("retired", stale_definition)]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session_metadata(
            &mut store,
            &tui_project(&temporary),
            "stale",
            "retired",
            100,
        );
        drop(store);
        std::fs::remove_file(
            bootstrap
                .paths
                .global_config
                .with_file_name("agents")
                .join("retired.md"),
        )
        .unwrap();

        let resumed = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &TuiCredentialResolver::production(),
        )
        .unwrap();

        assert_eq!(
            resumed.note(),
            "Agent 'retired' is unavailable; resumed with primary."
        );
        assert_eq!(resumed.metadata.as_ref().unwrap().active_agent, "primary");
        assert!(resumed.active_agent.is_none());
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .active_agent,
            "retired"
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let expected = session.lock().unwrap().clone();
        let outcome = commit_tui_session_resume(
            &bootstrap,
            &session,
            &expected,
            resumed,
            &TuiRouteCancellation::new(),
        )
        .unwrap();
        assert!(matches!(
            outcome,
            TuiSubmissionOutcome::SessionResumed { message, .. }
                if message == "Agent 'retired' is unavailable; resumed with primary."
        ));
        ensure_active_tui_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .active_agent,
            "primary"
        );
        assert!(!session.lock().unwrap().agent_correction_pending);
        assert!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .unwrap()
                .capabilities
                .descriptors()
                .is_empty()
        );
        let diagnostics = std::fs::read_to_string(
            bootstrap
                .data_directory()
                .join("diagnostics")
                .join(format!("agens-{}.jsonl", std::process::id())),
        )
        .unwrap();
        assert!(diagnostics.contains(r#""event":"agent_fallback""#));
        assert!(!diagnostics.contains("Retired work"));
        assert!(!diagnostics.contains(&tui_project(&temporary)));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn explicit_unavailable_agent_model_and_ineligible_primary_are_hard_errors() {
        for (case, definition, active_agent, expected) in [
            (
                "explicit-model",
                "---\nname: reviewer\ndescription: reviewer\nmode: primary\nmodel: gpt-4o\npermissions: []\n---\nReview.\n",
                "reviewer",
                "agent model is unavailable",
            ),
            (
                "ineligible-primary",
                "---\nname: primary\ndescription: primary\nmode: subagent\npermissions: []\n---\nWrong mode.\n",
                "primary",
                "primary agent is unavailable",
            ),
        ] {
            let temporary = tui_session_directory(case);
            let bootstrap = tui_session_bootstrap_for_provider(
                &temporary,
                &[(active_agent, definition)],
                "openai-chatgpt",
                "gpt-5.5",
            );
            let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
            let metadata = persist_tui_session_metadata(
                &mut store,
                &tui_project(&temporary),
                case,
                active_agent,
                100,
            );
            drop(store);

            let error = resume_tui_session(
                &bootstrap,
                metadata.id,
                &SkillCatalog::default(),
                &TuiCredentialResolver::production(),
            )
            .unwrap_err();
            assert_eq!(error.message, expected, "{case}");
            assert_eq!(
                SessionStore::open(bootstrap.data_directory())
                    .unwrap()
                    .load_session_for_resume(metadata.id)
                    .unwrap()
                    .metadata
                    .active_agent,
                active_agent,
                "{case}"
            );
            let diagnostics = std::fs::read_to_string(
                bootstrap
                    .data_directory()
                    .join("diagnostics")
                    .join(format!("agens-{}.jsonl", std::process::id())),
            )
            .unwrap();
            assert!(diagnostics.contains(r#""event":"agent_unavailable""#));
            assert!(!diagnostics.contains(definition));

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    #[test]
    fn explicit_agent_models_use_the_provider_aware_effective_registry() {
        for (provider, model, expected_effort) in [
            ("openai-api", "gpt-4o", None),
            ("openai-chatgpt", "gpt-5.4", None),
            ("openai-api", "gpt-5.6-luna", None),
            ("openai-chatgpt", "gpt-5.6-luna", None),
            (
                "openai-api",
                "gpt-5.5",
                Some(agens_core::ReasoningEffort::High),
            ),
            (
                "openai-chatgpt",
                "gpt-5.5",
                Some(agens_core::ReasoningEffort::High),
            ),
        ] {
            let temporary = tui_session_directory(&format!("explicit-{provider}-{model}"));
            let definition = format!(
                "---\nname: reviewer\ndescription: reviewer\nmode: primary\nmodel: {model}\npermissions: []\n---\nReview.\n"
            );
            let bootstrap = tui_session_bootstrap_for_provider(
                &temporary,
                &[("reviewer", &definition)],
                provider,
                "gpt-5.5",
            );
            let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
            let mut metadata = persist_tui_session_metadata(
                &mut store,
                &tui_project(&temporary),
                "explicit",
                "reviewer",
                100,
            );
            metadata.provider_id = Some(provider.into());
            metadata.model_id = Some("gpt-5.5".into());
            metadata.reasoning_effort = Some(agens_core::ReasoningEffort::High);
            store.update_session_selection(&metadata).unwrap();
            drop(store);
            let resumed = resume_tui_session(
                &bootstrap,
                metadata.id,
                &SkillCatalog::default(),
                &TuiCredentialResolver::production(),
            )
            .unwrap();
            let session = Arc::new(Mutex::new(resumed));

            ensure_active_tui_agent_runtime(
                &bootstrap,
                &session,
                &Arc::new(Mutex::new(rotation_dispatcher())),
            )
            .unwrap();

            let context = session.lock().unwrap();
            assert_eq!(context.active_agent.as_ref().unwrap().name, "reviewer");
            assert_eq!(
                context.active_agent.as_ref().unwrap().model.as_deref(),
                Some(model),
                "{provider} {model}"
            );
            let request = context.apply_to(parse_chat_request(&["review".into()]).unwrap());
            assert_eq!(request.model.as_deref(), Some(model), "{provider} {model}");
            assert_eq!(
                request.request_config.reasoning_effort(),
                expected_effort,
                "{provider} {model}"
            );
            drop(context);
            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    #[test]
    fn explicit_agent_missing_keeps_active_primary_and_persisted_metadata_unchanged() {
        let temporary = tui_session_directory("explicit-agent-missing");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "primary");
        drop(store);
        let resumed = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &TuiCredentialResolver::production(),
        )
        .unwrap();
        let session = Arc::new(Mutex::new(resumed));
        ensure_active_tui_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();
        let before = session.lock().unwrap().clone();

        let error = rotate_tui_agent(&bootstrap, "missing", &session, &SkillCatalog::default())
            .unwrap_err();

        assert_eq!(error.category, "usage");
        assert_eq!(*session.lock().unwrap(), before);
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .active_agent,
            "primary"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn barrier_resume_loader_is_local_and_discards_its_late_cancelled_result() {
        let temporary = tui_session_directory("barrier-resume");
        let stale_definition = "---\nname: retired\ndescription: retired\nmode: primary\npermissions: []\n---\nRetired.\n";
        let bootstrap = tui_session_bootstrap(&temporary, &[("retired", stale_definition)]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session_metadata(
            &mut store,
            &tui_project(&temporary),
            "barrier",
            "retired",
            100,
        );
        drop(store);
        std::fs::remove_file(
            bootstrap
                .paths
                .global_config
                .with_file_name("agents")
                .join("retired.md"),
        )
        .unwrap();
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let original = session.lock().unwrap().clone();
        let cancellation = TuiRouteCancellation::new();
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let bootstrap = bootstrap.clone();
            let session = Arc::clone(&session);
            let original = original.clone();
            let cancellation = cancellation.clone();
            move || {
                reset_tui_resume_test_counters();
                started_sender.send(()).unwrap();
                release_receiver.recv().unwrap();
                let prepared = resume_tui_session(
                    &bootstrap,
                    metadata.id,
                    &SkillCatalog::default(),
                    &TuiCredentialResolver::production(),
                )
                .unwrap();
                let outcome = commit_tui_session_resume(
                    &bootstrap,
                    &session,
                    &original,
                    prepared,
                    &cancellation,
                )
                .unwrap();
                (outcome, tui_resume_test_counters())
            }
        });
        started_receiver.recv().unwrap();

        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        tui.set_presentation("old-provider", "old-model", "session #1");
        tui.begin_submission("old prompt");
        tui.finish_submission(Ok("old answer".into()));
        for character in "preserved draft".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        assert!(tui.begin_session_load());
        assert!(tui.view().session_loading);
        assert!(!tui.view().running);
        assert_eq!(tui.view().conversation.unwrap().user, "old prompt");

        assert!(cancellation.cancel());
        tui.cancel_session_load();
        release_sender.send(()).unwrap();
        let (outcome, counters) = worker.join().unwrap();
        assert_eq!(outcome, TuiSubmissionOutcome::RouteCancelled);
        assert_eq!(counters, (1, 1, 0, 0));
        assert_eq!(*session.lock().unwrap(), original);
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .active_agent,
            "retired"
        );
        assert_eq!(tui.view().provider_model, "old-provider / old-model");
        assert_eq!(tui.input(), "preserved draft");
        assert_eq!(tui.view().conversation.unwrap().user, "old prompt");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_resume_fails_closed_for_cross_project_missing_and_legacy_records() {
        let temporary = tui_session_directory("fail-closed");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        persist_tui_session(
            &mut store,
            &temporary.join("other").display().to_string(),
            "other",
        );
        let saved_sessions = store.list_sessions().unwrap();
        drop(store);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let original = session.lock().unwrap().clone();

        for command in ["/resume 1", "/resume 2"] {
            assert_eq!(
                run_tui_prompt(
                    &bootstrap,
                    command,
                    &HeadlessTurnCancellation::new(),
                    &session,
                    None,
                )
                .unwrap_err()
                .to_string(),
                "store: saved session is unavailable"
            );
            assert_eq!(*session.lock().unwrap(), original);
            assert_eq!(
                SessionStore::open(bootstrap.data_directory())
                    .unwrap()
                    .list_sessions()
                    .unwrap(),
                saved_sessions
            );
        }

        let legacy_temporary = tui_session_directory("legacy-fail-closed");
        let legacy_bootstrap = tui_session_bootstrap(&legacy_temporary, &[]);
        let mut legacy_store = SessionStore::open(legacy_bootstrap.data_directory()).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(
                legacy_store.persist_completed_turn(
                    CompletedTurnSnapshot::from_persisted_events(vec![
                        TurnEvent::StateChanged(TurnState::Requesting),
                        TurnEvent::StateChanged(TurnState::Streaming),
                        TurnEvent::ProviderPart(MessagePart::Text("legacy answer".into())),
                        TurnEvent::StateChanged(TurnState::Completed),
                    ])
                    .unwrap(),
                ),
            )
            .unwrap();
        drop(legacy_store);
        let legacy_session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let legacy_original = legacy_session.lock().unwrap().clone();
        assert_eq!(
            run_tui_prompt(
                &legacy_bootstrap,
                "/resume 1",
                &HeadlessTurnCancellation::new(),
                &legacy_session,
                None,
            )
            .unwrap_err()
            .to_string(),
            "store: saved session is unavailable"
        );
        assert_eq!(*legacy_session.lock().unwrap(), legacy_original);

        std::fs::remove_dir_all(temporary).unwrap();
        std::fs::remove_dir_all(legacy_temporary).unwrap();
    }

    #[test]
    fn tui_session_busy_resume_and_subagent_commands_leave_context_unchanged() {
        let temporary = tui_session_directory("busy");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext {
            identifier: Some(7),
            selected_subagent: Some("reviewer".into()),
            running: true,
            ..TuiSessionContext::fresh()
        }));
        let original = session.lock().unwrap().clone();

        for command in ["/resume 1", "/subagent reviewer"] {
            assert_eq!(
                run_tui_prompt(
                    &bootstrap,
                    command,
                    &HeadlessTurnCancellation::new(),
                    &session,
                    None,
                )
                .unwrap_err()
                .to_string(),
                "runtime: headless turn entered an invalid state"
            );
            assert_eq!(*session.lock().unwrap(), original);
        }

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_agent_selectors_expose_only_eligible_deterministic_options() {
        let temporary = tui_session_directory("agent-selectors");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[
                (
                    "all",
                    "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
                ),
                (
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                ),
            ],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));

        assert_eq!(
            list_tui_agents(&bootstrap, &session, AgentMode::Primary).unwrap(),
            "Active agent: none. Available: primary, all."
        );
        assert_eq!(
            list_tui_agents(&bootstrap, &session, AgentMode::Subagent).unwrap(),
            "Subagent: none. Available: explore, general, reviewer."
        );

        let no_agents_temporary = tui_session_directory("no-agent-selectors");
        let no_subagents = tui_session_bootstrap(&no_agents_temporary, &[]);
        assert_eq!(
            list_tui_agents(&no_subagents, &session, AgentMode::Subagent).unwrap(),
            "Subagent: none. Available: explore, general."
        );

        std::fs::remove_dir_all(temporary).unwrap();
        std::fs::remove_dir_all(no_agents_temporary).unwrap();
    }

    #[test]
    fn tui_session_agent_command_rotates_to_an_eligible_primary_agent() {
        let temporary = tui_session_directory("agent-command");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "all",
                "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
            )],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/agent all",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap(),
            "Active agent: all."
        );
        assert_eq!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("all")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_subagent_command_selects_an_exact_subagent() {
        let temporary = tui_session_directory("subagent-command");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        ensure_active_tui_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/subagent reviewer",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap(),
            "Subagent: reviewer."
        );
        assert_eq!(
            session.lock().unwrap().selected_subagent.as_deref(),
            Some("reviewer")
        );
        assert_eq!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn u15_c1a_subagent_overlay_and_alias_expose_only_eligible_agents() {
        let temporary = tui_session_directory("u15-c1a-subagents");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[
                (
                    "all",
                    "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
                ),
                (
                    "primary",
                    "---\nname: primary\ndescription: primary\nmode: primary\npermissions: []\n---\nPrimary work.\n",
                ),
                (
                    "invalid-model",
                    "---\nname: invalid-model\ndescription: invalid\nmode: subagent\nmodel: unavailable\npermissions: []\n---\nInvalid work.\n",
                ),
                (
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                ),
            ],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        assert!(
            router
                .palette_entries()
                .iter()
                .any(|entry| entry.name() == "subagent")
        );

        assert!(matches!(
            router.route("/subagent".into()),
            TuiSubmissionOutcome::SafeDialog(_)
        ));
        tui.set_running(true);
        assert!(
            tui.apply_submission_outcome(router.route("/subagent".into()))
                .is_none()
        );
        assert!(tui.view().running);
        let overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(!overlay.contains("main"));
        assert!(overlay.contains("explore"));
        assert!(overlay.contains("general"));
        assert!(overlay.contains("reviewer"));
        assert!(!overlay.contains("all"));
        assert!(!overlay.contains("primary"));
        assert!(!overlay.contains("invalid-model"));
        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::DialogAction("subagent:explore".into())
        );
        assert!(tui.transcript().is_empty());

        assert!(matches!(
            router.route("/subagent reviewer".into()),
            TuiSubmissionOutcome::ContextChanged { .. }
        ));
        assert_eq!(
            session.lock().unwrap().selected_subagent.as_deref(),
            Some("reviewer")
        );
        assert!(matches!(
            router.route("/subagent all".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ));

        let unavailable_bootstrap = tui_session_bootstrap_for_provider(
            &temporary,
            &[(
                "unavailable-provider",
                "---\nname: unavailable-provider\ndescription: unavailable\nmode: subagent\npermissions: []\n---\nUnavailable work.\n",
            )],
            "unavailable-provider",
            "gpt-4.1",
        );
        let unavailable_session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let unavailable_router = TuiRuntimeRouter::new(
            unavailable_bootstrap.clone(),
            Arc::clone(&unavailable_session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );

        assert!(
            !unavailable_router
                .palette_entries()
                .iter()
                .any(|entry| entry.name() == "subagent")
        );

        let unavailable_selection =
            unavailable_router.route("/subagent unavailable-provider".into());
        assert!(matches!(
            &unavailable_selection,
            TuiSubmissionOutcome::LocalActionableError { message, .. }
                if message.contains("No eligible subagents")
        ));
        assert!(
            unavailable_session
                .lock()
                .unwrap()
                .selected_subagent
                .is_none()
        );
        assert!(unavailable_session.lock().unwrap().messages.is_empty());

        let mut unavailable_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        let captured = Arc::new(Mutex::new(Vec::new()));
        submit_tui_command(
            &mut unavailable_tui,
            &unavailable_router,
            &unavailable_bootstrap,
            "/subagent unavailable-provider",
            &captured,
        );
        assert!(captured.lock().unwrap().is_empty());
        assert!(!unavailable_tui.view().running);

        let empty_selection =
            unavailable_tui.apply_submission_outcome(unavailable_router.route("/subagent".into()));
        assert_eq!(empty_selection, None);
        let unavailable_overlay = render_tui_test_backend(&unavailable_tui, 80, 24);
        assert!(
            unavailable_overlay.contains("No eligible subagents are available."),
            "{unavailable_overlay:?}"
        );
        assert_eq!(
            unavailable_tui.handle(Event::Key(Key::Enter)),
            Action::Render
        );

        unavailable_tui.apply_submission_outcome(unavailable_router.route("/subagent".into()));
        assert_eq!(
            unavailable_tui.handle(Event::Key(Key::Escape)),
            Action::Render
        );
        assert!(unavailable_tui.transcript().is_empty());
        let unavailable_context = unavailable_session.lock().unwrap();
        assert!(unavailable_context.selected_subagent.is_none());
        assert!(unavailable_context.messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn plural_subagents_command_opens_the_transcript_picker_without_changing_next_type() {
        let temporary = tui_session_directory("plural-subagents");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext {
            selected_subagent: Some("explore".into()),
            ..TuiSessionContext::fresh()
        }));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );

        assert!(matches!(
            router.route("/subagents".into()),
            TuiSubmissionOutcome::TranscriptDialog
        ));
        assert_eq!(
            session.lock().unwrap().selected_subagent.as_deref(),
            Some("explore")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_new_command_synchronizes_idle_context() {
        let temporary = tui_session_directory("new-command");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let dispatcher = rotation_dispatcher();
        let active_agent = ActiveAgentRuntime::build(
            &rotation_agent("primary", Some("gpt-4.1"), true),
            None,
            &tui_project(&temporary),
            &dispatcher,
            &BundledModelValidator,
        )
        .unwrap();
        let session = Arc::new(Mutex::new(TuiSessionContext {
            identifier: Some(7),
            metadata: Some(SessionMetadata {
                id: 7,
                project: tui_project(&temporary),
                title: "conversation".into(),
                active_agent: "primary".into(),
                provider_id: None,
                model_id: None,
                reasoning_effort: None,
                created_at: 1,
                updated_at: 1,
                completed_turn_count: 1,
                resumable: true,
            }),
            messages: tui_session_messages(),
            active_agent: Some(active_agent),
            pending_system_reminder: Some("previous reminder".into()),
            selection: Some(TuiModelSelector::new("gpt-4.1")),
            selected_subagent: Some("reviewer".into()),
            ..TuiSessionContext::fresh()
        }));

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/new",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap(),
            "Started a new session."
        );
        assert_eq!(*session.lock().unwrap(), TuiSessionContext::fresh());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_enter_routes_unknown_slash_and_local_output_without_provider_history() {
        let temporary = tui_session_directory("enter-local-routing");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "current");
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine { cancellation });
        let input = enter_tui_input(&mut tui, "/unknown");
        let provider_invocations =
            usize::from(tui.apply_submission_outcome(router.route(input)).is_some());
        assert_eq!(provider_invocations, 0);
        assert!(tui.transcript().is_empty());
        assert!(tui.view().dialog.is_some());

        session.lock().unwrap().running = true;
        let input = enter_tui_input(&mut tui, "/new");
        tui.apply_submission_outcome(router.route(input));
        assert!(tui.view().dialog.is_some());

        session.lock().unwrap().running = false;
        let input = enter_tui_input(&mut tui, "/new");
        tui.apply_submission_outcome(router.route(input));
        assert!(tui.transcript().is_empty());
        assert_eq!(tui.view().status, Some("Started a new session."));

        let input = enter_tui_input(&mut tui, &format!("/resume {}", metadata.id));
        tui.apply_submission_outcome(router.route(input));
        assert_eq!(tui.view().session, format!("session #{}", metadata.id));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_startup_commands_route_real_enter_to_captured_provider_requests() {
        let temporary = tui_session_directory("declarative-commands");
        let config_home = temporary.join("config");
        let global_commands = config_home.join("commands");
        let project_commands = temporary.join("project/.agens/commands");
        std::fs::create_dir_all(&global_commands).unwrap();
        std::fs::create_dir_all(&project_commands).unwrap();
        for (root, name, description, template) in [
            (&global_commands, "shared", "global", "global:$ARGUMENTS"),
            (
                &global_commands,
                "global-only",
                "global only",
                "Keep literal text [$ARGUMENTS]",
            ),
            (
                &global_commands,
                "slash-template",
                "literal slash",
                "/literal $ARGUMENTS",
            ),
            (
                &global_commands,
                "connect",
                "collision",
                "must not run $ARGUMENTS",
            ),
            (&project_commands, "shared", "project", "project:$ARGUMENTS"),
        ] {
            write_tui_command(root, name, description, template);
        }
        std::fs::write(
            project_commands.join("broken.md"),
            "---\ndescription: [invalid\n---\nbroken\n",
        )
        .unwrap();

        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let commands = start_tui_commands(&mut tui, &bootstrap).unwrap();
        assert!(tui.view().dialog.is_some());
        assert!(tui.transcript().is_empty());
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            cancellation,
            commands,
            Arc::new(SkillCatalog::default()),
        );
        let captured = Arc::new(Mutex::new(Vec::new()));

        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/shared   hello world   ",
            &captured,
        );
        assert!(tui.transcript().contains(&agens_tui::TranscriptEntry::User(
            "/shared   hello world   ".into()
        )));
        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/global-only   value   ",
            &captured,
        );
        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/slash-template text",
            &captured,
        );

        let requests = captured.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.prompt.as_str())
                .collect::<Vec<_>>(),
            vec![
                "project:hello world",
                "Keep literal text [value]",
                "/literal text",
            ]
        );
        drop(requests);

        for input in ["/connect custom", "/unknown"] {
            submit_tui_command(&mut tui, &router, &bootstrap, input, &captured);
        }
        assert_eq!(captured.lock().unwrap().len(), 3);
        assert!(tui.view().dialog.is_some());
        assert!(session.lock().unwrap().messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_startup_skills_reach_parent_context_and_tool_with_builtin_subagents() {
        let temporary = tui_session_directory("parent-skills");
        let config_home = temporary.join("config");
        let global_skills = config_home.join("skills");
        let project_skills = temporary.join("project/.agens/skills");
        write_tui_skill(
            &global_skills,
            "alpha",
            "global alpha",
            "GLOBAL_ALPHA_BODY_SENTINEL",
        );
        write_tui_skill(
            &global_skills,
            "shared",
            "global shared",
            "GLOBAL_SHARED_BODY_SENTINEL",
        );
        write_tui_skill(
            &global_skills,
            "invoke",
            "global invoke",
            "GLOBAL_INVOKE_BODY_SENTINEL",
        );
        write_tui_skill(
            &project_skills,
            "shared",
            "project shared",
            "PROJECT_SHARED_BODY_SENTINEL",
        );
        write_tui_skill(
            &project_skills,
            "invoke",
            "project invoke",
            "PROJECT_INVOKE_BODY_SENTINEL",
        );
        write_tui_skill(
            &project_skills,
            "broken",
            "broken after startup",
            "BROKEN_BODY_SENTINEL",
        );
        let global_commands = config_home.join("commands");
        std::fs::create_dir_all(&global_commands).unwrap();
        write_tui_command(
            &global_commands,
            "shared",
            "command wins",
            "COMMAND:$ARGUMENTS",
        );
        std::fs::create_dir_all(project_skills.join("shared/references")).unwrap();
        std::fs::write(
            project_skills.join("shared/references/guide.md"),
            "RESOURCE_SENTINEL",
        )
        .unwrap();

        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let commands = start_tui_commands(&mut tui, &bootstrap).unwrap();
        let skills = start_tui_skills(&mut tui, &bootstrap).unwrap();
        report_tui_extension_collisions(&mut tui, &commands, &skills);
        assert!(tui.view().dialog.is_some());
        assert!(tui.transcript().is_empty());
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            session,
            cancellation,
            commands,
            Arc::clone(&skills),
        );
        let captured = Arc::new(Mutex::new(Vec::new()));

        submit_tui_command(&mut tui, &router, &bootstrap, "normal prompt", &captured);

        let request = captured.lock().unwrap()[0].clone();
        let context = request.system_prompt.unwrap();
        assert_eq!(context.matches("## Available skills").count(), 1);
        assert!(context.contains("- alpha: global alpha"));
        assert!(context.contains("- invoke: project invoke"));
        assert!(context.contains("- shared: project shared"));
        for secret in [
            "GLOBAL_ALPHA_BODY_SENTINEL",
            "GLOBAL_SHARED_BODY_SENTINEL",
            "GLOBAL_INVOKE_BODY_SENTINEL",
            "PROJECT_SHARED_BODY_SENTINEL",
            "PROJECT_INVOKE_BODY_SENTINEL",
            "BROKEN_BODY_SENTINEL",
            "RESOURCE_SENTINEL",
        ] {
            assert!(!context.contains(secret));
        }

        let (tools, dispatcher) = production_tool_runtime(
            &bootstrap,
            bootstrap.project_root().unwrap(),
            Some(skills.as_ref()),
        )
        .unwrap();
        assert!(tools.iter().any(|tool| tool.name() == "skill"));
        assert!(tools.iter().any(|tool| tool.name() == "task"));
        assert!(
            dispatcher
                .lock()
                .unwrap()
                .canonical_identity("skill")
                .is_some()
        );
        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::skill".into()),
                PermissionPattern::Any,
            )],
        );
        let mut dispatcher = dispatcher.lock().unwrap();
        let ToolEvaluationOutcome::Authorized(call) = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new("project", "skill", serde_json::json!({"skill":"shared"})),
            )
            .unwrap()
        else {
            panic!("skill tool should pass normal authorization");
        };
        assert_eq!(
            dispatcher
                .execute(
                    call,
                    &ToolExecutionContext::with_timeout(std::time::Duration::from_secs(1)),
                )
                .unwrap()
                .content,
            "PROJECT_SHARED_BODY_SENTINEL"
        );
        drop(dispatcher);

        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/invoke   explicit arguments   ",
            &captured,
        );
        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/shared command arguments",
            &captured,
        );
        std::fs::remove_file(project_skills.join("broken/SKILL.md")).unwrap();
        submit_tui_command(&mut tui, &router, &bootstrap, "/broken args", &captured);

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[1].prompt,
            "## Skill: invoke\nPROJECT_INVOKE_BODY_SENTINEL\n\n## User arguments\nexplicit arguments"
        );
        assert_eq!(requests[2].prompt, "COMMAND:command arguments");
        assert!(tui.transcript().contains(&agens_tui::TranscriptEntry::User(
            "/invoke   explicit arguments   ".into()
        )));
        assert!(tui.view().dialog.is_some());
        drop(requests);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_palette_uses_the_resolved_surface_and_renders_inside_a_narrow_resize() {
        let temporary = tui_session_directory("resolved-palette");
        let config_home = temporary.join("config");
        let global_commands = config_home.join("commands");
        let project_commands = temporary.join("project/.agens/commands");
        let global_skills = config_home.join("skills");
        let project_skills = temporary.join("project/.agens/skills");
        std::fs::create_dir_all(&global_commands).unwrap();
        std::fs::create_dir_all(&project_commands).unwrap();
        write_tui_command(&global_commands, "shared", "global command", "global");
        write_tui_command(&project_commands, "shared", "project command", "project");
        write_tui_command(
            &project_commands,
            "review",
            "review changes",
            "review:$ARGUMENTS",
        );
        write_tui_command(&project_commands, "connect", "reserved collision", "wrong");
        write_tui_skill(&global_skills, "shared", "shadowed skill", "wrong");
        write_tui_skill(&project_skills, "inspect", "inspect code", "INSPECT");
        std::fs::write(
            project_commands.join("broken.md"),
            "---\ndescription: [invalid\n---\nbroken\n",
        )
        .unwrap();

        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let commands = start_tui_commands(&mut tui, &bootstrap).unwrap();
        let skills = start_tui_skills(&mut tui, &bootstrap).unwrap();
        report_tui_extension_collisions(&mut tui, &commands, &skills);
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            commands,
            skills,
        );
        let entries = router.palette_entries();

        assert_eq!(
            entries.iter().map(|entry| entry.name()).collect::<Vec<_>>(),
            vec![
                "connect",
                "disconnect",
                "diagnostics",
                "new",
                "sessions",
                "resume",
                "agent",
                "provider",
                "model",
                "effort",
                "help",
                "mcp",
                "select",
                "quit",
                "subagent",
                "subagents",
                "review",
                "shared",
                "inspect",
            ]
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.name() == "shared")
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.name() == "shared")
                .unwrap()
                .kind(),
            agens_tui::PaletteEntryKind::Command
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.name() == "shared")
                .unwrap()
                .description(),
            "project command"
        );
        assert!(entries.iter().any(|entry| entry.name() == "subagent"));
        assert!(tui.transcript().is_empty());
        assert!(tui.view().dialog.is_some());

        tui.set_palette_entries(entries.to_vec());
        for character in "/sha".chars() {
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
        }
        tui.handle(agens_tui::Event::Resize {
            width: 20,
            height: 6,
        });
        let terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6)).unwrap();
        let mut renderer = agens_tui::RatatuiRenderer::new(terminal);
        agens_tui::Renderer::render(&mut renderer, tui.view()).unwrap();
        let text = renderer
            .terminal()
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("commands"), "{text:?}");
        assert!(text.contains("/shared"), "{text:?}");
        assert!(!text.contains("inspect"), "{text:?}");

        let original = session.lock().unwrap().clone();
        assert_eq!(
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Escape)),
            agens_tui::Action::Render
        );
        assert_eq!(tui.input(), "/sha");
        assert_eq!(*session.lock().unwrap(), original);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_palette_enter_routes_built_in_command_skill_help_quit_and_unknown_once() {
        let temporary = tui_session_directory("palette-routing");
        let config_home = temporary.join("config");
        let project_commands = temporary.join("project/.agens/commands");
        let project_skills = temporary.join("project/.agens/skills");
        std::fs::create_dir_all(config_home.join("commands")).unwrap();
        std::fs::create_dir_all(&project_commands).unwrap();
        write_tui_command(
            &project_commands,
            "review",
            "review changes",
            "REVIEW:$ARGUMENTS",
        );
        write_tui_skill(&project_skills, "inspect", "inspect code", "INSPECT_BODY");

        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let commands = start_tui_commands(&mut tui, &bootstrap).unwrap();
        let skills = start_tui_skills(&mut tui, &bootstrap).unwrap();
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            commands,
            skills,
        );
        tui.set_palette_entries(router.palette_entries().to_vec());
        let mut provider_prompts = Vec::new();

        for (input, expected) in [
            ("/review target", "REVIEW:target"),
            (
                "/inspect src",
                "## Skill: inspect\nINSPECT_BODY\n\n## User arguments\nsrc",
            ),
        ] {
            let input = enter_tui_input(&mut tui, input);
            let prompt = tui.apply_submission_outcome(router.route(input)).unwrap();
            provider_prompts.push(prompt.clone());
            tui.finish_provider_turn(TuiProviderOutcome::Completed("captured".into()));
            assert_eq!(prompt, expected);
        }

        let sessions = router.open_dialog("sessions").unwrap();
        assert!(matches!(sessions, TuiSubmissionOutcome::Dialog(_)));
        assert!(matches!(
            router.route("/help".into()),
            TuiSubmissionOutcome::Dialog(_)
        ));
        assert!(matches!(
            router.route("/mouse".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ));

        let unknown = enter_tui_input(&mut tui, "/unknown");
        assert!(
            tui.apply_submission_outcome(router.route(unknown))
                .is_none()
        );
        assert_eq!(provider_prompts.len(), 2);
        assert!(session.lock().unwrap().messages.is_empty());

        let quit = enter_tui_input(&mut tui, "/quit");
        assert_eq!(router.route(quit), TuiSubmissionOutcome::Quit);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn dialog_recovery_is_confirmed_private_local_safe_and_retryable() {
        let temporary = tui_session_directory("recovery-dialog");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            Arc::clone(&cancellation),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let metadata = SessionMetadata {
            id: 1,
            project: tui_project(&temporary),
            title: "Interrupted session".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 7,
            completed_turn_count: 0,
            resumable: false,
        };
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let attempt = store
            .begin_session_attempt(&metadata, "SENTINEL_PRIVATE_RETRY".into())
            .unwrap();
        drop(store);

        let confirmation = router.route_dialog_action("session:1", std::sync::mpsc::channel().0);
        let confirmation_debug = format!("{confirmation:?}");
        let mut tui = Tui::new(ProductionTuiEngine { cancellation });
        assert!(tui.apply_submission_outcome(confirmation).is_none());
        let confirmation_text = render_tui_test_backend(&tui, 100, 24);
        assert!(confirmation_text.contains("Recover interrupted attempt"));
        assert!(confirmation_text.contains("Interrupted session"));
        assert!(confirmation_text.contains("ID: 1"));
        assert!(confirmation_text.contains("Status: running"));
        assert!(confirmation_text.contains("Started: 7"));
        assert!(
            confirmation_debug
                .contains("This may invalidate an attempt still running in another process.")
        );
        assert!(!confirmation_debug.contains("SENTINEL_PRIVATE_RETRY"));

        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        let store = SessionStore::open(bootstrap.data_directory()).unwrap();
        assert_eq!(
            store
                .load_session_for_resume(1)
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Running
        );
        drop(store);

        let locally_active_metadata = SessionMetadata {
            id: 2,
            ..metadata.clone()
        };
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let locally_active = active_session_attempts()
            .begin_and_register(
                &mut store,
                &locally_active_metadata,
                "local private retry".into(),
            )
            .unwrap();
        drop(store);
        let local_refusal = router.route_dialog_action(
            &format!(
                "session:recover:{}:{}",
                locally_active.key().session_id(),
                locally_active.key().attempt_id()
            ),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(local_refusal, TuiSubmissionOutcome::Dialog(_)));
        let store = SessionStore::open(bootstrap.data_directory()).unwrap();
        assert_eq!(
            store
                .load_session_for_resume(locally_active.key().session_id())
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Running
        );
        drop(store);
        active_session_attempts().unregister(locally_active.key());

        let recovered = router.route_dialog_action(
            &format!(
                "session:recover:{}:{}",
                attempt.key().session_id(),
                attempt.key().attempt_id()
            ),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(
            recovered,
            TuiSubmissionOutcome::ProviderTurn { ref display, ref prompt }
                if display == "Retrying recovered attempt." && prompt == "SENTINEL_PRIVATE_RETRY"
        ));
        assert_eq!(session.lock().unwrap().identifier, Some(1));
        let store = SessionStore::open(bootstrap.data_directory()).unwrap();
        assert_eq!(
            store
                .load_session_for_resume(1)
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Interrupted
        );

        let stale = router.route_dialog_action(
            &format!(
                "session:recover:{}:{}",
                attempt.key().session_id(),
                attempt.key().attempt_id()
            ),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(stale, TuiSubmissionOutcome::Dialog(_)));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_model_effort_and_help_palette_routes_open_local_overlays_and_dispatch_once() {
        let temporary = tui_session_directory("local-overlays");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        tui.set_palette_entries(router.palette_entries().to_vec());
        let (progress, _) = std::sync::mpsc::channel();

        for (prefix, route_id, expected) in [
            ("/mo", "model", ["Choose model", "gpt-4.1 (current)"]),
            ("/ef", "effort", ["Choose effort", "Default"]),
            ("/he", "help", ["Commands and skills", "/connect"]),
        ] {
            for character in prefix.chars() {
                tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
            }
            let agens_tui::Action::OpenDialog(actual_route) =
                tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter))
            else {
                panic!("palette Enter should open the selected overlay");
            };
            assert_eq!(actual_route, route_id);
            let outcome = router.route_request(
                agens_tui::TuiRouteRequest::OpenDialog(actual_route),
                progress.clone(),
            );
            assert!(tui.apply_submission_outcome(outcome).is_none());
            let text = render_tui_test_backend(&tui, 80, 24);
            assert!(text.contains(expected[0]), "{route_id}: {text:?}");
            assert!(text.contains(expected[1]), "{route_id}: {text:?}");

            if route_id == "help" {
                assert_eq!(
                    tui.handle(agens_tui::Event::Key(agens_tui::Key::CtrlC)),
                    agens_tui::Action::Render
                );
                continue;
            }
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Down));
            let agens_tui::Action::DialogAction(action_id) =
                tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter))
            else {
                panic!("dialog Enter should emit one action ID");
            };
            let outcome = router.route_request(
                agens_tui::TuiRouteRequest::DialogAction(action_id),
                progress.clone(),
            );
            assert!(tui.apply_submission_outcome(outcome).is_none());
            assert!(tui.view().dialog.is_none());
        }

        assert!(session.lock().unwrap().messages.is_empty());
        assert!(
            tui.transcript()
                .iter()
                .all(|entry| !matches!(entry, agens_tui::TranscriptEntry::User(_)))
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_mcp_overlay_is_local_safe_refreshable_and_includes_disabled_servers() {
        let temporary = tui_session_directory("mcp-overlay");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.mcp_servers = vec![
            agens_config::McpServerConfig {
                name: "files".into(),
                disabled: false,
                transport: McpTransport::Stdio,
                command: Some("/private/bin/files-server".into()),
                args: vec!["SENTINEL_ARG_SECRET".into()],
                environment: BTreeMap::from([("TOKEN".into(), "SENTINEL_ENV_SECRET".into())]),
                cwd: None,
                url: None,
                headers: BTreeMap::new(),
                max_retries: 0,
                timeout_ms: 250,
            },
            agens_config::McpServerConfig {
                name: "disabled".into(),
                disabled: true,
                transport: McpTransport::Sse,
                command: None,
                args: Vec::new(),
                environment: BTreeMap::new(),
                cwd: None,
                url: Some("https://user:SENTINEL_URL_SECRET@example.test/mcp?token=secret".into()),
                headers: BTreeMap::from([(
                    "Authorization".into(),
                    "SENTINEL_HEADER_SECRET".into(),
                )]),
                max_retries: 0,
                timeout_ms: 500,
            },
        ];
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        assert!(
            tui.apply_submission_outcome(router.route("/mcp".into()))
                .is_none()
        );
        for character in "idle".chars() {
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
        }
        let filtered = render_tui_test_backend(&tui, 90, 24);
        assert!(filtered.contains("files") && !filtered.contains("disabled  sse"));
        tui.handle(agens_tui::Event::Key(agens_tui::Key::Escape));
        tui.apply_submission_outcome(router.open_dialog("mcp").unwrap());
        tui.handle(agens_tui::Event::Key(agens_tui::Key::Down));
        tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter));
        let text = render_tui_test_backend(&tui, 90, 24);
        assert!(text.contains("stdio"), "{text:?}");
        assert!(text.contains("enabled/idle"), "{text:?}");
        assert!(text.contains("disabled"), "{text:?}");
        assert!(text.contains("Source: global"), "{text:?}");
        assert!(text.contains("files-server"), "{text:?}");
        assert!(text.contains("250ms"), "{text:?}");
        for secret in [
            "SENTINEL_ARG_SECRET",
            "SENTINEL_ENV_SECRET",
            "SENTINEL_URL_SECRET",
            "SENTINEL_HEADER_SECRET",
        ] {
            assert!(!text.contains(secret), "{secret}: {text:?}");
        }

        let mut live = McpRegistry::with_status_handle(router.mcp_status.clone());
        live.register_disabled_server(McpServerDescriptor::new(
            "later",
            McpServerSource::Global,
            McpServerTransport::Stdio,
            false,
            std::time::Duration::from_secs(10),
            None,
        ))
        .unwrap();
        let agens_tui::Action::OpenDialog(route_id) =
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char('r')))
        else {
            panic!("MCP refresh should remain local");
        };
        let refreshed = router.open_dialog(&route_id).unwrap();
        tui.apply_submission_outcome(refreshed);
        assert!(render_tui_test_backend(&tui, 90, 24).contains("later"));
        assert!(session.lock().unwrap().messages.is_empty());
        assert!(tui.transcript().is_empty());
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_model_overlay_labels_source_metadata_current_and_compatible_sets() {
        for (provider, source, included, excluded) in [
            ("openai-api", "OpenAI API", "gpt-4o", "gpt-5.4"),
            (
                "openai-chatgpt",
                "ChatGPT subscription",
                "gpt-5.4",
                "gpt-4o",
            ),
        ] {
            let temporary = tui_session_directory(&format!("model-source-{provider}"));
            let bootstrap =
                tui_session_bootstrap_for_provider(&temporary, &[], provider, "gpt-5.5");
            let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
            let cancellation = Arc::new(Mutex::new(None));
            let mut tui = Tui::new(ProductionTuiEngine {
                cancellation: Arc::clone(&cancellation),
            });
            let router = TuiRuntimeRouter::new(
                bootstrap,
                Arc::clone(&session),
                cancellation,
                Arc::new(CommandCatalog::default()),
                Arc::new(SkillCatalog::default()),
            );
            let (progress, _) = std::sync::mpsc::channel();

            assert!(
                tui.apply_submission_outcome(
                    router.route_request(TuiRouteRequest::OpenDialog("model".into()), progress)
                )
                .is_none()
            );
            let text = render_tui_test_backend(&tui, 140, 60);

            assert!(text.contains(source), "{provider}: {text:?}");
            assert!(text.contains("gpt-5.5 (current)"), "{provider}: {text:?}");
            assert!(text.contains(included), "{provider}: {text:?}");
            assert!(!text.contains(excluded), "{provider}: {text:?}");
            assert!(text.contains("272K context"), "{provider}: {text:?}");
            assert!(text.contains("128K output"), "{provider}: {text:?}");
            assert!(text.contains("reasoning"), "{provider}: {text:?}");

            let source = if provider == "openai-chatgpt" {
                TuiModelSource::ChatGptSubscription
            } else {
                TuiModelSource::OpenAiApi
            };
            let models = TuiModelSelector::for_source("gpt-5.5", source)
                .models()
                .unwrap();
            let family = models
                .iter()
                .filter(|model| model.id.starts_with("gpt-5.6"))
                .map(|model| {
                    (
                        model.id.as_str(),
                        model.name.as_deref(),
                        model.context,
                        model.output,
                        model.reasoning,
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                family,
                [
                    (
                        "gpt-5.6",
                        Some("GPT-5.6 (Sol alias)"),
                        Some(1_050_000),
                        Some(128_000),
                        Some(true)
                    ),
                    (
                        "gpt-5.6-luna",
                        Some("GPT-5.6 Luna"),
                        Some(1_050_000),
                        Some(128_000),
                        Some(true)
                    ),
                    (
                        "gpt-5.6-sol",
                        Some("GPT-5.6 Sol"),
                        Some(1_050_000),
                        Some(128_000),
                        Some(true)
                    ),
                    (
                        "gpt-5.6-terra",
                        Some("GPT-5.6 Terra"),
                        Some(1_050_000),
                        Some(128_000),
                        Some(true)
                    ),
                ],
                "official OpenAI GPT-5.6 catalog metadata for {provider}"
            );
            for model in &family {
                assert_eq!(
                    models
                        .iter()
                        .filter(|candidate| candidate.id == model.0)
                        .count(),
                    1,
                    "duplicate {} in {provider}",
                    model.0
                );
            }
            assert!(text.contains("gpt-5.6"), "{provider}: {text:?}");
            assert!(text.contains("gpt-5.6-luna"), "{provider}: {text:?}");
            assert!(
                !text.contains("unverified metadata"),
                "{provider}: {text:?}"
            );

            for _ in 0..4 {
                tui.handle(Event::Key(Key::Down));
            }
            let scrolled = render_tui_test_backend(&tui, 80, 24);
            assert!(scrolled.contains("gpt-5.6-sol"), "{provider}: {scrolled:?}");
            assert!(
                scrolled.contains("gpt-5.6-terra"),
                "{provider}: {scrolled:?}"
            );
            tui.handle(Event::Key(Key::Up));
            tui.handle(Event::Key(Key::Up));
            tui.handle(Event::Key(Key::Up));
            let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
                panic!("verified gpt-5.6 alias should be selectable");
            };
            let outcome = router.route_request(
                TuiRouteRequest::DialogAction(action_id),
                std::sync::mpsc::channel().0,
            );
            assert!(matches!(
                &outcome,
                TuiSubmissionOutcome::ContextChanged { message, presentation }
                    if message == "Model: gpt-5.6."
                        && presentation
                            == &TuiPresentation::new(provider, "gpt-5.6", "new session")
                                .with_effort("medium")
                                .with_context_window(Some(1_050_000))
            ));
            tui.apply_submission_outcome(outcome);
            let selection = session.lock().unwrap().selection.clone().unwrap();
            assert!(selection.metadata_known());
            assert_eq!(selection.reasoning_effort_default(), Some("medium"));
            assert_eq!(
                selection.reasoning_effort_values(),
                ["default", "none", "low", "medium", "high", "xhigh", "max"]
            );

            tui.apply_submission_outcome(router.open_dialog("model").unwrap());
            for character in "gpt-5.6-sol".chars() {
                tui.handle(Event::Key(Key::Char(character)));
            }
            let filtered = render_tui_test_backend(&tui, 80, 24);
            assert!(filtered.contains("gpt-5.6-sol"), "{provider}: {filtered:?}");
            assert!(
                !filtered.contains("unverified metadata"),
                "{provider}: {filtered:?}"
            );

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    #[test]
    fn tui_provider_availability_uses_complete_current_credentials_without_exposing_them() {
        let temporary = tui_session_directory("provider-status");
        let credentials = temporary.join("auth.json");
        std::fs::write(
            &credentials,
            r#"{"openai-chatgpt":{"access_token":"access","refresh_token":"refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let resolver = TuiCredentialResolver::with_environment(BTreeMap::new());

        let statuses =
            TuiProvider::ALL.map(|provider| resolver.status(&credentials, provider).label());
        assert_eq!(statuses, ["ready", "credential required"]);
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_provider_overlay_filters_unavailable_entries_and_switches_without_history() {
        let temporary = tui_session_directory("provider-overlay");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        std::fs::write(
            &bootstrap.paths.credentials,
            r#"{"openai-chatgpt":{"access_token":"secret-access","refresh_token":"secret-refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::with_credential_resolver(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            TuiCredentialResolver::with_environment(BTreeMap::new()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        let (progress, _) = std::sync::mpsc::channel();
        tui.apply_submission_outcome(router.route_request(
            TuiRouteRequest::OpenDialog("provider".into()),
            progress.clone(),
        ));
        let overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(
            overlay.contains("Current: OpenAI API · credential required"),
            "{overlay:?}"
        );
        assert!(overlay.contains("❯ ChatGPT subscription"), "{overlay:?}");
        assert!(overlay.contains("ready"), "{overlay:?}");
        assert!(!overlay.contains("OpenAI API (current)"), "{overlay:?}");
        assert!(!overlay.contains("secret-"), "{overlay:?}");

        dispatch_tui_dialog_selection(&mut tui, &router, progress);
        assert_eq!(tui.view().provider_model, "openai-chatgpt / gpt-5.5");
        tui.apply_submission_outcome(router.open_dialog("model").unwrap());
        let model_overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(model_overlay.contains("Source: ChatGPT subscription"));
        assert!(model_overlay.contains("gpt-5.5 (current)"));
        assert!(tui.transcript().is_empty());
        assert!(session.lock().unwrap().messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_provider_switch_reconciles_compatible_incompatible_and_busy_state_atomically() {
        let temporary = tui_session_directory("provider-reconcile");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        std::fs::write(
            &bootstrap.paths.credentials,
            r#"{"openai-chatgpt":{"access_token":"access","refresh_token":"refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::with_credential_resolver(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            TuiCredentialResolver::with_environment(BTreeMap::from([(
                "OPENAI_API_KEY".into(),
                "api-secret".into(),
            )])),
        );

        let retained = router.route("/provider openai-chatgpt".into());
        assert!(
            matches!(retained, TuiSubmissionOutcome::ContextChanged { ref message, .. } if message.contains("Model retained: gpt-5.5"))
        );
        router.route("/model gpt-5.4".into());
        router.route("/effort high".into());
        let reset = router.route("/provider openai-api".into());
        assert!(
            matches!(reset, TuiSubmissionOutcome::ContextChanged { ref message, .. } if message.contains("Model reset to gpt-4.1") && message.contains("Default"))
        );
        let idle = session.lock().unwrap().clone();
        assert_eq!(idle.selection.as_ref().unwrap().model(), "gpt-4.1");
        assert_eq!(idle.selection.as_ref().unwrap().reasoning_effort(), None);
        let mut context = session.lock().unwrap();
        context.messages = tui_session_messages();
        context.running = true;
        drop(context);
        let busy = session.lock().unwrap().clone();
        assert!(matches!(
            router.route("/provider openai-chatgpt".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ));
        assert_eq!(*session.lock().unwrap(), busy);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_turn_bootstrap_resolves_changed_and_removed_credentials_without_stale_reuse() {
        let temporary = tui_session_directory("fresh-turn-credentials");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let configured_provider = bootstrap.provider_type.clone();
        let credentials = bootstrap.paths.credentials.clone();
        let environment = Arc::new(Mutex::new(BTreeMap::new()));
        let resolver = TuiCredentialResolver::with_environment_resolver({
            let environment = Arc::clone(&environment);
            move || environment.lock().unwrap().clone()
        });
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::with_credential_resolver(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            resolver,
        );

        std::fs::write(&credentials, r#"{"openai-api":{"api_key":"file-one"}}"#).unwrap();
        assert_eq!(
            router.turn_bootstrap().unwrap().openai_api_key.as_deref(),
            Some("file-one")
        );
        std::fs::write(&credentials, r#"{"openai-api":{"api_key":"file-two"}}"#).unwrap();
        assert_eq!(
            router.turn_bootstrap().unwrap().openai_api_key.as_deref(),
            Some("file-two")
        );
        environment
            .lock()
            .unwrap()
            .insert("OPENAI_API_KEY".into(), "env-current".into());
        assert_eq!(
            router.turn_bootstrap().unwrap().openai_api_key.as_deref(),
            Some("env-current")
        );
        environment.lock().unwrap().clear();
        std::fs::remove_file(&credentials).unwrap();
        assert!(router.turn_bootstrap().is_err());

        session.lock().unwrap().provider = Some(TuiProvider::OpenAiChatGpt);
        std::fs::write(
            &credentials,
            r#"{"openai-chatgpt":{"access_token":"chat-access","refresh_token":"chat-refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(
            router.turn_bootstrap().unwrap().provider_type(),
            Some("openai-chatgpt")
        );
        std::fs::remove_file(&credentials).unwrap();
        assert!(router.turn_bootstrap().is_err());
        assert!(matches!(
            router.route("/provider openai-chatgpt".into()),
            TuiSubmissionOutcome::LocalActionableError { ref message, .. }
                if message.contains("run /connect")
        ));
        assert_eq!(
            router.bootstrap().unwrap().provider_type,
            configured_provider
        );
        assert!(session.lock().unwrap().messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn persisted_selection_updates_and_resume_are_atomic_and_credential_fresh() {
        let temporary = tui_session_directory("persisted-selection");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let mut metadata = persist_tui_session(&mut store, &tui_project(&temporary), "selection");
        metadata.provider_id = Some("openai-api".into());
        metadata.model_id = Some("gpt-5.5".into());
        metadata.reasoning_effort = Some(agens_core::ReasoningEffort::High);
        store.update_session_selection(&metadata).unwrap();
        drop(store);
        let resolver = TuiCredentialResolver::with_environment(BTreeMap::from([(
            "OPENAI_API_KEY".into(),
            "fresh-secret".into(),
        )]));
        let resumed =
            resume_tui_session(&bootstrap, metadata.id, &SkillCatalog::default(), &resolver)
                .unwrap();
        assert_eq!(resumed.selection.as_ref().unwrap().model(), "gpt-5.5");
        assert_eq!(
            resumed.selection.as_ref().unwrap().reasoning_effort(),
            Some("high")
        );
        let session = Arc::new(Mutex::new(resumed));
        let router = TuiRuntimeRouter::with_credential_resolver(
            bootstrap.clone(),
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            resolver,
        );
        assert_eq!(router.turn_bootstrap().unwrap().model(), Some("gpt-5.5"));
        assert_eq!(
            router
                .task_parent_request_config()
                .unwrap()
                .reasoning_effort(),
            Some(agens_core::ReasoningEffort::High)
        );
        assert!(matches!(
            router.route("/model gpt-4.1".into()),
            TuiSubmissionOutcome::ContextChanged { .. }
        ));
        assert_eq!(router.turn_bootstrap().unwrap().model(), Some("gpt-4.1"));
        assert_eq!(
            router
                .task_parent_request_config()
                .unwrap()
                .reasoning_effort(),
            None
        );
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .model_id
                .as_deref(),
            Some("gpt-4.1")
        );

        let database = SessionStore::open(bootstrap.data_directory())
            .unwrap()
            .database_path();
        rusqlite::Connection::open(database)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_selection BEFORE UPDATE OF provider_id ON sessions
             BEGIN SELECT RAISE(ABORT, 'reject selection'); END;",
            )
            .unwrap();
        let before = session.lock().unwrap().clone();
        assert!(matches!(
            router.route("/effort default".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ));
        assert_eq!(*session.lock().unwrap(), before);

        let unavailable = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &TuiCredentialResolver::with_environment(BTreeMap::new()),
        )
        .unwrap();
        assert_eq!(unavailable.messages, before.messages);
        assert_eq!(
            unavailable.resume_error.as_deref(),
            Some("connect or choose provider")
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_model_overlay_selects_exact_future_id_with_unknown_metadata_and_default_effort() {
        let temporary = tui_session_directory("unverified-model");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let (progress, _) = std::sync::mpsc::channel();
        assert!(matches!(
            router.route("/effort xhigh".into()),
            TuiSubmissionOutcome::ContextChanged { .. }
        ));
        tui.apply_submission_outcome(router.route_request(
            TuiRouteRequest::OpenDialog("model".into()),
            progress.clone(),
        ));

        for character in "gpt-future-1".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        let overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(
            overlay.contains("Use gpt-future-1 (unverified metadata)"),
            "{overlay:?}"
        );
        let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("unverified model should dispatch a local action");
        };
        let outcome = router.route_request(TuiRouteRequest::DialogAction(action_id), progress);
        let TuiSubmissionOutcome::ContextChanged {
            message,
            presentation,
        } = &outcome
        else {
            panic!("unverified model should update session context");
        };
        assert_eq!(
            message,
            "Model: gpt-future-1 (unverified metadata). Reasoning effort reset to Default."
        );
        assert_eq!(
            presentation,
            &TuiPresentation::new("openai-api", "gpt-future-1", "new session")
        );
        tui.apply_submission_outcome(outcome);

        let selection = session.lock().unwrap().selection.clone().unwrap();
        assert_eq!(selection.model(), "gpt-future-1");
        assert!(!selection.metadata_known());
        assert_eq!(selection.reasoning_effort(), None);
        assert_eq!(
            selection.request_config(),
            &agens_core::RequestConfig::default()
        );
        assert!(session.lock().unwrap().messages.is_empty());
        assert!(tui.transcript().is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_effort_overlay_and_model_change_use_grounded_sets_and_atomic_reset() {
        let temporary = tui_session_directory("effort-capabilities");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let (progress, _) = std::sync::mpsc::channel();

        assert_eq!(
            router.route("/effort xhigh".into()),
            TuiSubmissionOutcome::ContextChanged {
                message: "Reasoning effort: xhigh.".into(),
                presentation: router.presentation().unwrap(),
            }
        );
        assert!(
            tui.apply_submission_outcome(
                router.route_request(TuiRouteRequest::OpenDialog("effort".into()), progress)
            )
            .is_none()
        );
        let overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(overlay.contains("Default"), "{overlay:?}");
        assert!(overlay.contains("xhigh (current)"), "{overlay:?}");
        assert!(!overlay.contains("minimal"), "{overlay:?}");

        let reset = router.route("/model gpt-4.1".into());
        let TuiSubmissionOutcome::ContextChanged { message, .. } = reset else {
            panic!("model change should be local context information");
        };
        assert_eq!(
            message,
            "Model: gpt-4.1. Reasoning effort reset to Default because xhigh is unsupported."
        );
        let selection = session.lock().unwrap().selection.clone().unwrap();
        assert_eq!(selection.model(), "gpt-4.1");
        assert_eq!(selection.reasoning_effort(), None);
        assert_eq!(selection.request_config().reasoning_effort(), None);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_sessions_resume_and_agent_overlays_filter_navigate_cancel_and_apply_typed_outcomes() {
        let temporary = tui_session_directory("session-agent-overlays");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[
                (
                    "all",
                    "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
                ),
                (
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                ),
            ],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        tui.set_palette_entries(router.palette_entries().to_vec());
        let (progress, _) = std::sync::mpsc::channel();

        let empty = router.route_request(
            agens_tui::TuiRouteRequest::OpenDialog("sessions".into()),
            progress.clone(),
        );
        tui.apply_submission_outcome(empty);
        assert!(
            render_tui_test_backend(&tui, 80, 24)
                .contains("No resumable sessions in current project.")
        );
        tui.handle(Event::Key(Key::Escape));

        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let current = persist_tui_session(&mut store, &tui_project(&temporary), "current");
        let other = persist_tui_session(
            &mut store,
            &temporary.join("other").display().to_string(),
            "other",
        );
        drop(store);

        open_tui_palette_dialog(&mut tui, &router, "/se", "sessions", progress.clone());
        let sessions = render_tui_test_backend(&tui, 80, 24);
        assert!(sessions.contains(&format!("#{} current", current.id)));
        assert!(!sessions.contains(&format!("#{} other", other.id)));
        let original = session.lock().unwrap().clone();
        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        assert_eq!(*session.lock().unwrap(), original);

        open_tui_palette_dialog(&mut tui, &router, "/re", "sessions", progress.clone());
        dispatch_tui_dialog_selection(&mut tui, &router, progress.clone());
        assert_eq!(tui.view().session, format!("session #{}", current.id));
        assert!(tui.transcript().is_empty());
        assert!(
            tui.view()
                .status
                .is_some_and(|status| status.contains("Resumed session"))
        );

        open_tui_palette_dialog(&mut tui, &router, "/ag", "agent", progress.clone());
        let agents = render_tui_test_backend(&tui, 80, 24);
        assert!(agents.contains("primary (current)"), "{agents:?}");
        tui.handle(Event::Key(Key::Down));
        dispatch_tui_dialog_selection(&mut tui, &router, progress);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn session_overlay_uses_real_metadata_scope_search_sort_clock_and_atomic_failure() {
        let temporary = tui_session_directory("session-metadata-overlay");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = tui_project(&temporary);
        let other_project = temporary.join("other-root").display().to_string();
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let old = persist_tui_session_metadata(&mut store, &project, "Alpha", "primary", 9_900);
        let other =
            persist_tui_session_metadata(&mut store, &other_project, "Beta", "build", 9_950);
        let mut current =
            persist_tui_session_metadata(&mut store, &project, "Gamma", "reviewer", 9_950);
        current.provider_id = Some("openai-chatgpt".into());
        current.model_id = Some("gpt-5.5".into());
        current.reasoning_effort = Some(agens_core::ReasoningEffort::High);
        store.update_session_selection(&current).unwrap();
        drop(store);

        let session = Arc::new(Mutex::new(TuiSessionContext {
            identifier: Some(current.id),
            ..TuiSessionContext::fresh()
        }));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        tui.set_presentation("openai-api", "gpt-4.1", format!("session #{}", current.id));
        tui.replace_history(&tui_session_messages()).unwrap();
        let router = TuiRuntimeRouter::with_clock(
            bootstrap.clone(),
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            || 10_000,
        );
        tui.set_palette_entries(router.palette_entries().to_vec());
        let (progress, _) = std::sync::mpsc::channel();
        let original_context = session.lock().unwrap().clone();

        open_tui_palette_dialog(&mut tui, &router, "/se", "sessions", progress.clone());
        let project_rows = render_tui_test_backend(&tui, 100, 26);
        assert!(project_rows.contains("Resume session · Current project"));
        assert!(project_rows.contains(&format!("#{} Gamma", current.id)));
        assert!(project_rows.contains(&format!("#{} Alpha", old.id)));
        assert!(
            project_rows.contains("1 turn · 50s ago"),
            "{project_rows:?}"
        );
        assert!(!project_rows.contains("reviewer"), "{project_rows:?}");
        assert!(!project_rows.contains("Provider:"), "{project_rows:?}");
        assert!(!project_rows.contains("Model:"), "{project_rows:?}");
        assert!(!project_rows.contains("Effort:"), "{project_rows:?}");
        assert!(!project_rows.contains("Updated:"), "{project_rows:?}");
        tui.handle(Event::Key(Key::CtrlO));
        let project_details = render_tui_test_backend(&tui, 100, 26);
        assert!(
            project_details.contains("Provider: openai-chatgpt · Model: gpt-5.5"),
            "{project_details:?}"
        );
        assert!(
            project_details.contains("Effort: high · Updated: 9950 (50s ago)"),
            "{project_details:?}"
        );
        let old_details = format!(
            "{:?}",
            session_dialog_entry(
                &StoredSession {
                    metadata: old.clone(),
                    messages: Vec::new(),
                    latest_attempt: None,
                },
                None,
                false,
                10_000,
            )
        );
        assert!(old_details.contains("Provider: current runtime"));
        assert!(old_details.contains("Model: current runtime"));
        assert!(old_details.contains("Effort: current runtime"));
        assert!(project_rows.find("Gamma").unwrap() < project_rows.find("Alpha").unwrap());
        assert!(!project_rows.contains("Beta"));

        let global_action = tui.handle(Event::Key(Key::LineStart));
        dispatch_tui_session_page(&mut tui, &router, global_action, progress.clone());
        let global_rows = render_tui_test_backend(&tui, 100, 24);
        assert!(global_rows.contains("Resume session · All projects"));
        assert!(global_rows.contains(&format!("#{} Beta", other.id)));
        assert!(!global_rows.contains("root="), "{global_rows:?}");
        assert!(!global_rows.contains("other-root"), "{global_rows:?}");
        assert!(global_rows.find("Gamma").unwrap() < global_rows.find("Beta").unwrap());
        assert!(global_rows.find("Beta").unwrap() < global_rows.find("Alpha").unwrap());

        let mut search_action = Action::Render;
        for character in "reviewer".chars() {
            search_action = tui.handle(Event::Key(Key::Char(character)));
        }
        dispatch_tui_session_page(&mut tui, &router, search_action, progress.clone());
        let agent_search = render_tui_test_backend(&tui, 100, 24);
        assert!(agent_search.contains("Gamma"));
        assert!(!agent_search.contains("Alpha"));
        assert!(!agent_search.contains("Beta"));
        tui.handle(Event::Key(Key::Escape));
        tui.apply_submission_outcome(router.open_dialog("sessions").unwrap());
        let global_action = tui.handle(Event::Key(Key::LineStart));
        dispatch_tui_session_page(&mut tui, &router, global_action, progress.clone());
        let mut search_action = Action::Render;
        for character in "other-root".chars() {
            search_action = tui.handle(Event::Key(Key::Char(character)));
        }
        dispatch_tui_session_page(&mut tui, &router, search_action, progress.clone());
        let root_search = render_tui_test_backend(&tui, 100, 24);
        assert!(root_search.contains("Beta"));
        assert!(!root_search.contains("Gamma"));
        assert_eq!(*session.lock().unwrap(), original_context);

        tui.handle(Event::Key(Key::Escape));
        tui.apply_submission_outcome(router.open_dialog("sessions").unwrap());
        SessionStore::open(bootstrap.data_directory())
            .unwrap()
            .delete_session(current.id)
            .unwrap();
        let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("session Enter should dispatch through the router");
        };
        let outcome = router.route_request(TuiRouteRequest::DialogAction(action_id), progress);
        tui.apply_submission_outcome(outcome);
        assert_eq!(tui.view().session, format!("session #{}", current.id));
        assert_eq!(*session.lock().unwrap(), original_context);
        assert!(render_tui_test_backend(&tui, 100, 24).contains("saved session is unavailable"));
        tui.handle(Event::Key(Key::Escape));
        assert!(render_tui_test_backend(&tui, 100, 24).contains("previous request"));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn session_relative_age_uses_stable_boundaries() {
        for (updated_at, expected) in [
            (100_000, "now"),
            (99_941, "59s ago"),
            (99_940, "1m ago"),
            (96_401, "59m ago"),
            (96_400, "1h ago"),
            (13_601, "23h ago"),
            (13_600, "1d ago"),
        ] {
            assert_eq!(session_relative_age(updated_at, 100_000), expected);
        }
    }

    #[test]
    fn tui_resume_overlay_restores_appends_reopens_and_resets_complete_history() {
        let temporary = tui_session_directory("resume-production-path");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let first = persist_tui_session(&mut store, &tui_project(&temporary), "history");
        let restored =
            append_tui_session_turn(&mut store, &first, "second request", "second answer");
        let restored_messages = store.load_session_for_resume(restored.id).unwrap().messages;
        drop(store);

        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        tui.set_palette_entries(router.palette_entries().to_vec());
        let (progress, _) = std::sync::mpsc::channel();

        open_tui_palette_dialog(&mut tui, &router, "/re", "sessions", progress.clone());
        dispatch_tui_dialog_selection(&mut tui, &router, progress.clone());
        let restored_render = render_tui_test_backend(&tui, 120, 50);
        for expected in [
            "previous request",
            "Thought",
            "previous answer",
            "persisted reminder",
            "second request",
            "second answer",
        ] {
            assert!(restored_render.contains(expected), "{restored_render:?}");
            assert_eq!(
                restored_render.matches(expected).count(),
                1,
                "{restored_render:?}"
            );
        }
        // Tool name appears on header and result footer; assert the card chrome once.
        assert!(restored_render.contains("read {}"), "{restored_render:?}");
        assert_eq!(
            restored_render.matches("read {}").count(),
            1,
            "{restored_render:?}"
        );
        assert!(
            restored_render.contains("output collapsed"),
            "{restored_render:?}"
        );
        assert!(
            !restored_render.contains("previous reasoning"),
            "{restored_render:?}"
        );
        assert!(
            !restored_render.contains("previous result"),
            "{restored_render:?}"
        );
        assert!(
            !restored_render.contains("resume-call"),
            "{restored_render:?}"
        );

        tui.handle(Event::Key(Key::PageUp));
        let restored_anchor = (
            tui.view().following_bottom,
            tui.view().scroll_offset,
            tui.view().focus,
        );

        // Ctrl+O is thinking-first: expand collapsed reasoning before tool bodies.
        tui.handle(Event::Key(Key::CtrlO));
        assert_eq!(
            (
                tui.view().following_bottom,
                tui.view().scroll_offset,
                tui.view().focus,
            ),
            restored_anchor
        );
        let thinking_expanded = render_tui_test_backend(&tui, 120, 50);
        assert!(
            thinking_expanded.contains("previous reasoning"),
            "{thinking_expanded:?}"
        );
        assert!(
            !thinking_expanded.contains("previous result"),
            "{thinking_expanded:?}"
        );

        tui.handle(Event::Key(Key::CtrlO));
        assert_eq!(
            (
                tui.view().following_bottom,
                tui.view().scroll_offset,
                tui.view().focus,
            ),
            restored_anchor
        );
        let tools_expanded = render_tui_test_backend(&tui, 120, 50);
        assert!(
            tools_expanded.contains("previous result"),
            "{tools_expanded:?}"
        );

        tui.handle(Event::Key(Key::CtrlO));
        assert_eq!(
            (
                tui.view().following_bottom,
                tui.view().scroll_offset,
                tui.view().focus,
            ),
            restored_anchor
        );
        // Completes the Collapsed -> Truncated -> Expanded -> Collapsed
        // cycle (S1 renders Truncated and Expanded identically).
        tui.handle(Event::Key(Key::CtrlO));
        assert_eq!(
            (
                tui.view().following_bottom,
                tui.view().scroll_offset,
                tui.view().focus,
            ),
            restored_anchor
        );
        assert_eq!(
            tui.view().tool_display_modes.get("resume-call"),
            Some(&agens_tui::DisplayMode::Collapsed)
        );
        tui.handle(Event::Key(Key::End));

        assert_eq!(tui.view().session, format!("session #{}", restored.id));
        assert!(tui.transcript().is_empty());
        assert!(!restored_render.contains("INFO      Resumed session"));

        let before_failure = session.lock().unwrap().clone();
        let input = enter_tui_input(&mut tui, "/resume 999");
        tui.apply_submission_outcome(router.route(input));
        let failed = render_tui_test_backend(&tui, 120, 50);
        assert!(
            failed.contains("saved session is unavailable"),
            "{failed:?}"
        );
        assert!(failed.contains("Action:"), "{failed:?}");
        assert_eq!(tui.view().session, format!("session #{}", restored.id));
        assert_eq!(*session.lock().unwrap(), before_failure);
        assert!(tui.transcript().is_empty());

        tui.handle(Event::Key(Key::Escape));
        let prompt = enter_tui_input(&mut tui, "third request");
        let prompt = tui.apply_submission_outcome(router.route(prompt)).unwrap();
        let result = run_tui_prompt_with(
            &bootstrap,
            &prompt,
            &router.session,
            Some(Arc::clone(&router.skills)),
            |request| {
                assert_eq!(request.history, restored_messages);
                let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
                let metadata = append_tui_session_turn(
                    &mut store,
                    request.session.as_ref().unwrap(),
                    "third request",
                    "third answer",
                );
                let messages = store.load_session_for_resume(metadata.id).unwrap().messages;
                Ok(HeadlessChatCompletion {
                    text: "third answer".into(),
                    metadata,
                    messages,
                })
            },
        );
        tui.finish_provider_turn(tui_provider_outcome(result));
        let reopened = SessionStore::open(bootstrap.data_directory())
            .unwrap()
            .load_session_for_resume(restored.id)
            .unwrap();
        assert_eq!(session.lock().unwrap().messages, reopened.messages);

        open_tui_palette_dialog(&mut tui, &router, "/re", "sessions", progress);
        dispatch_tui_dialog_selection(&mut tui, &router, std::sync::mpsc::channel().0);
        let reopened_render = render_tui_test_backend(&tui, 120, 60);
        for expected in [
            "previous request",
            "second request",
            "third request",
            "third answer",
        ] {
            assert_eq!(
                reopened_render.matches(expected).count(),
                1,
                "{reopened_render:?}"
            );
        }

        for _ in 0..20 {
            tui.handle(Event::Key(Key::PageUp));
        }
        assert!(render_tui_test_backend(&tui, 60, 14).contains("previous request"));

        let input = enter_tui_input(&mut tui, "/new");
        tui.apply_submission_outcome(router.route(input));
        let reset = render_tui_test_backend(&tui, 120, 24);
        assert_eq!(tui.view().session, "new session");
        assert!(!reset.contains("previous request"), "{reset:?}");
        assert!(!reset.contains("INFO"), "{reset:?}");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_connect_and_disconnect_overlays_select_flows_and_cancel_without_credentials_mutation() {
        let temporary = tui_session_directory("auth-overlays");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        let initial_credentials = r#"{"openai-api":{"api_key":"preserved"}}"#;
        std::fs::write(&credentials_path, initial_credentials).unwrap();
        let flows = Arc::new(Mutex::new(Vec::new()));
        let coordinator = ChatGptAuthCoordinator::with_authenticator({
            let flows = Arc::clone(&flows);
            move |flow, _, publish| {
                flows.lock().unwrap().push(flow);
                publish(ChatGptAuthProgress::BrowserUrl("auth-url".into()));
                Ok(test_chatgpt_credentials("new-access"))
            }
        });
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::with_auth_coordinator(
            tui_session_bootstrap(&temporary, &[]),
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            coordinator,
        );
        tui.set_palette_entries(router.palette_entries().to_vec());
        let (progress, _) = std::sync::mpsc::channel();

        for (prefix, down, flow) in [
            ("/co", false, ChatGptAuthFlow::Browser),
            ("/co", true, ChatGptAuthFlow::Device),
        ] {
            open_tui_palette_dialog(&mut tui, &router, prefix, "connect", progress.clone());
            if down {
                tui.handle(Event::Key(Key::Down));
            }
            dispatch_tui_dialog_selection(&mut tui, &router, progress.clone());
            assert_eq!(flows.lock().unwrap().last(), Some(&flow));
        }

        open_tui_palette_dialog(&mut tui, &router, "/di", "disconnect", progress.clone());
        let connected = std::fs::read_to_string(&credentials_path).unwrap();
        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        let after_cancel = std::fs::read_to_string(&credentials_path).unwrap();
        assert_eq!(after_cancel, connected);
        open_tui_palette_dialog(&mut tui, &router, "/di", "disconnect", progress);
        dispatch_tui_dialog_selection(&mut tui, &router, std::sync::mpsc::channel().0);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_router_connect_device_disconnect_uses_coordinator_without_provider_history() {
        let temporary = tui_session_directory("auth-router");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"openai-api":{"api_key":"preserved"},"other":{"value":"kept"}}"#,
        )
        .unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::Auto;
        bootstrap.provider_type = Some("openai-api".into());
        bootstrap.openai_api_key = Some("preserved".into());
        let flows = Arc::new(Mutex::new(Vec::new()));
        let coordinator = ChatGptAuthCoordinator::with_authenticator({
            let flows = Arc::clone(&flows);
            move |flow, _, publish| {
                flows.lock().unwrap().push(flow);
                publish(ChatGptAuthProgress::BrowserUrl("auth-url".into()));
                Ok(test_chatgpt_credentials("new-access"))
            }
        });
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::with_auth_coordinator(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            coordinator,
        );
        let (progress_tx, progress_rx) = std::sync::mpsc::channel();

        assert!(matches!(
            router.route_with_progress("/connect --device-auth".into(), progress_tx),
            TuiSubmissionOutcome::LocalInfo(_)
        ));
        assert_eq!(progress_rx.try_iter().count(), 1);
        assert_eq!(*flows.lock().unwrap(), vec![ChatGptAuthFlow::Device]);
        let context = session.lock().unwrap();
        assert_eq!(context.provider, Some(TuiProvider::OpenAiChatGpt));
        assert!(context.messages.is_empty());
        drop(context);
        let configured = router.bootstrap().unwrap();
        assert_eq!(configured.provider_type(), Some("openai-api"));
        let connected = std::fs::read_to_string(&credentials_path).unwrap();
        assert!(connected.contains("new-access"));

        assert!(router.disconnect().is_ok());
        assert_eq!(
            session.lock().unwrap().provider,
            Some(TuiProvider::OpenAiApi)
        );
        let stored = std::fs::read_to_string(&credentials_path).unwrap();
        assert!(stored.contains("preserved"));
        assert!(stored.contains("kept"));
        assert!(!stored.contains("new-access"));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_preserves_intervening_unrelated_provider_write() {
        let temporary = tui_session_directory("refresh-rollback");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        let before = br#"{"openai-api":{"api_key":"preserved"},"openai-chatgpt":{"access_token":"old-access","refresh_token":"old-refresh","account_id":"old-account","expires_at":"2099-01-01T00:00:00Z"}}"#;
        std::fs::write(&credentials_path, before).unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::Auto;
        bootstrap.provider_type = Some("openai-api".into());
        bootstrap.openai_api_key = Some("preserved".into());
        let session = Arc::new(Mutex::new(TuiSessionContext {
            running: true,
            ..TuiSessionContext::fresh()
        }));
        let original_runtime = session.lock().unwrap().clone();
        let router = TuiRuntimeRouter::with_auth_coordinator(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            ChatGptAuthCoordinator::with_authenticator(|_, _, _| {
                Ok(test_chatgpt_credentials("new-access"))
            }),
        )
        .with_credential_restorer(|path, snapshot| {
            upsert_provider_entry(path, "other-provider", serde_json::json!({"key": "kept"}))
                .map_err(|_| CliError::storage("unrelated provider write failed"))?;
            restore_chatgpt_credentials(path, snapshot)
        });

        assert!(
            router
                .connect(ChatGptAuthFlow::Browser, std::sync::mpsc::channel().0)
                .is_err()
        );
        let mut expected = serde_json::from_slice::<serde_json::Value>(before).unwrap();
        expected
            .as_object_mut()
            .unwrap()
            .insert("other-provider".into(), serde_json::json!({"key": "kept"}));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&credentials_path).unwrap())
                .unwrap(),
            expected
        );
        assert_eq!(*session.lock().unwrap(), original_runtime);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_disconnects_explicit_chatgpt_fail_closed() {
        let temporary = tui_session_directory("explicit-disconnect");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"openai-api":{"api_key":"preserved"},"openai-chatgpt":{"access_token":"old-access","refresh_token":"old-refresh","account_id":"old-account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::ExplicitChatGpt;
        bootstrap.provider_type = Some("openai-chatgpt".into());
        let session = Arc::new(Mutex::new(TuiSessionContext {
            provider: Some(TuiProvider::OpenAiChatGpt),
            ..TuiSessionContext::fresh()
        }));
        ensure_active_tui_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );

        assert!(router.disconnect().is_ok());
        assert_eq!(session.lock().unwrap().provider, None);
        assert!(session.lock().unwrap().chatgpt_unavailable);
        assert!(session.lock().unwrap().active_agent.is_none());
        let error = match router.turn_bootstrap() {
            Ok(_) => panic!("disconnected ChatGPT runtime must be unavailable"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "auth: ChatGPT credentials are unavailable; run /connect"
        );
        assert!(
            !std::fs::read_to_string(&credentials_path)
                .unwrap()
                .contains("old-access")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_fails_closed_when_credential_restore_fails() {
        let temporary = tui_session_directory("restore-failure");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"openai-api":{"api_key":"preserved"}}"#,
        )
        .unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::Auto;
        bootstrap.provider_type = Some("openai-api".into());
        bootstrap.openai_api_key = Some("preserved".into());
        let session = Arc::new(Mutex::new(TuiSessionContext {
            running: true,
            ..TuiSessionContext::fresh()
        }));
        let router = TuiRuntimeRouter::with_auth_coordinator(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            ChatGptAuthCoordinator::with_authenticator(|_, _, _| {
                Ok(test_chatgpt_credentials("new-access"))
            }),
        )
        .with_credential_restorer(|_, _| Err(CliError::storage("injected restore failure")));

        let outcome = auth_route_outcome(
            router.connect(ChatGptAuthFlow::Browser, std::sync::mpsc::channel().0),
        );
        assert!(matches!(
            outcome,
            TuiSubmissionOutcome::LocalActionableError { message, .. }
                if message == "store: ChatGPT credential recovery failed"
        ));
        assert!(session.lock().unwrap().chatgpt_unavailable);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_preserves_runtime_on_credential_write_failures() {
        let temporary = tui_session_directory("credential-write-failures");
        let config_home = temporary.join("config");
        std::fs::create_dir_all(&config_home).unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.paths.credentials = config_home.clone();
        let session = Arc::new(Mutex::new(TuiSessionContext {
            provider: Some(TuiProvider::OpenAiApi),
            ..TuiSessionContext::fresh()
        }));
        let original_runtime = session.lock().unwrap().clone();
        let router = TuiRuntimeRouter::with_auth_coordinator(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            ChatGptAuthCoordinator::with_authenticator(|_, _, _| {
                Ok(test_chatgpt_credentials("new-access"))
            }),
        );

        for outcome in [
            auth_route_outcome(
                router.connect(ChatGptAuthFlow::Browser, std::sync::mpsc::channel().0),
            ),
            auth_route_outcome(router.disconnect()),
        ] {
            assert!(matches!(
                outcome,
                TuiSubmissionOutcome::LocalActionableError { message, .. }
                    if message == "ChatGPT credentials could not be saved"
            ));
            assert_eq!(*session.lock().unwrap(), original_runtime);
        }

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_leaves_auto_unavailable_after_disconnect_rebuild_failure()
    {
        let temporary = tui_session_directory("auto-disconnect-failure");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"openai-chatgpt":{"access_token":"old-access","refresh_token":"old-refresh","account_id":"old-account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::Auto;
        bootstrap.provider_type = Some("openai-chatgpt".into());
        let session = Arc::new(Mutex::new(TuiSessionContext {
            provider: Some(TuiProvider::OpenAiChatGpt),
            ..TuiSessionContext::fresh()
        }));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );

        assert!(router.disconnect().is_err());
        assert!(session.lock().unwrap().chatgpt_unavailable);
        assert!(
            !std::fs::read_to_string(&credentials_path)
                .unwrap()
                .contains("old-access")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    fn test_chatgpt_credentials(
        access_token: &str,
    ) -> agens_providers::chatgpt_login::ChatGptCredentials {
        agens_providers::chatgpt_login::ChatGptCredentials {
            access_token: access_token.into(),
            refresh_token: "refresh".into(),
            account_id: "account".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn tui_session_busy_agent_command_leaves_context_and_store_unchanged() {
        let temporary = tui_session_directory("busy-agent-command");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "all",
                "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
            )],
        );
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "current");
        let saved_sessions = store.list_sessions().unwrap();
        drop(store);
        let session = Arc::new(Mutex::new(TuiSessionContext {
            identifier: Some(metadata.id),
            metadata: Some(metadata),
            messages: tui_session_messages(),
            selected_subagent: Some("reviewer".into()),
            running: true,
            ..TuiSessionContext::fresh()
        }));
        let original = session.lock().unwrap().clone();

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/agent all",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap_err()
            .to_string(),
            "runtime: headless turn entered an invalid state"
        );
        assert_eq!(*session.lock().unwrap(), original);
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .list_sessions()
                .unwrap(),
            saved_sessions
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_file_candidates_and_expansion_use_confined_reads() {
        let temporary = tui_session_directory("files");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = temporary.join("project");
        std::fs::write(project.join("zeta.txt"), "zeta").unwrap();
        std::fs::write(project.join("alpha.txt"), "alpha").unwrap();
        let oversized = vec![b'x'; 1024 * 1024 + 1];
        std::fs::write(project.join("large.txt"), oversized).unwrap();

        assert_eq!(
            tui_file_candidates(&bootstrap).unwrap(),
            vec!["alpha.txt".to_owned(), "zeta.txt".to_owned()]
        );
        assert_eq!(
            expand_tui_file_reference(&bootstrap, "review @alpha.txt please").unwrap(),
            "review <file path=\"alpha.txt\">\nalpha\n</file> please"
        );
        assert_eq!(
            expand_tui_file_reference(&bootstrap, "@../outside.txt")
                .unwrap_err()
                .to_string(),
            "file: path: traversal is not allowed"
        );
        assert_eq!(
            expand_tui_file_reference(&bootstrap, "@large.txt")
                .unwrap_err()
                .to_string(),
            "file: read: file exceeds 1048576 byte limit"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn tui_native_select_preserves_running_turn_outcomes_and_terminal_cleanup() {
        use std::os::unix::fs::symlink;

        let temporary = tui_session_directory("native-select");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = temporary.join("project");
        let outside = temporary.join("outside.txt");
        std::fs::write(project.join("approved.txt"), "approved").unwrap();
        std::fs::create_dir(project.join("directory")).unwrap();
        std::fs::write(project.join("large.txt"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        std::fs::write(&outside, "outside").unwrap();
        symlink(&outside, project.join("escape.txt")).unwrap();
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        let mut control = TuiTerminalControl::default();
        let mut guard = agens_tui::TerminalModeGuard::enter(&mut control).unwrap();
        let transcript_count = open_running_tui_select(&mut tui, &router);
        assert!(render_tui_test_backend(&tui, 80, 24).contains("Select project file"));
        assert_eq!(
            tui.handle(Event::Key(Key::Escape)),
            Action::SafeDialogAction("select:cancel".into())
        );
        let cancelled = router.route_request(
            TuiRouteRequest::DialogAction("select:cancel".into()),
            std::sync::mpsc::channel().0,
        );
        assert_eq!(cancelled, TuiSubmissionOutcome::SelectionCancelled);
        assert!(tui.apply_submission_outcome(cancelled).is_none());
        assert!(tui.view().dialog.is_none());
        assert!(tui.view().running);
        assert_eq!(tui.transcript().len(), transcript_count);
        assert!(
            tui.apply_submission_outcome(router.route_request(
                TuiRouteRequest::DialogAction("select:cancel".into()),
                std::sync::mpsc::channel().0,
            ))
            .is_none()
        );
        assert_eq!(tui.transcript().len(), transcript_count);
        open_running_tui_select(&mut tui, &router);
        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
        assert!(tui.view().quit_armed);
        assert!(tui.view().dialog.is_some());
        assert_eq!(
            tui.handle(Event::Key(Key::Escape)),
            Action::SafeDialogAction("select:cancel".into())
        );
        assert_eq!(
            router.route_request(
                TuiRouteRequest::DialogAction("select:cancel".into()),
                std::sync::mpsc::channel().0,
            ),
            TuiSubmissionOutcome::SelectionCancelled
        );
        guard.restore(&mut control).unwrap();
        assert_tui_terminal_restored(&control);

        let mut control = TuiTerminalControl::default();
        let mut guard = agens_tui::TerminalModeGuard::enter(&mut control).unwrap();
        let transcript_count = open_running_tui_select(&mut tui, &router);
        let Action::SafeDialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("selection Enter should use the safe local action path");
        };
        let selected = router.route_request(
            TuiRouteRequest::DialogAction(action_id),
            std::sync::mpsc::channel().0,
        );
        assert_eq!(
            selected,
            TuiSubmissionOutcome::SelectionInfo("Selected file: approved.txt".into())
        );
        assert!(tui.apply_submission_outcome(selected).is_none());
        assert!(tui.view().running);
        assert_eq!(tui.transcript().len(), transcript_count);
        guard.restore(&mut control).unwrap();
        assert_tui_terminal_restored(&control);

        let mut control = TuiTerminalControl::default();
        let mut guard = agens_tui::TerminalModeGuard::enter(&mut control).unwrap();
        let transcript_count = open_running_tui_select(&mut tui, &router);
        let rejected = router.route_request(
            TuiRouteRequest::DialogAction("select:escape.txt".into()),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(
            rejected,
            TuiSubmissionOutcome::SelectionError { .. }
        ));
        assert!(tui.apply_submission_outcome(rejected).is_none());
        assert!(tui.view().running);
        assert_eq!(tui.transcript().len(), transcript_count);
        guard.restore(&mut control).unwrap();
        assert_tui_terminal_restored(&control);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[derive(Default)]
    struct TuiTerminalControl {
        operations: Vec<agens_tui::TerminalOperation>,
    }

    impl agens_tui::TerminalControl for TuiTerminalControl {
        fn apply(&mut self, operation: agens_tui::TerminalOperation) -> std::io::Result<()> {
            self.operations.push(operation);
            Ok(())
        }
    }

    fn assert_tui_terminal_restored(control: &TuiTerminalControl) {
        use agens_tui::TerminalOperation::*;

        assert_eq!(
            control.operations,
            vec![
                EnableRaw,
                EnterAlternate,
                HideCursor,
                EnableMouse,
                EnableKeyboardEnhancement,
                EnablePaste,
                DisablePaste,
                DisableKeyboardEnhancement,
                DisableMouse,
                ShowCursor,
                LeaveAlternate,
                DisableRaw,
            ]
        );
    }

    #[test]
    fn second_control_c_uses_the_owned_turn_cancellation_before_quit() {
        let cancellation = HeadlessTurnCancellation::new();
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(Some(cancellation.clone()))),
        });
        tui.set_running(true);

        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
        assert!(!cancellation.is_cancelled());
        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Quit);
        assert!(cancellation.is_cancelled());
    }

    fn open_running_tui_select(
        tui: &mut Tui<ProductionTuiEngine>,
        router: &TuiRuntimeRouter,
    ) -> usize {
        tui.begin_submission("running");
        let transcript_count = tui.transcript().len();
        for character in "/select".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }

        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::OpenDialog("select".into())
        );
        let outcome = router.route_request(
            TuiRouteRequest::OpenDialog("select".into()),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(outcome, TuiSubmissionOutcome::SafeDialog(_)));
        assert!(tui.apply_submission_outcome(outcome).is_none());
        assert!(tui.view().running);
        assert_eq!(tui.transcript().len(), transcript_count);

        transcript_count
    }

    fn tui_session_directory(label: &str) -> PathBuf {
        let temporary = std::env::temp_dir().join(format!(
            "agens-tui-session-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(temporary.join("project/.git")).unwrap();
        temporary
    }

    fn enter_tui_input(tui: &mut Tui<ProductionTuiEngine>, input: &str) -> String {
        for character in input.chars() {
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
        }
        let agens_tui::Action::Submit(input) =
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter))
        else {
            panic!("Enter should submit through the production TUI path");
        };
        input
    }

    fn render_tui_test_backend(tui: &Tui<ProductionTuiEngine>, width: u16, height: u16) -> String {
        let terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut renderer = agens_tui::RatatuiRenderer::new(terminal);
        agens_tui::Renderer::render(&mut renderer, tui.view()).unwrap();
        renderer
            .terminal()
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn open_tui_palette_dialog(
        tui: &mut Tui<ProductionTuiEngine>,
        router: &TuiRuntimeRouter,
        prefix: &str,
        expected_route: &str,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) {
        for character in prefix.chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        let Action::OpenDialog(route_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("palette Enter should open a dialog");
        };
        assert_eq!(route_id, expected_route);
        let outcome = router.route_request(TuiRouteRequest::OpenDialog(route_id), progress);
        assert!(tui.apply_submission_outcome(outcome).is_none());
    }

    fn dispatch_tui_dialog_selection(
        tui: &mut Tui<ProductionTuiEngine>,
        router: &TuiRuntimeRouter,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) {
        let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("dialog Enter should dispatch an action");
        };
        let outcome = router.route_request(TuiRouteRequest::DialogAction(action_id), progress);
        assert!(tui.apply_submission_outcome(outcome).is_none());
    }

    fn dispatch_tui_session_page(
        tui: &mut Tui<ProductionTuiEngine>,
        router: &TuiRuntimeRouter,
        action: Action,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) {
        let Action::LoadSessionPage(request) = action else {
            panic!("session dialog action should request a page");
        };
        let outcome = router.route_request(TuiRouteRequest::SessionPage(request), progress);
        assert!(tui.apply_submission_outcome(outcome).is_none());
    }

    fn write_tui_command(root: &Path, name: &str, description: &str, template: &str) {
        std::fs::write(
            root.join(format!("{name}.md")),
            format!("---\ndescription: {description}\n---\n{template}\n"),
        )
        .unwrap();
    }

    fn write_tui_skill(root: &Path, name: &str, description: &str, body: &str) {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
        )
        .unwrap();
    }

    fn submit_tui_command(
        tui: &mut Tui<ProductionTuiEngine>,
        router: &TuiRuntimeRouter,
        bootstrap: &Bootstrap,
        input: &str,
        captured: &Arc<Mutex<Vec<HeadlessChatRequest>>>,
    ) {
        let input = enter_tui_input(tui, input);
        let Some(prompt) = tui.apply_submission_outcome(router.route(input)) else {
            return;
        };
        let result = run_tui_prompt_with(
            bootstrap,
            &prompt,
            &router.session,
            Some(Arc::clone(&router.skills)),
            {
                let captured = Arc::clone(captured);
                move |request| {
                    captured.lock().unwrap().push(request);
                    Ok(HeadlessChatCompletion {
                        text: "captured".into(),
                        metadata: SessionMetadata {
                            id: 1,
                            project: "project".into(),
                            title: "captured".into(),
                            active_agent: "build".into(),
                            provider_id: None,
                            model_id: None,
                            reasoning_effort: None,
                            created_at: 1,
                            updated_at: 1,
                            completed_turn_count: 1,
                            resumable: true,
                        },
                        messages: Vec::new(),
                    })
                }
            },
        );
        tui.finish_provider_turn(tui_provider_outcome(result));
    }

    fn tui_project(temporary: &Path) -> String {
        temporary.join("project").display().to_string()
    }

    fn tui_session_bootstrap(temporary: &Path, agents: &[(&str, &str)]) -> Bootstrap {
        tui_session_bootstrap_for_provider(temporary, agents, "openai-api", "gpt-4.1")
    }

    fn tui_session_bootstrap_for_provider(
        temporary: &Path,
        agents: &[(&str, &str)],
        provider: &str,
        model: &str,
    ) -> Bootstrap {
        let config_home = temporary.join("config");
        let data_directory = temporary.join("data");
        let agents_directory = config_home.join("agents");
        std::fs::create_dir_all(&agents_directory).unwrap();
        for (name, contents) in agents {
            std::fs::write(agents_directory.join(format!("{name}.md")), contents).unwrap();
        }
        bootstrap(&CliDependencies::for_test(
            temporary.join("project"),
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            BTreeMap::from([(
                config_home.join("config.toml"),
                format!(
                    "[provider]\ntype = \"{provider}\"\nmodel = \"{model}\"\n\n[options]\ndata_dir = \"{}\"\n",
                    data_directory.display()
                ),
            )]),
        ))
        .unwrap()
    }

    #[test]
    fn bootstrap_retains_the_ui_collapse_thinking_setting() {
        let temporary =
            std::env::temp_dir().join(format!("agens-collapse-thinking-{}", std::process::id()));
        let config_home = temporary.join("config");
        let dependencies = CliDependencies::for_test(
            temporary.join("project"),
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            BTreeMap::from([(
                config_home.join("config.toml"),
                "[ui]\ncollapse_thinking = true\n".to_owned(),
            )]),
        );

        let bootstrap = bootstrap(&dependencies).expect("UI configuration should be valid");

        assert!(bootstrap.collapse_thinking);
    }

    fn tui_session_messages() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("previous request".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::Reasoning("previous reasoning".into()),
                    MessagePart::ToolCall {
                        id: "resume-call".into(),
                        name: "read".into(),
                        input: "{}".into(),
                    },
                    MessagePart::Text("previous answer".into()),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "resume-call".into(),
                    content: "previous result".into(),
                    is_error: false,
                }],
            },
        ]
    }

    fn append_tui_session_turn(
        store: &mut SessionStore,
        metadata: &SessionMetadata,
        user: &str,
        answer: &str,
    ) -> SessionMetadata {
        let messages = vec![
            Message {
                role: Role::System,
                parts: vec![MessagePart::Text("persisted reminder".into())],
            },
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text(user.into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text(answer.into())],
            },
        ];
        let turn = CompletedSessionTurn::new(
            messages
                .into_iter()
                .map(SessionMessage::try_from)
                .collect::<Result<_, _>>()
                .unwrap(),
        )
        .unwrap();
        store
            .persist_completed_session_turn(metadata, &turn)
            .unwrap()
    }

    fn persist_tui_session(
        store: &mut SessionStore,
        project: &str,
        title: &str,
    ) -> SessionMetadata {
        let turn = CompletedSessionTurn::new(
            tui_session_messages()
                .into_iter()
                .map(SessionMessage::try_from)
                .collect::<Result<_, _>>()
                .unwrap(),
        )
        .unwrap();
        store
            .persist_completed_session_turn(
                &SessionMetadata {
                    id: 0,
                    project: project.into(),
                    title: title.into(),
                    active_agent: "primary".into(),
                    provider_id: None,
                    model_id: None,
                    reasoning_effort: None,
                    created_at: 1,
                    updated_at: 1,
                    completed_turn_count: 0,
                    resumable: false,
                },
                &turn,
            )
            .unwrap()
    }

    fn persist_tui_session_metadata(
        store: &mut SessionStore,
        project: &str,
        title: &str,
        active_agent: &str,
        updated_at: i64,
    ) -> SessionMetadata {
        let mut metadata = persist_tui_session(store, project, title);
        metadata.active_agent = active_agent.into();
        metadata.updated_at = updated_at;
        store.update_session(&metadata).unwrap();
        metadata
    }

    #[test]
    fn resumed_tui_session_preserves_typed_history_for_the_next_prompt() {
        let metadata = SessionMetadata {
            id: 7,
            project: "project".into(),
            title: "conversation".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 10,
            updated_at: 20,
            completed_turn_count: 1,
            resumable: true,
        };
        let messages = vec![
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::Reasoning("previous reasoning".into()),
                    MessagePart::ToolCall {
                        id: "call-1".into(),
                        name: "native::read".into(),
                        input: r#"{"path":"notes.md"}"#.into(),
                    },
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "call-1".into(),
                    content: "previous result".into(),
                    is_error: false,
                }],
            },
        ];

        let dispatcher = rotation_dispatcher();
        let active_agent = ActiveAgentRuntime::build(
            &rotation_agent("primary", None, false),
            None,
            "project",
            &dispatcher,
            &BundledModelValidator,
        )
        .unwrap();
        let request = TuiSessionContext::resumed(7, metadata, messages.clone(), active_agent)
            .apply_to(HeadlessChatRequest {
                prompt: "next question".into(),
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

        assert_eq!(request.prompt, "next question");
        assert_eq!(request.history, messages);
        assert_eq!(request.system_prompt.as_deref(), Some("You are primary."));
        assert_eq!(request.session.as_ref().map(|session| session.id), Some(7));
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

        let mut request = resume_tui_session(
            &bootstrap,
            1,
            &SkillCatalog::default(),
            &TuiCredentialResolver::production(),
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

    #[test]
    fn fresh_tui_session_does_not_reuse_prior_context() {
        let request = TuiSessionContext::fresh().apply_to(HeadlessChatRequest {
            prompt: "new question".into(),
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

        assert_eq!(request.system_prompt, None);
    }

    #[test]
    fn tui_model_and_effort_commands_reach_each_provider_with_latest_selection_only() {
        for provider_type in ["openai-api", "openai-chatgpt"] {
            for model in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
                let request = run_tui_model_effort_provider_case(provider_type, model);

                assert_eq!(request["model"], model, "{provider_type}: {model}");
                assert_eq!(request["reasoning"]["effort"], "max", "{request}");
                assert!(
                    !request["input"].to_string().contains("gpt-4.1"),
                    "{provider_type} request input retained the replaced model: {request}"
                );
            }
        }
    }

    fn run_tui_model_effort_provider_case(
        provider_type: &str,
        selected_model: &str,
    ) -> serde_json::Value {
        let temporary = std::env::temp_dir().join(format!(
            "agens-tui-model-effort-{provider_type}-{}-{}",
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
        std::fs::create_dir_all(&config_home).expect("config directory should be created");

        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("mock provider should bind");
        let address = listener
            .local_addr()
            .expect("mock provider should have an address");
        let expected_path = match provider_type {
            "openai-chatgpt" => "POST /codex/responses HTTP/1.1\r\n",
            _ => "POST /responses HTTP/1.1\r\n",
        };
        let worker = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};

            let (mut stream, _) = listener
                .accept()
                .expect("mock provider should accept the selected request");
            let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .expect("request line should be readable");
            assert_eq!(request_line, expected_path);

            let mut content_length = None;
            loop {
                let mut header = String::new();
                reader
                    .read_line(&mut header)
                    .expect("request header should be readable");
                if header == "\r\n" {
                    break;
                }
                if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length: ") {
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
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"selected answer\"}\n\ndata: {\"type\":\"response.completed\"}\n\n")
                .expect("mock response should be written");

            serde_json::from_slice::<serde_json::Value>(&body)
                .expect("provider request should be valid JSON")
        });

        if provider_type == "openai-chatgpt" {
            std::fs::write(
                config_home.join("auth.json"),
                r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature","refresh_token":"refresh","account_id":"account","expires_at":"2030-01-01T00:00:00Z"}}"#,
            )
            .expect("ChatGPT credentials should be written");
        } else {
            std::fs::write(
                config_home.join("auth.json"),
                r#"{"openai-api":{"api_key":"test-key"}}"#,
            )
            .expect("OpenAI API credentials should be written");
        }

        let dependencies = CliDependencies::for_test(
            project_root,
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
                    "[provider]\ntype = \"{provider_type}\"\nmodel = \"gpt-4.1\"\nbase_url = \"http://{address}\"\n\n[options]\ndata_dir = \"{}\"\n",
                    data_directory.display()
                ),
            )]),
        );
        let bootstrap = bootstrap(&dependencies).expect("production bootstrap should be valid");
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = HeadlessTurnCancellation::new();

        let previous_model = if provider_type == "openai-chatgpt" {
            "gpt-5.4"
        } else {
            "o3"
        };
        let commands = [
            (
                format!("/model {previous_model}"),
                format!("Model: {previous_model}."),
            ),
            (
                "/effort high".to_owned(),
                "Reasoning effort: high.".to_owned(),
            ),
            (
                format!("/model {selected_model}"),
                format!("Model: {selected_model}."),
            ),
            (
                "/effort max".to_owned(),
                "Reasoning effort: max.".to_owned(),
            ),
        ];
        for (command, expected) in commands {
            assert_eq!(
                run_tui_prompt(&bootstrap, &command, &cancellation, &session, None)
                    .expect("valid TUI selection should succeed"),
                expected
            );
        }
        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/model unavailable",
                &cancellation,
                &session,
                None
            )
            .expect_err("invalid model should be refused")
            .to_string(),
            format!(
                "config: model is unavailable for {}",
                if provider_type == "openai-chatgpt" {
                    "ChatGPT subscription"
                } else {
                    "OpenAI API"
                }
            )
        );
        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/effort unsupported",
                &cancellation,
                &session,
                None
            )
            .expect_err("invalid effort should be refused")
            .to_string(),
            "config: reasoning effort is unsupported"
        );
        let runtime_bootstrap = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        )
        .turn_bootstrap()
        .expect("turn provider credentials should resolve freshly");
        assert_eq!(
            run_tui_prompt(
                &runtime_bootstrap,
                "next request",
                &cancellation,
                &session,
                None
            )
            .expect("selected prompt should complete"),
            "selected answer"
        );

        let persisted = SessionStore::open(&data_directory)
            .unwrap()
            .load_session_for_resume(1)
            .unwrap();
        assert_eq!(
            persisted.metadata.provider_id.as_deref(),
            Some(provider_type)
        );
        assert_eq!(persisted.metadata.model_id.as_deref(), Some(selected_model));
        assert_eq!(
            persisted
                .metadata
                .reasoning_effort
                .map(agens_core::ReasoningEffort::as_str),
            Some("max")
        );
        assert!(!format!("{persisted:?}").contains("test-key"));
        assert!(!format!("{persisted:?}").contains("refresh"));

        let reopened = resume_tui_session(
            &bootstrap,
            persisted.metadata.id,
            &SkillCatalog::default(),
            &TuiCredentialResolver::with_environment(BTreeMap::from([(
                "OPENAI_API_KEY".into(),
                "test-key".into(),
            )])),
        )
        .expect("persisted selection should reopen");
        let reopened_selection = reopened.selection.unwrap();
        assert_eq!(reopened_selection.model(), selected_model);
        assert!(reopened_selection.metadata_known());
        assert_eq!(reopened_selection.reasoning_effort(), Some("max"));

        let request = worker.join().expect("mock provider should finish");
        std::fs::remove_dir_all(temporary).expect("temporary files should be removed");
        request
    }

    #[test]
    fn permission_prompt_answers_preserve_choices_and_redact_sensitive_targets() {
        for (input, expected) in [
            ("a", PermissionPromptAnswer::AllowOnce),
            ("always", PermissionPromptAnswer::AllowAlways),
            ("d", PermissionPromptAnswer::DenyOnce),
            ("deny-always", PermissionPromptAnswer::DenyAlways),
            ("cancel", PermissionPromptAnswer::Cancel),
        ] {
            assert_eq!(parse_permission_prompt_answer(input), Some(expected));
        }
        assert_eq!(parse_permission_prompt_answer("unknown"), None);

        let prompt = render_permission_prompt(&agens_tools::PermissionPromptContext {
            project_id: "project".into(),
            qualified_tool_name: "native::webfetch".into(),
            target_identifier:
                "https://user:SENTINEL_URL_SECRET@example.test/path?token=SENTINEL_TOKEN".into(),
            access: agens_core::ToolAccess::ReadOnly,
            reason: "permission policy requires confirmation".into(),
        });

        assert!(prompt.contains("native::webfetch"));
        assert!(prompt.contains("https://example.test/path"));
        assert!(!prompt.contains("SENTINEL_URL_SECRET"));
        assert!(!prompt.contains("SENTINEL_TOKEN"));

        let prompt = render_permission_prompt(&agens_tools::PermissionPromptContext {
            project_id: "project".into(),
            qualified_tool_name: "native::webfetch".into(),
            target_identifier:
                "https://user:SENTINEL_URL_SECRET@example.test?token=SENTINEL_TOKEN#fragment".into(),
            access: agens_core::ToolAccess::ReadOnly,
            reason: "permission policy requires confirmation".into(),
        });

        assert!(prompt.contains("https://example.test/"));
        assert!(!prompt.contains("SENTINEL_URL_SECRET"));
        assert!(!prompt.contains("SENTINEL_TOKEN"));
        assert!(!prompt.contains("fragment"));

        let prompt = render_permission_prompt(&agens_tools::PermissionPromptContext {
            project_id: "project".into(),
            qualified_tool_name: "native::webfetch".into(),
            target_identifier: r#"{"url":"https://example.test","token":"SENTINEL_JSON"}"#.into(),
            access: agens_core::ToolAccess::ReadOnly,
            reason: "permission policy requires confirmation".into(),
        });

        assert!(prompt.contains("Target: [redacted]"));
        assert!(!prompt.contains("SENTINEL_JSON"));
    }

    struct BatchProvider {
        iterations: Vec<Result<Vec<MessagePart>, HeadlessTurnPortError>>,
    }

    impl TurnProvider for BatchProvider {
        fn next_parts(
            &mut self,
            _: &[TurnEvent],
            _: &HeadlessTurnCancellation,
        ) -> impl std::future::Future<Output = Result<Vec<MessagePart>, HeadlessTurnPortError>> + Send
        {
            std::future::ready(self.iterations.remove(0))
        }
    }

    struct BatchRepository {
        fail_persistence: bool,
    }

    impl CompletedTurnRepository for BatchRepository {
        fn persist_completed_turn(
            &mut self,
            _: CompletedTurnSnapshot,
        ) -> impl std::future::Future<Output = Result<(), agens_core::CompletedTurnStoreError>> + Send
        {
            if self.fail_persistence {
                std::future::ready(Err(agens_core::CompletedTurnStoreError::new(
                    "database unavailable",
                )))
            } else {
                std::future::ready(Ok(()))
            }
        }
    }

    struct RecordingPrompt {
        answers: Vec<PermissionPromptAnswer>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl PermissionPrompter for RecordingPrompt {
        fn prompt(
            &mut self,
            context: &PermissionPromptContext,
            _: &HeadlessTurnCancellation,
        ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
            self.calls
                .lock()
                .expect("prompt calls should be available")
                .push(context.target_identifier.clone());
            Ok(self.answers.remove(0))
        }
    }

    struct BatchTool {
        name: String,
        calls: Arc<Mutex<Vec<String>>>,
        cancellation: Option<HeadlessTurnCancellation>,
    }

    impl DispatchTool for BatchTool {
        fn permission_target(
            &self,
            arguments: &serde_json::Value,
        ) -> Result<String, agens_core::Error> {
            if arguments
                .get("_inject_permission_evaluator_failure")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                return Err(agens_core::Error::Tool(
                    "injected permission evaluator failure".into(),
                ));
            }

            NativePermissionTarget::parse(&self.name, arguments)
                .map(NativePermissionTarget::into_value)
                .map_err(|error| agens_core::Error::Tool(error.to_string()))
        }

        fn execute(
            &mut self,
            _: &ToolExecutionContext,
            arguments: serde_json::Value,
        ) -> Result<ToolOutput, agens_core::Error> {
            let path = self.permission_target(&arguments)?;
            self.calls
                .lock()
                .expect("tool calls should be available")
                .push(path.clone());
            if let Some(message) = arguments
                .get("_inject_tool_failure")
                .and_then(serde_json::Value::as_str)
            {
                return Ok(ToolOutput::failure(message));
            }
            if let Some(cancellation) = &self.cancellation {
                cancellation.cancel();
            }
            Ok(ToolOutput::success(format!("executed {path}")))
        }
    }

    fn batch_call(id: &str, path: &str) -> MessagePart {
        MessagePart::ToolCall {
            id: id.into(),
            name: "native::read".into(),
            input: format!(r#"{{"path":"{path}"}}"#),
        }
    }

    fn native_batch_call(id: &str, name: &str, arguments: serde_json::Value) -> MessagePart {
        MessagePart::ToolCall {
            id: id.into(),
            name: name.into(),
            input: serde_json::to_string(&arguments).expect("native test arguments should encode"),
        }
    }

    fn batch_policy() -> PermissionPolicy {
        PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Ask,
                PermissionPattern::Exact("native::read".into()),
                PermissionPattern::Any,
            )],
        )
    }

    struct BatchOutcome {
        result: Result<CompletedTurnSnapshot, HeadlessTurnError>,
        prompts: Vec<String>,
        executions: Vec<String>,
        progress: Vec<TurnEvent>,
        metrics: Vec<TuiRuntimeEvent>,
    }

    struct ProductionBatchInput<'a> {
        directory_name: &'a str,
        answers: Vec<PermissionPromptAnswer>,
        calls: Vec<MessagePart>,
        cancellation: Option<HeadlessTurnCancellation>,
        provider_error: Option<HeadlessTurnPortError>,
        fail_persistence: bool,
        policy: PermissionPolicy,
        bypass: bool,
        dangerous_override: bool,
    }

    impl<'a> ProductionBatchInput<'a> {
        fn new(
            directory_name: &'a str,
            answers: Vec<PermissionPromptAnswer>,
            calls: Vec<MessagePart>,
        ) -> Self {
            Self {
                directory_name,
                answers,
                calls,
                cancellation: None,
                provider_error: None,
                fail_persistence: false,
                policy: batch_policy(),
                bypass: false,
                dangerous_override: false,
            }
        }

        fn with_runtime(
            mut self,
            cancellation: Option<HeadlessTurnCancellation>,
            provider_error: Option<HeadlessTurnPortError>,
            fail_persistence: bool,
        ) -> Self {
            self.cancellation = cancellation;
            self.provider_error = provider_error;
            self.fail_persistence = fail_persistence;
            self
        }

        fn with_policy(mut self, policy: PermissionPolicy) -> Self {
            self.policy = policy;
            self
        }

        fn with_bypass(mut self) -> Self {
            self.bypass = true;
            self
        }

        fn with_dangerous_override(mut self) -> Self {
            self.dangerous_override = true;
            self
        }
    }

    fn run_ready<T>(
        future: impl std::future::Future<Output = Result<T, HeadlessTurnError>>,
    ) -> Result<T, HeadlessTurnError> {
        let mut future = std::pin::pin!(future);
        let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

        match future.as_mut().poll(context) {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => panic!("batch ports must complete synchronously"),
        }
    }

    fn run_production_batch(
        directory_name: &str,
        answers: Vec<PermissionPromptAnswer>,
        calls: Vec<MessagePart>,
        cancellation: Option<HeadlessTurnCancellation>,
        provider_error: Option<HeadlessTurnPortError>,
        fail_persistence: bool,
    ) -> BatchOutcome {
        run_production_batch_with_policy(
            ProductionBatchInput::new(directory_name, answers, calls).with_runtime(
                cancellation,
                provider_error,
                fail_persistence,
            ),
        )
    }

    fn run_production_batch_with_policy(input: ProductionBatchInput<'_>) -> BatchOutcome {
        let ProductionBatchInput {
            directory_name,
            answers,
            calls,
            cancellation,
            provider_error,
            fail_persistence,
            policy,
            bypass,
            dangerous_override,
        } = input;
        let directory =
            std::env::temp_dir().join(format!("agens-{directory_name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let executions = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
        let mut dispatcher_guard = dispatcher.lock().expect("dispatcher should be available");
        for name in [
            "native::read",
            "native::list",
            "native::glob",
            "native::grep",
            "native::webfetch",
            "native::write",
        ] {
            dispatcher_guard
                .register_native(
                    name,
                    if name == "native::write" {
                        agens_core::ToolAccess::Write
                    } else {
                        agens_core::ToolAccess::ReadOnly
                    },
                    BatchTool {
                        name: name.into(),
                        calls: Arc::clone(&executions),
                        cancellation: cancellation.clone(),
                    },
                )
                .expect("batch tool should register");
        }
        drop(dispatcher_guard);
        let grants = Arc::new(Mutex::new(Vec::new()));
        let allowed = Arc::new(Mutex::new(BTreeMap::new()));
        let pending_prompts = Arc::new(Mutex::new(BTreeMap::new()));
        let mut gate = ProductionPermissionGate::new(
            policy.clone(),
            Arc::clone(&grants),
            if bypass {
                PermissionSession::with_temporary_bypass()
            } else {
                PermissionSession::new()
            },
            "project".into(),
            Arc::clone(&dispatcher),
            Arc::clone(&allowed),
            Arc::clone(&pending_prompts),
        )
        .with_dangerous_override(dangerous_override);
        let mut resolver = ProductionPermissionResolver::new(
            RecordingPrompt {
                answers,
                calls: Arc::clone(&prompts),
            },
            PermissionGrantStore::open(&directory).expect("grant store should open"),
            grants,
            pending_prompts,
            ProductionPromptAuthorization {
                policy,
                session: PermissionSession::new(),
                project: "project".into(),
                dispatcher: Arc::clone(&dispatcher),
                allowed: Arc::clone(&allowed),
            },
        );
        let mut tool_dispatcher = ProductionToolDispatcher::new(dispatcher, allowed);
        let mut provider = BatchProvider {
            iterations: provider_error
                .map(Err)
                .into_iter()
                .chain(std::iter::once(Ok(calls)))
                .chain(std::iter::once(Ok(vec![MessagePart::Text(
                    "complete".into(),
                )])))
                .collect(),
        };
        let progress_events = Arc::new(Mutex::new(Vec::new()));
        let (metrics_sender, metrics_receiver) = BridgeTx::bounded(16);
        let metrics = Arc::new(Mutex::new(TuiMetricsPublisher::new(
            metrics_sender,
            BridgeCancel::new(),
            "test-model",
        )));
        let progress: TurnProgressSink = {
            let progress_events = Arc::clone(&progress_events);
            let metrics = Arc::clone(&metrics);
            Arc::new(move |event| {
                metrics.lock().unwrap().observe(&event);
                progress_events.lock().unwrap().push(event);
            })
        };
        let cancellation = cancellation.unwrap_or_default();
        let result = run_ready(agens_core::run_headless_turn_with_progress(
            &mut provider,
            &mut gate,
            &mut resolver,
            &mut tool_dispatcher,
            &mut BatchRepository { fail_persistence },
            &cancellation,
            Some(&progress),
        ));
        let terminal = result
            .as_ref()
            .map(|_| ())
            .map_err(|error| CliError::runtime(*error));
        finish_tui_metrics(&metrics, &terminal);
        std::fs::remove_dir_all(&directory).expect("temporary grant directory should be removed");

        BatchOutcome {
            result,
            prompts: prompts.lock().unwrap().clone(),
            executions: executions.lock().unwrap().clone(),
            progress: progress_events.lock().unwrap().clone(),
            metrics: std::iter::from_fn(|| metrics_receiver.try_recv().ok())
                .map(|envelope| envelope.into_parts().1)
                .collect(),
        }
    }

    #[test]
    fn production_allow_always_remembers_a_matching_call_within_one_batch() {
        let outcome = run_production_batch(
            "batch-allow-always",
            vec![PermissionPromptAnswer::AllowAlways],
            vec![
                batch_call("first", "notes.md"),
                batch_call("later", "notes.md"),
            ],
            None,
            None,
            false,
        );

        assert!(outcome.result.is_ok());
        assert_eq!(outcome.prompts, ["notes.md"]);
        assert_eq!(outcome.executions, ["notes.md", "notes.md"]);
    }

    #[test]
    fn production_deny_always_denies_later_matching_calls_without_execution() {
        let outcome = run_production_batch(
            "batch-deny-always",
            vec![PermissionPromptAnswer::DenyAlways],
            vec![
                batch_call("first", "notes.md"),
                batch_call("later", "notes.md"),
            ],
            None,
            None,
            false,
        );

        let snapshot = outcome
            .result
            .expect("denied calls should let the turn complete");
        assert_eq!(outcome.prompts, ["notes.md"]);
        assert!(outcome.executions.is_empty());
        assert_eq!(
            snapshot
                .events()
                .iter()
                .filter_map(|event| match event {
                    TurnEvent::ToolResult(MessagePart::ToolResult {
                        tool_call_id,
                        is_error,
                        ..
                    }) => {
                        Some((tool_call_id.as_str(), *is_error))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [("first", true), ("later", true)]
        );
    }

    #[test]
    fn grouped_native_permission_regressions_preserve_native_target_boundaries() {
        let ask_every_native_tool = || {
            PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    PermissionDecision::Ask,
                    PermissionPattern::glob("native::*").expect("native glob should be valid"),
                    PermissionPattern::Any,
                )],
            )
        };
        let valid_calls = || {
            vec![
                native_batch_call("list", "native::list", serde_json::json!({"path":"src"})),
                native_batch_call(
                    "glob",
                    "native::glob",
                    serde_json::json!({"pattern":"src/**/*.rs"}),
                ),
                native_batch_call(
                    "grep",
                    "native::grep",
                    serde_json::json!({"pattern":"Permission", "path":"src"}),
                ),
                native_batch_call(
                    "webfetch",
                    "native::webfetch",
                    serde_json::json!({"url":"https://example.test/docs"}),
                ),
            ]
        };

        let allowed = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "grouped-native-allow-always",
                vec![
                    PermissionPromptAnswer::AllowAlways,
                    PermissionPromptAnswer::AllowAlways,
                    PermissionPromptAnswer::AllowAlways,
                    PermissionPromptAnswer::AllowAlways,
                ],
                valid_calls(),
            )
            .with_policy(ask_every_native_tool()),
        );
        assert!(allowed.result.is_ok());
        assert_eq!(
            allowed.prompts,
            [
                "src",
                "src/**/*.rs",
                "Permission",
                "https://example.test/docs"
            ]
        );
        assert_eq!(
            allowed.executions,
            [
                "src",
                "src/**/*.rs",
                "Permission",
                "https://example.test/docs"
            ]
        );

        let partial = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "grouped-native-partial-grant",
                vec![
                    PermissionPromptAnswer::AllowAlways,
                    PermissionPromptAnswer::DenyOnce,
                ],
                vec![
                    native_batch_call(
                        "granted",
                        "native::glob",
                        serde_json::json!({"pattern":"src/**/*.rs"}),
                    ),
                    native_batch_call(
                        "sibling",
                        "native::glob",
                        serde_json::json!({"pattern":"tests/**/*.rs"}),
                    ),
                ],
            )
            .with_policy(ask_every_native_tool()),
        );
        assert!(partial.result.is_ok());
        assert_eq!(partial.prompts, ["src/**/*.rs", "tests/**/*.rs"]);
        assert_eq!(partial.executions, ["src/**/*.rs"]);

        let ask = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "grouped-native-ask",
                vec![PermissionPromptAnswer::Cancel],
                vec![native_batch_call(
                    "ask",
                    "native::grep",
                    serde_json::json!({"pattern":"TODO", "path":"src"}),
                )],
            )
            .with_policy(ask_every_native_tool()),
        );
        assert_eq!(ask.result, Err(HeadlessTurnError::Cancelled));
        assert_eq!(ask.prompts, ["TODO"]);
        assert!(ask.executions.is_empty());

        let deny_policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::Exact("native::webfetch".into()),
                PermissionPattern::Any,
            )],
        );
        let denied = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "grouped-native-deny-bypass",
                vec![PermissionPromptAnswer::AllowAlways],
                vec![native_batch_call(
                    "denied",
                    "native::webfetch",
                    serde_json::json!({"url":"https://example.test/blocked"}),
                )],
            )
            .with_policy(deny_policy)
            .with_bypass(),
        );
        assert!(denied.result.is_ok());
        assert!(denied.prompts.is_empty());
        assert!(denied.executions.is_empty());

        for (name, input) in [
            ("native::list", "{malformed"),
            ("native::glob", r#"{}"#),
            ("native::unknown", r#"{"path":"src"}"#),
            (
                "native::grep",
                r#"{"pattern":"TODO","_inject_permission_evaluator_failure":true}"#,
            ),
        ] {
            let invalid = run_production_batch_with_policy(
                ProductionBatchInput::new(
                    "grouped-native-invalid",
                    Vec::new(),
                    vec![MessagePart::ToolCall {
                        id: "invalid".into(),
                        name: name.into(),
                        input: input.into(),
                    }],
                )
                .with_policy(ask_every_native_tool())
                .with_bypass(),
            );
            assert_eq!(invalid.result, Err(HeadlessTurnError::PermissionEvaluation));
            assert!(invalid.prompts.is_empty());
            assert!(invalid.executions.is_empty());
        }

        production_prompt_decisions_authorize_only_allowed_calls();
    }

    #[test]
    fn permission_error_mapping_is_sanitized_and_fails_closed() {
        let secret_input = r#"{"command":"SENTINEL_COMMAND","token":"SENTINEL_TOKEN"}"#;
        for (name, input) in [
            ("native::read", "{malformed"),
            ("native::read", secret_input),
            ("native::unknown", r#"{"path":"SENTINEL_PATH"}"#),
        ] {
            let outcome = run_production_batch(
                "permission-evaluation-invalid",
                Vec::new(),
                vec![MessagePart::ToolCall {
                    id: "invalid".into(),
                    name: name.into(),
                    input: input.into(),
                }],
                None,
                None,
                false,
            );

            assert_eq!(outcome.result, Err(HeadlessTurnError::PermissionEvaluation));
            assert!(outcome.executions.is_empty());
        }

        for (turn_error, expected) in [
            (
                HeadlessTurnError::Permission,
                "permission: permission evaluation failed",
            ),
            (
                HeadlessTurnError::PermissionRequired,
                "permission: permission approval is required",
            ),
            (
                HeadlessTurnError::PermissionEvaluation,
                "permission: permission target could not be evaluated; correct the tool arguments and retry",
            ),
        ] {
            let error = CliError::runtime(turn_error);
            assert_eq!(error.category, "permission");
            assert_eq!(error.to_string(), expected);
            assert!(!error.to_string().contains("SENTINEL_COMMAND"));
            assert!(!error.to_string().contains("SENTINEL_TOKEN"));

            assert!(matches!(
                tui_provider_outcome(Err(error)),
                TuiProviderOutcome::Failed { message, action }
                    if message == expected && action == TUI_ERROR_ACTION
            ));
        }
    }

    #[test]
    fn provider_context_and_network_render_sanitized_actions() {
        for (turn_error, expected_message, expected_action) in [
            (
                HeadlessTurnError::ProviderContext,
                "provider: request exceeds the model context window",
                "Start a new session or shorten the prompt, then retry.",
            ),
            (
                HeadlessTurnError::ProviderNetwork,
                "provider: network request failed",
                "Check the network connection, then retry.",
            ),
        ] {
            let error = CliError::runtime(turn_error);

            assert!(matches!(
                tui_provider_outcome(Err(error)),
                TuiProviderOutcome::Failed { message, action }
                    if message == expected_message && action == expected_action
            ));
        }
    }

    #[test]
    fn production_batch_prompts_each_distinct_ask_individually() {
        let outcome = run_production_batch(
            "batch-distinct-prompts",
            vec![
                PermissionPromptAnswer::AllowOnce,
                PermissionPromptAnswer::DenyOnce,
            ],
            vec![
                batch_call("first", "first.md"),
                batch_call("second", "second.md"),
            ],
            None,
            None,
            false,
        );

        assert!(outcome.result.is_ok());
        assert_eq!(outcome.prompts, ["first.md", "second.md"]);
        assert_eq!(outcome.executions, ["first.md"]);
    }

    #[test]
    fn production_batch_progress_has_boundaries_and_cancellation_never_completes() {
        let cancellation = HeadlessTurnCancellation::new();
        let outcome = run_production_batch(
            "batch-cancellation-progress",
            vec![
                PermissionPromptAnswer::AllowOnce,
                PermissionPromptAnswer::AllowOnce,
            ],
            vec![
                batch_call("first", "first.md"),
                batch_call("second", "second.md"),
            ],
            Some(cancellation),
            None,
            false,
        );

        assert_eq!(outcome.result, Err(HeadlessTurnError::Cancelled));
        assert_eq!(outcome.executions, ["first.md"]);
        assert_eq!(
            outcome.progress,
            vec![
                TurnEvent::StateChanged(TurnState::Requesting),
                TurnEvent::StateChanged(TurnState::Streaming),
                TurnEvent::ProviderPart(batch_call("first", "first.md")),
                TurnEvent::ProviderPart(batch_call("second", "second.md")),
                TurnEvent::StateChanged(TurnState::Dispatching),
                TurnEvent::ToolCallRequested {
                    id: "first".into(),
                    name: "native::read".into(),
                    input: r#"{"path":"first.md"}"#.into(),
                },
                TurnEvent::ToolCallRequested {
                    id: "second".into(),
                    name: "native::read".into(),
                    input: r#"{"path":"second.md"}"#.into(),
                },
                TurnEvent::ToolResult(MessagePart::ToolResult {
                    tool_call_id: "first".into(),
                    content: "tool execution failed".into(),
                    is_error: true,
                }),
                TurnEvent::StateChanged(TurnState::Cancelled),
            ]
        );
    }

    #[test]
    fn production_dispatcher_preserves_safe_native_failure_reason() {
        let outcome = run_production_batch(
            "safe-native-failure",
            vec![PermissionPromptAnswer::AllowOnce],
            vec![MessagePart::ToolCall {
                id: "glob".into(),
                name: "native::glob".into(),
                input: serde_json::json!({
                    "pattern": "**/*.md",
                    "_inject_tool_failure": "glob: entry limit of 10000 exceeded",
                })
                .to_string(),
            }],
            None,
            None,
            false,
        );

        assert!(outcome.result.is_ok());
        assert!(outcome.progress.iter().any(|event| matches!(
            event,
            TurnEvent::ToolResult(MessagePart::ToolResult {
                content,
                is_error: true,
                ..
            }) if content == "glob: entry limit of 10000 exceeded"
        )));
        assert_eq!(
            sanitized_native_tool_failure(
                "glob: /home/user/private token=SECRET remote body details"
            ),
            "tool execution failed"
        );
        assert_eq!(
            sanitized_native_tool_failure("glob: path is outside project root"),
            "glob: path validation failed"
        );
    }

    #[test]
    fn tui_metrics_publish_one_terminal_after_the_production_turn_outcome() {
        let success = run_production_batch(
            "metrics-success",
            Vec::new(),
            vec![MessagePart::Text("complete".into())],
            None,
            None,
            false,
        );
        let cancellation = run_production_batch(
            "metrics-cancelled",
            vec![PermissionPromptAnswer::AllowOnce],
            vec![batch_call("first", "notes.md")],
            Some(HeadlessTurnCancellation::new()),
            None,
            false,
        );
        let provider_failure = run_production_batch(
            "metrics-provider-failure",
            Vec::new(),
            Vec::new(),
            None,
            Some(HeadlessTurnPortError::Provider),
            false,
        );
        let persistence_failure = run_production_batch(
            "metrics-persistence-failure",
            Vec::new(),
            vec![MessagePart::Text("complete".into())],
            None,
            None,
            true,
        );

        assert!(success.result.is_ok());
        assert!(matches!(
            success.metrics.as_slice(),
            [
                TuiRuntimeEvent::TurnStarted,
                TuiRuntimeEvent::TurnEnded {
                    status: TurnState::Completed,
                    duration: Some(_)
                },
            ]
        ));

        assert_eq!(cancellation.result, Err(HeadlessTurnError::Cancelled));
        assert!(matches!(
            cancellation.metrics.as_slice(),
            [
                TuiRuntimeEvent::TurnStarted,
                TuiRuntimeEvent::ToolStarted { call_id, .. },
                TuiRuntimeEvent::ToolEnded { call_id: ended_call_id, .. },
                TuiRuntimeEvent::TurnEnded { status: TurnState::Cancelled, duration: Some(_) },
            ] if call_id == "first" && ended_call_id == "first"
        ));

        assert_eq!(provider_failure.result, Err(HeadlessTurnError::Provider));
        assert!(matches!(
            provider_failure.metrics.as_slice(),
            [
                TuiRuntimeEvent::TurnStarted,
                TuiRuntimeEvent::TurnEnded {
                    status: TurnState::Failed,
                    duration: Some(_)
                },
            ]
        ));

        assert_eq!(persistence_failure.result, Err(HeadlessTurnError::Store));
        assert!(
            persistence_failure
                .progress
                .contains(&TurnEvent::StateChanged(TurnState::Completed))
        );
        assert!(matches!(
            persistence_failure.metrics.as_slice(),
            [
                TuiRuntimeEvent::TurnStarted,
                TuiRuntimeEvent::TurnEnded {
                    status: TurnState::Failed,
                    duration: Some(_)
                },
            ]
        ));
    }

    #[test]
    fn tui_metrics_production_publication_preserves_usage_tools_and_diffs_in_source_order() {
        let (bridge, receiver) = agens_tui::BridgeTx::bounded(16);
        let cancellation = agens_tui::BridgeCancel::new();
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation, "unknown-model");

        for event in [
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::Usage(agens_core::Usage {
                input_tokens: Some(11),
                output_tokens: None,
                total_tokens: Some(17),
                context_window: None,
            }),
            TurnEvent::ToolCallRequested {
                id: "edit-1".into(),
                name: "native::edit".into(),
                input: r#"{"path":"notes.md","token":"SENTINEL"}"#.into(),
            },
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "edit-1".into(),
                content: "--- notes.md\n+++ notes.md\n@@ -1,1 +1,1 @@\n-old\n+new\n".into(),
                is_error: false,
            }),
        ] {
            publisher.observe(&event);
        }

        publisher.finish(Ok(()));

        let events = (0..6)
            .map(|_| {
                receiver
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .unwrap()
                    .into_parts()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            events
                .iter()
                .map(|(ordinal, _)| *ordinal)
                .collect::<Vec<_>>(),
            (0..6).collect::<Vec<_>>()
        );
        assert!(matches!(
            events.as_slice(),
            [
                (_, agens_tui::TuiRuntimeEvent::TurnStarted),
                (_, agens_tui::TuiRuntimeEvent::Usage(agens_core::Usage {
                    input_tokens: Some(11), output_tokens: None, total_tokens: Some(17), context_window: None,
                })),
                (_, agens_tui::TuiRuntimeEvent::ToolStarted { call_id, name, input, .. }),
                _, _, _,
            ] if call_id == "edit-1" && name == "native::edit" && input == "[redacted]"
        ));
        assert!(matches!(
            &events[3].1,
            agens_tui::TuiRuntimeEvent::ToolEnded {
                call_id,
                duration: Some(_),
                result: agens_tui::ToolResultState::Success,
            } if call_id == "edit-1"
        ));
        assert!(matches!(
            &events[4].1,
            agens_tui::TuiRuntimeEvent::Diff { call_id, lines }
                if call_id == "edit-1" && lines == &vec![
                    agens_tui::DiffLine::new(1, agens_tui::DiffLineKind::Removed, "old"),
                    agens_tui::DiffLine::new(1, agens_tui::DiffLineKind::Added, "new"),
                ]
        ));
        assert!(matches!(
            &events[5].1,
            agens_tui::TuiRuntimeEvent::TurnEnded {
                status: TurnState::Completed,
                duration: Some(_),
            }
        ));
    }

    #[test]
    fn tui_metrics_production_publication_keeps_missing_timing_and_failed_tool_state() {
        let (bridge, receiver) = agens_tui::BridgeTx::bounded(4);
        let cancellation = agens_tui::BridgeCancel::new();
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation, "unknown-model");

        publisher.observe(&TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "unknown".into(),
            content: "failed".into(),
            is_error: true,
        }));
        publisher.finish(Err(&CliError::runtime(HeadlessTurnError::Provider)));

        let events = (0..2)
            .map(|_| {
                receiver
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .unwrap()
                    .into_parts()
                    .1
            })
            .collect::<Vec<_>>();

        assert!(matches!(
            events.as_slice(),
            [
                agens_tui::TuiRuntimeEvent::ToolEnded {
                    call_id, duration: None, result: agens_tui::ToolResultState::Failure,
                },
                agens_tui::TuiRuntimeEvent::TurnEnded { status: TurnState::Failed, duration: None },
            ] if call_id == "unknown"
        ));

        publisher.observe(&TurnEvent::ToolCallRequested {
            id: "write-1".into(),
            name: "native::write".into(),
            input: r#"{"path":"notes.md"}"#.into(),
        });
        publisher.observe(&TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "write-1".into(),
            content: "--- notes.md\n+++ notes.md\n@@ -1,1 +1,1 @@\n-old\n+new\n".into(),
            is_error: false,
        }));

        let events = (0..2)
            .map(|_| {
                receiver
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .unwrap()
                    .into_parts()
                    .1
            })
            .collect::<Vec<_>>();

        assert!(matches!(
            events[0],
            agens_tui::TuiRuntimeEvent::ToolStarted { ref name, .. } if name == "native::write"
        ));
        assert!(matches!(
            events[1],
            agens_tui::TuiRuntimeEvent::ToolEnded {
                result: agens_tui::ToolResultState::Success,
                ..
            }
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn tui_metrics_publisher_enriches_context_window_from_registry_for_known_model() {
        let (bridge, receiver) = agens_tui::BridgeTx::bounded(4);
        let cancellation = agens_tui::BridgeCancel::new();
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation, "gpt-4.1");

        publisher.observe(&TurnEvent::Usage(agens_core::Usage {
            input_tokens: Some(11),
            output_tokens: None,
            total_tokens: Some(17),
            context_window: None,
        }));

        let event = receiver
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap()
            .into_parts()
            .1;

        assert!(matches!(
            event,
            agens_tui::TuiRuntimeEvent::Usage(agens_core::Usage {
                input_tokens: Some(11),
                output_tokens: None,
                total_tokens: Some(17),
                context_window: Some(1_047_576),
            })
        ));
    }

    #[test]
    fn tui_metrics_publisher_leaves_context_window_none_for_unknown_model() {
        let (bridge, receiver) = agens_tui::BridgeTx::bounded(4);
        let cancellation = agens_tui::BridgeCancel::new();
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation, "not-a-real-model-xyz");

        publisher.observe(&TurnEvent::Usage(agens_core::Usage {
            input_tokens: Some(3),
            output_tokens: Some(5),
            total_tokens: Some(8),
            context_window: None,
        }));

        let event = receiver
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap()
            .into_parts()
            .1;

        assert!(matches!(
            event,
            agens_tui::TuiRuntimeEvent::Usage(agens_core::Usage {
                total_tokens: Some(8),
                context_window: None,
                ..
            })
        ));
    }

    #[test]
    fn tui_metrics_publisher_preserves_provider_context_window_when_present() {
        let (bridge, receiver) = agens_tui::BridgeTx::bounded(4);
        let cancellation = agens_tui::BridgeCancel::new();
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation, "gpt-4.1");

        publisher.observe(&TurnEvent::Usage(agens_core::Usage {
            input_tokens: Some(1),
            output_tokens: Some(2),
            total_tokens: Some(3),
            context_window: Some(42),
        }));

        let event = receiver
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap()
            .into_parts()
            .1;

        assert!(matches!(
            event,
            agens_tui::TuiRuntimeEvent::Usage(agens_core::Usage {
                context_window: Some(42),
                ..
            })
        ));
    }

    #[test]
    fn u15_authorization_model_and_tui_launch_share_one_native_task_path() {
        struct RecordingTaskTool(Arc<std::sync::atomic::AtomicUsize>);

        impl DispatchTool for RecordingTaskTool {
            fn permission_target(
                &self,
                arguments: &serde_json::Value,
            ) -> Result<String, agens_core::Error> {
                arguments
                    .get("agent")
                    .and_then(serde_json::Value::as_str)
                    .filter(|agent| !agent.is_empty())
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| agens_core::Error::Tool("missing agent".into()))
            }

            fn execute(
                &mut self,
                _: &ToolExecutionContext,
                _: serde_json::Value,
            ) -> Result<ToolOutput, agens_core::Error> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ToolOutput::success("executed"))
            }
        }

        fn authorized_native_task_runtime<P: PermissionPrompter>(
            directory: &Path,
            policy: PermissionPolicy,
            dispatcher: SharedToolDispatcher,
            prompt: P,
        ) -> AuthorizedNativeTaskRuntime<P> {
            let grants = Arc::new(Mutex::new(Vec::new()));
            let allowed = Arc::new(Mutex::new(BTreeMap::new()));
            let prompts = Arc::new(Mutex::new(BTreeMap::new()));
            let gate = ProductionPermissionGate::new(
                policy.clone(),
                Arc::clone(&grants),
                PermissionSession::new(),
                "project".into(),
                Arc::clone(&dispatcher),
                Arc::clone(&allowed),
                Arc::clone(&prompts),
            );
            let resolver = ProductionPermissionResolver::new(
                prompt,
                PermissionGrantStore::open(directory).unwrap(),
                grants,
                prompts,
                ProductionPromptAuthorization {
                    policy,
                    session: PermissionSession::new(),
                    project: "project".into(),
                    dispatcher: Arc::clone(&dispatcher),
                    allowed: Arc::clone(&allowed),
                },
            );

            AuthorizedNativeTaskRuntime {
                gate,
                resolver,
                dispatcher: ProductionToolDispatcher::new(dispatcher, allowed),
                next_call_id: 0,
            }
        }

        fn launch_request<'a>(
            agent: &'a str,
            description: &'a str,
            background: bool,
        ) -> TaskLaunchRequest<'a> {
            TaskLaunchRequest {
                agent,
                description,
                background,
            }
        }

        let directory =
            std::env::temp_dir().join(format!("agens-u15-authorization-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
        dispatcher
            .lock()
            .unwrap()
            .register_native(
                "native::task",
                agens_core::ToolAccess::Write,
                RecordingTaskTool(Arc::clone(&executions)),
            )
            .unwrap();

        let ask_policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Ask,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let mut model = authorized_native_task_runtime(
            &directory,
            PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    PermissionDecision::Allow,
                    PermissionPattern::Exact("native::task".into()),
                    PermissionPattern::Any,
                )],
            ),
            Arc::clone(&dispatcher),
            RecordingPrompt {
                answers: vec![PermissionPromptAnswer::AllowOnce],
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );
        let mut tui = authorized_native_task_runtime(
            &directory,
            ask_policy,
            Arc::clone(&dispatcher),
            RecordingPrompt {
                answers: vec![PermissionPromptAnswer::AllowOnce],
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );
        let cancellation = HeadlessTurnCancellation::new();

        assert_eq!(
            model.launch(
                launch_request("reviewer", "model task", false),
                &cancellation
            ),
            Ok(TaskLaunchOutcome::Dispatched(HeadlessToolOutput::success(
                "executed"
            )))
        );
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            tui.launch(launch_request("reviewer", "TUI task", true), &cancellation),
            Ok(TaskLaunchOutcome::Dispatched(HeadlessToolOutput::success(
                "executed"
            )))
        );
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 2);

        let mut denied = authorized_native_task_runtime(
            &directory,
            PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    PermissionDecision::Deny,
                    PermissionPattern::Exact("native::task".into()),
                    PermissionPattern::Any,
                )],
            ),
            Arc::clone(&dispatcher),
            RecordingPrompt {
                answers: Vec::new(),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );
        assert_eq!(
            denied.launch(launch_request("reviewer", "denied", false), &cancellation),
            Ok(TaskLaunchOutcome::Denied)
        );
        assert_eq!(
            tui.launch(launch_request("", "invalid", false), &cancellation),
            Ok(TaskLaunchOutcome::RejectedEmptyInput)
        );
        assert_eq!(
            tui.launch(launch_request("reviewer", "", false), &cancellation),
            Ok(TaskLaunchOutcome::RejectedEmptyInput)
        );
        cancellation.cancel();
        assert_eq!(
            tui.launch(
                launch_request("reviewer", "cancelled", false),
                &cancellation
            ),
            Ok(TaskLaunchOutcome::RejectedCancelled)
        );
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 2);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn production_task_catalog_includes_built_ins_and_dispatches_requested_call() {
        struct RecordingTaskRunner(Arc<Mutex<Vec<(String, TaskLaunchMode)>>>);

        impl TaskRunner for RecordingTaskRunner {
            fn run(
                &self,
                request: TaskTurnRequest,
                context: &TaskRunContext,
            ) -> Result<TaskTurnResult, TaskRunnerError> {
                self.0.lock().unwrap().push((
                    request.agent_name().to_owned(),
                    context.execution().unwrap().mode(),
                ));
                Ok(TaskTurnResult {
                    output: request.description().to_owned(),
                    iterations: 1,
                })
            }
        }

        let temporary = tui_session_directory("conditional-task-catalog");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[
                (
                    "alpha",
                    "---\nname: alpha\ndescription: default\nmode: subagent\npermissions: []\n---\nDefault work.\n",
                ),
                (
                    "reviewer",
                    "---\nname: reviewer\ndescription: review\nmode: subagent\npermissions: []\n---\nReview work.\n",
                ),
            ],
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (provider_tools, dispatcher) = production_tool_runtime_with_task_runner(
            &bootstrap,
            bootstrap.project_root().unwrap(),
            Some(&SkillCatalog::default()),
            RecordingTaskRunner(Arc::clone(&calls)),
        )
        .unwrap();
        let task = provider_tools
            .iter()
            .find(|tool| tool.name() == "task")
            .expect("eligible catalog should expose task");
        assert_eq!(
            task.description(),
            "Dispatch an isolated eligible subagent task in the foreground or background"
        );
        assert_eq!(
            task.parameters()["properties"]["agent"]["enum"],
            serde_json::json!(["alpha", "explore", "general", "reviewer"])
        );
        assert_eq!(
            task.parameters()["properties"]["model"]["enum"],
            serde_json::json!(task_model_catalog(&bootstrap).unwrap())
        );
        let task_schema = task.parameters().to_string();
        assert!(task_schema.contains("Explore the codebase without modifying files"));
        assert!(task_schema.contains("Handle a general delegated coding task"));
        assert!(!task_schema.contains("You are the read-only exploration subagent"));

        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let cancellation = HeadlessTurnCancellation::new();
        let context = ToolExecutionContext::from_headless_adapter(cancellation.adapter_view());
        let mut dispatcher = dispatcher.lock().unwrap();
        for arguments in [
            serde_json::json!({"agent":"reviewer","background":true,"description":"selected"}),
            serde_json::json!({"description":"default"}),
        ] {
            let ToolEvaluationOutcome::Authorized(handle) = dispatcher
                .evaluate(
                    &policy,
                    &[],
                    &PermissionSession::new(),
                    ToolDispatchRequest::new("project", "task", arguments),
                )
                .unwrap()
            else {
                panic!("provider task call should authorize");
            };
            dispatcher.execute(handle, &context).unwrap();
        }
        drop(dispatcher);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                ("reviewer".to_owned(), TaskLaunchMode::Background),
                ("alpha".to_owned(), TaskLaunchMode::Foreground),
            ]
        );

        for provider in ["openai-api", "openai-chatgpt"] {
            let provider_temporary = tui_session_directory(provider);
            let bootstrap =
                tui_session_bootstrap_for_provider(&provider_temporary, &[], provider, "gpt-4.1");
            let (provider_tools, dispatcher) = production_tool_runtime_with_task_runner(
                &bootstrap,
                bootstrap.project_root().unwrap(),
                Some(&SkillCatalog::default()),
                RecordingTaskRunner(Arc::new(Mutex::new(Vec::new()))),
            )
            .unwrap();

            let task = provider_tools
                .iter()
                .find(|tool| tool.name() == "task")
                .expect("built-in subagents should expose task for both providers");
            assert_eq!(
                task.parameters()["properties"]["agent"]["enum"],
                serde_json::json!(["explore", "general"])
            );
            assert_eq!(
                task.parameters()["properties"]["model"]["enum"],
                serde_json::json!(task_model_catalog(&bootstrap).unwrap())
            );
            assert!(
                task.parameters()["properties"]["model"]["enum"]
                    .as_array()
                    .is_some_and(|models| !models.is_empty() && models.len() <= 256)
            );
            assert!(
                dispatcher
                    .lock()
                    .unwrap()
                    .canonical_identity("native::task")
                    .is_some()
            );

            std::fs::remove_dir_all(provider_temporary).unwrap();
        }

        let override_temporary = tui_session_directory("overridden-built-ins");
        let bootstrap = tui_session_bootstrap(
            &override_temporary,
            &[
                (
                    "explore",
                    "---\nname: explore\ndescription: primary override\nmode: primary\npermissions: []\n---\nPrimary work.\n",
                ),
                (
                    "general",
                    "---\nname: general\ndescription: all override\nmode: all\npermissions: []\n---\nAll work.\n",
                ),
            ],
        );
        let (provider_tools, dispatcher) = production_tool_runtime_with_task_runner(
            &bootstrap,
            bootstrap.project_root().unwrap(),
            Some(&SkillCatalog::default()),
            RecordingTaskRunner(Arc::new(Mutex::new(Vec::new()))),
        )
        .unwrap();

        assert!(provider_tools.iter().all(|tool| tool.name() != "task"));
        let dispatcher = dispatcher.lock().unwrap();
        assert_eq!(dispatcher.canonical_identity("task"), None);
        assert_eq!(dispatcher.canonical_identity("native::task"), None);
        drop(dispatcher);
        std::fs::remove_dir_all(override_temporary).unwrap();

        std::fs::remove_dir_all(temporary).unwrap();
    }

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
    fn u15_a1b1_production_task_runtime_assembles_current_turn_registration() {
        let temporary = tui_session_directory("production-task-runtime");
        let mut bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        bootstrap.model = Some("gpt-5.6-sol".into());
        let probe = Arc::new(Mutex::new(Vec::new()));
        let runtime = production_tui_task_runtime_with_runner_and_parent_config(
            &bootstrap,
            &SkillCatalog::default(),
            production_tui_permission_bridge().0,
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                bootstrap.project_root().unwrap().to_path_buf(),
                Arc::clone(&probe),
            ),
            agens_core::RequestConfig::with_reasoning_effort("high").unwrap(),
            None,
        )
        .unwrap();

        assert!(
            runtime
                .provider_tools
                .iter()
                .any(|tool| tool.name() == "task")
        );
        let mut dispatcher = runtime.dispatcher.lock().unwrap();
        assert_eq!(
            dispatcher.canonical_identity("task"),
            dispatcher.canonical_identity("native::task")
        );
        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let ToolEvaluationOutcome::Authorized(handle) = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new(
                    "project",
                    "native::task",
                    serde_json::json!({"agent":"reviewer","description":"probe"}),
                ),
            )
            .unwrap()
        else {
            panic!("registered task should authorize");
        };
        let cancellation = HeadlessTurnCancellation::new();
        let output = dispatcher
            .execute(
                handle,
                &ToolExecutionContext::from_headless_adapter(cancellation.adapter_view()),
            )
            .unwrap();
        assert_eq!(output.content, "probe");
        let probe = probe.lock().unwrap();
        assert_eq!(probe.len(), 1);
        assert_eq!(probe[0].1, TaskLaunchMode::Foreground);
        assert_eq!(probe[0].2, "gpt-5.6-sol");
        assert_eq!(probe[0].3, Some(agens_core::ReasoningEffort::High));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn u15_a1b2_selected_launch_uses_the_registered_production_task_runner() {
        let temporary = tui_session_directory("selected-task-launch");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (bridge, requests) = production_tui_permission_bridge();
        let reply_bridge = bridge.clone();
        let reply = std::thread::spawn(move || {
            let request = requests
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("selected task should request permission");
            reply_bridge.reply(request.id(), TuiPermissionReply::AllowOnce)
        });
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &SkillCatalog::default(),
            bridge,
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                bootstrap.project_root().unwrap().to_path_buf(),
                Arc::clone(&probe),
            ),
        )
        .unwrap();
        let session = Arc::new(Mutex::new(TuiSessionContext {
            selected_subagent: Some("reviewer".into()),
            ..TuiSessionContext::fresh()
        }));
        let cancellation = HeadlessTurnCancellation::new();
        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let mut dispatcher = runtime.dispatcher.lock().unwrap();
        let ToolEvaluationOutcome::Authorized(handle) = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new(
                    "project",
                    "native::task",
                    serde_json::json!({
                        "agent": "reviewer",
                        "description": "model task",
                        "background": true,
                    }),
                ),
            )
            .unwrap()
        else {
            panic!("registered model task should authorize");
        };
        assert_eq!(
            dispatcher
                .execute(
                    handle,
                    &ToolExecutionContext::from_headless_adapter(cancellation.adapter_view()),
                )
                .unwrap()
                .content,
            "Subagent #1 running in background"
        );
        drop(dispatcher);

        assert_eq!(
            launch_selected_tui_task(&mut runtime, &session, "review task", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Dispatched)
        );
        let probe = probe.lock().unwrap();
        assert_eq!(probe.len(), 2);
        assert_eq!(probe[0].1, TaskLaunchMode::Background);
        assert_eq!(probe[1].1, TaskLaunchMode::Foreground);
        assert_ne!(probe[0].0, probe[1].0);
        assert!(reply.join().unwrap());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_runtime_scheduled_turn_never_consumes_the_armed_subagent() {
        let temporary = tui_session_directory("auto-turn-armed-subagent");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        assert_eq!(
            select_tui_subagent(&bootstrap, "reviewer", &session),
            Ok("Subagent: reviewer.".to_owned())
        );

        assert!(origin_launches_selected_subagent(TuiSubmitOrigin::User));
        assert!(origin_launches_selected_subagent(
            TuiSubmitOrigin::Background
        ));
        assert!(!origin_launches_selected_subagent(
            TuiSubmitOrigin::SubagentCompletion
        ));
        assert_eq!(
            session.lock().unwrap().selected_subagent.as_deref(),
            Some("reviewer")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn u15_c1c_backgrounded_success_skips_the_parent_provider_and_history_path() {
        let temporary = tui_session_directory("selected-background-launch");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (events, receiver) = BridgeTx::bounded(8);
        let controls = TuiTaskControls::default();
        let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, controls.clone());
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &SkillCatalog::default(),
            production_tui_permission_bridge().0,
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                bootstrap.project_root().unwrap().to_path_buf(),
                Arc::clone(&probe),
            )
            .with_lifecycle_bridge(lifecycle_bridge.clone()),
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
        let session = Arc::new(Mutex::new(TuiSessionContext {
            selected_subagent: Some("reviewer".into()),
            ..TuiSessionContext::fresh()
        }));
        ensure_active_tui_agent_runtime(&bootstrap, &session, &runtime.dispatcher).unwrap();
        let cancellation = HeadlessTurnCancellation::new();
        let parent_runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let next_event = |timeout| receiver.recv_timeout(timeout).unwrap().into_parts().1;
        let worker = std::thread::spawn({
            let session = Arc::clone(&session);
            let cancellation = cancellation.clone();
            let lifecycle_bridge = lifecycle_bridge.clone();
            let parent_runs = Arc::clone(&parent_runs);
            move || {
                let skips_parent = selected_tui_task_skips_parent(
                    launch_selected_tui_task(
                        &mut runtime,
                        &session,
                        "review task",
                        false,
                        &cancellation,
                    ),
                    &lifecycle_bridge,
                )?;
                if skips_parent {
                    Ok(TuiProviderOutcome::Backgrounded)
                } else {
                    parent_runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err(CliError::runtime(HeadlessTurnError::Provider))
                }
            }
        });
        assert_eq!(
            next_event(std::time::Duration::from_secs(1)),
            TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: TuiExecutionEvent::ForegroundStarted { id: 1 },
            }
        );
        assert_eq!(
            next_event(std::time::Duration::from_secs(1)),
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::started(
                1,
                "reviewer",
                "review task",
                agens_tui::TuiExecutionState::ForegroundRunning,
            ))
        );
        assert!(controls.transition_to_background(1));
        assert_eq!(
            next_event(std::time::Duration::from_secs(1)),
            TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: TuiExecutionEvent::Backgrounded { id: 1 },
            }
        );
        assert_eq!(worker.join().unwrap(), Ok(TuiProviderOutcome::Backgrounded));
        assert_eq!(
            next_event(std::time::Duration::from_secs(1)),
            TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: TuiExecutionEvent::Completed { id: 1 },
            }
        );
        let probe = probe.lock().unwrap();
        assert_eq!(probe.len(), 1);
        assert_eq!(parent_runs.load(std::sync::atomic::Ordering::SeqCst), 0);
        let session = session.lock().unwrap();
        assert!(session.messages.is_empty());
        assert_eq!(
            session
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );
        std::fs::remove_dir_all(temporary).unwrap();
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
        let session = Arc::new(Mutex::new(TuiSessionContext {
            selected_subagent: Some("reviewer".into()),
            ..TuiSessionContext::fresh()
        }));
        let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, controls.clone())
            .with_session_writer(bootstrap.clone(), Arc::clone(&session));
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &SkillCatalog::default(),
            production_tui_permission_bridge().0,
            ProductionTaskRunner::with_progress_probe(
                bootstrap.clone(),
                bootstrap.project_root().unwrap().to_path_buf(),
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
        (0..100)
            .find_map(|_| {
                let identifier = session.lock().unwrap().identifier;
                if identifier.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                identifier
            })
            .expect("a completed background subagent persists one durable turn");

        let queued = Arc::new(Mutex::new(Vec::new()));
        let mut provider = TaskMailboxProvider::new(
            RecordingMailboxProvider {
                queued: Arc::clone(&queued),
            },
            Some(controls.0.clone()),
            TaskMessageTarget::Main,
        );
        block_on_headless_turn(provider.next_parts(&[], &cancellation))
            .unwrap()
            .unwrap();

        let queued = queued.lock().unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].len(), 1);
        assert_eq!(queued[0][0].role, Role::User);
        let [MessagePart::Text(notice)] = queued[0][0].parts.as_slice() else {
            panic!("a mailbox notice is text: {:?}", queued[0][0].parts)
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

    #[test]
    fn p1c1_p1b_authorized_runner_persists_one_completed_subagent_turn() {
        let temporary = tui_session_directory("p1b-child-events");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (events, receiver) = BridgeTx::bounded(16);
        let controls = TuiTaskControls::default();
        let session = Arc::new(Mutex::new(TuiSessionContext {
            selected_subagent: Some("reviewer".into()),
            ..TuiSessionContext::fresh()
        }));
        let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, controls)
            .with_session_writer(bootstrap.clone(), Arc::clone(&session));
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &SkillCatalog::default(),
            production_tui_permission_bridge().0,
            ProductionTaskRunner::with_progress_probe(
                bootstrap.clone(),
                bootstrap.project_root().unwrap().to_path_buf(),
                Arc::clone(&probe),
                vec![
                    TurnEvent::ProviderPart(MessagePart::Reasoning("inspect".into())),
                    TurnEvent::ProviderPart(MessagePart::Text("partial".into())),
                    TurnEvent::ToolCallRequested {
                        id: "read-1".into(),
                        name: "native::read".into(),
                        input: format!("authorization {}", "x".repeat(300)),
                    },
                    TurnEvent::ToolResult(MessagePart::ToolResult {
                        tool_call_id: "read-1".into(),
                        content: format!("token {}", "y".repeat(300)),
                        is_error: false,
                    }),
                ],
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

        assert_eq!(
            launch_selected_tui_task(&mut runtime, &session, "review task", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Dispatched)
        );

        let mut received = Vec::new();
        for _ in 0..8 {
            match receiver.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(event) => received.push(event.into_parts().1),
                Err(error) => {
                    panic!("child event should reach the TUI bridge: {received:?}: {error}")
                }
            }
        }
        assert_eq!(
            received,
            vec![
                TuiRuntimeEvent::TaskExecution {
                    agent: "reviewer".into(),
                    event: TuiExecutionEvent::ForegroundStarted { id: 1 },
                },
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::started(
                    1,
                    "reviewer",
                    "review task",
                    agens_tui::TuiExecutionState::ForegroundRunning,
                )),
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::reasoning(1, "inspect")),
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::text(1, "partial")),
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::tool_call(
                    1,
                    "read-1",
                    "native::read",
                    "[redacted]",
                )),
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::tool_result(
                    1,
                    "read-1",
                    "[redacted]",
                    false,
                )),
                TuiRuntimeEvent::TaskExecution {
                    agent: "reviewer".into(),
                    event: TuiExecutionEvent::Completed { id: 1 },
                },
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::terminal(
                    1,
                    TuiSubagentStatus::Success,
                    "probe",
                )),
            ]
        );
        assert_eq!(probe.lock().unwrap().len(), 1);
        let session_id = (0..100)
            .find_map(|_| {
                let identifier = session.lock().unwrap().identifier;
                if identifier.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                identifier
            })
            .expect("completed terminal should persist exactly one durable turn");
        let stored = SessionStore::open(bootstrap.data_directory())
            .unwrap()
            .load_session_for_resume(session_id)
            .unwrap();
        assert_eq!(stored.metadata.completed_turn_count, 1);
        assert_eq!(stored.messages.len(), 3);
        assert_eq!(
            stored.messages[2].parts[0],
            MessagePart::ToolResult {
                tool_call_id: "subagent:1".into(),
                content: "probe".into(),
                is_error: false,
            }
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn failed_subagent_turn_persistence_publishes_a_runtime_error() {
        let temporary = tui_session_directory("subagent-persistence-failure");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        std::fs::create_dir_all(bootstrap.data_directory().join("sessions.db")).unwrap();
        let (events, receiver) = BridgeTx::bounded(4);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let bridge = TuiTaskLifecycleBridge::new(events, TuiTaskControls::default())
            .with_session_writer(bootstrap.clone(), Arc::clone(&session));
        let persist = bridge
            .persist_completed
            .clone()
            .expect("session writer should be installed");

        persist(CompletedSubagentTurn {
            id: 7,
            agent: "reviewer".into(),
            task: "review task".into(),
            final_result: "done".into(),
            tool_uses: 1,
        });

        assert_eq!(
            receiver
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("persistence failure should reach the TUI bridge")
                .into_parts()
                .1,
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::error(
                7,
                TuiSubagentErrorKind::Runtime,
            ))
        );
        assert!(session.lock().unwrap().identifier.is_none());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn production_runner_error_publication_orders_sanitized_typed_failure_before_terminal() {
        for (
            source,
            expected_error,
            expected_kind,
            expected_execution,
            expected_status,
            expected_result,
        ) in [
            (
                ChildRunError::Authentication,
                TaskRunnerError::ProviderFailure,
                Some(TuiSubagentErrorKind::Authentication),
                TuiExecutionEvent::Failed { id: 1 },
                TuiSubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Context,
                TaskRunnerError::ProviderFailure,
                Some(TuiSubagentErrorKind::Context),
                TuiExecutionEvent::Failed { id: 1 },
                TuiSubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Network,
                TaskRunnerError::ProviderFailure,
                Some(TuiSubagentErrorKind::Network),
                TuiExecutionEvent::Failed { id: 1 },
                TuiSubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Provider,
                TaskRunnerError::ProviderFailure,
                Some(TuiSubagentErrorKind::Provider),
                TuiExecutionEvent::Failed { id: 1 },
                TuiSubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Protocol,
                TaskRunnerError::ProviderFailure,
                Some(TuiSubagentErrorKind::Protocol),
                TuiExecutionEvent::Failed { id: 1 },
                TuiSubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::RateLimited,
                TaskRunnerError::ProviderFailure,
                Some(TuiSubagentErrorKind::RateLimited),
                TuiExecutionEvent::Failed { id: 1 },
                TuiSubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Rejected,
                TaskRunnerError::ProviderFailure,
                Some(TuiSubagentErrorKind::Rejected),
                TuiExecutionEvent::Failed { id: 1 },
                TuiSubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Server,
                TaskRunnerError::ProviderFailure,
                Some(TuiSubagentErrorKind::Server),
                TuiExecutionEvent::Failed { id: 1 },
                TuiSubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Tool,
                TaskRunnerError::ChildFailure,
                Some(TuiSubagentErrorKind::Tool),
                TuiExecutionEvent::Failed { id: 1 },
                TuiSubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Runtime,
                TaskRunnerError::ChildFailure,
                Some(TuiSubagentErrorKind::Runtime),
                TuiExecutionEvent::Failed { id: 1 },
                TuiSubagentStatus::Failure,
                "failed",
            ),
            (
                ChildRunError::Cancelled,
                TaskRunnerError::Cancelled,
                None,
                TuiExecutionEvent::Cancelled { id: 1 },
                TuiSubagentStatus::Cancelled,
                "cancelled",
            ),
            (
                ChildRunError::TimedOut,
                TaskRunnerError::TimedOut,
                None,
                TuiExecutionEvent::Failed { id: 1 },
                TuiSubagentStatus::Failure,
                "failed",
            ),
        ] {
            let temporary = tui_session_directory("runner-error-publication");
            let bootstrap = tui_session_bootstrap(
                &temporary,
                &[(
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                )],
            );
            let (events, receiver) = BridgeTx::bounded(8);
            let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, TuiTaskControls::default());
            let mut runtime = production_tui_task_runtime_with_runner(
                &bootstrap,
                &SkillCatalog::default(),
                production_tui_permission_bridge().0,
                ProductionTaskRunner::with_failure_probe(
                    bootstrap.clone(),
                    bootstrap.project_root().unwrap().to_path_buf(),
                    source,
                    "provider-token=super-secret-error-detail",
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
            let session = Arc::new(Mutex::new(TuiSessionContext {
                selected_subagent: Some("reviewer".into()),
                ..TuiSessionContext::fresh()
            }));

            assert_eq!(
                launch_selected_tui_task(
                    &mut runtime,
                    &session,
                    "review task",
                    false,
                    &HeadlessTurnCancellation::new(),
                ),
                Err(CliError::runtime(HeadlessTurnError::Tool))
            );

            let mut expected = vec![
                TuiRuntimeEvent::TaskExecution {
                    agent: "reviewer".into(),
                    event: TuiExecutionEvent::ForegroundStarted { id: 1 },
                },
                TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::started(
                    1,
                    "reviewer",
                    "review task",
                    agens_tui::TuiExecutionState::ForegroundRunning,
                )),
            ];
            if let Some(kind) = expected_kind {
                expected.push(TuiRuntimeEvent::SubagentExecution(
                    TuiSubagentEvent::error_with_reference(1, kind, "abc12345"),
                ));
            }
            expected.push(TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: expected_execution,
            });
            expected.push(TuiRuntimeEvent::SubagentExecution(
                TuiSubagentEvent::terminal(1, expected_status, expected_result),
            ));

            let received = (0..expected.len())
                .map(|_| {
                    receiver
                        .recv_timeout(std::time::Duration::from_secs(1))
                        .expect("runner failure should publish every bridge event")
                        .into_parts()
                        .1
                })
                .collect::<Vec<_>>();
            assert_eq!(received, expected);
            assert!(
                received
                    .iter()
                    .all(|event| !format!("{event:?}").contains("super-secret"))
            );
            assert!(
                receiver
                    .recv_timeout(std::time::Duration::from_millis(20))
                    .is_err(),
                "runner failure must publish exactly one terminal"
            );
            assert_eq!(expected_error, source.task_runner_error());

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    #[test]
    fn u15_a1b2_permission_cardinality_is_exact_for_allow_ask_and_deny() {
        fn policy(decision: PermissionDecision) -> PermissionPolicy {
            PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    decision,
                    PermissionPattern::Exact("native::task".into()),
                    PermissionPattern::Any,
                )],
            )
        }

        let temporary = tui_session_directory("selected-task-cardinality");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (bridge, requests) = production_tui_permission_bridge();
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &SkillCatalog::default(),
            bridge.clone(),
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                bootstrap.project_root().unwrap().to_path_buf(),
                Arc::clone(&probe),
            ),
        )
        .unwrap();
        let cancellation = HeadlessTurnCancellation::new();
        let selected = || {
            Arc::new(Mutex::new(TuiSessionContext {
                selected_subagent: Some("reviewer".into()),
                ..TuiSessionContext::fresh()
            }))
        };

        runtime.authorized.gate.policy = policy(PermissionDecision::Allow);
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "allow", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Dispatched)
        );
        assert_eq!(probe.lock().unwrap().len(), 1);
        assert!(requests.try_recv().is_err());

        let ask = policy(PermissionDecision::Ask);
        runtime.authorized.gate.policy = ask.clone();
        runtime.authorized.resolver.authorization.policy = ask;
        let reply_bridge = bridge.clone();
        let reply = std::thread::spawn(move || {
            let request = requests
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("ask should prompt once");
            reply_bridge.reply(request.id(), TuiPermissionReply::AllowOnce)
        });
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "ask", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Dispatched)
        );
        assert!(reply.join().unwrap());
        assert_eq!(probe.lock().unwrap().len(), 2);

        runtime.authorized.gate.policy = policy(PermissionDecision::Deny);
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "deny", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Rejected(TaskLaunchOutcome::Denied))
        );
        assert_eq!(probe.lock().unwrap().len(), 2);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn u15_a1b2_rejections_leave_the_concrete_runner_and_grants_unchanged() {
        let temporary = tui_session_directory("selected-task-rejections");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (bridge, requests) = production_tui_permission_bridge();
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &SkillCatalog::default(),
            bridge.clone(),
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                bootstrap.project_root().unwrap().to_path_buf(),
                Arc::clone(&probe),
            ),
        )
        .unwrap();
        let selected = || {
            Arc::new(Mutex::new(TuiSessionContext {
                selected_subagent: Some("reviewer".into()),
                ..TuiSessionContext::fresh()
            }))
        };
        let cancellation = HeadlessTurnCancellation::new();

        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Rejected(
                TaskLaunchOutcome::RejectedEmptyInput
            ))
        );
        cancellation.cancel();
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "cancelled", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Rejected(
                TaskLaunchOutcome::RejectedCancelled
            ))
        );
        assert_eq!(probe.lock().unwrap().len(), 0);
        assert!(requests.try_recv().is_err());
        assert!(runtime.authorized.gate.grants.lock().unwrap().is_empty());

        runtime.authorized.gate.policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let unavailable = Arc::new(Mutex::new(TuiSessionContext {
            selected_subagent: Some("missing".into()),
            ..TuiSessionContext::fresh()
        }));
        assert_eq!(
            launch_selected_tui_task(
                &mut runtime,
                &unavailable,
                "missing",
                false,
                &HeadlessTurnCancellation::new(),
            ),
            Err(CliError::runtime(HeadlessTurnError::Tool))
        );
        assert_eq!(probe.lock().unwrap().len(), 0);

        let expired = HeadlessTurnCancellation::with_deadline(std::time::Duration::ZERO);
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "expired", false, &expired),
            Err(CliError::runtime(HeadlessTurnError::TimedOut))
        );
        assert_eq!(probe.lock().unwrap().len(), 0);

        let ask = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Ask,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        runtime.authorized.gate.policy = ask.clone();
        runtime.authorized.resolver.authorization.policy = ask;
        let active = HeadlessTurnCancellation::new();
        let reply_bridge = bridge.clone();
        let reply = std::thread::spawn(move || {
            [TuiPermissionReply::DenyOnce, TuiPermissionReply::Cancelled]
                .into_iter()
                .map(|answer| {
                    let request = requests
                        .recv_timeout(std::time::Duration::from_secs(1))
                        .expect("asked rejection should prompt once");
                    reply_bridge.reply(request.id(), answer)
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "deny once", false, &active),
            Ok(TuiSelectedTaskLaunch::Rejected(TaskLaunchOutcome::Denied))
        );
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "cancel ask", false, &active),
            Err(CliError::runtime(HeadlessTurnError::Cancelled))
        );
        assert!(reply.join().unwrap().into_iter().all(|replied| replied));
        assert_eq!(probe.lock().unwrap().len(), 0);
        assert!(runtime.authorized.gate.grants.lock().unwrap().is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn production_prompt_decisions_authorize_only_allowed_calls() {
        struct RecordingTool(Arc<std::sync::atomic::AtomicUsize>);

        impl DispatchTool for RecordingTool {
            fn permission_target(
                &self,
                arguments: &serde_json::Value,
            ) -> Result<String, agens_core::Error> {
                arguments
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| agens_core::Error::Tool("missing path".into()))
            }

            fn execute(
                &mut self,
                _: &ToolExecutionContext,
                _: serde_json::Value,
            ) -> Result<ToolOutput, agens_core::Error> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ToolOutput::success("executed"))
            }
        }

        fn run_ready<T>(
            future: impl std::future::Future<Output = Result<T, HeadlessTurnPortError>>,
        ) -> Result<T, HeadlessTurnPortError> {
            let mut future = std::pin::pin!(future);
            let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

            match future.as_mut().poll(context) {
                std::task::Poll::Ready(result) => result,
                std::task::Poll::Pending => {
                    panic!("production permission ports must complete synchronously")
                }
            }
        }

        for (answer, expected_executions, expected_grants) in [
            (PermissionPromptAnswer::AllowOnce, 1, 0),
            (PermissionPromptAnswer::AllowAlways, 2, 1),
            (PermissionPromptAnswer::DenyOnce, 0, 0),
            (PermissionPromptAnswer::DenyAlways, 0, 1),
            (PermissionPromptAnswer::Cancel, 0, 0),
        ] {
            let directory = std::env::temp_dir().join(format!(
                "agens-production-permission-{}-{:?}",
                std::process::id(),
                answer
            ));
            let _ = std::fs::remove_dir_all(&directory);

            let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
            dispatcher
                .lock()
                .expect("dispatcher lock should be available")
                .register_native(
                    "native::read",
                    agens_core::ToolAccess::ReadOnly,
                    RecordingTool(Arc::clone(&executions)),
                )
                .expect("recording tool should register");

            let grants = Arc::new(Mutex::new(Vec::new()));
            let allowed = Arc::new(Mutex::new(BTreeMap::new()));
            let prompts = Arc::new(Mutex::new(BTreeMap::new()));
            let policy = PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    PermissionDecision::Ask,
                    PermissionPattern::Exact("native::read".into()),
                    PermissionPattern::Exact("notes.md".into()),
                )],
            );
            let call = HeadlessToolCall {
                id: "current".into(),
                name: "native::read".into(),
                input: r#"{"path":"notes.md"}"#.into(),
            };
            let cancellation = HeadlessTurnCancellation::new();
            let mut gate = ProductionPermissionGate::new(
                policy.clone(),
                Arc::clone(&grants),
                PermissionSession::new(),
                "project".into(),
                Arc::clone(&dispatcher),
                Arc::clone(&allowed),
                Arc::clone(&prompts),
            );
            let store = PermissionGrantStore::open(&directory).expect("grant store should open");
            let (bridge, requests) = production_tui_permission_bridge();
            let reply_bridge = bridge.clone();
            let reply = std::thread::spawn(move || {
                let request = requests
                    .recv()
                    .expect("permission request should reach the TUI");
                let reply = match answer {
                    PermissionPromptAnswer::AllowOnce => TuiPermissionReply::AllowOnce,
                    PermissionPromptAnswer::AllowAlways => TuiPermissionReply::AllowAlways,
                    PermissionPromptAnswer::DenyOnce => TuiPermissionReply::DenyOnce,
                    PermissionPromptAnswer::DenyAlways => TuiPermissionReply::DenyAlways,
                    PermissionPromptAnswer::Cancel => TuiPermissionReply::Cancelled,
                };
                let replied = reply_bridge.reply(request.id(), reply);
                (request, replied)
            });
            let mut resolver = ProductionPermissionResolver::new(
                ProductionPermissionPrompter::Tui(bridge),
                store,
                Arc::clone(&grants),
                Arc::clone(&prompts),
                ProductionPromptAuthorization {
                    policy,
                    session: PermissionSession::new(),
                    project: "project".into(),
                    dispatcher: Arc::clone(&dispatcher),
                    allowed: Arc::clone(&allowed),
                },
            );
            let mut production_dispatcher = ProductionToolDispatcher::new(dispatcher, allowed);

            assert_eq!(
                run_ready(gate.evaluate(&call, &cancellation)),
                Ok(PermissionDecision::Ask)
            );
            let decision = run_ready(resolver.resolve(&call, &cancellation));
            let (request, replied) = reply.join().expect("TUI permission reply should finish");
            assert!(request.details().0.starts_with("native:"));
            assert!(request.details().1.contains("notes.md"));
            assert!(replied);

            match answer {
                PermissionPromptAnswer::AllowOnce | PermissionPromptAnswer::AllowAlways => {
                    assert_eq!(decision, Ok(PermissionDecision::Allow));
                    let changed_call = HeadlessToolCall {
                        input: r#"{"path":"changed.md"}"#.into(),
                        ..call.clone()
                    };
                    assert_eq!(
                        run_ready(production_dispatcher.dispatch(changed_call, &cancellation)),
                        Err(HeadlessTurnPortError::Tool)
                    );
                    assert_eq!(
                        run_ready(production_dispatcher.dispatch(call.clone(), &cancellation)),
                        Ok(HeadlessToolOutput::success("executed"))
                    );
                    assert_eq!(
                        run_ready(production_dispatcher.dispatch(call.clone(), &cancellation)),
                        Err(HeadlessTurnPortError::Tool)
                    );
                    if answer == PermissionPromptAnswer::AllowAlways {
                        let later_call = HeadlessToolCall {
                            id: "later".into(),
                            ..call.clone()
                        };
                        assert_eq!(
                            run_ready(gate.evaluate(&later_call, &cancellation)),
                            Ok(PermissionDecision::Allow)
                        );
                        assert_eq!(
                            run_ready(production_dispatcher.dispatch(later_call, &cancellation)),
                            Ok(HeadlessToolOutput::success("executed"))
                        );
                    }
                }
                PermissionPromptAnswer::DenyOnce | PermissionPromptAnswer::DenyAlways => {
                    assert_eq!(decision, Ok(PermissionDecision::Deny));
                }
                PermissionPromptAnswer::Cancel => {
                    assert_eq!(decision, Err(HeadlessTurnPortError::Cancelled));
                }
            }

            assert_eq!(
                executions.load(std::sync::atomic::Ordering::SeqCst),
                expected_executions
            );
            assert_eq!(
                PermissionGrantStore::open(&directory)
                    .expect("grant store should reopen")
                    .grants_for_project("project")
                    .expect("project grants should load")
                    .len(),
                expected_grants
            );
            std::fs::remove_dir_all(&directory)
                .expect("temporary grant directory should be removed");
        }
    }

    #[test]
    fn canonical_and_legacy_mcp_permission_aliases_resolve_after_reload() {
        struct RuntimeTool;

        impl DispatchTool for RuntimeTool {
            fn execute(
                &mut self,
                _: &ToolExecutionContext,
                _: serde_json::Value,
            ) -> Result<ToolOutput, agens_core::Error> {
                Ok(ToolOutput::success("executed"))
            }
        }

        fn dispatcher() -> ToolDispatcher {
            let mut dispatcher = ToolDispatcher::new();
            dispatcher
                .register_mcp(
                    &RemoteToolMetadata {
                        qualified_name: "files::read".into(),
                        server_name: "files".into(),
                        tool_name: "read".into(),
                        description: None,
                        input_schema: serde_json::json!({}),
                        access: agens_tools::RemoteToolAccess::ReadOnly,
                    },
                    RuntimeTool,
                )
                .expect("MCP tool should register");
            dispatcher
        }

        let directory =
            std::env::temp_dir().join(format!("agens-canonical-grants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let request = || {
            ToolDispatchRequest::new(
                "project",
                "files_read",
                serde_json::json!({"target": "notes.md"}),
            )
        };
        let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
        let initial = dispatcher();
        let ToolEvaluationOutcome::PromptRequired(context) = initial
            .evaluate(&policy, &[], &PermissionSession::new(), request())
            .expect("canonical model name should resolve")
        else {
            panic!("ungranted MCP call should require a prompt");
        };
        assert_ne!(context.qualified_tool_name, "files::read");
        let canonical_name = context.qualified_tool_name.clone();

        let canonical = agens_core::ProjectPermissionGrant::allow(
            "project",
            PermissionPattern::Exact(canonical_name.clone()),
            PermissionPattern::Exact(context.target_identifier),
        );
        PermissionGrantStore::open(&directory)
            .expect("grant store should open")
            .append_grants(&[canonical])
            .expect("canonical grant should save");
        let grants = PermissionGrantStore::open(&directory)
            .expect("grant store should reopen")
            .grants_for_project("project")
            .expect("canonical grant should reload");
        assert_eq!(
            grants[0].tool,
            PermissionPattern::Exact(canonical_name),
            "prompt grants must persist the canonical identity"
        );
        let mut reloaded = dispatcher();
        let ToolEvaluationOutcome::Authorized(handle) = reloaded
            .evaluate(&policy, &grants, &PermissionSession::new(), request())
            .expect("canonical grant should resolve after reload")
        else {
            panic!("canonical grant should allow the model call");
        };
        assert_eq!(
            reloaded
                .execute(
                    handle,
                    &ToolExecutionContext::with_timeout(std::time::Duration::from_secs(1))
                )
                .expect("reloaded canonical grant should execute"),
            ToolOutput::success("executed")
        );

        for decision in [PermissionDecision::Allow, PermissionDecision::Deny] {
            let directory = directory.join(format!("legacy-{decision:?}"));
            PermissionGrantStore::open(&directory)
                .expect("grant store should open")
                .append_grants(&[agens_core::ProjectPermissionGrant::new(
                    "project",
                    decision,
                    PermissionPattern::Exact("files::read".into()),
                    PermissionPattern::Exact("notes.md".into()),
                )])
                .expect("legacy grant should save");
            let grants = PermissionGrantStore::open(&directory)
                .expect("grant store should reopen")
                .grants_for_project("project")
                .expect("legacy grant should reload");
            let outcome = dispatcher()
                .evaluate(&policy, &grants, &PermissionSession::new(), request())
                .expect("legacy grant should resolve through the model alias");
            match decision {
                PermissionDecision::Allow => {
                    assert!(matches!(outcome, ToolEvaluationOutcome::Authorized(_)));
                }
                PermissionDecision::Deny => {
                    assert!(matches!(outcome, ToolEvaluationOutcome::Denied));
                }
                PermissionDecision::Ask => unreachable!(),
            }
        }

        for (configured_decision, expected_decision) in [
            (ConfigPermissionDecision::Allow, PermissionDecision::Allow),
            (ConfigPermissionDecision::Deny, PermissionDecision::Deny),
        ] {
            let runtime = Arc::new(Mutex::new(dispatcher()));
            let policy = permission_policy(
                &[ConfigPermissionRule {
                    scope: ConfigPermissionScope::Global,
                    decision: configured_decision,
                    tool_pattern: "files::read".into(),
                    target_pattern: None,
                }],
                "project",
                PermissionMode::Edit,
                &runtime,
                None,
            )
            .expect("legacy configuration should resolve to the canonical model tool");
            let outcome = runtime
                .lock()
                .expect("dispatcher should remain available")
                .evaluate(&policy, &[], &PermissionSession::new(), request())
                .expect("canonical model call should evaluate");
            match expected_decision {
                PermissionDecision::Allow => {
                    assert!(matches!(outcome, ToolEvaluationOutcome::Authorized(_)));
                }
                PermissionDecision::Deny => {
                    assert!(matches!(outcome, ToolEvaluationOutcome::Denied));
                }
                PermissionDecision::Ask => unreachable!(),
            }
        }

        std::fs::remove_dir_all(&directory).expect("temporary grant directory should be removed");
    }

    #[test]
    fn production_mcp_runtime_reloads_dispatcher_and_retains_failed_generation() {
        use std::{collections::VecDeque, sync::atomic::AtomicUsize, time::Duration};

        struct TestTransport(VecDeque<agens_tools::McpResponse>);

        impl McpTransportPort for TestTransport {
            fn execute(
                &mut self,
                _: agens_tools::McpRequest,
                _: &agens_tools::McpOperationContext,
            ) -> Result<agens_tools::McpResponse, McpTransportError> {
                Ok(self
                    .0
                    .pop_front()
                    .expect("test transport response is configured"))
            }

            fn notify(
                &mut self,
                _: agens_tools::McpRequest,
                _: &agens_tools::McpOperationContext,
            ) -> Result<(), McpTransportError> {
                Ok(())
            }

            fn close(
                &mut self,
                _: &agens_tools::McpOperationContext,
            ) -> Result<(), McpTransportError> {
                Ok(())
            }
        }

        fn transport(name: &str) -> TestTransport {
            TestTransport(
                [
                    agens_tools::McpResponse::Initialized(agens_tools::McpInitializeResult::new(
                        "2025-06-18",
                        serde_json::json!({"tools": {}}),
                    )),
                    agens_tools::McpResponse::ToolsListed(agens_tools::McpToolsPage::new(
                        vec![agens_tools::McpToolDefinition {
                            name: name.into(),
                            description: Some(name.into()),
                            input_schema: serde_json::json!({"type": "object"}),
                            annotations: agens_tools::McpToolAnnotations {
                                read_only_hint: Some(true),
                            },
                        }],
                        None,
                    )),
                ]
                .into(),
            )
        }

        let attempts = Arc::new(AtomicUsize::new(0));
        let attempt_counter = Arc::clone(&attempts);
        let registry = Arc::new(Mutex::new(McpRegistry::new()));
        registry
            .lock()
            .unwrap()
            .configure_server(
                "files",
                move || match attempt_counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel) {
                    0 => Ok(Box::new(transport("old"))),
                    1 => Err(McpTransportError::Transport("SENTINEL_SECRET".into())),
                    _ => Ok(Box::new(transport("new"))),
                },
                McpTimeouts::new(
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .unwrap(),
                McpLimits::default(),
            )
            .unwrap();
        let mut runtime = ProductionMcpRuntime {
            registry,
            dispatcher: Arc::new(Mutex::new(ToolDispatcher::new())),
        };

        runtime.discover_server("files").unwrap();
        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Any,
                PermissionPattern::Any,
            )],
        );
        let ToolEvaluationOutcome::Authorized(handle) = runtime
            .dispatcher
            .lock()
            .unwrap()
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new("project", "files_old", serde_json::json!({})),
            )
            .unwrap()
        else {
            panic!("discovered MCP tool must be callable through the dispatcher");
        };

        assert!(runtime.reload_server("files").unwrap().is_failed());
        assert!(
            runtime
                .diagnostics()
                .unwrap()
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("SENTINEL_SECRET"))
        );
        assert!(
            runtime
                .dispatcher
                .lock()
                .unwrap()
                .canonical_identity("files_old")
                .is_some()
        );

        runtime.reload_server("files").unwrap();
        let mut dispatcher = runtime.dispatcher.lock().unwrap();
        assert!(dispatcher.canonical_identity("files_old").is_none());
        assert!(dispatcher.canonical_identity("files_new").is_some());
        assert!(
            dispatcher
                .execute(
                    handle,
                    &ToolExecutionContext::with_timeout(Duration::from_secs(1))
                )
                .is_err()
        );
    }
}
#[test]
fn production_chatgpt_login_errors_render_fixed_sanitized_stages() {
    for error in [
        LoginError::Authentication("setup detail"),
        LoginError::Authentication("callback request is invalid"),
        LoginError::Authentication("authorization was denied"),
        LoginError::TokenTransport,
        LoginError::TokenStatus,
        LoginError::TokenFormat,
        LoginError::Account,
        LoginError::Expiry,
        LoginError::Cancelled,
        LoginError::TimedOut,
    ] {
        let expected = format!("error: auth: {}\n", error.stage_message());
        let result = error_result(&[], chatgpt_login_error(error));
        assert_eq!(result.stderr, expected);
        assert!(!result.stderr.contains("detail"));
        assert_ne!(result.stderr, "error: auth: ChatGPT login failed\n");
    }
}

#[test]
fn turn_attempt_registry_blocks_same_session_begin_and_preserves_primary_errors() {
    let directory =
        std::env::temp_dir().join(format!("agens-attempt-registry-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let metadata = SessionMetadata {
        id: 1,
        project: "project".into(),
        title: "title".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 1,
        updated_at: 1,
        completed_turn_count: 0,
        resumable: false,
    };
    let mut store = SessionStore::open(&directory).unwrap();
    let registry = AttemptActivityRegistry::default();
    let provider_calls = std::sync::atomic::AtomicUsize::new(0);

    let attempt = registry
        .begin_and_register(&mut store, &metadata, "prompt".into())
        .unwrap();
    let second = run_session_attempt_lifecycle(
        &registry,
        &mut store,
        metadata.clone(),
        "second".into(),
        || {
            provider_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(CliError::runtime(HeadlessTurnError::Provider))
        },
    );

    assert!(matches!(
        second,
        Err(AttemptLifecycleError::Begin(
            BeginSessionAttemptError::AlreadyRunning(_)
        ))
    ));
    assert_eq!(provider_calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(registry.contains(attempt.key()));
    registry.unregister(attempt.key());
    assert!(!registry.contains(attempt.key()));

    let mut unrelated = metadata.clone();
    unrelated.id = 2;
    let primary_error =
        run_session_attempt_lifecycle(&registry, &mut store, unrelated, "unrelated".into(), || {
            Err(CliError::runtime(HeadlessTurnError::Provider))
        })
        .unwrap_err();

    assert_eq!(
        primary_error,
        AttemptLifecycleError::runtime(CliError::runtime(HeadlessTurnError::Provider))
    );
    assert!(!registry.contains(attempt.key()));
    assert_eq!(
        store
            .load_session_for_resume(2)
            .unwrap()
            .latest_attempt
            .unwrap()
            .status(),
        agens_core::SessionAttemptStatus::ProviderError
    );

    let mut terminal_failure = metadata.clone();
    terminal_failure.id = 3;
    let terminal_error = run_session_attempt_lifecycle_with_terminal_writer(
        &registry,
        &mut store,
        terminal_failure,
        "terminal failure".into(),
        || Err(CliError::runtime(HeadlessTurnError::Cancelled)),
        |_, _| Err(()),
    )
    .unwrap_err();
    let running = store
        .load_session_for_resume(3)
        .unwrap()
        .latest_attempt
        .unwrap();

    assert_eq!(
        terminal_error,
        AttemptLifecycleError::runtime(CliError::runtime(HeadlessTurnError::Cancelled))
    );
    assert_eq!(running.status(), agens_core::SessionAttemptStatus::Running);
    assert!(!registry.contains(running.key()));

    let mut successful = metadata.clone();
    successful.id = 4;
    let completion = run_session_attempt_lifecycle(
        &registry,
        &mut store,
        successful,
        "successful".into(),
        || {
            let snapshot = CompletedTurnSnapshot::from_persisted_events(vec![
                TurnEvent::StateChanged(TurnState::Requesting),
                TurnEvent::StateChanged(TurnState::Streaming),
                TurnEvent::ProviderPart(MessagePart::Text("answer".into())),
                TurnEvent::StateChanged(TurnState::Completed),
            ])
            .unwrap();
            let turn = completed_session_turn("successful", &snapshot, None).unwrap();

            Ok((snapshot, turn))
        },
    )
    .unwrap();

    assert_eq!(completion.metadata.completed_turn_count, 1);
    assert_eq!(completion.messages.len(), 2);
    assert_eq!(
        store
            .load_session_for_resume(4)
            .unwrap()
            .latest_attempt
            .unwrap()
            .status(),
        agens_core::SessionAttemptStatus::Completed
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn interrupted_attempt_persists_prompt_and_note_and_reuses_the_session() {
    let directory =
        std::env::temp_dir().join(format!("agens-interrupted-partial-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let mut store = SessionStore::open(&directory).unwrap();
    let registry = AttemptActivityRegistry::default();
    let metadata = SessionMetadata {
        id: 1,
        project: "project".into(),
        title: "launch the explorer subagent".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 1,
        updated_at: 1,
        completed_turn_count: 0,
        resumable: false,
    };

    let cancelled = run_session_attempt_lifecycle(
        &registry,
        &mut store,
        metadata.clone(),
        "launch the explorer subagent".into(),
        || Err(CliError::runtime(HeadlessTurnError::Cancelled)),
    )
    .unwrap_err();

    let stored = store.load_session_for_resume(metadata.id).unwrap();

    assert_eq!(stored.metadata.completed_turn_count, 1);
    assert_eq!(
        stored.messages.first().map(|message| message.role),
        Some(Role::User)
    );
    assert_eq!(
        stored.messages.first().map(|message| message.parts.clone()),
        Some(vec![MessagePart::Text(
            "launch the explorer subagent".into()
        )])
    );
    let note = match stored.messages.get(1) {
        Some(Message {
            role: Role::Assistant,
            parts,
        }) => match parts.as_slice() {
            [MessagePart::Text(note)] => note.clone(),
            other => panic!("expected a single note part, got {other:?}"),
        },
        other => panic!("expected an assistant note, got {other:?}"),
    };
    assert!(note.contains("interrupted"), "{note:?}");
    assert_eq!(stored.messages.len(), 2);
    assert_eq!(
        stored.latest_attempt.as_ref().unwrap().status(),
        agens_core::SessionAttemptStatus::Cancelled
    );

    let AttemptLifecycleError::Runtime { error, partial } = cancelled else {
        panic!("expected a runtime failure");
    };
    let partial = partial.expect("an interrupted attempt carries its persisted turn");
    assert!(!format!("{partial:?}").contains("launch the explorer subagent"));

    let mut context = TuiSessionContext::fresh();
    assert!(
        complete_tui_turn(
            &mut context,
            Err(HeadlessChatFailure {
                error,
                partial: Some(partial),
            }),
            false,
        )
        .is_err()
    );
    assert_eq!(context.identifier, Some(metadata.id));

    let next = context.apply_to(interrupted_turn_test_request(
        "volve a lanzar el subagente que cancele",
    ));
    assert_eq!(next.history, stored.messages);
    assert_eq!(
        next.session.as_ref().map(|session| session.id),
        Some(metadata.id)
    );
    assert!(
        OpenAiResponsesProvider::from_api_key_with_messages_and_tools_and_timeout(
            "test-key".into(),
            None,
            "gpt-5.5".into(),
            provider_messages(&next, false),
            Vec::new(),
            std::time::Duration::from_secs(1),
        )
        .is_ok()
    );

    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(test)]
fn interrupted_turn_test_request(prompt: &str) -> HeadlessChatRequest {
    HeadlessChatRequest {
        prompt: prompt.to_owned(),
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
    }
}

#[test]
fn timed_out_attempt_notes_the_interruption_with_requested_subagents() {
    let directory = std::env::temp_dir().join(format!(
        "agens-interrupted-subagents-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let mut store = SessionStore::open(&directory).unwrap();
    let registry = AttemptActivityRegistry::default();
    let metadata = SessionMetadata {
        id: 4,
        project: "project".into(),
        title: "explore the runtime".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 1,
        updated_at: 1,
        completed_turn_count: 0,
        resumable: false,
    };
    let requested = Mutex::new(Vec::new());
    record_requested_subagent(
        &requested,
        &TurnEvent::ToolCallRequested {
            id: "call-1".into(),
            name: "native::task".into(),
            input: r#"{"agent":"explorer","description":"map the session writer"}"#.into(),
        },
    );
    record_requested_subagent(
        &requested,
        &TurnEvent::ToolCallRequested {
            id: "call-2".into(),
            name: "native::read".into(),
            input: r#"{"path":"notes.md"}"#.into(),
        },
    );
    assert_eq!(
        requested.lock().unwrap().as_slice(),
        [RequestedSubagent {
            agent: "explorer".into(),
            description: "map the session writer".into(),
        }]
    );

    let note = interrupted_turn_note(&requested.lock().unwrap());
    let timed_out = run_session_attempt_lifecycle_with_terminal_writer(
        &registry,
        &mut store,
        metadata.clone(),
        "explore the runtime".into(),
        || Err(CliError::runtime(HeadlessTurnError::TimedOut)),
        |store, write| {
            assert_eq!(write.status, agens_core::SessionAttemptStatus::Cancelled);

            write_terminal_attempt(store, write, &note)
        },
    )
    .unwrap_err();

    assert!(matches!(
        timed_out,
        AttemptLifecycleError::Runtime {
            partial: Some(_),
            ..
        }
    ));
    let stored = store.load_session_for_resume(metadata.id).unwrap();
    let [_, Message { parts, .. }] = stored.messages.as_slice() else {
        panic!("expected a prompt and a note, got {:?}", stored.messages);
    };
    let [MessagePart::Text(note)] = parts.as_slice() else {
        panic!("expected a single note part, got {parts:?}");
    };

    assert!(note.contains("interrupted"), "{note:?}");
    assert!(!note.to_ascii_lowercase().contains("cancel"), "{note:?}");
    assert!(note.contains("explorer"), "{note:?}");
    assert!(note.contains("map the session writer"), "{note:?}");
    assert!(
        OpenAiResponsesProvider::from_api_key_with_messages_and_tools_and_timeout(
            "test-key".into(),
            None,
            "gpt-5.5".into(),
            stored.messages,
            Vec::new(),
            std::time::Duration::from_secs(1),
        )
        .is_ok()
    );

    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn explicit_attempt_recovery_is_exact_stale_safe_and_history_preserving() {
    let directory =
        std::env::temp_dir().join(format!("agens-explicit-recovery-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let metadata = SessionMetadata {
        id: 9,
        project: "project".into(),
        title: "title".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 1,
        updated_at: 1,
        completed_turn_count: 0,
        resumable: false,
    };
    let mut store = SessionStore::open(&directory).unwrap();
    let registry = AttemptActivityRegistry::default();
    let active = registry
        .begin_and_register(&mut store, &metadata, "private retry prompt".into())
        .unwrap();

    assert!(matches!(
        recover_session_attempt_lifecycle(&registry, &mut store, active.key(), 2, |_, _, _| {
            unreachable!("a locally active attempt must not invoke retry runtime")
        })
        .unwrap(),
        ExplicitAttemptRecoveryOutcome::LocallyActive
    ));
    assert_eq!(
        store
            .load_session_for_resume(metadata.id)
            .unwrap()
            .latest_attempt
            .unwrap()
            .status(),
        agens_core::SessionAttemptStatus::Running
    );

    registry.unregister(active.key());
    let recovered = recover_session_attempt_lifecycle(
        &registry,
        &mut store,
        active.key(),
        3,
        |history, prompt, _| {
            assert!(history.is_empty());
            assert_eq!(prompt, "private retry prompt");
            let snapshot = CompletedTurnSnapshot::from_persisted_events(vec![
                TurnEvent::StateChanged(TurnState::Requesting),
                TurnEvent::StateChanged(TurnState::Streaming),
                TurnEvent::ProviderPart(MessagePart::Text("recovered answer".into())),
                TurnEvent::StateChanged(TurnState::Completed),
            ])
            .unwrap();
            let turn = completed_session_turn("private retry prompt", &snapshot, None).unwrap();

            Ok((snapshot, turn))
        },
    )
    .unwrap();

    assert!(matches!(
        recovered,
        ExplicitAttemptRecoveryOutcome::Recovered(_)
    ));
    let stored = store.load_session_for_resume(metadata.id).unwrap();
    assert_eq!(stored.metadata.completed_turn_count, 1);
    assert_eq!(stored.messages.len(), 2);
    assert_eq!(
        store.recover_running_attempt(active.key(), 4).unwrap(),
        agens_core::RecoveryOutcome::Stale
    );
    assert!(!registry.contains(active.key()));
    assert!(!format!("{recovered:?}").contains("private retry prompt"));

    let terminal_metadata = SessionMetadata { id: 10, ..metadata };
    let terminal_error = run_session_attempt_lifecycle_with_terminal_writer(
        &registry,
        &mut store,
        terminal_metadata.clone(),
        "terminal retry prompt".into(),
        || Err(CliError::runtime(HeadlessTurnError::Cancelled)),
        |_, _| Err(()),
    )
    .unwrap_err();
    let terminal = store
        .load_session_for_resume(terminal_metadata.id)
        .unwrap()
        .latest_attempt
        .unwrap();

    assert_eq!(
        terminal_error,
        AttemptLifecycleError::runtime(CliError::runtime(HeadlessTurnError::Cancelled))
    );
    assert_eq!(terminal.status(), agens_core::SessionAttemptStatus::Running);
    assert!(!registry.contains(terminal.key()));

    drop(store);
    let mut reopened = SessionStore::open(&directory).unwrap();
    let empty_registry = AttemptActivityRegistry::default();
    assert_eq!(
        reopened
            .load_session_for_resume(terminal_metadata.id)
            .unwrap()
            .latest_attempt
            .unwrap()
            .status(),
        agens_core::SessionAttemptStatus::Running
    );
    assert!(matches!(
        recover_session_attempt_lifecycle(
            &empty_registry,
            &mut reopened,
            terminal.key(),
            5,
            |history, prompt, _| {
                assert!(history.is_empty());
                assert_eq!(prompt, "terminal retry prompt");
                let snapshot = CompletedTurnSnapshot::from_persisted_events(vec![
                    TurnEvent::StateChanged(TurnState::Requesting),
                    TurnEvent::StateChanged(TurnState::Streaming),
                    TurnEvent::ProviderPart(MessagePart::Text("terminal answer".into())),
                    TurnEvent::StateChanged(TurnState::Completed),
                ])
                .unwrap();
                let turn =
                    completed_session_turn("terminal retry prompt", &snapshot, None).unwrap();

                Ok((snapshot, turn))
            },
        )
        .unwrap(),
        ExplicitAttemptRecoveryOutcome::Recovered(_)
    ));
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(test)]
#[test]
fn reliability_integration_bounds_recovers_attempts_and_sanitizes_failures() {
    let directory = std::env::temp_dir().join(format!(
        "agens-reliability-integration-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let mut store = SessionStore::open(&directory).unwrap();
    let registry = AttemptActivityRegistry::default();
    let metadata = reliability_integration_metadata(7, 20);

    let failure = run_session_attempt_lifecycle(
        &registry,
        &mut store,
        metadata.clone(),
        "SENTINEL_PRIVATE_PROVIDER_RETRY".into(),
        || Err(CliError::runtime(HeadlessTurnError::ProviderServer)),
    )
    .unwrap_err();
    let failed = store.load_session_for_resume(metadata.id).unwrap();

    assert_eq!(
        failure,
        AttemptLifecycleError::runtime(CliError::runtime(HeadlessTurnError::ProviderServer))
    );
    assert!(failed.messages.is_empty());
    assert_eq!(
        failed.latest_attempt.as_ref().unwrap().status(),
        agens_core::SessionAttemptStatus::ProviderError
    );
    assert!(!format!("{failed:?}").contains("SENTINEL_PRIVATE_PROVIDER_RETRY"));

    let completed_metadata = reliability_integration_metadata(8, 21);
    let completed = run_session_attempt_lifecycle(
        &registry,
        &mut store,
        completed_metadata.clone(),
        "bounded successful prompt".into(),
        || {
            Ok(reliability_integration_completion(
                "bounded successful prompt",
                "answer",
            ))
        },
    )
    .unwrap();

    assert_eq!(completed.metadata.completed_turn_count, 1);
    assert_eq!(completed.messages.len(), 2);
    assert_eq!(
        store
            .load_session_for_resume(completed_metadata.id)
            .unwrap()
            .latest_attempt
            .unwrap()
            .status(),
        agens_core::SessionAttemptStatus::Completed
    );

    let recovery_metadata = reliability_integration_metadata(9, 22);
    let active = registry
        .begin_and_register(
            &mut store,
            &recovery_metadata,
            "SENTINEL_PRIVATE_RECOVERY_RETRY".into(),
        )
        .unwrap();
    registry.unregister(active.key());
    let recovery = recover_session_attempt_lifecycle(
        &registry,
        &mut store,
        active.key(),
        23,
        |history, prompt, _| {
            assert!(history.is_empty());
            assert_eq!(prompt, "SENTINEL_PRIVATE_RECOVERY_RETRY");
            Ok(reliability_integration_completion(
                prompt,
                "recovered answer",
            ))
        },
    )
    .unwrap();

    assert!(matches!(
        recovery,
        ExplicitAttemptRecoveryOutcome::Recovered(_)
    ));
    assert_eq!(
        store
            .load_session_for_resume(recovery_metadata.id)
            .unwrap()
            .metadata
            .completed_turn_count,
        1
    );

    for id in 10..76 {
        let metadata = reliability_integration_metadata(id, id);
        store
            .begin_session_attempt(&metadata, format!("SENTINEL_PRIVATE_PAGE_{id}"))
            .unwrap();
    }
    let first_page = store.list_session_page(None, "", None, 64).unwrap();
    let second_page = store
        .list_session_page(None, "", first_page.next_cursor, 64)
        .unwrap();

    assert_eq!(first_page.sessions.len(), 64);
    assert_eq!(second_page.sessions.len(), 5);
    assert_eq!(first_page.sessions.len() + second_page.sessions.len(), 69);
    assert!(
        first_page
            .sessions
            .iter()
            .all(|session| session.latest_attempt.is_some())
    );
    assert!(!format!("{first_page:?}").contains("SENTINEL_PRIVATE_PAGE_"));

    for error in [
        HeadlessTurnError::ProviderContext,
        HeadlessTurnError::ProviderNetwork,
        HeadlessTurnError::ProviderRateLimited,
        HeadlessTurnError::ProviderServer,
        HeadlessTurnError::ProviderProtocol,
        HeadlessTurnError::Cancelled,
        HeadlessTurnError::TimedOut,
    ] {
        let error = CliError::runtime(error);
        let rendered = tui_provider_outcome(Err(error));
        assert!(!format!("{rendered:?}").contains("SENTINEL_REMOTE_SECRET"));
    }

    let project_root = directory.join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let (catalog, dispatcher) = production_dangerous_child_tool_runtime(&project_root).unwrap();
    assert_eq!(
        catalog.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
        [
            "read", "list", "search", "glob", "grep", "write", "edit", "bash", "webfetch"
        ]
    );
    assert!(
        dispatcher
            .lock()
            .unwrap()
            .canonical_identity("native::task")
            .is_none()
    );

    let mut tui = Tui::new(ReliabilityTuiEngine);
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 7 },
    });
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
        TuiSubagentEvent::started(
            7,
            "reviewer",
            "owner hierarchy",
            agens_tui::TuiExecutionState::ForegroundRunning,
        ),
    ));
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::text(
        7,
        "child-only-sentinel",
    )));
    assert!(
        tui.view().conversation.unwrap().subagent_cards[0]
            .tool_calls
            .is_empty()
    );
    tui.select_transcript(agens_tui::TranscriptId::Subagent(7));
    assert!(
        tui.view()
            .conversation
            .unwrap()
            .live_markdown
            .contains("child-only-sentinel")
    );

    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(test)]
struct ReliabilityTuiEngine;

#[cfg(test)]
impl TuiEngine for ReliabilityTuiEngine {
    fn cancel(&mut self) {}
}

#[cfg(test)]
fn reliability_integration_metadata(id: i64, updated_at: i64) -> SessionMetadata {
    SessionMetadata {
        id,
        project: "reliability".into(),
        title: format!("session-{id}"),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: updated_at,
        updated_at,
        completed_turn_count: 0,
        resumable: false,
    }
}

#[cfg(test)]
fn reliability_integration_completion(
    prompt: &str,
    answer: &str,
) -> (CompletedTurnSnapshot, CompletedSessionTurn) {
    let snapshot = CompletedTurnSnapshot::from_persisted_events(vec![
        TurnEvent::StateChanged(TurnState::Requesting),
        TurnEvent::StateChanged(TurnState::Streaming),
        TurnEvent::ProviderPart(MessagePart::Text(answer.into())),
        TurnEvent::StateChanged(TurnState::Completed),
    ])
    .unwrap();
    let turn = completed_session_turn(prompt, &snapshot, None).unwrap();

    (snapshot, turn)
}

#[cfg(unix)]
#[test]
fn diagnostics_store_writes_only_allowlisted_jsonl_with_private_bounded_files() {
    use std::os::unix::fs::PermissionsExt;

    let data_directory = std::env::temp_dir().join(format!(
        "agens-safe-diagnostics-{}-{}",
        std::process::id(),
        DIAGNOSTIC_REFERENCE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&data_directory).expect("test data directory should be created");
    let store = SafeDiagnosticStore::new(data_directory.clone());
    let event = ProviderDiagnosticEvent {
        reference: DiagnosticRef::new("abc12345".into()).expect("reference should be valid"),
        scope: ProviderDiagnosticScope::Subagent,
        component: ProviderDiagnosticComponent::Responses,
        event: ProviderDiagnosticKind::RetryScheduled,
        attempt: 1,
        max_attempts: 3,
        delay_ms: Some(275),
        status: Some(429),
        class: Some(ProviderDiagnosticClass::RateLimited),
    };

    store.record(&event);

    let diagnostics_directory = data_directory.join("diagnostics");
    assert_eq!(
        std::fs::metadata(&diagnostics_directory)
            .expect("diagnostics metadata should be readable")
            .permissions()
            .mode()
            & 0o077,
        0
    );
    let active = diagnostics_directory.join(format!("agens-{}.jsonl", std::process::id()));
    let line = std::fs::read_to_string(&active).expect("diagnostic should be readable");
    let object = serde_json::from_str::<serde_json::Value>(&line)
        .expect("diagnostic should be JSON")
        .as_object()
        .expect("diagnostic should be an object")
        .clone();
    assert_eq!(
        object.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "attempt",
            "class",
            "component",
            "delay_ms",
            "event",
            "max_attempts",
            "reference",
            "scope",
            "status",
            "timestamp_ms",
        ])
    );
    assert_eq!(object["reference"], "abc12345");
    assert!(!line.contains("prompt"));
    assert!(!line.contains("authorization"));
    assert_eq!(
        std::fs::metadata(&active)
            .expect("diagnostic file metadata should be readable")
            .permissions()
            .mode()
            & 0o077,
        0
    );

    for _ in 0..4 {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&active)
            .expect("active diagnostics file should open")
            .set_len(DIAGNOSTIC_FILE_LIMIT_BYTES)
            .expect("test should fill diagnostics file");
        store.record(&event);
    }
    assert_eq!(
        std::fs::read_dir(&diagnostics_directory)
            .expect("diagnostics directory should be readable")
            .count(),
        DIAGNOSTIC_FILE_COUNT_LIMIT
    );
    assert!(
        std::fs::read_dir(&diagnostics_directory)
            .expect("diagnostics directory should be readable")
            .all(|entry| entry
                .expect("diagnostic entry should be readable")
                .metadata()
                .expect("diagnostic metadata should be readable")
                .len()
                <= DIAGNOSTIC_FILE_LIMIT_BYTES)
    );

    std::fs::remove_dir_all(data_directory).expect("test directory should be removed");
}

#[cfg(unix)]
#[test]
fn diagnostics_dialog_projects_only_safe_fields_and_relative_paths() {
    use std::os::unix::fs::symlink;

    let data_directory = std::env::temp_dir().join(format!(
        "agens-diagnostics-dialog-{}-{}",
        std::process::id(),
        DIAGNOSTIC_REFERENCE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let diagnostics_directory = data_directory.join("diagnostics");
    std::fs::create_dir_all(&diagnostics_directory)
        .expect("diagnostics directory should be created");
    std::fs::write(
        diagnostics_directory.join("agens-42.jsonl"),
        concat!(
            "{\"timestamp_ms\":1,\"reference\":\"abc12345\",\"scope\":\"parent\",",
            "\"component\":\"responses\",\"event\":\"terminal\",\"attempt\":3,",
            "\"max_attempts\":3,\"delay_ms\":null,\"status\":429,",
            "\"class\":\"rate_limited\",\"unknown\":\"SENTINEL_SECRET\"}\n"
        ),
    )
    .expect("diagnostic fixture should be written");
    let outside = data_directory.join("outside.txt");
    std::fs::write(&outside, "SENTINEL_OUTSIDE").expect("outside fixture should be written");
    symlink(&outside, diagnostics_directory.join("agens-99.jsonl"))
        .expect("diagnostic symlink should be created");

    let rendered = format!("{:?}", diagnostics_dialog(&data_directory));

    assert!(rendered.contains("abc12345"));
    assert!(rendered.contains("diagnostics/agens-42.jsonl"));
    assert!(!rendered.contains(&data_directory.display().to_string()));
    assert!(!rendered.contains("SENTINEL_SECRET"));
    assert!(!rendered.contains("SENTINEL_OUTSIDE"));

    std::fs::remove_dir_all(data_directory).expect("test directory should be removed");
}
