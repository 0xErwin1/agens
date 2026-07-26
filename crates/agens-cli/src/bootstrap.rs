//! Resolves the effective runtime configuration (`Bootstrap`) from the
//! project/global TOML documents, environment, and stored credentials.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agens_config::{
    ConfigPaths, ConfigPermissionRule, McpDefaultSettings, McpTransport, ResolvedSettings,
    SubagentSettings, ToolLimitSettings, expand_environment, expand_environment_with_commands,
    extract_permission_rules, mcp_servers, merge_toml_documents, parse_toml_document,
    resolve_paths, resolve_settings, validate_toml_document,
};
use agens_tools::{McpStatusHandle, McpStdioTransport, McpStdioTransportConfig};

use crate::{CliDependencies, CliError, HeadlessChatRequest};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderSource {
    Auto,
    ExplicitChatGpt,
    ExplicitOther,
}
pub struct Bootstrap {
    pub(crate) paths: ConfigPaths,
    pub(crate) global_loaded: bool,
    pub(crate) project_loaded: bool,
    pub(crate) model: Option<String>,
    pub(crate) provider_type: Option<String>,
    pub(crate) provider_source: ProviderSource,
    pub(crate) provider_base_url: Option<String>,
    pub(crate) system_prompt: Option<String>,
    pub(crate) max_iterations: Option<usize>,
    pub(crate) parallel_tool_calls: bool,
    pub(crate) collapse_thinking: bool,
    pub(crate) debug: bool,
    pub(crate) default_agent: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) tool_limits: ToolLimitSettings,
    pub(crate) subagent_limits: SubagentSettings,
    pub(crate) mcp_defaults: McpDefaultSettings,
    pub(crate) settings: ResolvedSettings,
    pub(crate) openai_api_key: Option<String>,
    pub(crate) data_directory: PathBuf,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) mcp_servers: Vec<agens_config::McpServerConfig>,
    pub(crate) mcp_status: Option<McpStatusHandle>,
    pub(crate) permission_rules: Vec<ConfigPermissionRule>,
}

impl Clone for Bootstrap {
    fn clone(&self) -> Self {
        Self {
            paths: ConfigPaths {
                global_config: self.paths.global_config.clone(),
                credentials: self.paths.credentials.clone(),
                project_config: self.paths.project_config.clone(),
            },
            global_loaded: self.global_loaded,
            project_loaded: self.project_loaded,
            model: self.model.clone(),
            provider_type: self.provider_type.clone(),
            provider_source: self.provider_source,
            provider_base_url: self.provider_base_url.clone(),
            system_prompt: self.system_prompt.clone(),
            max_iterations: self.max_iterations,
            parallel_tool_calls: self.parallel_tool_calls,
            collapse_thinking: self.collapse_thinking,
            debug: self.debug,
            default_agent: self.default_agent.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            tool_limits: self.tool_limits,
            subagent_limits: self.subagent_limits,
            mcp_defaults: self.mcp_defaults,
            settings: self.settings.clone(),
            openai_api_key: self.openai_api_key.clone(),
            data_directory: self.data_directory.clone(),
            project_root: self.project_root.clone(),
            mcp_servers: self.mcp_servers.clone(),
            mcp_status: self.mcp_status.clone(),
            permission_rules: self.permission_rules.clone(),
        }
    }
}

impl Bootstrap {
    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    pub fn settings(&self) -> &ResolvedSettings {
        &self.settings
    }

    pub fn tool_limits(&self) -> ToolLimitSettings {
        self.tool_limits
    }

    pub fn subagent_limits(&self) -> SubagentSettings {
        self.subagent_limits
    }

    pub fn mcp_defaults(&self) -> McpDefaultSettings {
        self.mcp_defaults
    }

    pub fn debug(&self) -> bool {
        self.debug
    }

    pub fn default_agent(&self) -> Option<&str> {
        self.default_agent.as_deref()
    }

    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn provider_type(&self) -> Option<&str> {
        self.provider_type.as_deref()
    }

    pub fn provider_base_url(&self) -> Option<&str> {
        self.provider_base_url.as_deref()
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }

    pub(crate) fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    pub(crate) fn permission_rules(&self) -> &[ConfigPermissionRule] {
        &self.permission_rules
    }

    pub fn mcp_transports(
        &self,
    ) -> Result<Vec<(String, McpStdioTransport, std::time::Duration)>, CliError> {
        let project_root = self
            .project_root
            .as_deref()
            .ok_or_else(|| CliError::configuration("MCP project root is unavailable"))?;
        self.mcp_servers
            .iter()
            .filter(|server| !server.disabled && server.transport == McpTransport::Stdio)
            .map(|server| {
                let transport = McpStdioTransport::spawn(McpStdioTransportConfig {
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
                .map_err(|_| CliError::configuration("MCP server configuration is unavailable"))?;
                Ok((
                    server.name.clone(),
                    transport,
                    std::time::Duration::from_millis(server.timeout_ms),
                ))
            })
            .collect()
    }
}

/// Applies the configured reasoning effort to a request that carries none.
/// An explicit model selection or `/effort` choice already populated the
/// request config, and must not be overwritten by the configured default.
pub(crate) fn seed_configured_reasoning_effort(
    request: &mut HeadlessChatRequest,
    bootstrap: &Bootstrap,
) {
    if request.request_config.reasoning_effort().is_some() {
        return;
    }
    let Some(effort) = bootstrap.reasoning_effort() else {
        return;
    };
    let Ok(config) = agens_core::RequestConfig::with_reasoning_effort(effort) else {
        return;
    };

    request.session_reasoning_effort = config.reasoning_effort();
    request.request_config = config;
}

/// Applies the configuration precedence contract for a turn's iteration cap:
/// a command-line value always wins over the configured one.
pub(crate) fn effective_max_iterations(
    flag: Option<usize>,
    configured: Option<usize>,
) -> Option<usize> {
    flag.or(configured)
}

pub fn bootstrap(dependencies: &CliDependencies) -> Result<Bootstrap, CliError> {
    let current_directory = (dependencies.current_directory)()?;
    let home_directory = (dependencies.home_directory)();
    let environment = (dependencies.environment)();
    let project_root = discover_project_root(&current_directory);
    let config_root = project_root.as_deref().unwrap_or(&current_directory);
    let paths = resolve_paths(config_root, home_directory.as_deref(), &environment);
    let (global, global_loaded) = load_toml(&paths.global_config, "global", dependencies)?;
    let (project, project_loaded) = load_toml(&paths.project_config, "project", dependencies)?;
    if project.contains_key("mcp") {
        return Err(CliError::configuration(
            "project configuration cannot define MCP servers",
        ));
    }
    let permission_rules = extract_permission_rules(&global, &project)
        .map_err(|_| CliError::configuration("permission configuration is invalid"))?;
    let documented_global = global.clone();
    let documented_project = project.clone();
    let global = expand_global_mcp(global, &environment)?;
    let document = merge_toml_documents(global, project);
    let document = expand_document(document, &environment)?;
    let settings = resolve_settings(&documented_global, &documented_project, &document);

    let mcp_servers = mcp_servers(&document)
        .map_err(|_| CliError::configuration("MCP server configuration is invalid"))?;
    let credentials = (dependencies.read_file)(&paths.credentials)?;
    let configured_provider = settings.text("provider.type").map(ToOwned::to_owned);
    let provider_source = match configured_provider.as_deref() {
        None => ProviderSource::Auto,
        Some("openai-chatgpt") => ProviderSource::ExplicitChatGpt,
        Some(_) => ProviderSource::ExplicitOther,
    };
    let provider_type =
        resolve_provider_type(configured_provider, credentials.as_deref(), &environment);
    Ok(Bootstrap {
        model: settings.text("provider.model").map(ToOwned::to_owned),
        provider_type,
        provider_source,
        provider_base_url: settings.text("provider.base_url").map(ToOwned::to_owned),
        system_prompt: settings.text("agent.system_prompt").map(ToOwned::to_owned),
        max_iterations: settings
            .integer("agent.max_iterations")
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0),
        parallel_tool_calls: settings
            .boolean("agent.parallel_tool_calls")
            .unwrap_or(true),
        collapse_thinking: settings.boolean("ui.collapse_thinking").unwrap_or(false),
        debug: settings.boolean("options.debug").unwrap_or(true),
        default_agent: settings.text("agent.default_agent").map(ToOwned::to_owned),
        reasoning_effort: settings
            .text("agent.reasoning_effort")
            .map(ToOwned::to_owned),
        tool_limits: ToolLimitSettings::from(&settings),
        subagent_limits: SubagentSettings::from(&settings),
        mcp_defaults: McpDefaultSettings::from(&settings),
        settings,
        openai_api_key: openai_api_key(credentials.as_deref(), &environment),
        data_directory: data_directory(&document, home_directory.as_deref(), &environment),
        project_root,
        mcp_servers,
        mcp_status: None,
        permission_rules,
        paths,
        global_loaded,
        project_loaded,
    })
}

pub(crate) fn discover_project_root(current_directory: &Path) -> Option<PathBuf> {
    let mut current = fs::canonicalize(current_directory).ok()?;

    loop {
        if current.join(".git").exists() {
            return Some(current);
        }

        let parent = current.parent().map(Path::to_path_buf);
        match parent {
            Some(parent) if parent != current => current = parent,
            _ => return None,
        }
    }
}

fn data_directory(
    document: &toml::Table,
    home_directory: Option<&Path>,
    environment: &BTreeMap<String, String>,
) -> PathBuf {
    string_value(document, &["options", "data_dir"])
        .filter(|directory| !directory.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            environment
                .get("XDG_DATA_HOME")
                .filter(|directory| !directory.is_empty())
                .map(PathBuf::from)
                .or_else(|| home_directory.map(|directory| directory.join(".local/share")))
                .unwrap_or_else(|| PathBuf::from(".local/share"))
                .join("agens")
        })
}

fn load_toml(
    path: &Path,
    scope: &str,
    dependencies: &CliDependencies,
) -> Result<(toml::Table, bool), CliError> {
    let Some(contents) = (dependencies.read_file)(path)? else {
        return Ok((toml::Table::new(), false));
    };

    let document = parse_toml_document(&contents)
        .map_err(|_| CliError::configuration(format!("{scope} configuration is invalid")))?;
    validate_toml_document(&document)
        .map_err(|_| CliError::configuration(format!("{scope} configuration is invalid")))?;

    Ok((document, true))
}

fn expand_document(
    mut document: toml::Table,
    environment: &BTreeMap<String, String>,
) -> Result<toml::Table, CliError> {
    for (section, field) in [("options", "data_dir"), ("provider", "base_url")] {
        if let Some(table) = document
            .get_mut(section)
            .and_then(toml::Value::as_table_mut)
        {
            expand_string_field(table, field, environment)?;
        }
    }
    Ok(document)
}

fn expand_global_mcp(
    mut document: toml::Table,
    environment: &BTreeMap<String, String>,
) -> Result<toml::Table, CliError> {
    if let Some(servers) = document.get_mut("mcp").and_then(toml::Value::as_table_mut) {
        for server in servers
            .iter_mut()
            .filter_map(|(_, value)| value.as_table_mut())
        {
            if server
                .get("disabled")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            for field in ["command", "cwd", "url"] {
                expand_mcp_string_field(server, field, environment)?;
            }
            for field in ["env", "headers"] {
                if let Some(values) = server.get_mut(field).and_then(toml::Value::as_table_mut) {
                    for (_, value) in values.iter_mut() {
                        expand_mcp_value_in_place(value, environment)?;
                    }
                }
            }
            if let Some(args) = server.get_mut("args").and_then(toml::Value::as_array_mut) {
                for value in args {
                    expand_mcp_value_in_place(value, environment)?;
                }
            }
        }
    }
    Ok(document)
}

pub(crate) fn resolve_provider_type(
    configured: Option<String>,
    credentials: Option<&str>,
    environment: &BTreeMap<String, String>,
) -> Option<String> {
    if matches!(configured.as_deref(), Some("openai-api" | "openai-chatgpt")) {
        return configured;
    }
    let credentials =
        credentials.and_then(|contents| serde_json::from_str::<serde_json::Value>(contents).ok());
    let chatgpt = credentials
        .as_ref()
        .and_then(|credentials| credentials.get("openai-chatgpt"));
    if chatgpt.is_some_and(|entry| {
        ["access_token", "refresh_token", "account_id", "expires_at"]
            .iter()
            .all(|field| {
                entry
                    .get(*field)
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            })
    }) {
        return Some("openai-chatgpt".to_owned());
    }
    if credentials
        .as_ref()
        .and_then(|credentials| credentials.get("openai-api"))
        .and_then(|entry| entry.get("api_key"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| !value.is_empty())
        || environment
            .get("OPENAI_API_KEY")
            .is_some_and(|value| !value.is_empty())
    {
        return Some("openai-api".to_owned());
    }
    None
}

pub(crate) fn openai_api_key(
    credentials: Option<&str>,
    environment: &BTreeMap<String, String>,
) -> Option<String> {
    environment
        .get("OPENAI_API_KEY")
        .filter(|key| !key.is_empty())
        .cloned()
        .or_else(|| {
            credentials
                .and_then(|contents| serde_json::from_str::<serde_json::Value>(contents).ok())
                .and_then(|credentials| {
                    credentials
                        .get("openai-api")?
                        .get("api_key")?
                        .as_str()
                        .filter(|key| !key.is_empty())
                        .map(ToOwned::to_owned)
                })
        })
}

fn expand_value_in_place(
    value: &mut toml::Value,
    environment: &BTreeMap<String, String>,
) -> Result<(), CliError> {
    if let Some(raw) = value.as_str() {
        *value =
            toml::Value::String(expand_environment(raw, environment).map_err(|_| {
                CliError::configuration("configuration environment expansion failed")
            })?);
    }
    Ok(())
}

fn expand_mcp_value_in_place(
    value: &mut toml::Value,
    environment: &BTreeMap<String, String>,
) -> Result<(), CliError> {
    if let Some(raw) = value.as_str() {
        *value =
            toml::Value::String(expand_environment_with_commands(raw, environment).map_err(
                |_| CliError::configuration("configuration environment expansion failed"),
            )?);
    }
    Ok(())
}

fn expand_string_field(
    table: &mut toml::Table,
    field: &str,
    environment: &BTreeMap<String, String>,
) -> Result<(), CliError> {
    if let Some(value) = table.get_mut(field) {
        expand_value_in_place(value, environment)?;
    }
    Ok(())
}

fn expand_mcp_string_field(
    table: &mut toml::Table,
    field: &str,
    environment: &BTreeMap<String, String>,
) -> Result<(), CliError> {
    if let Some(value) = table.get_mut(field) {
        expand_mcp_value_in_place(value, environment)?;
    }
    Ok(())
}

fn string_value(document: &toml::Table, path: &[&str]) -> Option<String> {
    let mut value = document.get(*path.first()?)?;

    for key in &path[1..] {
        value = value.as_table()?.get(*key)?;
    }

    value.as_str().map(ToOwned::to_owned)
}
