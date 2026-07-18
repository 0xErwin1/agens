use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use agens_core::{HeadlessTurnCancellation, HeadlessTurnPortError, TurnProvider};
use agens_providers::OpenAiResponsesProvider;

const SECRET_SENTINEL: &str = "SENTINEL_REMOTE_ERROR_BODY";

#[test]
fn cancellation_interrupts_connect_headers_stalled_body_and_late_events() {
    for mode in [
        ServerMode::DelayedAccept,
        ServerMode::DelayedHeaders,
        ServerMode::StalledBody,
        ServerMode::LateEvent,
    ] {
        let mut server = LocalResponsesServer::start(mode);
        let cancellation = HeadlessTurnCancellation::new();
        let canceller = cancellation.clone();
        let observed_request =
            (!matches!(mode, ServerMode::DelayedAccept)).then(|| server.take_observed_request());

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
fn timeout_is_distinct_from_cancellation_and_repeated_stops_do_not_accumulate_workers() {
    for _ in 0..8 {
        let server = LocalResponsesServer::start(ServerMode::DelayedHeaders);
        let cancellation = HeadlessTurnCancellation::with_deadline(Duration::from_millis(25));

        let result = run_provider(server.base_url(), cancellation, Duration::from_secs(1));

        assert_eq!(result, Err(HeadlessTurnPortError::TimedOut));
        server.join();
    }
}

#[test]
fn malformed_or_oversized_frames_and_remote_errors_are_sanitized_provider_failures() {
    for mode in [
        ServerMode::MalformedFrame,
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
    DelayedAccept,
    DelayedHeaders,
    StalledBody,
    LateEvent,
    MalformedFrame,
    OversizedFrame,
    ErrorBody,
}

struct LocalResponsesServer {
    address: std::net::SocketAddr,
    observed_request: Option<mpsc::Receiver<()>>,
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
            if matches!(mode, ServerMode::DelayedAccept) {
                thread::sleep(Duration::from_millis(100));
                let _ = listener.accept();
                return;
            }

            let (mut stream, _) = listener.accept().expect("server should accept one request");
            read_request(&stream);
            observed_sender
                .send(())
                .expect("test should receive request observation");

            match mode {
                ServerMode::DelayedAccept => unreachable!("delayed accept returns before handling"),
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
                ServerMode::ErrorBody => {
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{SECRET_SENTINEL}",
                                SECRET_SENTINEL.len()
                            )
                            .as_bytes(),
                        )
                        .expect("error response should be written");
                }
            }
        });

        Self {
            address,
            observed_request: Some(observed_request),
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

    fn join(self) {
        self.worker.join().expect("server worker should finish");
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
