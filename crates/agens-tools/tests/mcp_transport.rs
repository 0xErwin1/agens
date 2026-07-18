use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use agens_tools::{
    McpCallResult, McpClient, McpContentBlock, McpHttpTransport, McpInitialize,
    McpInitializeResult, McpLimits, McpOperationContext, McpProtocolError, McpRegistry, McpRequest,
    McpResponse, McpServerReport, McpTimeouts, McpToolAnnotations, McpToolDefinition, McpToolsPage,
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

        let response = transport
            .execute(
                McpRequest::Initialize(initialize()),
                &McpOperationContext::new(cancellation, Duration::from_secs(1)),
            )
            .unwrap();

        assert_eq!(response, initialized());
        server.join().unwrap();
        assert_eq!(attempts.load(Ordering::Acquire), expected_requests);
    }
}
