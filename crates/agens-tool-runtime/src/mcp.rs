//! Production MCP transport wiring: loads configured servers into a registry,
//! synchronizes discovered remote tools with the dispatcher, and builds the
//! provider-facing function-tool metadata for remote calls.

use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_config::{DEFAULT_MCP_CONNECT_TIMEOUT_MS, DEFAULT_MCP_LIST_TIMEOUT_MS, McpTransport};
use agens_providers::OpenAiFunctionTool;
use agens_tools::{
    McpEndpointSummary, McpErrorCategory, McpHttpTransport, McpLimits, McpRegistry,
    McpServerDescriptor, McpServerReport, McpServerSource, McpServerTransport, McpSseTransport,
    McpStdioTransport, McpStdioTransportConfig, McpTimeouts, McpTransport as McpTransportPort,
    McpTransportError, RemoteToolMetadata, ToolDispatcher,
};

use agens_bootstrap::Bootstrap;
use agens_dispatch::RegisteredMcpTool;
use agens_error::CliError;
use agens_permissions::SharedToolDispatcher;

pub struct ProductionMcpRuntime {
    pub registry: Arc<Mutex<McpRegistry>>,
    pub dispatcher: SharedToolDispatcher,
}

impl ProductionMcpRuntime {
    /// Discovers every configured server and returns both the merged tool
    /// metadata and each server's discovery report.
    ///
    /// The reports are load-bearing, not incidental: a caller that only
    /// wanted the tools used to discard them (`let _ = ...`), which meant a
    /// server that failed to connect vanished from the returned data entirely
    /// instead of surfacing as failed.
    pub fn discover_configured_tools(
        &mut self,
    ) -> Result<(Vec<RemoteToolMetadata>, Vec<McpServerReport>), CliError> {
        let servers = self
            .registry
            .lock()
            .map_err(|_| CliError::configuration("MCP tools are unavailable"))?
            .configured_server_names();

        let mut reports = Vec::with_capacity(servers.len());
        for server in servers {
            reports.push(self.discover_server(&server)?);
        }

        Ok((self.tools()?, reports))
    }

    pub fn discover_server(
        &mut self,
        server: &str,
    ) -> Result<agens_tools::McpServerReport, CliError> {
        let mut dispatcher = self
            .dispatcher
            .lock()
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| CliError::configuration("MCP tools are unavailable"))?;
        let report = registry.discover_server(server);
        if !report.is_failed() {
            synchronize_server_dispatcher(&mut dispatcher, &registry, &self.registry, server)?;
        }
        Ok(report)
    }

    pub fn reload_server(
        &mut self,
        server: &str,
    ) -> Result<agens_tools::McpServerReport, CliError> {
        let mut dispatcher = self
            .dispatcher
            .lock()
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| CliError::configuration("MCP tools are unavailable"))?;
        let report = registry.reload_server(server);
        if !report.is_failed() {
            synchronize_server_dispatcher(&mut dispatcher, &registry, &self.registry, server)?;
        }
        Ok(report)
    }

    pub fn diagnostics(&self) -> Result<Vec<agens_tools::McpServerDiagnostic>, CliError> {
        Ok(self
            .registry
            .lock()
            .map_err(|_| CliError::configuration("MCP tools are unavailable"))?
            .diagnostics()
            .into_iter()
            .cloned()
            .collect())
    }

    fn tools(&self) -> Result<Vec<RemoteToolMetadata>, CliError> {
        Ok(self
            .registry
            .lock()
            .map_err(|_| CliError::configuration("MCP tools are unavailable"))?
            .tools()
            .into_iter()
            .cloned()
            .collect())
    }
}

fn synchronize_server_dispatcher(
    dispatcher: &mut ToolDispatcher,
    registry: &McpRegistry,
    shared_registry: &Arc<Mutex<McpRegistry>>,
    server: &str,
) -> Result<(), CliError> {
    let tools = registry
        .tools()
        .into_iter()
        .filter(|tool| tool.server_name == server)
        .cloned()
        .collect::<Vec<_>>();

    dispatcher.remove_mcp_server(server);
    for metadata in tools {
        dispatcher
            .register_mcp(
                &metadata,
                RegisteredMcpTool {
                    name: metadata.qualified_name.clone(),
                    registry: Arc::clone(shared_registry),
                },
            )
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    }
    Ok(())
}

pub fn load_configured_mcp_registry(bootstrap: &Bootstrap, project_root: &Path) -> McpRegistry {
    let mut registry = McpRegistry::with_status_handle(bootstrap.mcp_status.clone());

    for server in &bootstrap.mcp_servers {
        let descriptor = mcp_server_descriptor(server);
        if server.disabled {
            let _ = registry.register_disabled_server(descriptor);
            continue;
        }

        let timeouts = match configured_mcp_timeouts(server.timeout_ms) {
            Ok(timeouts) => timeouts,
            Err(error) => {
                register_configuration_failure(&mut registry, descriptor, &error);
                continue;
            }
        };

        let server = server.clone();
        let project_root = project_root.to_path_buf();
        if let Err(error) = registry.configure_server_with_descriptor(
            descriptor.clone(),
            move || configured_mcp_transport(&server, &project_root),
            timeouts,
            McpLimits::default(),
        ) {
            register_configuration_failure(&mut registry, descriptor, &error);
        }
    }

    registry
}

/// Spreads the single configured `timeout_ms` over the three MCP phases.
///
/// `timeout_ms` sizes a tool call, which is the only phase a user can reason
/// about. Connect and tool listing take that value as a lower bound and widen
/// it to their own floors, so a server whose handshake is slower than one tool
/// call still comes up. A deliberately generous `timeout_ms` still raises all
/// three, which is what a slow remote server needs.
fn configured_mcp_timeouts(timeout_ms: u64) -> Result<McpTimeouts, McpTransportError> {
    let call = std::time::Duration::from_millis(timeout_ms);
    let connect = call.max(std::time::Duration::from_millis(
        DEFAULT_MCP_CONNECT_TIMEOUT_MS,
    ));
    let list = call.max(std::time::Duration::from_millis(
        DEFAULT_MCP_LIST_TIMEOUT_MS,
    ));

    McpTimeouts::new(connect, list, call)
}

/// Records a server that failed before any connect attempt was possible
/// (an invalid timeout or a rejected server name) as `Failed` on the shared
/// status handle, so it stays visible in `/mcp` instead of silently
/// disappearing from the configured set.
fn register_configuration_failure(
    registry: &mut McpRegistry,
    descriptor: McpServerDescriptor,
    error: &McpTransportError,
) {
    let category = McpErrorCategory::from(error);
    let message = format!("{}: server configuration is invalid", category.label());
    let _ = registry.register_failed_server(descriptor, category, &message);
}

fn mcp_server_descriptor(server: &agens_config::McpServerConfig) -> McpServerDescriptor {
    let transport = match server.transport {
        McpTransport::Stdio => McpServerTransport::Stdio,
        McpTransport::Http => McpServerTransport::Http,
        McpTransport::Sse => McpServerTransport::Sse,
    };
    let endpoint = match server.transport {
        McpTransport::Stdio => server.command.as_ref().map(McpEndpointSummary::stdio),
        McpTransport::Http | McpTransport::Sse => server
            .url
            .as_deref()
            .and_then(|url| McpEndpointSummary::remote(url).ok()),
    };
    McpServerDescriptor::new(
        &server.name,
        McpServerSource::Global,
        transport,
        !server.disabled,
        std::time::Duration::from_millis(server.timeout_ms),
        endpoint,
    )
}

fn configured_mcp_transport(
    server: &agens_config::McpServerConfig,
    project_root: &Path,
) -> Result<Box<dyn McpTransportPort>, McpTransportError> {
    match server.transport {
        McpTransport::Stdio => McpStdioTransport::spawn(McpStdioTransportConfig {
            command: server
                .command
                .clone()
                .expect("stdio MCP commands are validated"),
            args: server.args.clone(),
            environment: server.environment.clone(),
            project_root: server
                .cwd
                .clone()
                .unwrap_or_else(|| project_root.to_path_buf()),
        })
        .map(|transport| Box::new(transport) as Box<dyn McpTransportPort>),
        McpTransport::Http => McpHttpTransport::new(
            server.url.clone().expect("HTTP MCP URLs are validated"),
            server.headers.clone(),
            server.max_retries,
        )
        .map(|transport| Box::new(transport) as Box<dyn McpTransportPort>),
        McpTransport::Sse => McpSseTransport::new(
            server.url.clone().expect("SSE MCP URLs are validated"),
            server.headers.clone(),
            server.max_retries,
        )
        .map(|transport| Box::new(transport) as Box<dyn McpTransportPort>),
    }
}

pub fn native_model_tool_name(qualified_name: &str) -> Result<String, CliError> {
    qualified_name
        .strip_prefix("native::")
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| CliError::configuration("native tool metadata is invalid"))
}

pub fn mcp_model_tool_name(metadata: &RemoteToolMetadata) -> String {
    format!("{}_{}", metadata.server_name, metadata.tool_name)
}

pub fn remote_function_tool(
    metadata: &RemoteToolMetadata,
    model_name: String,
) -> Result<OpenAiFunctionTool, CliError> {
    OpenAiFunctionTool::new(
        model_name,
        metadata
            .description
            .clone()
            .unwrap_or_else(|| "MCP tool".to_owned()),
        metadata.input_schema.clone(),
    )
    .map_err(|_| CliError::configuration("MCP tool metadata is invalid"))
}

#[cfg(test)]
mod tests {
    use agens_core::{
        PermissionDecision, PermissionMode, PermissionPattern, PermissionPolicy, PermissionRule,
        PermissionSession,
    };
    use agens_tools::{
        McpLifecycleState, ToolDispatchRequest, ToolEvaluationOutcome, ToolExecutionContext,
    };

    use super::*;

    #[test]
    fn a_short_configured_timeout_bounds_the_call_but_keeps_the_connect_and_list_floors() {
        let timeouts = configured_mcp_timeouts(200).expect("200ms is a valid call timeout");

        assert_eq!(timeouts.call, std::time::Duration::from_millis(200));
        assert_eq!(
            timeouts.connect,
            std::time::Duration::from_millis(DEFAULT_MCP_CONNECT_TIMEOUT_MS)
        );
        assert_eq!(
            timeouts.list,
            std::time::Duration::from_millis(DEFAULT_MCP_LIST_TIMEOUT_MS)
        );
    }

    #[test]
    fn a_generous_configured_timeout_widens_every_phase() {
        let timeout_ms = DEFAULT_MCP_CONNECT_TIMEOUT_MS * 12;
        let timeouts =
            configured_mcp_timeouts(timeout_ms).expect("a generous call timeout is valid");

        let expected = std::time::Duration::from_millis(timeout_ms);
        assert_eq!(timeouts.call, expected);
        assert_eq!(timeouts.connect, expected);
        assert_eq!(timeouts.list, expected);
    }

    #[test]
    fn a_zero_configured_timeout_is_still_rejected_despite_the_connect_floor() {
        assert!(configured_mcp_timeouts(0).is_err());
    }

    #[test]
    fn invalid_timeout_still_yields_an_mcp_visible_failed_descriptor() {
        let mut bootstrap =
            agens_fixtures::bootstrap_from_configuration("invalid-mcp-timeout", None, None);
        bootstrap.mcp_servers = vec![agens_config::McpServerConfig {
            name: "broken".into(),
            disabled: false,
            transport: McpTransport::Stdio,
            command: Some("/bin/echo".into()),
            args: Vec::new(),
            environment: std::collections::BTreeMap::new(),
            cwd: None,
            url: None,
            headers: std::collections::BTreeMap::new(),
            max_retries: 0,
            timeout_ms: 0,
        }];

        let registry = load_configured_mcp_registry(&bootstrap, Path::new("/tmp"));
        let snapshot = registry.status_handle().snapshot();
        let broken = snapshot
            .server("broken")
            .expect("an invalid-timeout server must remain visible in /mcp instead of vanishing");

        assert_eq!(broken.state(), McpLifecycleState::Failed);
        assert!(
            !registry
                .configured_server_names()
                .contains(&"broken".to_owned()),
            "an invalid-timeout server was never successfully configured for connect attempts"
        );
    }

    #[test]
    fn production_mcp_runtime_reloads_dispatcher_and_retains_failed_generation() {
        use std::{collections::VecDeque, sync::atomic::AtomicUsize, time::Duration};

        struct TestTransport(VecDeque<agens_tools::McpResponse>);

        impl McpTransportPort for TestTransport {
            fn execute(
                &mut self,
                _: agens_tools::McpRequest,
                _: &agens_tools::McpOperationContext,
            ) -> Result<agens_tools::McpResponse, McpTransportError> {
                Ok(self
                    .0
                    .pop_front()
                    .expect("test transport response is configured"))
            }

            fn notify(
                &mut self,
                _: agens_tools::McpRequest,
                _: &agens_tools::McpOperationContext,
            ) -> Result<(), McpTransportError> {
                Ok(())
            }

            fn close(
                &mut self,
                _: &agens_tools::McpOperationContext,
            ) -> Result<(), McpTransportError> {
                Ok(())
            }
        }

        fn transport(name: &str) -> TestTransport {
            TestTransport(
                [
                    agens_tools::McpResponse::Initialized(agens_tools::McpInitializeResult::new(
                        agens_tools::MCP_PROTOCOL_VERSION,
                        serde_json::json!({"tools": {}}),
                    )),
                    agens_tools::McpResponse::ToolsListed(agens_tools::McpToolsPage::new(
                        vec![agens_tools::McpToolDefinition {
                            name: name.into(),
                            description: Some(name.into()),
                            input_schema: serde_json::json!({"type": "object"}),
                            annotations: agens_tools::McpToolAnnotations {
                                read_only_hint: Some(true),
                            },
                        }],
                        None,
                    )),
                ]
                .into(),
            )
        }

        let attempts = Arc::new(AtomicUsize::new(0));
        let attempt_counter = Arc::clone(&attempts);
        let registry = Arc::new(Mutex::new(McpRegistry::new()));
        registry
            .lock()
            .unwrap()
            .configure_server(
                "files",
                move || match attempt_counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel) {
                    0 => Ok(Box::new(transport("old"))),
                    1 => Err(McpTransportError::Transport("SENTINEL_SECRET".into())),
                    _ => Ok(Box::new(transport("new"))),
                },
                McpTimeouts::new(
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .unwrap(),
                McpLimits::default(),
            )
            .unwrap();
        let mut runtime = ProductionMcpRuntime {
            registry,
            dispatcher: Arc::new(Mutex::new(ToolDispatcher::new())),
        };

        runtime.discover_server("files").unwrap();
        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Any,
                PermissionPattern::Any,
            )],
        );
        let ToolEvaluationOutcome::Authorized(handle) = runtime
            .dispatcher
            .lock()
            .unwrap()
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new("project", "files_old", serde_json::json!({})),
            )
            .unwrap()
        else {
            panic!("discovered MCP tool must be callable through the dispatcher");
        };

        assert!(runtime.reload_server("files").unwrap().is_failed());
        assert!(
            runtime
                .diagnostics()
                .unwrap()
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("SENTINEL_SECRET"))
        );
        assert!(
            runtime
                .dispatcher
                .lock()
                .unwrap()
                .canonical_identity("files_old")
                .is_some()
        );

        runtime.reload_server("files").unwrap();
        let mut dispatcher = runtime.dispatcher.lock().unwrap();
        assert!(dispatcher.canonical_identity("files_old").is_none());
        assert!(dispatcher.canonical_identity("files_new").is_some());
        assert!(
            dispatcher
                .execute(
                    handle,
                    &ToolExecutionContext::with_timeout(Duration::from_secs(1))
                )
                .is_err()
        );
    }
}
