use std::collections::BTreeMap;

use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::Value;

use crate::stdio_mcp::{parse_response, request_wire};
use crate::{McpOperationContext, McpRequest, McpResponse, McpTransport, McpTransportError};

const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;

/// JSON-RPC MCP transport for both streamable HTTP and SSE endpoints.
///
/// The transport never follows redirects. This prevents configured credentials
/// from leaving the configured origin while preserving the Go client's
/// same-origin-only header contract.
pub struct McpHttpTransport {
    endpoint: String,
    headers: HeaderMap,
    max_retries: u32,
    next_id: u64,
}

impl McpHttpTransport {
    pub fn new(
        endpoint: String,
        headers: BTreeMap<String, String>,
        max_retries: u32,
    ) -> Result<Self, McpTransportError> {
        let endpoint = reqwest::Url::parse(&endpoint)
            .map_err(|_| McpTransportError::Transport("MCP endpoint is invalid".into()))?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host().is_none() {
            return Err(McpTransportError::Transport(
                "MCP endpoint is invalid".into(),
            ));
        }
        let endpoint = endpoint.to_string();
        let mut parsed_headers = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| McpTransportError::Transport("MCP headers are invalid".into()))?;
            let value = HeaderValue::from_str(&value)
                .map_err(|_| McpTransportError::Transport("MCP headers are invalid".into()))?;
            parsed_headers.insert(name, value);
        }
        Ok(Self {
            endpoint,
            headers: parsed_headers,
            max_retries,
            next_id: 1,
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
        let wire = request_wire(request, id);
        let attempts = self.max_retries.saturating_add(1);
        for attempt in 0..attempts {
            let timeout = context.remaining()?;
            let client = Client::builder()
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| {
                    McpTransportError::Transport("MCP HTTP client is unavailable".into())
                })?;
            let response = client
                .post(&self.endpoint)
                .headers(self.headers.clone())
                .json(&wire)
                .send();
            match response {
                Ok(response) if response.status().is_success() => {
                    if notify {
                        return Ok(None);
                    }
                    let body = response.bytes().map_err(|_| {
                        McpTransportError::Transport("MCP HTTP response is unavailable".into())
                    })?;
                    if body.len() > MAX_HTTP_BODY_BYTES {
                        return Err(McpTransportError::Protocol(
                            "MCP HTTP response exceeds limit".into(),
                        ));
                    }
                    let value = parse_body(&body)?;
                    return parse_response(value, id.expect("requests have identifiers")).map(Some);
                }
                Ok(response) if response.status().is_server_error() && attempt + 1 < attempts => {
                    continue;
                }
                Ok(response) if response.status().is_redirection() => {
                    return Err(McpTransportError::Transport(
                        "MCP cross-origin redirect refused".into(),
                    ));
                }
                Ok(response) => {
                    return Err(McpTransportError::Transport(format!(
                        "MCP HTTP request failed with {}",
                        response.status()
                    )));
                }
                Err(error) if error.is_timeout() => return Err(McpTransportError::TimedOut),
                Err(_) if attempt + 1 < attempts => continue,
                Err(_) => {
                    return Err(McpTransportError::Transport(
                        "MCP HTTP request failed".into(),
                    ));
                }
            }
        }
        Err(McpTransportError::Transport(
            "MCP HTTP request failed".into(),
        ))
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
        Ok(())
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
