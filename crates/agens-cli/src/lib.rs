use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agens_config::{
    ConfigPaths, ConfigPermissionDecision, ConfigPermissionRule, ConfigPermissionScope,
    McpTransport, expand_environment, expand_environment_with_commands, extract_permission_rules,
    mcp_servers, merge_toml_documents, parse_toml_document, resolve_paths, validate_toml_document,
};
use agens_core::{
    CompletedTurnSnapshot, HeadlessPermissionGate, HeadlessPermissionResolver, HeadlessToolCall,
    HeadlessToolDispatcher, HeadlessToolOutput, HeadlessTurnCancellation, HeadlessTurnError,
    HeadlessTurnPortError, MessagePart, PermissionDecision, PermissionMode, PermissionPattern,
    PermissionPolicy, PermissionRule, PermissionSession, TurnEvent, TurnProgressSink,
    run_headless_turn_with_max_iterations_and_progress,
};
use agens_providers::chatgpt_login::{
    ChatGptDeviceCodeLoginOptions, ChatGptLoginOptions, LoginCancellation,
    device_code_login_with_progress, login, remove_provider_entry, upsert_chatgpt_credentials,
};
use agens_providers::{
    ChatGptAuthState, ChatGptResponsesProvider, OpenAiFunctionTool, OpenAiResponsesProvider,
    ProgressAwareProvider, load_chatgpt_auth_state,
};
use agens_store::{PermissionGrantStore, SessionStore};
use agens_tools::{
    AuthorizedToolCall, DispatchTool, McpHttpTransport, McpInitialize, McpLimits, McpRegistry,
    McpStdioTransport, McpStdioTransportConfig, McpTimeouts, NativeToolCatalog, NativeTools,
    RemoteToolMetadata, ToolDispatchRequest, ToolDispatcher, ToolEvaluationOutcome,
    ToolExecutionContext, ToolOutput,
};
use agens_tui::{Engine as TuiEngine, Tui, run_with_default_progress_submit};

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
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub max_iterations: Option<usize>,
    pub mode: PermissionMode,
    pub dangerously_allow_all: bool,
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
        [command] if command == "login" => run_auth_login(dependencies, false),
        [command, flag] if command == "login" && flag == "--device-auth" => {
            run_auth_login(dependencies, true)
        }
        [command, subcommand, ..] if command == "login" && subcommand == "api-key" => {
            Err(CliError::unavailable(UNAVAILABLE_MESSAGE))
        }
        [command, provider] if command == "logout" => {
            let bootstrap = bootstrap(dependencies)?;
            let removed =
                remove_provider_entry(&bootstrap.paths.credentials, provider).map_err(|_| {
                    CliError::authentication("ChatGPT credentials are unavailable or invalid")
                })?;
            if removed {
                Ok(format!("Logged out of {provider}.\n"))
            } else {
                Ok(format!("No credentials stored for {provider}.\n"))
            }
        }
        _ => Err(CliError::usage("auth requires status, login, or logout")),
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
        [] => Err(CliError::unavailable(UNAVAILABLE_MESSAGE)),
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
                .list_completed_turns()
                .map_err(|_| CliError::storage("saved sessions could not be listed"))?;

            if sessions.is_empty() {
                return Ok("No saved sessions.\n".to_owned());
            }

            let rows = sessions
                .iter()
                .map(|session| {
                    format!(
                        "{}\t{} event(s)",
                        session.id,
                        session.snapshot.events().len()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(format!("ID\tEVENTS\n{rows}\n"))
        }
        [command, identifier] if command == "show" => {
            let identifier = identifier
                .parse::<i64>()
                .map_err(|_| CliError::usage("sessions show requires a numeric id"))?;
            let bootstrap = bootstrap(dependencies)?;
            let store = SessionStore::open(&bootstrap.data_directory)
                .map_err(|_| CliError::storage("sessions database is unavailable"))?;
            let snapshot = store
                .load_completed_turn_for_resume(identifier)
                .map_err(|_| CliError::storage("saved session is unavailable"))?;
            Ok(format!(
                "Session {identifier}: {} event(s)\n",
                snapshot.events().len()
            ))
        }
        [command, ..] if command == "rm" => Err(CliError::unavailable(UNAVAILABLE_MESSAGE)),
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

#[derive(Clone, Default)]
struct TuiSessionContext {
    identifier: Option<i64>,
    snapshot: Option<CompletedTurnSnapshot>,
}

impl TuiSessionContext {
    fn fresh() -> Self {
        Self::default()
    }

    fn resumed(identifier: i64, snapshot: CompletedTurnSnapshot) -> Self {
        Self {
            identifier: Some(identifier),
            snapshot: Some(snapshot),
        }
    }

    fn note(&self) -> String {
        let identifier = self
            .identifier
            .expect("resumed TUI session context always has an identifier");
        let events = self
            .snapshot
            .as_ref()
            .expect("resumed TUI session context always has a snapshot")
            .events()
            .len();
        format!("Resumed session {identifier}: {events} event(s)")
    }

    fn apply_to(&self, mut request: HeadlessChatRequest) -> HeadlessChatRequest {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return request;
        };
        let Some(identifier) = self.identifier else {
            return request;
        };
        let context = snapshot
            .events()
            .iter()
            .filter_map(|event| match event {
                TurnEvent::ProviderPart(MessagePart::Text(text)) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();

        if !context.is_empty() {
            request.prompt = format!(
                "Resumed session {identifier} context:\n{context}\n\nUser: {}",
                request.prompt
            );
        }

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

    if let Some(identifier) = resume {
        let resumed = resume_tui_session(bootstrap, identifier)?;
        tui.add_info(resumed.note());
        *session.lock().map_err(|_| {
            CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
        })? = resumed;
    }

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
        prompt => {
            let request = session
                .lock()
                .map_err(|_| {
                    CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable")
                })?
                .apply_to(HeadlessChatRequest {
                    prompt: prompt.to_owned(),
                    model: None,
                    system_prompt: None,
                    max_iterations: None,
                    mode: PermissionMode::Edit,
                    dangerously_allow_all: false,
                });
            run_production_headless_chat_with_progress(request, bootstrap, cancellation, progress)
        }
    }
}

fn list_tui_sessions(bootstrap: &Bootstrap) -> Result<String, CliError> {
    let store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let sessions = store
        .list_completed_turns()
        .map_err(|_| CliError::storage("saved sessions could not be listed"))?;

    if sessions.is_empty() {
        return Ok("No saved sessions.".to_owned());
    }

    Ok(sessions
        .iter()
        .map(|session| {
            format!(
                "{}\t{} event(s)",
                session.id,
                session.snapshot.events().len()
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn resume_tui_session(
    bootstrap: &Bootstrap,
    identifier: i64,
) -> Result<TuiSessionContext, CliError> {
    let store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let snapshot = store
        .load_completed_turn_for_resume(identifier)
        .map_err(|_| CliError::storage("saved session is unavailable"))?;
    Ok(TuiSessionContext::resumed(identifier, snapshot))
}

fn parse_chat_request(arguments: &[String]) -> Result<HeadlessChatRequest, CliError> {
    let mut request = HeadlessChatRequest {
        prompt: String::new(),
        model: None,
        system_prompt: None,
        max_iterations: None,
        mode: PermissionMode::Edit,
        dangerously_allow_all: false,
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
            .filter(|server| server.transport == McpTransport::Stdio)
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
}

fn run_production_headless_chat_with_progress(
    request: HeadlessChatRequest,
    bootstrap: &Bootstrap,
    cancellation: &HeadlessTurnCancellation,
    progress: Option<&TurnProgressSink>,
) -> Result<String, CliError> {
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
                move |model, prompt, tools| {
                    OpenAiResponsesProvider::from_api_key_with_tools_and_timeout(
                        api_key,
                        bootstrap.provider_base_url(),
                        model,
                        prompt,
                        tools,
                        std::time::Duration::from_secs(120),
                    )
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
                move |model, prompt, tools| {
                    ChatGptResponsesProvider::from_credentials_with_tools_and_timeout_and_auth_url(
                        &credentials_path,
                        bootstrap.provider_base_url(),
                        None,
                        model,
                        instructions,
                        prompt,
                        tools,
                        std::time::Duration::from_secs(120),
                    )
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
    build_provider: impl FnOnce(String, String, Vec<OpenAiFunctionTool>) -> Result<P, CliError>,
) -> Result<String, CliError>
where
    P: ProgressAwareProvider,
{
    let model = request
        .model
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
    let policy = permission_policy(bootstrap.permission_rules(), &project, request.mode)?;
    let grant_store = PermissionGrantStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("permission grants are unavailable"))?;
    let grants = grant_store
        .grants_for_project(&project)
        .map_err(|_| CliError::storage("permission grants are unavailable"))?;
    let session = if request.dangerously_allow_all {
        PermissionSession::with_temporary_bypass()
    } else {
        PermissionSession::new()
    };
    let pending = Arc::new(Mutex::new(BTreeMap::new()));
    let mut provider = build_provider(model, request.prompt, provider_tools)?;
    if let Some(progress) = progress {
        provider = provider.with_progress_sink(Arc::clone(progress));
    }
    cancellation_result(cancellation)?;
    let mut store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let mut gate = ProductionPermissionGate::new(
        policy,
        grants,
        session,
        project,
        Arc::clone(&tool_runtime),
        Arc::clone(&pending),
    );
    let mut resolver = ProductionPermissionResolver;
    let mut dispatcher = ProductionToolDispatcher::new(tool_runtime, pending);
    let snapshot = match request.max_iterations.or(bootstrap.max_iterations) {
        Some(max_iterations) => {
            block_on_headless_turn(run_headless_turn_with_max_iterations_and_progress(
                &mut provider,
                &mut gate,
                &mut resolver,
                &mut dispatcher,
                &mut store,
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
            &mut store,
            cancellation,
            progress,
        )),
    }?
    .map_err(CliError::runtime)?;

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
        Ok("completed".to_owned())
    } else {
        Ok(text)
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
    let remote_tools = mcp_registry
        .lock()
        .map_err(|_| CliError::configuration("MCP tools are unavailable"))?
        .tools()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let mut dispatcher = ToolDispatcher::new();
    let mut provider_tools = Vec::new();

    for metadata in NativeToolCatalog::metadata() {
        provider_tools.push(
            OpenAiFunctionTool::new(
                metadata.qualified_name.clone(),
                metadata.description,
                metadata.input_schema,
            )
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

    for metadata in remote_tools {
        provider_tools.push(remote_function_tool(&metadata)?);
        dispatcher
            .register_mcp(
                &metadata,
                RegisteredMcpTool {
                    name: metadata.qualified_name.clone(),
                    registry: Arc::clone(&mcp_registry),
                },
            )
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    }

    Ok((provider_tools, Arc::new(Mutex::new(dispatcher))))
}

fn load_configured_mcp_registry(bootstrap: &Bootstrap, project_root: &Path) -> McpRegistry {
    let mut registry = McpRegistry::new();
    let cancellation = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let initialize = McpInitialize::new("2025-06-18", serde_json::json!({}), "agens", "0.1.0");

    for server in &bootstrap.mcp_servers {
        let timeout = std::time::Duration::from_millis(server.timeout_ms);
        let Ok(timeouts) = McpTimeouts::new(timeout, timeout, timeout) else {
            continue;
        };

        match server.transport {
            McpTransport::Stdio => {
                let Ok(transport) = McpStdioTransport::spawn(McpStdioTransportConfig {
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
                }) else {
                    continue;
                };
                let _ = registry.load_server(
                    &server.name,
                    transport,
                    &initialize,
                    timeouts,
                    McpLimits::default(),
                    Arc::clone(&cancellation),
                );
            }
            McpTransport::Http | McpTransport::Sse => {
                let Ok(transport) = McpHttpTransport::new(
                    server.url.clone().expect("HTTP MCP URLs are validated"),
                    server.headers.clone(),
                    server.max_retries,
                ) else {
                    continue;
                };
                let _ = registry.load_server(
                    &server.name,
                    transport,
                    &initialize,
                    timeouts,
                    McpLimits::default(),
                    Arc::clone(&cancellation),
                );
            }
        }
    }

    registry
}

fn remote_function_tool(metadata: &RemoteToolMetadata) -> Result<OpenAiFunctionTool, CliError> {
    OpenAiFunctionTool::new(
        metadata.qualified_name.clone(),
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

struct ProductionPermissionGate {
    policy: PermissionPolicy,
    grants: Vec<agens_core::ProjectPermissionGrant>,
    session: PermissionSession,
    project: String,
    dispatcher: SharedToolDispatcher,
    allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
}

impl ProductionPermissionGate {
    fn new(
        policy: PermissionPolicy,
        grants: Vec<agens_core::ProjectPermissionGrant>,
        session: PermissionSession,
        project: String,
        dispatcher: SharedToolDispatcher,
        allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
    ) -> Self {
        Self {
            policy,
            grants,
            session,
            project,
            dispatcher,
            allowed,
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
        let result = match self
            .dispatcher
            .lock()
            .map_err(|_| HeadlessTurnPortError::Permission)
            .and_then(|dispatcher| {
                dispatcher
                    .evaluate(
                        &self.policy,
                        &self.grants,
                        &self.session,
                        ToolDispatchRequest::new(
                            &self.project,
                            &call.name,
                            parse_tool_input(call)?,
                        ),
                    )
                    .map_err(|_| HeadlessTurnPortError::Permission)
            }) {
            Ok(ToolEvaluationOutcome::Authorized(handle)) => self
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
            Ok(ToolEvaluationOutcome::Denied) => Ok(PermissionDecision::Deny),
            Ok(ToolEvaluationOutcome::PromptRequired(_)) => Ok(PermissionDecision::Ask),
            Err(error) => Err(error),
        };
        std::future::ready(result)
    }
}

struct ProductionPermissionResolver;

impl HeadlessPermissionResolver for ProductionPermissionResolver {
    fn resolve(
        &mut self,
        _call: &HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send
    {
        std::future::ready(Ok(PermissionDecision::Ask))
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
) -> Result<PermissionPolicy, CliError> {
    let rules = rules
        .iter()
        .map(|rule| {
            let decision = match rule.decision {
                ConfigPermissionDecision::Allow => PermissionDecision::Allow,
                ConfigPermissionDecision::Deny => PermissionDecision::Deny,
            };
            let tool = PermissionPattern::Exact(configured_tool_name(&rule.tool_pattern)?);
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
    Ok(PermissionPolicy::new(mode, rules))
}

fn configured_tool_name(name: &str) -> Result<String, CliError> {
    match name {
        "read" => Ok("native::read".to_owned()),
        "write" | "edit" => Ok("native::write".to_owned()),
        "list" => Ok("native::list".to_owned()),
        "search" => Ok("native::search".to_owned()),
        "bash" => Ok("native::bash".to_owned()),
        name if name.contains('_') => {
            let (server, tool) = name
                .split_once('_')
                .expect("MCP permission name was validated by configuration parsing");
            Ok(format!("{server}::{tool}"))
        }
        _ => Err(CliError::configuration(
            "permission configuration is invalid",
        )),
    }
}

fn parse_tool_input(call: &HeadlessToolCall) -> Result<serde_json::Value, HeadlessTurnPortError> {
    serde_json::from_str(&call.input).map_err(|_| HeadlessTurnPortError::Permission)
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
    use agens_core::TurnState;

    #[test]
    fn resumed_tui_session_adds_restored_context_to_the_next_prompt() {
        let snapshot = CompletedTurnSnapshot::from_persisted_events(vec![
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("previous answer".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ])
        .expect("completed turn snapshot should be valid");

        let request = TuiSessionContext::resumed(7, snapshot).apply_to(HeadlessChatRequest {
            prompt: "next question".into(),
            model: None,
            system_prompt: None,
            max_iterations: None,
            mode: PermissionMode::Edit,
            dangerously_allow_all: false,
        });

        assert_eq!(
            request.prompt,
            "Resumed session 7 context:\nprevious answer\n\nUser: next question"
        );
        assert_eq!(request.system_prompt, None);
    }

    #[test]
    fn fresh_tui_session_does_not_reuse_prior_context() {
        let request = TuiSessionContext::fresh().apply_to(HeadlessChatRequest {
            prompt: "new question".into(),
            model: None,
            system_prompt: None,
            max_iterations: None,
            mode: PermissionMode::Edit,
            dangerously_allow_all: false,
        });

        assert_eq!(request.system_prompt, None);
    }
}
