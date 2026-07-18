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
pub const CALLBACK_PORTS: [u16; 2] = [1455, 1457];
pub const SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
pub const ORIGINATOR: &str = "codex_cli_rs";
const CALLBACK_PATH: &str = "/auth/callback";
const MAX_REQUEST_BYTES: usize = 8 * 1024;
const CALLBACK_READ_SLICE: Duration = Duration::from_millis(25);
const CALLBACK_CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const ACCOUNT_ID_CLAIM: &str = "https://api.openai.com/auth.chatgpt_account_id";
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

impl std::fmt::Debug for ChatGptCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ChatGptCredentials { redacted: true }")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginError {
    Authentication(&'static str),
    Cancelled,
    TimedOut,
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authentication(message) => {
                write!(formatter, "ChatGPT authentication required: {message}")
            }
            Self::Cancelled => formatter.write_str("ChatGPT login was cancelled"),
            Self::TimedOut => formatter.write_str("ChatGPT login timed out"),
        }
    }
}

impl std::error::Error for LoginError {}

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

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

type BrowserOpener = Arc<dyn Fn(&str) -> std::io::Result<()> + Send + Sync>;
type UrlPublisher = Arc<dyn Fn(&str) + Send + Sync>;
type RandomBytes = Arc<dyn Fn(usize) -> Result<Vec<u8>, LoginError> + Send + Sync>;
type PortBinder = Arc<dyn Fn(u16) -> std::io::Result<TcpListener> + Send + Sync>;

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

    let code = wait_for_callback(listener, &state, options.timeout, &cancellation)?;
    exchange_code(
        &options.token_endpoint,
        &code,
        &redirect_uri,
        &pkce.verifier,
    )
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
    timeout: Duration,
    cancellation: &LoginCancellation,
) -> Result<String, LoginError> {
    listener
        .set_nonblocking(true)
        .map_err(|_| LoginError::Authentication("loopback callback is unavailable"))?;
    let started = Instant::now();
    loop {
        if cancellation.is_cancelled() {
            return Err(LoginError::Cancelled);
        }
        if started.elapsed() >= timeout {
            return Err(LoginError::TimedOut);
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .map_err(|_| LoginError::Authentication("loopback callback failed"))?;
                if let Some(result) = handle_callback(
                    &mut stream,
                    state,
                    listener.local_addr().ok(),
                    started + timeout,
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
            write_response(stream, 200, "Login complete");
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

fn exchange_code(
    token_endpoint: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<ChatGptCredentials, LoginError> {
    let response = reqwest::blocking::Client::new()
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .map_err(|_| LoginError::Authentication("token exchange failed"))?;
    if !response.status().is_success() {
        return Err(LoginError::Authentication("token exchange failed"));
    }
    let token = response
        .json::<Value>()
        .map_err(|_| LoginError::Authentication("token response is invalid"))?;
    let id_token = required_token(&token, "id_token")?;
    let access_token = required_token(&token, "access_token")?;
    let refresh_token = required_token(&token, "refresh_token")?;
    let account_id = jwt_claim(id_token, ACCOUNT_ID_CLAIM)
        .as_ref()
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(LoginError::Authentication("token response is invalid"))?
        .to_owned();
    let expires_at = jwt_claim(access_token, "exp")
        .as_ref()
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds >= 0)
        .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64)))
        .and_then(format_expiry)
        .ok_or(LoginError::Authentication("token response is invalid"))?;
    Ok(ChatGptCredentials {
        access_token: access_token.to_owned(),
        refresh_token: refresh_token.to_owned(),
        account_id,
        expires_at,
    })
}

fn required_token<'a>(token: &'a Value, field: &str) -> Result<&'a str, LoginError> {
    token
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(LoginError::Authentication("token response is incomplete"))
}

fn jwt_claim(token: &str, claim: &str) -> Option<Value> {
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
    let value = payload.as_object()?.get(claim)?;
    match claim {
        ACCOUNT_ID_CLAIM => value
            .as_str()
            .filter(|value| !value.is_empty())
            .map(|value| Value::String(value.to_owned())),
        "exp" => value.as_i64().filter(|value| *value >= 0).map(Value::from),
        _ => Some(value.clone()),
    }
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
            assert!(jwt_claim(&token, ACCOUNT_ID_CLAIM).is_none());
        }
    }

    #[test]
    fn jwt_claim_accepts_only_valid_account_and_expiration_claim_types() {
        let header = r#"{"alg":"RS256"}"#;
        assert_eq!(
            jwt_claim(
                &jwt(
                    header,
                    r#"{"https://api.openai.com/auth.chatgpt_account_id":"account"}"#,
                    "signature"
                ),
                ACCOUNT_ID_CLAIM
            ),
            Some(Value::String("account".to_owned()))
        );
        for payload in [
            r#"{"https://api.openai.com/auth.chatgpt_account_id":""}"#,
            r#"{"https://api.openai.com/auth.chatgpt_account_id":false}"#,
            r#"{"exp":-1}"#,
            r#"{"exp":"1"}"#,
            r#"{"exp":18446744073709551615}"#,
        ] {
            let claim = if payload.contains("account") {
                ACCOUNT_ID_CLAIM
            } else {
                "exp"
            };
            let value = jwt_claim(&jwt(header, payload, "signature"), claim);
            assert!(value.is_none() || value.as_ref().is_some_and(Value::is_i64));
        }
        assert_eq!(
            jwt_claim(&jwt(header, r#"{"exp":0}"#, "signature"), "exp"),
            Some(Value::from(0))
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
                Duration::from_millis(250),
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
                Duration::from_millis(35),
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
