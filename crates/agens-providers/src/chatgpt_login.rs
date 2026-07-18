use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
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
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const ACCOUNT_ID_CLAIM: &str = "https://api.openai.com/auth.chatgpt_account_id";
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatGptCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    pub expires_at: String,
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

    let mut root = match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .map_err(|_| LoginError::Authentication("credentials file is invalid"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(_) => {
            return Err(LoginError::Authentication(
                "credentials file is unavailable",
            ));
        }
    };
    let root_object = root
        .as_object_mut()
        .ok_or(LoginError::Authentication("credentials file is invalid"))?;
    let entry = root_object
        .entry("openai-chatgpt".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let entry = entry
        .as_object_mut()
        .ok_or(LoginError::Authentication("credentials file is invalid"))?;
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
    entry.remove("id_token");

    write_credentials_atomically(
        path,
        &serde_json::to_vec(&root)
            .map_err(|_| LoginError::Authentication("credentials could not be encoded"))?,
    )
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
                if let Some(result) = handle_callback(&mut stream, state) {
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
) -> Option<Result<String, LoginError>> {
    let mut request = vec![0; MAX_REQUEST_BYTES + 1];
    let read = stream.read(&mut request).ok()?;
    if read > MAX_REQUEST_BYTES {
        write_response(stream, 400, "Login failed");
        return Some(Err(LoginError::Authentication(
            "callback request is invalid",
        )));
    }
    let request = std::str::from_utf8(&request[..read]).ok()?;
    let target = request.split_whitespace().nth(1)?;
    let url = Url::parse(&format!("http://localhost{target}")).ok()?;
    if url.path() != CALLBACK_PATH {
        write_response(stream, 404, "Not found");
        return None;
    }
    let query = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let actual_state = query
        .iter()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.as_str())
        .unwrap_or_default();
    if !constant_time_equal(actual_state.as_bytes(), expected_state.as_bytes()) {
        write_response(stream, 400, "Login failed");
        return Some(Err(LoginError::Authentication(
            "callback state did not match",
        )));
    }
    if query.iter().any(|(key, _)| key == "error") {
        write_response(stream, 400, "Login failed");
        return Some(Err(LoginError::Authentication("authorization was denied")));
    }
    let code = query
        .iter()
        .find(|(key, value)| key == "code" && !value.is_empty())
        .map(|(_, value)| value.clone());
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
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get(claim)
        .cloned()
}

fn format_expiry(expiry: SystemTime) -> Option<String> {
    OffsetDateTime::from(expiry).format(&Rfc3339).ok()
}

fn write_credentials_atomically(path: &Path, contents: &[u8]) -> Result<(), LoginError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(LoginError::Authentication("credentials path is invalid"))?;
    if !parent.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| {
                LoginError::Authentication("credentials directory could not be created")
            })?;
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .map_err(|_| LoginError::Authentication("credentials directory could not be secured"))?;
    let temporary = parent.join(format!(
        ".auth-login-{}-{}.json",
        std::process::id(),
        TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| LoginError::Authentication("credentials could not be persisted"))?;
        file.write_all(contents)
            .and_then(|_| file.sync_all())
            .map_err(|_| LoginError::Authentication("credentials could not be persisted"))?;
        drop(file);
        fs::rename(&temporary, path)
            .map_err(|_| LoginError::Authentication("credentials could not be persisted"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| LoginError::Authentication("credentials file could not be secured"))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| LoginError::Authentication("credentials could not be persisted"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
