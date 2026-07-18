use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigPermissionDecision {
    Allow,
    Deny,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigPermissionScope {
    Global,
    Project,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ConfigPermissionRule {
    pub scope: ConfigPermissionScope,
    pub decision: ConfigPermissionDecision,
    pub tool_pattern: String,
    pub target_pattern: Option<String>,
}

impl fmt::Debug for ConfigPermissionRule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigPermissionRule")
            .field("scope", &self.scope)
            .field("decision", &self.decision)
            .field("has_tool_pattern", &true)
            .field("has_target_pattern", &self.target_pattern.is_some())
            .finish()
    }
}

pub struct ConfigPaths {
    pub global_config: PathBuf,
    pub credentials: PathBuf,
    pub project_config: PathBuf,
}

pub const DEFAULT_MCP_TIMEOUT_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
}

#[derive(Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    pub command: Option<PathBuf>,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub max_retries: u32,
    pub timeout_ms: u64,
}

impl fmt::Debug for McpServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("command", &self.command)
            .field("args_count", &self.args.len())
            .field(
                "environment_keys",
                &self.environment.keys().collect::<Vec<_>>(),
            )
            .field("environment_count", &self.environment.len())
            .field("cwd", &self.cwd)
            .field("url", &self.url)
            .field("header_keys", &self.headers.keys().collect::<Vec<_>>())
            .field("max_retries", &self.max_retries)
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

pub fn mcp_stdio_servers(
    document: &toml::Table,
) -> Result<Vec<McpServerConfig>, ConfigValidationError> {
    Ok(mcp_servers(document)?
        .into_iter()
        .filter(|server| server.transport == McpTransport::Stdio)
        .collect())
}

pub fn mcp_servers(document: &toml::Table) -> Result<Vec<McpServerConfig>, ConfigValidationError> {
    validate_mcp(document)?;
    let Some(servers) = document.get("mcp").and_then(toml::Value::as_table) else {
        return Ok(Vec::new());
    };
    servers
        .iter()
        .map(|(name, value)| {
            let server = value.as_table().ok_or_else(|| invalid_field("mcp", name))?;
            let path = format!("mcp.{name}");
            let transport = match server.get("transport").and_then(toml::Value::as_str) {
                Some("stdio") => McpTransport::Stdio,
                Some("http") => McpTransport::Http,
                Some("sse") => McpTransport::Sse,
                _ => return Err(invalid_field(&path, "transport")),
            };
            let command = server.get("command").and_then(toml::Value::as_str);
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
                .unwrap_or(DEFAULT_MCP_TIMEOUT_MS);
            let url = server
                .get("url")
                .and_then(toml::Value::as_str)
                .map(ToOwned::to_owned);
            let headers = server
                .get("headers")
                .and_then(toml::Value::as_table)
                .map_or_else(BTreeMap::new, |headers| {
                    headers
                        .iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_owned()))
                        })
                        .collect()
                });
            let max_retries = server
                .get("max_retries")
                .and_then(toml::Value::as_integer)
                .and_then(|retries| u32::try_from(retries).ok())
                .ok_or_else(|| invalid_field(&path, "max_retries"))
                .or_else(|_| {
                    if server.contains_key("max_retries") {
                        Err(invalid_field(&path, "max_retries"))
                    } else {
                        Ok(0)
                    }
                })?;
            let cwd = server
                .get("cwd")
                .and_then(toml::Value::as_str)
                .map(PathBuf::from);
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
            match transport {
                McpTransport::Stdio => {
                    if command
                        .is_none_or(|command| command.trim().is_empty() || command.contains('\0'))
                        || url.is_some()
                        || !headers.is_empty()
                        || max_retries != 0
                    {
                        return Err(invalid_field("mcp", name));
                    }
                }
                McpTransport::Http => {
                    if url.as_deref().is_none_or(|url| url.trim().is_empty())
                        || command.is_some()
                        || !args.is_empty()
                        || !environment.is_empty()
                        || cwd.is_some()
                    {
                        return Err(invalid_field("mcp", name));
                    }
                }
                McpTransport::Sse => {
                    if url.as_deref().is_none_or(|url| url.trim().is_empty())
                        || command.is_some()
                        || !args.is_empty()
                        || !environment.is_empty()
                        || cwd.is_some()
                    {
                        return Err(invalid_field("mcp", name));
                    }
                }
            }
            Ok(McpServerConfig {
                name: name.clone(),
                transport,
                command: command.map(PathBuf::from),
                args,
                environment,
                cwd,
                url,
                headers,
                max_retries,
                timeout_ms,
            })
        })
        .collect()
}

pub fn extract_permission_rules(
    global: &toml::Table,
    project: &toml::Table,
) -> Result<Vec<ConfigPermissionRule>, ConfigValidationError> {
    validate_toml_document(global)?;
    validate_toml_document(project)?;

    let mut rules = Vec::new();

    extract_scoped_permission_rules(global, ConfigPermissionScope::Global, &mut rules)?;
    extract_scoped_permission_rules(project, ConfigPermissionScope::Project, &mut rules)?;

    Ok(rules)
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

/// Expands environment expressions and bounded `$(...)` substitutions for MCP
/// fields only. Command output is inserted literally and is never re-expanded.
pub fn expand_environment_with_commands(
    input: &str,
    environment: &BTreeMap<String, String>,
) -> Result<String, EnvironmentExpansionError> {
    let mut output = String::new();
    let mut remainder = input;
    while let Some(start) = remainder.find("$(") {
        output.push_str(&expand_environment(&remainder[..start], environment)?);
        let command_start = start + 2;
        let Some(end) = remainder[command_start..].find(')') else {
            return Err(EnvironmentExpansionError::UnterminatedExpression);
        };
        let command = &remainder[command_start..command_start + end];
        if command.trim().is_empty() {
            return Err(EnvironmentExpansionError::InvalidVariable(
                "command substitution".into(),
            ));
        }
        output.push_str(&run_command_substitution(command)?);
        remainder = &remainder[command_start + end + 1..];
    }
    output.push_str(&expand_environment(remainder, environment)?);
    Ok(output)
}

fn run_command_substitution(command: &str) -> Result<String, EnvironmentExpansionError> {
    let command = format!("{command} 2>&1");
    let output = Command::new("timeout")
        .args(["2s", "sh", "-c", &command])
        .stdin(Stdio::null())
        .output()
        .map_err(|_| EnvironmentExpansionError::InvalidVariable("command substitution".into()))?;
    if output.status.code() == Some(124) || output.stdout.len() > 64 * 1024 {
        return Err(EnvironmentExpansionError::InvalidVariable(
            "command substitution".into(),
        ));
    }
    if !output.status.success() {
        return Err(EnvironmentExpansionError::InvalidVariable(
            "command substitution".into(),
        ));
    }
    let mut value = String::from_utf8(output.stdout)
        .map_err(|_| EnvironmentExpansionError::InvalidVariable("command substitution".into()))?;
    if value.ends_with('\n') {
        value.pop();
    }
    Ok(value)
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

fn extract_scoped_permission_rules(
    document: &toml::Table,
    scope: ConfigPermissionScope,
    rules: &mut Vec<ConfigPermissionRule>,
) -> Result<(), ConfigValidationError> {
    let Some(permissions) = document.get("permissions").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for (field, decision) in [
        ("allow", ConfigPermissionDecision::Allow),
        ("deny", ConfigPermissionDecision::Deny),
    ] {
        let Some(entries) = permissions.get(field).and_then(toml::Value::as_array) else {
            continue;
        };

        for (index, entry) in entries.iter().enumerate() {
            let value = entry
                .as_str()
                .ok_or_else(|| invalid_field("permissions", field))?;
            let (tool_pattern, target_pattern) = parse_permission_rule(value)
                .ok_or_else(|| invalid_indexed_field("permissions", field, index))?;

            if rules.iter().any(|rule| {
                permission_rules_overlap(rule, &tool_pattern, target_pattern.as_deref())
            }) {
                return Err(invalid_indexed_field("permissions", field, index));
            }

            rules.push(ConfigPermissionRule {
                scope,
                decision,
                tool_pattern,
                target_pattern,
            });
        }
    }

    Ok(())
}

fn parse_permission_rule(value: &str) -> Option<(String, Option<String>)> {
    if value.trim().is_empty() || value.trim() != value {
        return None;
    }

    let (tool_pattern, target_pattern) = match value.split_once('(') {
        Some((tool_pattern, target_pattern)) => {
            let target_pattern = target_pattern.strip_suffix(')')?;
            if target_pattern.is_empty()
                || target_pattern.trim().is_empty()
                || target_pattern.contains(['(', ')'])
            {
                return None;
            }
            (tool_pattern, Some(target_pattern.to_owned()))
        }
        None if !value.contains(')') => (value, None),
        None => return None,
    };

    if !is_grounded_tool_name(tool_pattern)
        || target_pattern
            .as_deref()
            .is_some_and(|target| !is_safe_target_pattern(target))
    {
        return None;
    }

    Some((tool_pattern.to_owned(), target_pattern))
}

fn is_grounded_tool_name(pattern: &str) -> bool {
    matches!(
        pattern,
        "bash" | "read" | "edit" | "write" | "list" | "search" | "webfetch"
    ) || is_mcp_qualified_tool_name(pattern)
}

fn is_mcp_qualified_tool_name(pattern: &str) -> bool {
    let Some((server_name, tool_name)) = pattern.split_once('_') else {
        return false;
    };

    is_ascii_identifier(server_name) && is_ascii_identifier(tool_name)
}

fn is_ascii_identifier(value: &str) -> bool {
    let mut characters = value.bytes();

    matches!(characters.next(), Some(b'a'..=b'z'))
        && characters.all(|character| matches!(character, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

fn is_safe_target_pattern(pattern: &str) -> bool {
    !pattern.is_empty()
        && !pattern.chars().any(is_separator_confusable)
        && !pattern.split('/').any(|segment| segment == "..")
        && has_valid_glob_syntax(pattern)
}

fn is_separator_confusable(character: char) -> bool {
    matches!(
        character,
        '\\' | '\u{2044}'
            | '\u{2215}'
            | '\u{29f5}'
            | '\u{29f6}'
            | '\u{29f7}'
            | '\u{29f8}'
            | '\u{29f9}'
            | '\u{2e4a}'
            | '\u{2f03}'
            | '\u{fe68}'
            | '\u{ff0f}'
            | '\u{ff3c}'
            | '\u{1f67c}'
            | '\u{1f67d}'
    )
}

fn has_valid_glob_syntax(pattern: &str) -> bool {
    let mut in_class = false;
    let mut class_content = false;

    for character in pattern.chars() {
        match character {
            '\\' => return false,
            '[' if !in_class => {
                in_class = true;
                class_content = false;
            }
            '[' => return false,
            ']' if in_class && class_content => {
                in_class = false;
                class_content = false;
            }
            ']' => return false,
            _ if in_class => class_content = true,
            _ => {}
        }
    }

    !in_class
        && pattern.split('[').skip(1).all(|part| {
            part.split_once(']')
                .is_none_or(|(class, _)| is_valid_character_class(class))
        })
}

fn is_valid_character_class(class: &str) -> bool {
    let characters = class.chars().collect::<Vec<_>>();
    let content_start = usize::from(matches!(characters.first(), Some('!' | '^')));

    characters.len() > content_start
        && characters[content_start..]
            .iter()
            .enumerate()
            .all(|(index, character)| {
                *character != '-' || index > 0 && index + 1 < characters.len() - content_start
            })
}

fn permission_rules_overlap(
    existing: &ConfigPermissionRule,
    tool_pattern: &str,
    target_pattern: Option<&str>,
) -> bool {
    if existing.tool_pattern != tool_pattern {
        return false;
    }

    target_patterns_overlap(existing.target_pattern.as_deref(), target_pattern)
}

fn target_patterns_overlap(left: Option<&str>, right: Option<&str>) -> bool {
    let (Some(left), Some(right)) = (left, right) else {
        return true;
    };

    if left == right || left == "**" || right == "**" {
        return true;
    }

    if is_doublestar_prefix(left, right) || is_doublestar_prefix(right, left) {
        return true;
    }

    match (
        contains_glob_metacharacter(left),
        contains_glob_metacharacter(right),
    ) {
        (false, false) => false,
        (true, false) => glob_matches(left, right),
        (false, true) => glob_matches(right, left),
        (true, true) => !glob_patterns_proven_disjoint(left, right),
    }
}

fn is_doublestar_prefix(pattern: &str, candidate: &str) -> bool {
    let Some(prefix) = pattern.strip_suffix("/**") else {
        return false;
    };

    candidate == prefix
        || candidate
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn contains_glob_metacharacter(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

fn glob_patterns_proven_disjoint(left: &str, right: &str) -> bool {
    let left_prefix = literal_glob_prefix(left);
    let right_prefix = literal_glob_prefix(right);

    !left_prefix.starts_with(right_prefix) && !right_prefix.starts_with(left_prefix)
}

fn literal_glob_prefix(pattern: &str) -> &str {
    let prefix_length = pattern
        .char_indices()
        .find_map(|(index, character)| matches!(character, '*' | '?' | '[').then_some(index))
        .unwrap_or(pattern.len());

    &pattern[..prefix_length]
}

fn glob_matches(pattern: &str, candidate: &str) -> bool {
    glob_matches_from(
        &pattern.chars().collect::<Vec<_>>(),
        &candidate.chars().collect::<Vec<_>>(),
        0,
        0,
    )
}

fn glob_matches_from(
    pattern: &[char],
    candidate: &[char],
    pattern_index: usize,
    candidate_index: usize,
) -> bool {
    match pattern.get(pattern_index) {
        None => candidate_index == candidate.len(),
        Some('*') => {
            let crosses_segments = pattern.get(pattern_index + 1) == Some(&'*');

            if crosses_segments && pattern.get(pattern_index + 2) == Some(&'/') {
                return globstar_directory_matches(
                    pattern,
                    candidate,
                    pattern_index + 3,
                    candidate_index,
                );
            }

            let next_index = pattern_index + usize::from(crosses_segments) + 1;
            let maximum = if crosses_segments {
                candidate.len()
            } else {
                candidate[candidate_index..]
                    .iter()
                    .position(|character| *character == '/')
                    .map_or(candidate.len(), |offset| candidate_index + offset)
            };

            (candidate_index..=maximum).any(|next_candidate| {
                glob_matches_from(pattern, candidate, next_index, next_candidate)
            })
        }
        Some('?') => {
            candidate_index < candidate.len()
                && candidate[candidate_index] != '/'
                && glob_matches_from(pattern, candidate, pattern_index + 1, candidate_index + 1)
        }
        Some('[') => {
            let Some(class_end) = pattern[pattern_index + 1..]
                .iter()
                .position(|character| *character == ']')
                .map(|offset| pattern_index + offset + 1)
            else {
                return false;
            };
            let class = &pattern[pattern_index + 1..class_end];

            candidate_index < candidate.len()
                && candidate[candidate_index] != '/'
                && glob_class_matches(class, candidate[candidate_index])
                && glob_matches_from(pattern, candidate, class_end + 1, candidate_index + 1)
        }
        Some(character) => {
            candidate.get(candidate_index) == Some(character)
                && glob_matches_from(pattern, candidate, pattern_index + 1, candidate_index + 1)
        }
    }
}

fn globstar_directory_matches(
    pattern: &[char],
    candidate: &[char],
    next_pattern_index: usize,
    candidate_index: usize,
) -> bool {
    glob_matches_from(pattern, candidate, next_pattern_index, candidate_index)
        || candidate[candidate_index..]
            .iter()
            .enumerate()
            .filter_map(|(offset, character)| {
                (*character == '/').then_some(candidate_index + offset + 1)
            })
            .any(|next_candidate_index| {
                glob_matches_from(pattern, candidate, next_pattern_index, next_candidate_index)
            })
}

fn glob_class_matches(class: &[char], candidate: char) -> bool {
    let (negated, characters) = match class.first() {
        Some('!' | '^') => (true, &class[1..]),
        _ => (false, class),
    };
    let mut index = 0;
    let mut matched = false;

    while index < characters.len() {
        if index + 2 < characters.len() && characters[index + 1] == '-' {
            matched |= characters[index] <= candidate && candidate <= characters[index + 2];
            index += 3;
        } else {
            matched |= characters[index] == candidate;
            index += 1;
        }
    }

    matched != negated
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

fn invalid_indexed_field(path: &str, field: &str, index: usize) -> ConfigValidationError {
    invalid_field(path, &format!("{field}[{index}]"))
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
