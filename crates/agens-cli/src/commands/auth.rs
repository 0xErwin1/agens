//! The `auth` command: reports ChatGPT/OpenAI-API credential status, drives the
//! interactive and API-key login flows, and clears stored credentials on logout.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use agens_core::HeadlessTurnCancellation;
use agens_providers::chatgpt_login::{
    LoginCancellation, LoginError, remove_provider_entry, upsert_provider_entry,
};
use agens_providers::{ChatGptAuthState, load_chatgpt_auth_state};

use crate::CliDependencies;
use crate::cli;
use crate::deps::bootstrap;
use agens_auth::{ChatGptAuthCoordinator, ChatGptAuthFlow, ChatGptAuthProgress};
use agens_error::CliError;

pub(crate) fn run_auth(
    action: cli::AuthAction,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    match action {
        cli::AuthAction::Status { provider: None } => {
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
        cli::AuthAction::Status {
            provider: Some(provider),
        } => {
            let provider = CredentialProvider::parse(&provider)?;
            let bootstrap = bootstrap(dependencies)?;
            provider_status(&bootstrap.paths.credentials, provider)
        }
        cli::AuthAction::Login {
            device_auth: false,
            method: None,
        } => {
            if (dependencies.stdin_is_terminal)() {
                run_interactive_login(dependencies, cancellation)
            } else {
                Err(CliError::usage(login_methods()))
            }
        }
        cli::AuthAction::Login {
            device_auth: true,
            method: None,
        }
        | cli::AuthAction::Login {
            device_auth: true,
            method: Some(cli::LoginMethod::Chatgpt { .. }),
        } => run_auth_login(dependencies, true, cancellation),
        cli::AuthAction::Login {
            device_auth: false,
            method: Some(cli::LoginMethod::Chatgpt { device_auth }),
        } => run_auth_login(dependencies, device_auth, cancellation),
        cli::AuthAction::Login {
            device_auth: true,
            method: Some(cli::LoginMethod::ApiKey { .. }),
        } => {
            // clap parses `--device-auth` and `api-key ...` together
            // without complaint (a plain flag cannot `conflicts_with` a
            // nested subcommand in clap's grammar), so this guard is the
            // only thing standing between that combination and being
            // silently accepted with `--device-auth` quietly ignored.
            Err(CliError::usage("auth requires status, login, or logout"))
        }
        cli::AuthAction::Login {
            device_auth: false,
            method: Some(cli::LoginMethod::ApiKey { provider, api_key }),
        } => run_api_key_login(&provider, api_key, dependencies),
        cli::AuthAction::Logout { provider } => {
            let provider = CredentialProvider::parse(&provider)?;
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
    }
}

/// What `auth login` offers when it is not told which provider to use.
///
/// Naming one of them as the default would be a guess about which account the
/// user wants to spend, so the command lists them and stops instead.
fn login_methods() -> String {
    let mut message = String::from("auth login requires a provider:\n");
    for choice in LOGIN_CHOICES {
        message.push_str(&format!(
            "\n  {}\n      {}\n",
            choice.command(),
            choice.description()
        ));
    }
    message.trim_end().to_owned()
}

/// One flow a bare `auth login` can start, shared between the usage listing
/// and the interactive menu so both always offer the same providers in the
/// same order.
#[derive(Clone, Copy)]
enum LoginChoice {
    Chatgpt { device_auth: bool },
    ApiKey(CredentialProvider),
}

const LOGIN_CHOICES: [LoginChoice; 4] = [
    LoginChoice::Chatgpt { device_auth: false },
    LoginChoice::Chatgpt { device_auth: true },
    LoginChoice::ApiKey(CredentialProvider::OpenAiApi),
    LoginChoice::ApiKey(CredentialProvider::Moonshot),
];

impl LoginChoice {
    const fn command(self) -> &'static str {
        match self {
            Self::Chatgpt { device_auth: false } => "agens auth login chatgpt",
            Self::Chatgpt { device_auth: true } => "agens auth login chatgpt --device-auth",
            Self::ApiKey(CredentialProvider::OpenAiApi) => "agens auth login api-key openai-api",
            Self::ApiKey(_) => "agens auth login api-key moonshotai",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Chatgpt { device_auth: false } => {
                "ChatGPT subscription, through OAuth in a browser"
            }
            Self::Chatgpt { device_auth: true } => "ChatGPT subscription, through a device code",
            Self::ApiKey(CredentialProvider::OpenAiApi) => "OpenAI API key",
            Self::ApiKey(_) => "Moonshot AI (Kimi) API key",
        }
    }
}

/// Runs the interactive provider menu a bare `auth login` opens on a
/// terminal, then starts the chosen flow exactly as its explicit invocation
/// would — including the hidden API-key prompt for the api-key entries.
fn run_interactive_login(
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    let bootstrap = bootstrap(dependencies)?;
    let items = login_menu_items(&bootstrap.paths.credentials);

    let Some(index) = (dependencies.login_menu)(&items)? else {
        return Err(CliError::authentication("login was cancelled"));
    };

    match LOGIN_CHOICES.get(index) {
        Some(LoginChoice::Chatgpt { device_auth }) => {
            run_auth_login(dependencies, *device_auth, cancellation)
        }
        Some(LoginChoice::ApiKey(provider)) => {
            run_api_key_login(provider.identifier(), None, dependencies)
        }
        None => Err(CliError::authentication("login selection is invalid")),
    }
}

/// One menu line per [`LOGIN_CHOICES`] entry, marked with the same
/// credential status `auth status` reports for that provider.
fn login_menu_items(credentials: &Path) -> Vec<String> {
    let chatgpt_mark = match load_chatgpt_auth_state(credentials, std::time::SystemTime::now()) {
        Ok(ChatGptAuthState::Ready) => "ready",
        Ok(ChatGptAuthState::RefreshRequired) => "refresh required",
        Err(_) => "not logged in",
    };

    LOGIN_CHOICES
        .iter()
        .map(|choice| {
            let mark = match choice {
                LoginChoice::Chatgpt { .. } => chatgpt_mark,
                LoginChoice::ApiKey(provider) => {
                    if api_key_is_ready(credentials, *provider) {
                        "ready"
                    } else {
                        "not logged in"
                    }
                }
            };
            format!("{} ({mark})", choice.description())
        })
        .collect()
}

#[derive(Clone, Copy)]
enum CredentialProvider {
    OpenAiApi,
    OpenAiChatGpt,
    Moonshot,
}

impl CredentialProvider {
    fn parse(value: &str) -> Result<Self, CliError> {
        match value {
            "openai-api" => Ok(Self::OpenAiApi),
            "openai-chatgpt" => Ok(Self::OpenAiChatGpt),
            "moonshotai" => Ok(Self::Moonshot),
            _ => Err(CliError::usage("auth provider is unsupported")),
        }
    }

    const fn identifier(self) -> &'static str {
        match self {
            Self::OpenAiApi => "openai-api",
            Self::OpenAiChatGpt => "openai-chatgpt",
            Self::Moonshot => "moonshotai",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::OpenAiApi => "OpenAI API",
            Self::OpenAiChatGpt => "ChatGPT subscription",
            Self::Moonshot => "Moonshot AI",
        }
    }
}

fn run_api_key_login(
    provider: &str,
    api_key: Option<String>,
    dependencies: &CliDependencies,
) -> Result<String, CliError> {
    let provider = CredentialProvider::parse(provider)?;
    if matches!(provider, CredentialProvider::OpenAiChatGpt) {
        return Err(CliError::usage(
            "openai-chatgpt signs in through OAuth; run auth login instead",
        ));
    }

    let supplied_key = validate_api_key_flag(api_key)?;
    let api_key = read_api_key(supplied_key.as_deref(), dependencies)?;
    let bootstrap = bootstrap(dependencies)?;
    upsert_provider_entry(
        &bootstrap.paths.credentials,
        provider.identifier(),
        serde_json::json!({ "api_key": api_key }),
    )
    .map_err(|_| CliError::authentication("API-key credentials could not be saved"))?;

    Ok(format!("Logged in to {}.\n", provider.identifier()))
}

/// clap already owns whether `--api-key` was supplied at all (and rejects
/// any unrecognized trailing argument on its own); the only domain rule
/// left is that a supplied value cannot be blank.
fn validate_api_key_flag(api_key: Option<String>) -> Result<Option<String>, CliError> {
    match api_key {
        None => Ok(None),
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(CliError::usage(
                    "auth login api-key requires a non-empty API key",
                ));
            }
            Ok(Some(trimmed.to_owned()))
        }
    }
}

fn read_api_key(
    supplied_key: Option<&str>,
    dependencies: &CliDependencies,
) -> Result<String, CliError> {
    if (dependencies.stdin_is_terminal)() {
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

/// The largest key either input path accepts, so a stuck producer cannot grow
/// the buffer without bound.
const MAX_API_KEY_INPUT_BYTES: u64 = 8192;

/// Puts the terminal in raw mode — no echo, no line buffering, no kernel
/// signal generation — and restores the original attributes on drop.
///
/// Raw mode means the holder, not the kernel, owns interrupt handling. That
/// is deliberate: the process installs an async `ctrl_c` handler that
/// replaces SIGINT's default disposition, so a blocking line read would
/// swallow the interrupt and leave the prompt waiting forever with the
/// terminal still not echoing.
#[cfg(unix)]
struct RawModeGuard(libc::termios);

#[cfg(unix)]
impl RawModeGuard {
    fn enable(unavailable: &'static str) -> Result<Self, CliError> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
            return Err(CliError::authentication(unavailable));
        }
        let original = unsafe { original.assume_init() };
        let guard = Self(original);

        let mut raw = original;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(CliError::authentication(unavailable));
        }

        Ok(guard)
    }
}

#[cfg(unix)]
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
        }
    }
}

/// Presents the provider menu on the terminal: one line per entry, arrow
/// keys (or a digit) move the cursor, Enter selects, and `q` or ctrl-c
/// cancels. Only ever called when standard input is a terminal.
#[cfg(unix)]
pub(crate) fn run_production_login_menu(items: &[String]) -> Result<Option<usize>, CliError> {
    let _guard = RawModeGuard::enable("login menu is unavailable")?;

    eprintln!("Select a login method (arrows move, Enter selects, q cancels):");
    let mut selected = 0;
    draw_login_menu(items, selected, true);

    let mut stdin = std::io::stdin().lock();
    let mut byte = [0_u8; 1];

    loop {
        match stdin.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(_) => return Err(CliError::authentication("login menu is unavailable")),
        }

        match byte[0] {
            b'\r' | b'\n' => return Ok(Some(selected)),
            0x03 | 0x04 | b'q' => return Ok(None),
            0x1b => {
                match read_arrow_key(&mut stdin) {
                    Some(ArrowKey::Up) if selected > 0 => selected -= 1,
                    Some(ArrowKey::Down) if selected + 1 < items.len() => selected += 1,
                    _ => continue,
                }
                draw_login_menu(items, selected, false);
            }
            digit @ b'1'..=b'9' => {
                let index = usize::from(digit - b'1');
                if index < items.len() {
                    selected = index;
                    draw_login_menu(items, selected, false);
                }
            }
            _ => {}
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn run_production_login_menu(_items: &[String]) -> Result<Option<usize>, CliError> {
    Err(CliError::authentication("login menu is unavailable"))
}

/// Redraws the menu in place: after the first draw, the cursor moves back
/// up over the previously drawn lines instead of appending a new copy.
#[cfg(unix)]
fn draw_login_menu(items: &[String], selected: usize, first_draw: bool) {
    if !first_draw {
        eprint!("\u{1b}[{}A", items.len());
    }

    for (index, item) in items.iter().enumerate() {
        let cursor = if index == selected { '>' } else { ' ' };
        eprint!("\r\u{1b}[K");
        eprintln!("{cursor} {item}");
    }

    let _ = std::io::stderr().flush();
}

#[cfg(unix)]
enum ArrowKey {
    Up,
    Down,
}

/// Consumes one terminal escape sequence and reports whether it was an up
/// or down arrow; every other sequence is swallowed like
/// [`discard_escape_sequence`] does.
#[cfg(unix)]
fn read_arrow_key(stdin: &mut std::io::StdinLock<'_>) -> Option<ArrowKey> {
    let mut byte = [0_u8; 1];
    if stdin.read(&mut byte).unwrap_or(0) == 0 || byte[0] != b'[' {
        return None;
    }

    loop {
        if stdin.read(&mut byte).unwrap_or(0) == 0 {
            return None;
        }

        match byte[0] {
            b'A' => return Some(ArrowKey::Up),
            b'B' => return Some(ArrowKey::Down),
            value if value.is_ascii_alphabetic() || value == b'~' => return None,
            _ => {}
        }
    }
}

/// Reads a key from the terminal without ever echoing it, showing one mask
/// character per accepted byte so the terminal does not look frozen.
#[cfg(unix)]
fn read_hidden_tty_api_key() -> Result<String, CliError> {
    const MASK: &str = "*";
    const ERASE: &str = "\u{8} \u{8}";

    let _guard = RawModeGuard::enable("API-key input is unavailable")?;

    eprint!("API key (ctrl-c to cancel): ");
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    let mut stdin = std::io::stdin().lock();
    let mut byte = [0_u8; 1];

    loop {
        match stdin.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => return Err(CliError::authentication("API-key input is unavailable")),
        }

        match byte[0] {
            b'\r' | b'\n' => break,
            0x03 | 0x04 => {
                eprintln!();
                return Err(CliError::authentication("API-key login was cancelled"));
            }
            0x15 => {
                for _ in 0..input.chars().count() {
                    eprint!("{ERASE}");
                }
                input.clear();
            }
            0x7f | 0x08 => {
                if input.pop().is_some() {
                    eprint!("{ERASE}");
                }
            }
            0x1b => discard_escape_sequence(&mut stdin),
            value
                if (value.is_ascii_graphic() || value == b' ')
                    && input.len() < MAX_API_KEY_INPUT_BYTES as usize =>
            {
                input.push(char::from(value));
                eprint!("{MASK}");
            }
            _ => {}
        }

        let _ = std::io::stderr().flush();
    }

    eprintln!();
    normalize_api_key_input(&input)
}

/// Swallows the rest of a terminal escape sequence so an arrow key does not
/// arrive as the printable characters that follow the escape byte.
#[cfg(unix)]
fn discard_escape_sequence(stdin: &mut std::io::StdinLock<'_>) {
    let mut byte = [0_u8; 1];
    if stdin.read(&mut byte).unwrap_or(0) == 0 || byte[0] != b'[' {
        return;
    }

    while stdin.read(&mut byte).unwrap_or(0) == 1 {
        if byte[0].is_ascii_alphabetic() || byte[0] == b'~' {
            return;
        }
    }
}

#[cfg(not(unix))]
fn read_hidden_tty_api_key() -> Result<String, CliError> {
    Err(CliError::authentication("API-key input is unavailable"))
}

fn read_stdin_api_key() -> Result<String, CliError> {
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

fn api_key_is_ready(path: &Path, provider: CredentialProvider) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
        .and_then(|root| root.get(provider.identifier()).cloned())
        .and_then(|entry| entry.get("api_key").cloned())
        .and_then(|key| key.as_str().map(|key| !key.trim().is_empty()))
        .unwrap_or(false)
}

fn provider_status(path: &Path, provider: CredentialProvider) -> Result<String, CliError> {
    match provider {
        CredentialProvider::OpenAiApi | CredentialProvider::Moonshot => {
            let label = provider.label();

            if api_key_is_ready(path, provider) {
                Ok(format!("{label} authentication: ready\n"))
            } else {
                Err(CliError::authentication(format!(
                    "{label} credentials are unavailable or invalid"
                )))
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

pub(crate) fn run_production_auth_login(
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

pub(crate) fn chatgpt_login_error(error: LoginError) -> CliError {
    CliError::authentication(error.stage_message())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error_result;

    #[test]
    fn production_chatgpt_login_errors_render_fixed_sanitized_stages() {
        for error in [
            LoginError::Authentication("setup detail"),
            LoginError::Authentication("callback request is invalid"),
            LoginError::Authentication("authorization was denied"),
            LoginError::CallbackPortsBusy,
            LoginError::CallbackPortsDenied,
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
}
