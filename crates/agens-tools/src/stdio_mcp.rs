use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, BufWriter, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use agens_core::redaction::redact_exact_values;
use serde_json::{Value, json};

use crate::{
    McpCallResult, McpContentBlock, McpInitializeResult, McpOperationContext, McpProtocolError,
    McpRequest, McpResponse, McpToolAnnotations, McpToolDefinition, McpToolsPage, McpTransport,
    McpTransportError,
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Below this length an exact configured value is more likely to appear inside unrelated server
/// output than to identify the credential it came from.
const MIN_CONFIGURED_SECRET_CHARS: usize = 8;

/// The largest single JSON-RPC frame this client will hold in memory.
///
/// This is a memory ceiling, not the model's budget: a legitimate result can
/// be megabytes (a base64 screenshot, a whole file), and what actually reaches
/// the model is truncated later by `map_call_result`. A frame past this
/// ceiling is drained to its newline so the stream stays aligned, and the call
/// fails as a protocol irregularity without taking the server down with it.
pub const MAX_MCP_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// How many frames that are not the pending response this client will read
/// past before giving up on the pending one.
///
/// Notifications, server-initiated requests, and answers to requests this
/// client already abandoned are all legal traffic on the same pipe, so the
/// reader has to walk past them. The bound is what stops a server that only
/// ever emits notifications from holding the reader forever.
const MAX_INTERLEAVED_FRAMES: usize = 256;

/// How much of a server-supplied block type or media type is interpolated into
/// the description of a block agens cannot forward.
const MAX_CONTENT_DESCRIPTOR_CHARS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpStdioTransportConfig {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub project_root: PathBuf,
}

impl McpStdioTransportConfig {
    pub fn validate(&self) -> Result<(), McpTransportError> {
        if self.command.as_os_str().is_empty() || self.project_root.as_os_str().is_empty() {
            return Err(McpTransportError::Transport(
                "MCP command and project root are required".into(),
            ));
        }
        if self.args.iter().any(|arg| arg.contains('\0'))
            || self.environment.iter().any(|(key, value)| {
                key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0')
            })
        {
            return Err(McpTransportError::Transport(
                "MCP command arguments or environment are invalid".into(),
            ));
        }
        Ok(())
    }
}

struct WriteRequest {
    frame: Vec<u8>,
    response: mpsc::SyncSender<Result<(), McpTransportError>>,
}

/// Which result shape a response to a given request has to carry.
///
/// Without it the parser had to guess from the payload, so a `tools/call`
/// result that carried no `content` array — an empty answer, or one that only
/// set `structuredContent` — was indistinguishable from a shape this client
/// does not speak.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpResponseKind {
    Initialize,
    ToolsList,
    ToolCall,
}

impl McpResponseKind {
    pub(crate) fn of(request: &McpRequest) -> Option<Self> {
        match request {
            McpRequest::Initialize(_) => Some(Self::Initialize),
            McpRequest::Initialized => None,
            McpRequest::ListTools { .. } => Some(Self::ToolsList),
            McpRequest::CallTool { .. } => Some(Self::ToolCall),
        }
    }
}

/// What the reader does with a frame it just pulled off the pipe.
enum FrameRouting {
    /// The response to the pending request.
    Response,
    /// Legal traffic that is not that response, so the reader keeps going.
    Skipped,
    /// A frame no correct server sends on this pipe.
    Invalid,
}

/// The child process handle, held apart from the reader so that terminating
/// the server never waits on the thread blocked reading from it.
///
/// On unix `terminate` escapes through a signal to the process group, but the
/// portable path has to call `Child::kill`, and with one shared lock that call
/// could only run once the reader released it — which the reader does only
/// when the process it is waiting on produces output. Two locks break that
/// cycle: killing needs the child, reading needs the pipe, and neither waits
/// for the other.
pub struct McpStdioTransport {
    child: Arc<Mutex<Option<Child>>>,
    stdout: Arc<Mutex<BufReader<ChildStdout>>>,
    /// Set once a frame was abandoned part-read, which leaves the rest of it
    /// in the pipe with no way to tell where the next frame begins.
    ///
    /// This is the one irregularity that is not recoverable in place: every
    /// later frame would parse against the tail of the abandoned one. The
    /// caller that hit it still gets a protocol error rather than a dead
    /// server, and the connection is retired on the next request so the
    /// registry can rebuild it.
    desynchronized: Arc<AtomicBool>,
    writer: mpsc::SyncSender<WriteRequest>,
    process_id: AtomicU32,
    next_id: AtomicU64,
    configured_secret_values: Vec<String>,
}

impl McpStdioTransport {
    pub fn spawn(config: McpStdioTransportConfig) -> Result<Self, McpTransportError> {
        config.validate()?;
        let configured_secret_values = configured_secret_values(&config.environment);
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .current_dir(&config.project_root)
            // The child inherits the parent environment, and the configured entries
            // are layered on top. Clearing it first left the child without `PATH`, so
            // a command named rather than pathed — the portable way to declare one —
            // could never be resolved, and every stdio server failed to start.
            .envs(&config.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|_| McpTransportError::Transport("MCP process failed to start".into()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpTransportError::Transport("MCP stdin pipe is unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpTransportError::Transport("MCP stdout pipe is unavailable".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| McpTransportError::Transport("MCP stderr pipe is unavailable".into()))?;
        drain_stderr(stderr);
        let process_id = child.id();
        let (writer, requests) = mpsc::sync_channel(1);
        start_writer(stdin, requests);
        Ok(Self {
            child: Arc::new(Mutex::new(Some(child))),
            stdout: Arc::new(Mutex::new(BufReader::new(stdout))),
            desynchronized: Arc::new(AtomicBool::new(false)),
            writer,
            next_id: AtomicU64::new(1),
            process_id: AtomicU32::new(process_id),
            configured_secret_values,
        })
    }

    fn request(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<McpResponse, McpTransportError> {
        context.check()?;
        if self.desynchronized.load(Ordering::Acquire) {
            let _ = self.terminate();
            return Err(McpTransportError::Transport(
                "MCP stdout stream is unusable".into(),
            ));
        }
        let kind = McpResponseKind::of(&request).ok_or_else(|| {
            McpTransportError::Protocol("MCP notification cannot carry a response".into())
        })?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let wire = request_wire(request, Some(id));
        self.write_frame(wire, context)?;
        let stdout = Arc::clone(&self.stdout);
        let desynchronized = Arc::clone(&self.desynchronized);
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = read_matching_frame(&stdout, id, &desynchronized)
                .and_then(|frame| parse_response(frame, id, kind));
            let _ = sender.send(result);
        });
        loop {
            match receiver.recv_timeout(POLL_INTERVAL) {
                Ok(result) => match context.check() {
                    Ok(()) => {
                        // Only a failure of the pipe itself takes the server
                        // down. A protocol irregularity — an unparseable
                        // frame, an oversized one, a result shape this client
                        // does not speak — costs the caller one answer, and
                        // the connection stays usable for the next call.
                        if matches!(result, Err(McpTransportError::Transport(_))) {
                            let _ = self.terminate();
                        }
                        return result;
                    }
                    Err(primary) => return self.give_up(primary),
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(primary) = context.check() {
                        return self.give_up(primary);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(McpTransportError::Transport(
                        "MCP response worker stopped".into(),
                    ));
                }
            }
        }
    }

    /// Ends a request whose caller stopped waiting for it.
    ///
    /// Cancellation abandons the pending id and nothing else. The server is
    /// healthy and will answer, and `frame_routing` walks past every id below
    /// the pending one, so the next call reaches the same process instead of
    /// paying for a reconnect nobody needed.
    ///
    /// A deadline is different evidence: the server did not answer inside its
    /// budget. Abandoning that id would leave the reader holding the stdout
    /// lock for every later call while `is_alive` still reports a healthy
    /// server, so nothing would ever rebuild the connection. The process goes.
    fn give_up<T>(&self, primary: McpTransportError) -> Result<T, McpTransportError> {
        if matches!(primary, McpTransportError::TimedOut) {
            let _ = self.terminate();
        }
        Err(primary)
    }

    fn write_frame(
        &self,
        value: Value,
        context: &McpOperationContext,
    ) -> Result<(), McpTransportError> {
        let encoded = serde_json::to_vec(&value)
            .map_err(|_| McpTransportError::Protocol("MCP request could not be encoded".into()))?;
        if encoded.len() > MAX_MCP_FRAME_BYTES {
            return Err(McpTransportError::Protocol(
                "MCP request frame exceeds limit".into(),
            ));
        }
        context.check()?;
        let (sender, receiver) = mpsc::sync_channel(1);
        self.writer
            .send(WriteRequest {
                frame: encoded,
                response: sender,
            })
            .map_err(|_| McpTransportError::Transport("MCP process stdin is unavailable".into()))?;
        wait_for_write(receiver, context, self)
    }

    fn terminate(&self) -> Result<(), McpTransportError> {
        let process_id = self.process_id.swap(0, Ordering::AcqRel);
        if process_id != 0 {
            #[cfg(unix)]
            unsafe {
                if libc::kill(-(process_id as i32), libc::SIGKILL) != 0
                    && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                {
                    return Err(McpTransportError::Transport(
                        "MCP process termination failed".into(),
                    ));
                }
            }
        }
        let mut child = self
            .child
            .lock()
            .map_err(|_| McpTransportError::Transport("MCP process lock is unavailable".into()))?;
        let Some(mut child) = child.take() else {
            return Ok(());
        };
        #[cfg(not(unix))]
        child
            .kill()
            .map_err(|_| McpTransportError::Transport("MCP process termination failed".into()))?;
        child
            .wait()
            .map_err(|_| McpTransportError::Transport("MCP process reap failed".into()))?;
        Ok(())
    }
}

impl McpTransport for McpStdioTransport {
    fn execute(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<McpResponse, McpTransportError> {
        self.request(request, context)
            .map(|response| redact_configured_secrets(response, &self.configured_secret_values))
    }

    fn notify(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<(), McpTransportError> {
        context.check()?;
        self.write_frame(request_wire(request, None), context)
    }

    fn close(&mut self, _: &McpOperationContext) -> Result<(), McpTransportError> {
        self.terminate()
    }

    /// Answers from what this process already knows: whether the child is
    /// still running, and whether the stream it speaks over was retired.
    ///
    /// No request is sent, so the check costs nothing a caller has to wait
    /// for. A lock this process cannot take, or a child that has exited,
    /// both count as gone.
    fn is_alive(&mut self) -> bool {
        if self.desynchronized.load(Ordering::Acquire) {
            return false;
        }
        let Ok(mut child) = self.child.lock() else {
            return false;
        };
        child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }
}

impl Drop for McpStdioTransport {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

pub(crate) fn request_wire(request: McpRequest, id: Option<u64>) -> Value {
    let (method, params) = match request {
        McpRequest::Initialize(value) => (
            "initialize",
            json!({"protocolVersion": value.protocol_version, "capabilities": value.capabilities, "clientInfo": {"name": value.client_info_name, "version": value.client_info_version}}),
        ),
        McpRequest::Initialized => ("notifications/initialized", json!({})),
        McpRequest::ListTools { cursor } => {
            let params = cursor.map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));

            ("tools/list", params)
        }
        McpRequest::CallTool { name, arguments } => {
            ("tools/call", json!({"name": name, "arguments": arguments}))
        }
    };
    match id {
        Some(id) => json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}),
        None => json!({"jsonrpc":"2.0", "method":method, "params":params}),
    }
}

/// Reads frames until the response to `expected_id` arrives, walking past the
/// traffic the protocol allows a server to interleave with it.
///
/// Notifications (`notifications/message`, `notifications/tools/list_changed`,
/// progress), requests the server makes of this client, and answers to
/// requests this client already abandoned all share the one pipe. Treating any
/// of them as a failure was what let a single well-behaved notification kill a
/// server for the rest of the session.
fn read_matching_frame(
    stdout: &Arc<Mutex<BufReader<ChildStdout>>>,
    expected_id: u64,
    desynchronized: &AtomicBool,
) -> Result<Value, McpTransportError> {
    let mut stdout = stdout
        .lock()
        .map_err(|_| McpTransportError::Transport("MCP process lock is unavailable".into()))?;

    for _ in 0..MAX_INTERLEAVED_FRAMES {
        let frame = read_frame(&mut stdout, desynchronized)?;
        match frame_routing(&frame, expected_id) {
            FrameRouting::Response => return Ok(frame),
            FrameRouting::Skipped => {}
            FrameRouting::Invalid => {
                return Err(McpTransportError::Protocol(
                    "MCP response id does not match request".into(),
                ));
            }
        }
    }

    Err(McpTransportError::Protocol(
        "MCP server sent too many frames before responding".into(),
    ))
}

/// Decides whether a frame answers the pending request, precedes it, or is
/// something no correct server puts on this pipe.
///
/// This client keeps one request in flight and numbers them upwards, so an id
/// below the pending one belongs to a request already given up on and an id
/// above it was never sent. A frame carrying `method` is a notification or a
/// request from the server: its `id`, when it has one, lives in the server's
/// own numbering and never answers ours.
fn frame_routing(frame: &Value, expected_id: u64) -> FrameRouting {
    let Some(object) = frame.as_object() else {
        return FrameRouting::Invalid;
    };
    if object.contains_key("method") {
        return FrameRouting::Skipped;
    }
    match object.get("id") {
        None | Some(Value::Null) => FrameRouting::Skipped,
        Some(id) => match id.as_u64() {
            Some(id) if id == expected_id => FrameRouting::Response,
            Some(id) if id < expected_id => FrameRouting::Skipped,
            _ => FrameRouting::Invalid,
        },
    }
}

fn read_frame(
    stdout: &mut BufReader<ChildStdout>,
    desynchronized: &AtomicBool,
) -> Result<Value, McpTransportError> {
    let mut frame = Vec::new();
    let mut received = false;
    loop {
        let (count, complete) = {
            let buffer = stdout
                .fill_buf()
                .map_err(|_| McpTransportError::Transport("MCP stdout failed".into()))?;
            if buffer.is_empty() {
                break;
            }
            let count = buffer
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(buffer.len(), |position| position + 1);
            // Draining the rest would mean blocking on a frame this client has
            // already decided not to keep, and the frame may never terminate
            // at all. The remainder stays in the pipe and the stream is
            // retired instead.
            if frame.len() + count > MAX_MCP_FRAME_BYTES {
                desynchronized.store(true, Ordering::Release);
                return Err(McpTransportError::Protocol(
                    "MCP stdout frame exceeds limit".into(),
                ));
            }
            frame.extend_from_slice(&buffer[..count]);
            (count, buffer[count - 1] == b'\n')
        };
        stdout.consume(count);
        received = true;
        if complete {
            break;
        }
    }
    if !received {
        return Err(McpTransportError::Transport(
            "MCP process ended before a response".into(),
        ));
    }
    serde_json::from_slice(&frame)
        .map_err(|_| McpTransportError::Protocol("MCP stdout frame is malformed".into()))
}

fn start_writer(stdin: ChildStdin, requests: mpsc::Receiver<WriteRequest>) {
    thread::spawn(move || {
        let mut stdin = BufWriter::new(stdin);
        for request in requests {
            let result = stdin
                .write_all(&request.frame)
                .and_then(|_| stdin.write_all(b"\n"))
                .and_then(|_| stdin.flush())
                .map_err(|_| McpTransportError::Transport("MCP process stdin failed".into()));
            let _ = request.response.send(result);
        }
    });
}

fn wait_for_write(
    receiver: mpsc::Receiver<Result<(), McpTransportError>>,
    context: &McpOperationContext,
    transport: &McpStdioTransport,
) -> Result<(), McpTransportError> {
    loop {
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(result) => {
                if result.is_err() {
                    let _ = transport.terminate();
                }
                return result;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Err(primary) = context.check() {
                    return transport.give_up(primary);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(McpTransportError::Transport(
                    "MCP stdin worker stopped".into(),
                ));
            }
        }
    }
}

pub(crate) fn parse_response(
    value: Value,
    expected_id: u64,
    kind: McpResponseKind,
) -> Result<McpResponse, McpTransportError> {
    let object = value
        .as_object()
        .ok_or_else(|| McpTransportError::Protocol("MCP response must be an object".into()))?;
    if object.get("jsonrpc") != Some(&Value::String("2.0".into()))
        || object.get("id").and_then(Value::as_u64) != Some(expected_id)
    {
        return Err(McpTransportError::Protocol(
            "MCP response id does not match request".into(),
        ));
    }
    if let Some(error) = object.get("error").and_then(Value::as_object) {
        return Ok(McpResponse::ProtocolError(McpProtocolError::new(
            error
                .get("code")
                .and_then(Value::as_i64)
                .ok_or_else(|| McpTransportError::Protocol("MCP error code is invalid".into()))?,
            error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    McpTransportError::Protocol("MCP error message is invalid".into())
                })?,
        )));
    }
    let result = object
        .get("result")
        .ok_or_else(|| McpTransportError::Protocol("MCP response has no result".into()))?;

    match kind {
        McpResponseKind::Initialize => Ok(McpResponse::Initialized(McpInitializeResult::new(
            result
                .get("protocolVersion")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    McpTransportError::Protocol("MCP protocol version is invalid".into())
                })?,
            result.get("capabilities").cloned().ok_or_else(|| {
                McpTransportError::Protocol("MCP capabilities are missing".into())
            })?,
        ))),
        McpResponseKind::ToolsList => {
            let tools = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| McpTransportError::Protocol("MCP tools are missing".into()))?
                .iter()
                .map(parse_tool)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(McpResponse::ToolsListed(McpToolsPage::new(
                tools,
                result
                    .get("nextCursor")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            )))
        }
        McpResponseKind::ToolCall => Ok(McpResponse::ToolCalled(parse_call_result(result))),
    }
}

/// Reads a `tools/call` result, keeping whatever of it a text-only tool output
/// can carry.
///
/// Every departure from a plain `content` array of text blocks used to fail
/// the call as a protocol error, which on stdio then killed the server: an
/// image block, an audio block, an embedded binary resource, a result that
/// only sets `structuredContent`, or an empty answer. None of those are
/// irregular — they are the protocol — so each one now yields the best text
/// available for it.
fn parse_call_result(result: &Value) -> McpCallResult {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if let Some(blocks) = result.get("content").and_then(Value::as_array)
        && !blocks.is_empty()
    {
        return McpCallResult {
            content: blocks.iter().map(parse_content_block).collect(),
            is_error,
        };
    }

    if let Some(structured) = result.get("structuredContent") {
        return McpCallResult {
            content: vec![McpContentBlock::Text(structured.to_string())],
            is_error,
        };
    }

    McpCallResult {
        content: Vec::new(),
        is_error,
    }
}

/// Projects one content block onto the text a tool output can hold.
///
/// A `resource` block carries its payload one level down, and an embedded text
/// resource is as usable as a top-level text block. Anything left has no text
/// at all, so what survives is an agens-authored line naming what came back.
fn parse_content_block(block: &Value) -> McpContentBlock {
    if let Some(text) = block.get("text").and_then(Value::as_str) {
        return McpContentBlock::Text(text.to_owned());
    }

    let resource = block.get("resource");
    if let Some(text) = resource
        .and_then(|resource| resource.get("text"))
        .and_then(Value::as_str)
    {
        return McpContentBlock::Text(text.to_owned());
    }

    let block_type = block.get("type").and_then(Value::as_str);
    let media_type = block.get("mimeType").and_then(Value::as_str).or_else(|| {
        resource
            .and_then(|resource| resource.get("mimeType"))
            .and_then(Value::as_str)
    });

    McpContentBlock::NonText(non_text_description(block_type, media_type))
}

/// Renders a block agens cannot forward as a bounded, agens-authored line.
///
/// The block type and media type come from the server, so both are reduced to
/// visible ASCII and cut short before they are interpolated: the description
/// exists to tell the model what came back, not to become a channel for
/// arbitrary remote text.
fn non_text_description(block_type: Option<&str>, media_type: Option<&str>) -> String {
    let block_type = sanitized_descriptor(block_type).unwrap_or_else(|| "unknown".to_owned());
    match sanitized_descriptor(media_type) {
        Some(media_type) => format!("[mcp {block_type} content: {media_type}]"),
        None => format!("[mcp {block_type} content]"),
    }
}

fn sanitized_descriptor(value: Option<&str>) -> Option<String> {
    let value = value?
        .chars()
        .filter(|character| character.is_ascii_graphic())
        .take(MAX_CONTENT_DESCRIPTOR_CHARS)
        .collect::<String>();
    (!value.is_empty()).then_some(value)
}

/// The configured transport environment entries this transport treats as secrets.
///
/// A configured environment is mostly operational settings, not credentials: `example/config.toml`
/// alone sets `LANG = "C"`. Since [`redact_exact_values`] replaces a match wherever it appears,
/// with no surrounding context, treating every configured value as a secret rewrites unrelated
/// output — every capital `C` in every result from that server. An entry qualifies only when its
/// NAME is a credential key and its value is long enough that an exact match identifies the secret
/// rather than colliding with ordinary text.
fn configured_secret_values(environment: &BTreeMap<String, String>) -> Vec<String> {
    environment
        .iter()
        .filter(|(name, value)| {
            agens_core::redaction::is_credential_key(name)
                && value.chars().count() >= MIN_CONFIGURED_SECRET_CHARS
        })
        .map(|(_, value)| value.clone())
        .collect()
}

/// Withholds this server's own configured transport secrets from a tool call's text before the
/// caller can pass it on as `HeadlessToolOutput`. `map_call_result` otherwise forwards the
/// server's text verbatim, and a server can echo back exactly the values this process handed it
/// in `[mcp.files.env]` — the only secrets this transport can know about without guessing at
/// shape.
///
/// Successful and `isError` results are treated alike: both reach the model and the persisted
/// session, so a credential echoed into either is the same leak. Only [`configured_secret_values`]
/// decides what counts as a secret, and text carrying none is returned unchanged. Every other
/// response variant is returned unchanged.
fn redact_configured_secrets(response: McpResponse, secrets: &[String]) -> McpResponse {
    if secrets.is_empty() {
        return response;
    }
    let McpResponse::ToolCalled(result) = response else {
        return response;
    };
    let content = result
        .content
        .into_iter()
        .map(|block| match block {
            McpContentBlock::Text(text) => {
                McpContentBlock::Text(redact_exact_values(&text, secrets))
            }
            // Authored here from a sanitized block type and media type, so it
            // carries no server text a configured secret could be echoed in.
            block @ McpContentBlock::NonText(_) => block,
        })
        .collect();
    McpResponse::ToolCalled(McpCallResult {
        content,
        is_error: result.is_error,
    })
}

fn parse_tool(value: &Value) -> Result<McpToolDefinition, McpTransportError> {
    Ok(McpToolDefinition {
        name: value
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| McpTransportError::Protocol("MCP tool name is invalid".into()))?
            .into(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        input_schema: value
            .get("inputSchema")
            .cloned()
            .ok_or_else(|| McpTransportError::Protocol("MCP tool inputSchema is missing".into()))?,
        annotations: McpToolAnnotations {
            read_only_hint: value
                .get("annotations")
                .and_then(|annotations| annotations.get("readOnlyHint"))
                .and_then(Value::as_bool),
        },
    })
}

fn drain_stderr(mut stderr: impl std::io::Read + Send + 'static) {
    thread::spawn(move || {
        let mut remaining = MAX_STDERR_BYTES;
        let mut buffer = [0; 4096];
        loop {
            let count = buffer.len().min(remaining.max(1));
            match stderr.read(&mut buffer[..count]) {
                Ok(0) | Err(_) => return,
                Ok(count) => remaining = remaining.saturating_sub(count),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_list_omits_an_absent_cursor_instead_of_sending_null() {
        let absent = request_wire(McpRequest::ListTools { cursor: None }, Some(1));
        assert_eq!(absent["params"], json!({}));

        let present = request_wire(
            McpRequest::ListTools {
                cursor: Some("next".into()),
            },
            Some(2),
        );
        assert_eq!(present["params"], json!({"cursor": "next"}));
    }

    /// The server's own configured environment values are the only secrets this transport can
    /// know about, so a tool call whose `isError` text echoes one back is redacted by that exact
    /// value before the caller ever sees it — no shape detection involved.
    #[test]
    fn tool_called_responses_redact_the_servers_own_configured_secret_values() {
        let secrets = vec!["CONFIGURED_TRANSPORT_SECRET".to_owned()];
        let response = McpResponse::ToolCalled(McpCallResult {
            content: vec![McpContentBlock::Text(
                "server rejected the call: CONFIGURED_TRANSPORT_SECRET was invalid".into(),
            )],
            is_error: true,
        });

        let redacted = redact_configured_secrets(response, &secrets);

        let McpResponse::ToolCalled(result) = redacted else {
            panic!("expected a ToolCalled response");
        };
        assert!(result.is_error);
        let McpContentBlock::Text(text) = &result.content[0] else {
            panic!("expected a text content block");
        };
        assert!(!text.contains("CONFIGURED_TRANSPORT_SECRET"));
        assert!(text.starts_with("server rejected the call: [redacted:"));
        assert!(text.ends_with("was invalid"));
    }

    #[test]
    fn tool_called_responses_are_unchanged_when_no_secrets_are_configured() {
        let response = McpResponse::ToolCalled(McpCallResult {
            content: vec![McpContentBlock::Text("no secrets here".into())],
            is_error: false,
        });

        let redacted = redact_configured_secrets(response.clone(), &[]);

        assert_eq!(redacted, response);
    }

    /// The values in `example/config.toml`'s own `[mcp.filesystem.env]` block, plus the other
    /// short operational settings a server is routinely configured with. Treating these as
    /// secrets turns every capital `C` — or every `1`, or every `UTC` — in a server's output
    /// into a withheld marker.
    #[test]
    fn benign_configured_environment_values_are_never_collected_as_secrets() {
        let environment = BTreeMap::from([
            ("LANG".to_owned(), "C".to_owned()),
            ("DEBUG".to_owned(), "1".to_owned()),
            ("TZ".to_owned(), "UTC".to_owned()),
            ("NODE_ENV".to_owned(), "production".to_owned()),
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
        ]);

        assert_eq!(configured_secret_values(&environment), Vec::<String>::new());

        let response = McpResponse::ToolCalled(McpCallResult {
            content: vec![McpContentBlock::Text(
                "Cannot open Config.toml in production: 1 error".into(),
            )],
            is_error: true,
        });

        assert_eq!(
            redact_configured_secrets(response.clone(), &configured_secret_values(&environment)),
            response
        );
    }

    /// A short configured credential is still not matched by exact replacement: a two- or
    /// three-character value collides with unrelated output far more often than it identifies
    /// the secret, and the marker would then claim a redaction that never happened.
    #[test]
    fn only_long_credential_keyed_environment_values_are_collected_as_secrets() {
        let environment = BTreeMap::from([
            ("GITHUB_TOKEN".to_owned(), "ghp_abcdefghijklmnop".to_owned()),
            ("API_KEY".to_owned(), "abc".to_owned()),
            ("TOKENIZER".to_owned(), "a-long-benign-setting".to_owned()),
        ]);

        assert_eq!(
            configured_secret_values(&environment),
            vec!["ghp_abcdefghijklmnop".to_owned()]
        );
    }

    /// The whole point of collecting the transport's own environment: a server that echoes a
    /// configured credential back in its failure text must not hand it to the model.
    #[test]
    fn a_configured_credential_value_never_survives_a_failed_tool_call() {
        let environment = BTreeMap::from([(
            "MCP_API_KEY".to_owned(),
            "SENTINEL_CONFIGURED_TRANSPORT_SECRET".to_owned(),
        )]);
        let response = McpResponse::ToolCalled(McpCallResult {
            content: vec![McpContentBlock::Text(
                "upstream rejected SENTINEL_CONFIGURED_TRANSPORT_SECRET".into(),
            )],
            is_error: true,
        });

        let redacted = redact_configured_secrets(response, &configured_secret_values(&environment));

        let McpResponse::ToolCalled(result) = redacted else {
            panic!("expected a ToolCalled response");
        };
        let McpContentBlock::Text(text) = &result.content[0] else {
            panic!("expected a text content block");
        };
        assert_eq!(text, "upstream rejected [redacted: 36 characters]");
    }

    /// Content that carries no configured secret is never rewritten, so a server's answer
    /// survives byte for byte.
    #[test]
    fn tool_call_content_without_a_configured_secret_is_returned_unchanged() {
        let secrets = vec!["CONFIGURED_TRANSPORT_SECRET".to_owned()];

        for is_error in [false, true] {
            let response = McpResponse::ToolCalled(McpCallResult {
                content: vec![McpContentBlock::Text("the answer is 42".into())],
                is_error,
            });

            assert_eq!(
                redact_configured_secrets(response.clone(), &secrets),
                response
            );
        }
    }

    /// A server echoes a configured credential into a SUCCESSFUL result exactly as it does into
    /// a failure, and both reach the model and the persisted session, so both are withheld.
    #[test]
    fn a_configured_credential_value_never_survives_a_successful_tool_call() {
        let environment = BTreeMap::from([(
            "MCP_API_KEY".to_owned(),
            "SENTINEL_CONFIGURED_TRANSPORT_SECRET".to_owned(),
        )]);
        let response = McpResponse::ToolCalled(McpCallResult {
            content: vec![McpContentBlock::Text(
                "resolved SENTINEL_CONFIGURED_TRANSPORT_SECRET".into(),
            )],
            is_error: false,
        });

        let redacted = redact_configured_secrets(response, &configured_secret_values(&environment));

        let McpResponse::ToolCalled(result) = redacted else {
            panic!("expected a ToolCalled response");
        };
        assert!(!result.is_error);
        let McpContentBlock::Text(text) = &result.content[0] else {
            panic!("expected a text content block");
        };
        assert_eq!(text, "resolved [redacted: 36 characters]");
    }

    #[test]
    fn non_tool_called_responses_are_unaffected() {
        let response = McpResponse::ProtocolError(McpProtocolError::new(
            -32000,
            "CONFIGURED_TRANSPORT_SECRET failed",
        ));
        let secrets = vec!["CONFIGURED_TRANSPORT_SECRET".to_owned()];

        let redacted = redact_configured_secrets(response.clone(), &secrets);

        assert_eq!(redacted, response);
    }
}
