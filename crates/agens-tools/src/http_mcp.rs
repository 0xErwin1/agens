use std::collections::BTreeMap;

use reqwest::{
    Client,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use serde_json::Value;

use crate::http_worker::{
    HttpRequest, HttpResponse, HttpWorker, HttpWorkerError, HttpWorkerFuture, HttpWorkerOperation,
};
use crate::stdio_mcp::{MAX_MCP_FRAME_BYTES, McpResponseKind, parse_response, request_wire};
use crate::{McpOperationContext, McpRequest, McpResponse, McpTransport, McpTransportError};

/// The largest response body this transport will hold in memory, on the same
/// ceiling the stdio transport reads a frame against. What actually reaches
/// the model is truncated later by `map_call_result`.
const MAX_HTTP_BODY_BYTES: usize = MAX_MCP_FRAME_BYTES;
const HTTP_WORKER_CAPACITY: usize = 8;
const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
const MAX_MCP_SESSION_ID_BYTES: usize = 512;

/// JSON-RPC MCP transport that executes requests on an owned async HTTP worker.
pub struct McpHttpTransport {
    max_retries: u32,
    next_id: u64,
    worker: HttpWorker,
    endpoint: String,
    session_id: Option<String>,
}

impl McpHttpTransport {
    pub fn new(
        endpoint: String,
        headers: BTreeMap<String, String>,
        max_retries: u32,
    ) -> Result<Self, McpTransportError> {
        if max_retries > 8 {
            return Err(McpTransportError::Transport(
                "MCP retries are invalid".into(),
            ));
        }
        let endpoint = reqwest::Url::parse(&endpoint)
            .map_err(|_| McpTransportError::Transport("MCP endpoint is invalid".into()))?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host().is_none() {
            return Err(McpTransportError::Transport(
                "MCP endpoint is invalid".into(),
            ));
        }
        let endpoint = endpoint.to_string();
        let mut parsed_headers = HeaderMap::new();
        for (name, value) in &headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| McpTransportError::Transport("MCP headers are invalid".into()))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| McpTransportError::Transport("MCP headers are invalid".into()))?;
            parsed_headers.insert(name, value);
        }
        let worker = HttpWorker::start(
            HTTP_WORKER_CAPACITY,
            McpHttpOperation {
                client: None,
                headers: parsed_headers,
            },
        )
        .map_err(worker_error)?;
        Ok(Self {
            max_retries,
            next_id: 1,
            worker,
            endpoint,
            session_id: None,
        })
    }

    fn send(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
        notify: bool,
    ) -> Result<Option<McpResponse>, McpTransportError> {
        context.check()?;
        let kind = McpResponseKind::of(&request);
        let id = (!notify).then(|| {
            let id = self.next_id;
            self.next_id += 1;
            id
        });
        let body = serde_json::to_vec(&request_wire(request, id))
            .map_err(|_| McpTransportError::Protocol("MCP request is malformed".into()))?;
        let mut request_headers = BTreeMap::new();
        if let Some(session_id) = &self.session_id {
            request_headers.insert(MCP_SESSION_ID_HEADER.to_owned(), session_id.clone());
        }
        let attempts = self.max_retries + 1;
        for attempt in 0..attempts {
            let response = self.worker.request(
                HttpRequest {
                    method: "POST".into(),
                    endpoint: self.endpoint.clone(),
                    headers: request_headers.clone(),
                    body: body.clone(),
                },
                context.cancellation_probe(),
                context.deadline(),
            );
            let response = match response {
                Ok(response) => response,
                Err(HttpWorkerError::Transport) if attempt + 1 < attempts => continue,
                Err(HttpWorkerError::Transport) => return Err(McpTransportError::RetriesExhausted),
                Err(HttpWorkerError::ResponseTooLarge) => {
                    return Err(McpTransportError::Protocol(
                        "MCP HTTP response exceeds limit".into(),
                    ));
                }
                Err(error) => return Err(worker_error(error)),
            };
            if response.status == 408 || response.status == 429 || response.status >= 500 {
                if attempt + 1 < attempts {
                    continue;
                }
                return Err(McpTransportError::RetriesExhausted);
            }
            if (300..400).contains(&response.status) {
                return Err(McpTransportError::Transport(
                    "MCP HTTP redirect refused".into(),
                ));
            }
            if !(200..300).contains(&response.status) {
                return Err(McpTransportError::HttpStatus(response.status));
            }
            if let Some(session_id) = captured_session_id(&response.headers) {
                self.session_id = Some(session_id);
            }
            if notify {
                return Ok(None);
            }
            let id = id.expect("requests have identifiers");
            let kind = kind.expect("requests expect a response");
            return parse_response(parse_body(&response.body, id)?, id, kind).map(Some);
        }
        Err(McpTransportError::RetriesExhausted)
    }
}

/// Validates a captured `Mcp-Session-Id` header value before it is trusted.
///
/// The value is server-controlled, so it is accepted only if it is visible
/// ASCII and within a bounded length; anything else is silently ignored
/// rather than echoed back or rendered anywhere.
fn captured_session_id(headers: &BTreeMap<String, String>) -> Option<String> {
    let value = headers.get(MCP_SESSION_ID_HEADER)?;
    let is_valid = !value.is_empty()
        && value.len() <= MAX_MCP_SESSION_ID_BYTES
        && value.chars().all(|character| character.is_ascii_graphic());
    is_valid.then(|| value.clone())
}

impl McpTransport for McpHttpTransport {
    fn execute(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<McpResponse, McpTransportError> {
        self.send(request, context, false)?
            .ok_or_else(|| McpTransportError::Transport("MCP HTTP response is unavailable".into()))
    }

    fn notify(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<(), McpTransportError> {
        self.send(request, context, true).map(|_| ())
    }

    fn close(&mut self, _: &McpOperationContext) -> Result<(), McpTransportError> {
        self.worker.close().map_err(worker_error)
    }
}

pub struct McpSseTransport {
    max_retries: u32,
    next_id: u64,
    worker: HttpWorker,
    endpoint: String,
}

impl McpSseTransport {
    pub fn new(
        endpoint: String,
        headers: BTreeMap<String, String>,
        max_retries: u32,
    ) -> Result<Self, McpTransportError> {
        if max_retries > 8 {
            return Err(McpTransportError::Transport(
                "MCP retries are invalid".into(),
            ));
        }
        let endpoint = reqwest::Url::parse(&endpoint)
            .map_err(|_| McpTransportError::Transport("MCP endpoint is invalid".into()))?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host().is_none() {
            return Err(McpTransportError::Transport(
                "MCP endpoint is invalid".into(),
            ));
        }
        let endpoint = endpoint.to_string();
        let mut parsed_headers = HeaderMap::new();
        for (name, value) in &headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| McpTransportError::Transport("MCP headers are invalid".into()))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| McpTransportError::Transport("MCP headers are invalid".into()))?;
            parsed_headers.insert(name, value);
        }
        let worker = HttpWorker::start(
            HTTP_WORKER_CAPACITY,
            McpSseOperation {
                client: None,
                headers: parsed_headers,
            },
        )
        .map_err(worker_error)?;
        Ok(Self {
            max_retries,
            next_id: 1,
            worker,
            endpoint,
        })
    }

    fn send(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
        notify: bool,
    ) -> Result<Option<McpResponse>, McpTransportError> {
        context.check()?;
        let kind = McpResponseKind::of(&request);
        let id = (!notify).then(|| {
            let id = self.next_id;
            self.next_id += 1;
            id
        });
        let body = serde_json::to_vec(&request_wire(request, id))
            .map_err(|_| McpTransportError::Protocol("MCP request is malformed".into()))?;
        let attempts = self.max_retries + 1;
        for attempt in 0..attempts {
            let response = self.worker.request(
                HttpRequest {
                    method: "GET".into(),
                    endpoint: self.endpoint.clone(),
                    headers: BTreeMap::new(),
                    body: body.clone(),
                },
                context.cancellation_probe(),
                context.deadline(),
            );
            let response = match response {
                Ok(response) => response,
                Err(HttpWorkerError::Transport) if attempt + 1 < attempts => continue,
                Err(HttpWorkerError::Transport) => return Err(McpTransportError::RetriesExhausted),
                Err(HttpWorkerError::ResponseTooLarge) => {
                    return Err(McpTransportError::Protocol(
                        "MCP SSE event exceeds limit".into(),
                    ));
                }
                Err(error) => return Err(worker_error(error)),
            };
            if response.status == 408 || response.status == 429 || response.status >= 500 {
                if attempt + 1 < attempts {
                    continue;
                }
                return Err(McpTransportError::RetriesExhausted);
            }
            if (300..400).contains(&response.status) {
                return Err(McpTransportError::Transport(
                    "MCP HTTP redirect refused".into(),
                ));
            }
            if !(200..300).contains(&response.status) {
                return Err(McpTransportError::HttpStatus(response.status));
            }
            if notify {
                return Ok(None);
            }
            let id = id.expect("requests have identifiers");
            let kind = kind.expect("requests expect a response");
            return parse_response(parse_body(&response.body, id)?, id, kind).map(Some);
        }
        Err(McpTransportError::RetriesExhausted)
    }
}
impl McpTransport for McpSseTransport {
    fn execute(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<McpResponse, McpTransportError> {
        self.send(request, context, false)?
            .ok_or_else(|| McpTransportError::Transport("MCP SSE response is unavailable".into()))
    }

    fn notify(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<(), McpTransportError> {
        self.send(request, context, true).map(|_| ())
    }

    fn close(&mut self, _: &McpOperationContext) -> Result<(), McpTransportError> {
        self.worker.close().map_err(worker_error)
    }
}

struct McpHttpOperation {
    client: Option<Client>,
    headers: HeaderMap,
}

impl HttpWorkerOperation for McpHttpOperation {
    fn start(&mut self) -> Result<(), HttpWorkerError> {
        self.client = Some(
            Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .default_headers(self.headers.clone())
                .build()
                .map_err(|_| HttpWorkerError::Transport)?,
        );
        Ok(())
    }

    fn execute(&mut self, request: HttpRequest) -> HttpWorkerFuture {
        let client = self
            .client
            .as_ref()
            .expect("HTTP worker starts before requests")
            .clone();
        Box::pin(async move {
            let mut builder = client
                .request(
                    request
                        .method
                        .parse()
                        .map_err(|_| HttpWorkerError::Transport)?,
                    request.endpoint,
                )
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(ACCEPT, "application/json, text/event-stream");
            for (name, value) in &request.headers {
                let name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| HttpWorkerError::Transport)?;
                let value = HeaderValue::from_str(value).map_err(|_| HttpWorkerError::Transport)?;
                builder = builder.header(name, value);
            }
            let response = builder
                .body(request.body)
                .send()
                .await
                .map_err(|_| HttpWorkerError::Transport)?;
            let status = response.status().as_u16();
            let headers = response_headers(response.headers());
            let mut response = response;
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| HttpWorkerError::Transport)?
            {
                if body.len().saturating_add(chunk.len()) > MAX_HTTP_BODY_BYTES {
                    return Err(HttpWorkerError::ResponseTooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        })
    }

    fn close(&mut self) {
        self.client = None;
    }
}

/// Lower-cases header names for observability, dropping any value that is not
/// visible ASCII rather than lossily reinterpreting it.
fn response_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
        })
        .collect()
}

struct McpSseOperation {
    client: Option<Client>,
    headers: HeaderMap,
}

impl HttpWorkerOperation for McpSseOperation {
    fn start(&mut self) -> Result<(), HttpWorkerError> {
        self.client = Some(
            Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .default_headers(self.headers.clone())
                .build()
                .map_err(|_| HttpWorkerError::Transport)?,
        );
        Ok(())
    }

    fn execute(&mut self, request: HttpRequest) -> HttpWorkerFuture {
        let client = self
            .client
            .as_ref()
            .expect("HTTP worker starts before requests")
            .clone();
        Box::pin(async move {
            let event_url =
                reqwest::Url::parse(&request.endpoint).map_err(|_| HttpWorkerError::Transport)?;
            let response = client
                .get(event_url.clone())
                .header(ACCEPT, "text/event-stream")
                .send()
                .await
                .map_err(|_| HttpWorkerError::Transport)?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status)
                || !response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                return Ok(HttpResponse {
                    status,
                    headers: BTreeMap::new(),
                    body: Vec::new(),
                });
            }

            let mut response = response;
            let mut frame = SseFrame::default();
            let mut buffer = Vec::new();
            // Bytes at the front of `buffer` already known to hold no newline.
            // Rescanning the whole buffer per chunk made a long unterminated
            // line cost time quadratic in its length, which at the frame
            // ceiling is minutes rather than the prompt rejection intended.
            let mut scanned = 0;
            let mut message_endpoint = None;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| HttpWorkerError::Transport)?
            {
                buffer.extend_from_slice(&chunk);
                while let Some(offset) = buffer[scanned..].iter().position(|byte| *byte == b'\n') {
                    let mut line = buffer.drain(..=scanned + offset).collect::<Vec<_>>();
                    scanned = 0;
                    if line.ends_with(b"\n") {
                        line.pop();
                    }
                    if line.ends_with(b"\r") {
                        line.pop();
                    }
                    if line.is_empty() {
                        if let Some(response) = frame
                            .finish(&client, &event_url, &request.body, &mut message_endpoint)
                            .await?
                        {
                            return Ok(response);
                        }
                        continue;
                    }
                    if line.starts_with(b":") {
                        continue;
                    }
                    let line = std::str::from_utf8(&line).map_err(|_| HttpWorkerError::Protocol)?;
                    frame.push(line);
                    if frame.bytes > MAX_HTTP_BODY_BYTES {
                        return Err(HttpWorkerError::ResponseTooLarge);
                    }
                }
                scanned = buffer.len();
                if pending_line_data_bytes(&buffer) > MAX_HTTP_BODY_BYTES {
                    return Err(HttpWorkerError::ResponseTooLarge);
                }
            }
            Ok(HttpResponse {
                status,
                headers: BTreeMap::new(),
                body: Vec::new(),
            })
        })
    }

    fn close(&mut self) {
        self.client = None;
    }
}

/// Bytes of a pending, unterminated SSE line that would count toward the frame
/// limit once the line completes. `buffer` holds at most one such line, because
/// every terminated line is drained before this is consulted. A line that cannot
/// contribute data is charged in full so an unterminated field of any other name
/// stays bounded.
fn pending_line_data_bytes(buffer: &[u8]) -> usize {
    let line = buffer.strip_suffix(b"\r").unwrap_or(buffer);
    match line.strip_prefix(b"data:") {
        Some(rest) => rest.strip_prefix(b" ").unwrap_or(rest).len(),
        None => line.len(),
    }
}

#[derive(Default)]
struct SseFrame {
    bytes: usize,
    event: Option<String>,
    data: Vec<String>,
}

impl SseFrame {
    fn push(&mut self, line: &str) {
        if let Some(value) = line.strip_prefix("event:") {
            self.event = Some(value.trim_start().into());
        } else if let Some(value) = line.strip_prefix("data:") {
            let value = value.strip_prefix(' ').unwrap_or(value);
            self.bytes += value.len();
            self.data.push(value.into());
        }
    }

    async fn finish(
        &mut self,
        client: &Client,
        event_url: &reqwest::Url,
        body: &[u8],
        message_endpoint: &mut Option<reqwest::Url>,
    ) -> Result<Option<HttpResponse>, HttpWorkerError> {
        let event = self.event.take();
        let data = std::mem::take(&mut self.data).join("\n");
        self.bytes = 0;
        match event.as_deref() {
            Some("endpoint") => {
                let endpoint = event_url
                    .join(&data)
                    .ok()
                    .filter(|endpoint| endpoint.origin() == event_url.origin())
                    .ok_or(HttpWorkerError::Protocol)?;
                let status = client
                    .post(endpoint.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .body(body.to_vec())
                    .send()
                    .await
                    .map_err(|_| HttpWorkerError::Transport)?
                    .status()
                    .as_u16();
                if !(200..300).contains(&status) {
                    return Ok(Some(HttpResponse {
                        status,
                        headers: BTreeMap::new(),
                        body: Vec::new(),
                    }));
                }
                if !serde_json::from_slice::<Value>(body)
                    .ok()
                    .is_some_and(|request| request.get("id").is_some())
                {
                    return Ok(Some(HttpResponse {
                        status,
                        headers: BTreeMap::new(),
                        body: Vec::new(),
                    }));
                }
                *message_endpoint = Some(endpoint);
                Ok(None)
            }
            Some("message") if message_endpoint.is_some() => Ok(Some(HttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: data.into_bytes(),
            })),
            Some(_) if !data.is_empty() => Err(HttpWorkerError::Protocol),
            _ => Ok(None),
        }
    }
}

fn worker_error(error: HttpWorkerError) -> McpTransportError {
    match error {
        HttpWorkerError::Cancelled => McpTransportError::Cancelled,
        HttpWorkerError::TimedOut => McpTransportError::TimedOut,
        HttpWorkerError::Transport => {
            McpTransportError::Transport("MCP HTTP request failed".into())
        }
        HttpWorkerError::Protocol => {
            McpTransportError::Protocol("MCP HTTP response is malformed".into())
        }
        HttpWorkerError::ResponseTooLarge => {
            McpTransportError::Protocol("MCP HTTP response exceeds limit".into())
        }
        HttpWorkerError::Busy => McpTransportError::Transport("MCP HTTP worker is busy".into()),
        HttpWorkerError::Startup | HttpWorkerError::Panicked | HttpWorkerError::Shutdown => {
            McpTransportError::Transport("MCP HTTP worker is unavailable".into())
        }
    }
}

/// Picks the response to `expected_id` out of a body that may be a bare JSON
/// object or a stream of SSE events.
///
/// A streamable-HTTP server is free to emit notifications ahead of the answer
/// on the same response body, so taking the first `data:` line meant one
/// well-behaved progress notification broke the call it was reporting on.
fn parse_body(body: &[u8], expected_id: u64) -> Result<Value, McpTransportError> {
    let body = std::str::from_utf8(body)
        .map_err(|_| McpTransportError::Protocol("MCP HTTP response is malformed".into()))?;

    let mut payloads = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .peekable();
    if payloads.peek().is_none() {
        return serde_json::from_str(body)
            .map_err(|_| McpTransportError::Protocol("MCP HTTP response is malformed".into()));
    }

    let mut fallback = None;
    for payload in payloads {
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if value.get("id").and_then(Value::as_u64) == Some(expected_id) {
            return Ok(value);
        }
        fallback.get_or_insert(value);
    }

    // Nothing in the stream answered this request. The first well-formed
    // event is still the most informative thing to fail against, since a
    // server-side JSON-RPC error arrives with no id at all.
    fallback.ok_or_else(|| McpTransportError::Protocol("MCP HTTP response is malformed".into()))
}
