use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use agens_core::{
    Error, HeadlessTurnCancellation, HeadlessTurnPortError, MessagePart, TurnProvider,
};
use agens_providers::ChatGptResponsesProvider;
use serde_json::{Value, json};

static TEMP_DIRECTORY_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
const SECRET_BODY_SENTINEL: &str = "SENTINEL_CHATGPT_REMOTE_BODY";
const SECRET_HEADER_SENTINEL: &str = "SENTINEL_CHATGPT_REMOTE_HEADER";

#[test]
fn subscription_transport_posts_the_codex_request_and_returns_text() {
    let directory = temporary_directory("transport");
    let credentials = write_credentials(&directory);
    let mut server = LocalServer::start(ServerBehavior::Sse(completed_text_sse("hello")));
    let observed_request = server.take_observed_request();
    let mut provider = provider(&credentials, &server.base_url());

    assert_eq!(
        run(&mut provider, HeadlessTurnCancellation::new()),
        Ok(vec![MessagePart::Text("hello".to_owned())])
    );

    let request = observed_request
        .recv_timeout(Duration::from_secs(1))
        .expect("server should receive the request");
    assert_eq!(request.path, "/backend-api/codex/responses");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer synthetic-access")
    );
    assert_eq!(request.header("chatgpt-account-id"), Some("account_123"));
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("accept"), Some("text/event-stream"));
    assert_eq!(request.header("originator"), Some("codex_cli_rs"));
    assert_eq!(request.header("user-agent"), Some("Agens/0.1.0"));
    assert!(
        request
            .header("session-id")
            .is_some_and(|session_id| session_id.starts_with("agens-"))
    );
    assert_eq!(
        request.body,
        json!({
            "model": "test-model",
            "instructions": "test instructions",
            "input": [{
                "role": "user",
                "content": [{"type": "input_text", "text": "test input"}],
            }],
            "tools": [],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "reasoning": {"summary": "auto"},
        })
    );

    server.join();
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_transport_maps_auth_provider_and_semantic_stream_failures_without_secrets() {
    for behavior in [
        ServerBehavior::Status(401),
        ServerBehavior::Status(403),
        ServerBehavior::Status(400),
        ServerBehavior::Status(429),
        ServerBehavior::Status(500),
        ServerBehavior::Sse("data: {\"type\":\"response.failed\"}\n\n".to_owned()),
        ServerBehavior::Sse("data: {\"type\":\"response.incomplete\"}\n\n".to_owned()),
    ] {
        let expected = if matches!(behavior, ServerBehavior::Status(401 | 403)) {
            HeadlessTurnPortError::Authentication
        } else {
            HeadlessTurnPortError::Provider
        };
        let directory = temporary_directory("status");
        let credentials = write_credentials(&directory);
        let server = LocalServer::start(behavior);
        let mut provider = provider(&credentials, &server.base_url());

        let result = run(&mut provider, HeadlessTurnCancellation::new());
        assert_eq!(result, Err(expected));

        let rendered = format!("{result:?}");
        assert!(!rendered.contains(SECRET_BODY_SENTINEL));
        assert!(!rendered.contains(SECRET_HEADER_SENTINEL));
        server.join();
        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }
}

#[test]
fn subscription_transport_keeps_cancellation_and_timeout_distinct() {
    for (behavior, cancellation, expected) in [
        (
            ServerBehavior::WaitForClientClose,
            HeadlessTurnCancellation::new(),
            HeadlessTurnPortError::Cancelled,
        ),
        (
            ServerBehavior::WaitForClientClose,
            HeadlessTurnCancellation::with_deadline(Duration::from_millis(25)),
            HeadlessTurnPortError::TimedOut,
        ),
    ] {
        let directory = temporary_directory("stop");
        let credentials = write_credentials(&directory);
        let mut server = LocalServer::start(behavior);
        let observed_request = server.take_observed_request();
        let mut provider = provider(&credentials, &server.base_url());
        let canceller = cancellation.clone();
        let cancellation_thread = thread::spawn(move || {
            observed_request
                .recv_timeout(Duration::from_secs(1))
                .expect("server should observe the request before cancellation");
            if expected == HeadlessTurnPortError::Cancelled {
                canceller.cancel();
            }
        });

        assert_eq!(run(&mut provider, cancellation), Err(expected));

        cancellation_thread
            .join()
            .expect("cancellation thread should finish");
        server.join();
        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }
}

#[test]
fn subscription_constructor_rejects_incomplete_existing_credentials_without_an_api_key() {
    let directory = temporary_directory("credentials");
    let credentials = directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"synthetic-access"}}"#,
    )
    .expect("credentials should be written");

    assert!(matches!(
        ChatGptResponsesProvider::from_credentials_with_timeout(
            &credentials,
            None,
            "test-model".to_owned(),
            "test instructions".to_owned(),
            "test input".to_owned(),
            Duration::from_secs(1),
        ),
        Err(Error::Auth(error)) if error ==
            "ChatGPT authentication required: credentials are incomplete"
    ));

    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

fn provider(credentials: &Path, base_url: &str) -> ChatGptResponsesProvider {
    ChatGptResponsesProvider::from_credentials_with_timeout(
        credentials,
        Some(base_url),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured")
}

fn run(
    provider: &mut ChatGptResponsesProvider,
    cancellation: HeadlessTurnCancellation,
) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime should build");

    runtime.block_on(provider.next_parts(&[], &cancellation))
}

fn write_credentials(directory: &Path) -> PathBuf {
    let credentials = directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"synthetic-access","refresh_token":"synthetic-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
    )
    .expect("credentials should be written");
    credentials
}

fn temporary_directory(name: &str) -> PathBuf {
    let sequence = TEMP_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "agens-providers-chatgpt-http-{name}-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temporary directory should be created");
    path
}

fn completed_text_sse(text: &str) -> String {
    format!(
        "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\n\
data: {{\"type\":\"response.completed\"}}\n\n"
    )
}

#[derive(Clone)]
enum ServerBehavior {
    Status(u16),
    Sse(String),
    WaitForClientClose,
}

struct ObservedRequest {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl ObservedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find_map(|(candidate, value)| (candidate == name).then_some(value.as_str()))
    }
}

struct LocalServer {
    address: std::net::SocketAddr,
    observed_request: Option<mpsc::Receiver<ObservedRequest>>,
    worker: thread::JoinHandle<()>,
}

impl LocalServer {
    fn start(behavior: ServerBehavior) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("server should bind");
        let address = listener
            .local_addr()
            .expect("server address should be available");
        let (sender, observed_request) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("server should accept a request");
            sender
                .send(read_request(&stream))
                .expect("test should receive the request");

            match behavior {
                ServerBehavior::Status(status) => write_status(&mut stream, status),
                ServerBehavior::Sse(events) => write_sse(&mut stream, &events),
                ServerBehavior::WaitForClientClose => wait_for_client_close(&stream),
            }
        });

        Self {
            address,
            observed_request: Some(observed_request),
            worker,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/backend-api/codex", self.address)
    }

    fn take_observed_request(&mut self) -> mpsc::Receiver<ObservedRequest> {
        self.observed_request
            .take()
            .expect("request receiver should only be taken once")
    }

    fn join(self) {
        self.worker.join().expect("server worker should finish");
    }
}

fn read_request(stream: &TcpStream) -> ObservedRequest {
    let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .expect("request line should be readable");
    let path = request_line
        .split_whitespace()
        .nth(1)
        .expect("request line should contain a path")
        .to_owned();

    let mut headers = Vec::new();
    let mut content_length = None;
    loop {
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .expect("header should be readable");
        if header == "\r\n" {
            break;
        }
        let (name, value) = header
            .trim_end()
            .split_once(": ")
            .expect("header should be well formed");
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(
                value
                    .parse::<usize>()
                    .expect("content length should be numeric"),
            );
        }
        headers.push((name.to_ascii_lowercase(), value.to_owned()));
    }

    let mut body = vec![0; content_length.expect("request should have a content length")];
    reader
        .read_exact(&mut body)
        .expect("body should be readable");
    ObservedRequest {
        path,
        headers,
        body: serde_json::from_slice(&body).expect("body should be JSON"),
    }
}

fn write_status(stream: &mut TcpStream, status: u16) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} Test\r\nX-Secret: {SECRET_HEADER_SENTINEL}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{SECRET_BODY_SENTINEL}",
                SECRET_BODY_SENTINEL.len()
            )
            .as_bytes(),
        )
        .expect("status response should be written");
}

fn write_sse(stream: &mut TcpStream, events: &str) {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .expect("SSE headers should be written");
    stream
        .write_all(events.as_bytes())
        .expect("SSE body should be written");
}

fn wait_for_client_close(stream: &TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("read timeout should be configured");
    let mut byte = [0_u8; 1];
    let _ = stream
        .try_clone()
        .expect("stream should clone")
        .read(&mut byte);
}
