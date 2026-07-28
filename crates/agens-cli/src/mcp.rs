//! Production MCP transport wiring: loads configured servers into a registry,
//! synchronizes discovered remote tools with the dispatcher, and builds the
//! provider-facing function-tool metadata for remote calls.

use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_config::McpTransport;
use agens_providers::OpenAiFunctionTool;
use agens_tools::{
    McpEndpointSummary, McpHttpTransport, McpLimits, McpRegistry, McpServerDescriptor,
    McpServerSource, McpServerTransport, McpSseTransport, McpStdioTransport,
    McpStdioTransportConfig, McpTimeouts, McpTransport as McpTransportPort, McpTransportError,
    RemoteToolMetadata, ToolDispatcher,
};

use crate::Bootstrap;
use crate::dispatch::RegisteredMcpTool;
use crate::permissions::SharedToolDispatcher;
use agens_error::CliError;

pub(crate) struct ProductionMcpRuntime {
    pub(crate) registry: Arc<Mutex<McpRegistry>>,
    pub(crate) dispatcher: SharedToolDispatcher,
}

impl ProductionMcpRuntime {
    pub(crate) fn discover_configured_tools(
        &mut self,
    ) -> Result<Vec<RemoteToolMetadata>, CliError> {
        let servers = self
            .registry
            .lock()
            .map_err(|_| CliError::configuration("MCP tools are unavailable"))?
            .configured_server_names();

        for server in servers {
            let _ = self.discover_server(&server)?;
        }

        self.tools()
    }

    pub(crate) fn discover_server(
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

    #[allow(dead_code)]
    pub(crate) fn reload_server(
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

    #[allow(dead_code)]
    pub(crate) fn diagnostics(&self) -> Result<Vec<agens_tools::McpServerDiagnostic>, CliError> {
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

pub(crate) fn load_configured_mcp_registry(
    bootstrap: &Bootstrap,
    project_root: &Path,
) -> McpRegistry {
    let mut registry = bootstrap
        .mcp_status
        .clone()
        .map_or_else(McpRegistry::new, McpRegistry::with_status_handle);

    for server in &bootstrap.mcp_servers {
        let descriptor = mcp_server_descriptor(server);
        if server.disabled {
            let _ = registry.register_disabled_server(descriptor);
            continue;
        }
        let timeout = std::time::Duration::from_millis(server.timeout_ms);
        let Ok(timeouts) = McpTimeouts::new(timeout, timeout, timeout) else {
            continue;
        };

        let server = server.clone();
        let project_root = project_root.to_path_buf();
        let _ = registry.configure_server_with_descriptor(
            descriptor,
            move || configured_mcp_transport(&server, &project_root),
            timeouts,
            McpLimits::default(),
        );
    }

    registry
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

pub(crate) fn native_model_tool_name(qualified_name: &str) -> Result<String, CliError> {
    qualified_name
        .strip_prefix("native::")
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| CliError::configuration("native tool metadata is invalid"))
}

pub(crate) fn mcp_model_tool_name(metadata: &RemoteToolMetadata) -> String {
    format!("{}_{}", metadata.server_name, metadata.tool_name)
}

pub(crate) fn remote_function_tool(
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
    use agens_tools::{ToolDispatchRequest, ToolEvaluationOutcome, ToolExecutionContext};

    use super::*;

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
                        "2025-06-18",
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
