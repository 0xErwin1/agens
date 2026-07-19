use std::collections::BTreeMap;

use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::Value;

use crate::http_worker::{
    HttpRequest, HttpResponse, HttpWorker, HttpWorkerError, HttpWorkerFuture, HttpWorkerOperation,
};
use crate::stdio_mcp::{parse_response, request_wire};
use crate::{McpOperationContext, McpRequest, McpResponse, McpTransport, McpTransportError};

const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;
const HTTP_WORKER_CAPACITY: usize = 8;

/// JSON-RPC MCP transport that executes requests on an owned async HTTP worker.
pub struct McpHttpTransport {
    headers: BTreeMap<String, String>,
    max_retries: u32,
    next_id: u64,
    worker: HttpWorker,
    endpoint: String,
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
            headers,
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
                    method: "POST".into(),
                    endpoint: self.endpoint.clone(),
                    headers: self.headers.clone(),
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
                return Err(McpTransportError::Transport(format!(
                    "MCP HTTP request failed with status {}",
                    response.status
                )));
            }
            if notify {
                return Ok(None);
            }
            let value = parse_body(&response.body)?;
            return parse_response(value, id.expect("requests have identifiers")).map(Some);
        }
        Err(McpTransportError::RetriesExhausted)
    }
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
            let response = client
                .request(
                    request
                        .method
                        .parse()
                        .map_err(|_| HttpWorkerError::Transport)?,
                    request.endpoint,
                )
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(request.body)
                .send()
                .await
                .map_err(|_| HttpWorkerError::Transport)?;
            let status = response.status().as_u16();
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
            Ok(HttpResponse { status, body })
        })
    }

    fn close(&mut self) {
        self.client = None;
    }
}

fn worker_error(error: HttpWorkerError) -> McpTransportError {
    match error {
        HttpWorkerError::Cancelled => McpTransportError::Cancelled,
        HttpWorkerError::TimedOut => McpTransportError::TimedOut,
        HttpWorkerError::Transport => {
            McpTransportError::Transport("MCP HTTP request failed".into())
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

fn parse_body(body: &[u8]) -> Result<Value, McpTransportError> {
    let body = std::str::from_utf8(body)
        .map_err(|_| McpTransportError::Protocol("MCP HTTP response is malformed".into()))?;
    let payload = body
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap_or(body);
    serde_json::from_str(payload)
        .map_err(|_| McpTransportError::Protocol("MCP HTTP response is malformed".into()))
}
