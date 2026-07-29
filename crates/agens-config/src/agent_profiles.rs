use std::collections::BTreeMap;
use std::fmt;

use toml_edit::{DocumentMut, Item, Table, value as toml_value};

use crate::{
    ConfigValidationError, invalid_field, parse_toml_document, reject_unknown_fields,
    validate_toml_document,
};

const EFFORT_VALUES: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentProfile {
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentProfilePatch {
    pub model: Option<Option<String>>,
    pub effort: Option<Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentProfileEditError(String);

impl fmt::Display for AgentProfileEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AgentProfileEditError {}

pub fn apply_agent_profile_patch(
    input: &str,
    name: &str,
    patch: &AgentProfilePatch,
) -> Result<String, AgentProfileEditError> {
    let mut document = input
        .parse::<DocumentMut>()
        .map_err(|error| AgentProfileEditError(error.to_string()))?;
    let root = document.as_table_mut();
    if !root.contains_key("agents") {
        let mut agents = Table::new();
        agents.set_implicit(true);
        root.insert("agents", Item::Table(agents));
    }
    let agents = root
        .get_mut("agents")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| AgentProfileEditError("invalid configuration field agents".to_owned()))?;
    if !agents.contains_key(name) {
        agents.insert(name, Item::Table(Table::new()));
    }
    let profile = agents
        .get_mut(name)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| {
            AgentProfileEditError(format!("invalid configuration field agents.{name}"))
        })?;

    apply_profile_field(profile, "model", &patch.model);
    apply_profile_field(profile, "effort", &patch.effort);

    let output = document.to_string();
    let parsed =
        parse_toml_document(&output).map_err(|error| AgentProfileEditError(error.to_string()))?;
    validate_toml_document(&parsed).map_err(|error| AgentProfileEditError(error.to_string()))?;
    Ok(output)
}

fn apply_profile_field(table: &mut Table, key: &str, patch: &Option<Option<String>>) {
    match patch {
        Some(Some(value)) => {
            table.insert(key, toml_value(value));
        }
        Some(None) => {
            table.remove(key);
        }
        None => {}
    }
}

pub fn parse_agent_profiles(
    document: &toml::Table,
) -> Result<BTreeMap<String, AgentProfile>, ConfigValidationError> {
    validate_agent_profiles(document)?;

    let Some(agents) = document.get("agents") else {
        return Ok(BTreeMap::new());
    };
    let agents = agents
        .as_table()
        .ok_or_else(|| invalid_field("", "agents"))?;

    agents
        .iter()
        .map(|(name, value)| {
            let profile = value
                .as_table()
                .ok_or_else(|| invalid_field("agents", name))?;
            Ok((
                name.clone(),
                AgentProfile {
                    model: profile
                        .get("model")
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned),
                    effort: profile
                        .get("effort")
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned),
                },
            ))
        })
        .collect()
}

pub(crate) fn validate_agent_profiles(document: &toml::Table) -> Result<(), ConfigValidationError> {
    let Some(agents) = document.get("agents") else {
        return Ok(());
    };
    let agents = agents
        .as_table()
        .ok_or_else(|| invalid_field("", "agents"))?;

    for (name, value) in agents {
        let path = format!("agents.{name}");
        let profile = value
            .as_table()
            .ok_or_else(|| invalid_field("agents", name))?;
        reject_unknown_fields(profile, &path, &["model", "effort"])?;

        if profile.get("model").is_some_and(|model| !model.is_str()) {
            return Err(invalid_field(&path, "model"));
        }
        if profile.get("effort").is_some_and(|effort| {
            !effort
                .as_str()
                .is_some_and(|effort| EFFORT_VALUES.contains(&effort))
        }) {
            return Err(invalid_field(&path, "effort"));
        }
    }

    Ok(())
}
