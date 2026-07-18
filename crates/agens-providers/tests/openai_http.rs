use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use agens_core::{HeadlessTurnCancellation, HeadlessTurnPortError, TurnProvider};
use agens_providers::OpenAiResponsesProvider;

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
