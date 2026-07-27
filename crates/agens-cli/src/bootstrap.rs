//! Resolves the effective runtime configuration (`Bootstrap`) from the
//! project/global TOML documents, environment, and stored credentials.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agens_config::{
    ConfigPaths, ConfigPermissionRule, McpDefaultSettings, McpTransport, ResolvedSettings,
    SubagentSettings, ToolLimitSettings, expand_environment, expand_environment_with_commands,
    extract_permission_rules, mcp_servers, merge_toml_documents, parse_toml_document,
    resolve_paths, resolve_settings, validate_toml_document,
};
use agens_tools::{McpStatusHandle, McpStdioTransport, McpStdioTransportConfig};

use crate::{CliDependencies, CliError, HeadlessChatRequest};

pub(crate) mod session_config;
pub(crate) mod session_root;

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
    /// Re-reads a project configuration document from an arbitrary path, the same way
    /// `bootstrap()` read this process's own project config, so [`session_config::SessionConfig`]
    /// can re-derive session-scoped configuration from a session's OWN recorded root instead of
    /// trusting the value this struct captured once from the PROCESS's discovered root.
    pub(in crate::bootstrap) config_reader: crate::deps::ConfigReader,
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
            config_reader: Arc::clone(&self.config_reader),
        }
    }
}

impl Bootstrap {
    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    /// The process's own merged, project-precedence configuration settings, keyed by dotted
    /// path (e.g. `"agent.system_prompt"`, `"provider.base_url"`).
    ///
    /// Visible only within `crate::bootstrap` — narrower than the historical `pub`, because the
    /// untyped [`ResolvedSettings::text`] path reaches every project-settable value through one
    /// generic accessor, including session-scoped ones such as `agent.system_prompt` and
    /// `provider.base_url`, with no name-level signal that a session-scoped decision is being made
    /// from the wrong (process) root. A session-scoped caller must go through
    /// [`session_config::SessionConfig::resolve`] instead, which re-reads the relevant keys fresh
    /// from a session's OWN recorded root. `commands::config::run_config` is the one legitimate
    /// process-level reader left (the `config doctor` report, which is about the process's own
    /// configuration by definition, not a session's).
    pub(crate) fn settings(&self) -> &ResolvedSettings {
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

    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }

    /// The process's own discovered project root, from walking up from the current working
    /// directory to find `.git`.
    ///
    /// This is deliberately visible only to [`session_root`]: a session's confinement root must
    /// come from that module's `SessionRoot`, not from re-deriving this value at an arbitrary
    /// call site, because after a resume this process's current working directory can
    /// legitimately differ from the root the session was created under. The compiler forces any
    /// new call site outside `session_root` to explicitly name and re-export this escape hatch
    /// before it can be reached — it cannot make that escape hatch unreachable once named, so a
    /// call site inside `session_root` that itself re-derives the process root instead of using
    /// the session's recorded one is still a possible, silent mistake, not a compile error.
    pub(in crate::bootstrap) fn discovered_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    /// The permission rules this PROCESS captured from its own discovered root at `bootstrap()`
    /// time. Visible only within `crate::bootstrap`, on purpose: a session-scoped permission
    /// decision must go through [`session_config::SessionConfig::resolve`] instead, which
    /// re-reads a session's OWN recorded root rather than trusting this process-lifetime value —
    /// see that type's documentation for why the distinction matters.
    pub(in crate::bootstrap) fn permission_rules(&self) -> &[ConfigPermissionRule] {
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
        config_reader: Arc::clone(&dependencies.read_file),
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::commands::chat::{chat_args_with_prompt, chat_request};
    use crate::test_support::bootstrap_from_configuration;

    #[test]
    fn bootstrap_retains_the_ui_collapse_thinking_setting() {
        let temporary =
            std::env::temp_dir().join(format!("agens-collapse-thinking-{}", std::process::id()));
        let config_home = temporary.join("config");
        let dependencies = CliDependencies::for_test(
            temporary.join("project"),
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            BTreeMap::from([(
                config_home.join("config.toml"),
                "[ui]\ncollapse_thinking = true\n".to_owned(),
            )]),
        );

        let bootstrap = bootstrap(&dependencies).expect("UI configuration should be valid");

        assert!(bootstrap.collapse_thinking);
    }

    #[test]
    fn bootstrap_defaults_reproduce_the_limits_the_runtime_hardcoded() {
        let bootstrap = bootstrap_from_configuration("config-defaults", None, None);

        let tools = bootstrap.tool_limits();
        assert_eq!(tools.max_list_entries, 1_000);
        assert_eq!(tools.max_search_entries, 10_000);
        assert_eq!(tools.max_search_results, 100);
        assert_eq!(tools.max_search_depth, 32);
        assert_eq!(tools.operation_timeout_ms, 5_000);
        assert_eq!(tools.bash_timeout_ms, 120_000);

        let subagents = bootstrap.subagent_limits();
        assert_eq!(subagents.max_iterations, 16);
        assert_eq!(subagents.max_concurrency, 4);
        assert_eq!(subagents.max_output_chars, 65_536);

        assert_eq!(bootstrap.mcp_defaults().timeout_ms, 10_000);
        assert_eq!(bootstrap.mcp_defaults().max_retries, 0);
        assert!(bootstrap.debug());
        assert_eq!(bootstrap.default_agent(), None);
        assert_eq!(bootstrap.reasoning_effort(), None);
    }

    #[test]
    fn project_configuration_overrides_global_settings_and_records_the_origin() {
        let bootstrap = bootstrap_from_configuration(
            "config-precedence",
            Some("[tools]\nmax_search_depth = 8\nmax_search_results = 25\n"),
            Some("[tools]\nmax_search_depth = 4\n"),
        );

        assert_eq!(bootstrap.tool_limits().max_search_depth, 4);
        assert_eq!(bootstrap.tool_limits().max_search_results, 25);
        assert_eq!(
            bootstrap.settings().origin("tools.max_search_depth"),
            agens_config::Origin::Project
        );
        assert_eq!(
            bootstrap.settings().origin("tools.max_search_results"),
            agens_config::Origin::Global
        );
        assert_eq!(
            bootstrap.settings().origin("tools.max_list_entries"),
            agens_config::Origin::Default
        );
    }

    #[test]
    fn configured_behavioral_settings_reach_bootstrap() {
        let bootstrap = bootstrap_from_configuration(
            "config-behavior",
            Some(
                "[options]\ndebug = true\n\n[agent]\ndefault_agent = \"reviewer\"\nreasoning_effort = \"high\"\n\n[subagents]\nmax_concurrency = 2\n",
            ),
            None,
        );

        assert!(bootstrap.debug());
        assert_eq!(bootstrap.default_agent(), Some("reviewer"));
        assert_eq!(bootstrap.reasoning_effort(), Some("high"));
        assert_eq!(bootstrap.subagent_limits().max_concurrency, 2);
    }

    #[test]
    fn diagnostics_are_captured_unless_debug_is_disabled() {
        let enabled = bootstrap_from_configuration("config-debug-default", None, None);
        let disabled = bootstrap_from_configuration(
            "config-debug-off",
            Some("[options]\ndebug = false\n"),
            None,
        );

        assert!(enabled.debug());
        assert!(!disabled.debug());
    }

    #[test]
    fn the_configured_reasoning_effort_seeds_a_request_that_carries_none() {
        let bootstrap = bootstrap_from_configuration(
            "config-effort",
            Some("[agent]\nreasoning_effort = \"high\"\n"),
            None,
        );
        let mut request = chat_request(chat_args_with_prompt("work")).unwrap();

        seed_configured_reasoning_effort(&mut request, &bootstrap);

        assert_eq!(
            request.request_config.reasoning_effort(),
            Some(agens_core::ReasoningEffort::High)
        );
        assert_eq!(
            request.session_reasoning_effort,
            Some(agens_core::ReasoningEffort::High)
        );
    }

    #[test]
    fn an_explicit_effort_survives_the_configured_default() {
        let bootstrap = bootstrap_from_configuration(
            "config-effort-explicit",
            Some("[agent]\nreasoning_effort = \"high\"\n"),
            None,
        );
        let mut request = chat_request(chat_args_with_prompt("work")).unwrap();
        request.request_config = agens_core::RequestConfig::with_reasoning_effort("low").unwrap();

        seed_configured_reasoning_effort(&mut request, &bootstrap);

        assert_eq!(
            request.request_config.reasoning_effort(),
            Some(agens_core::ReasoningEffort::Low)
        );
    }

    #[test]
    fn an_absent_configured_effort_leaves_the_request_untouched() {
        let bootstrap = bootstrap_from_configuration("config-effort-absent", None, None);
        let mut request = chat_request(chat_args_with_prompt("work")).unwrap();

        seed_configured_reasoning_effort(&mut request, &bootstrap);

        assert_eq!(request.request_config.reasoning_effort(), None);
        assert_eq!(request.session_reasoning_effort, None);
    }

    /// `agent.system_prompt` is read straight out of the merged document with no environment
    /// expansion of any kind — [`expand_document`] only expands `options.data_dir` and
    /// `provider.base_url` — so a `$(...)` pattern in it stays literal, unlike MCP `command`,
    /// `args` and `env` fields, which DO run command substitution
    /// (`global_mcp_command_and_environment_fields_expand` in `tests/cli.rs` covers that
    /// contrast).
    #[test]
    fn system_prompt_is_never_environment_expanded() {
        let bootstrap = bootstrap_from_configuration(
            "config-system-prompt-literal",
            Some("[agent]\nsystem_prompt = \"literal $(printf ignored)\"\n"),
            None,
        );

        assert_eq!(
            bootstrap.settings().text("agent.system_prompt"),
            Some("literal $(printf ignored)")
        );
    }

    #[test]
    fn a_command_line_iteration_cap_overrides_the_configured_one() {
        assert_eq!(effective_max_iterations(Some(9), Some(5)), Some(9));
        assert_eq!(effective_max_iterations(None, Some(5)), Some(5));
        assert_eq!(effective_max_iterations(Some(9), None), Some(9));
        assert_eq!(effective_max_iterations(None, None), None);
    }
}
