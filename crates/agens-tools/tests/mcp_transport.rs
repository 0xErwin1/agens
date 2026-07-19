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
    McpCallResult, McpClient, McpContentBlock, McpHttpTransport, McpInitialize,
    McpInitializeResult, McpLimits, McpOperationContext, McpProtocolError, McpRegistry, McpRequest,
    McpResponse, McpServerReport, McpSseTransport, McpTimeouts, McpToolAnnotations,
    McpToolDefinition, McpToolsPage, McpTransport, McpTransportError, RemoteToolAccess, ToolOutput,
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
    McpInitialize::new("2025-06-18", json!({}), "agens", "0.1.0")
}

fn initialized() -> McpResponse {
    McpResponse::Initialized(McpInitializeResult::new("2025-06-18", json!({"tools": {}})))
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

fn accept_http_request(listener: &TcpListener) -> (TcpStream, String) {
    let (stream, _) = listener.accept().unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut headers = String::new();

    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        headers.push_str(&line);
        if line == "\r\n" {
            return (stream, headers);
        }
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
                "2025-06-18",
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
fn concurrent_server_loading_isolates_a_cooperative_deadline_and_keeps_resources_bounded() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let slow =
        LocalTransport::with_responses([Ok(initialized())]).delayed(Duration::from_millis(20));
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

    assert!(start.elapsed() < Duration::from_millis(15));
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
            assert!(
                matches!(result, Err(McpTransportError::Transport(_))),
                "unexpected result: {result:?}"
            );
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
        &McpOperationContext::new(cancellation, Duration::from_secs(1)),
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
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut first, _) = accept_http_request(&listener);
        respond(&mut first, "500 Internal Server Error", b"", "");
        let (_second, _) = accept_http_request(&listener);
        thread::sleep(Duration::from_secs(1));
    });
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut transport =
        McpHttpTransport::new(format!("http://{address}/mcp"), Default::default(), 1).unwrap();
    let start = Instant::now();

    assert_eq!(
        transport.execute(
            McpRequest::Initialize(initialize()),
            &McpOperationContext::new(cancellation, Duration::from_millis(50)),
        ),
        Err(McpTransportError::TimedOut)
    );
    assert!(start.elapsed() < Duration::from_millis(250));
    server.join().unwrap();

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut transport =
        McpHttpTransport::new(format!("http://{address}/mcp"), Default::default(), 1).unwrap();
    assert_eq!(
        transport.execute(
            McpRequest::Initialize(initialize()),
            &McpOperationContext::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)),
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
            &McpOperationContext::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)),
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
            &McpOperationContext::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)),
        ),
        Ok(initialized())
    );
    server.join().unwrap();
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
