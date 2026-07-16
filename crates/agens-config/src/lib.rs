use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

pub struct ConfigPaths {
    pub global_config: PathBuf,
    pub credentials: PathBuf,
    pub project_config: PathBuf,
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
