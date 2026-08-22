use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use agens_tools::{
    MCP_PROTOCOL_VERSION, McpClient, McpInitialize, McpLimits, McpOperationContext, McpRequest,
    McpStdioTransport, McpStdioTransportConfig, McpTimeouts, McpTransport, McpTransportError,
};
use serde_json::json;

/// A deadline no framing decision can plausibly exhaust, for the tests that
/// assert which failure a mode produces rather than how long it takes.
const OVER_ANY_FRAMING_COST: Duration = Duration::from_secs(30);

/// Long enough that a reconnect running to its own budget is unmistakable
/// next to one that stops at the caller's cancellation.
const RECONNECT_BUDGET: Duration = Duration::from_secs(10);

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
    McpInitialize::new(MCP_PROTOCOL_VERSION, json!({}), "agens", "test")
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
        // Generous, because these assert WHICH failure the mode produces, not
        // how fast: `oversize` alone pushes the frame ceiling through the pipe,
        // and a deadline tight enough to matter would answer `TimedOut` under
        // load no matter what the framing did.
        let mut client = client(mode, OVER_ANY_FRAMING_COST);
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

/// Notifications, a request the server makes of this client, and an answer to
/// a request already abandoned all share the one pipe with the pending
/// response. Every one of them used to fail the call and kill the server.
#[test]
fn stdio_transport_reads_past_the_traffic_the_protocol_interleaves() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut client = client("notify", Duration::from_secs(2));

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

/// A screenshot, an audio clip, or an embedded binary resource has no text to
/// forward, so each becomes a description of what came back. The call still
/// succeeds and the server stays up.
#[test]
fn stdio_transport_describes_content_blocks_that_carry_no_text() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut client = client("call-image", Duration::from_secs(2));
    client.connect(initialize(), &cancellation).unwrap();

    let output = client.call_tool("first", json!({}), &cancellation).unwrap();

    assert!(!output.is_error);
    assert_eq!(
        output.content,
        concat!(
            "here is the screenshot\n",
            "[mcp image content: image/png]\n",
            "[mcp audio content: audio/wav]\n",
            "embedded\n",
            "[mcp resource content: application/octet-stream]"
        )
    );
    // The connection survived the non-text blocks, so the next call still works.
    assert_eq!(
        client
            .call_tool("first", json!({}), &cancellation)
            .unwrap()
            .content,
        concat!(
            "here is the screenshot\n",
            "[mcp image content: image/png]\n",
            "[mcp audio content: audio/wav]\n",
            "embedded\n",
            "[mcp resource content: application/octet-stream]"
        )
    );
}

/// A result that only sets `structuredContent`, and one that is empty, are
/// both legal answers rather than shapes to fail the call over.
#[test]
fn stdio_transport_accepts_a_structured_or_empty_tool_result() {
    let cancellation = Arc::new(AtomicBool::new(false));

    let mut structured = client("call-structured", Duration::from_secs(2));
    structured.connect(initialize(), &cancellation).unwrap();
    assert_eq!(
        structured
            .call_tool("first", json!({}), &cancellation)
            .unwrap()
            .content,
        r#"{"answer":42}"#
    );

    let mut empty = client("call-empty", Duration::from_secs(2));
    empty.connect(initialize(), &cancellation).unwrap();
    let output = empty.call_tool("first", json!({}), &cancellation).unwrap();
    assert!(!output.is_error);
    assert_eq!(output.content, "");
}

/// A result larger than the model's budget is cut down to it with a marker,
/// not failed: the answer is worth more truncated than lost.
#[test]
fn stdio_transport_truncates_a_tool_result_past_the_model_budget() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut client = client("call-oversize", Duration::from_secs(2));
    client.connect(initialize(), &cancellation).unwrap();

    let output = client.call_tool("first", json!({}), &cancellation).unwrap();

    assert!(!output.is_error);
    assert!(output.content.len() <= 64 * 1024);
    assert!(output.content.ends_with("[mcp output truncated]"));
    assert!(output.content.starts_with("xxxx"));
}

/// A server that still answers `2024-11-05` speaks every shape this client
/// sends, so refusing it only cost reachable servers.
#[test]
fn stdio_transport_accepts_the_2024_11_05_protocol_version() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut client = client("legacy-protocol", Duration::from_secs(2));

    client.connect(initialize(), &cancellation).unwrap();

    assert_eq!(
        client
            .call_tool("first", json!({}), &cancellation)
            .unwrap()
            .content,
        "tool succeeded"
    );
}

/// A malformed or oversized frame costs the caller one answer. It used to cost
/// the whole server, so a single bad frame disabled every later tool call.
#[test]
fn stdio_transport_survives_a_protocol_irregularity_on_a_tool_call() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut client = client("call-malformed", Duration::from_secs(2));
    client.connect(initialize(), &cancellation).unwrap();

    assert!(matches!(
        client.call_tool("first", json!({}), &cancellation),
        Err(McpTransportError::Protocol(_))
    ));

    // The transport is still alive: tool listing goes through the same pipe.
    let tools = client.list_tools(&cancellation).unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
}

/// Cancelling a call used to cost the whole server: the pending id was
/// abandoned by killing the process group, so the next call paid a full
/// reconnect for a process that was still perfectly able to answer. The id is
/// abandoned on its own now.
#[test]
fn stdio_transport_keeps_the_server_after_a_cancelled_call() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut client = client("call-slow-once", OVER_ANY_FRAMING_COST);
    client.connect(initialize(), &cancellation).unwrap();

    let signal = Arc::clone(&cancellation);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(10));
        signal.store(true, Ordering::Release);
    });
    assert_eq!(
        client.call_tool("first", json!({}), &cancellation),
        Err(McpTransportError::Cancelled)
    );

    // The same process, reached over the same pipe: the answer to the
    // abandoned id is walked past rather than mistaken for this one.
    cancellation.store(false, Ordering::Release);
    assert_eq!(
        client
            .call_tool("first", json!({}), &cancellation)
            .unwrap()
            .content,
        "tool succeeded"
    );
}

/// A stdio server that dies mid-turn used to leave the registry holding a dead
/// client and a `Ready` status, with `/mcp reload` explicitly skipping it: the
/// only exit was restarting agens. The registry now rebuilds the connection in
/// place and the call the server died on still returns an answer.
#[test]
fn registry_rebuilds_a_stdio_connection_the_server_died_on() {
    let directory = TemporaryDirectory::new("mcp-reconnect");
    let marker = directory.path().join("crashed");
    let command = PathBuf::from(env!("CARGO_BIN_EXE_fake-mcp-child"));
    let project_root = std::env::current_dir().unwrap();

    let mut registry = agens_tools::McpRegistry::new();
    let status = registry.status_handle();
    registry
        .configure_server(
            "files",
            move || {
                McpStdioTransport::spawn(McpStdioTransportConfig {
                    command: command.clone(),
                    args: vec!["call-crash-once".into()],
                    environment: BTreeMap::from([(
                        "FAKE_MCP_CRASH_MARKER".to_owned(),
                        marker.to_string_lossy().into_owned(),
                    )]),
                    project_root: project_root.clone(),
                })
                .map(|transport| Box::new(transport) as Box<dyn McpTransport>)
            },
            McpTimeouts::new(
                Duration::from_secs(2),
                Duration::from_secs(2),
                Duration::from_secs(2),
            )
            .unwrap(),
            McpLimits::new(4, 4).unwrap(),
        )
        .unwrap();
    assert!(!registry.discover_server("files").is_failed());

    let output = registry
        .call_tool(
            "files::first",
            json!({}),
            &agens_tools::ToolExecutionContext::with_timeout(Duration::from_secs(4)),
        )
        .expect("the call must survive the server dying under it");

    assert_eq!(output.content, "tool succeeded");
    assert!(!output.is_error);
    // The status handle tracks the connection that actually exists now, rather
    // than reporting `Ready` on the strength of a process that is gone.
    let snapshot = status.snapshot();
    let files = snapshot.server("files").unwrap();
    assert_eq!(files.state(), agens_tools::McpLifecycleState::Ready);
    assert!(files.last_error().is_none());
}

/// The reconnect is not limited to the call that observed the death: a server
/// that exited between two calls is noticed before the next one is spent on it.
#[test]
fn registry_notices_a_stdio_server_that_died_between_calls() {
    let directory = TemporaryDirectory::new("mcp-liveness");
    let marker = directory.path().join("crashed");
    let command = PathBuf::from(env!("CARGO_BIN_EXE_fake-mcp-child"));
    let project_root = std::env::current_dir().unwrap();

    let mut registry = agens_tools::McpRegistry::new();
    registry
        .configure_server(
            "files",
            move || {
                McpStdioTransport::spawn(McpStdioTransportConfig {
                    command: command.clone(),
                    args: vec!["call-crash-once".into()],
                    environment: BTreeMap::from([(
                        "FAKE_MCP_CRASH_MARKER".to_owned(),
                        marker.to_string_lossy().into_owned(),
                    )]),
                    project_root: project_root.clone(),
                })
                .map(|transport| Box::new(transport) as Box<dyn McpTransport>)
            },
            McpTimeouts::new(
                Duration::from_secs(2),
                Duration::from_secs(2),
                Duration::from_secs(2),
            )
            .unwrap(),
            McpLimits::new(4, 4).unwrap(),
        )
        .unwrap();
    assert!(!registry.discover_server("files").is_failed());

    for _ in 0..3 {
        assert_eq!(
            registry
                .call_tool(
                    "files::first",
                    json!({}),
                    &agens_tools::ToolExecutionContext::with_timeout(Duration::from_secs(4)),
                )
                .unwrap()
                .content,
            "tool succeeded"
        );
    }
}

/// A reconnect a call triggers spends that call's budget: a connect timeout
/// plus one list timeout per page. It used to run under the handle the
/// registry holds for the life of the daemon, so neither the user's Esc nor
/// the tool deadline reached it.
#[test]
fn registry_reconnect_answers_to_the_cancellation_of_the_call_that_triggered_it() {
    let command = PathBuf::from(env!("CARGO_BIN_EXE_fake-mcp-child"));
    let project_root = std::env::current_dir().unwrap();
    let spawned = Arc::new(AtomicUsize::new(0));

    let mut registry = agens_tools::McpRegistry::new();
    registry
        .configure_server(
            "files",
            move || {
                // The first process answers discovery and dies on the call
                // that follows it; the one the reconnect starts holds
                // `initialize` far past every budget in this test.
                let mode = if spawned.fetch_add(1, Ordering::Relaxed) == 0 {
                    "call-crash"
                } else {
                    "sleep"
                };
                McpStdioTransport::spawn(McpStdioTransportConfig {
                    command: command.clone(),
                    args: vec![mode.into()],
                    environment: BTreeMap::new(),
                    project_root: project_root.clone(),
                })
                .map(|transport| Box::new(transport) as Box<dyn McpTransport>)
            },
            McpTimeouts::new(RECONNECT_BUDGET, RECONNECT_BUDGET, Duration::from_secs(2)).unwrap(),
            McpLimits::new(4, 4).unwrap(),
        )
        .unwrap();
    assert!(!registry.discover_server("files").is_failed());

    let cancellation = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&cancellation);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        signal.store(true, Ordering::Release);
    });

    let started = Instant::now();
    let result = registry.call_tool(
        "files::first",
        json!({}),
        &agens_tools::ToolExecutionContext::new(Arc::clone(&cancellation), OVER_ANY_FRAMING_COST),
    );

    assert!(result.is_err());
    let elapsed = started.elapsed();
    assert!(
        elapsed < RECONNECT_BUDGET / 2,
        "the reconnect ran to its own budget instead of stopping at the cancellation: {elapsed:?}"
    );
}

#[test]
fn stdio_transport_rejects_an_unterminated_oversized_stdout_frame() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut transport = transport("unterminated-oversize");
    let context = McpOperationContext::new(cancellation, OVER_ANY_FRAMING_COST);

    let result = transport.execute(
        McpRequest::CallTool {
            name: "x".into(),
            arguments: json!({}),
        },
        &context,
    );

    assert_eq!(
        result,
        Err(McpTransportError::Protocol(
            "MCP stdout frame exceeds limit".into()
        ))
    );
}

#[test]
fn stdio_transport_returns_promptly_when_a_child_does_not_read_stdin() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let (mut transport, _temporary) = no_read_transport();
    let context = McpOperationContext::new(Arc::clone(&cancellation), Duration::from_millis(100));
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let started = Instant::now();
        let result = transport.execute(
            McpRequest::CallTool {
                name: "x".repeat(512 * 1024),
                arguments: json!({}),
            },
            &context,
        );
        let _ = sender.send((started.elapsed(), result));
    });

    let (elapsed, result) = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("transport should not hang while the child never reads stdin");
    assert_eq!(result, Err(McpTransportError::TimedOut));
    assert!(
        elapsed < Duration::from_secs(2),
        "transport did not return promptly: {elapsed:?}"
    );
}

/// Bound on the fixture handshake that precedes the cancellation: the child has to
/// notice its stdin pipe filled and publish the marker file, and the watcher thread has
/// to poll for it.
///
/// Nothing here is the property under test, so this only has to be loud rather than
/// hanging. The quarter of a second it replaces is not survivable: that whole handshake
/// runs while the machine is also moving half a megabyte through a pipe, and on a loaded
/// core it routinely takes longer.
const CHILD_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline of the cancelled call.
///
/// It must be unreachable rather than merely generous. The handshake above runs inside
/// this window, so a deadline sized for a quiet machine ends the call on its own under
/// load and returns `TimedOut`, which is indistinguishable from the cancellation never
/// having been observed at all. Leaving cancellation as the only bound that can end the
/// call is what makes the assertion below mean what it says.
const UNREACHED_WRITE_DEADLINE: Duration = Duration::from_secs(30);

/// Ceiling on how long the blocked write may take to come back once cancelled.
///
/// This one is the property the test exists for and stays a real bound. It has to remain
/// far below [`UNREACHED_WRITE_DEADLINE`], because a call that returned only because its
/// deadline expired would otherwise pass as a prompt cancellation; the margin above the
/// cancellation itself is only there to absorb scheduler jitter.
const PROMPT_CANCELLATION_CEILING: Duration = Duration::from_secs(2);

#[test]
fn stdio_transport_cancels_an_observably_blocked_stdin_write_promptly() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let (mut transport, blocked_path, _temporary) = blocked_stdin_transport();
    let context = McpOperationContext::new(Arc::clone(&cancellation), UNREACHED_WRITE_DEADLINE);
    let (sender, receiver) = mpsc::sync_channel(1);
    let signal = Arc::clone(&cancellation);
    let (blocked_sender, blocked_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        wait_for_path(&blocked_path);
        signal.store(true, Ordering::Release);
        let _ = blocked_sender.send(());
    });
    thread::spawn(move || {
        let result = transport.execute(
            McpRequest::CallTool {
                name: "x".repeat(512 * 1024),
                arguments: json!({}),
            },
            &context,
        );
        let _ = sender.send(result);
    });

    assert_eq!(
        blocked_receiver.recv_timeout(CHILD_HANDSHAKE_TIMEOUT),
        Ok(()),
        "child must confirm the stdin pipe filled before cancellation"
    );
    assert_eq!(
        receiver.recv_timeout(PROMPT_CANCELLATION_CEILING),
        Ok(Err(McpTransportError::Cancelled))
    );
}

fn no_read_transport() -> (McpStdioTransport, TemporaryDirectory) {
    let temporary = TemporaryDirectory::new("no-read-stdin");
    let ready_path = temporary.path().join("ready");
    let transport = McpStdioTransport::spawn(McpStdioTransportConfig {
        command: PathBuf::from(env!("CARGO_BIN_EXE_fake-mcp-child")),
        args: vec!["no-read-stdin".into(), ready_path.display().to_string()],
        environment: BTreeMap::new(),
        project_root: std::env::current_dir().unwrap(),
    })
    .unwrap();
    wait_for_file_contents(&ready_path, "4096");

    (transport, temporary)
}

fn blocked_stdin_transport() -> (McpStdioTransport, PathBuf, TemporaryDirectory) {
    let temporary = TemporaryDirectory::new("blocked-stdin");
    let ready_path = temporary.path().join("ready");
    let blocked_path = temporary.path().join("blocked");
    let transport = McpStdioTransport::spawn(McpStdioTransportConfig {
        command: PathBuf::from(env!("CARGO_BIN_EXE_fake-mcp-child")),
        args: vec![
            "no-read-stdin".into(),
            ready_path.display().to_string(),
            blocked_path.display().to_string(),
        ],
        environment: BTreeMap::new(),
        project_root: std::env::current_dir().unwrap(),
    })
    .unwrap();
    wait_for_file_contents(&ready_path, "4096");

    (transport, blocked_path, temporary)
}

fn wait_for_path(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "child should signal readiness");
        thread::sleep(Duration::from_millis(2));
    }
}

fn wait_for_file_contents(path: &std::path::Path, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(path).is_ok_and(|content| content.trim() == expected) {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "child should publish the complete readiness signal"
        );
        thread::sleep(Duration::from_millis(2));
    }
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

#[cfg(unix)]
#[test]
fn stdio_transport_reaps_process_group_descendants_after_timeout_cancellation_and_crash() {
    for (mode, timeout, cancel) in [
        ("descendant-timeout", Duration::from_millis(20), false),
        ("descendant-cancel", Duration::from_secs(1), true),
        ("descendant-crash", Duration::from_secs(1), false),
    ] {
        let temporary = TemporaryDirectory::new(mode);
        let pid_path = temporary.path().join("descendant.pid");
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut client = McpClient::new(
            McpStdioTransport::spawn(McpStdioTransportConfig {
                command: PathBuf::from(env!("CARGO_BIN_EXE_fake-mcp-child")),
                args: vec![mode.into(), pid_path.display().to_string()],
                environment: BTreeMap::new(),
                project_root: std::env::current_dir().unwrap(),
            })
            .unwrap(),
            McpTimeouts::new(timeout, timeout, timeout).unwrap(),
            McpLimits::new(4, 4).unwrap(),
        );
        let descendant = wait_for_descendant(&pid_path);
        if cancel {
            let signal = Arc::clone(&cancellation);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(10));
                signal.store(true, Ordering::Release);
            });
        }

        let result = client.call_tool("x", json!({}), &cancellation);

        assert!(
            matches!(
                result,
                Err(McpTransportError::TimedOut)
                    | Err(McpTransportError::Cancelled)
                    | Err(McpTransportError::Transport(_))
            ),
            "{mode}: {result:?}"
        );
        if cancel {
            // Cancelling abandons the pending id, not the process group: the
            // server is still there for the next call, and its descendant
            // goes when the transport itself does.
            let mut transport = client.into_transport();
            assert!(transport.is_alive(), "{mode}");
            drop(transport);
        }
        assert_no_orphan(descendant, mode);
    }
}

#[cfg(unix)]
fn wait_for_descendant(path: &std::path::Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // The child records the PID with a plain write, which creates the file
        // before filling it, so an existing-but-empty read means "not yet"
        // rather than "malformed".
        if let Ok(pid) = std::fs::read_to_string(path)
            && let Ok(pid) = pid.trim().parse()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "descendant PID was not recorded");
        thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(unix)]
fn assert_no_orphan(pid: i32, mode: &str) {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let exists = unsafe { libc::kill(pid, 0) == 0 };
        if !exists {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{mode} left descendant {pid} running"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

/// Distinguishes directories created within the same process, where the pid is
/// shared and the clock can report the same nanosecond twice.
static NEXT_TEMPORARY_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    /// A private temporary directory keyed on the pid, a process-local sequence
    /// number and the wall clock.
    ///
    /// No two live processes share a pid and no two calls in one process share a
    /// sequence number, so concurrent runs cannot collide; the timestamp only
    /// separates a fresh directory from one a killed process left behind under a
    /// since-recycled pid. `create_dir` rather than `create_dir_all` keeps any
    /// residual collision loud instead of silently sharing state.
    fn new(name: &str) -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let sequence = NEXT_TEMPORARY_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agens-tools-{name}-{}-{sequence}-{timestamp}",
            std::process::id()
        ));

        std::fs::create_dir(&path).expect("temporary directory should be created");

        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
