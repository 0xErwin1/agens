//! Resolving a run's configuration: paths, credentials, settings, MCP servers.
//!
//! It reads the host through [`HostEnvironment`] rather than through any
//! command surface, so the daemon and the CLI resolve the same way.

mod host;
pub mod session_config;
pub mod session_root;

pub use host::{ConfigReader, HostEnvironment};

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agens_config::{
    ConfigPaths, ConfigPermissionRule, McpDefaultSettings, McpTransport, ResolvedSettings,
    SubagentSettings, ToolLimitSettings, expand_environment, expand_environment_with_commands,
    extract_permission_rules, mcp_servers_with_defaults, merge_toml_documents, parse_toml_document,
    resolve_paths, resolve_settings, validate_toml_document,
};
use agens_tools::{McpStatusHandle, McpStdioTransport, McpStdioTransportConfig};

use agens_error::CliError;

pub struct Bootstrap {
    pub paths: ConfigPaths,
    pub global_loaded: bool,
    pub project_loaded: bool,
    pub model: Option<String>,
    pub max_iterations: Option<usize>,
    pub parallel_tool_calls: bool,
    pub collapse_thinking: bool,
    pub debug: bool,
    pub default_agent: Option<String>,
    pub reasoning_effort: Option<String>,
    pub tool_limits: ToolLimitSettings,
    pub subagent_limits: SubagentSettings,
    pub mcp_defaults: McpDefaultSettings,
    pub settings: ResolvedSettings,
    /// The environment this run resolved against, retained so a per-provider
    /// credential lookup answers the same way `resolve` did instead of reading
    /// the real process environment behind an injected host's back.
    pub(crate) environment: BTreeMap<String, String>,
    pub data_directory: PathBuf,
    pub project_root: Option<PathBuf>,
    pub mcp_servers: Vec<agens_config::McpServerConfig>,
    /// The shared MCP status handle every per-turn and long-lived registry
    /// built from this bootstrap registers against.
    ///
    /// Non-optional: headless has no other way to inject a handle, so if this
    /// were `None` the per-turn registry would silently fall back to a
    /// private handle nobody else reads, and the failure-visibility surfaces
    /// (`/mcp`, the TUI notice, the headless stderr line) would have nothing
    /// to observe.
    pub mcp_status: McpStatusHandle,
    pub permission_rules: Vec<ConfigPermissionRule>,
    /// Re-reads a project configuration document from an arbitrary path, the same way
    /// `bootstrap()` read this process's own project config, so [`session_config::SessionConfig`]
    /// can re-derive session-scoped configuration from a session's OWN recorded root instead of
    /// trusting the value this struct captured once from the PROCESS's discovered root.
    pub(crate) config_reader: crate::host::ConfigReader,
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
            environment: self.environment.clone(),
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

    /// The environment this run resolved against, for a credential lookup that
    /// must answer identically to `resolve` under an injected host.
    pub fn credential_environment(&self) -> BTreeMap<String, String> {
        self.environment.clone()
    }

    /// One provider's API key, resolved the way this run's own credentials were.
    ///
    /// Per provider rather than one key for the whole run: a turn picks its
    /// provider from the model it was given, so any of them may need
    /// authenticating within the same run.
    pub fn api_key_for(&self, provider: &str) -> Option<String> {
        let credentials = (self.config_reader)(&self.paths.credentials).ok().flatten();

        provider_api_key(provider, credentials.as_deref(), &self.environment)
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
    pub(crate) fn discovered_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    /// The permission rules this PROCESS captured from its own discovered root at `bootstrap()`
    /// time. Visible only within `crate::bootstrap`, on purpose: a session-scoped permission
    /// decision must go through [`session_config::SessionConfig::resolve`] instead, which
    /// re-reads a session's OWN recorded root rather than trusting this process-lifetime value —
    /// see that type's documentation for why the distinction matters.
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

/// Applies the configuration precedence contract for a turn's iteration cap:
/// a command-line value always wins over the configured one.
pub fn effective_max_iterations(flag: Option<usize>, configured: Option<usize>) -> Option<usize> {
    flag.or(configured)
}

pub fn resolve(host: &HostEnvironment) -> Result<Bootstrap, CliError> {
    let current_directory = (host.current_directory)()?;
    let home_directory = (host.home_directory)();
    let environment = (host.environment)();
    let project_root = discover_project_root(&current_directory);
    let config_root = project_root.as_deref().unwrap_or(&current_directory);
    let paths = resolve_paths(config_root, home_directory.as_deref(), &environment);
    let (global, global_loaded) = load_toml(&paths.global_config, "global", host)?;
    let (project, project_loaded) = load_toml(&paths.project_config, "project", host)?;
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

    let mcp_defaults = McpDefaultSettings::from(&settings);
    let mcp_servers = mcp_servers_with_defaults(&document, mcp_defaults)
        .map_err(|_| CliError::configuration("MCP server configuration is invalid"))?;
    // Read for its error alone. Credentials are resolved per provider from here
    // on, and that lookup cannot report a failure, so an unreadable file would
    // otherwise surface much later as "no provider has usable credentials".
    (host.read_file)(&paths.credentials)?;

    let model = settings
        .text("provider.model")
        .map(resolve_configured_model)
        .transpose()?;

    Ok(Bootstrap {
        model,
        environment: environment.clone(),
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
        mcp_defaults,
        settings,
        data_directory: data_directory(&document, home_directory.as_deref(), &environment),
        project_root,
        mcp_servers,
        mcp_status: McpStatusHandle::default(),
        permission_rules,
        config_reader: Arc::clone(&host.read_file),
        paths,
        global_loaded,
        project_loaded,
    })
}

pub fn discover_project_root(current_directory: &Path) -> Option<PathBuf> {
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
    host: &HostEnvironment,
) -> Result<(toml::Table, bool), CliError> {
    let Some(contents) = (host.read_file)(path)? else {
        return Ok((toml::Table::new(), false));
    };

    let document = parse_toml_document(&contents)
        .map_err(|_| CliError::configuration(format!("{scope} configuration is invalid")))?;
    validate_toml_document(&document).map_err(|error| {
        if error.retired() {
            CliError::configuration(format!("{scope} configuration: {error}"))
        } else {
            CliError::configuration(format!("{scope} configuration is invalid"))
        }
    })?;

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

/// The environment variable each API-key provider reads its credential from.
/// `openai-chatgpt` is absent on purpose: it authenticates through OAuth, not a
/// key.
const API_KEY_ENVIRONMENT: [(&str, &str); 2] = [
    ("openai-api", "OPENAI_API_KEY"),
    ("moonshotai", "MOONSHOT_API_KEY"),
];

/// Resolves one provider's API key, environment first and stored credential
/// second. Keyed by provider so a key configured for one provider can never
/// authenticate a run against another.
pub fn provider_api_key(
    provider: &str,
    credentials: Option<&str>,
    environment: &BTreeMap<String, String>,
) -> Option<String> {
    let variable = API_KEY_ENVIRONMENT
        .iter()
        .find_map(|(identifier, variable)| (*identifier == provider).then_some(*variable))?;

    environment
        .get(variable)
        .filter(|key| !key.is_empty())
        .cloned()
        .or_else(|| stored_api_key(credentials, provider))
}

fn stored_api_key(credentials: Option<&str>, provider: &str) -> Option<String> {
    credentials
        .and_then(|contents| serde_json::from_str::<serde_json::Value>(contents).ok())
        .and_then(|credentials| {
            credentials
                .get(provider)?
                .get("api_key")?
                .as_str()
                .filter(|key| !key.is_empty())
                .map(ToOwned::to_owned)
        })
}

/// Validates `provider.model` without reducing it.
///
/// The identifier now carries the provider, so the `provider/model` form has to
/// survive to whoever resolves the turn; stripping it here would throw away the
/// only statement of which provider was meant.
fn resolve_configured_model(model: &str) -> Result<String, CliError> {
    agens_models::QualifiedModel::parse(model)
        .map(|parsed| parsed.to_string())
        .map_err(|message| CliError::configuration(format!("provider.model: {message}")))
}

/// The OpenAI API key, resolved through [`provider_api_key`] so it cannot drift
/// from how every other provider's key is found.
pub fn openai_api_key(
    credentials: Option<&str>,
    environment: &BTreeMap<String, String>,
) -> Option<String> {
    provider_api_key("openai-api", credentials, environment)
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

/// Where a run's skills come from: the global catalog beside the global config,
/// and the project's own. Path resolution, which is this crate's job.
pub fn discover_skill_catalog(
    bootstrap: &Bootstrap,
    project_root: &std::path::Path,
) -> Result<agens_tools::SkillDiscovery, CliError> {
    agens_tools::SkillCatalog::discover(
        bootstrap.paths.global_config.with_file_name("skills"),
        project_root.join(agens_tools::PROJECT_SKILLS_DIRECTORY),
    )
    .map_err(|_| CliError::configuration("skill catalog is unavailable"))
}

#[cfg(test)]
mod tests {
    use super::provider_api_key;
    use std::collections::BTreeMap;

    fn environment(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    const CREDENTIALS: &str = r#"{
        "openai-api": {"api_key": "stored-openai"},
        "moonshotai": {"api_key": "stored-moonshot"}
    }"#;

    #[test]
    fn environment_key_wins_over_the_stored_credential() {
        let environment = environment(&[
            ("OPENAI_API_KEY", "env-openai"),
            ("MOONSHOT_API_KEY", "env-moonshot"),
        ]);

        assert_eq!(
            provider_api_key("openai-api", Some(CREDENTIALS), &environment),
            Some("env-openai".to_owned())
        );
        assert_eq!(
            provider_api_key("moonshotai", Some(CREDENTIALS), &environment),
            Some("env-moonshot".to_owned())
        );
    }

    #[test]
    fn stored_credential_is_used_when_the_environment_is_silent() {
        let environment = environment(&[]);

        assert_eq!(
            provider_api_key("openai-api", Some(CREDENTIALS), &environment),
            Some("stored-openai".to_owned())
        );
        assert_eq!(
            provider_api_key("moonshotai", Some(CREDENTIALS), &environment),
            Some("stored-moonshot".to_owned())
        );
    }

    #[test]
    fn one_providers_environment_key_never_answers_for_another() {
        let environment = environment(&[("OPENAI_API_KEY", "env-openai")]);

        assert_eq!(
            provider_api_key("moonshotai", None, &environment),
            None,
            "an OpenAI key must not authenticate a Moonshot run"
        );
    }

    #[test]
    fn a_provider_without_an_api_key_path_resolves_to_nothing() {
        let environment = environment(&[("OPENAI_API_KEY", "env-openai")]);

        assert_eq!(
            provider_api_key("openai-chatgpt", Some(CREDENTIALS), &environment),
            None
        );
    }
}
