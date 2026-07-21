use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use super::McpTransportError;

pub const MAX_MCP_STATUS_TOOL_NAMES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpServerSource {
    Global,
    Project,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpServerTransport {
    Stdio,
    Http,
    Sse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpLifecycleState {
    Disabled,
    Idle,
    Connecting,
    Ready,
    Degraded,
    Failed,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpErrorCategory {
    Cancelled,
    Timeout,
    RetriesExhausted,
    Protocol,
    Transport,
    Unavailable,
}

impl From<&McpTransportError> for McpErrorCategory {
    fn from(error: &McpTransportError) -> Self {
        match error {
            McpTransportError::Cancelled => Self::Cancelled,
            McpTransportError::TimedOut => Self::Timeout,
            McpTransportError::RetriesExhausted => Self::RetriesExhausted,
            McpTransportError::Protocol(_) => Self::Protocol,
            McpTransportError::Transport(_) => Self::Transport,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpEndpointSummary(String);

impl McpEndpointSummary {
    pub fn stdio(command: impl AsRef<Path>) -> Self {
        let command = command.as_ref();
        Self(
            command
                .file_name()
                .unwrap_or(command.as_os_str())
                .to_string_lossy()
                .into_owned(),
        )
    }

    pub fn remote(endpoint: &str) -> Result<Self, McpTransportError> {
        let url = reqwest::Url::parse(endpoint)
            .map_err(|_| McpTransportError::Transport("MCP endpoint is invalid".into()))?;
        let origin = url.origin().ascii_serialization();
        if origin == "null" {
            return Err(McpTransportError::Transport(
                "MCP endpoint is invalid".into(),
            ));
        }

        Ok(Self(format!("{origin}{}", url.path())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerDescriptor {
    pub(crate) name: String,
    pub(crate) source: McpServerSource,
    pub(crate) transport: McpServerTransport,
    pub(crate) enabled: bool,
    pub(crate) timeout: Duration,
    pub(crate) endpoint: Option<McpEndpointSummary>,
}

impl McpServerDescriptor {
    pub fn new(
        name: impl Into<String>,
        source: McpServerSource,
        transport: McpServerTransport,
        enabled: bool,
        timeout: Duration,
        endpoint: Option<McpEndpointSummary>,
    ) -> Self {
        Self {
            name: name.into(),
            source,
            transport,
            enabled,
            timeout,
            endpoint,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn source(&self) -> McpServerSource {
        self.source
    }

    pub const fn transport(&self) -> McpServerTransport {
        self.transport
    }

    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn endpoint(&self) -> Option<&McpEndpointSummary> {
        self.endpoint.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpStatusError {
    pub(crate) category: McpErrorCategory,
    pub(crate) message: String,
}

impl McpStatusError {
    pub const fn category(&self) -> McpErrorCategory {
        self.category
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerStatus {
    pub(crate) descriptor: McpServerDescriptor,
    pub(crate) state: McpLifecycleState,
    pub(crate) tool_count: usize,
    pub(crate) tool_names: Vec<String>,
    pub(crate) last_error: Option<McpStatusError>,
}

impl McpServerStatus {
    pub fn descriptor(&self) -> &McpServerDescriptor {
        &self.descriptor
    }

    pub const fn state(&self) -> McpLifecycleState {
        self.state
    }

    pub const fn tool_count(&self) -> usize {
        self.tool_count
    }

    pub fn tool_names(&self) -> &[String] {
        &self.tool_names
    }

    pub fn last_error(&self) -> Option<&McpStatusError> {
        self.last_error.as_ref()
    }

    pub fn endpoint(&self) -> Option<&McpEndpointSummary> {
        self.descriptor.endpoint()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpStatusSnapshot {
    servers: Vec<McpServerStatus>,
}

impl McpStatusSnapshot {
    pub fn servers(&self) -> &[McpServerStatus] {
        &self.servers
    }

    pub fn server(&self, name: &str) -> Option<&McpServerStatus> {
        self.servers
            .iter()
            .find(|server| server.descriptor.name == name)
    }
}

#[derive(Clone, Default)]
pub struct McpStatusHandle(pub(crate) Arc<Mutex<BTreeMap<String, McpServerStatus>>>);

impl McpStatusHandle {
    pub fn snapshot(&self) -> McpStatusSnapshot {
        let statuses = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        McpStatusSnapshot {
            servers: statuses.values().cloned().collect(),
        }
    }

    pub(crate) fn register(&self, descriptor: McpServerDescriptor) {
        let state = if descriptor.enabled {
            McpLifecycleState::Idle
        } else {
            McpLifecycleState::Disabled
        };
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                descriptor.name.clone(),
                McpServerStatus {
                    descriptor,
                    state,
                    tool_count: 0,
                    tool_names: Vec::new(),
                    last_error: None,
                },
            );
    }

    pub(crate) fn update(&self, name: &str, update: impl FnOnce(&mut McpServerStatus)) {
        let mut statuses = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(status) = statuses.get_mut(name) {
            update(status);
        }
    }

    pub(crate) fn close(&self) {
        for status in self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values_mut()
        {
            if status.state != McpLifecycleState::Disabled {
                status.state = McpLifecycleState::Closed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_summaries_exclude_stdio_arguments_and_remote_secrets() {
        assert_eq!(
            McpEndpointSummary::stdio("/private/bin/files-server").as_str(),
            "files-server"
        );
        let remote = McpEndpointSummary::remote(
            "https://user:SENTINEL_SECRET@example.test/mcp?token=SENTINEL_SECRET#fragment",
        )
        .unwrap();
        assert_eq!(remote.as_str(), "https://example.test/mcp");
        assert!(!format!("{remote:?}").contains("SENTINEL_SECRET"));
    }
}
