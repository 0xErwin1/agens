use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, BufWriter, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use serde_json::{Value, json};

use crate::{
    McpCallResult, McpContentBlock, McpInitializeResult, McpOperationContext, McpProtocolError,
    McpRequest, McpResponse, McpToolAnnotations, McpToolDefinition, McpToolsPage, McpTransport,
    McpTransportError,
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(2);

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

struct Process {
    child: Child,
    stdout: BufReader<ChildStdout>,
}

struct WriteRequest {
    frame: Vec<u8>,
    response: mpsc::SyncSender<Result<(), McpTransportError>>,
}

pub struct McpStdioTransport {
    process: Arc<Mutex<Option<Process>>>,
    writer: mpsc::SyncSender<WriteRequest>,
    process_id: AtomicU32,
    next_id: AtomicU64,
}

impl McpStdioTransport {
    pub fn spawn(config: McpStdioTransportConfig) -> Result<Self, McpTransportError> {
        config.validate()?;
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .current_dir(&config.project_root)
            .env_clear()
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
            process: Arc::new(Mutex::new(Some(Process {
                child,
                stdout: BufReader::new(stdout),
            }))),
            writer,
            next_id: AtomicU64::new(1),
            process_id: AtomicU32::new(process_id),
        })
    }

    fn request(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<McpResponse, McpTransportError> {
        context.check()?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let wire = request_wire(request, Some(id));
        self.write_frame(wire, context)?;
        let process = Arc::clone(&self.process);
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = read_frame(process).and_then(|frame| parse_response(frame, id));
            let _ = sender.send(result);
        });
        loop {
            match receiver.recv_timeout(POLL_INTERVAL) {
                Ok(result) => match context.check() {
                    Ok(()) => {
                        if result.is_err() {
                            let _ = self.terminate();
                        }
                        return result;
                    }
                    Err(primary) => {
                        let _ = self.terminate();
                        return Err(primary);
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(primary) = context.check() {
                        let _ = self.terminate();
                        return Err(primary);
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

    fn write_frame(
        &self,
        value: Value,
        context: &McpOperationContext,
    ) -> Result<(), McpTransportError> {
        let encoded = serde_json::to_vec(&value)
            .map_err(|_| McpTransportError::Protocol("MCP request could not be encoded".into()))?;
        if encoded.len() > MAX_FRAME_BYTES {
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
        let mut process = self
            .process
            .lock()
            .map_err(|_| McpTransportError::Transport("MCP process lock is unavailable".into()))?;
        let Some(mut process) = process.take() else {
            return Ok(());
        };
        #[cfg(not(unix))]
        process
            .child
            .kill()
            .map_err(|_| McpTransportError::Transport("MCP process termination failed".into()))?;
        process
            .child
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
}

impl Drop for McpStdioTransport {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

fn request_wire(request: McpRequest, id: Option<u64>) -> Value {
    let (method, params) = match request {
        McpRequest::Initialize(value) => (
            "initialize",
            json!({"protocolVersion": value.protocol_version, "capabilities": value.capabilities, "clientInfo": {"name": value.client_info_name, "version": value.client_info_version}}),
        ),
        McpRequest::Initialized => ("notifications/initialized", json!({})),
        McpRequest::ListTools { cursor } => ("tools/list", json!({"cursor": cursor})),
        McpRequest::CallTool { name, arguments } => {
            ("tools/call", json!({"name": name, "arguments": arguments}))
        }
    };
    match id {
        Some(id) => json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}),
        None => json!({"jsonrpc":"2.0", "method":method, "params":params}),
    }
}

fn read_frame(process: Arc<Mutex<Option<Process>>>) -> Result<Value, McpTransportError> {
    let mut process = process
        .lock()
        .map_err(|_| McpTransportError::Transport("MCP process lock is unavailable".into()))?;
    let process = process
        .as_mut()
        .ok_or_else(|| McpTransportError::Transport("MCP process is closed".into()))?;
    let mut frame = Vec::with_capacity(MAX_FRAME_BYTES);
    let mut received = false;
    loop {
        let (count, complete) = {
            let buffer = process
                .stdout
                .fill_buf()
                .map_err(|_| McpTransportError::Transport("MCP stdout failed".into()))?;
            if buffer.is_empty() {
                break;
            }
            let count = buffer
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(buffer.len(), |position| position + 1);
            if frame.len() + count > MAX_FRAME_BYTES {
                return Err(McpTransportError::Protocol(
                    "MCP stdout frame exceeds limit".into(),
                ));
            }
            frame.extend_from_slice(&buffer[..count]);
            (count, buffer[count - 1] == b'\n')
        };
        process.stdout.consume(count);
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
                    let _ = transport.terminate();
                    return Err(primary);
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

fn parse_response(value: Value, expected_id: u64) -> Result<McpResponse, McpTransportError> {
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
    if result.get("protocolVersion").is_some() {
        return Ok(McpResponse::Initialized(McpInitializeResult::new(
            result
                .get("protocolVersion")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    McpTransportError::Protocol("MCP protocol version is invalid".into())
                })?,
            result.get("capabilities").cloned().ok_or_else(|| {
                McpTransportError::Protocol("MCP capabilities are missing".into())
            })?,
        )));
    }
    if let Some(tools) = result.get("tools").and_then(Value::as_array) {
        let tools = tools
            .iter()
            .map(parse_tool)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(McpResponse::ToolsListed(McpToolsPage::new(
            tools,
            result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        )));
    }
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        let content = content
            .iter()
            .map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| McpContentBlock::Text(text.into()))
                    .ok_or_else(|| {
                        McpTransportError::Protocol("MCP tool content is invalid".into())
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(McpResponse::ToolCalled(McpCallResult {
            content,
            is_error: result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }));
    }
    Err(McpTransportError::Protocol(
        "MCP result shape is unsupported".into(),
    ))
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
