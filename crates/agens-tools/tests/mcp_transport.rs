use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agens_tools::{
    McpCallResult, McpClient, McpContentBlock, McpInitialize, McpProtocolError, McpRegistry,
    McpRequest, McpRequestId, McpResponse, McpServerReport, McpTimeouts, McpToolAnnotations,
    McpToolDefinition, McpTransport, McpTransportError, RemoteToolAccess, ToolOutput,
};
use serde_json::json;

#[derive(Default)]
struct LocalTransport {
    responses: VecDeque<Result<Option<McpResponse>, McpTransportError>>,
    requests: Vec<McpRequest>,
    cancelled: Vec<McpRequestId>,
    closed: bool,
    next_id: u64,
}

impl LocalTransport {
    fn with_responses(
        responses: impl IntoIterator<Item = Result<Option<McpResponse>, McpTransportError>>,
    ) -> Self {
        Self {
            responses: responses.into_iter().collect(),
            ..Self::default()
        }
    }
}

impl McpTransport for LocalTransport {
    fn begin(&mut self, request: McpRequest) -> Result<McpRequestId, McpTransportError> {
        self.next_id += 1;
        self.requests.push(request);
        Ok(McpRequestId(self.next_id))
    }

    fn poll(
        &mut self,
        _request_id: McpRequestId,
    ) -> Result<Option<McpResponse>, McpTransportError> {
        self.responses.pop_front().unwrap_or(Ok(None))
    }

    fn cancel(&mut self, request_id: McpRequestId) {
        self.cancelled.push(request_id);
    }

    fn close(&mut self) -> Result<(), McpTransportError> {
        self.closed = true;
        Ok(())
    }
}

fn initialize() -> McpInitialize {
    McpInitialize::new("2025-06-18", json!({}), "agens", "0.1.0")
}

fn timeouts() -> McpTimeouts {
    McpTimeouts::new(
        Duration::from_millis(20),
        Duration::from_millis(20),
        Duration::from_millis(20),
    )
    .unwrap()
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

#[test]
fn registers_remote_tools_with_qualified_names_and_conservative_access() {
    let cancellation = AtomicBool::new(false);
    let mut transport = LocalTransport::with_responses([
        Ok(Some(McpResponse::Initialized)),
        Ok(Some(McpResponse::ToolsListed(vec![
            tool("read", Some(true)),
            tool("write", None),
        ]))),
    ]);
    let mut registry = McpRegistry::new();

    let report = registry.load_server(
        "files",
        &mut transport,
        &initialize(),
        timeouts(),
        &cancellation,
    );

    assert_eq!(report, McpServerReport::loaded("files", 2));
    assert_eq!(registry.len(), 2);
    assert_eq!(
        registry.tool("files::read").unwrap().access,
        RemoteToolAccess::ReadOnly
    );
    assert_eq!(
        registry.tool("files::write").unwrap().access,
        RemoteToolAccess::Write
    );
    assert!(matches!(transport.requests[0], McpRequest::Initialize(_)));
    assert!(matches!(transport.requests[1], McpRequest::Initialized));
    assert!(matches!(transport.requests[2], McpRequest::ListTools));
}

#[test]
fn rejects_duplicate_qualified_names_and_invalid_protocol_shapes() {
    let cancellation = AtomicBool::new(false);
    let mut registry = McpRegistry::new();
    let mut first = LocalTransport::with_responses([
        Ok(Some(McpResponse::Initialized)),
        Ok(Some(McpResponse::ToolsListed(vec![tool(
            "read",
            Some(true),
        )]))),
    ]);
    assert_eq!(
        registry.load_server(
            "files",
            &mut first,
            &initialize(),
            timeouts(),
            &cancellation
        ),
        McpServerReport::loaded("files", 1)
    );

    let mut duplicate = LocalTransport::with_responses([
        Ok(Some(McpResponse::Initialized)),
        Ok(Some(McpResponse::ToolsListed(vec![tool(
            "read",
            Some(true),
        )]))),
    ]);
    assert!(
        registry
            .load_server(
                "files",
                &mut duplicate,
                &initialize(),
                timeouts(),
                &cancellation
            )
            .is_failed()
    );

    let mut invalid_schema = LocalTransport::with_responses([
        Ok(Some(McpResponse::Initialized)),
        Ok(Some(McpResponse::ToolsListed(vec![McpToolDefinition {
            input_schema: json!("not-an-object"),
            ..tool("bad", None)
        }]))),
    ]);
    assert!(
        registry
            .load_server(
                "invalid",
                &mut invalid_schema,
                &initialize(),
                timeouts(),
                &cancellation
            )
            .is_failed()
    );
    assert!(invalid_schema.closed);

    let mut client = McpClient::new(&mut first, timeouts());
    assert_eq!(
        client.call_tool("read", json!("not-an-object"), &cancellation),
        Ok(ToolOutput::failure(
            "mcp: tool arguments must be a JSON object"
        ))
    );
}

#[test]
fn maps_protocol_and_tool_errors_without_false_success() {
    let cancellation = AtomicBool::new(false);
    let mut transport = LocalTransport::with_responses([
        Ok(Some(McpResponse::ProtocolError(McpProtocolError::new(
            -32001, "denied",
        )))),
        Ok(Some(McpResponse::ToolCalled(McpCallResult {
            content: vec![McpContentBlock::Text("invalid input".into())],
            is_error: true,
        }))),
    ]);
    let mut client = McpClient::new(&mut transport, timeouts());

    assert_eq!(
        client.call_tool("write", json!({"path": "x"}), &cancellation),
        Ok(ToolOutput::failure("mcp protocol error -32001: denied"))
    );
    assert_eq!(
        client.call_tool("write", json!({"path": "x"}), &cancellation),
        Ok(ToolOutput::failure("invalid input"))
    );
}

#[test]
fn isolates_one_server_failure_from_other_server_metadata() {
    let cancellation = AtomicBool::new(false);
    let mut registry = McpRegistry::new();
    let mut unavailable = LocalTransport::with_responses([Ok(Some(McpResponse::Initialized))]);
    let mut available = LocalTransport::with_responses([
        Ok(Some(McpResponse::Initialized)),
        Ok(Some(McpResponse::ToolsListed(vec![tool(
            "status",
            Some(true),
        )]))),
    ]);

    let reports = registry.load_servers(
        [
            ("unavailable", &mut unavailable),
            ("available", &mut available),
        ],
        &initialize(),
        timeouts(),
        &cancellation,
    );

    assert!(reports[0].is_failed());
    assert_eq!(reports[1], McpServerReport::loaded("available", 1));
    assert_eq!(
        registry.tool("available::status").unwrap().access,
        RemoteToolAccess::ReadOnly
    );
}

#[test]
fn timeout_or_cancellation_closes_the_transport_and_rejects_late_responses() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut transport = LocalTransport::with_responses([
        Ok(None),
        Ok(Some(McpResponse::ToolCalled(McpCallResult {
            content: vec![McpContentBlock::Text("late success".into())],
            is_error: false,
        }))),
    ]);
    let mut client = McpClient::new(
        &mut transport,
        McpTimeouts::new(
            Duration::from_millis(1),
            Duration::from_millis(1),
            Duration::from_millis(1),
        )
        .unwrap(),
    );

    std::thread::sleep(Duration::from_millis(2));
    assert_eq!(
        client.call_tool("slow", json!({}), &cancellation),
        Err(McpTransportError::TimedOut)
    );
    assert!(transport.closed);
    assert_eq!(transport.cancelled.len(), 1);

    let mut cancelled_transport = LocalTransport::default();
    let mut cancelled_client = McpClient::new(&mut cancelled_transport, timeouts());
    cancellation.store(true, Ordering::Release);
    assert_eq!(
        cancelled_client.call_tool("slow", json!({}), &cancellation),
        Err(McpTransportError::Cancelled)
    );
    assert!(cancelled_transport.closed);
    assert_eq!(cancelled_transport.cancelled.len(), 1);
}
