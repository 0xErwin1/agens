use std::collections::BTreeMap;

use crate::{ConfigValidationError, invalid_field, reject_unknown_fields};

const EFFORT_VALUES: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentProfile {
    pub model: Option<String>,
    pub effort: Option<String>,
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
