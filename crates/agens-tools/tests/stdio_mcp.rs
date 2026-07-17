use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use agens_tools::{
    McpClient, McpInitialize, McpLimits, McpStdioTransport, McpStdioTransportConfig, McpTimeouts,
    McpTransportError,
};
use serde_json::json;

fn transport(mode: &str) -> McpStdioTransport {
    McpStdioTransport::spawn(McpStdioTransportConfig {
        command: PathBuf::from(env!("CARGO_BIN_EXE_fake-mcp-child")),
        args: vec![mode.into()],
        environment: BTreeMap::new(),
        project_root: std::env::current_dir().unwrap(),
    })
    .unwrap()
}

fn client(mode: &str, timeout: Duration) -> McpClient<McpStdioTransport> {
    McpClient::new(
        transport(mode),
        McpTimeouts::new(timeout, timeout, timeout).unwrap(),
        McpLimits::new(4, 4).unwrap(),
    )
}

fn initialize() -> McpInitialize {
    McpInitialize::new("2025-06-18", json!({}), "agens", "test")
}

#[test]
fn stdio_transport_initializes_lists_paginates_and_maps_tool_results() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut client = client("success", Duration::from_secs(1));
    client.connect(initialize(), &cancellation).unwrap();
    let tools = client.list_tools(&cancellation).unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(
        client
            .call_tool("first", json!({}), &cancellation)
            .unwrap()
            .content,
        "tool succeeded"
    );
}

#[test]
fn stdio_transport_keeps_protocol_transport_deadline_and_cancellation_failures_distinct() {
    let cancellation = Arc::new(AtomicBool::new(false));
    for mode in ["malformed", "oversize", "id-mismatch"] {
        let mut client = client(mode, Duration::from_secs(1));
        assert!(
            matches!(
                client.call_tool("x", json!({}), &cancellation),
                Err(McpTransportError::Protocol(_))
            ),
            "{mode}"
        );
    }
    let mut crashed = client("crash", Duration::from_secs(1));
    assert!(matches!(
        crashed.call_tool("x", json!({}), &cancellation),
        Err(McpTransportError::Transport(_))
    ));
    let mut timed = client("sleep", Duration::from_millis(20));
    assert_eq!(
        timed.call_tool("x", json!({}), &cancellation),
        Err(McpTransportError::TimedOut)
    );
    let mut cancelled = client("sleep", Duration::from_secs(1));
    let signal = Arc::clone(&cancellation);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(10));
        signal.store(true, Ordering::Release);
    });
    assert_eq!(
        cancelled.call_tool("x", json!({}), &cancellation),
        Err(McpTransportError::Cancelled)
    );
}

#[test]
fn stdio_transport_drains_stderr_and_maps_is_error() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut noisy = client("stderr-flood", Duration::from_secs(1));
    assert_eq!(
        noisy
            .call_tool("x", json!({}), &cancellation)
            .unwrap()
            .content,
        "tool succeeded"
    );
    let mut failed = client("call-error", Duration::from_secs(1));
    let output = failed.call_tool("x", json!({}), &cancellation).unwrap();
    assert_eq!(
        (output.content, output.is_error),
        ("tool failed".into(), true)
    );
}
