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
            McpTransportError::Transport(_) | McpTransportError::HttpStatus(_) => Self::Transport,
        }
    }
}

impl McpErrorCategory {
    /// Stable, lower-case identifier for this category, used as the prefix of
    /// every sanitized status reason and by any surface that renders the
    /// category on its own (e.g. the `/mcp` overlay).
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::RetriesExhausted => "retries_exhausted",
            Self::Protocol => "protocol",
            Self::Transport => "transport",
            Self::Unavailable => "unavailable",
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

#[derive(Default)]
struct McpStatusInner {
    servers: BTreeMap<String, McpServerStatus>,
    /// Number of live registries that currently claim a given server name.
    ///
    /// Several registries can share one `McpStatusHandle` and configure the
    /// same server name (e.g. a long-lived router registry and a per-turn
    /// registry). `close_servers` must only retire an entry once every
    /// claiming registry has released it, otherwise an ephemeral registry's
    /// shutdown would falsely report a server owned by another registry as
    /// `Closed`.
    claims: BTreeMap<String, usize>,
}

#[derive(Clone, Default)]
pub struct McpStatusHandle(Arc<Mutex<McpStatusInner>>);

impl McpStatusHandle {
    pub fn snapshot(&self) -> McpStatusSnapshot {
        let inner = self.lock();
        McpStatusSnapshot {
            servers: inner.servers.values().cloned().collect(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, McpStatusInner> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers or refreshes a server's descriptor.
    ///
    /// `claim` must be `true` exactly once per (registry, server name) pair —
    /// the caller is responsible for deduplicating repeated configuration
    /// calls for the same server within one registry, so that the matching
    /// `close_servers` call retires the entry only when every claiming
    /// registry has released it.
    ///
    /// An existing entry whose enabled-ness is unchanged keeps its lifecycle
    /// state, tool count, tool names, and last error: re-registering a server
    /// (e.g. because a new per-turn registry is constructed against the same
    /// shared handle) must not falsify a `Ready` server back to `Idle`.
    ///
    /// The one exception is `Closed`: it only ever means "no registry
    /// currently claims this name". A fresh claim on a closed enabled server
    /// means no connect attempt has been made by its new owner yet, so
    /// preserving `Closed` would misreport a healthy-until-proven-otherwise
    /// server as permanently closed. That entry resets to `Idle` instead.
    pub(crate) fn register(&self, descriptor: McpServerDescriptor, claim: bool) {
        let mut inner = self.lock();
        if claim {
            *inner.claims.entry(descriptor.name.clone()).or_insert(0) += 1;
        }
        match inner.servers.get_mut(&descriptor.name) {
            Some(existing)
                if existing.descriptor.enabled == descriptor.enabled
                    && existing.state == McpLifecycleState::Closed
                    && descriptor.enabled =>
            {
                existing.descriptor = descriptor;
                existing.state = McpLifecycleState::Idle;
                existing.tool_count = 0;
                existing.tool_names.clear();
                existing.last_error = None;
            }
            Some(existing) if existing.descriptor.enabled == descriptor.enabled => {
                existing.descriptor = descriptor;
            }
            _ => {
                let state = if descriptor.enabled {
                    McpLifecycleState::Idle
                } else {
                    McpLifecycleState::Disabled
                };
                inner.servers.insert(
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
        }
    }

    pub(crate) fn update(&self, name: &str, update: impl FnOnce(&mut McpServerStatus)) {
        let mut inner = self.lock();
        if let Some(status) = inner.servers.get_mut(name) {
            update(status);
        }
    }

    /// Releases one claim per named server, marking it closed only once its
    /// claim count reaches zero, leaving every other entry untouched.
    ///
    /// The handle is shared by every registry built from the same bootstrap, so a
    /// registry that shuts down must only retire the servers it owns, and only
    /// once no other registry still claims that same name. Closing the whole
    /// map would retire the entries of registries that are still live.
    pub(crate) fn close_servers<'a>(&self, names: impl IntoIterator<Item = &'a str>) {
        let mut inner = self.lock();
        for name in names {
            let released = match inner.claims.get_mut(name) {
                Some(count) if *count > 1 => {
                    *count -= 1;
                    false
                }
                Some(_) => {
                    inner.claims.remove(name);
                    true
                }
                None => true,
            };
            if !released {
                continue;
            }
            if let Some(status) = inner.servers.get_mut(name)
                && status.state != McpLifecycleState::Disabled
            {
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
