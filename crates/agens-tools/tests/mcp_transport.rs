use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use agens_core::HeadlessTurnCancellation;
use agens_tools::{
    MAX_MCP_STATUS_TOOL_NAMES, McpCallResult, McpClient, McpContentBlock, McpEndpointSummary,
    McpErrorCategory, McpHttpTransport, McpInitialize, McpInitializeResult, McpLifecycleState,
    McpLimits, McpOperationContext, McpProtocolError, McpRegistry, McpRequest, McpResponse,
    McpServerDescriptor, McpServerReport, McpServerSource, McpServerTransport, McpSseTransport,
    McpStatusHandle, McpTimeouts, McpToolAnnotations, McpToolDefinition, McpToolsPage,
    McpTransport, McpTransportError, RemoteToolAccess, ToolOutput,
};
use serde_json::json;

#[derive(Clone)]
struct LocalTransport {
    responses: Arc<Mutex<VecDeque<Result<McpResponse, McpTransportError>>>>,
    requests: Arc<Mutex<Vec<McpRequest>>>,
    closed: Arc<AtomicBool>,
    cancelled: Arc<AtomicUsize>,
    delay: Duration,
}

impl LocalTransport {
    fn with_responses(
        responses: impl IntoIterator<Item = Result<McpResponse, McpTransportError>>,
    ) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
            closed: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicUsize::new(0)),
            delay: Duration::ZERO,
        }
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

impl McpTransport for LocalTransport {
    fn execute(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<McpResponse, McpTransportError> {
        self.requests.lock().unwrap().push(request);
        while !context.is_expired() && !context.is_cancelled() && self.delay > Duration::ZERO {
            let slice = self.delay.min(Duration::from_millis(1));
            thread::sleep(slice);
            self.delay = self.delay.saturating_sub(slice);
        }
        context.check()?;
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                Err(McpTransportError::Protocol(
                    "missing deterministic response".into(),
                ))
            })
    }

    fn notify(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<(), McpTransportError> {
        self.requests.lock().unwrap().push(request);
        context.check()
    }

    fn close(&mut self, context: &McpOperationContext) -> Result<(), McpTransportError> {
        self.cancelled.fetch_add(1, Ordering::AcqRel);
        self.closed.store(true, Ordering::Release);
        context.check()
    }
}

struct StepDelayTransport {
    responses: VecDeque<Result<McpResponse, McpTransportError>>,
    delays: VecDeque<Duration>,
    phases: mpsc::SyncSender<McpRequest>,
    permits: mpsc::Receiver<()>,
}

impl StepDelayTransport {
    fn new(
        responses: impl IntoIterator<Item = Result<McpResponse, McpTransportError>>,
        delays: impl IntoIterator<Item = Duration>,
        phases: mpsc::SyncSender<McpRequest>,
        permits: mpsc::Receiver<()>,
    ) -> Self {
        Self {
            responses: responses.into_iter().collect(),
            delays: delays.into_iter().collect(),
            phases,
            permits,
        }
    }

    fn wait_for_step(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<(), McpTransportError> {
        self.phases
            .send(request)
            .expect("test must observe every transport step");
        self.permits
            .recv_timeout(Duration::from_secs(2))
            .expect("test must release every observed transport step");

        context.check()?;
        thread::sleep(self.delays.pop_front().unwrap_or(Duration::ZERO));
        context.check()
    }
}

impl McpTransport for StepDelayTransport {
    fn execute(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<McpResponse, McpTransportError> {
        self.wait_for_step(request, context)?;
        self.responses.pop_front().unwrap_or_else(|| {
            Err(McpTransportError::Protocol(
                "missing deterministic response".into(),
            ))
        })
    }

    fn notify(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<(), McpTransportError> {
        self.wait_for_step(request, context)
    }

    fn close(&mut self, _: &McpOperationContext) -> Result<(), McpTransportError> {
        Ok(())
    }
}

fn initialize() -> McpInitialize {
    McpInitialize::new(
        agens_tools::MCP_PROTOCOL_VERSION,
        json!({}),
        "agens",
        "0.1.0",
    )
}

fn initialized() -> McpResponse {
    McpResponse::Initialized(McpInitializeResult::new(
        agens_tools::MCP_PROTOCOL_VERSION,
        json!({"tools": {}}),
    ))
}

fn timeouts() -> McpTimeouts {
    McpTimeouts::new(
        Duration::from_millis(20),
        Duration::from_millis(20),
        Duration::from_millis(20),
    )
    .unwrap()
}

fn limits() -> McpLimits {
    McpLimits::new(8, 16).unwrap()
}

fn tool(name: &str, read_only: Option<bool>) -> McpToolDefinition {
    McpToolDefinition {
        name: name.into(),
        description: Some(format!("{name} description")),
        input_schema: json!({"type": "object"}),
        annotations: McpToolAnnotations {
            read_only_hint: read_only,
        },
    }
}

fn page(tools: Vec<McpToolDefinition>, next_cursor: Option<&str>) -> McpResponse {
    McpResponse::ToolsListed(McpToolsPage::new(tools, next_cursor.map(str::to_owned)))
}

/// Deadline for transport calls whose assertions are about framing, limits or retry
/// accounting rather than about time.
///
/// None of those tests observe the deadline, so it only has to be large enough never to
/// fire and small enough that a genuine hang still fails loudly. A one-second budget is
/// not large enough: moving a megabyte over loopback and parsing it routinely exceeds it
/// on a contended core, and every such assertion then reports `TimedOut` instead of the
/// framing error it was written for. Do not tighten this back toward the observed
/// latency.
const UNOBSERVED_DEADLINE: Duration = Duration::from_secs(30);

/// Deadline shared by both attempts of an SSE request whose retry
/// [`stall_the_retry_of_a_failed_attempt`] never answers.
///
/// It has to comfortably exceed one loopback round trip, because the first attempt must
/// complete inside it for the retry under test to happen at all; the fifty milliseconds
/// this replaces are routinely spent before the first response arrives on a contended
/// core, and the retry then never leaves the client.
const SSE_STALLED_RETRY_DEADLINE: Duration = Duration::from_secs(1);

/// Ceiling on a retried call bounded by [`SSE_STALLED_RETRY_DEADLINE`].
///
/// The server holding the retry open never answers it, so what this bound proves is that
/// the call ended on its own deadline rather than on anything the server did. Keep it
/// close to the deadline — the margin is only there to absorb the cost of noticing the
/// deadline and unwinding on a loaded machine.
const SSE_STALLED_RETRY_CEILING: Duration = Duration::from_millis(1_800);

/// Retries allowed to a request whose attempts are each charged [`HELD_ATTEMPT`].
///
/// Two is the smallest number that lets the two deadline models end differently: the shared
/// budget runs out during the second attempt and leaves the third unmade, while a budget
/// renewed per attempt reaches the third and exhausts the retries instead.
const BUDGETED_RETRIES: u32 = 2;

/// Deadline shared by every attempt of an HTTP request whose attempts
/// [`fail_every_attempt_after_spending_the_budget`] deliberately spends most of.
///
/// One shared deadline is only distinguishable from a deadline renewed per attempt once the
/// earlier attempts have consumed enough of the budget that a later one cannot be paid for,
/// so this is sized against [`HELD_ATTEMPT`] rather than against loopback latency.
const HTTP_SPENT_BUDGET_DEADLINE: Duration = Duration::from_secs(2);

/// What the fixture charges every attempt before failing it.
///
/// It has to fit inside [`HTTP_SPENT_BUDGET_DEADLINE`] once but not twice. Below half the budget the
/// two models agree, because a shared budget still has room to pay for the retry; above the
/// whole budget the first attempt times out and no retry is made at all. Both edges are six
/// hundred milliseconds away, so neither is reachable by scheduling noise.
const HELD_ATTEMPT: Duration = Duration::from_millis(1_400);

/// Ceiling on a retried call bounded by [`HTTP_SPENT_BUDGET_DEADLINE`].
///
/// What it proves is that the call ended on its own deadline rather than on the fixture,
/// which cannot answer a second attempt before two [`HELD_ATTEMPT`]s have passed; keep it
/// below that sum. The margin over the budget only absorbs the cost of noticing the deadline
/// and unwinding on a loaded machine.
const HTTP_SPENT_BUDGET_CEILING: Duration = Duration::from_millis(2_600);

/// Authority of an endpoint that can never complete a connection.
///
/// The obvious alternative — bind an ephemeral port, drop the listener, then connect —
/// only holds while nothing else claims the released port, and under a high process-fork
/// rate the kernel hands it to another socket between the drop and the connect, so the
/// refusal these tests depend on silently becomes a successful connection. Port zero is
/// never assigned to a listening socket, so connecting to it always fails to establish
/// without depending on the state of the rest of the machine.
const UNREACHABLE_AUTHORITY: &str = "127.0.0.1:0";

/// Binds a listener whose accept loop can be polled, so a fixture never parks in a
/// blocking `accept` for a client that was cancelled or timed out before connecting.
fn bind_pollable_listener() -> (TcpListener, std::net::SocketAddr) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();

    (listener, address)
}

/// Waits for one connection while honoring the stop flag, so joining the fixture
/// terminates even when no client ever arrives.
fn accept_until_stopped(listener: &TcpListener, stop: &AtomicBool) -> Option<TcpStream> {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return Some(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }

    None
}

fn accept_http_request(listener: &TcpListener) -> (TcpStream, String) {
    let (stream, _) = listener.accept().unwrap();
    let headers = read_http_request(&stream);

    (stream, headers)
}

/// Reads a request head from an already accepted connection.
///
/// A client that goes away mid-request ends the read instead of leaving the fixture
/// spinning on an endless stream of empty reads; the truncated head it returns then
/// fails whatever assertion was written against it.
fn read_http_request(stream: &TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut headers = String::new();

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            return headers;
        }
        headers.push_str(&line);
        if line == "\r\n" {
            return headers;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HttpAttempt {
    Failed,
    Stalled,
}

/// Answers the first request with a retryable failure and then holds the retry open
/// until the caller stops it, reporting each attempt it observed.
///
/// Stalling by holding rather than by sleeping is what lets the caller's deadline be the
/// only thing that ends the retried call: a fixture that stops on its own clock races
/// the client, and one that parks in a blocking `accept` never returns at all when the
/// client gives up before retrying.
fn stall_the_retry_of_a_failed_attempt(
    listener: TcpListener,
    attempts: mpsc::SyncSender<HttpAttempt>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let Some(mut failed) = accept_until_stopped(&listener, &stop) else {
            return;
        };
        read_http_request(&failed);
        respond(&mut failed, "500 Internal Server Error", b"", "");
        attempts.send(HttpAttempt::Failed).unwrap();

        let Some(stalled) = accept_until_stopped(&listener, &stop) else {
            return;
        };
        read_http_request(&stalled);
        attempts.send(HttpAttempt::Stalled).unwrap();

        while !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
    })
}

/// Spends [`HELD_ATTEMPT`] of the caller's budget on every attempt before failing it
/// retryably, reporting each attempt it answered.
///
/// Charging every attempt for the time it takes is what makes the two deadline models
/// diverge into different observations: a shared budget runs out partway through an attempt
/// the client then abandons, while a budget renewed per attempt pays for all of them and the
/// client keeps retrying until its retries run out. An abandoned attempt leaves no trace on
/// its own connection — the client neither closes nor resets it — so what tells the two
/// models apart is the count of attempts that reached this fixture at all.
fn fail_every_attempt_after_spending_the_budget(
    listener: TcpListener,
    attempts: mpsc::SyncSender<HttpAttempt>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while let Some(mut attempt) = accept_until_stopped(&listener, &stop) {
            read_http_request(&attempt);
            withhold_response_for(HELD_ATTEMPT, &stop);

            respond(&mut attempt, "500 Internal Server Error", b"", "");
            attempts.send(HttpAttempt::Failed).unwrap();
        }
    })
}

/// Withholds a response for `duration`, cutting the wait short once the fixture is stopped so
/// that joining it never has to wait out an attempt the client already gave up on.
fn withhold_response_for(duration: Duration, stop: &AtomicBool) {
    let end = Instant::now() + duration;

    while Instant::now() < end && !stop.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(1));
    }
}

fn respond(stream: &mut TcpStream, status: &str, body: &[u8], extra_headers: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
}

fn initialized_body() -> Vec<u8> {
    br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}}"#.to_vec()
}

#[test]
fn registers_negotiated_paginated_tools_with_conservative_access() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let transport = LocalTransport::with_responses([
        Ok(initialized()),
        Ok(page(vec![tool("read", Some(true))], Some("next"))),
        Ok(page(vec![tool("write", None)], None)),
    ]);
    let requests = Arc::clone(&transport.requests);
    let mut registry = McpRegistry::new();

    let report = registry.load_server(
        "files",
        transport,
        &initialize(),
        timeouts(),
        limits(),
        cancellation,
    );

    assert_eq!(report, McpServerReport::loaded("files", 2));
    assert_eq!(
        registry.tool("files::read").unwrap().access,
        RemoteToolAccess::ReadOnly
    );
    assert_eq!(
        registry.tool("files::write").unwrap().access,
        RemoteToolAccess::Write
    );
    assert_eq!(
        requests.lock().unwrap().as_slice(),
        [
            McpRequest::Initialize(initialize()),
            McpRequest::Initialized,
            McpRequest::ListTools { cursor: None },
            McpRequest::ListTools {
                cursor: Some("next".into())
            }
        ]
    );
}

#[test]
fn registry_retains_callable_clients_after_metadata_enumeration() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let transport = LocalTransport::with_responses([
        Ok(initialized()),
        Ok(page(vec![tool("status", Some(true))], None)),
        Ok(McpResponse::ToolCalled(McpCallResult {
            content: vec![McpContentBlock::Text("ready".into())],
            is_error: false,
        })),
    ]);
    let mut registry = McpRegistry::new();
    assert_eq!(
        registry.load_server(
            "server",
            transport,
            &initialize(),
            timeouts(),
            limits(),
            cancellation,
        ),
        McpServerReport::loaded("server", 1)
    );
    assert_eq!(
        registry
            .call_tool(
                "server::status",
                json!({}),
                &agens_tools::ToolExecutionContext::with_timeout(Duration::from_secs(1)),
            )
            .unwrap(),
        ToolOutput::success("ready")
    );
}

#[test]
fn rejects_invalid_schema_negotiation_and_pagination_without_registry_mutation() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let cases = [
        (
            "schema",
            vec![
                Ok(initialized()),
                Ok(page(
                    vec![McpToolDefinition {
                        input_schema: json!({}),
                        ..tool("bad", None)
                    }],
                    None,
                )),
            ],
        ),
        (
            "version",
            vec![Ok(McpResponse::Initialized(McpInitializeResult::new(
                "2024-11-05",
                json!({"tools": {}}),
            )))],
        ),
        (
            "capability",
            vec![Ok(McpResponse::Initialized(McpInitializeResult::new(
                agens_tools::MCP_PROTOCOL_VERSION,
                json!({}),
            )))],
        ),
        (
            "cursor",
            vec![
                Ok(initialized()),
                Ok(page(vec![tool("one", None)], Some("loop"))),
                Ok(page(vec![tool("two", None)], Some("loop"))),
            ],
        ),
    ];

    for (name, responses) in cases {
        let transport = LocalTransport::with_responses(responses);
        let closed = Arc::clone(&transport.closed);
        let mut registry = McpRegistry::new();
        let report = registry.load_server(
            name,
            transport,
            &initialize(),
            timeouts(),
            limits(),
            Arc::clone(&cancellation),
        );
        assert!(report.is_failed(), "{name}");
        assert!(registry.is_empty(), "{name}");
        assert!(closed.load(Ordering::Acquire), "{name}");
    }
}

#[test]
fn rejects_tools_list_page_and_resource_limit_exhaustion() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut page_limited = McpClient::new(
        LocalTransport::with_responses([Ok(page(vec![tool("one", None)], Some("next")))]),
        timeouts(),
        McpLimits::new(1, 2).unwrap(),
    );
    assert_eq!(
        page_limited.list_tools(&cancellation),
        Err(McpTransportError::Protocol(
            "MCP tools/list page limit exceeded".into()
        ))
    );

    let mut resource_limited = McpClient::new(
        LocalTransport::with_responses([Ok(page(
            vec![tool("one", None), tool("two", None)],
            None,
        ))]),
        timeouts(),
        McpLimits::new(2, 1).unwrap(),
    );
    assert_eq!(
        resource_limited.list_tools(&cancellation),
        Err(McpTransportError::Protocol(
            "MCP tools/list tool limit exceeded".into()
        ))
    );
}

#[test]
fn maps_call_errors_and_rejects_non_object_arguments_without_sending() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let transport = LocalTransport::with_responses([
        Ok(McpResponse::ProtocolError(McpProtocolError::new(
            -32001, "denied",
        ))),
        Ok(McpResponse::ToolCalled(McpCallResult {
            content: vec![McpContentBlock::Text("invalid input".into())],
            is_error: true,
        })),
    ]);
    let requests = Arc::clone(&transport.requests);
    let mut client = McpClient::new(transport, timeouts(), limits());

    assert_eq!(
        client.call_tool("write", json!("not-an-object"), &cancellation),
        Ok(ToolOutput::failure(
            "mcp: tool arguments must be a JSON object"
        ))
    );
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(
        client.call_tool("write", json!({}), &cancellation),
        Ok(ToolOutput::failure("mcp protocol failure"))
    );
    assert_eq!(
        client.call_tool("write", json!({}), &cancellation),
        Ok(ToolOutput::failure("invalid input"))
    );
}

#[test]
fn registry_enumerates_metadata_and_atomically_replaces_a_reloaded_server() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut registry = McpRegistry::new();

    assert_eq!(
        registry.load_server(
            "server",
            LocalTransport::with_responses([
                Ok(initialized()),
                Ok(page(vec![tool("old", Some(true))], None))
            ]),
            &initialize(),
            timeouts(),
            limits(),
            Arc::clone(&cancellation),
        ),
        McpServerReport::loaded("server", 1)
    );
    assert_eq!(registry.tools().len(), 1);

    let failed_reload = registry.load_server(
        "server",
        LocalTransport::with_responses([
            Ok(initialized()),
            Ok(page(vec![tool("bad", Some(true))], Some("loop"))),
            Ok(page(vec![], Some("loop"))),
        ]),
        &initialize(),
        timeouts(),
        limits(),
        Arc::clone(&cancellation),
    );
    assert!(failed_reload.is_failed());
    assert!(registry.tool("server::old").is_some());

    assert_eq!(
        registry.load_server(
            "server",
            LocalTransport::with_responses([
                Ok(initialized()),
                Ok(page(vec![tool("new", Some(true))], None))
            ]),
            &initialize(),
            timeouts(),
            limits(),
            cancellation,
        ),
        McpServerReport::loaded("server", 1)
    );
    assert!(registry.tool("server::old").is_none());
    assert_eq!(
        registry
            .tools()
            .iter()
            .map(|tool| tool.qualified_name.as_str())
            .collect::<Vec<_>>(),
        ["server::new"]
    );
}

/// `/mcp` labels this value "Tool call timeout", and that is the only number an operator has
/// to size `timeout_ms` against. The connect and list budgets sit on a ten-second floor that
/// is deliberately unrelated to what a call is allowed to take, so publishing either of them
/// under that label would tell a user who configured two hundred milliseconds that they had
/// configured ten seconds.
#[test]
fn a_configured_servers_descriptor_publishes_the_call_timeout_not_the_connect_floor() {
    let status = McpStatusHandle::default();
    let mut registry = McpRegistry::with_status_handle(status.clone());

    registry
        .configure_server(
            "clocks",
            || {
                Err(McpTransportError::Transport(
                    "the descriptor is registered before any connection is attempted".into(),
                ))
            },
            McpTimeouts::new(
                Duration::from_secs(10),
                Duration::from_secs(10),
                Duration::from_millis(200),
            )
            .unwrap(),
            limits(),
        )
        .unwrap();

    let snapshot = status.snapshot();
    let server = snapshot.server("clocks").expect("the server is registered");

    assert_eq!(server.descriptor().timeout(), Duration::from_millis(200));
}

#[test]
fn configured_servers_load_lazily_retry_only_on_reload_and_keep_working_tools() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let first_close_count = Arc::new(AtomicUsize::new(0));
    let first_close_count_factory = Arc::clone(&first_close_count);
    let replacement_close_count = Arc::new(AtomicUsize::new(0));
    let replacement_close_count_factory = Arc::clone(&replacement_close_count);
    let attempts_factory = Arc::clone(&attempts);
    let mut registry = McpRegistry::new();

    registry
        .configure_server(
            "files",
            move || match attempts_factory.fetch_add(1, Ordering::AcqRel) {
                0 => {
                    let transport = LocalTransport {
                        responses: Arc::new(Mutex::new(
                            [
                                Ok(initialized()),
                                Ok(page(vec![tool("old", Some(true))], None)),
                                Ok(McpResponse::ToolCalled(McpCallResult {
                                    content: vec![McpContentBlock::Text("old callable".into())],
                                    is_error: false,
                                })),
                            ]
                            .into(),
                        )),
                        requests: Arc::new(Mutex::new(Vec::new())),
                        closed: Arc::new(AtomicBool::new(false)),
                        cancelled: Arc::clone(&first_close_count_factory),
                        delay: Duration::ZERO,
                    };
                    Ok(Box::new(transport) as Box<dyn McpTransport>)
                }
                1 => Err(McpTransportError::Transport(
                    "SENTINEL_SECRET reload failed".into(),
                )),
                _ => Ok(Box::new(LocalTransport {
                    responses: Arc::new(Mutex::new(
                        [
                            Ok(initialized()),
                            Ok(page(vec![tool("new", Some(true))], None)),
                        ]
                        .into(),
                    )),
                    requests: Arc::new(Mutex::new(Vec::new())),
                    closed: Arc::new(AtomicBool::new(false)),
                    cancelled: Arc::clone(&replacement_close_count_factory),
                    delay: Duration::ZERO,
                }) as Box<dyn McpTransport>),
            },
            timeouts(),
            limits(),
        )
        .unwrap();

    assert_eq!(attempts.load(Ordering::Acquire), 0);
    assert!(registry.tools().is_empty());
    assert_eq!(
        registry.discover_server("files"),
        McpServerReport::loaded("files", 1)
    );
    assert_eq!(attempts.load(Ordering::Acquire), 1);
    assert!(registry.tool("files::old").is_some());

    assert!(registry.reload_server("files").is_failed());
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    assert!(registry.tool("files::old").is_some());
    assert_eq!(registry.diagnostics().len(), 1);
    assert!(
        !registry.diagnostics()[0]
            .message
            .contains("SENTINEL_SECRET")
    );
    assert_eq!(
        registry
            .call_tool(
                "files::old",
                json!({}),
                &agens_tools::ToolExecutionContext::with_timeout(Duration::from_secs(1)),
            )
            .unwrap(),
        ToolOutput::success("old callable")
    );
    assert!(registry.discover_server("files").is_failed());
    assert_eq!(attempts.load(Ordering::Acquire), 2);

    assert_eq!(
        registry.reload_server("files"),
        McpServerReport::loaded("files", 1)
    );
    assert_eq!(attempts.load(Ordering::Acquire), 3);
    assert!(registry.tool("files::old").is_none());
    assert!(registry.tool("files::new").is_some());
    assert_eq!(first_close_count.load(Ordering::Acquire), 1);

    drop(registry);
    assert_eq!(replacement_close_count.load(Ordering::Acquire), 1);
}

fn status_descriptor(
    name: &str,
    transport: McpServerTransport,
    enabled: bool,
) -> McpServerDescriptor {
    McpServerDescriptor::new(
        name,
        McpServerSource::Global,
        transport,
        enabled,
        Duration::from_secs(2),
        (name == "files").then(|| McpEndpointSummary::stdio("/private/bin/files-server")),
    )
}

#[test]
fn registry_status_snapshot_tracks_authoritative_lifecycle_without_forcing_discovery() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_factory = Arc::clone(&attempts);
    let (entered, entered_rx) = mpsc::sync_channel(1);
    let (release, release_rx) = mpsc::sync_channel(1);
    let mut registry = McpRegistry::new();
    registry
        .register_disabled_server(status_descriptor(
            "disabled",
            McpServerTransport::Stdio,
            false,
        ))
        .unwrap();
    registry
        .configure_server_with_descriptor(
            status_descriptor("files", McpServerTransport::Stdio, true),
            move || {
                attempts_factory.fetch_add(1, Ordering::AcqRel);
                entered.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(Box::new(LocalTransport::with_responses([
                    Ok(initialized()),
                    Ok(page(
                        (0..MAX_MCP_STATUS_TOOL_NAMES + 5)
                            .map(|index| tool(&format!("tool-{index:02}"), Some(true)))
                            .collect(),
                        None,
                    )),
                ])) as Box<dyn McpTransport>)
            },
            timeouts(),
            McpLimits::new(2, MAX_MCP_STATUS_TOOL_NAMES + 5).unwrap(),
        )
        .unwrap();

    let status = registry.status_handle();
    assert_eq!(attempts.load(Ordering::Acquire), 0);
    assert_eq!(registry.configured_server_names(), ["files"]);
    assert_eq!(
        status.snapshot().server("disabled").unwrap().state(),
        McpLifecycleState::Disabled
    );
    assert_eq!(
        status.snapshot().server("files").unwrap().state(),
        McpLifecycleState::Idle
    );

    let worker = thread::spawn(move || {
        let report = registry.discover_server("files");
        (registry, report)
    });
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        status.snapshot().server("files").unwrap().state(),
        McpLifecycleState::Connecting
    );
    release.send(()).unwrap();
    let (mut registry, report) = worker.join().unwrap();
    assert_eq!(
        report,
        McpServerReport::loaded("files", MAX_MCP_STATUS_TOOL_NAMES + 5)
    );

    let ready = status.snapshot();
    let files = ready.server("files").unwrap();
    assert_eq!(
        (files.state(), files.tool_count()),
        (McpLifecycleState::Ready, 37)
    );
    assert_eq!(files.tool_names().len(), MAX_MCP_STATUS_TOOL_NAMES);
    assert_eq!(files.endpoint().unwrap().as_str(), "files-server");

    registry
        .configure_server_with_descriptor(
            files.descriptor().clone(),
            || Err(McpTransportError::Transport("SENTINEL_SECRET body".into())),
            timeouts(),
            limits(),
        )
        .unwrap();
    assert!(registry.reload_server("files").is_failed());
    let degraded = status.snapshot();
    let files = degraded.server("files").unwrap();
    assert_eq!(files.state(), McpLifecycleState::Degraded);
    assert_eq!(
        files.last_error().unwrap().category(),
        McpErrorCategory::Transport
    );
    assert!(!format!("{degraded:?}").contains("SENTINEL_SECRET"));

    registry
        .configure_server_with_descriptor(
            status_descriptor("broken", McpServerTransport::Http, true),
            || {
                Err(McpTransportError::Protocol(
                    "SENTINEL_SECRET response".into(),
                ))
            },
            timeouts(),
            limits(),
        )
        .unwrap();
    assert!(registry.discover_server("broken").is_failed());
    let failed = status.snapshot();
    let broken = failed.server("broken").unwrap();
    assert_eq!(broken.state(), McpLifecycleState::Failed);
    assert_eq!(
        broken.last_error().unwrap().category(),
        McpErrorCategory::Protocol
    );
    assert!(!format!("{failed:?}").contains("SENTINEL_SECRET"));

    registry.close();
    let closed = status.snapshot();
    assert_eq!(
        closed.server("files").unwrap().state(),
        McpLifecycleState::Closed
    );
    assert_eq!(
        closed.server("broken").unwrap().state(),
        McpLifecycleState::Closed
    );
    assert_eq!(
        closed.server("disabled").unwrap().state(),
        McpLifecycleState::Disabled
    );
}

#[test]
fn dropping_one_registry_leaves_the_servers_of_another_registry_on_the_shared_handle_open() {
    let status = McpStatusHandle::default();

    let mut long_lived = McpRegistry::with_status_handle(status.clone());
    long_lived
        .configure_server_with_descriptor(
            status_descriptor("long-lived", McpServerTransport::Http, true),
            || Ok(Box::new(LocalTransport::with_responses([])) as Box<dyn McpTransport>),
            timeouts(),
            limits(),
        )
        .unwrap();

    {
        let mut short_lived = McpRegistry::with_status_handle(status.clone());
        short_lived
            .configure_server_with_descriptor(
                status_descriptor("short-lived", McpServerTransport::Http, true),
                || Ok(Box::new(LocalTransport::with_responses([])) as Box<dyn McpTransport>),
                timeouts(),
                limits(),
            )
            .unwrap();
    }

    let snapshot = status.snapshot();
    assert_eq!(
        snapshot.server("short-lived").unwrap().state(),
        McpLifecycleState::Closed,
        "the dropped registry must retire its own server"
    );
    assert_eq!(
        snapshot.server("long-lived").unwrap().state(),
        McpLifecycleState::Idle,
        "a live registry must keep its servers off the closed state"
    );

    drop(long_lived);
    assert_eq!(
        status.snapshot().server("long-lived").unwrap().state(),
        McpLifecycleState::Closed
    );
}

#[test]
fn dropping_one_of_two_registries_sharing_a_server_name_preserves_ready_state_and_reregistering_does_not_reset_it()
 {
    let status = McpStatusHandle::default();
    let shared = || {
        Ok(Box::new(LocalTransport::with_responses([
            Ok(initialized()),
            Ok(page(vec![], None)),
        ])) as Box<dyn McpTransport>)
    };

    let mut long_lived = McpRegistry::with_status_handle(status.clone());
    long_lived
        .configure_server_with_descriptor(
            status_descriptor("shared", McpServerTransport::Http, true),
            shared,
            timeouts(),
            limits(),
        )
        .unwrap();
    assert!(!long_lived.discover_server("shared").is_failed());
    assert_eq!(
        status.snapshot().server("shared").unwrap().state(),
        McpLifecycleState::Ready
    );

    let mut ephemeral = McpRegistry::with_status_handle(status.clone());
    ephemeral
        .configure_server_with_descriptor(
            status_descriptor("shared", McpServerTransport::Http, true),
            shared,
            timeouts(),
            limits(),
        )
        .unwrap();
    assert_eq!(
        status.snapshot().server("shared").unwrap().state(),
        McpLifecycleState::Ready,
        "an ephemeral registry configuring the same name must not reset Ready to Idle"
    );

    drop(ephemeral);
    assert_eq!(
        status.snapshot().server("shared").unwrap().state(),
        McpLifecycleState::Ready,
        "dropping the ephemeral registry must not close a server still claimed by another registry"
    );

    let mut reregistering = McpRegistry::with_status_handle(status.clone());
    reregistering
        .configure_server_with_descriptor(
            status_descriptor("shared", McpServerTransport::Http, true),
            shared,
            timeouts(),
            limits(),
        )
        .unwrap();
    assert_eq!(
        status.snapshot().server("shared").unwrap().state(),
        McpLifecycleState::Ready,
        "re-registering must preserve state, tool count, and last error"
    );

    drop(reregistering);
    assert_eq!(
        status.snapshot().server("shared").unwrap().state(),
        McpLifecycleState::Ready,
        "long_lived still claims the server, so it must remain open"
    );

    drop(long_lived);
    assert_eq!(
        status.snapshot().server("shared").unwrap().state(),
        McpLifecycleState::Closed,
        "once every claiming registry has dropped, the server must close"
    );
}

#[test]
fn reregistering_a_closed_enabled_server_resets_it_to_idle_instead_of_preserving_closed() {
    let status = McpStatusHandle::default();

    let mut first_owner = McpRegistry::with_status_handle(status.clone());
    first_owner
        .configure_server_with_descriptor(
            status_descriptor("recycled", McpServerTransport::Http, true),
            || Ok(Box::new(LocalTransport::with_responses([])) as Box<dyn McpTransport>),
            timeouts(),
            limits(),
        )
        .unwrap();
    drop(first_owner);
    assert_eq!(
        status.snapshot().server("recycled").unwrap().state(),
        McpLifecycleState::Closed,
        "dropping the only claimant must close the server"
    );

    let mut new_owner = McpRegistry::with_status_handle(status.clone());
    new_owner
        .configure_server_with_descriptor(
            status_descriptor("recycled", McpServerTransport::Http, true),
            || {
                Ok(Box::new(LocalTransport::with_responses([
                    Ok(initialized()),
                    Ok(page(vec![], None)),
                ])) as Box<dyn McpTransport>)
            },
            timeouts(),
            limits(),
        )
        .unwrap();
    assert_eq!(
        status.snapshot().server("recycled").unwrap().state(),
        McpLifecycleState::Idle,
        "a fresh claim on a closed enabled server must reset it to Idle, not preserve Closed"
    );

    assert!(!new_owner.discover_server("recycled").is_failed());
    assert_eq!(
        status.snapshot().server("recycled").unwrap().state(),
        McpLifecycleState::Ready,
        "a live Ready state must still be preserved across an intervening claim"
    );
}

#[test]
fn timeout_and_cancellation_preserve_primary_result_despite_cleanup_error_and_suppress_late_success()
 {
    #[derive(Clone)]
    struct CleanupErrorTransport(LocalTransport);
    impl McpTransport for CleanupErrorTransport {
        fn execute(
            &mut self,
            request: McpRequest,
            context: &McpOperationContext,
        ) -> Result<McpResponse, McpTransportError> {
            self.0.execute(request, context)
        }
        fn notify(
            &mut self,
            request: McpRequest,
            context: &McpOperationContext,
        ) -> Result<(), McpTransportError> {
            self.0.notify(request, context)
        }
        fn close(&mut self, _: &McpOperationContext) -> Result<(), McpTransportError> {
            Err(McpTransportError::Transport("cleanup failed".into()))
        }
    }

    let cancellation = Arc::new(AtomicBool::new(false));
    let timeout_transport = CleanupErrorTransport(
        LocalTransport::with_responses([Ok(McpResponse::ToolCalled(McpCallResult {
            content: vec![McpContentBlock::Text("late success".into())],
            is_error: false,
        }))])
        .delayed(Duration::from_millis(5)),
    );
    let short = McpTimeouts::new(
        Duration::from_millis(1),
        Duration::from_millis(1),
        Duration::from_millis(1),
    )
    .unwrap();
    let mut timeout_client = McpClient::new(timeout_transport, short, limits());
    assert_eq!(
        timeout_client.call_tool("slow", json!({}), &cancellation),
        Err(McpTransportError::TimedOut)
    );

    cancellation.store(true, Ordering::Release);
    let timeout_transport = timeout_client.into_transport();
    let mut cancelled_client = McpClient::new(timeout_transport, timeouts(), limits());
    assert_eq!(
        cancelled_client.call_tool("slow", json!({}), &cancellation),
        Err(McpTransportError::Cancelled)
    );
}

#[test]
fn connect_and_list_tools_enforce_one_deadline_across_internal_steps() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let operation_timeout = Duration::from_secs(2);
    let first_step_delay = Duration::from_millis(400);
    let second_step_delay = Duration::from_millis(1800);
    let timeouts =
        McpTimeouts::new(operation_timeout, operation_timeout, operation_timeout).unwrap();

    let (connect_phases, connect_phase_receiver) = mpsc::sync_channel(2);
    let (connect_permits, connect_permit_receiver) = mpsc::sync_channel(2);
    let (connect_result, connect_result_receiver) = mpsc::sync_channel(1);
    let mut connect_client = McpClient::new(
        StepDelayTransport::new(
            [Ok(initialized())],
            [first_step_delay, second_step_delay],
            connect_phases,
            connect_permit_receiver,
        ),
        timeouts,
        limits(),
    );
    let connect_cancellation = Arc::clone(&cancellation);
    let connect_worker = thread::spawn(move || {
        connect_result
            .send(connect_client.connect(initialize(), &connect_cancellation))
            .unwrap();
    });

    assert_eq!(
        connect_phase_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        McpRequest::Initialize(initialize())
    );
    connect_permits.send(()).unwrap();
    assert_eq!(
        connect_phase_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        McpRequest::Initialized
    );
    connect_permits.send(()).unwrap();
    let connect_outcome = connect_result_receiver
        .recv_timeout(Duration::from_secs(4))
        .unwrap();
    connect_worker.join().unwrap();

    let (list_phases, list_phase_receiver) = mpsc::sync_channel(2);
    let (list_permits, list_permit_receiver) = mpsc::sync_channel(2);
    let (list_result, list_result_receiver) = mpsc::sync_channel(1);
    let mut list_client = McpClient::new(
        StepDelayTransport::new(
            [
                Ok(page(vec![tool("one", None)], Some("next"))),
                Ok(page(vec![tool("two", None)], None)),
            ],
            [first_step_delay, second_step_delay],
            list_phases,
            list_permit_receiver,
        ),
        timeouts,
        limits(),
    );
    let list_cancellation = Arc::clone(&cancellation);
    let list_worker = thread::spawn(move || {
        list_result
            .send(list_client.list_tools(&list_cancellation))
            .unwrap();
    });

    assert_eq!(
        list_phase_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        McpRequest::ListTools { cursor: None }
    );
    list_permits.send(()).unwrap();
    assert_eq!(
        list_phase_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        McpRequest::ListTools {
            cursor: Some("next".into())
        }
    );
    list_permits.send(()).unwrap();
    let list_outcome = list_result_receiver
        .recv_timeout(Duration::from_secs(4))
        .unwrap();
    assert_eq!(
        (connect_outcome, list_outcome),
        (
            Err(McpTransportError::TimedOut),
            Err(McpTransportError::TimedOut)
        )
    );
    list_worker.join().unwrap();
}

#[test]
fn discover_server_observes_a_live_cancellation_flag_during_connect() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let transport =
        LocalTransport::with_responses([Ok(initialized())]).delayed(Duration::from_secs(5));
    let mut registry = McpRegistry::new();
    registry
        .configure_server(
            "files",
            move || Ok(Box::new(transport.clone())),
            timeouts(),
            limits(),
        )
        .unwrap();
    registry.set_discovery_cancellation(Arc::clone(&cancellation));

    let started = Instant::now();
    let worker_cancellation = Arc::clone(&cancellation);
    let worker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(5));
        worker_cancellation.store(true, Ordering::Release);
        registry.discover_server("files")
    });
    let report = worker.join().unwrap();

    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(report.is_failed());
    assert_eq!(
        report,
        McpServerReport::Failed {
            server_name: "files".into(),
            message: "cancelled: connect cancelled".into(),
        }
    );
}

#[test]
fn concurrent_server_loading_isolates_a_cooperative_deadline_and_keeps_resources_bounded() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let slow = LocalTransport::with_responses([Ok(initialized())]).delayed(Duration::from_secs(5));
    let healthy = LocalTransport::with_responses([
        Ok(initialized()),
        Ok(page(vec![tool("status", Some(true))], None)),
    ]);
    let mut registry = McpRegistry::new();
    let start = Instant::now();

    let reports = registry.load_servers(
        [("slow".into(), slow), ("healthy".into(), healthy)],
        &initialize(),
        McpTimeouts::new(
            Duration::from_millis(2),
            Duration::from_millis(2),
            Duration::from_millis(2),
        )
        .unwrap(),
        limits(),
        Arc::clone(&cancellation),
    );

    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(reports[0].is_failed());
    assert_eq!(reports[1], McpServerReport::loaded("healthy", 1));
    assert!(registry.tool("healthy::status").is_some());
}

#[test]
fn repeated_cooperative_timeouts_do_not_accumulate_workers() {
    let cancellation = Arc::new(AtomicBool::new(false));
    for _ in 0..32 {
        let transport =
            LocalTransport::with_responses([Ok(McpResponse::ToolCalled(McpCallResult {
                content: vec![],
                is_error: false,
            }))])
            .delayed(Duration::from_millis(3));
        let mut client = McpClient::new(
            transport,
            McpTimeouts::new(
                Duration::from_millis(1),
                Duration::from_millis(1),
                Duration::from_millis(1),
            )
            .unwrap(),
            limits(),
        );
        assert_eq!(
            client.call_tool("slow", json!({}), &cancellation),
            Err(McpTransportError::TimedOut)
        );
        let transport = client.into_transport();
        assert_eq!(transport.cancelled.load(Ordering::Acquire), 1);
    }
}

#[test]
fn http_and_sse_transports_send_json_rpc_requests() {
    for content_type in ["application/json", "text/event-stream"] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let max_retries = u32::from(content_type == "text/event-stream") * 2;
        let expected_requests = max_retries as usize + 1;
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = Arc::clone(&attempts);
        let server = thread::spawn(move || {
            for attempt in 0..expected_requests {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert_eq!(request, "POST /mcp HTTP/1.1\r\n");
                server_attempts.fetch_add(1, Ordering::AcqRel);

                let response = if attempt < max_retries as usize {
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                } else {
                    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}}"#;
                    let body = if content_type == "text/event-stream" {
                        format!("data: {body}\n\n")
                    } else {
                        body.to_owned()
                    };
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                stream
                    .try_clone()
                    .unwrap()
                    .write_all(response.as_bytes())
                    .unwrap();
            }
        });
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut transport = McpHttpTransport::new(
            format!("http://{address}/mcp"),
            Default::default(),
            max_retries,
        )
        .unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let response = runtime
            .block_on(async {
                transport.execute(
                    McpRequest::Initialize(initialize()),
                    &McpOperationContext::new(cancellation, Duration::from_secs(1)),
                )
            })
            .unwrap();

        assert_eq!(response, initialized());
        server.join().unwrap();
        assert_eq!(attempts.load(Ordering::Acquire), expected_requests);
    }
}

#[test]
fn legacy_sse_transport_discovers_the_message_endpoint_and_returns_json_rpc_responses() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut events, request) = accept_http_request(&listener);
        assert_eq!(request.lines().next(), Some("GET /events HTTP/1.1"));
        write!(
            events,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n: keepalive\n\nevent: endpoint\ndata: /message\n\n"
        )
        .unwrap();
        events.flush().unwrap();

        let (mut message, request) = accept_http_request(&listener);
        assert_eq!(request.lines().next(), Some("POST /message HTTP/1.1"));
        respond(&mut message, "202 Accepted", b"", "");
        let response = String::from_utf8(initialized_body()).unwrap();
        let split = response.find(",\"id\"").unwrap();
        write!(
            events,
            "event: message\ndata: {}\ndata: {}\n\n",
            &response[..split + 1],
            &response[split + 1..]
        )
        .unwrap();
        events.flush().unwrap();
    });
    let mut transport =
        McpSseTransport::new(format!("http://{address}/events"), Default::default(), 0).unwrap();

    assert_eq!(
        transport.execute(
            McpRequest::Initialize(initialize()),
            &McpOperationContext::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)),
        ),
        Ok(initialized())
    );
    server.join().unwrap();
}

#[test]
fn legacy_sse_transport_retries_transient_failures_with_one_deadline() {
    for (status, reason) in [
        ("408", "Request Timeout"),
        ("429", "Too Many Requests"),
        ("500", "Internal Server Error"),
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut failed, _) = accept_http_request(&listener);
            respond(&mut failed, &format!("{status} {reason}"), b"", "");
            let (mut events, _) = accept_http_request(&listener);
            write!(
                events,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: endpoint\ndata: /message\n\n"
            )
            .unwrap();
            events.flush().unwrap();
            let (mut message, _) = accept_http_request(&listener);
            respond(&mut message, "202 Accepted", b"", "");
            write!(
                events,
                "event: message\ndata: {}\n\n",
                String::from_utf8(initialized_body()).unwrap()
            )
            .unwrap();
            events.flush().unwrap();
        });
        let mut transport =
            McpSseTransport::new(format!("http://{address}/events"), Default::default(), 1)
                .unwrap();

        assert_eq!(
            transport.execute(
                McpRequest::Initialize(initialize()),
                &McpOperationContext::new(Arc::new(AtomicBool::new(false)), UNOBSERVED_DEADLINE)
            ),
            Ok(initialized()),
            "{status} must retry once then accept the SSE response"
        );
        server.join().unwrap();
    }

    let mut transport = McpSseTransport::new(
        format!("http://{UNREACHABLE_AUTHORITY}/events"),
        Default::default(),
        1,
    )
    .unwrap();
    assert_eq!(
        transport.execute(
            McpRequest::Initialize(initialize()),
            &McpOperationContext::new(Arc::new(AtomicBool::new(false)), UNOBSERVED_DEADLINE)
        ),
        Err(McpTransportError::RetriesExhausted)
    );

    let (listener, address) = bind_pollable_listener();
    let (stalled, attempts) = mpsc::sync_channel(2);
    let stop = Arc::new(AtomicBool::new(false));
    let server = stall_the_retry_of_a_failed_attempt(listener, stalled, Arc::clone(&stop));
    let mut transport =
        McpSseTransport::new(format!("http://{address}/events"), Default::default(), 1).unwrap();
    let start = Instant::now();

    assert_eq!(
        transport.execute(
            McpRequest::Initialize(initialize()),
            &McpOperationContext::new(Arc::new(AtomicBool::new(false)), SSE_STALLED_RETRY_DEADLINE)
        ),
        Err(McpTransportError::TimedOut)
    );
    assert!(
        start.elapsed() < SSE_STALLED_RETRY_CEILING,
        "{:?}",
        start.elapsed()
    );
    assert_eq!(
        attempts.recv_timeout(UNOBSERVED_DEADLINE),
        Ok(HttpAttempt::Failed)
    );
    assert_eq!(
        attempts.recv_timeout(UNOBSERVED_DEADLINE),
        Ok(HttpAttempt::Stalled)
    );

    stop.store(true, Ordering::Release);
    server.join().unwrap();
}

#[test]
fn legacy_sse_transport_rejects_non_retryable_protocols_and_cross_origin_endpoints() {
    for (response, expected) in [
        (
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
            McpTransportError::HttpStatus(401),
        ),
        (
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n",
            McpTransportError::HttpStatus(400),
        ),
        (
            "HTTP/1.1 302 Found\r\nLocation: /other\r\nContent-Length: 0\r\n\r\n",
            McpTransportError::Transport("MCP HTTP redirect refused".into()),
        ),
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = accept_http_request(&listener);
            stream.write_all(response.as_bytes()).unwrap();
            thread::sleep(Duration::from_millis(50));
            listener.set_nonblocking(true).unwrap();
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
            );
        });
        let mut transport =
            McpSseTransport::new(format!("http://{address}/events"), Default::default(), 1)
                .unwrap();
        assert_eq!(
            transport.execute(
                McpRequest::Initialize(initialize()),
                &McpOperationContext::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1))
            ),
            Err(expected)
        );
        server.join().unwrap();
    }

    let remote = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let remote_address = remote.local_addr().unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut events, headers) = accept_http_request(&listener);
        assert!(headers.contains("authorization: SENTINEL_SECRET\r\n"));
        write!(events, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: endpoint\ndata: http://{remote_address}/message\n\n").unwrap();
        events.flush().unwrap();
        thread::sleep(Duration::from_millis(50));
        remote.set_nonblocking(true).unwrap();
        assert!(
            matches!(remote.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    });
    let mut transport = McpSseTransport::new(
        format!("http://{address}/events"),
        [("authorization".into(), "SENTINEL_SECRET".into())].into(),
        1,
    )
    .unwrap();
    let result = transport.execute(
        McpRequest::Initialize(initialize()),
        &McpOperationContext::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)),
    );
    assert_eq!(
        result,
        Err(McpTransportError::Protocol(
            "MCP HTTP response is malformed".into()
        ))
    );
    assert!(!result.unwrap_err().to_string().contains("SENTINEL_SECRET"));
    server.join().unwrap();
}

#[test]
fn legacy_sse_transport_bounds_framing_and_waits_interruptibly() {
    for (body, expected) in [
        (
            "",
            McpTransportError::Protocol("MCP HTTP response is malformed".into()),
        ),
        (
            "event: unknown\ndata: value\n\n",
            McpTransportError::Protocol("MCP HTTP response is malformed".into()),
        ),
        (
            "event: message\ndata: not json\n\n",
            McpTransportError::Protocol("MCP HTTP response is malformed".into()),
        ),
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = accept_http_request(&listener);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{body}"
            )
            .unwrap();
        });
        let mut transport =
            McpSseTransport::new(format!("http://{address}/events"), Default::default(), 1)
                .unwrap();
        assert_eq!(
            transport.execute(
                McpRequest::Initialize(initialize()),
                &McpOperationContext::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1))
            ),
            Err(expected)
        );
        server.join().unwrap();
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_http_request(&listener);
        let body = vec![b'x'; 1024 * 1024 + 1];
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: message\ndata: "
        )
        .unwrap();
        stream.write_all(&body).unwrap();
    });
    let mut transport =
        McpSseTransport::new(format!("http://{address}/events"), Default::default(), 0).unwrap();
    assert_eq!(
        transport.execute(
            McpRequest::Initialize(initialize()),
            &McpOperationContext::new(Arc::new(AtomicBool::new(false)), UNOBSERVED_DEADLINE)
        ),
        Err(McpTransportError::Protocol(
            "MCP SSE event exceeds limit".into()
        ))
    );
    server.join().unwrap();

    // Each half stalls the server for as long as the client is willing to wait, so a
    // result arriving at all is what proves the wait was interrupted. Only one bound may
    // end the call: the cancelled half gets a deadline it cannot reach, and the timed-out
    // half is never cancelled and gets a deadline long enough that connecting to loopback
    // cannot consume it, because a deadline that expires before the client connects would
    // leave the accept below unmatched.
    for (cancel, deadline, expected) in [
        (true, UNOBSERVED_DEADLINE, McpTransportError::Cancelled),
        (
            false,
            Duration::from_millis(500),
            McpTransportError::TimedOut,
        ),
    ] {
        let (listener, address) = bind_pollable_listener();
        let (started, ready) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = thread::spawn(move || {
            let Some(_stream) = accept_until_stopped(&listener, &server_stop) else {
                return;
            };
            started.send(()).unwrap();
            while !server_stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
        });
        let cancellation = Arc::new(AtomicBool::new(false));
        let context = McpOperationContext::new(Arc::clone(&cancellation), deadline);
        let mut transport =
            McpSseTransport::new(format!("http://{address}/events"), Default::default(), 0)
                .unwrap();
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            result_sender
                .send(transport.execute(McpRequest::Initialize(initialize()), &context))
                .unwrap();
        });
        ready.recv_timeout(UNOBSERVED_DEADLINE).unwrap();
        if cancel {
            cancellation.store(true, Ordering::Release);
        }
        assert_eq!(
            result_receiver.recv_timeout(UNOBSERVED_DEADLINE).unwrap(),
            Err(expected)
        );

        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }
}

#[test]
fn legacy_sse_transport_accepts_exact_limit_exhausts_retries_and_closes_on_current_runtime() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut events, _) = accept_http_request(&listener);
        write!(events, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: endpoint\ndata: /message\n\n").unwrap();
        events.flush().unwrap();
        let (mut message, _) = accept_http_request(&listener);
        respond(&mut message, "202 Accepted", b"", "");
        let mut body = initialized_body();
        body.extend(std::iter::repeat_n(b' ', 1024 * 1024 - body.len()));
        write!(events, "event: message\ndata: ").unwrap();
        events.write_all(&body).unwrap();
        events.flush().unwrap();
        // Force the "\n\n" terminator into a later socket read so the reader
        // observes a pending, unterminated line at exactly the byte limit.
        thread::sleep(Duration::from_millis(50));
        write!(events, "\n\n").unwrap();
        events.flush().unwrap();
    });
    let mut transport =
        McpSseTransport::new(format!("http://{address}/events"), Default::default(), 0).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    assert_eq!(
        runtime.block_on(async {
            transport.execute(
                McpRequest::Initialize(initialize()),
                &McpOperationContext::new(Arc::new(AtomicBool::new(false)), UNOBSERVED_DEADLINE),
            )
        }),
        Ok(initialized())
    );
    transport
        .close(&McpOperationContext::new(
            Arc::new(AtomicBool::new(false)),
            UNOBSERVED_DEADLINE,
        ))
        .unwrap();
    drop(transport);
    server.join().unwrap();

    for (status, reason) in [
        ("408", "Request Timeout"),
        ("429", "Too Many Requests"),
        ("500", "Internal Server Error"),
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = accept_http_request(&listener);
                respond(&mut stream, &format!("{status} {reason}"), b"", "");
            }
        });
        let mut transport =
            McpSseTransport::new(format!("http://{address}/events"), Default::default(), 1)
                .unwrap();
        assert_eq!(
            transport.execute(
                McpRequest::Initialize(initialize()),
                &McpOperationContext::new(Arc::new(AtomicBool::new(false)), UNOBSERVED_DEADLINE)
            ),
            Err(McpTransportError::RetriesExhausted)
        );
        server.join().unwrap();
    }
}

#[test]
fn http_transport_retries_only_transient_statuses_and_reports_exhaustion() {
    for (status, reason, retries) in [
        (408, "Request Timeout", true),
        (429, "Too Many Requests", true),
        (500, "Internal Server Error", true),
        (400, "Bad Request", false),
        (401, "Unauthorized", false),
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = Arc::clone(&attempts);
        let expected_attempts = usize::from(retries) + 1;
        let server = thread::spawn(move || {
            for _ in 0..expected_attempts {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                server_attempts.fetch_add(1, Ordering::AcqRel);
                write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
        });
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut transport =
            McpHttpTransport::new(format!("http://{address}/mcp"), Default::default(), 1).unwrap();

        let result = transport.execute(
            McpRequest::Initialize(initialize()),
            &McpOperationContext::new(cancellation, Duration::from_secs(1)),
        );

        if retries {
            assert_eq!(result, Err(McpTransportError::RetriesExhausted));
        } else {
            assert_eq!(result, Err(McpTransportError::HttpStatus(status)));
        }
        server.join().unwrap();
        assert_eq!(attempts.load(Ordering::Acquire), expected_attempts);
    }
}

#[test]
fn http_transport_rejects_responses_larger_than_one_mib() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
        }
        let body = vec![b'x'; 1024 * 1024 + 1];
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
    });
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut transport =
        McpHttpTransport::new(format!("http://{address}/mcp"), Default::default(), 1).unwrap();

    let result = transport.execute(
        McpRequest::Initialize(initialize()),
        &McpOperationContext::new(cancellation, UNOBSERVED_DEADLINE),
    );

    assert_eq!(
        result,
        Err(McpTransportError::Protocol(
            "MCP HTTP response exceeds limit".into()
        ))
    );
    server.join().unwrap();
}

#[test]
fn http_transport_cancels_a_live_headless_turn_after_request_admission() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (admitted, admission) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let (_stream, _) = accept_http_request(&listener);
        admitted.send(()).unwrap();
        thread::sleep(Duration::from_secs(1));
    });
    let cancellation = HeadlessTurnCancellation::with_deadline(Duration::from_secs(2));
    let context = McpOperationContext::from_headless_adapter(cancellation.adapter_view());
    let mut transport =
        McpHttpTransport::new(format!("http://{address}/mcp"), Default::default(), 0).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let (result_sender, result_receiver) = mpsc::sync_channel(1);

    thread::spawn(move || {
        result_sender
            .send(runtime.block_on(async {
                transport.execute(McpRequest::Initialize(initialize()), &context)
            }))
            .unwrap();
    });
    admission.recv_timeout(Duration::from_secs(1)).unwrap();
    cancellation.cancel();

    assert_eq!(
        result_receiver
            .recv_timeout(Duration::from_millis(250))
            .unwrap(),
        Err(McpTransportError::Cancelled)
    );
    server.join().unwrap();
}

#[test]
fn http_transport_shares_one_deadline_across_retries_and_retries_network_failures() {
    let (listener, address) = bind_pollable_listener();
    let (observed, attempts) = mpsc::sync_channel(BUDGETED_RETRIES as usize + 1);
    let stop = Arc::new(AtomicBool::new(false));
    let server =
        fail_every_attempt_after_spending_the_budget(listener, observed, Arc::clone(&stop));
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut transport = McpHttpTransport::new(
        format!("http://{address}/mcp"),
        Default::default(),
        BUDGETED_RETRIES,
    )
    .unwrap();
    let start = Instant::now();

    let result = transport.execute(
        McpRequest::Initialize(initialize()),
        &McpOperationContext::new(cancellation, HTTP_SPENT_BUDGET_DEADLINE),
    );
    let elapsed = start.elapsed();

    stop.store(true, Ordering::Release);
    server.join().unwrap();

    assert_eq!(
        result,
        Err(McpTransportError::TimedOut),
        "what the first attempt spent must still bind the retries; a deadline renewed per \
         attempt would instead pay for every one of them and report exhausted retries"
    );
    assert_eq!(
        attempts.iter().collect::<Vec<_>>(),
        vec![HttpAttempt::Failed, HttpAttempt::Failed],
        "the shared budget must run out before the last permitted attempt is made"
    );
    assert!(elapsed < HTTP_SPENT_BUDGET_CEILING, "{elapsed:?}");

    let mut transport = McpHttpTransport::new(
        format!("http://{UNREACHABLE_AUTHORITY}/mcp"),
        Default::default(),
        1,
    )
    .unwrap();
    assert_eq!(
        transport.execute(
            McpRequest::Initialize(initialize()),
            &McpOperationContext::new(Arc::new(AtomicBool::new(false)), UNOBSERVED_DEADLINE),
        ),
        Err(McpTransportError::RetriesExhausted)
    );
}

#[test]
fn http_transport_never_retries_protocol_errors_and_accepts_exactly_one_mib() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_http_request(&listener);
        respond(&mut stream, "200 OK", b"not json", "");
        thread::sleep(Duration::from_millis(100));
        listener.set_nonblocking(true).unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    });
    let mut transport =
        McpHttpTransport::new(format!("http://{address}/mcp"), Default::default(), 1).unwrap();
    assert!(matches!(
        transport.execute(
            McpRequest::Initialize(initialize()),
            &McpOperationContext::new(Arc::new(AtomicBool::new(false)), UNOBSERVED_DEADLINE),
        ),
        Err(McpTransportError::Protocol(_))
    ));
    server.join().unwrap();

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_http_request(&listener);
        let mut body = initialized_body();
        body.extend(std::iter::repeat_n(b' ', 1024 * 1024 - body.len()));
        respond(
            &mut stream,
            "200 OK",
            &body,
            "Content-Type: application/json\r\n",
        );
    });
    let mut transport =
        McpHttpTransport::new(format!("http://{address}/mcp"), Default::default(), 0).unwrap();
    assert_eq!(
        transport.execute(
            McpRequest::Initialize(initialize()),
            &McpOperationContext::new(Arc::new(AtomicBool::new(false)), UNOBSERVED_DEADLINE),
        ),
        Ok(initialized())
    );
    server.join().unwrap();
}

#[test]
fn http_transport_sends_the_streamable_http_accept_header_on_every_post() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, headers) = accept_http_request(&listener);
        assert!(
            headers.contains("accept: application/json, text/event-stream\r\n"),
            "missing Accept header: {headers}"
        );
        respond(
            &mut stream,
            "200 OK",
            &initialized_body(),
            "Content-Type: application/json\r\n",
        );
    });
    let mut transport =
        McpHttpTransport::new(format!("http://{address}/mcp"), Default::default(), 0).unwrap();
    assert_eq!(
        transport.execute(
            McpRequest::Initialize(initialize()),
            &McpOperationContext::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)),
        ),
        Ok(initialized())
    );
    server.join().unwrap();
}

#[test]
fn http_transport_surfaces_non_retryable_statuses_as_structured_http_status_without_leaking_remote_text()
 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_http_request(&listener);
        respond(
            &mut stream,
            "406 Not Acceptable",
            b"SENTINEL_SECRET body",
            "",
        );
    });
    let mut transport =
        McpHttpTransport::new(format!("http://{address}/mcp"), Default::default(), 0).unwrap();
    let result = transport.execute(
        McpRequest::Initialize(initialize()),
        &McpOperationContext::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)),
    );
    assert_eq!(result, Err(McpTransportError::HttpStatus(406)));
    assert!(!result.unwrap_err().to_string().contains("SENTINEL_SECRET"));
    server.join().unwrap();
}

#[test]
fn http_transport_captures_and_echoes_the_mcp_session_id() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut first, _) = accept_http_request(&listener);
        respond(
            &mut first,
            "200 OK",
            &initialized_body(),
            "Content-Type: application/json\r\nMcp-Session-Id: s-1\r\n",
        );

        let (mut second, headers) = accept_http_request(&listener);
        assert!(
            headers.contains("mcp-session-id: s-1\r\n"),
            "session id was not echoed back: {headers}"
        );
        let body = br#"{"jsonrpc":"2.0","id":2,"result":{"tools":[],"nextCursor":null}}"#.to_vec();
        respond(
            &mut second,
            "200 OK",
            &body,
            "Content-Type: application/json\r\n",
        );
    });
    let mut transport =
        McpHttpTransport::new(format!("http://{address}/mcp"), Default::default(), 0).unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));
    let context = McpOperationContext::new(Arc::clone(&cancellation), Duration::from_secs(1));
    assert_eq!(
        transport.execute(McpRequest::Initialize(initialize()), &context),
        Ok(initialized())
    );
    assert_eq!(
        transport.execute(McpRequest::ListTools { cursor: None }, &context),
        Ok(page(vec![], None))
    );
    server.join().unwrap();
}

#[test]
fn http_transport_ignores_out_of_bounds_session_ids() {
    for session_id_header in [
        format!("Mcp-Session-Id: {}\r\n", "s".repeat(513)),
        "Mcp-Session-Id: caf\u{e9}\r\n".to_string(),
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut first, _) = accept_http_request(&listener);
            respond(
                &mut first,
                "200 OK",
                &initialized_body(),
                &format!("Content-Type: application/json\r\n{session_id_header}"),
            );

            let (mut second, headers) = accept_http_request(&listener);
            assert!(
                !headers.to_ascii_lowercase().contains("mcp-session-id"),
                "an invalid session id must never be echoed: {headers}"
            );
            let body =
                br#"{"jsonrpc":"2.0","id":2,"result":{"tools":[],"nextCursor":null}}"#.to_vec();
            respond(
                &mut second,
                "200 OK",
                &body,
                "Content-Type: application/json\r\n",
            );
        });
        let mut transport =
            McpHttpTransport::new(format!("http://{address}/mcp"), Default::default(), 0).unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        let context = McpOperationContext::new(Arc::clone(&cancellation), Duration::from_secs(1));
        assert_eq!(
            transport.execute(McpRequest::Initialize(initialize()), &context),
            Ok(initialized())
        );
        assert_eq!(
            transport.execute(McpRequest::ListTools { cursor: None }, &context),
            Ok(page(vec![], None))
        );
        server.join().unwrap();
    }
}

#[test]
fn connect_accepts_any_supported_protocol_version_and_rejects_an_unsupported_one() {
    let cancellation = Arc::new(AtomicBool::new(false));
    for version in agens_tools::SUPPORTED_MCP_PROTOCOL_VERSIONS {
        let transport = LocalTransport::with_responses([Ok(McpResponse::Initialized(
            McpInitializeResult::new(version, json!({"tools": {}})),
        ))]);
        let mut client = McpClient::new(transport, timeouts(), limits());
        assert_eq!(
            client.connect(initialize(), &cancellation),
            Ok(()),
            "version {version} must be accepted"
        );
    }

    let transport = LocalTransport::with_responses([Ok(McpResponse::Initialized(
        McpInitializeResult::new("2024-11-05", json!({"tools": {}})),
    ))]);
    let mut client = McpClient::new(transport, timeouts(), limits());
    assert_eq!(
        client.connect(initialize(), &cancellation),
        Err(McpTransportError::Protocol(
            "MCP protocol version negotiation failed".into()
        ))
    );
}

#[test]
fn http_transport_refuses_redirects_without_leaking_sensitive_headers() {
    let redirect = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let redirect_address = redirect.local_addr().unwrap();
    let origin = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let origin_address = origin.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, headers) = accept_http_request(&origin);
        assert!(headers.contains("authorization: SENTINEL_SECRET\r\n"));
        respond(
            &mut stream,
            "302 Found",
            b"",
            &format!("Location: http://{redirect_address}/other\r\n"),
        );
        thread::sleep(Duration::from_millis(100));
        redirect.set_nonblocking(true).unwrap();
        assert!(
            matches!(redirect.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    });
    let mut transport = McpHttpTransport::new(
        format!("http://{origin_address}/mcp"),
        [("authorization".into(), "SENTINEL_SECRET".into())].into(),
        1,
    )
    .unwrap();
    let result = transport.execute(
        McpRequest::Initialize(initialize()),
        &McpOperationContext::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)),
    );

    assert_eq!(
        result,
        Err(McpTransportError::Transport(
            "MCP HTTP redirect refused".into()
        ))
    );
    assert!(!result.unwrap_err().to_string().contains("SENTINEL_SECRET"));
    server.join().unwrap();
}
