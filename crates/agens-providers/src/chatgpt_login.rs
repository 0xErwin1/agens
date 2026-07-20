use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fs4::fs_std::FileExt;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

pub const ISSUER: &str = "https://auth.openai.com";
pub const AUTHORIZATION_ENDPOINT: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const DEVICE_USER_CODE_ENDPOINT: &str =
    "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const DEVICE_TOKEN_ENDPOINT: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
pub const CALLBACK_PORTS: [u16; 2] = [1455, 1457];
pub const SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
pub const ORIGINATOR: &str = "codex_cli_rs";
const CALLBACK_PATH: &str = "/auth/callback";
const MAX_REQUEST_BYTES: usize = 8 * 1024;
const CALLBACK_READ_SLICE: Duration = Duration::from_millis(25);
const CALLBACK_CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_CALLBACK_URI: &str = "https://auth.openai.com/deviceauth/callback";
const MAX_DEVICE_FIELD_BYTES: usize = 4096;
const MAX_DEVICE_JSON_BYTES: usize = 16 * 1024;
const MAX_DEVICE_POLL_INTERVAL_SECONDS: f64 = 60.0;
const DEVICE_WAIT_SLICE: Duration = Duration::from_millis(5);
const DEVICE_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_CLAIM_NAMESPACE: &str = "https://api.openai.com/auth";
const MIN_PKCE_VERIFIER_BYTES: usize = 43;
const MAX_PKCE_VERIFIER_BYTES: usize = 128;
const PKCE_CHALLENGE_BYTES: usize = 32;
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Pkce { redacted: true }")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ChatGptCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    pub expires_at: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ChatGptDeviceCodeLogin {
    pub verification_url: String,
    pub user_code: String,
    pub credentials: ChatGptCredentials,
}

impl std::fmt::Debug for ChatGptDeviceCodeLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChatGptDeviceCodeLogin")
            .field("verification_url", &self.verification_url)
            .field("user_code", &self.user_code)
            .field("credentials", &self.credentials)
            .finish()
    }
}

impl std::fmt::Debug for ChatGptCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ChatGptCredentials { redacted: true }")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginError {
    Authentication(&'static str),
    TokenTransport,
    TokenStatus,
    TokenFormat,
    Account,
    Expiry,
    Cancelled,
    TimedOut,
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Authentication(message) => {
                return write!(formatter, "ChatGPT authentication required: {message}");
            }
            Self::TokenTransport => "ChatGPT token request failed; check the network and retry",
            Self::TokenStatus => "ChatGPT token request was rejected; retry authentication",
            Self::TokenFormat => "ChatGPT token response was invalid; retry authentication",
            Self::Account => "ChatGPT account ID was missing; retry authentication",
            Self::Expiry => "ChatGPT token expiry was invalid; retry authentication",
            Self::Cancelled => "ChatGPT login was cancelled",
            Self::TimedOut => "ChatGPT login timed out",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for LoginError {}

impl LoginError {
    pub fn stage_message(&self) -> &'static str {
        match self {
            Self::Authentication("authorization was denied") => {
                "ChatGPT authorization was denied; retry and approve access"
            }
            Self::Authentication(
                "callback request is invalid"
                | "callback state did not match"
                | "callback code is missing"
                | "loopback callback failed",
            ) => "ChatGPT login callback failed; retry authentication",
            Self::Authentication(_) => "ChatGPT login setup failed; retry authentication",
            Self::TokenTransport => "ChatGPT token request failed; check the network and retry",
            Self::TokenStatus => "ChatGPT token request was rejected; retry authentication",
            Self::TokenFormat => "ChatGPT token response was invalid; retry authentication",
            Self::Account => "ChatGPT account ID was missing; retry authentication",
            Self::Expiry => "ChatGPT token expiry was invalid; retry authentication",
            Self::Cancelled => "ChatGPT login was cancelled",
            Self::TimedOut => "ChatGPT login timed out",
        }
    }
}

#[derive(Clone, Default)]
pub struct LoginCancellation {
    cancelled: Arc<AtomicBool>,
}

impl LoginCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn from_shared_flag(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

type BrowserOpener = Arc<dyn Fn(&str) -> std::io::Result<()> + Send + Sync>;
type UrlPublisher = Arc<dyn Fn(&str) + Send + Sync>;
type RandomBytes = Arc<dyn Fn(usize) -> Result<Vec<u8>, LoginError> + Send + Sync>;
type PortBinder = Arc<dyn Fn(u16) -> std::io::Result<TcpListener> + Send + Sync>;
type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

#[derive(Clone)]
pub struct ChatGptLoginOptions {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub callback_ports: Vec<u16>,
    pub timeout: Duration,
    pub open_browser: BrowserOpener,
    pub publish_url: UrlPublisher,
    pub random_bytes: RandomBytes,
    pub bind_port: PortBinder,
    pub now: Clock,
}

impl ChatGptLoginOptions {
    pub fn new(open_browser: BrowserOpener, publish_url: UrlPublisher) -> Self {
        Self {
            authorization_endpoint: AUTHORIZATION_ENDPOINT.to_owned(),
            token_endpoint: TOKEN_ENDPOINT.to_owned(),
            callback_ports: CALLBACK_PORTS.to_vec(),
            timeout: LOGIN_TIMEOUT,
            open_browser,
            publish_url,
            random_bytes: Arc::new(secure_random_bytes),
            bind_port: Arc::new(bind_loopback_port),
            now: Arc::new(SystemTime::now),
        }
    }

    pub fn for_test(authorization_endpoint: &str, token_endpoint: &str) -> Self {
        Self::new(Arc::new(|_| Ok(())), Arc::new(|_| {}))
            .with_endpoints(authorization_endpoint, token_endpoint)
    }

    fn with_endpoints(mut self, authorization_endpoint: &str, token_endpoint: &str) -> Self {
        self.authorization_endpoint = authorization_endpoint.to_owned();
        self.token_endpoint = token_endpoint.to_owned();
        self
    }
}

#[derive(Clone)]
pub struct ChatGptDeviceCodeLoginOptions {
    pub user_code_endpoint: String,
    pub device_token_endpoint: String,
    pub token_endpoint: String,
    pub timeout: Duration,
    pub now: Clock,
}

impl ChatGptDeviceCodeLoginOptions {
    pub fn new() -> Self {
        Self {
            user_code_endpoint: DEVICE_USER_CODE_ENDPOINT.to_owned(),
            device_token_endpoint: DEVICE_TOKEN_ENDPOINT.to_owned(),
            token_endpoint: TOKEN_ENDPOINT.to_owned(),
            timeout: LOGIN_TIMEOUT,
            now: Arc::new(SystemTime::now),
        }
    }

    pub fn for_test(
        user_code_endpoint: &str,
        device_token_endpoint: &str,
        token_endpoint: &str,
    ) -> Self {
        Self {
            user_code_endpoint: user_code_endpoint.to_owned(),
            device_token_endpoint: device_token_endpoint.to_owned(),
            token_endpoint: token_endpoint.to_owned(),
            timeout: LOGIN_TIMEOUT,
            now: Arc::new(SystemTime::now),
        }
    }
}

impl Default for ChatGptDeviceCodeLoginOptions {
    fn default() -> Self {
        Self::new()
    }
}

pub fn generate_pkce(
    random_bytes: &dyn Fn(usize) -> Result<Vec<u8>, LoginError>,
) -> Result<Pkce, LoginError> {
    let verifier_bytes = random_bytes(64)?;
    if verifier_bytes.len() != 64 {
        return Err(LoginError::Authentication(
            "secure random generation failed",
        ));
    }
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok(Pkce {
        verifier,
        challenge,
    })
}

pub fn generate_state(
    random_bytes: &dyn Fn(usize) -> Result<Vec<u8>, LoginError>,
) -> Result<String, LoginError> {
    let state = random_bytes(32)?;
    if state.len() != 32 {
        return Err(LoginError::Authentication(
            "secure random generation failed",
        ));
    }
    Ok(URL_SAFE_NO_PAD.encode(state))
}

pub fn authorization_url(redirect_uri: &str, challenge: &str, state: &str) -> Url {
    authorization_url_for_endpoint(AUTHORIZATION_ENDPOINT, redirect_uri, challenge, state)
        .expect("the fixed authorization endpoint is valid")
}

pub fn login(
    options: ChatGptLoginOptions,
    cancellation: LoginCancellation,
) -> Result<ChatGptCredentials, LoginError> {
    let deadline = Instant::now() + options.timeout.min(LOGIN_TIMEOUT);
    check_login_stop(&cancellation, deadline)?;
    let pkce = generate_pkce(options.random_bytes.as_ref())?;
    let state = generate_state(options.random_bytes.as_ref())?;
    let listener = bind_candidate_port(&options.callback_ports, options.bind_port.as_ref())?;
    let redirect_uri = format!(
        "http://localhost:{}/auth/callback",
        listener
            .local_addr()
            .map_err(|_| LoginError::Authentication("loopback callback is unavailable"))?
            .port()
    );
    let authorization_url = authorization_url_for_endpoint(
        &options.authorization_endpoint,
        &redirect_uri,
        &pkce.challenge,
        &state,
    )?;

    (options.publish_url)(authorization_url.as_str());
    let _ = (options.open_browser)(authorization_url.as_str());

    let code = wait_for_callback(listener, &state, deadline, &cancellation)?;
    exchange_code_cancellable(
        &options.token_endpoint,
        &code,
        &redirect_uri,
        &pkce.verifier,
        &cancellation,
        deadline,
        options.now,
    )
}

pub fn device_code_login(
    options: ChatGptDeviceCodeLoginOptions,
    cancellation: LoginCancellation,
) -> Result<ChatGptDeviceCodeLogin, LoginError> {
    device_code_login_with_progress(options, cancellation, |_, _| {})
}

pub fn device_code_login_with_progress(
    options: ChatGptDeviceCodeLoginOptions,
    cancellation: LoginCancellation,
    publish: impl FnOnce(&str, &str),
) -> Result<ChatGptDeviceCodeLogin, LoginError> {
    let deadline = Instant::now() + options.timeout.min(LOGIN_TIMEOUT);
    let device = device_user_code_request(&options.user_code_endpoint, &cancellation, deadline)?;
    let (device_auth_id, user_code, interval) = parse_device_user_code(&device)?;
    publish(DEVICE_VERIFICATION_URL, &user_code);

    loop {
        check_login_stop(&cancellation, deadline)?;
        let response = device_token_request(
            &options.device_token_endpoint,
            &device_auth_id,
            &user_code,
            &cancellation,
            deadline,
        )?;
        if let Some(token) = response {
            let (authorization_code, code_challenge, code_verifier) = parse_device_token(&token)?;
            validate_device_pkce(&code_challenge, &code_verifier)?;
            let credentials = exchange_code_cancellable(
                &options.token_endpoint,
                &authorization_code,
                DEVICE_CALLBACK_URI,
                &code_verifier,
                &cancellation,
                deadline,
                options.now,
            )?;
            return Ok(ChatGptDeviceCodeLogin {
                verification_url: DEVICE_VERIFICATION_URL.to_owned(),
                user_code,
                credentials,
            });
        }
        sleep_with_cancellation(interval, &cancellation, deadline)?;
    }
}

pub fn upsert_chatgpt_credentials(
    path: &Path,
    credentials: &ChatGptCredentials,
) -> Result<(), LoginError> {
    if credentials.access_token.is_empty()
        || credentials.refresh_token.is_empty()
        || credentials.account_id.is_empty()
        || credentials.expires_at.is_empty()
    {
        return Err(LoginError::Authentication("token response is incomplete"));
    }

    let mut entry = Map::new();
    entry.insert(
        "access_token".to_owned(),
        Value::String(credentials.access_token.clone()),
    );
    entry.insert(
        "refresh_token".to_owned(),
        Value::String(credentials.refresh_token.clone()),
    );
    entry.insert(
        "account_id".to_owned(),
        Value::String(credentials.account_id.clone()),
    );
    entry.insert(
        "expires_at".to_owned(),
        Value::String(credentials.expires_at.clone()),
    );
    upsert_provider_entry_inner(
        path,
        "openai-chatgpt",
        Value::Object(entry),
        &["id_token"],
        None,
    )
}

/// Merges one provider object into auth.json without losing independently written providers.
pub fn upsert_provider_entry(path: &Path, provider: &str, entry: Value) -> Result<(), LoginError> {
    upsert_provider_entry_inner(path, provider, entry, &[], None)
}

/// Like [`upsert_provider_entry`], but aborts while waiting for the process lock.
pub fn upsert_provider_entry_with_deadline(
    path: &Path,
    provider: &str,
    entry: Value,
    cancellation: &LoginCancellation,
    deadline: Instant,
) -> Result<(), LoginError> {
    upsert_provider_entry_inner(path, provider, entry, &[], Some((cancellation, deadline)))
}

/// Removes one provider entry from auth.json without changing other providers.
pub fn remove_provider_entry(path: &Path, provider: &str) -> Result<bool, LoginError> {
    if provider.is_empty() {
        return Err(LoginError::Authentication("credentials file is invalid"));
    }

    let (parent, name) = open_secure_parent(path)?;
    let lock = open_file_at(
        &parent,
        b".auth.json.lock",
        libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW,
        0o600,
    )
    .map_err(|_| LoginError::Authentication("credentials file is unavailable"))?;
    acquire_lock(&lock, None)?;
    if lock
        .metadata()
        .map(|metadata| metadata.nlink() != 1)
        .unwrap_or(true)
    {
        return Err(LoginError::Authentication(
            "credentials file is unavailable",
        ));
    }

    let mut root = read_credentials_at(&parent, &name)?;
    let root_object = root
        .as_object_mut()
        .ok_or(LoginError::Authentication("credentials file is invalid"))?;
    if root_object.remove(provider).is_none() {
        return Ok(false);
    }

    let contents = serde_json::to_vec(&root)
        .map_err(|_| LoginError::Authentication("credentials could not be encoded"))?;
    write_credentials_atomically_at(&parent, &name, &contents)?;
    Ok(true)
}

fn upsert_provider_entry_inner(
    path: &Path,
    provider: &str,
    entry: Value,
    remove: &[&str],
    wait: Option<(&LoginCancellation, Instant)>,
) -> Result<(), LoginError> {
    if provider.is_empty() || !matches!(entry, Value::Object(_)) {
        return Err(LoginError::Authentication("credentials file is invalid"));
    }
    let (parent, name) = open_secure_parent(path)?;
    let lock = open_file_at(
        &parent,
        b".auth.json.lock",
        libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW,
        0o600,
    )
    .map_err(|_| LoginError::Authentication("credentials file is unavailable"))?;
    acquire_lock(&lock, wait)?;
    if lock
        .metadata()
        .map(|metadata| metadata.nlink() != 1)
        .unwrap_or(true)
    {
        return Err(LoginError::Authentication(
            "credentials file is unavailable",
        ));
    }
    let mut root = read_credentials_at(&parent, &name)?;
    let root_object = root
        .as_object_mut()
        .ok_or(LoginError::Authentication("credentials file is invalid"))?;
    let target = root_object
        .entry(provider.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let target = target
        .as_object_mut()
        .ok_or(LoginError::Authentication("credentials file is invalid"))?;
    for (key, value) in entry.as_object().expect("validated object") {
        target.insert(key.clone(), value.clone());
    }
    for key in remove {
        target.remove(*key);
    }
    let contents = serde_json::to_vec(&root)
        .map_err(|_| LoginError::Authentication("credentials could not be encoded"))?;
    write_credentials_atomically_at(&parent, &name, &contents)
}

fn secure_random_bytes(length: usize) -> Result<Vec<u8>, LoginError> {
    let mut bytes = vec![0; length];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|_| LoginError::Authentication("secure random generation failed"))?;
    Ok(bytes)
}

fn authorization_url_for_endpoint(
    endpoint: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> Result<Url, LoginError> {
    let mut url = Url::parse(endpoint)
        .map_err(|_| LoginError::Authentication("authorization endpoint is invalid"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("originator", ORIGINATOR)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true");
    Ok(url)
}

fn bind_loopback_port(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
}

fn bind_candidate_port(
    ports: &[u16],
    bind: &dyn Fn(u16) -> std::io::Result<TcpListener>,
) -> Result<TcpListener, LoginError> {
    ports
        .iter()
        .copied()
        .find_map(|port| bind(port).ok())
        .ok_or(LoginError::Authentication(
            "loopback callback is unavailable",
        ))
}

fn wait_for_callback(
    listener: TcpListener,
    state: &str,
    deadline: Instant,
    cancellation: &LoginCancellation,
) -> Result<String, LoginError> {
    listener
        .set_nonblocking(true)
        .map_err(|_| LoginError::Authentication("loopback callback is unavailable"))?;
    loop {
        check_login_stop(cancellation, deadline)?;
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .map_err(|_| LoginError::Authentication("loopback callback failed"))?;
                if let Some(result) = handle_callback(
                    &mut stream,
                    state,
                    listener.local_addr().ok(),
                    deadline,
                    cancellation,
                ) {
                    return result;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(_) => return Err(LoginError::Authentication("loopback callback failed")),
        }
    }
}

fn handle_callback(
    stream: &mut TcpStream,
    expected_state: &str,
    callback_address: Option<SocketAddr>,
    deadline: Instant,
    cancellation: &LoginCancellation,
) -> Option<Result<String, LoginError>> {
    let request = match read_callback_request(stream, deadline, cancellation) {
        Ok(Some(request)) => request,
        Ok(None) => return None,
        Err(()) => {
            write_response(stream, 400, "Login failed");
            return Some(Err(LoginError::Authentication(
                "callback request is invalid",
            )));
        }
    };
    let callback_address = match callback_address {
        Some(address) => address,
        None => {
            write_response(stream, 400, "Login failed");
            return Some(Err(LoginError::Authentication(
                "callback request is invalid",
            )));
        }
    };
    let (target, host) = match parse_http_request(&request) {
        Some(request) => request,
        None => {
            write_response(stream, 400, "Login failed");
            return Some(Err(LoginError::Authentication(
                "callback request is invalid",
            )));
        }
    };
    if !valid_callback_host(host, callback_address.port()) {
        write_response(stream, 400, "Login failed");
        return Some(Err(LoginError::Authentication(
            "callback request is invalid",
        )));
    }
    let (path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    if path != CALLBACK_PATH {
        write_response(stream, 404, "Not found");
        return None;
    }
    let query = match parse_callback_query(raw_query) {
        Some(query) => query,
        None => {
            write_response(stream, 400, "Login failed");
            return Some(Err(LoginError::Authentication(
                "callback request is invalid",
            )));
        }
    };
    let actual_state = query.state.as_deref().unwrap_or_default();
    if !constant_time_equal(actual_state.as_bytes(), expected_state.as_bytes()) {
        write_response(stream, 400, "Login failed");
        return Some(Err(LoginError::Authentication(
            "callback state did not match",
        )));
    }
    if query.error.is_some() {
        write_response(stream, 400, "Login failed");
        return Some(Err(LoginError::Authentication("authorization was denied")));
    }
    let code = query.code.filter(|code| !code.is_empty());
    match code {
        Some(code) => {
            write_response(
                stream,
                200,
                "Authorization received. Return to the terminal.",
            );
            Some(Ok(code))
        }
        None => {
            write_response(stream, 400, "Login failed");
            Some(Err(LoginError::Authentication("callback code is missing")))
        }
    }
}

struct CallbackQuery {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

fn read_callback_request(
    stream: &mut TcpStream,
    deadline: Instant,
    cancellation: &LoginCancellation,
) -> Result<Option<Vec<u8>>, ()> {
    let client_deadline = deadline.min(Instant::now() + CALLBACK_CLIENT_TIMEOUT);
    let mut request = Vec::with_capacity(1024);
    let mut buffer = [0_u8; 1024];

    loop {
        if cancellation.is_cancelled() || Instant::now() >= client_deadline {
            return Ok(None);
        }
        let remaining = client_deadline.saturating_duration_since(Instant::now());
        stream
            .set_read_timeout(Some(remaining.min(CALLBACK_READ_SLICE)))
            .map_err(|_| ())?;
        match stream.read(&mut buffer) {
            Ok(0) => return Err(()),
            Ok(read) => {
                if request.len() + read > MAX_REQUEST_BYTES {
                    return Err(());
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    return Ok(Some(request));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return Err(()),
        }
    }
}

fn parse_http_request(request: &[u8]) -> Option<(&str, &str)> {
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    let header = std::str::from_utf8(&request[..header_end]).ok()?;
    let mut lines = header.split("\r\n");
    let request_line = lines.next()?;
    let mut fields = request_line.split(' ');
    if fields.next()? != "GET"
        || fields.next()?.is_empty()
        || fields.next()? != "HTTP/1.1"
        || fields.next().is_some()
    {
        return None;
    }
    let target = request_line.split(' ').nth(1)?;
    if !target.starts_with('/') || !target.is_ascii() {
        return None;
    }
    let mut host = None;
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("host") && host.replace(value.trim()).is_some() {
            return None;
        }
        if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            return None;
        }
    }
    Some((target, host?))
}

fn valid_callback_host(host: &str, port: u16) -> bool {
    host == format!("localhost:{port}") || host == format!("127.0.0.1:{port}")
}

fn parse_callback_query(raw_query: &str) -> Option<CallbackQuery> {
    let mut query = CallbackQuery {
        state: None,
        code: None,
        error: None,
        error_description: None,
    };
    for pair in raw_query.split('&') {
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = strict_query_decode(raw_key)?;
        let value = strict_query_decode(raw_value)?;
        let target = match key.as_str() {
            "state" => &mut query.state,
            "code" => &mut query.code,
            "error" => &mut query.error,
            "error_description" => &mut query.error_description,
            _ => continue,
        };
        if target.replace(value).is_some() {
            return None;
        }
    }
    Some(query)
}

fn strict_query_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = *bytes.get(index + 1)?;
                let low = *bytes.get(index + 2)?;
                decoded.push((hex_value(high)? << 4) | hex_value(low)?);
                index += 3;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte if byte.is_ascii() => {
                decoded.push(byte);
                index += 1;
            }
            _ => return None,
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn write_response(stream: &mut TcpStream, status: u16, message: &str) {
    let body = format!("<!doctype html><title>{message}</title><p>{message}</p>");
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(*left.get(index).unwrap_or(&0) ^ *right.get(index).unwrap_or(&0));
    }
    difference == 0
}

fn credentials_from_token(
    token: &Value,
    now: SystemTime,
) -> Result<ChatGptCredentials, LoginError> {
    let id_token = required_token(token, "id_token")?;
    let access_token = required_token(token, "access_token")?;
    let refresh_token = required_token(token, "refresh_token")?;
    let account_id = account_id_from_id_token(id_token)?;
    let expires_at = jwt_expiry(access_token, now)
        .or_else(|| expires_in_expiry(token, now))
        .ok_or(LoginError::Expiry)?;
    Ok(ChatGptCredentials {
        access_token: access_token.to_owned(),
        refresh_token: refresh_token.to_owned(),
        account_id,
        expires_at,
    })
}

pub fn account_id_from_id_token(id_token: &str) -> Result<String, LoginError> {
    let claims = jwt_payload(id_token).ok_or(LoginError::Account)?;
    let account_id = claims
        .get(AUTH_CLAIM_NAMESPACE)
        .and_then(Value::as_object)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            claims
                .get("chatgpt_account_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(Value::as_array)
                .and_then(|organizations| organizations.first())
                .and_then(|organization| organization.get("id"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        })
        .ok_or(LoginError::Account)?;

    Ok(account_id.to_owned())
}

fn device_user_code_request(
    endpoint: &str,
    cancellation: &LoginCancellation,
    deadline: Instant,
) -> Result<Value, LoginError> {
    let endpoint = endpoint.to_owned();
    let response = cancellable_request(cancellation, deadline, move || {
        reqwest::blocking::Client::builder()
            .timeout(DEVICE_HTTP_TIMEOUT)
            .build()
            .expect("the fixed device HTTP client configuration is valid")
            .post(endpoint)
            .json(&serde_json::json!({ "client_id": CLIENT_ID }))
            .send()
            .map(|mut response| {
                let status = response.status().as_u16();
                (status, read_device_json(&mut response))
            })
    })?;
    match response {
        (404, _) => Err(LoginError::Authentication("Authentication unavailable")),
        (status, Ok(body)) if (200..300).contains(&status) => Ok(body),
        _ => Err(LoginError::Authentication("device authorization failed")),
    }
}

fn device_token_request(
    endpoint: &str,
    device_auth_id: &str,
    user_code: &str,
    cancellation: &LoginCancellation,
    deadline: Instant,
) -> Result<Option<Value>, LoginError> {
    let endpoint = endpoint.to_owned();
    let device_auth_id = device_auth_id.to_owned();
    let user_code = user_code.to_owned();
    let response = cancellable_request(cancellation, deadline, move || {
        reqwest::blocking::Client::builder()
            .timeout(DEVICE_HTTP_TIMEOUT)
            .build()
            .expect("the fixed device HTTP client configuration is valid")
            .post(endpoint)
            .json(&serde_json::json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
            }))
            .send()
            .map(|mut response| {
                let status = response.status().as_u16();
                (status, read_device_json(&mut response))
            })
    })?;
    match response {
        (403 | 404, _) => Ok(None),
        (status, Ok(body)) if (200..300).contains(&status) => Ok(Some(body)),
        _ => Err(LoginError::Authentication("device authorization failed")),
    }
}

fn parse_device_user_code(value: &Value) -> Result<(String, String, Duration), LoginError> {
    let device_auth_id = bounded_device_string(value, "device_auth_id")?;
    let user_code = value
        .get("user_code")
        .or_else(|| value.get("usercode"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_DEVICE_FIELD_BYTES)
        .ok_or(LoginError::Authentication(
            "device authorization response is invalid",
        ))?
        .to_owned();
    let interval = value
        .get("interval")
        .and_then(Value::as_str)
        .filter(|value| value.len() <= 32)
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| {
            value.is_finite() && *value > 0.0 && *value <= MAX_DEVICE_POLL_INTERVAL_SECONDS
        })
        .map(Duration::from_secs_f64)
        .ok_or(LoginError::Authentication(
            "device authorization response is invalid",
        ))?;
    Ok((device_auth_id, user_code, interval))
}

fn parse_device_token(value: &Value) -> Result<(String, String, String), LoginError> {
    let authorization_code = bounded_device_string(value, "authorization_code")?;
    let code_challenge = bounded_device_string(value, "code_challenge")?;
    let code_verifier = bounded_device_string(value, "code_verifier")?;

    Ok((authorization_code, code_challenge, code_verifier))
}

fn validate_device_pkce(code_challenge: &str, code_verifier: &str) -> Result<(), LoginError> {
    if !valid_base64url_no_pad(code_verifier)
        || !(MIN_PKCE_VERIFIER_BYTES..=MAX_PKCE_VERIFIER_BYTES).contains(&code_verifier.len())
        || !valid_base64url_no_pad(code_challenge)
        || URL_SAFE_NO_PAD
            .decode(code_challenge)
            .ok()
            .is_none_or(|decoded| decoded.len() != PKCE_CHALLENGE_BYTES)
    {
        return Err(LoginError::Authentication(
            "device authorization response is invalid",
        ));
    }

    let expected_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    if !constant_time_equal(expected_challenge.as_bytes(), code_challenge.as_bytes()) {
        return Err(LoginError::Authentication(
            "device authorization response is invalid",
        ));
    }

    Ok(())
}

fn valid_base64url_no_pad(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn read_device_json(response: &mut reqwest::blocking::Response) -> Result<Value, ()> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DEVICE_JSON_BYTES as u64)
    {
        return Err(());
    }

    let mut body = Vec::new();
    response
        .take((MAX_DEVICE_JSON_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|_| ())?;
    if body.len() > MAX_DEVICE_JSON_BYTES {
        return Err(());
    }

    serde_json::from_slice(&body).map_err(|_| ())
}

fn bounded_device_string(value: &Value, field: &str) -> Result<String, LoginError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_DEVICE_FIELD_BYTES)
        .map(str::to_owned)
        .ok_or(LoginError::Authentication(
            "device authorization response is invalid",
        ))
}

fn exchange_code_cancellable(
    token_endpoint: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    cancellation: &LoginCancellation,
    deadline: Instant,
    now: Clock,
) -> Result<ChatGptCredentials, LoginError> {
    let token_endpoint = token_endpoint.to_owned();
    let code = code.to_owned();
    let redirect_uri = redirect_uri.to_owned();
    let verifier = verifier.to_owned();
    let request_timeout = token_request_timeout(deadline)?;
    let response = cancellable_request(cancellation, deadline, move || {
        reqwest::blocking::Client::builder()
            .timeout(request_timeout)
            .build()
            .expect("the fixed authentication HTTP client configuration is valid")
            .post(token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
                ("client_id", CLIENT_ID),
                ("code_verifier", verifier.as_str()),
            ])
            .send()
            .map(|mut response| {
                let status = response.status().as_u16();
                let body = if (200..300).contains(&status) {
                    read_device_json(&mut response)
                } else {
                    Err(())
                };
                (status, body)
            })
    })
    .map_err(|error| match error {
        LoginError::Cancelled | LoginError::TimedOut => error,
        _ => LoginError::TokenTransport,
    })?;
    let (status, body) = response;
    if !(200..300).contains(&status) {
        return Err(LoginError::TokenStatus);
    };
    let token = body.map_err(|()| LoginError::TokenFormat)?;
    credentials_from_token(&token, now())
}

fn cancellable_request<T: Send + 'static>(
    cancellation: &LoginCancellation,
    deadline: Instant,
    request: impl FnOnce() -> Result<T, reqwest::Error> + Send + 'static,
) -> Result<T, LoginError> {
    check_login_stop(cancellation, deadline)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(request().map_err(|_| ()));
    });
    loop {
        check_login_stop(cancellation, deadline)?;
        match receiver.recv_timeout(DEVICE_WAIT_SLICE) {
            Ok(Ok(value)) => {
                check_login_stop(cancellation, deadline)?;
                return Ok(value);
            }
            Ok(Err(())) | Err(RecvTimeoutError::Disconnected) => {
                return Err(LoginError::Authentication("device authorization failed"));
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn token_request_timeout(deadline: Instant) -> Result<Duration, LoginError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(LoginError::TimedOut);
    }
    if remaining < DEVICE_HTTP_TIMEOUT {
        return Ok(remaining.saturating_add(DEVICE_WAIT_SLICE));
    }
    Ok(DEVICE_HTTP_TIMEOUT)
}

fn sleep_with_cancellation(
    interval: Duration,
    cancellation: &LoginCancellation,
    deadline: Instant,
) -> Result<(), LoginError> {
    let until = Instant::now() + interval;
    while Instant::now() < until {
        check_login_stop(cancellation, deadline)?;
        thread::sleep(DEVICE_WAIT_SLICE.min(until.saturating_duration_since(Instant::now())));
    }
    check_login_stop(cancellation, deadline)
}

fn check_login_stop(cancellation: &LoginCancellation, deadline: Instant) -> Result<(), LoginError> {
    if cancellation.is_cancelled() {
        return Err(LoginError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(LoginError::TimedOut);
    }
    Ok(())
}

fn required_token<'a>(token: &'a Value, field: &str) -> Result<&'a str, LoginError> {
    token
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(LoginError::Authentication("token response is incomplete"))
}

fn jwt_expiry(token: &str, now: SystemTime) -> Option<String> {
    let seconds = jwt_payload(token)?.get("exp")?.as_u64()?;
    let expiry = UNIX_EPOCH.checked_add(Duration::from_secs(seconds))?;
    if expiry <= now {
        return None;
    }
    format_expiry(expiry)
}

fn expires_in_expiry(token: &Value, now: SystemTime) -> Option<String> {
    let seconds = token
        .get("expires_in")?
        .as_u64()
        .filter(|seconds| (1..=u64::from(u32::MAX)).contains(seconds))?;
    now.checked_add(Duration::from_secs(seconds))
        .and_then(format_expiry)
}

#[cfg(test)]
fn jwt_claim(token: &str, claim: &str) -> Option<Value> {
    let payload = jwt_payload(token)?;
    let value = payload.get(claim)?;
    match claim {
        "exp" => value.as_i64().filter(|value| *value >= 0).map(Value::from),
        _ => Some(value.clone()),
    }
}

fn jwt_payload(token: &str) -> Option<Map<String, Value>> {
    let mut segments = token.split('.');
    let header = segments.next()?;
    let payload = segments.next()?;
    let signature = segments.next()?;
    if segments.next().is_some()
        || header.len() > 1024
        || payload.len() > 8192
        || signature.is_empty()
    {
        return None;
    }
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).ok()?).ok()?;
    let algorithm = header.get("alg")?.as_str()?;
    if !matches!(algorithm, "RS256" | "ES256" | "PS256") {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    if bytes.len() > 6 * 1024 {
        return None;
    }
    let payload = serde_json::from_slice::<Value>(&bytes).ok()?;
    payload.as_object().cloned()
}

fn format_expiry(expiry: SystemTime) -> Option<String> {
    OffsetDateTime::from(expiry).format(&Rfc3339).ok()
}

fn open_secure_parent(path: &Path) -> Result<(File, Vec<u8>), LoginError> {
    let name = path
        .file_name()
        .filter(|name| !name.as_bytes().is_empty())
        .ok_or(LoginError::Authentication("credentials path is invalid"))?
        .as_bytes()
        .to_vec();
    let parent = path
        .parent()
        .ok_or(LoginError::Authentication("credentials path is invalid"))?;
    let mut directory = if parent.is_absolute() {
        File::open("/")
    } else {
        File::open(".")
    }
    .map_err(|_| LoginError::Authentication("credentials directory could not be secured"))?;
    for component in parent.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                directory =
                    open_or_create_directory_at(&directory, name.as_bytes()).map_err(|_| {
                        LoginError::Authentication("credentials directory could not be secured")
                    })?
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(LoginError::Authentication("credentials path is invalid"));
            }
        }
    }
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(LoginError::Authentication(
            "credentials directory could not be secured",
        ));
    }
    Ok((directory, name))
}

fn open_or_create_directory_at(parent: &File, name: &[u8]) -> std::io::Result<File> {
    match open_file_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    ) {
        Ok(directory) => Ok(directory),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let name = c_name(name)?;
            let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
            if result != 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(std::io::Error::last_os_error());
            }
            open_file_at(
                parent,
                name.as_bytes(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )
        }
        Err(error) => Err(error),
    }
}

fn open_file_at(
    parent: &File,
    name: &[u8],
    flags: libc::c_int,
    mode: libc::mode_t,
) -> std::io::Result<File> {
    let name = c_name(name)?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn c_name(name: &[u8]) -> std::io::Result<CString> {
    CString::new(name)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL path"))
}

fn acquire_lock(
    lock: &File,
    wait: Option<(&LoginCancellation, Instant)>,
) -> Result<(), LoginError> {
    loop {
        match lock.try_lock_exclusive() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                return Err(LoginError::Authentication(
                    "credentials file is unavailable",
                ));
            }
        }
        if let Some((cancellation, deadline)) = wait {
            if cancellation.is_cancelled() {
                return Err(LoginError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(LoginError::TimedOut);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn read_credentials_at(parent: &File, name: &[u8]) -> Result<Value, LoginError> {
    let mut file = match open_file_at(parent, name, libc::O_RDONLY | libc::O_NOFOLLOW, 0) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Value::Object(Map::new()));
        }
        Err(_) => {
            return Err(LoginError::Authentication(
                "credentials file is unavailable",
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|_| LoginError::Authentication("credentials file is unavailable"))?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(LoginError::Authentication(
            "credentials file is unavailable",
        ));
    }
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .map_err(|_| LoginError::Authentication("credentials file is unavailable"))?;
    serde_json::from_slice(&contents)
        .map_err(|_| LoginError::Authentication("credentials file is invalid"))
}

fn write_credentials_atomically_at(
    parent: &File,
    name: &[u8],
    contents: &[u8],
) -> Result<(), LoginError> {
    let temporary = format!(
        ".auth-login-{}-{}.json",
        std::process::id(),
        TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let temporary = temporary.into_bytes();
    let result = (|| {
        let mut file = open_file_at(
            parent,
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        )
        .map_err(|_| LoginError::Authentication("credentials could not be persisted"))?;
        file.write_all(contents)
            .and_then(|_| file.sync_all())
            .map_err(|_| LoginError::Authentication("credentials could not be persisted"))?;
        let source = c_name(&temporary)
            .map_err(|_| LoginError::Authentication("credentials could not be persisted"))?;
        let destination = c_name(name)
            .map_err(|_| LoginError::Authentication("credentials could not be persisted"))?;
        if unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                source.as_ptr(),
                parent.as_raw_fd(),
                destination.as_ptr(),
            )
        } != 0
        {
            return Err(LoginError::Authentication(
                "credentials could not be persisted",
            ));
        }
        let final_file = open_file_at(parent, name, libc::O_RDONLY | libc::O_NOFOLLOW, 0)
            .map_err(|_| LoginError::Authentication("credentials file could not be secured"))?;
        if final_file
            .metadata()
            .map(|metadata| !metadata.is_file() || metadata.nlink() != 1)
            .unwrap_or(true)
            || unsafe { libc::fchmod(final_file.as_raw_fd(), 0o600) } != 0
        {
            return Err(LoginError::Authentication(
                "credentials file could not be secured",
            ));
        }
        parent
            .sync_all()
            .map_err(|_| LoginError::Authentication("credentials could not be persisted"))
    })();
    if result.is_err()
        && let Ok(temporary) = c_name(&temporary)
    {
        unsafe { libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0) };
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_token_endpoint(status: u16, body: &'static str) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("token listener should bind");
        let endpoint = format!("http://{}/token", listener.local_addr().expect("address"));
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("token request should arrive");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        (endpoint, server)
    }

    fn token_response(access_claims: Value, expires_in: Option<Value>) -> Value {
        let id_token = jwt(
            r#"{"alg":"RS256"}"#,
            r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account"}}"#,
            "signature",
        );
        let access_token = jwt(
            r#"{"alg":"RS256"}"#,
            &serde_json::to_string(&access_claims).expect("claims should encode"),
            "signature",
        );
        let mut token = serde_json::json!({
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": "refresh",
        });
        if let Some(expires_in) = expires_in {
            token["expires_in"] = expires_in;
        }
        token
    }

    fn jwt(header: &str, payload: &str, signature: &str) -> String {
        format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(header),
            URL_SAFE_NO_PAD.encode(payload),
            signature
        )
    }

    #[test]
    fn jwt_claim_rejects_malformed_structure_without_echoing_tokens() {
        let secret = "raw-secret-token";
        let malformed = [
            secret.to_owned(),
            format!("{secret}.{secret}"),
            format!("{secret}.{secret}.{secret}.extra"),
            format!("not-base64.{}.signature", URL_SAFE_NO_PAD.encode("{}")),
            jwt("[]", "{}", "signature"),
            jwt(r#"{"alg":"none"}"#, "{}", "signature"),
            jwt(r#"{"alg":"HS256"}"#, "{}", "signature"),
            jwt(r#"{"alg":"RS256"}"#, "[]", "signature"),
            jwt(r#"{"alg":"RS256"}"#, "{}", ""),
        ];
        for token in malformed {
            assert!(jwt_payload(&token).is_none());
        }
    }

    #[test]
    fn jwt_claim_accepts_only_valid_expiration_claim_types() {
        let header = r#"{"alg":"RS256"}"#;
        for payload in [
            r#"{"exp":-1}"#,
            r#"{"exp":"1"}"#,
            r#"{"exp":18446744073709551615}"#,
        ] {
            let value = jwt_claim(&jwt(header, payload, "signature"), "exp");
            assert!(value.is_none() || value.as_ref().is_some_and(Value::is_i64));
        }
        assert_eq!(
            jwt_claim(&jwt(header, r#"{"exp":0}"#, "signature"), "exp"),
            Some(Value::from(0))
        );
    }

    #[test]
    fn token_expiry_prefers_a_future_jwt_and_falls_back_only_to_bounded_expires_in() {
        let now_seconds = 1_700_000_000_u64;
        let now = UNIX_EPOCH + Duration::from_secs(now_seconds);
        let expected_jwt = format_expiry(UNIX_EPOCH + Duration::from_secs(now_seconds + 1))
            .expect("expiry should format");
        let expected_fallback =
            format_expiry(now + Duration::from_secs(1)).expect("expiry should format");

        let jwt_wins = credentials_from_token(
            &token_response(
                serde_json::json!({"exp": now_seconds + 1}),
                Some(Value::from(0)),
            ),
            now,
        )
        .expect("a future JWT expiry should win");
        assert_eq!(jwt_wins.expires_at, expected_jwt);

        for unusable_exp in [
            Value::Null,
            Value::String("1700000001".to_owned()),
            serde_json::json!(1.5),
            Value::from(-1),
            Value::from(0),
            Value::from(now_seconds - 1),
            Value::from(now_seconds),
            Value::from(u64::MAX),
        ] {
            let fallback = credentials_from_token(
                &token_response(
                    serde_json::json!({"exp": unusable_exp}),
                    Some(Value::from(1)),
                ),
                now,
            )
            .expect("a bounded fallback should replace an unusable JWT expiry");
            assert_eq!(fallback.expires_at, expected_fallback);
        }

        let maximum = credentials_from_token(
            &token_response(serde_json::json!({}), Some(Value::from(u32::MAX))),
            UNIX_EPOCH,
        )
        .expect("the inclusive fallback bound should be accepted");
        assert_eq!(
            maximum.expires_at,
            format_expiry(UNIX_EPOCH + Duration::from_secs(u64::from(u32::MAX)))
                .expect("maximum expiry should format")
        );

        for invalid_fallback in [
            None,
            Some(Value::String("1".to_owned())),
            Some(serde_json::json!(1.5)),
            Some(Value::from(-1)),
            Some(Value::from(0)),
            Some(Value::from(u64::from(u32::MAX) + 1)),
        ] {
            assert_eq!(
                credentials_from_token(
                    &token_response(serde_json::json!({"exp": now_seconds}), invalid_fallback),
                    now,
                ),
                Err(LoginError::Expiry)
            );
        }

        let maximum_time = UNIX_EPOCH
            .checked_add(Duration::from_secs(i64::MAX as u64))
            .expect("the platform should represent its maximum second");
        assert_eq!(
            credentials_from_token(
                &token_response(serde_json::json!({}), Some(Value::from(1))),
                maximum_time,
            ),
            Err(LoginError::Expiry)
        );
    }

    #[test]
    fn token_exchange_reports_sanitized_transport_status_and_format_stages() {
        for (status, body, expected) in [
            (503, r#"{"private":"body"}"#, LoginError::TokenStatus),
            (200, "private-invalid-body", LoginError::TokenFormat),
        ] {
            let (endpoint, server) = local_token_endpoint(status, body);
            let error = exchange_code_cancellable(
                &endpoint,
                "private-code",
                "http://localhost/private-callback",
                "private-verifier",
                &LoginCancellation::new(),
                Instant::now() + Duration::from_secs(1),
                Arc::new(SystemTime::now),
            )
            .expect_err("token response should fail");
            assert_eq!(error, expected);
            assert!(!error.stage_message().contains("private"));
            server.join().expect("token server should finish");
        }

        let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let endpoint = format!("http://{}/token", listener.local_addr().expect("address"));
        drop(listener);
        assert_eq!(
            exchange_code_cancellable(
                &endpoint,
                "private-code",
                "http://localhost/private-callback",
                "private-verifier",
                &LoginCancellation::new(),
                Instant::now() + Duration::from_secs(1),
                Arc::new(SystemTime::now),
            ),
            Err(LoginError::TokenTransport)
        );
    }

    #[test]
    fn callback_parsers_reject_the_http_and_query_attack_matrix() {
        assert!(
            parse_http_request(b"GET /auth/callback HTTP/1.1\r\nHost: localhost:1455\r\n\r\n")
                .is_some()
        );
        for request in [
            b"POST /auth/callback HTTP/1.1\r\nHost: localhost:1455\r\n\r\n".as_slice(),
            b"GET /auth/callback HTTP/1.0\r\nHost: localhost:1455\r\n\r\n".as_slice(),
            b"GET /auth/callback HTTP/1.1\r\nHost: localhost:1455\r\nHost: localhost:1455\r\n\r\n"
                .as_slice(),
            b"GET /auth/callback HTTP/1.1\r\nHost: localhost:1455\r\nContent-Length: 1\r\n\r\n"
                .as_slice(),
            b"GET /auth/callback HTTP/1.1\r\nHost: \xff\r\n\r\n".as_slice(),
        ] {
            assert!(parse_http_request(request).is_none());
        }
        for query in [
            "state=a&state=b",
            "code=a&code=b",
            "state=%",
            "state=%ZZ",
            "state=%FF",
            "state=\u{80}",
        ] {
            assert!(parse_callback_query(query).is_none());
        }
        assert!(parse_callback_query("state=a&code=b").is_some());
        assert!(valid_callback_host("localhost:1455", 1455));
        assert!(!valid_callback_host("attacker.example", 1455));
    }

    #[test]
    fn callback_accepts_fragmented_requests_and_closes_slow_incomplete_clients() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        let waiting = thread::spawn(move || {
            wait_for_callback(
                listener,
                "expected-state",
                Instant::now() + Duration::from_millis(250),
                &LoginCancellation::new(),
            )
        });
        let mut client = TcpStream::connect(address).expect("client should connect");
        client
            .write_all(b"GET /auth/callback?state=expected-")
            .expect("first fragment");
        thread::sleep(Duration::from_millis(5));
        client
            .write_all(format!("state&code=code HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
            .expect("second fragment");
        assert_eq!(
            waiting.join().expect("waiter should finish"),
            Ok("code".to_owned())
        );

        let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        let waiting = thread::spawn(move || {
            wait_for_callback(
                listener,
                "expected-state",
                Instant::now() + Duration::from_millis(35),
                &LoginCancellation::new(),
            )
        });
        let mut slow = TcpStream::connect(address).expect("slow client should connect");
        slow.write_all(b"GET /auth/callback?")
            .expect("partial request");
        thread::sleep(Duration::from_millis(60));
        assert_eq!(
            waiting.join().expect("waiter should finish"),
            Err(LoginError::TimedOut)
        );
        let mut byte = [0_u8; 1];
        slow.set_read_timeout(Some(Duration::from_millis(50)))
            .expect("read timeout");
        assert_eq!(
            slow.read(&mut byte)
                .expect("closed callback should read EOF"),
            0
        );
    }
}
