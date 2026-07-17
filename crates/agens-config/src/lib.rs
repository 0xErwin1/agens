use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

pub struct ConfigPaths {
    pub global_config: PathBuf,
    pub credentials: PathBuf,
    pub project_config: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub timeout_ms: u64,
}

pub fn mcp_stdio_servers(
    document: &toml::Table,
) -> Result<Vec<McpServerConfig>, ConfigValidationError> {
    validate_mcp(document)?;
    let Some(servers) = document.get("mcp").and_then(toml::Value::as_table) else {
        return Ok(Vec::new());
    };
    servers
        .iter()
        .map(|(name, value)| {
            let server = value.as_table().ok_or_else(|| invalid_field("mcp", name))?;
            if server.get("transport").and_then(toml::Value::as_str) != Some("stdio") {
                return Err(invalid_field("mcp", name));
            }
            if server.contains_key("cwd") {
                return Err(invalid_field(&format!("mcp.{name}"), "cwd"));
            }
            let command = server
                .get("command")
                .and_then(toml::Value::as_str)
                .filter(|command| !command.trim().is_empty() && !command.contains('\0'))
                .ok_or_else(|| invalid_field(&format!("mcp.{name}"), "command"))?;
            let args = server
                .get("args")
                .and_then(toml::Value::as_array)
                .map_or_else(Vec::new, |args| {
                    args.iter()
                        .filter_map(toml::Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                });
            let environment = server
                .get("env")
                .and_then(toml::Value::as_table)
                .map_or_else(BTreeMap::new, |env| {
                    env.iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_owned()))
                        })
                        .collect()
                });
            let timeout_ms = server
                .get("timeout_ms")
                .and_then(toml::Value::as_integer)
                .and_then(|timeout| u64::try_from(timeout).ok())
                .filter(|timeout| *timeout > 0)
                .ok_or_else(|| invalid_field(&format!("mcp.{name}"), "timeout_ms"))?;
            if name.is_empty()
                || name.contains("::")
                || args.iter().any(|arg| arg.contains('\0'))
                || environment.iter().any(|(key, value)| {
                    key.is_empty()
                        || key.contains('=')
                        || key.contains('\0')
                        || value.contains('\0')
                })
            {
                return Err(invalid_field("mcp", name));
            }
            Ok(McpServerConfig {
                name: name.clone(),
                command: PathBuf::from(command),
                args,
                environment,
                timeout_ms,
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentExpansionError {
    MissingVariable(String),
    UnterminatedExpression,
    InvalidVariable(String),
}

impl fmt::Display for EnvironmentExpansionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingVariable(name) => {
                write!(formatter, "environment variable {name:?} is not set")
            }
            Self::UnterminatedExpression => {
                formatter.write_str("unterminated environment expression")
            }
            Self::InvalidVariable(name) => {
                write!(formatter, "invalid environment variable {name:?}")
            }
        }
    }
}

impl std::error::Error for EnvironmentExpansionError {}

pub fn parse_toml_document(input: &str) -> Result<toml::Table, toml::de::Error> {
    input.parse()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValidationError {
    field: String,
}

impl fmt::Display for ConfigValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid configuration field {}", self.field)
    }
}

impl std::error::Error for ConfigValidationError {}

pub fn validate_toml_document(document: &toml::Table) -> Result<(), ConfigValidationError> {
    reject_unknown_fields(
        document,
        "",
        &["options", "provider", "agent", "ui", "mcp", "permissions"],
    )?;

    validate_named_table(
        document,
        "options",
        &["debug", "data_dir"],
        |table, path| {
            validate_optional(table, "debug", path, toml::Value::is_bool)?;
            validate_optional(table, "data_dir", path, toml::Value::is_str)
        },
    )?;
    validate_named_table(
        document,
        "provider",
        &["type", "model", "base_url"],
        |table, path| {
            validate_optional(table, "type", path, toml::Value::is_str)?;
            validate_optional(table, "model", path, toml::Value::is_str)?;
            validate_optional(table, "base_url", path, toml::Value::is_str)
        },
    )?;
    validate_named_table(
        document,
        "agent",
        &["system_prompt", "max_iterations", "parallel_tool_calls"],
        |table, path| {
            validate_optional(table, "system_prompt", path, toml::Value::is_str)?;
            validate_optional(table, "max_iterations", path, toml::Value::is_integer)?;
            validate_optional(table, "parallel_tool_calls", path, toml::Value::is_bool)
        },
    )?;
    validate_named_table(
        document,
        "ui",
        &["collapse_thinking", "truncate_tool_output"],
        |table, path| {
            validate_optional(table, "collapse_thinking", path, toml::Value::is_bool)?;
            validate_optional(table, "truncate_tool_output", path, toml::Value::is_bool)
        },
    )?;
    validate_named_table(
        document,
        "permissions",
        &["allow", "deny"],
        |table, path| {
            validate_optional(table, "allow", path, is_string_array)?;
            validate_optional(table, "deny", path, is_string_array)
        },
    )?;
    validate_mcp(document)
}

pub fn merge_toml_documents(mut global: toml::Table, project: toml::Table) -> toml::Table {
    merge_tables(&mut global, project);
    global
}

pub fn expand_environment(
    input: &str,
    environment: &BTreeMap<String, String>,
) -> Result<String, EnvironmentExpansionError> {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();

    while let Some(character) = characters.next() {
        if character != '$' {
            output.push(character);
            continue;
        }

        match characters.peek() {
            Some('{') => {
                characters.next();
                let expression = read_braced_expression(&mut characters)?;
                output.push_str(&expand_braced_expression(&expression, environment)?);
            }
            Some(character) if is_variable_start(*character) => {
                let mut name = String::new();
                while let Some(character) = characters.peek() {
                    if !is_variable_part(*character) {
                        break;
                    }
                    name.push(*character);
                    characters.next();
                }
                output.push_str(
                    environment
                        .get(&name)
                        .ok_or(EnvironmentExpansionError::MissingVariable(name))?,
                );
            }
            _ => output.push('$'),
        }
    }

    Ok(output)
}

pub fn resolve_paths(
    project_root: &Path,
    home_directory: Option<&Path>,
    environment: &BTreeMap<String, String>,
) -> ConfigPaths {
    let config_home = resolve_config_home(home_directory, environment);

    ConfigPaths {
        global_config: config_home.join("config.toml"),
        credentials: config_home.join("auth.json"),
        project_config: project_root.join(".agens/config.toml"),
    }
}

fn merge_tables(global: &mut toml::Table, project: toml::Table) {
    for (key, project_value) in project {
        if let (Some(toml::Value::Table(global_table)), toml::Value::Table(project_table)) =
            (global.get_mut(&key), &project_value)
        {
            merge_tables(global_table, project_table.clone());
            continue;
        }

        global.insert(key, project_value);
    }
}

fn reject_unknown_fields(
    table: &toml::Table,
    path: &str,
    allowed_fields: &[&str],
) -> Result<(), ConfigValidationError> {
    for field in table.keys() {
        if !allowed_fields.contains(&field.as_str()) {
            return Err(invalid_field(path, field));
        }
    }

    Ok(())
}

fn validate_named_table(
    document: &toml::Table,
    name: &str,
    allowed_fields: &[&str],
    validate: impl FnOnce(&toml::Table, &str) -> Result<(), ConfigValidationError>,
) -> Result<(), ConfigValidationError> {
    let Some(value) = document.get(name) else {
        return Ok(());
    };
    let table = value.as_table().ok_or_else(|| invalid_field("", name))?;

    reject_unknown_fields(table, name, allowed_fields)?;
    validate(table, name)
}

fn validate_optional(
    table: &toml::Table,
    name: &str,
    path: &str,
    predicate: impl FnOnce(&toml::Value) -> bool,
) -> Result<(), ConfigValidationError> {
    match table.get(name) {
        Some(value) if !predicate(value) => Err(invalid_field(path, name)),
        _ => Ok(()),
    }
}

fn validate_mcp(document: &toml::Table) -> Result<(), ConfigValidationError> {
    let Some(value) = document.get("mcp") else {
        return Ok(());
    };
    let servers = value.as_table().ok_or_else(|| invalid_field("", "mcp"))?;

    for (name, value) in servers {
        let path = format!("mcp.{name}");
        let server = value.as_table().ok_or_else(|| invalid_field("mcp", name))?;
        reject_unknown_fields(
            server,
            &path,
            &[
                "transport",
                "command",
                "args",
                "env",
                "cwd",
                "url",
                "headers",
                "max_retries",
                "timeout_ms",
            ],
        )?;
        validate_optional(server, "transport", &path, toml::Value::is_str)?;
        validate_optional(server, "command", &path, toml::Value::is_str)?;
        validate_optional(server, "args", &path, is_string_array)?;
        validate_optional(server, "env", &path, is_string_table)?;
        validate_optional(server, "cwd", &path, toml::Value::is_str)?;
        validate_optional(server, "url", &path, toml::Value::is_str)?;
        validate_optional(server, "headers", &path, is_string_table)?;
        validate_optional(server, "max_retries", &path, toml::Value::is_integer)?;
        validate_optional(server, "timeout_ms", &path, toml::Value::is_integer)?;
    }

    Ok(())
}

fn is_string_array(value: &toml::Value) -> bool {
    value
        .as_array()
        .is_some_and(|values| values.iter().all(toml::Value::is_str))
}

fn is_string_table(value: &toml::Value) -> bool {
    value
        .as_table()
        .is_some_and(|values| values.values().all(toml::Value::is_str))
}

fn invalid_field(path: &str, field: &str) -> ConfigValidationError {
    let field = if path.is_empty() {
        field.to_owned()
    } else {
        format!("{path}.{field}")
    };

    ConfigValidationError { field }
}

fn read_braced_expression(
    characters: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<String, EnvironmentExpansionError> {
    let mut expression = String::new();

    for character in characters.by_ref() {
        if character == '}' {
            return Ok(expression);
        }
        expression.push(character);
    }

    Err(EnvironmentExpansionError::UnterminatedExpression)
}

fn expand_braced_expression(
    expression: &str,
    environment: &BTreeMap<String, String>,
) -> Result<String, EnvironmentExpansionError> {
    let (name, fallback) = expression
        .split_once(":-")
        .map_or((expression, None), |(name, fallback)| {
            (name, Some(fallback))
        });

    if !is_variable_name(name) {
        return Err(EnvironmentExpansionError::InvalidVariable(name.to_owned()));
    }

    match environment.get(name) {
        Some(value) => Ok(value.clone()),
        None => fallback
            .map(str::to_owned)
            .ok_or_else(|| EnvironmentExpansionError::MissingVariable(name.to_owned())),
    }
}

fn is_variable_name(name: &str) -> bool {
    let mut characters = name.chars();

    matches!(characters.next(), Some(character) if is_variable_start(character))
        && characters.all(is_variable_part)
}

fn is_variable_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

fn is_variable_part(character: char) -> bool {
    is_variable_start(character) || character.is_numeric()
}

fn resolve_config_home(
    home_directory: Option<&Path>,
    environment: &BTreeMap<String, String>,
) -> PathBuf {
    if let Some(path) = environment
        .get("AGENS_CONFIG_HOME")
        .filter(|path| !path.is_empty())
    {
        return PathBuf::from(path);
    }

    if let Some(path) = environment
        .get("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
    {
        return PathBuf::from(path).join("agens");
    }

    match home_directory.filter(|path| !path.as_os_str().is_empty()) {
        Some(path) => path.join(".config/agens"),
        None => PathBuf::from(".agens"),
    }
}
