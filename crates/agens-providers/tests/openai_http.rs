use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use agens_core::{HeadlessTurnCancellation, HeadlessTurnPortError, TurnEvent, TurnProvider};
use agens_providers::{OpenAiFunctionTool, OpenAiResponsesProvider};
use serde_json::json;

const SECRET_BODY_SENTINEL: &str = "SENTINEL_REMOTE_ERROR_BODY";
const SECRET_HEADER_SENTINEL: &str = "SENTINEL_REMOTE_ERROR_HEADER";

#[test]
fn cancellation_interrupts_connect_headers_stalled_body_and_late_events() {
    for mode in [
        ServerMode::StalledConnect,
        ServerMode::DelayedHeaders,
        ServerMode::StalledBody,
        ServerMode::LateEvent,
    ] {
        let mut server = LocalResponsesServer::start(mode);
        let cancellation = HeadlessTurnCancellation::new();
        let canceller = cancellation.clone();
        let observed_request =
            (!matches!(mode, ServerMode::StalledConnect)).then(|| server.take_observed_request());

        let canceller_thread = thread::spawn(move || {
            if let Some(observed_request) = observed_request {
                observed_request
                    .recv_timeout(Duration::from_secs(1))
                    .expect("server should observe the request before cancellation");
            } else {
                thread::sleep(Duration::from_millis(10));
            }
            canceller.cancel();
        });

        let started_at = Instant::now();
        let result = run_provider(server.base_url(), cancellation, Duration::from_secs(1));

        assert_eq!(result, Err(HeadlessTurnPortError::Cancelled));
        assert!(started_at.elapsed() < Duration::from_millis(250));
        canceller_thread
            .join()
            .expect("canceller thread should finish");
        server.join();
    }
}

#[test]
fn one_hundred_same_process_cancellations_and_timeouts_have_bounded_resources() {
    let baseline = ResourceSnapshot::capture();

    for _ in 0..100 {
        let server = LocalResponsesServer::start(ServerMode::DelayedHeaders);
        let cancellation = HeadlessTurnCancellation::with_deadline(Duration::from_millis(25));

        let result = run_provider(server.base_url(), cancellation, Duration::from_secs(1));

        assert_eq!(result, Err(HeadlessTurnPortError::TimedOut));
        server.join();
    }

    for _ in 0..100 {
        let mut server = LocalResponsesServer::start(ServerMode::DelayedHeaders);
        let cancellation = HeadlessTurnCancellation::new();
        let observed_request = server.take_observed_request();
        let canceller = cancellation.clone();
        let cancellation_thread = thread::spawn(move || {
            observed_request
                .recv_timeout(Duration::from_secs(1))
                .expect("server should observe the request before cancellation");
            canceller.cancel();
        });

        let result = run_provider(server.base_url(), cancellation, Duration::from_secs(1));

        assert_eq!(result, Err(HeadlessTurnPortError::Cancelled));
        cancellation_thread
            .join()
            .expect("cancellation thread should finish");
        server.join();
    }

    let after = ResourceSnapshot::capture();
    assert!(
        after.tasks <= baseline.tasks + 2,
        "task count grew from {} to {}",
        baseline.tasks,
        after.tasks
    );
    assert!(
        after.file_descriptors <= baseline.file_descriptors + 2,
        "file descriptor count grew from {} to {}",
        baseline.file_descriptors,
        after.file_descriptors
    );
}

#[test]
fn cancellation_wins_when_a_remote_error_completes_after_cancellation() {
    let mut server = LocalResponsesServer::start(ServerMode::CancelledError);
    let cancellation = HeadlessTurnCancellation::new();
    let observed_request = server.take_observed_request();
    let canceller = cancellation.clone();
    let cancellation_thread = thread::spawn(move || {
        observed_request
            .recv_timeout(Duration::from_secs(1))
            .expect("server should observe the request");
        canceller.cancel();
    });

    let result = run_provider(server.base_url(), cancellation, Duration::from_secs(1));

    assert_eq!(result, Err(HeadlessTurnPortError::Cancelled));
    cancellation_thread
        .join()
        .expect("cancellation thread should finish");
    server.join();
}

#[test]
fn malformed_unterminated_or_oversized_frames_and_remote_errors_are_sanitized_provider_failures() {
    for mode in [
        ServerMode::MalformedFrame,
        ServerMode::UnterminatedOversizedFrame,
        ServerMode::OversizedFrame,
        ServerMode::ErrorBody,
    ] {
        let server = LocalResponsesServer::start(mode);
        let result = run_provider(
            server.base_url(),
            HeadlessTurnCancellation::with_deadline(Duration::from_secs(1)),
            Duration::from_secs(1),
        );

        assert_eq!(result, Err(HeadlessTurnPortError::Provider));
        server.join();
    }
}

#[test]
fn tool_enabled_initial_request_uses_flat_function_tool_json() {
    let mut server = LocalResponsesServer::start_scripted(vec![
        concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_initial\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_initial\"}}\n\n"
        )
        .to_owned(),
    ]);
    let observed_body = server.take_observed_body();
    let tool = OpenAiFunctionTool::new(
        "lookup_weather",
        "Looks up current weather.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
    )
    .expect("tool should be valid");
    let mut provider = OpenAiResponsesProvider::from_api_key_with_tools_and_timeout(
        "test-api-key".into(),
        Some(&server.base_url()),
        "test-model".into(),
        "test prompt".into(),
        vec![tool],
        Duration::from_secs(1),
    )
    .expect("provider should be configured");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime should build");

    runtime
        .block_on(provider.next_parts(&[], &HeadlessTurnCancellation::new()))
        .expect("initial response should complete");

    assert_eq!(
        observed_body
            .recv_timeout(Duration::from_secs(1))
            .expect("server should capture initial request"),
        json!({
            "model": "test-model",
            "input": [{"role": "user", "content": "test prompt"}],
            "tools": [{
                "type": "function",
                "name": "lookup_weather",
                "description": "Looks up current weather.",
                "parameters": {"type": "object", "properties": {}, "additionalProperties": false},
                "strict": true,
            }],
            "stream": true,
        })
    );
    server.join();
}

#[test]
fn sends_ordered_tool_outputs_in_a_second_responses_request() {
    let mut server = LocalResponsesServer::start_scripted(vec![
        concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_initial\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_first\",\"call_id\":\"call_first\",\"name\":\"first\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_second\",\"call_id\":\"call_second\",\"name\":\"second\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_second\",\"arguments\":\"{}\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_first\",\"arguments\":\"{}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_initial\"}}\n\n"
        )
        .to_owned(),
        concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_second\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_second\"}}\n\n"
        )
        .to_owned(),
    ]);
    let observed_body = server.take_observed_body();
    let tool = OpenAiFunctionTool::new(
        "lookup_weather",
        "Looks up current weather.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
    )
    .expect("tool should be valid");
    let mut provider = OpenAiResponsesProvider::from_api_key_with_tools_and_timeout(
        "test-api-key".into(),
        Some(&server.base_url()),
        "test-model".into(),
        "test prompt".into(),
        vec![tool],
        Duration::from_secs(1),
    )
    .expect("provider should be configured");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime should build");
    let cancellation = HeadlessTurnCancellation::new();

    runtime
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("initial tool-call response should complete");
    let parts = runtime
        .block_on(provider.next_parts(
            &[
                TurnEvent::ToolResult(agens_core::MessagePart::ToolResult {
                    tool_call_id: "call_second".to_owned(),
                    content: "second result".to_owned(),
                    is_error: false,
                }),
                TurnEvent::ToolResult(agens_core::MessagePart::ToolResult {
                    tool_call_id: "call_first".to_owned(),
                    content: "first result".to_owned(),
                    is_error: false,
                }),
            ],
            &cancellation,
        ))
        .expect("continuation should complete");

    assert_eq!(
        parts,
        vec![agens_core::MessagePart::Text("done".to_owned())]
    );
    let _initial_body = observed_body
        .recv_timeout(Duration::from_secs(1))
        .expect("server should capture initial request");
    assert_eq!(
        observed_body
            .recv_timeout(Duration::from_secs(1))
            .expect("server should capture continuation request"),
        json!({
            "model": "test-model",
            "previous_response_id": "resp_initial",
            "input": [
                {"type": "function_call_output", "call_id": "call_first", "output": "first result"},
                {"type": "function_call_output", "call_id": "call_second", "output": "second result"},
            ],
            "tools": [{
                "type": "function",
                "name": "lookup_weather",
                "description": "Looks up current weather.",
                "parameters": {"type": "object", "properties": {}, "additionalProperties": false},
                "strict": true,
            }],
            "stream": true,
        })
    );
    server.join();
}

#[test]
fn continues_through_two_tool_rounds_and_sanitizes_error_outputs() {
    let mut server = LocalResponsesServer::start_scripted(vec![
        tool_call_response("resp_first", "fc_first", "call_first"),
        tool_call_response("resp_second", "fc_second", "call_second"),
        completed_text_response("resp_third", "complete"),
    ]);
    let observed_body = server.take_observed_body();
    let mut provider = scripted_provider(server.base_url());
    let runtime = provider_runtime();
    let cancellation = HeadlessTurnCancellation::new();
    let first_events = [tool_result(
        "call_first",
        "internal failure: secret=hidden",
        true,
    )];
    let second_events = [
        tool_result("call_first", "internal failure: secret=hidden", true),
        tool_result("call_second", "second result", false),
    ];

    runtime
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("first tool-call response should complete");
    runtime
        .block_on(provider.next_parts(&first_events, &cancellation))
        .expect("second tool-call response should complete");
    assert_eq!(
        runtime
            .block_on(provider.next_parts(&second_events, &cancellation))
            .expect("third response should complete"),
        vec![agens_core::MessagePart::Text("complete".to_owned())]
    );

    let _initial = observed_body
        .recv_timeout(Duration::from_secs(1))
        .expect("initial body");
    assert_eq!(
        observed_body
            .recv_timeout(Duration::from_secs(1))
            .expect("second body"),
        json!({
            "model": "test-model",
            "previous_response_id": "resp_first",
            "input": [{"type": "function_call_output", "call_id": "call_first", "output": "Tool execution failed"}],
            "stream": true,
        })
    );
    assert_eq!(
        observed_body
            .recv_timeout(Duration::from_secs(1))
            .expect("third body"),
        json!({
            "model": "test-model",
            "previous_response_id": "resp_second",
            "input": [{"type": "function_call_output", "call_id": "call_second", "output": "second result"}],
            "stream": true,
        })
    );
    server.join();
}

#[test]
fn rejects_missing_duplicate_and_foreign_tool_results_before_a_continuation_request() {
    for events in [
        Vec::new(),
        vec![
            tool_result("call_first", "first", false),
            tool_result("call_first", "again", false),
        ],
        vec![tool_result("foreign", "foreign", false)],
    ] {
        let mut server = LocalResponsesServer::start_scripted(vec![tool_call_response(
            "resp_first",
            "fc_first",
            "call_first",
        )]);
        let observed_body = server.take_observed_body();
        let mut provider = scripted_provider(server.base_url());
        let runtime = provider_runtime();
        let cancellation = HeadlessTurnCancellation::new();

        runtime
            .block_on(provider.next_parts(&[], &cancellation))
            .expect("initial tool-call response should complete");
        assert_eq!(
            runtime.block_on(provider.next_parts(&events, &cancellation)),
            Err(HeadlessTurnPortError::Provider)
        );
        assert!(
            observed_body
                .recv_timeout(Duration::from_secs(1))
                .expect("initial request should be observed")
                .get("input")
                .is_some()
        );
        assert!(
            observed_body
                .recv_timeout(Duration::from_millis(25))
                .is_err()
        );
        server.join();
    }
}

fn tool_call_response(response_id: &str, item_id: &str, call_id: &str) -> String {
    format!(
        "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{response_id}\"}}}}\n\n\
data: {{\"type\":\"response.output_item.added\",\"item\":{{\"type\":\"function_call\",\"id\":\"{item_id}\",\"call_id\":\"{call_id}\",\"name\":\"lookup\",\"arguments\":\"\"}}}}\n\n\
data: {{\"type\":\"response.function_call_arguments.done\",\"item_id\":\"{item_id}\",\"arguments\":\"{{}}\"}}\n\n\
data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{response_id}\"}}}}\n\n"
    )
}

fn completed_text_response(response_id: &str, text: &str) -> String {
    format!(
        "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{response_id}\"}}}}\n\n\
data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\n\
data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{response_id}\"}}}}\n\n"
    )
}

fn tool_result(call_id: &str, content: &str, is_error: bool) -> TurnEvent {
    TurnEvent::ToolResult(agens_core::MessagePart::ToolResult {
        tool_call_id: call_id.to_owned(),
        content: content.to_owned(),
        is_error,
    })
}

fn scripted_provider(base_url: String) -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::from_api_key_with_timeout(
        "test-api-key".into(),
        Some(&base_url),
        "test-model".into(),
        "test prompt".into(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured")
}

fn provider_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime should build")
}

fn run_provider(
    base_url: String,
    cancellation: HeadlessTurnCancellation,
    timeout: Duration,
) -> Result<(), HeadlessTurnPortError> {
    let mut provider = OpenAiResponsesProvider::from_api_key_with_timeout(
        "test-api-key".into(),
        Some(&base_url),
        "test-model".into(),
        "test prompt".into(),
        timeout,
    )
    .expect("provider should be configured");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime should build");

    runtime
        .block_on(provider.next_parts(&[], &cancellation))
        .map(|_| ())
}

#[derive(Clone, Copy)]
enum ServerMode {
    StalledConnect,
    DelayedHeaders,
    StalledBody,
    LateEvent,
    MalformedFrame,
    OversizedFrame,
    UnterminatedOversizedFrame,
    ErrorBody,
    CancelledError,
}

struct LocalResponsesServer {
    address: std::net::SocketAddr,
    observed_request: Option<mpsc::Receiver<()>>,
    observed_body: Option<mpsc::Receiver<serde_json::Value>>,
    worker: thread::JoinHandle<()>,
}

impl LocalResponsesServer {
    fn start(mode: ServerMode) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("server should bind");
        let address = listener
            .local_addr()
            .expect("server address should be available");
        let (observed_sender, observed_request) = mpsc::channel();
        let worker = thread::spawn(move || {
            if matches!(mode, ServerMode::StalledConnect) {
                let mut backlog_fillers = Vec::new();
                listener
                    .set_nonblocking(true)
                    .expect("listener should be nonblocking while the connect backlog is filled");
                let mut backlog_full = false;
                for _ in 0..512 {
                    match TcpStream::connect_timeout(&address, Duration::from_millis(5)) {
                        Ok(stream) => backlog_fillers.push(stream),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                            ) =>
                        {
                            backlog_full = true;
                            break;
                        }
                        Err(error) => panic!("backlog fill should only stop when full: {error}"),
                    }
                }
                assert!(
                    !backlog_fillers.is_empty(),
                    "the local listener should accept at least one queued connect"
                );
                assert!(
                    backlog_full,
                    "the local connect backlog should fill before the request starts"
                );
                thread::sleep(Duration::from_millis(250));
                return;
            }

            let (mut stream, _) = listener.accept().expect("server should accept one request");
            read_request(&stream);
            observed_sender
                .send(())
                .expect("test should receive request observation");

            match mode {
                ServerMode::StalledConnect => {
                    unreachable!("stalled connect returns before handling")
                }
                ServerMode::DelayedHeaders => wait_for_client_close(&stream),
                ServerMode::StalledBody => {
                    write_sse_headers(&mut stream);
                    wait_for_client_close(&stream);
                }
                ServerMode::LateEvent => {
                    write_sse_headers(&mut stream);
                    stream
                        .write_all(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"early\"}\n\n")
                        .expect("early event should be written");
                    wait_for_client_close(&stream);
                    let _ = stream.write_all(b"data: {\"type\":\"response.completed\"}\n\n");
                }
                ServerMode::MalformedFrame => {
                    write_sse_headers(&mut stream);
                    stream
                        .write_all(b"data: {not-json}\n\n")
                        .expect("malformed frame should be written");
                }
                ServerMode::OversizedFrame => {
                    write_sse_headers(&mut stream);
                    let frame = format!(
                        "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{}\"}}\n\n",
                        "x".repeat(128 * 1024)
                    );
                    stream
                        .write_all(frame.as_bytes())
                        .expect("oversized frame should be written");
                }
                ServerMode::UnterminatedOversizedFrame => {
                    write_sse_headers(&mut stream);
                    stream
                        .write_all(
                            format!(
                                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{}\"}}",
                                "x".repeat(128 * 1024)
                            )
                            .as_bytes(),
                        )
                        .expect("unterminated oversized frame should be written");
                }
                ServerMode::ErrorBody => {
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 500 Internal Server Error\r\nX-Remote-Secret: {SECRET_HEADER_SENTINEL}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{SECRET_BODY_SENTINEL}",
                                SECRET_BODY_SENTINEL.len()
                            )
                            .as_bytes(),
                        )
                        .expect("error response should be written");
                }
                ServerMode::CancelledError => {
                    thread::sleep(Duration::from_millis(25));
                    stream
                        .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .expect("error response should be written");
                }
            }
        });

        Self {
            address,
            observed_request: Some(observed_request),
            observed_body: None,
            worker,
        }
    }

    fn start_scripted(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("server should bind");
        let address = listener
            .local_addr()
            .expect("server address should be available");
        let (body_sender, observed_body) = mpsc::channel();
        let worker = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("server should accept a request");
                let body = read_request_body(&stream);
                body_sender
                    .send(body)
                    .expect("test should receive the request body");
                write_sse_headers(&mut stream);
                stream
                    .write_all(response.as_bytes())
                    .expect("scripted response should be written");
            }
        });

        Self {
            address,
            observed_request: None,
            observed_body: Some(observed_body),
            worker,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn take_observed_request(&mut self) -> mpsc::Receiver<()> {
        self.observed_request
            .take()
            .expect("request observation should only be taken once")
    }

    fn take_observed_body(&mut self) -> mpsc::Receiver<serde_json::Value> {
        self.observed_body
            .take()
            .expect("request body observation should only be taken once")
    }

    fn join(self) {
        self.worker.join().expect("server worker should finish");
    }
}

#[cfg(target_os = "linux")]
struct ResourceSnapshot {
    tasks: usize,
    file_descriptors: usize,
}

#[cfg(target_os = "linux")]
impl ResourceSnapshot {
    fn capture() -> Self {
        Self {
            tasks: std::fs::read_dir("/proc/self/task")
                .expect("task directory should be readable")
                .count(),
            file_descriptors: std::fs::read_dir("/proc/self/fd")
                .expect("file descriptor directory should be readable")
                .count(),
        }
    }
}

fn read_request(stream: &TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .expect("request line should be readable");
    assert_eq!(request_line, "POST /responses HTTP/1.1\r\n");

    loop {
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .expect("request header should be readable");
        if header == "\r\n" {
            return;
        }
    }
}

fn read_request_body(stream: &TcpStream) -> serde_json::Value {
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

    let mut body = vec![0; content_length.expect("request should include a content length")];
    reader
        .read_exact(&mut body)
        .expect("request body should be readable");
    serde_json::from_slice(&body).expect("request body should be JSON")
}

fn write_sse_headers(stream: &mut TcpStream) {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .expect("SSE headers should be written");
}

fn wait_for_client_close(stream: &TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("server read timeout should be configured");
    let mut byte = [0_u8; 1];
    let _ = stream
        .try_clone()
        .expect("stream should clone")
        .read(&mut byte);
}
