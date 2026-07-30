use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use agens_core::{
    AgentDefinition, AgentMode, PermissionDecision, PermissionPattern, PermissionRule,
    ReasoningEffort, RequestConfig,
};

use crate::markdown::{self, FrontmatterValue, MarkdownDocument};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentModelValidationError {
    Unavailable,
}

pub trait AgentModelValidator {
    fn validate_model(&self, model: &str) -> Result<(), AgentModelValidationError>;
}

struct AllowAllModels;

impl AgentModelValidator for AllowAllModels {
    fn validate_model(&self, _: &str) -> Result<(), AgentModelValidationError> {
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentCatalog {
    agents: Vec<AgentDefinition>,
    positions: BTreeMap<String, usize>,
    sources: BTreeMap<String, PathBuf>,
}

impl AgentCatalog {
    pub fn discover(
        built_ins: &[AgentDefinition],
        global_root: &Path,
        project_root: &Path,
    ) -> Result<AgentDiscovery, String> {
        Self::discover_with_model_validator(built_ins, global_root, project_root, &AllowAllModels)
    }

    pub fn discover_with_model_validator(
        built_ins: &[AgentDefinition],
        global_root: &Path,
        project_root: &Path,
        validator: &dyn AgentModelValidator,
    ) -> Result<AgentDiscovery, String> {
        let mut discovery = AgentDiscovery::default();
        load_built_ins(built_ins, validator, &mut discovery);
        load_root(global_root, validator, &mut discovery)?;
        load_root(project_root, validator, &mut discovery)?;
        Ok(discovery)
    }

    pub fn agent(&self, name: &str) -> Option<&AgentDefinition> {
        self.positions.get(name).map(|index| &self.agents[*index])
    }

    pub fn primary_or_all(&self) -> impl Iterator<Item = &AgentDefinition> {
        self.agents
            .iter()
            .filter(|agent| agent.mode != AgentMode::Subagent)
    }

    pub fn subagents(&self) -> impl Iterator<Item = &AgentDefinition> {
        self.agents
            .iter()
            .filter(|agent| agent.mode != AgentMode::Primary)
    }

    /// Appends `instructions` to every agent's OWN system prompt, whatever that prompt's origin
    /// (built-in fallback, `agent.system_prompt` config value, or a markdown definition's body).
    /// A no-op when `instructions` is empty. Agent positions and sources are unchanged.
    pub fn with_appended_instructions(mut self, instructions: &str) -> Self {
        if instructions.is_empty() {
            return self;
        }
        for agent in &mut self.agents {
            agent.system_prompt = format!("{}\n\n{instructions}", agent.system_prompt);
        }
        self
    }

    pub fn map_agents(&self, map: impl Fn(&AgentDefinition) -> AgentDefinition) -> Self {
        let mut catalog = self.clone();
        catalog.agents = self.agents.iter().map(map).collect();
        catalog
    }

    fn insert(&mut self, agent: AgentDefinition, source: PathBuf) -> Option<PathBuf> {
        if let Some(index) = self.positions.get(&agent.name).copied() {
            self.agents[index] = agent;
            return self.sources.insert(self.agents[index].name.clone(), source);
        }
        self.positions.insert(agent.name.clone(), self.agents.len());
        self.sources.insert(agent.name.clone(), source);
        self.agents.push(agent);
        None
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentDiscovery {
    catalog: AgentCatalog,
    diagnostics: Vec<AgentDiagnostic>,
    shadowed: Vec<AgentShadow>,
}

impl AgentDiscovery {
    pub fn catalog(&self) -> &AgentCatalog {
        &self.catalog
    }
    pub fn diagnostics(&self) -> &[AgentDiagnostic] {
        &self.diagnostics
    }
    pub fn shadowed(&self) -> &[AgentShadow] {
        &self.shadowed
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDiagnostic {
    path: PathBuf,
    message: String,
}

impl AgentDiagnostic {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentShadow {
    name: String,
    replaced: PathBuf,
    replacement: PathBuf,
}

impl AgentShadow {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn replaced(&self) -> &Path {
        &self.replaced
    }
    pub fn replacement(&self) -> &Path {
        &self.replacement
    }
}

fn load_built_ins(
    built_ins: &[AgentDefinition],
    validator: &dyn AgentModelValidator,
    discovery: &mut AgentDiscovery,
) {
    let mut names = BTreeMap::<String, usize>::new();
    for agent in built_ins {
        *names.entry(agent.name.clone()).or_default() += 1;
    }
    for agent in built_ins {
        let source = PathBuf::from(format!("<built-in:{}>", agent.name));
        if names[&agent.name] != 1 || agent.validate().is_err() {
            discovery
                .diagnostics
                .push(diagnostic(source, "invalid or duplicate built-in agent"));
            continue;
        }
        if agent
            .model
            .as_deref()
            .is_some_and(|model| validator.validate_model(model).is_err())
        {
            discovery
                .diagnostics
                .push(diagnostic(source.clone(), "agent model is unavailable"));
        }
        discovery.catalog.insert(agent.clone(), source);
    }
}

fn load_root(
    root: &Path,
    validator: &dyn AgentModelValidator,
    discovery: &mut AgentDiscovery,
) -> Result<(), String> {
    if std::fs::symlink_metadata(root)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(());
    }
    let root =
        markdown::load_root_with_definition_limit(root, markdown::MAX_MARKDOWN_ROOT_ENTRIES)?;
    discovery.diagnostics.extend(
        root.diagnostics
            .into_iter()
            .map(|item| diagnostic(item.path().into(), item.message())),
    );
    let mut candidates = BTreeMap::<String, MarkdownDocument>::new();
    for document in root.documents {
        if candidates
            .insert(document.name().into(), document.clone())
            .is_some()
        {
            discovery.diagnostics.push(diagnostic(
                document.source().into(),
                "duplicate agent name in the same root",
            ));
        }
    }
    let mut accepted = 0;
    let mut definition_limit_reported = false;
    for (_, document) in candidates {
        match parse_agent(&document) {
            Ok(agent) => {
                if accepted == markdown::MAX_MARKDOWN_DEFINITIONS {
                    if !definition_limit_reported {
                        discovery.diagnostics.push(diagnostic(
                            document.source().into(),
                            "accepted agent definition limit exceeded",
                        ));
                        definition_limit_reported = true;
                    }
                    continue;
                }
                accepted += 1;
                if agent
                    .model
                    .as_deref()
                    .is_some_and(|model| validator.validate_model(model).is_err())
                {
                    discovery.diagnostics.push(diagnostic(
                        document.source().into(),
                        "agent model is unavailable",
                    ));
                }
                if let Some(previous) = discovery
                    .catalog
                    .insert(agent.clone(), document.source().into())
                {
                    discovery.shadowed.push(AgentShadow {
                        name: agent.name,
                        replaced: previous,
                        replacement: document.source().into(),
                    });
                }
            }
            Err(message) => discovery
                .diagnostics
                .push(diagnostic(document.source().into(), message)),
        }
    }
    Ok(())
}

fn parse_agent(document: &MarkdownDocument) -> Result<AgentDefinition, String> {
    let field = |name| {
        document
            .parsed()
            .field(name)
            .ok_or_else(|| format!("agent field {name} is required"))
    };
    let scalar = |name| match field(name)? {
        FrontmatterValue::Scalar(value) => Ok(value.clone()),
        _ => Err(format!("agent field {name} must be a string")),
    };
    let mode = match scalar("mode")?.as_str() {
        "primary" => AgentMode::Primary,
        "subagent" => AgentMode::Subagent,
        "all" => AgentMode::All,
        _ => return Err("agent mode must be primary, subagent, or all".into()),
    };
    let model = match document.parsed().field("model") {
        Some(FrontmatterValue::Scalar(value)) => Some(value.clone()),
        Some(_) => return Err("agent field model must be a string".into()),
        None => None,
    };
    let reasoning_effort = match document.parsed().field("effort") {
        Some(FrontmatterValue::Scalar(value)) => parse_effort(value)?,
        Some(_) => return Err("agent field effort must be a string".into()),
        None => None,
    };
    let skills = list(document, "skills")?;
    let permissions = list(document, "permissions")?
        .iter()
        .map(|rule| permission(rule))
        .collect::<Result<_, _>>()?;
    let name = scalar("name")?;
    if name != document.name() {
        return Err("agent name must match its canonical filename".into());
    }
    let agent = AgentDefinition {
        name,
        description: scalar("description")?,
        mode,
        model,
        reasoning_effort,
        system_prompt: document.parsed().body().trim().into(),
        permission_rules: permissions,
        skills,
    };
    agent
        .validate()
        .map_err(|error| format!("invalid agent definition: {error:?}"))?;
    Ok(agent)
}

fn parse_effort(value: &str) -> Result<Option<ReasoningEffort>, String> {
    RequestConfig::with_reasoning_effort(value)
        .map(|config| config.reasoning_effort())
        .map_err(|_| "agent field effort is unsupported".to_owned())
}

fn list(document: &MarkdownDocument, name: &str) -> Result<Vec<String>, String> {
    match document.parsed().field(name) {
        Some(FrontmatterValue::List(values)) => Ok(values.clone()),
        Some(_) => Err(format!("agent field {name} must be a string list")),
        None => Ok(vec![]),
    }
}

fn permission(rule: &str) -> Result<PermissionRule, String> {
    let mut parts = rule.split_whitespace();
    let decision = match parts.next() {
        Some("allow") => PermissionDecision::Allow,
        Some("deny") => PermissionDecision::Deny,
        Some("ask") => PermissionDecision::Ask,
        _ => return Err("permission must begin with allow, deny, or ask".into()),
    };
    let tool = parts.next().ok_or("permission tool is required")?;
    let target = parts.next();
    if parts.next().is_some() {
        return Err("permission must contain decision, tool, and optional target".into());
    }
    Ok(PermissionRule::global(
        decision,
        PermissionPattern::glob(tool).map_err(|_| "invalid permission tool")?,
        match target {
            Some(target) => {
                PermissionPattern::glob(target).map_err(|_| "invalid permission target")?
            }
            None => PermissionPattern::Any,
        },
    ))
}

fn diagnostic(path: PathBuf, message: impl Into<String>) -> AgentDiagnostic {
    AgentDiagnostic {
        path,
        message: message.into(),
    }
}

#[cfg(test)]
mod with_appended_instructions_tests {
    use super::*;

    fn agent(name: &str, mode: AgentMode, system_prompt: &str) -> AgentDefinition {
        AgentDefinition {
            name: name.into(),
            description: format!("{name} description"),
            mode,
            model: None,
            system_prompt: system_prompt.into(),
            permission_rules: Vec::new(),
            skills: Vec::new(),
            reasoning_effort: None,
        }
    }

    fn sample_catalog() -> AgentCatalog {
        let mut catalog = AgentCatalog::default();
        catalog.insert(
            agent("primary", AgentMode::Primary, "PRIMARY-PROMPT"),
            PathBuf::from("<built-in:primary>"),
        );
        catalog.insert(
            agent("explore", AgentMode::Subagent, "EXPLORE-PROMPT"),
            PathBuf::from("<built-in:explore>"),
        );
        catalog.insert(
            agent("general", AgentMode::Subagent, "GENERAL-PROMPT"),
            PathBuf::from("<built-in:general>"),
        );
        catalog.insert(
            agent("reviewer", AgentMode::All, "REVIEWER-PROMPT"),
            PathBuf::from("/project/.agens/agents/reviewer.md"),
        );
        catalog
    }

    #[test]
    fn appends_the_instructions_to_every_agents_own_distinct_prompt() {
        let catalog = sample_catalog();
        let positions_before = catalog.positions.clone();
        let sources_before = catalog.sources.clone();

        let appended = catalog.with_appended_instructions("INSTRUCTIONS-TEXT");

        for (name, own_prompt) in [
            ("primary", "PRIMARY-PROMPT"),
            ("explore", "EXPLORE-PROMPT"),
            ("general", "GENERAL-PROMPT"),
            ("reviewer", "REVIEWER-PROMPT"),
        ] {
            let system_prompt = &appended.agent(name).unwrap().system_prompt;
            assert_eq!(
                *system_prompt,
                format!("{own_prompt}\n\nINSTRUCTIONS-TEXT"),
                "{name}'s own prompt must be preserved with the instructions appended after it"
            );
        }
        assert_eq!(
            appended.positions, positions_before,
            "appending instructions must not change agent positions"
        );
        assert_eq!(
            appended.sources, sources_before,
            "appending instructions must not change agent sources"
        );
    }

    #[test]
    fn empty_instructions_is_a_no_op() {
        let catalog = sample_catalog();

        let unchanged = catalog.clone().with_appended_instructions("");

        assert_eq!(unchanged, catalog);
    }
}
