//! The HTTP fixtures the provider suites share.
//!
//! These stay inside `agens-providers` rather than moving to the shared
//! `agens-fixtures` journey harness: the workspace contract pins this crate to
//! `agens-config` and `agens-core`, and that narrowness is deliberate. What
//! these fixtures test is also a different thing — bytes and timing on the
//! wire rather than a sequence of model turns — so they are expressed in
//! responses, not in turns.
//!
//! What they no longer do is each re-implement accepting, reading, and
//! shutting down a local server. Every suite here reaches the same primitives,
//! and the scripted server below answers a fixed sequence of responses for the
//! cases that need nothing more than that.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;

pub(crate) const SSE_HEADERS: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";

pub(crate) fn bind_pollable_listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("server should bind");
    listener
        .set_nonblocking(true)
        .expect("listener should be nonblocking");
    let address = listener
        .local_addr()
        .expect("server address should be available");

    (listener, address)
}

/// Waits for one connection while honoring the stop flag, so `join` terminates even
/// when the client was cancelled or timed out before it opened a socket at all.
pub(crate) fn accept_until_stopped(listener: &TcpListener, stop: &AtomicBool) -> Option<TcpStream> {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => return Some(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }

    None
}

/// Keeps the fixture's port bound until the test releases it, closing anything that
/// arrives once the scripted exchanges are done.
///
/// A worker that returned as soon as it had served its script dropped the listener while
/// the provider under test could still be retrying against that address, so the kernel
/// handed the port to whatever bound next — including another fixture in another test,
/// whose canned status then answered this test's retry. Closing late arrivals rather
/// than leaving them outstanding keeps those retries as fast as the refused connections
/// they used to be.
pub(crate) fn hold_address_until_stopped(listener: &TcpListener, stop: &AtomicBool) {
    while let Some(stream) = accept_until_stopped(listener, stop) {
        drop(stream);
    }
}

/// Leaves one connection outstanding until the test releases it, for a fixture whose
/// scenario is a request that never gets an answer.
pub(crate) fn hold_connection_until_stopped(stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(1));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestArrival {
    Complete,
    ClientCut,
}

/// A line the client never terminated — an empty read, a partial line, or a reset —
/// means the connection was cut; anything that arrived whole is the client's own
/// output and stays subject to the assertions on it.
pub(crate) fn read_line_or_cut(
    reader: &mut BufReader<TcpStream>,
    line: &mut String,
) -> RequestArrival {
    match reader.read_line(line) {
        Ok(_) if line.ends_with('\n') => RequestArrival::Complete,
        Ok(_) => RequestArrival::ClientCut,
        Err(error) if is_client_cut(&error) => RequestArrival::ClientCut,
        Err(error) => panic!("request should be readable: {error}"),
    }
}

pub(crate) fn is_client_cut(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
    )
}

/// One request a fixture received, kept whole so a suite can assert on the path,
/// the headers and the body the provider actually sent.
#[derive(Clone)]
pub(crate) struct ObservedRequest {
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Value,
    pub(crate) raw_body: String,
}

impl ObservedRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find_map(|(candidate, value)| (candidate == name).then_some(value.as_str()))
    }
}

/// Reads a request, reporting `None` when the client cut the connection before it was
/// complete.
///
/// `accept` returns as soon as the kernel completes the handshake, so a client that is
/// cancelled or times out before writing leaves a connection with nothing on it. A
/// request that does arrive stays fully asserted; each caller decides whether a missing
/// one is an expected outcome of its scenario or a failure it must report.
pub(crate) fn read_request(stream: &TcpStream) -> Option<ObservedRequest> {
    let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
    let mut request_line = String::new();
    if read_line_or_cut(&mut reader, &mut request_line) == RequestArrival::ClientCut {
        return None;
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .expect("request line should contain a path")
        .to_owned();

    let mut headers = Vec::new();
    let mut content_length = None;
    loop {
        let mut header = String::new();
        if read_line_or_cut(&mut reader, &mut header) == RequestArrival::ClientCut {
            return None;
        }
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
    match reader.read_exact(&mut body) {
        Ok(()) => {}
        Err(error) if is_client_cut(&error) => return None,
        Err(error) => panic!("body should be readable: {error}"),
    }

    Some(ObservedRequest {
        path,
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        raw_body: String::from_utf8(body).expect("body should be UTF-8"),
    })
}

/// Reads only the request head, for a fixture whose scenario is about what the
/// server does next rather than about what the client sent.
pub(crate) fn read_request_head(stream: &TcpStream, expected_path: &str) -> RequestArrival {
    let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
    let mut request_line = String::new();
    if read_line_or_cut(&mut reader, &mut request_line) == RequestArrival::ClientCut {
        return RequestArrival::ClientCut;
    }
    assert_eq!(
        request_line.split_whitespace().nth(1),
        Some(expected_path),
        "unexpected request target: {request_line}"
    );

    loop {
        let mut header = String::new();
        if read_line_or_cut(&mut reader, &mut header) == RequestArrival::ClientCut {
            return RequestArrival::ClientCut;
        }
        if header == "\r\n" {
            return RequestArrival::Complete;
        }
    }
}

pub(crate) fn write_sse_headers(stream: &mut TcpStream) {
    stream
        .write_all(SSE_HEADERS)
        .expect("SSE headers should be written");
}

pub(crate) fn write_sse(stream: &mut TcpStream, events: &str) {
    write_sse_headers(stream);
    stream
        .write_all(events.as_bytes())
        .expect("SSE body should be written");
}

pub(crate) fn write_json(stream: &mut TcpStream, status: u16, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .expect("JSON response should be written");
}

pub(crate) fn write_raw(stream: &mut TcpStream, status: u16, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .expect("raw response should be written");
}

/// Answers with a status whose header and body carry the caller's sentinels, for
/// the suites that assert a remote failure never reaches a user-visible surface.
pub(crate) fn write_status_with_secrets(
    stream: &mut TcpStream,
    status: u16,
    header_secret: &str,
    body_secret: &str,
) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} Test\r\nX-Secret: {header_secret}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_secret}",
                body_secret.len()
            )
            .as_bytes(),
        )
        .expect("status response should be written");
}

/// Writes a response the client may already have abandoned.
///
/// These fixtures answer requests that the cancellation, deadline and frame-cap tests are
/// deliberately racing against, so the client can be gone before the response leaves the
/// server; that is the outcome under test, not a fixture failure, and it is the write-side
/// counterpart of `is_client_cut` on the read side. Every caller reports its request
/// unconditionally, so bytes that never land change neither what a test observes nor what
/// `join` counts.
pub(crate) fn write_to_possibly_gone_client(stream: &mut TcpStream, bytes: &[u8]) {
    match stream.write_all(bytes) {
        Ok(()) => {}
        Err(error) if is_client_cut(&error) => {}
        Err(error) => panic!("response should be writable: {error}"),
    }
}

pub(crate) fn wait_for_client_close(stream: &TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("read timeout should be configured");
    let mut byte = [0_u8; 1];
    let _ = stream
        .try_clone()
        .expect("stream should clone")
        .read(&mut byte);
}

/// One answer in a scripted server's sequence.
pub(crate) enum ScriptedResponse {
    /// Accept the request and close without answering.
    Disconnect,
    /// A status whose header and body carry the caller's sentinels.
    StatusWithSecrets {
        status: u16,
        header_secret: String,
        body_secret: String,
    },
    /// A bare status, optionally with a `Retry-After`.
    Status {
        status: u16,
        retry_after: Option<String>,
    },
    Json(u16, String),
    Raw(u16, String),
    Sse(String),
    /// Answer nothing and hold the connection until the client gives up.
    WaitForClientClose,
}

impl ScriptedResponse {
    pub(crate) fn status(status: u16) -> Self {
        Self::Status {
            status,
            retry_after: None,
        }
    }

    pub(crate) fn status_with_retry_after(status: u16, retry_after: &str) -> Self {
        Self::Status {
            status,
            retry_after: Some(retry_after.to_owned()),
        }
    }
}

/// A server that answers a fixed sequence of responses and hands back every
/// request it read.
///
/// One round is one connection: each response in the script closes its stream,
/// which is what every provider suite here already assumed. The port stays
/// bound until `join`, so a retry that arrives after the script is refused by
/// this fixture rather than answered by whatever bound the port next.
pub(crate) struct ScriptedServer {
    address: SocketAddr,
    observed_requests: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<ObservedRequest>>>,
    stop: Arc<AtomicBool>,
    worker: thread::JoinHandle<Vec<ObservedRequest>>,
}

impl ScriptedServer {
    pub(crate) fn start(responses: Vec<ScriptedResponse>) -> Self {
        let (listener, address) = bind_pollable_listener();
        let observed_requests = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::with_capacity(responses.len())));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_observed = Arc::clone(&observed_requests);
        let worker_requests = Arc::clone(&requests);
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut requests = Vec::with_capacity(responses.len());
            for response in responses {
                let Some(mut stream) = accept_until_stopped(&listener, &worker_stop) else {
                    break;
                };
                // Every scripted round answers a request the provider is expected to
                // complete, so a connection without one is a failure of the round, not
                // an accepted outcome: the request counts these tests assert on would
                // otherwise quietly drop it.
                let request = read_request(&stream).expect(
                    "scripted server should receive a complete request for every scripted round",
                );
                requests.push(request.clone());
                worker_requests
                    .lock()
                    .expect("scripted server requests should not be poisoned")
                    .push(request);
                worker_observed.store(requests.len(), Ordering::Release);
                match response {
                    ScriptedResponse::Disconnect => {}
                    ScriptedResponse::StatusWithSecrets {
                        status,
                        header_secret,
                        body_secret,
                    } => {
                        write_status_with_secrets(&mut stream, status, &header_secret, &body_secret)
                    }
                    ScriptedResponse::Status {
                        status,
                        retry_after,
                    } => {
                        let retry_after = retry_after
                            .map(|value| format!("Retry-After: {value}\r\n"))
                            .unwrap_or_default();
                        write_to_possibly_gone_client(
                            &mut stream,
                            format!(
                                "HTTP/1.1 {status} Test\r\n{retry_after}Content-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                            .as_bytes(),
                        );
                    }
                    ScriptedResponse::Json(status, body) => {
                        write_json(&mut stream, status, &body);
                    }
                    ScriptedResponse::Raw(status, body) => write_raw(&mut stream, status, &body),
                    ScriptedResponse::Sse(events) => {
                        write_to_possibly_gone_client(&mut stream, SSE_HEADERS);
                        write_to_possibly_gone_client(&mut stream, events.as_bytes());
                    }
                    ScriptedResponse::WaitForClientClose => wait_for_client_close(&stream),
                }
            }
            hold_address_until_stopped(&listener, &worker_stop);

            requests
        });

        Self {
            address,
            observed_requests,
            requests,
            stop,
            worker,
        }
    }

    pub(crate) fn address(&self) -> SocketAddr {
        self.address
    }

    pub(crate) fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The subscription transport's responses endpoint on this server.
    pub(crate) fn responses_base_url(&self) -> String {
        format!("http://{}/backend-api/codex", self.address)
    }

    /// The subscription transport's token endpoint on this server.
    pub(crate) fn oauth_url(&self) -> String {
        format!("http://{}/oauth/token", self.address)
    }

    /// Counts the scripted rounds whose request the server has already read, so a
    /// test can stop the provider once the round it means to stop is actually in
    /// flight.
    pub(crate) fn observed_requests(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.observed_requests)
    }

    /// The request the server read for scripted round `index`, waiting for it
    /// rather than sleeping so a loaded machine does not decide the outcome.
    pub(crate) fn wait_for_request(&self, index: usize) -> ObservedRequest {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);

        loop {
            if let Some(request) = self.request(index) {
                return request;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "scripted server never received round {index}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// The request for scripted round `index`, if it has arrived.
    pub(crate) fn request(&self, index: usize) -> Option<ObservedRequest> {
        self.requests
            .lock()
            .expect("scripted server requests should not be poisoned")
            .get(index)
            .cloned()
    }

    pub(crate) fn join(self) -> Vec<ObservedRequest> {
        self.stop.store(true, Ordering::Release);
        self.worker.join().expect("scripted server should finish")
    }
}
