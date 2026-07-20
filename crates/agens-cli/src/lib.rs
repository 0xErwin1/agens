use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agens_config::{
    ConfigPaths, ConfigPermissionDecision, ConfigPermissionRule, ConfigPermissionScope,
    McpTransport, expand_environment, expand_environment_with_commands, extract_permission_rules,
    mcp_servers, merge_toml_documents, parse_toml_document, resolve_paths, validate_toml_document,
};
use agens_core::{
    AgentDefinition, CompletedSessionTurn, CompletedTurnRepository, CompletedTurnSnapshot,
    CompletedTurnStoreError, HeadlessPermissionGate, HeadlessPermissionResolver, HeadlessToolCall,
    HeadlessToolDispatcher, HeadlessToolOutput, HeadlessTurnCancellation, HeadlessTurnError,
    HeadlessTurnPortError, Message, MessagePart, PermissionDecision, PermissionMode,
    PermissionPattern, PermissionPolicy, PermissionRule, PermissionSession, Role, SessionMessage,
    SessionMetadata, TurnEvent, TurnProgressSink, TurnState,
    run_headless_turn_with_max_iterations_and_progress,
};
use agens_providers::chatgpt_login::{
    LoginCancellation, LoginError, remove_provider_entry, upsert_provider_entry,
};
use agens_providers::{
    ChatGptAuthState, ChatGptResponsesProvider, OpenAiFunctionTool, OpenAiResponsesProvider,
    ProgressAwareProvider, load_chatgpt_auth_state,
};
use agens_store::{PermissionGrantStore, SessionStore};
use agens_tools::{
    AgentCatalog, AgentModelValidator, AuthorizedToolCall, CommandCatalog, CommandDefinition,
    DispatchTool, EffectiveCapabilitySet, McpHttpTransport, McpLimits, McpRegistry,
    McpSseTransport, McpStdioTransport, McpStdioTransportConfig, McpTimeouts,
    McpTransport as McpTransportPort, McpTransportError, NativeToolCatalog, NativeTools,
    PermissionPromptContext, ReadFileInput, RemoteToolMetadata, SkillCatalog, SkillResourceTool,
    TaskRunContext, TaskRunner, TaskRunnerError, TaskTool, TaskTurnRequest, TaskTurnResult,
    ToolDispatchRequest, ToolDispatcher, ToolEvaluationOutcome, ToolExecutionContext, ToolOutput,
};
use agens_tui::{
    BridgeCancel, BridgeTx, DiffLine, DiffLineKind, Engine as TuiEngine, PaletteEntry,
    PaletteEntryKind, ToolResultState, Tui, TuiPresentation, TuiProviderOutcome, TuiRouteProgress,
    TuiRuntimeEvent, TuiSubmissionOutcome, run_with_default_progress_submit,
};

mod chatgpt_auth;
mod model_registry;

use chatgpt_auth::{ChatGptAuthCoordinator, ChatGptAuthFlow, ChatGptAuthProgress};

pub use model_registry::TuiModelSelector;

const UNAVAILABLE_MESSAGE: &str = "this command is not implemented yet";
const TUI_ERROR_ACTION: &str = "Correct the command or runtime condition, then retry.";
const RESERVED_TUI_COMMANDS: &[&str] = &[
    "agent",
    "connect",
    "disconnect",
    "effort",
    "help",
    "model",
    "new",
    "quit",
    "resume",
    "sessions",
    "subagent",
];

const TUI_PALETTE_BUILT_INS: &[(&str, &str, &str)] = &[
    ("connect", "Connect to ChatGPT", "[--device-auth]"),
    ("disconnect", "Disconnect ChatGPT credentials", ""),
    ("new", "Start a new session", ""),
    ("sessions", "List saved sessions", ""),
    ("resume", "Resume a saved session", "<id>"),
    ("agent", "List or select the primary agent", "[name]"),
    ("model", "List or select the model", "[name]"),
    ("effort", "Show or set reasoning effort", "[level]"),
    ("help", "Show commands and skills", ""),
    ("quit", "Exit Agens", ""),
];

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
            HeadlessTurnError::ProviderRateLimited => (
                ExitStatus::Failure,
                "provider",
                "ChatGPT request was rate limited",
            ),
            HeadlessTurnError::ProviderServer => {
                (ExitStatus::Failure, "provider", "ChatGPT service failed")
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
    request_config: agens_core::RequestConfig,
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

struct TuiMetricsPublisher {
    bridge: BridgeTx<TuiRuntimeEvent>,
    cancellation: BridgeCancel,
    turn_started_at: Option<std::time::Instant>,
    tools: BTreeMap<String, (String, std::time::Instant)>,
}

impl TuiMetricsPublisher {
    fn new(bridge: BridgeTx<TuiRuntimeEvent>, cancellation: BridgeCancel) -> Self {
        Self {
            bridge,
            cancellation,
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
            TurnEvent::Usage(usage) => Some(TuiRuntimeEvent::Usage(usage.clone())),
            TurnEvent::ToolCallRequested { id, name, input } => {
                self.tools.insert(id.clone(), (name.clone(), now));
                Some(TuiRuntimeEvent::ToolStarted {
                    call_id: id.clone(),
                    name: name.clone(),
                    input: sanitize_tui_metric(input),
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
    active_agent: Option<ActiveAgentRuntime>,
    pending_system_reminder: Option<String>,
    selection: Option<TuiModelSelector>,
    selected_subagent: Option<String>,
    running: bool,
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
        let model = agent
            .model
            .as_deref()
            .or(inherited_model)
            .map(str::to_owned);
        if model
            .as_deref()
            .is_some_and(|model| validator.validate_model(model).is_err())
        {
            return Err(AgentRotationError::ModelUnavailable);
        }
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
    project: &str,
    dispatcher: &ToolDispatcher,
    validator: &dyn AgentModelValidator,
    store: Option<&mut SessionStore>,
    busy: bool,
) -> Result<(), AgentRotationError> {
    if busy {
        return Err(AgentRotationError::Busy);
    }
    let inherited_model = context
        .active_agent
        .as_ref()
        .and_then(|agent| agent.model.as_deref());
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

impl TuiSessionContext {
    fn fresh() -> Self {
        Self::default()
    }

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
            active_agent: Some(active_agent),
            pending_system_reminder: None,
            selection: None,
            selected_subagent: None,
            running: false,
        }
    }

    fn note(&self) -> String {
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
        if self.identifier.is_some() {
            request.history = self.messages.clone();
            request.session = self.metadata.clone();
        }

        if let Some(agent) = &self.active_agent {
            if request.model.is_none() {
                request.model = agent.model.clone();
            }
            request
                .system_prompt
                .get_or_insert_with(|| agent.system_prompt.clone());
            request.active_agent = Some(agent.name.clone());
            request.effective_capabilities = Some(agent.capabilities.clone());
        }
        if let Some(selection) = &self.selection {
            request.model = Some(selection.model().to_owned());
            request.request_config = selection.request_config().clone();
        }
        request.pending_system_reminder = self.pending_system_reminder.clone();

        request
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
    commands: Arc<CommandCatalog>,
    skills: Arc<SkillCatalog>,
    palette: Arc<[PaletteEntry]>,
}

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
        bootstrap: Bootstrap,
        session: Arc<Mutex<TuiSessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
        auth: ChatGptAuthCoordinator,
    ) -> Self {
        let palette = resolved_tui_palette(&commands, &skills).into();
        Self {
            bootstrap: Arc::new(Mutex::new(bootstrap)),
            session,
            cancellation,
            auth,
            commands,
            skills,
            palette,
        }
    }

    #[cfg(test)]
    fn route(&self, input: String) -> TuiSubmissionOutcome {
        let (progress, _) = std::sync::mpsc::channel();
        self.route_with_progress(input, progress)
    }

    fn route_with_progress(
        &self,
        input: String,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) -> TuiSubmissionOutcome {
        let command = input.trim();
        let auth = match command {
            "/connect" => Some(self.connect(ChatGptAuthFlow::Browser, progress)),
            "/connect --device-auth" => Some(self.connect(ChatGptAuthFlow::Device, progress)),
            "/disconnect" => Some(self.disconnect()),
            _ => None,
        };
        if let Some(result) = auth {
            return match result {
                Ok(message) => TuiSubmissionOutcome::LocalInfo(message),
                Err(AuthRouteError::Auth(error)) => TuiSubmissionOutcome::LocalActionableError {
                    message: error.message().into(),
                    action: error.action().into(),
                },
                Err(AuthRouteError::Runtime(error)) => TuiSubmissionOutcome::LocalActionableError {
                    message: error.to_string(),
                    action: TUI_ERROR_ACTION.into(),
                },
            };
        }
        self.resolve(input)
            .unwrap_or_else(|error| TuiSubmissionOutcome::LocalActionableError {
                message: error.to_string(),
                action: TUI_ERROR_ACTION.into(),
            })
    }

    fn palette_entries(&self) -> &[PaletteEntry] {
        &self.palette
    }

    fn resolve(&self, input: String) -> Result<TuiSubmissionOutcome, CliError> {
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
            "/help" => TuiSubmissionOutcome::LocalInfo(render_tui_help(&self.palette)),
            "/quit" => TuiSubmissionOutcome::Quit,
            "/sessions" => TuiSubmissionOutcome::LocalInfo(list_tui_sessions(&bootstrap)?),
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
                if tui_session_is_running(&self.session)? {
                    return Err(CliError::runtime(HeadlessTurnError::State));
                }
                let identifier = command[8..]
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| CliError::usage("/resume requires a numeric session id"))?;
                let resumed = resume_tui_session(&bootstrap, identifier, &self.skills)?;
                let message = resumed.note();
                let mut session = self.session.lock().map_err(|_| {
                    CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
                })?;
                if session.running {
                    return Err(CliError::runtime(HeadlessTurnError::State));
                }
                *session = resumed;
                drop(session);
                TuiSubmissionOutcome::ContextChanged {
                    message,
                    presentation: self.presentation()?,
                }
            }
            command if command.starts_with("/agent ") => TuiSubmissionOutcome::ContextChanged {
                message: rotate_tui_agent(&bootstrap, &command[7..], &self.session, &self.skills)?,
                presentation: self.presentation()?,
            },
            "/agent" => TuiSubmissionOutcome::LocalInfo(list_tui_agents(
                &bootstrap,
                &self.session,
                agens_core::AgentMode::Primary,
            )?),
            command if command.starts_with("/subagent ") => TuiSubmissionOutcome::ContextChanged {
                message: select_tui_subagent(&bootstrap, &command[10..], &self.session)?,
                presentation: self.presentation()?,
            },
            "/subagent" => TuiSubmissionOutcome::LocalInfo(list_tui_agents(
                &bootstrap,
                &self.session,
                agens_core::AgentMode::Subagent,
            )?),
            "/model" => TuiSubmissionOutcome::LocalInfo(select_tui_model(
                &bootstrap,
                command,
                &self.session,
            )?),
            command if command.starts_with("/model ") => TuiSubmissionOutcome::ContextChanged {
                message: select_tui_model(&bootstrap, command, &self.session)?,
                presentation: self.presentation()?,
            },
            "/effort" => TuiSubmissionOutcome::LocalInfo(select_tui_effort(
                &bootstrap,
                command,
                &self.session,
            )?),
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
        let model = session
            .selection
            .as_ref()
            .map(TuiModelSelector::model)
            .or_else(|| {
                session
                    .active_agent
                    .as_ref()
                    .and_then(|agent| agent.model.as_deref())
            })
            .or_else(|| bootstrap.model())
            .unwrap_or_else(|| default_model(&bootstrap));
        let label = session
            .identifier
            .map_or_else(|| "new session".into(), |id| format!("session #{id}"));
        Ok(TuiPresentation::new(
            bootstrap.provider_type().unwrap_or("provider"),
            model,
            label,
        ))
    }

    fn bootstrap(&self) -> Result<Bootstrap, CliError> {
        self.bootstrap
            .lock()
            .map(|bootstrap| bootstrap.clone())
            .map_err(|_| CliError::storage("TUI provider state is unavailable"))
    }

    fn connect(
        &self,
        flow: ChatGptAuthFlow,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) -> Result<String, AuthRouteError> {
        let operation =
            HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(600));
        *self.cancellation.lock().map_err(|_| {
            AuthRouteError::Runtime(CliError::storage("TUI cancellation is unavailable"))
        })? = Some(operation.clone());
        let view = operation.adapter_view();
        let path = self
            .bootstrap()
            .map_err(AuthRouteError::Runtime)?
            .paths
            .credentials;
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
        self.reconcile_provider(true)
            .map_err(AuthRouteError::Runtime)?;
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
            self.reconcile_provider(false)
                .map_err(AuthRouteError::Runtime)?;
            Ok("Disconnected from ChatGPT.".into())
        } else {
            Ok("No ChatGPT credentials were stored.".into())
        }
    }

    fn reconcile_provider(&self, connected: bool) -> Result<(), CliError> {
        let mut bootstrap = self
            .bootstrap
            .lock()
            .map_err(|_| CliError::storage("TUI provider state is unavailable"))?;
        match (bootstrap.provider_source, connected) {
            (ProviderSource::Auto, true) => {
                bootstrap.provider_type = Some("openai-chatgpt".into());
            }
            (ProviderSource::Auto, false) => {
                bootstrap.provider_type = resolve_current_auto_provider(&bootstrap)?;
            }
            (ProviderSource::ExplicitChatGpt, _) => {
                bootstrap.provider_type = Some("openai-chatgpt".into());
            }
            (ProviderSource::ExplicitOther, _) => {}
        }
        Ok(())
    }
}

enum AuthRouteError {
    Auth(chatgpt_auth::ChatGptAuthError),
    Runtime(CliError),
}

fn tui_provider_outcome(result: Result<String, CliError>) -> TuiProviderOutcome {
    match result {
        Ok(output) => TuiProviderOutcome::Completed(output),
        Err(error) if error.category == "cancelled" => TuiProviderOutcome::Cancelled {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
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

fn resolved_tui_palette(commands: &CommandCatalog, skills: &SkillCatalog) -> Vec<PaletteEntry> {
    let mut entries = TUI_PALETTE_BUILT_INS
        .iter()
        .map(|(name, description, hint)| {
            PaletteEntry::new(*name, *description, *hint, PaletteEntryKind::BuiltIn)
        })
        .collect::<Vec<_>>();
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

fn run_production_tui(bootstrap: &Bootstrap, resume: Option<i64>) -> Result<String, CliError> {
    let cancellation = Arc::new(Mutex::new(None));
    let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
    let engine = ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    };
    let mut tui = Tui::new(engine);
    let skills = start_tui_skills(&mut tui, bootstrap)?;
    let provider = bootstrap.provider_type().unwrap_or("provider");
    let model = bootstrap.model().unwrap_or("default model");
    let mut session_label = "new session".to_owned();

    if let Some(identifier) = resume {
        let resumed = resume_tui_session(bootstrap, identifier, &skills)?;
        tui.add_info(resumed.note());
        *session.lock().map_err(|_| {
            CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
        })? = resumed;
        session_label = format!("session #{identifier}");
    }
    tui.set_presentation(provider, model, session_label);

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
    run_with_default_progress_submit(
        &mut tui,
        move |prompt, progress| route_router.route_with_progress(prompt, progress),
        move |prompt, progress, metrics| {
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

            let metrics = Arc::new(Mutex::new(TuiMetricsPublisher::new(
                metrics,
                BridgeCancel::new(),
            )));
            let metrics_progress = Arc::clone(&metrics);
            let sink: TurnProgressSink = Arc::new(move |event| {
                if let Ok(mut metrics) = metrics_progress.lock() {
                    metrics.observe(&event);
                }
                let _ = progress.send(event);
            });
            let runtime_bootstrap = match router.bootstrap() {
                Ok(bootstrap) => bootstrap,
                Err(error) => return tui_provider_outcome(Err(error)),
            };
            let result = run_tui_prompt_with(
                &runtime_bootstrap,
                &prompt,
                &router.session,
                Some(Arc::clone(&router.skills)),
                |request| {
                    run_production_headless_chat_with_progress(
                        request,
                        &runtime_bootstrap,
                        &turn_cancellation,
                        Some(&sink),
                    )
                },
            );

            finish_tui_metrics(&metrics, &result);

            if let Ok(mut active) = cancellation.lock() {
                *active = None;
            }

            tui_provider_outcome(result)
        },
    )
    .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "terminal UI failed"))?;

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
                | TuiSubmissionOutcome::ResetSucceeded { message, .. }
                | TuiSubmissionOutcome::ContextChanged { message, .. } => Ok(message),
                TuiSubmissionOutcome::ProviderTurn { .. }
                | TuiSubmissionOutcome::LocalActionableError { .. } => {
                    unreachable!("slash routing returns a local result or CLI error")
                }
                TuiSubmissionOutcome::Quit => Ok(String::new()),
            }
        }
        prompt => run_tui_prompt_with(bootstrap, prompt, session, None, |request| {
            run_production_headless_chat_with_progress(request, bootstrap, cancellation, progress)
        }),
    }
}

fn run_tui_prompt_with(
    bootstrap: &Bootstrap,
    prompt: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
    skills: Option<Arc<SkillCatalog>>,
    run: impl FnOnce(HeadlessChatRequest) -> Result<HeadlessChatCompletion, CliError>,
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
            request_config: agens_core::RequestConfig::default(),
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
    completion: Result<HeadlessChatCompletion, CliError>,
    consumed_reminder: bool,
) -> Result<String, CliError> {
    let completion = completion?;
    session.identifier = Some(completion.metadata.id);
    session.metadata = Some(completion.metadata);
    if consumed_reminder {
        session.pending_system_reminder = None;
    }
    Ok(completion.text)
}

fn select_tui_model(
    bootstrap: &Bootstrap,
    command: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let model = command.strip_prefix("/model").unwrap_or_default().trim();
    if model.is_empty() {
        let selector = TuiModelSelector::new("gpt-4.1");
        let values = selector
            .model_values()
            .map_err(CliError::unavailable)?
            .join(", ");
        let context = session
            .lock()
            .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
        let current = context
            .selection
            .as_ref()
            .map(|selection| selection.model())
            .or_else(|| bootstrap.model())
            .unwrap_or_else(|| default_model(bootstrap));
        return Ok(format!("Model: {current}. Available: {values}."));
    }

    let mut selector = TuiModelSelector::new(model);
    selector
        .apply_model(model)
        .map_err(CliError::configuration)?;
    let model = selector.model().to_owned();
    let mut context = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?;
    context.selection = Some(selector);
    Ok(format!("Model: {model}."))
}

fn select_tui_effort(
    bootstrap: &Bootstrap,
    command: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let effort = command.strip_prefix("/effort").unwrap_or_default().trim();
    let mut context = session
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

    let model = context
        .selection
        .as_ref()
        .map(|selection| selection.model())
        .or_else(|| bootstrap.model())
        .unwrap_or_else(|| default_model(bootstrap));
    let mut selector = TuiModelSelector::new(model);
    selector
        .apply_reasoning_effort(effort)
        .map_err(CliError::configuration)?;
    context.selection = Some(selector);
    Ok(format!("Reasoning effort: {effort}."))
}

fn rotate_tui_agent(
    bootstrap: &Bootstrap,
    name: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
    skills: &SkillCatalog,
) -> Result<String, CliError> {
    let validator = BundledModelValidator;
    let catalog = tui_agent_catalog(bootstrap, &validator)?;
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    let (_, dispatcher) = production_tool_runtime(bootstrap, project_root, Some(skills))?;
    let dispatcher = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is unavailable"))?;
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    if context.running {
        return Err(CliError::runtime(HeadlessTurnError::State));
    }
    if context.active_agent.is_none() {
        let current = context
            .metadata
            .as_ref()
            .map(|metadata| metadata.active_agent.as_str())
            .unwrap_or("primary");
        let agent = catalog
            .agent(current)
            .ok_or_else(|| CliError::configuration("active agent is unavailable"))?;
        context.active_agent = Some(
            ActiveAgentRuntime::build(
                agent,
                bootstrap.model(),
                &project_root.display().to_string(),
                &dispatcher,
                &validator,
            )
            .map_err(agent_rotation_error)?,
        );
    }
    let agent = catalog
        .agent(name.trim())
        .filter(|agent| agent.mode != agens_core::AgentMode::Subagent)
        .ok_or_else(|| CliError::usage("/agent requires an available primary agent"))?;
    let mut store = context
        .metadata
        .is_some()
        .then(|| SessionStore::open(bootstrap.data_directory()))
        .transpose()
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let running = context.running;
    rotate_active_agent(
        &mut context,
        agent,
        &project_root.display().to_string(),
        &dispatcher,
        &validator,
        store.as_mut(),
        running,
    )
    .map_err(agent_rotation_error)?;
    Ok(format!("Active agent: {}.", agent.name))
}

fn tui_session_is_running(session: &Arc<Mutex<TuiSessionContext>>) -> Result<bool, CliError> {
    session
        .lock()
        .map(|context| context.running)
        .map_err(|_| CliError::storage("TUI session is unavailable"))
}

fn list_tui_agents(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<TuiSessionContext>>,
    mode: agens_core::AgentMode,
) -> Result<String, CliError> {
    let catalog = tui_agent_catalog(bootstrap, &BundledModelValidator)?;
    let context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
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
    let catalog = tui_agent_catalog(bootstrap, &BundledModelValidator)?;
    let agent = catalog
        .agent(name.trim())
        .filter(|agent| agent.mode == agens_core::AgentMode::Subagent)
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

fn tui_agent_catalog(
    bootstrap: &Bootstrap,
    validator: &dyn AgentModelValidator,
) -> Result<AgentCatalog, CliError> {
    let primary = AgentDefinition {
        name: "primary".into(),
        description: "Default interactive agent".into(),
        mode: agens_core::AgentMode::Primary,
        model: bootstrap.model().map(ToOwned::to_owned),
        system_prompt: bootstrap
            .system_prompt
            .clone()
            .unwrap_or_else(|| "You are Agens, a helpful coding agent.".into()),
        permission_rules: Vec::new(),
        skills: Vec::new(),
    };
    let global = bootstrap.paths.global_config.with_file_name("agents");
    let project = bootstrap.paths.project_config.with_file_name("agents");
    AgentCatalog::discover_with_model_validator(&[primary], &global, &project, validator)
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

struct BundledModelValidator;

impl AgentModelValidator for BundledModelValidator {
    fn validate_model(&self, model: &str) -> Result<(), agens_tools::AgentModelValidationError> {
        model_registry::bundled_openai_models()
            .map_err(|_| agens_tools::AgentModelValidationError::Unavailable)?
            .iter()
            .any(|candidate| candidate.id == model)
            .then_some(())
            .ok_or(agens_tools::AgentModelValidationError::Unavailable)
    }
}

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

fn resume_tui_session(
    bootstrap: &Bootstrap,
    identifier: i64,
    skills: &SkillCatalog,
) -> Result<TuiSessionContext, CliError> {
    let store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let session = store
        .load_session_for_resume(identifier)
        .map_err(|_| CliError::storage("saved session is unavailable"))?;
    if session.metadata.project != tui_project_identifier(bootstrap)? {
        return Err(CliError::storage("saved session is unavailable"));
    }
    let active_agent = active_tui_agent_runtime(bootstrap, &session.metadata.active_agent, skills)?;
    Ok(TuiSessionContext::resumed(
        identifier,
        session.metadata,
        session.messages,
        active_agent,
    ))
}

fn tui_project_identifier(bootstrap: &Bootstrap) -> Result<String, CliError> {
    bootstrap
        .project_root()
        .map(|project| project.display().to_string())
        .ok_or_else(|| CliError::configuration("TUI sessions require a project root"))
}

fn active_tui_agent_runtime(
    bootstrap: &Bootstrap,
    name: &str,
    skills: &SkillCatalog,
) -> Result<ActiveAgentRuntime, CliError> {
    let validator = BundledModelValidator;
    let catalog = tui_agent_catalog(bootstrap, &validator)?;
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    let (_, dispatcher) = production_tool_runtime(bootstrap, project_root, Some(skills))?;
    let dispatcher = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is unavailable"))?;
    let agent = catalog
        .agent(name)
        .filter(|agent| agent.mode != agens_core::AgentMode::Subagent)
        .ok_or_else(|| CliError::configuration("active agent is unavailable"))?;

    ActiveAgentRuntime::build(
        agent,
        bootstrap.model(),
        &project_root.display().to_string(),
        &dispatcher,
        &validator,
    )
    .map_err(agent_rotation_error)
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
        request_config: agens_core::RequestConfig::default(),
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

#[derive(Clone, Copy)]
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
    openai_api_key: Option<String>,
    data_directory: PathBuf,
    project_root: Option<PathBuf>,
    mcp_servers: Vec<agens_config::McpServerConfig>,
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
            openai_api_key: self.openai_api_key.clone(),
            data_directory: self.data_directory.clone(),
            project_root: self.project_root.clone(),
            mcp_servers: self.mcp_servers.clone(),
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
        openai_api_key: openai_api_key(credentials.as_deref(), &environment),
        data_directory: data_directory(&document, home_directory.as_deref(), &environment),
        project_root,
        mcp_servers,
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
    run_production_headless_chat_with_progress(request, bootstrap, cancellation, None)
        .map(|completion| completion.text)
}

struct HeadlessChatCompletion {
    text: String,
    metadata: SessionMetadata,
}

fn run_production_headless_chat_with_progress(
    request: HeadlessChatRequest,
    bootstrap: &Bootstrap,
    cancellation: &HeadlessTurnCancellation,
    progress: Option<&TurnProgressSink>,
) -> Result<HeadlessChatCompletion, CliError> {
    match bootstrap.provider_type() {
        Some("openai-api") => {
            let api_key = bootstrap.openai_api_key.clone().ok_or_else(|| {
                CliError::authentication("OpenAI API authentication is unavailable")
            })?;
            run_production_headless_chat_with_provider(
                request,
                bootstrap,
                cancellation,
                progress,
                true,
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
                    })
                    .map_err(|_| {
                        CliError::authentication("OpenAI API authentication is unavailable")
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
                bootstrap,
                cancellation,
                progress,
                false,
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
                    })
                    .map_err(|_| {
                        CliError::authentication("ChatGPT credentials are unavailable or invalid")
                    })
                },
            )
        }
        _ => Err(CliError::configuration(
            "headless chat requires provider.type = \"openai-api\" or \"openai-chatgpt\"",
        )),
    }
}

fn run_production_headless_chat_with_provider<P>(
    request: HeadlessChatRequest,
    bootstrap: &Bootstrap,
    cancellation: &HeadlessTurnCancellation,
    progress: Option<&TurnProgressSink>,
    include_system_prompt: bool,
    build_provider: impl FnOnce(
        String,
        Vec<Message>,
        Vec<OpenAiFunctionTool>,
        agens_core::RequestConfig,
    ) -> Result<P, CliError>,
) -> Result<HeadlessChatCompletion, CliError>
where
    P: ProgressAwareProvider,
{
    let model = request
        .model
        .clone()
        .or_else(|| bootstrap.model().map(ToOwned::to_owned))
        .unwrap_or_else(|| match bootstrap.provider_type() {
            Some("openai-chatgpt") => "gpt-5.5".to_owned(),
            _ => "gpt-4.1".to_owned(),
        });
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    let (provider_tools, tool_runtime) =
        production_tool_runtime(bootstrap, project_root, request.skills.as_deref())?;
    let project = project_root.display().to_string();
    let policy = permission_policy(
        bootstrap.permission_rules(),
        &project,
        request.mode,
        &tool_runtime,
        request.effective_capabilities.as_ref(),
    )?;
    let grant_store = PermissionGrantStore::open(bootstrap.data_directory())
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
    let mut provider = build_provider(
        model,
        provider_messages(&request, include_system_prompt),
        provider_tools,
        request.request_config.clone(),
    )?;
    if let Some(progress) = progress {
        provider = provider.with_progress_sink(Arc::clone(progress));
    }
    cancellation_result(cancellation)?;
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
        TtyPermissionPrompter,
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
    let snapshot = match request.max_iterations.or(bootstrap.max_iterations) {
        Some(max_iterations) => {
            block_on_headless_turn(run_headless_turn_with_max_iterations_and_progress(
                &mut provider,
                &mut gate,
                &mut resolver,
                &mut dispatcher,
                &mut repository,
                cancellation,
                max_iterations,
                progress,
            ))
        }
        None => block_on_headless_turn(agens_core::run_headless_turn_with_progress(
            &mut provider,
            &mut gate,
            &mut resolver,
            &mut dispatcher,
            &mut repository,
            cancellation,
            progress,
        )),
    }?
    .map_err(CliError::runtime)?;

    let turn = completed_session_turn(
        &request.prompt,
        &snapshot,
        request.pending_system_reminder.as_deref(),
    )?;
    let mut store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let metadata = next_session_metadata(
        bootstrap,
        &request.prompt,
        request.session.as_ref(),
        request.active_agent.as_deref(),
    )?;
    let metadata = store
        .persist_completed_session_turn(&metadata, &turn)
        .map_err(|_| CliError::storage("completed session could not be saved"))?;

    let text = snapshot
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
            metadata,
        })
    } else {
        Ok(HeadlessChatCompletion { text, metadata })
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
) -> Result<SessionMetadata, CliError> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| CliError::storage("session clock is unavailable"))?
        .as_secs() as i64;

    if let Some(metadata) = resumed {
        return Ok(SessionMetadata {
            updated_at: timestamp,
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
        project_root,
        skills,
        &mut dispatcher,
        &mut provider_tools,
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

fn register_production_task_tool(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    dispatcher: &mut ToolDispatcher,
    provider_tools: &mut BTreeMap<String, OpenAiFunctionTool>,
) -> Result<(), CliError> {
    let validator = BundledModelValidator;
    let agents = tui_agent_catalog(bootstrap, &validator)?;
    if !agents
        .subagents()
        .any(|agent| agent.mode == agens_core::AgentMode::Subagent)
    {
        return Ok(());
    }

    let parent_model = bootstrap
        .model()
        .unwrap_or_else(|| default_model(bootstrap))
        .to_owned();
    let task = TaskTool::from_catalogs_with_model_validator(
        agents,
        skills.clone(),
        parent_model,
        validator,
        ProductionTaskRunner {
            bootstrap: bootstrap.clone(),
            project_root: project_root.to_path_buf(),
        },
    );

    provider_tools.insert(
        "task".into(),
        OpenAiFunctionTool::new(
            "task",
            "Run an isolated subagent task",
            TaskTool::<ProductionTaskRunner>::input_schema(),
        )
        .map_err(|_| CliError::configuration("task tool is unavailable"))?,
    );
    dispatcher
        .register_native("native::task", agens_core::ToolAccess::Write, task)
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
}

impl TaskRunner for ProductionTaskRunner {
    fn run(
        &mut self,
        request: TaskTurnRequest,
        context: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        let cancellation = HeadlessTurnCancellation::with_cancellation_and_deadline(
            Arc::clone(&context.cancellation),
            Some(context.deadline),
        );
        run_production_task(request, &self.bootstrap, &self.project_root, &cancellation).map(
            |output| TaskTurnResult {
                output,
                iterations: 1,
            },
        )
    }
}

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

fn run_production_task(
    request: TaskTurnRequest,
    bootstrap: &Bootstrap,
    project_root: &Path,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, TaskRunnerError> {
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
    let (provider_tools, tool_runtime) = production_read_only_tool_runtime(project_root)
        .map_err(|_| TaskRunnerError::ChildFailure)?;

    match bootstrap.provider_type() {
        Some("openai-api") => {
            let api_key = bootstrap
                .openai_api_key
                .clone()
                .ok_or(TaskRunnerError::ChildFailure)?;
            let provider =
                OpenAiResponsesProvider::from_api_key_with_messages_and_tools_and_timeout(
                    api_key,
                    bootstrap.provider_base_url(),
                    request.model().to_owned(),
                    messages,
                    provider_tools,
                    std::time::Duration::from_secs(120),
                )
                .map(|provider| provider.with_parallel_tool_calls(bootstrap.parallel_tool_calls))
                .map_err(|_| TaskRunnerError::ChildFailure)?;
            run_isolated_task_turn(provider, tool_runtime, project_root, cancellation)
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
            .map(|provider| provider.with_parallel_tool_calls(bootstrap.parallel_tool_calls))
            .map_err(|_| TaskRunnerError::ChildFailure)?;
            run_isolated_task_turn(provider, tool_runtime, project_root, cancellation)
        }
        _ => Err(TaskRunnerError::ChildFailure),
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

fn run_isolated_task_turn<P>(
    mut provider: P,
    tool_runtime: SharedToolDispatcher,
    project_root: &Path,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, TaskRunnerError>
where
    P: ProgressAwareProvider,
{
    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Allow,
            PermissionPattern::Exact("native::read".into()),
            PermissionPattern::Any,
        )],
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
    );
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
        None,
    ))
    .map_err(|_| TaskRunnerError::ChildFailure)?
    .map_err(map_task_turn_error)?;

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
    let mut registry = McpRegistry::new();

    for server in &bootstrap.mcp_servers {
        if server.disabled {
            continue;
        }
        let timeout = std::time::Duration::from_millis(server.timeout_ms);
        let Ok(timeouts) = McpTimeouts::new(timeout, timeout, timeout) else {
            continue;
        };

        let server = server.clone();
        let name = server.name.clone();
        let project_root = project_root.to_path_buf();
        let _ = registry.configure_server(
            &name,
            move || configured_mcp_transport(&server, &project_root),
            timeouts,
            McpLimits::default(),
        );
    }

    registry
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

struct RegisteredNativeTool {
    name: String,
    catalog: Arc<Mutex<NativeToolCatalog>>,
}

impl DispatchTool for RegisteredNativeTool {
    fn permission_target(
        &self,
        arguments: &serde_json::Value,
    ) -> Result<String, agens_core::Error> {
        let field = if self.name == "native::bash" {
            "command"
        } else {
            "path"
        };
        arguments
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| agens_core::Error::Tool("native tool arguments are invalid".into()))
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
        }
    }
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
                            .evaluate(
                                &self.policy,
                                &grants,
                                &self.session,
                                ToolDispatchRequest::new(
                                    &self.project,
                                    &call.name,
                                    parse_tool_input(call)?,
                                ),
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
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError>;
}

struct TtyPermissionPrompter;

impl PermissionPrompter for TtyPermissionPrompter {
    fn prompt(
        &mut self,
        context: &PermissionPromptContext,
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
            let answer = self.prompt.prompt(&context)?;

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
            .and_then(|mut allowed| allowed.remove(&call.id).ok_or(HeadlessTurnPortError::Tool));
        let output = allowed
            .and_then(|allowed| {
                if allowed.name != call.name || allowed.input != call.input {
                    return Err(HeadlessTurnPortError::Tool);
                }
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
                    "tool execution failed".to_owned()
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

fn resolve_current_auto_provider(bootstrap: &Bootstrap) -> Result<Option<String>, CliError> {
    let credentials = match fs::read_to_string(&bootstrap.paths.credentials) {
        Ok(credentials) => Some(credentials),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(CliError::storage("ChatGPT credentials are unavailable")),
    };
    let environment = bootstrap
        .openai_api_key
        .as_ref()
        .map(|key| BTreeMap::from([("OPENAI_API_KEY".into(), key.clone())]))
        .unwrap_or_default();
    Ok(resolve_provider_type(
        None,
        credentials.as_deref(),
        &environment,
    ))
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

        let busy = rotate_active_agent(
            &mut context,
            &reviewer,
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
            true,
        );
        assert_eq!(busy, Err(AgentRotationError::Busy));
        assert_eq!(context, original);
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
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
            false,
        );
        assert_eq!(rollback, Err(AgentRotationError::Persistence));
        assert_eq!(context, original);

        context.metadata = Some(conflicting);
        rotate_active_agent(
            &mut context,
            &reviewer,
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
            false,
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
            request_config: agens_core::RequestConfig::default(),
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
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
            false,
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
            "project",
            &dispatcher,
            &BundledModelValidator,
            None,
            false,
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
                }),
                true,
            )
            .unwrap(),
            "answer"
        );
        assert_eq!(context.metadata, Some(metadata));
        assert!(context.pending_system_reminder.is_none());

        context.pending_system_reminder = Some("reminder".into());
        assert!(complete_tui_turn(&mut context, Err(CliError::storage("failed")), true).is_err());
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
                    input_price: None,
                    output_price: Some(0.6),
                },
                crate::model_registry::ModelMetadata {
                    id: "known".to_owned(),
                    name: Some("Known".to_owned()),
                    context: Some(128000),
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

        let resumed = resume_tui_session(&bootstrap, current.id, &SkillCatalog::default()).unwrap();
        assert_eq!(resumed.identifier, Some(current.id));
        assert_eq!(resumed.metadata, Some(current));
        assert_eq!(resumed.messages, tui_session_messages());
        assert_eq!(
            resumed
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );

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
            "Subagent: none. Available: reviewer."
        );

        let no_agents_temporary = tui_session_directory("no-agent-selectors");
        let no_subagents = tui_session_bootstrap(&no_agents_temporary, &[]);
        assert_eq!(
            list_tui_agents(&no_subagents, &session, AgentMode::Subagent).unwrap(),
            "Subagent: none."
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
        assert!(matches!(
            tui.transcript(),
            [agens_tui::TranscriptEntry::Error(_)]
        ));

        session.lock().unwrap().running = true;
        let input = enter_tui_input(&mut tui, "/new");
        tui.apply_submission_outcome(router.route(input));
        assert_eq!(tui.view().conversation.unwrap().errors.len(), 2);

        session.lock().unwrap().running = false;
        let input = enter_tui_input(&mut tui, "/new");
        tui.apply_submission_outcome(router.route(input));
        assert_eq!(
            tui.transcript(),
            [agens_tui::TranscriptEntry::Info(
                "Started a new session.".into()
            )]
        );

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
        assert!(matches!(
            tui.transcript().last(),
            Some(agens_tui::TranscriptEntry::Error(_))
        ));
        assert!(session.lock().unwrap().messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_startup_skills_reach_parent_context_and_tool_without_subagents() {
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
        assert!(!tools.iter().any(|tool| tool.name() == "task"));
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
        assert!(matches!(
            tui.transcript().last(),
            Some(agens_tui::TranscriptEntry::Error(_))
        ));
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
                "new",
                "sessions",
                "resume",
                "agent",
                "model",
                "effort",
                "help",
                "quit",
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
        assert!(!entries.iter().any(|entry| entry.name() == "subagent"));
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

        let sessions = enter_tui_input(&mut tui, "/sessions");
        assert!(
            tui.apply_submission_outcome(router.route(sessions))
                .is_none()
        );
        let help = enter_tui_input(&mut tui, "/h");
        assert!(tui.apply_submission_outcome(router.route(help)).is_none());
        let help = tui.transcript().last().unwrap();
        assert!(
            matches!(help, agens_tui::TranscriptEntry::Info(text) if text.contains("/connect") && text.contains("/review") && text.contains("/inspect") && !text.contains("/subagent"))
        );

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

        for command in ["/connect", "/connect --device-auth"] {
            assert!(matches!(
                router.route_with_progress(command.into(), progress_tx.clone()),
                TuiSubmissionOutcome::LocalInfo(_)
            ));
        }
        assert_eq!(progress_rx.try_iter().count(), 2);
        assert_eq!(
            *flows.lock().unwrap(),
            vec![ChatGptAuthFlow::Browser, ChatGptAuthFlow::Device]
        );
        assert_eq!(*session.lock().unwrap(), TuiSessionContext::fresh());
        assert!(router.bootstrap().unwrap().provider_type() == Some("openai-chatgpt"));
        let connected = std::fs::read_to_string(&credentials_path).unwrap();
        assert!(connected.contains("new-access"));

        assert!(matches!(
            router.route("/disconnect".into()),
            TuiSubmissionOutcome::LocalInfo(_)
        ));
        assert!(router.bootstrap().unwrap().provider_type() == Some("openai-api"));
        let stored = std::fs::read_to_string(&credentials_path).unwrap();
        assert!(stored.contains("preserved"));
        assert!(stored.contains("kept"));
        assert!(!stored.contains("new-access"));

        for (source, provider) in [
            (ProviderSource::ExplicitChatGpt, "openai-chatgpt"),
            (ProviderSource::ExplicitOther, "openai-api"),
            (ProviderSource::ExplicitOther, "unrelated"),
        ] {
            let mut bootstrap = router.bootstrap.lock().unwrap();
            bootstrap.provider_source = source;
            bootstrap.provider_type = Some(provider.into());
            drop(bootstrap);
            router.reconcile_provider(true).unwrap();
            router.reconcile_provider(false).unwrap();
            assert_eq!(router.bootstrap().unwrap().provider_type(), Some(provider));
        }
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
                            created_at: 1,
                            updated_at: 1,
                            completed_turn_count: 1,
                            resumable: true,
                        },
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
                    "[provider]\ntype = \"openai-api\"\nmodel = \"gpt-4.1\"\n\n[options]\ndata_dir = \"{}\"\n",
                    data_directory.display()
                ),
            )]),
        ))
        .unwrap()
    }

    fn tui_session_messages() -> Vec<Message> {
        vec![Message {
            role: Role::User,
            parts: vec![MessagePart::Text("previous request".into())],
        }]
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
                    created_at: 1,
                    updated_at: 1,
                    completed_turn_count: 0,
                    resumable: false,
                },
                &turn,
            )
            .unwrap()
    }

    #[test]
    fn resumed_tui_session_preserves_typed_history_for_the_next_prompt() {
        let metadata = SessionMetadata {
            id: 7,
            project: "project".into(),
            title: "conversation".into(),
            active_agent: "primary".into(),
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
                request_config: agens_core::RequestConfig::default(),
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
            created_at: 10,
            updated_at: 10,
            completed_turn_count: 0,
            resumable: false,
        };
        SessionStore::open(&data_directory)
            .expect("session store should open")
            .persist_completed_session_turn(&metadata, &initial_turn)
            .expect("normalized session should persist");

        let mut request = resume_tui_session(&bootstrap, 1, &SkillCatalog::default())
            .expect("normalized session should resume")
            .apply_to(HeadlessChatRequest {
                prompt: "second input".into(),
                history: Vec::new(),
                model: None,
                system_prompt: None,
                max_iterations: None,
                mode: PermissionMode::Edit,
                dangerously_allow_all: false,
                request_config: agens_core::RequestConfig::default(),
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
            request_config: agens_core::RequestConfig::default(),
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
            let request = run_tui_model_effort_provider_case(provider_type);

            assert_eq!(request["model"], "o3", "{provider_type}");
            assert!(
                !request.to_string().contains("gpt-4.1"),
                "{provider_type} request retained the replaced model: {request}"
            );

            let reasoning = &request["reasoning"];
            assert_eq!(reasoning["effort"], "high", "{provider_type}");
            assert!(
                !reasoning.to_string().contains("low"),
                "{provider_type} request retained the replaced effort: {reasoning}"
            );
        }
    }

    fn run_tui_model_effort_provider_case(provider_type: &str) -> serde_json::Value {
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

        for (command, expected) in [
            ("/model gpt-4.1", "Model: gpt-4.1."),
            ("/effort low", "Reasoning effort: low."),
            ("/model o3", "Model: o3."),
            ("/effort high", "Reasoning effort: high."),
        ] {
            assert_eq!(
                run_tui_prompt(&bootstrap, command, &cancellation, &session, None)
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
            "config: model is unavailable"
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
        assert_eq!(
            run_tui_prompt(&bootstrap, "next request", &cancellation, &session, None)
                .expect("selected prompt should complete"),
            "selected answer"
        );

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
        ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
            self.calls
                .lock()
                .expect("prompt calls should be available")
                .push(context.target_identifier.clone());
            Ok(self.answers.remove(0))
        }
    }

    struct BatchTool {
        calls: Arc<Mutex<Vec<String>>>,
        cancellation: Option<HeadlessTurnCancellation>,
    }

    impl DispatchTool for BatchTool {
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
            arguments: serde_json::Value,
        ) -> Result<ToolOutput, agens_core::Error> {
            let path = self.permission_target(&arguments)?;
            self.calls
                .lock()
                .expect("tool calls should be available")
                .push(path.clone());
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
        let directory =
            std::env::temp_dir().join(format!("agens-{directory_name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let executions = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
        dispatcher
            .lock()
            .expect("dispatcher should be available")
            .register_native(
                "native::read",
                agens_core::ToolAccess::ReadOnly,
                BatchTool {
                    calls: Arc::clone(&executions),
                    cancellation: cancellation.clone(),
                },
            )
            .expect("batch tool should register");
        let grants = Arc::new(Mutex::new(Vec::new()));
        let allowed = Arc::new(Mutex::new(BTreeMap::new()));
        let pending_prompts = Arc::new(Mutex::new(BTreeMap::new()));
        let policy = batch_policy();
        let mut gate = ProductionPermissionGate::new(
            policy.clone(),
            Arc::clone(&grants),
            PermissionSession::new(),
            "project".into(),
            Arc::clone(&dispatcher),
            Arc::clone(&allowed),
            Arc::clone(&pending_prompts),
        );
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
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation);

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
                (_, agens_tui::TuiRuntimeEvent::ToolStarted { call_id, name, input }),
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
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation);

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
    fn production_prompt_decisions_authorize_only_allowed_calls() {
        struct FixedPrompt(PermissionPromptAnswer);

        impl PermissionPrompter for FixedPrompt {
            fn prompt(
                &mut self,
                _: &PermissionPromptContext,
            ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
                Ok(self.0)
            }
        }

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
            let mut resolver = ProductionPermissionResolver::new(
                FixedPrompt(answer),
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

            match answer {
                PermissionPromptAnswer::AllowOnce | PermissionPromptAnswer::AllowAlways => {
                    assert_eq!(decision, Ok(PermissionDecision::Allow));
                    assert_eq!(
                        run_ready(production_dispatcher.dispatch(call.clone(), &cancellation)),
                        Ok(HeadlessToolOutput::success("executed"))
                    );
                    assert_eq!(
                        run_ready(production_dispatcher.dispatch(call.clone(), &cancellation)),
                        Err(HeadlessTurnPortError::Tool)
                    );
                    let changed_call = HeadlessToolCall {
                        input: r#"{"path":"changed.md"}"#.into(),
                        ..call.clone()
                    };
                    assert_eq!(
                        run_ready(production_dispatcher.dispatch(changed_call, &cancellation)),
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
