//! A scripted HTTP provider fake: a real socket speaking a real streaming
//! protocol, driven by a typed script of turns.
//!
//! A journey test needs the agent loop to be real and only the model to be a
//! fixture. That rules out mocking the HTTP client, and it rules out a server
//! that answers a single `accept()`: a tool loop is at least two requests, and
//! a delegation is at least four. This server accepts connections until it is
//! stopped, serves every request that arrives on each of them, and hands back
//! what it received so a test can assert on what the agent *sent* — history,
//! declared tools, system prompt — and not only on what it printed.
//!
//! The script is consumed in order and shared across connections, because the
//! agent's turns are sequential even when its transport is not.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long a scripted server waits for the agent to do something before it
/// gives up. Long enough that a loaded gate does not decide the outcome, short
/// enough that a genuinely stuck journey fails instead of hanging the suite.
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an idle accept loop sleeps between polls while it waits for either
/// a connection or the stop flag.
const ACCEPT_POLL: Duration = Duration::from_millis(2);

/// The streaming dialect a scripted turn is rendered in.
///
/// Agens speaks two, and a harness that only emits one is a harness for half
/// the providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptedDialect {
    /// The OpenAI `/responses` API, used by the `openai-api` and ChatGPT
    /// providers.
    Responses,
    /// Chat Completions, used by Moonshot/Kimi.
    ChatCompletions,
}

/// One scripted model turn.
#[derive(Clone, Debug)]
pub enum ScriptedTurn {
    /// The model asks for a tool.
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// The model answers with text and stops.
    Text { content: String },
    /// The endpoint fails with an HTTP status and body instead of streaming.
    Error { status: u16, body: String },
    /// The endpoint sends stream headers and then nothing at all for
    /// `duration`, so the client's read timeout is what ends the turn.
    Stall { duration: Duration },
    /// The endpoint starts streaming a text answer and closes the connection
    /// mid-stream, without a terminating event.
    Truncate,
    /// Verbatim HTTP response bytes, for a shape the typed turns do not model.
    Raw { response: String },
}

impl ScriptedTurn {
    /// A tool call turn.
    pub fn tool_call(
        call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self::ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// A final text turn.
    pub fn text(content: impl Into<String>) -> Self {
        Self::Text {
            content: content.into(),
        }
    }

    /// An HTTP failure instead of a stream.
    pub fn error(status: u16, body: impl Into<String>) -> Self {
        Self::Error {
            status,
            body: body.into(),
        }
    }

    /// Stream headers followed by silence for `duration`.
    pub fn stall(duration: Duration) -> Self {
        Self::Stall { duration }
    }

    /// A stream cut before its terminating event.
    pub fn truncate() -> Self {
        Self::Truncate
    }

    /// Verbatim response bytes.
    pub fn raw(response: impl Into<String>) -> Self {
        Self::Raw {
            response: response.into(),
        }
    }

    /// A short label naming this turn in a script-exhaustion failure.
    fn label(&self) -> String {
        match self {
            Self::ToolCall { name, .. } => format!("tool call {name}"),
            Self::Text { content } => format!("text {content:?}"),
            Self::Error { status, .. } => format!("error {status}"),
            Self::Stall { duration } => format!("stall {duration:?}"),
            Self::Truncate => "truncate".to_owned(),
            Self::Raw { .. } => "raw response".to_owned(),
        }
    }
}

/// One request the scripted server received, kept whole so a test can assert
/// on what the agent sent.
#[derive(Clone, Debug)]
pub struct ObservedRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: String,
    lane: Lane,
}

impl ObservedRequest {
    /// The request method.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The request target, including its path.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// A header value by its lowercased name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find_map(|(candidate, value)| (candidate == name).then_some(value.as_str()))
    }

    /// The request body as sent.
    pub fn body(&self) -> &str {
        &self.body
    }

    /// The request body parsed as JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("scripted provider request should be JSON")
    }

    /// Whether this request was routed to the delegated child's script.
    pub fn is_child(&self) -> bool {
        matches!(self.lane, Lane::Child)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    Main,
    Child,
}

/// A script: the turns the main session gets, and optionally the turns a
/// delegated child session gets instead.
///
/// A child starts a conversation of its own, seeded with the delegation
/// prompt, and the server recognises that opening request by the prompt
/// appearing in its body. Its continuations are recognised by the response
/// they follow: the `/responses` dialect replaces the history with a
/// `previous_response_id`, so the delegation prompt is in the opening request
/// only, and routing by the marker alone would send a child's second turn back
/// to the main script.
#[derive(Clone, Debug, Default)]
pub struct Script {
    main: Vec<ScriptedTurn>,
    child_marker: Option<String>,
    child: Vec<ScriptedTurn>,
}

impl Script {
    /// A script with only a main session.
    pub fn new(turns: impl IntoIterator<Item = ScriptedTurn>) -> Self {
        Self {
            main: turns.into_iter().collect(),
            child_marker: None,
            child: Vec::new(),
        }
    }

    /// Adds a child lane, selected by `marker` appearing in a request body.
    pub fn with_child(
        mut self,
        marker: impl Into<String>,
        turns: impl IntoIterator<Item = ScriptedTurn>,
    ) -> Self {
        self.child_marker = Some(marker.into());
        self.child = turns.into_iter().collect();
        self
    }
}

/// The scripted provider server.
pub struct ScriptedProvider {
    address: SocketAddr,
    dialect: ScriptedDialect,
    state: Arc<Mutex<ServerState>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

struct ServerState {
    main: VecDeque<ScriptedTurn>,
    child_marker: Option<String>,
    child: VecDeque<ScriptedTurn>,
    requests: Vec<ObservedRequest>,
    /// The lane each response identifier this server issued belongs to, so a
    /// continuation that carries only `previous_response_id` still lands in
    /// the conversation it continues.
    response_lanes: std::collections::HashMap<String, Lane>,
    /// Requests that arrived after the relevant lane's script ran out. A test
    /// reads this as a failure rather than the server inventing a turn.
    unscripted: Vec<String>,
}

impl ScriptedProvider {
    /// Starts a server on an ephemeral loopback port serving `script` in
    /// `dialect`.
    pub fn start(dialect: ScriptedDialect, script: Script) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("scripted provider should bind");
        listener
            .set_nonblocking(true)
            .expect("scripted provider listener should be pollable");
        let address = listener
            .local_addr()
            .expect("scripted provider should have an address");

        let state = Arc::new(Mutex::new(ServerState {
            main: script.main.into(),
            child_marker: script.child_marker,
            child: script.child.into(),
            requests: Vec::new(),
            response_lanes: std::collections::HashMap::new(),
            unscripted: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let worker_state = Arc::clone(&state);
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut connections = Vec::new();

            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let connection_state = Arc::clone(&worker_state);
                        let connection_stop = Arc::clone(&worker_stop);
                        connections.push(thread::spawn(move || {
                            serve_connection(stream, dialect, &connection_state, &connection_stop);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_POLL);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }

            for connection in connections {
                let _ = connection.join();
            }
        });

        Self {
            address,
            dialect,
            state,
            stop,
            worker: Some(worker),
        }
    }

    /// The base URL a `provider.base_url` should point at.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The address the server is listening on.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Every request received so far.
    pub fn requests(&self) -> Vec<ObservedRequest> {
        self.locked().requests.clone()
    }

    /// The requests routed to the delegated child's script.
    pub fn child_requests(&self) -> Vec<ObservedRequest> {
        self.requests()
            .into_iter()
            .filter(ObservedRequest::is_child)
            .collect()
    }

    /// Blocks until at least `count` requests have been received, then returns
    /// them.
    ///
    /// Waiting on the count rather than sleeping is what keeps a journey from
    /// needing a `sleep` to pass: a machine that is merely slow still gets
    /// there, and a journey that never sends the request fails with the count
    /// it reached instead of hanging.
    pub fn wait_for_requests(&self, count: usize) -> Vec<ObservedRequest> {
        let deadline = Instant::now() + OBSERVATION_TIMEOUT;

        loop {
            let requests = self.requests();
            if requests.len() >= count {
                return requests;
            }

            assert!(
                Instant::now() < deadline,
                "scripted provider expected {count} requests, received {}",
                requests.len()
            );
            thread::sleep(ACCEPT_POLL);
        }
    }

    /// Fails if the agent left scripted turns unused, or asked for a turn the
    /// script did not have.
    ///
    /// A journey that only checks its final output cannot tell a loop that ran
    /// to completion from one that stopped early and happened to print
    /// something plausible. This is the assertion that tells them apart.
    pub fn assert_script_consumed(&self) {
        let state = self.locked();

        assert!(
            state.unscripted.is_empty(),
            "scripted provider received requests its script did not cover: {:?}",
            state.unscripted
        );
        let remaining: Vec<String> = state
            .main
            .iter()
            .chain(state.child.iter())
            .map(ScriptedTurn::label)
            .collect();
        assert!(
            remaining.is_empty(),
            "scripted provider script was not consumed, {} turn(s) left: {remaining:?}",
            remaining.len()
        );
    }

    /// The `[provider]` configuration fragment pointing at this server.
    ///
    /// A journey needs a `config.toml`, and writing one by hand in every test
    /// is how eleven of them drifted apart. `extra` is appended verbatim for
    /// the sections a journey adds on top, such as `[permissions]`.
    pub fn provider_configuration(&self, data_directory: &Path, extra: &str) -> String {
        let provider = self.provider_name();
        let model = match self.dialect {
            ScriptedDialect::Responses => "openai-api/gpt-4.1",
            ScriptedDialect::ChatCompletions => "moonshotai/kimi-k3",
        };

        format!(
            "[provider]\ntype = \"{provider}\"\nmodel = \"{model}\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n{extra}",
            self.base_url(),
            data_directory.display(),
        )
    }

    /// Writes `config.toml` and `auth.json` into `config_home`, pointed at this
    /// server, so a journey is five lines and not eighty.
    pub fn write_configuration(&self, config_home: &Path, data_directory: &Path, extra: &str) {
        std::fs::create_dir_all(config_home).expect("journey config home should be created");
        std::fs::write(
            config_home.join("config.toml"),
            self.provider_configuration(data_directory, extra),
        )
        .expect("journey configuration should be written");

        std::fs::write(
            config_home.join("auth.json"),
            format!(
                r#"{{"{}": {{"api_key": "fixture"}}}}"#,
                self.provider_name()
            ),
        )
        .expect("journey credentials should be written");
    }

    /// The configured provider that speaks this server's dialect.
    fn provider_name(&self) -> &'static str {
        match self.dialect {
            ScriptedDialect::Responses => "openai-api",
            ScriptedDialect::ChatCompletions => "moonshotai",
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, ServerState> {
        self.state
            .lock()
            .expect("scripted provider state should not be poisoned")
    }
}

impl Drop for ScriptedProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve_connection(
    stream: TcpStream,
    dialect: ScriptedDialect,
    state: &Mutex<ServerState>,
    stop: &AtomicBool,
) {
    let mut writer = stream.try_clone().expect("scripted stream should clone");
    let mut reader = BufReader::new(stream);

    while !stop.load(Ordering::Acquire) {
        let Some(request) = read_request(&mut reader, state) else {
            return;
        };

        // A request the script does not cover gets no reply at all, only a
        // closed connection. Answering it with a status of the server's own
        // invention would put a turn the test never wrote into the run's
        // outcome, which is exactly what a scripted model exists to prevent;
        // the request is still recorded, so `assert_script_consumed` reports
        // it.
        let Some(turn) = take_turn(state, &request) else {
            let _ = writer.shutdown(Shutdown::Both);
            return;
        };

        if !write_turn(&mut writer, dialect, &turn) {
            return;
        }
    }
}

/// Reads one request, recording it, or returns `None` when the client is done.
fn read_request(
    reader: &mut BufReader<TcpStream>,
    state: &Mutex<ServerState>,
) -> Option<ObservedRequest> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();

    let mut headers = Vec::new();
    let mut content_length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            return None;
        }
        if header == "\r\n" || header == "\n" {
            break;
        }
        let (name, value) = header.split_once(':')?;
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        if name == "content-length" {
            content_length = value.parse().ok()?;
        }
        headers.push((name, value));
    }

    let mut body = vec![0_u8; content_length];
    reader.read_exact(&mut body).ok()?;
    let body = String::from_utf8(body).ok()?;

    let mut state = state
        .lock()
        .expect("scripted provider state should not be poisoned");
    let lane = continued_lane(&state.response_lanes, &body)
        .or_else(|| {
            state
                .child_marker
                .as_deref()
                .filter(|marker| body.contains(marker))
                .map(|_| Lane::Child)
        })
        .unwrap_or(Lane::Main);
    let request = ObservedRequest {
        method,
        target,
        headers,
        body,
        lane,
    };
    state.requests.push(request.clone());

    Some(request)
}

/// The lane a request continues, when it names a response this server issued.
fn continued_lane(
    response_lanes: &std::collections::HashMap<String, Lane>,
    body: &str,
) -> Option<Lane> {
    let previous = serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("previous_response_id")?
        .as_str()?
        .to_owned();

    response_lanes.get(&previous).copied()
}

/// Pops the next turn for the request's lane, recording an exhausted lane
/// rather than inventing a reply for it.
fn take_turn(state: &Mutex<ServerState>, request: &ObservedRequest) -> Option<ScriptedTurn> {
    let mut state = state
        .lock()
        .expect("scripted provider state should not be poisoned");

    let turn = match request.lane {
        Lane::Main => state.main.pop_front(),
        Lane::Child => state.child.pop_front(),
    };
    if let Some(ScriptedTurn::ToolCall { call_id, .. }) = &turn {
        state
            .response_lanes
            .insert(format!("response_{call_id}"), request.lane);
    }
    if turn.is_none() {
        let lane = request.lane;
        state
            .unscripted
            .push(format!("{lane:?} lane exhausted at {}", request.target));
    }

    turn
}

/// Writes one turn. Returns whether the connection may serve another request.
fn write_turn(stream: &mut TcpStream, dialect: ScriptedDialect, turn: &ScriptedTurn) -> bool {
    match turn {
        ScriptedTurn::ToolCall {
            call_id,
            name,
            arguments,
        } => {
            write_keep_alive_stream(stream, &tool_call_events(dialect, call_id, name, arguments));
            true
        }
        ScriptedTurn::Text { content } => {
            write_keep_alive_stream(stream, &text_events(dialect, content));
            true
        }
        ScriptedTurn::Error { status, body } => {
            write_all(
                stream,
                format!(
                    "HTTP/1.1 {status} Scripted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
            true
        }
        ScriptedTurn::Stall { duration } => {
            write_all(stream, STREAM_HEADERS_CLOSE);
            thread::sleep(*duration);
            let _ = stream.shutdown(Shutdown::Both);
            false
        }
        ScriptedTurn::Truncate => {
            write_all(stream, STREAM_HEADERS_CLOSE);
            let partial = match dialect {
                ScriptedDialect::Responses => {
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n"
                }
                ScriptedDialect::ChatCompletions => {
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n"
                }
            };
            write_all(stream, partial.as_bytes());
            let _ = stream.shutdown(Shutdown::Both);
            false
        }
        ScriptedTurn::Raw { response } => {
            write_all(stream, response.as_bytes());
            false
        }
    }
}

const STREAM_HEADERS_CLOSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";

/// Writes a complete stream with a length, so the client may reuse the
/// connection for the next turn instead of reconnecting for every one.
fn write_keep_alive_stream(stream: &mut TcpStream, body: &str) {
    write_all(
        stream,
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
}

fn tool_call_events(
    dialect: ScriptedDialect,
    call_id: &str,
    name: &str,
    arguments: &str,
) -> String {
    match dialect {
        ScriptedDialect::Responses => sse(&[
            serde_json::json!({
                "type": "response.output_item.added",
                "item": {
                    "id": format!("item_{call_id}"),
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": "",
                },
            }),
            serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": format!("item_{call_id}"),
                "arguments": arguments,
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {"id": format!("response_{call_id}")},
            }),
        ]),
        ScriptedDialect::ChatCompletions => sse_terminated(&[
            serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {"tool_calls": [{
                        "index": 0,
                        "id": call_id,
                        "type": "function",
                        "function": {"name": name, "arguments": ""},
                    }]},
                    "finish_reason": null,
                }],
            }),
            serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {"tool_calls": [{
                        "index": 0,
                        "function": {"arguments": arguments},
                    }]},
                    "finish_reason": null,
                }],
            }),
            serde_json::json!({
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
            }),
        ]),
    }
}

fn text_events(dialect: ScriptedDialect, content: &str) -> String {
    match dialect {
        ScriptedDialect::Responses => sse(&[
            serde_json::json!({"type": "response.output_text.delta", "delta": content}),
            serde_json::json!({"type": "response.completed"}),
        ]),
        ScriptedDialect::ChatCompletions => sse_terminated(&[
            serde_json::json!({
                "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}],
            }),
            serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        ]),
    }
}

fn sse(events: &[serde_json::Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn sse_terminated(events: &[serde_json::Value]) -> String {
    format!("{}data: [DONE]\n\n", sse(events))
}

/// Writes bytes to a client that may already be gone.
///
/// A client that has what it needs closes without reading the rest, and that
/// is a successful turn, not a server error: panicking here would turn the
/// agent's correct behaviour into a worker panic with no useful message.
fn write_all(stream: &mut TcpStream, bytes: &[u8]) {
    let _ = stream.write_all(bytes);
    let _ = stream.flush();
}
