use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use agens_core::{
    AgentDefinition, AgentMode, PermissionDecision, PermissionPattern, PermissionRule,
    ReasoningEffort, RequestConfig, permission_target_kind_for_tool,
};

use crate::markdown::{self, FrontmatterValue, MarkdownDocument};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentModelValidationError {
    Unavailable,
    /// The model is real, but only under a provider this session is not using.
    /// Kept distinct from [`Self::Unavailable`] because the two need opposite
    /// corrections: one is a typo, the other is a provider selection.
    ProviderMismatch {
        requested: &'static str,
        active: &'static str,
    },
}

impl AgentModelValidationError {
    /// The user-facing explanation, naming the model and — for a mismatch —
    /// both providers, since the same failure otherwise reads identically
    /// whether the model does not exist or merely lives elsewhere.
    pub fn message(self, model: &str) -> String {
        match self {
            Self::Unavailable => format!("agent model \"{model}\" is unavailable"),
            Self::ProviderMismatch { requested, active } => format!(
                "agent model \"{model}\" is served by provider \"{requested}\", not by this session's \"{active}\""
            ),
        }
    }
}

pub trait AgentModelValidator {
    fn validate_model(&self, model: &str) -> Result<(), AgentModelValidationError>;
}

/// The diagnostic an unusable model produces, or `None` when it validates.
fn model_rejection_message(model: &str, validator: &dyn AgentModelValidator) -> Option<String> {
    validator
        .validate_model(model)
        .err()
        .map(|error| error.message(model))
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
        if let Some(message) = agent
            .model
            .as_deref()
            .and_then(|model| model_rejection_message(model, validator))
        {
            discovery
                .diagnostics
                .push(diagnostic(source.clone(), message));
        }
        for message in permission_diagnostics(agent) {
            discovery
                .diagnostics
                .push(diagnostic(source.clone(), message));
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
                if let Some(message) = agent
                    .model
                    .as_deref()
                    .and_then(|model| model_rejection_message(model, validator))
                {
                    discovery
                        .diagnostics
                        .push(diagnostic(document.source().into(), message));
                }
                for message in permission_diagnostics(&agent) {
                    discovery
                        .diagnostics
                        .push(diagnostic(document.source().into(), message));
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
        model_source: None,
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

/// Parses one `permissions:` entry (`decision tool [target]`) from an
/// agent's frontmatter. Tokens split on whitespace with no quoting, so a
/// target naming a multi-word shell command — `rm -rf /**` — cannot be
/// written here; it has to go in the TOML `[permissions]` block instead,
/// whose `tool(target)` syntax keeps the target intact up to the closing
/// parenthesis. An omitted target defaults to `PermissionPattern::Any`,
/// matching every target for that tool. The target glob's `/`-crossing
/// behavior is chosen by `tool` via `permission_target_kind_for_tool`, so a
/// single-word `bash` target such as `rm*` already crosses `/`.
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
            Some(target) => PermissionPattern::glob_for_target_kind(
                target,
                permission_target_kind_for_tool(tool),
            )
            .map_err(|_| "invalid permission target")?,
            None => PermissionPattern::Any,
        },
    ))
}

#[cfg(test)]
mod permission_parsing_tests {
    use super::*;

    #[test]
    fn a_bare_tool_name_matches_every_target() {
        let rule = permission("deny bash").expect("bare declaration should parse");

        assert_eq!(rule.target, PermissionPattern::Any);
        assert!(rule.target.matches("rm -rf /"));
        assert!(rule.target.matches(""));
    }

    #[test]
    fn a_targeted_tool_name_matches_only_that_glob() {
        let rule = permission("deny bash rm*").expect("targeted declaration should parse");

        assert!(rule.target.matches("rm -rf"));
        assert!(!rule.target.matches("echo hi"));
    }

    /// `bash` is free-form text, not a filesystem path, so a bare `*`
    /// crosses `/` for a declaration parsed from agent frontmatter exactly
    /// as it does for one parsed from the TOML `[permissions]` block.
    #[test]
    fn a_bash_target_glob_crosses_a_slash() {
        let rule = permission("deny bash rm*").expect("targeted declaration should parse");

        assert!(rule.target.matches("rm -rf /tmp/x"));
    }

    /// A path-shaped tool's target glob keeps segment discipline: a bare
    /// `*` never crosses `/`.
    #[test]
    fn a_path_shaped_target_glob_does_not_cross_a_slash() {
        let rule = permission("deny write secret*").expect("targeted declaration should parse");

        assert!(rule.target.matches("secret.env"));
        assert!(!rule.target.matches("dir/secret.env"));
    }

    /// The markdown grammar is `decision tool target`, split on whitespace
    /// with no quoting: it can only ever express a single target token. A
    /// multi-word command pattern such as `rm -rf /**` has to be written in
    /// the TOML `[permissions]` block instead, where `tool(target)` keeps the
    /// target intact up to the closing parenthesis.
    #[test]
    fn a_target_pattern_containing_whitespace_is_rejected_by_the_markdown_grammar() {
        let error = permission("deny bash rm -rf /**").unwrap_err();

        assert_eq!(
            error,
            "permission must contain decision, tool, and optional target"
        );
    }
}

fn diagnostic(path: PathBuf, message: impl Into<String>) -> AgentDiagnostic {
    AgentDiagnostic {
        path,
        message: message.into(),
    }
}

/// Soft, load-time-only checks over an agent's `permission_rules`: neither
/// finding rejects the definition, because both are only ever partial
/// information at load time. A rule matching no native tool might still
/// match an MCP tool discovered later on the primary path; a declared `ask`
/// is only unreachable once this agent is actually delegated as a child.
fn permission_diagnostics(agent: &AgentDefinition) -> Vec<String> {
    let metadata = crate::NativeToolCatalog::metadata();
    let mut diagnostics = Vec::new();

    for rule in &agent.permission_rules {
        let matches_a_native_tool = metadata.iter().any(|entry| {
            let bare = entry
                .qualified_name
                .strip_prefix("native::")
                .unwrap_or(entry.qualified_name.as_str());
            rule.tool.matches(&entry.qualified_name) || rule.tool.matches(bare)
        });
        if !matches_a_native_tool {
            diagnostics.push(format!(
                "permission rule matches no known native tool: {}",
                permission_rule_tool_label(&rule.tool)
            ));
        }
    }

    if agent.mode == AgentMode::Subagent
        && agent
            .permission_rules
            .iter()
            .any(|rule| rule.decision == PermissionDecision::Ask)
    {
        diagnostics.push(
            "a subagent-mode definition declares ask, which is unreachable in a delegated \
             child and resolves to deny"
                .into(),
        );
    }

    diagnostics
}

fn permission_rule_tool_label(pattern: &PermissionPattern) -> String {
    match pattern {
        PermissionPattern::Any => "*".to_owned(),
        PermissionPattern::Exact(value) => value.clone(),
        PermissionPattern::Glob(_) => pattern.glob_source().unwrap_or("*").to_owned(),
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
            model_source: None,
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

#[cfg(test)]
mod permission_diagnostics_tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_CASE: AtomicUsize = AtomicUsize::new(0);

    fn temp_root(name: &str) -> PathBuf {
        let suffix = NEXT_CASE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agens-agent-permission-diagnostics-{name}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_agent(root: &std::path::Path, name: &str, contents: &str) {
        fs::write(root.join(format!("{name}.md")), contents).unwrap();
    }

    #[test]
    fn a_permission_rule_matching_no_known_tool_is_retained_with_a_diagnostic() {
        let root = temp_root("typo");
        write_agent(
            &root,
            "typo-agent",
            "---\nname: typo-agent\ndescription: probe\nmode: subagent\npermissions: [\"deny webfetc\"]\n---\nBody.\n",
        );

        let discovery = AgentCatalog::discover(&[], &PathBuf::from("/nonexistent"), &root).unwrap();

        let agent = discovery
            .catalog()
            .agent("typo-agent")
            .expect("a rule matching no tool must not reject the definition");
        assert_eq!(agent.permission_rules.len(), 1);
        assert!(
            discovery
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message().contains("webfetc")),
            "expected a diagnostic naming the unmatched rule, got: {:?}",
            discovery.diagnostics()
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_subagent_declaring_ask_is_retained_with_a_diagnostic() {
        let root = temp_root("ask");
        write_agent(
            &root,
            "ask-agent",
            "---\nname: ask-agent\ndescription: probe\nmode: subagent\npermissions: [\"ask bash\"]\n---\nBody.\n",
        );

        let discovery = AgentCatalog::discover(&[], &PathBuf::from("/nonexistent"), &root).unwrap();

        let agent = discovery
            .catalog()
            .agent("ask-agent")
            .expect("a declared ask must not reject the definition");
        assert_eq!(agent.permission_rules[0].decision, PermissionDecision::Ask);
        assert!(
            discovery
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message().contains("unreachable")),
            "expected a diagnostic about an unreachable ask, got: {:?}",
            discovery.diagnostics()
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_declared_ask_on_an_all_mode_agent_is_not_flagged() {
        let root = temp_root("ask-all");
        write_agent(
            &root,
            "ask-all-agent",
            "---\nname: ask-all-agent\ndescription: probe\nmode: all\npermissions: [\"ask bash\"]\n---\nBody.\n",
        );

        let discovery = AgentCatalog::discover(&[], &PathBuf::from("/nonexistent"), &root).unwrap();

        assert!(
            discovery
                .diagnostics()
                .iter()
                .all(|diagnostic| !diagnostic.message().contains("unreachable")),
            "an ask reachable on the primary path must not be flagged, got: {:?}",
            discovery.diagnostics()
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_well_formed_declaration_produces_no_diagnostic() {
        let root = temp_root("clean");
        write_agent(
            &root,
            "clean-agent",
            "---\nname: clean-agent\ndescription: probe\nmode: subagent\npermissions: [\"deny bash\"]\n---\nBody.\n",
        );

        let discovery = AgentCatalog::discover(&[], &PathBuf::from("/nonexistent"), &root).unwrap();

        assert!(
            discovery.diagnostics().is_empty(),
            "a well-formed declaration must not produce a diagnostic, got: {:?}",
            discovery.diagnostics()
        );

        fs::remove_dir_all(root).unwrap();
    }
}
