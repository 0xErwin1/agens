use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use agens_core::{
    AgentDefinition, AgentMode, PermissionDecision, PermissionPattern, PermissionRule,
};

use crate::markdown::{self, FrontmatterValue, MarkdownDocument};

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
        let mut discovery = AgentDiscovery::default();
        load_built_ins(built_ins, &mut discovery);
        load_root(global_root, &mut discovery)?;
        load_root(project_root, &mut discovery)?;
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
            .filter(|agent| agent.mode == AgentMode::Subagent)
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

fn load_built_ins(built_ins: &[AgentDefinition], discovery: &mut AgentDiscovery) {
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
        discovery.catalog.insert(agent.clone(), source);
    }
}

fn load_root(root: &Path, discovery: &mut AgentDiscovery) -> Result<(), String> {
    if std::fs::symlink_metadata(root)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(());
    }
    let root = markdown::load_root(root)?;
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
    for (_, document) in candidates {
        match parse_agent(&document) {
            Ok(agent) => {
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
        system_prompt: document.parsed().body().trim().into(),
        permission_rules: permissions,
        skills,
    };
    agent
        .validate()
        .map_err(|error| format!("invalid agent definition: {error:?}"))?;
    Ok(agent)
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
