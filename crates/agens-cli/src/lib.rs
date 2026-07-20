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
    SessionMetadata, TurnEvent, TurnProgressSink,
    run_headless_turn_with_max_iterations_and_progress,
};
use agens_providers::chatgpt_login::{
    ChatGptDeviceCodeLoginOptions, ChatGptLoginOptions, LoginCancellation,
    device_code_login_with_progress, login, remove_provider_entry, upsert_chatgpt_credentials,
    upsert_provider_entry,
};
use agens_providers::{
    ChatGptAuthState, ChatGptResponsesProvider, OpenAiFunctionTool, OpenAiResponsesProvider,
    ProgressAwareProvider, load_chatgpt_auth_state,
};
use agens_store::{PermissionGrantStore, SessionStore};
use agens_tools::{
    AgentCatalog, AgentModelValidator, AuthorizedToolCall, DispatchTool, EffectiveCapabilitySet,
    McpHttpTransport, McpLimits, McpRegistry, McpSseTransport, McpStdioTransport,
    McpStdioTransportConfig, McpTimeouts, McpTransport as McpTransportPort, McpTransportError,
    NativeToolCatalog, NativeTools, PermissionPromptContext, RemoteToolMetadata, SkillCatalog,
    TaskRunContext, TaskRunner, TaskRunnerError, TaskTool, TaskTurnRequest, TaskTurnResult,
    ToolDispatchRequest, ToolDispatcher, ToolEvaluationOutcome, ToolExecutionContext, ToolOutput,
};
use agens_tui::{Engine as TuiEngine, Tui, run_with_default_progress_submit};

mod model_registry;

const UNAVAILABLE_MESSAGE: &str = "this command is not implemented yet";

type CurrentDirectory = Box<dyn Fn() -> Result<PathBuf, CliError>>;
type HomeDirectory = Box<dyn Fn() -> Option<PathBuf>>;
type Environment = Box<dyn Fn() -> BTreeMap<String, String>>;
type ConfigReader = Box<dyn Fn(&Path) -> Result<Option<String>, CliError>>;
type HeadlessChat = Box<
    dyn Fn(HeadlessChatRequest, &Bootstrap, &HeadlessTurnCancellation) -> Result<String, CliError>,
>;
type TuiLauncher = Box<dyn Fn(&Bootstrap, Option<i64>) -> Result<String, CliError>>;
type AuthLogin = Box<dyn Fn(&Path, bool) -> Result<String, CliError>>;

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
            auth_login: Box::new(|_, _| Err(CliError::unavailable(UNAVAILABLE_MESSAGE))),
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
        login: impl Fn(&Path, bool) -> Result<String, CliError> + 'static,
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
    session: Option<SessionMetadata>,
    active_agent: Option<String>,
    effective_capabilities: Option<EffectiveCapabilitySet>,
    pending_system_reminder: Option<String>,
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
        [command, rest @ ..] if command == "auth" => run_auth(rest, dependencies),
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

fn run_auth(arguments: &[String], dependencies: &CliDependencies) -> Result<String, CliError> {
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
        [command] if command == "login" => run_auth_login(dependencies, false),
        [command, flag] if command == "login" && flag == "--device-auth" => {
            run_auth_login(dependencies, true)
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

fn run_auth_login(dependencies: &CliDependencies, device_auth: bool) -> Result<String, CliError> {
    let bootstrap = bootstrap(dependencies)?;
    let mut output = (dependencies.auth_login)(&bootstrap.paths.credentials, device_auth)?;
    output.push_str("Logged in to ChatGPT.\n");
    Ok(output)
}

fn run_production_auth_login(path: &Path, device_auth: bool) -> Result<String, CliError> {
    let credentials = if device_auth {
        device_code_login_with_progress(
            ChatGptDeviceCodeLoginOptions::default(),
            LoginCancellation::new(),
            move |verification_url, user_code| {
                let _ = writeln!(
                    std::io::stdout(),
                    "Open {} and enter code {}.",
                    verification_url,
                    user_code
                );
                let _ = std::io::stdout().flush();
            },
        )
        .map(|result| result.credentials)
    } else {
        login(
            ChatGptLoginOptions::new(
                Arc::new(|url| {
                    std::process::Command::new("xdg-open")
                        .arg(url)
                        .spawn()
                        .map(|_| ())
                }),
                Arc::new(|url| {
                    let _ = writeln!(std::io::stdout(), "Open {url} to authenticate.");
                    let _ = std::io::stdout().flush();
                }),
            ),
            LoginCancellation::new(),
        )
    }
    .map_err(|_| CliError::authentication("ChatGPT login failed"))?;

    upsert_chatgpt_credentials(path, &credentials)
        .map_err(|_| CliError::authentication("ChatGPT credentials could not be saved"))?;
    Ok(String::new())
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TuiSessionContext {
    identifier: Option<i64>,
    metadata: Option<SessionMetadata>,
    messages: Vec<Message>,
    active_agent: Option<ActiveAgentRuntime>,
    pending_system_reminder: Option<String>,
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

fn run_production_tui(bootstrap: &Bootstrap, resume: Option<i64>) -> Result<String, CliError> {
    let cancellation = Arc::new(Mutex::new(None));
    let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
    let engine = ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    };
    let mut tui = Tui::new(engine);
    let provider = bootstrap.provider_type().unwrap_or("provider");
    let model = bootstrap.model().unwrap_or("default model");
    let mut session_label = "new session".to_owned();

    if let Some(identifier) = resume {
        let resumed = resume_tui_session(bootstrap, identifier)?;
        tui.add_info(resumed.note());
        *session.lock().map_err(|_| {
            CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
        })? = resumed;
        session_label = format!("session #{identifier}");
    }
    tui.set_presentation(provider, model, session_label);

    let bootstrap = bootstrap.clone();
    let session = Arc::clone(&session);
    run_with_default_progress_submit(&mut tui, move |prompt, progress| {
        let turn_cancellation =
            HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(120));
        let Ok(mut active) = cancellation.lock() else {
            return Err("runtime: TUI cancellation is unavailable".to_owned());
        };
        *active = Some(turn_cancellation.clone());
        drop(active);

        let sink: TurnProgressSink = Arc::new(move |event| {
            let _ = progress.send(event);
        });
        let result = run_tui_prompt(
            &bootstrap,
            &prompt,
            &turn_cancellation,
            &session,
            Some(&sink),
        )
        .map_err(|error| error.to_string());

        if let Ok(mut active) = cancellation.lock() {
            *active = None;
        }

        result
    })
    .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "terminal UI failed"))?;

    Ok(String::new())
}

fn run_tui_prompt(
    bootstrap: &Bootstrap,
    prompt: &str,
    cancellation: &HeadlessTurnCancellation,
    session: &Arc<Mutex<TuiSessionContext>>,
    progress: Option<&TurnProgressSink>,
) -> Result<String, CliError> {
    match prompt.trim() {
        "/sessions" => list_tui_sessions(bootstrap),
        "/new" => {
            *session.lock().map_err(|_| {
                CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
            })? = TuiSessionContext::fresh();
            Ok("Started a new session.".to_owned())
        }
        command if command.starts_with("/resume ") => {
            let identifier = command[8..]
                .trim()
                .parse::<i64>()
                .map_err(|_| CliError::usage("/resume requires a numeric session id"))?;
            let resumed = resume_tui_session(bootstrap, identifier)?;
            let note = resumed.note();
            *session.lock().map_err(|_| {
                CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
            })? = resumed;
            Ok(note)
        }
        command if command.starts_with("/agent ") => {
            rotate_tui_agent(bootstrap, &command[7..], session)
        }
        prompt => {
            let request = session
                .lock()
                .map_err(|_| {
                    CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
                })?
                .apply_to(HeadlessChatRequest {
                    prompt: prompt.to_owned(),
                    history: Vec::new(),
                    model: None,
                    system_prompt: None,
                    max_iterations: None,
                    mode: PermissionMode::Edit,
                    dangerously_allow_all: false,
                    session: None,
                    active_agent: None,
                    effective_capabilities: None,
                    pending_system_reminder: None,
                });
            let consumed_reminder = request.pending_system_reminder.is_some();
            let completion = run_production_headless_chat_with_progress(
                request,
                bootstrap,
                cancellation,
                progress,
            );
            let mut session = session.lock().map_err(|_| {
                CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
            })?;
            complete_tui_turn(&mut session, completion, consumed_reminder)
        }
    }
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

fn rotate_tui_agent(
    bootstrap: &Bootstrap,
    name: &str,
    session: &Arc<Mutex<TuiSessionContext>>,
) -> Result<String, CliError> {
    let validator = BundledModelValidator;
    let catalog = tui_agent_catalog(bootstrap, &validator)?;
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    let (_, dispatcher) = production_tool_runtime(bootstrap, project_root)?;
    let dispatcher = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is unavailable"))?;
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
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
    rotate_active_agent(
        &mut context,
        agent,
        &project_root.display().to_string(),
        &dispatcher,
        &validator,
        store.as_mut(),
        false,
    )
    .map_err(agent_rotation_error)?;
    Ok(format!("Active agent: {}.", agent.name))
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
    let store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let sessions = store
        .list_sessions()
        .map_err(|_| CliError::storage("saved sessions could not be listed"))?;

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
) -> Result<TuiSessionContext, CliError> {
    let store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let session = store
        .load_session_for_resume(identifier)
        .map_err(|_| CliError::storage("saved session is unavailable"))?;
    let active_agent = active_tui_agent_runtime(bootstrap, &session.metadata.active_agent)?;
    Ok(TuiSessionContext::resumed(
        identifier,
        session.metadata,
        session.messages,
        active_agent,
    ))
}

fn active_tui_agent_runtime(
    bootstrap: &Bootstrap,
    name: &str,
) -> Result<ActiveAgentRuntime, CliError> {
    let validator = BundledModelValidator;
    let catalog = tui_agent_catalog(bootstrap, &validator)?;
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    let (_, dispatcher) = production_tool_runtime(bootstrap, project_root)?;
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
        session: None,
        active_agent: None,
        effective_capabilities: None,
        pending_system_reminder: None,
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

pub struct Bootstrap {
    paths: ConfigPaths,
    global_loaded: bool,
    project_loaded: bool,
    model: Option<String>,
    provider_type: Option<String>,
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
    let provider_type = resolve_provider_type(
        string_value(&document, &["provider", "type"]),
        credentials.as_deref(),
        &environment,
    );
    Ok(Bootstrap {
        model: string_value(&document, &["provider", "model"]),
        provider_type,
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
                move |model, messages, tools| {
                    OpenAiResponsesProvider::from_api_key_with_messages_and_tools_and_timeout(
                        api_key,
                        bootstrap.provider_base_url(),
                        model,
                        messages,
                        tools,
                        std::time::Duration::from_secs(120),
                    )
                    .map(|provider| {
                        provider.with_parallel_tool_calls(bootstrap.parallel_tool_calls)
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
                move |model, messages, tools| {
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
                        provider.with_parallel_tool_calls(bootstrap.parallel_tool_calls)
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
    build_provider: impl FnOnce(String, Vec<Message>, Vec<OpenAiFunctionTool>) -> Result<P, CliError>,
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
    let (provider_tools, tool_runtime) = production_tool_runtime(bootstrap, project_root)?;
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
    let mut provider = build_provider(model, provider_messages(&request), provider_tools)?;
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

fn provider_messages(request: &HeadlessChatRequest) -> Vec<Message> {
    let mut messages = request.history.clone();
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
    for event in snapshot.events() {
        let (next_role, part) = match event {
            TurnEvent::ProviderPart(part) => (Role::Assistant, part),
            TurnEvent::ToolResult(part) => (Role::Tool, part),
            TurnEvent::StateChanged(_) | TurnEvent::ToolCallRequested { .. } => continue,
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

    register_production_task_tool(
        bootstrap,
        project_root,
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

    let global_skills = bootstrap.paths.global_config.with_file_name("skills");
    let project_skills = bootstrap.paths.project_config.with_file_name("skills");
    let skills = SkillCatalog::discover(global_skills, project_skills)
        .map_err(|_| CliError::configuration("skill catalog is unavailable"))?
        .catalog()
        .clone();
    let parent_model = bootstrap
        .model()
        .unwrap_or_else(|| default_model(bootstrap))
        .to_owned();
    let task = TaskTool::from_catalogs_with_model_validator(
        agents,
        skills,
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
        HeadlessTurnError::Provider => TaskRunnerError::ProviderFailure,
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
        let output = allowed.and_then(|allowed| {
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
                .map(|output| {
                    let content = if output.is_error {
                        "tool execution failed".to_owned()
                    } else {
                        output.content
                    };
                    HeadlessToolOutput {
                        content,
                        is_error: output.is_error,
                    }
                })
                .map_err(headless_tool_error)
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
        Error as ToolError, PermissionRule, ToolAccess, TurnProvider, TurnState,
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
            session: None,
            active_agent: None,
            effective_capabilities: None,
            pending_system_reminder: None,
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
            provider_messages(&request),
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
                session: None,
                active_agent: None,
                effective_capabilities: None,
                pending_system_reminder: None,
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

        let mut request = resume_tui_session(&bootstrap, 1)
            .expect("normalized session should resume")
            .apply_to(HeadlessChatRequest {
                prompt: "second input".into(),
                history: Vec::new(),
                model: None,
                system_prompt: None,
                max_iterations: None,
                mode: PermissionMode::Edit,
                dangerously_allow_all: false,
                session: None,
                active_agent: None,
                effective_capabilities: None,
                pending_system_reminder: None,
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
            session: None,
            active_agent: None,
            effective_capabilities: None,
            pending_system_reminder: None,
        });

        assert_eq!(request.system_prompt, None);
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

    #[derive(Default)]
    struct BatchRepository;

    impl CompletedTurnRepository for BatchRepository {
        fn persist_completed_turn(
            &mut self,
            _: CompletedTurnSnapshot,
        ) -> impl std::future::Future<Output = Result<(), agens_core::CompletedTurnStoreError>> + Send
        {
            std::future::ready(Ok(()))
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
            iterations: vec![Ok(calls), Ok(vec![MessagePart::Text("complete".into())])],
        };
        let progress_events = Arc::new(Mutex::new(Vec::new()));
        let progress: TurnProgressSink = {
            let progress_events = Arc::clone(&progress_events);
            Arc::new(move |event| progress_events.lock().unwrap().push(event))
        };
        let cancellation = cancellation.unwrap_or_default();
        let result = run_ready(agens_core::run_headless_turn_with_progress(
            &mut provider,
            &mut gate,
            &mut resolver,
            &mut tool_dispatcher,
            &mut BatchRepository,
            &cancellation,
            Some(&progress),
        ));
        std::fs::remove_dir_all(&directory).expect("temporary grant directory should be removed");

        BatchOutcome {
            result,
            prompts: prompts.lock().unwrap().clone(),
            executions: executions.lock().unwrap().clone(),
            progress: progress_events.lock().unwrap().clone(),
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
