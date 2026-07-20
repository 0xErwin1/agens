use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use agens_core::{
    AgentDefinition, AgentMode, Error, PermissionDecision, PermissionPattern, PermissionRule,
    ToolAccess,
};
use agens_tools::{
    AgentCatalog, AgentModelValidationError, AgentModelValidator, CommandCatalog,
    CommandDefinition, DispatchTool, EffectiveCapabilitySet, SkillCatalog, TaskInvocation,
    TaskRunner, TaskSkill, TaskTool, TaskTurnRequest, ToolDispatcher, ToolExecutionContext,
    ToolOutput,
    markdown::{self, FrontmatterValue, MarkdownRoot},
};
use serde_json::Value;

#[test]
fn parses_scalar_and_list_frontmatter_without_changing_the_body() {
    let document =
        markdown::parse("---\nname: example\nskills:\n  - read\n  - write\n---\n  body remains\n")
            .expect("parse markdown");

    assert_eq!(document.body(), "  body remains\n");
    assert_eq!(
        document.field("name"),
        Some(&FrontmatterValue::Scalar("example".into()))
    );
    assert_eq!(
        document.field("skills"),
        Some(&FrontmatterValue::List(vec!["read".into(), "write".into()]))
    );
}

#[test]
fn rejects_malformed_frontmatter_and_unsafe_definition_names() {
    assert!(markdown::parse("name: missing delimiters\n").is_err());
    assert!(markdown::parse("---\nname: {not: text}\n---\nbody\n").is_err());
    assert!(markdown::canonical_filename("bad--name").is_err());
    assert!(markdown::canonical_filename("../escape").is_err());
    assert_eq!(
        markdown::canonical_filename("valid-name-2").unwrap(),
        "valid-name-2.md"
    );
}

#[test]
fn bounds_and_confines_root_reads_with_isolated_diagnostics() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("valid.md"), "---\nname: valid\n---\nbody\n").unwrap();
    fs::write(root.join("bad.md"), "---\nname: {bad: value}\n---\nbody\n").unwrap();
    fs::write(
        root.join("large.md"),
        "x".repeat(markdown::MAX_MARKDOWN_FILE_BYTES + 1),
    )
    .unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(temporary.path.join("outside.md"), root.join("escape.md")).unwrap();

    let MarkdownRoot {
        documents,
        diagnostics,
    } = markdown::load_root(&root).unwrap();
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0].name(), "valid");
    assert_eq!(diagnostics.len(), if cfg!(unix) { 3 } else { 2 });
}

#[test]
fn stops_at_root_and_accepted_definition_limits() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");
    fs::create_dir_all(&root).unwrap();
    for index in 0..=markdown::MAX_MARKDOWN_ROOT_ENTRIES {
        let name = format!("entry-{index:04}");
        fs::write(
            root.join(format!("{name}.md")),
            format!("---\nname: {name}\n---\nbody\n"),
        )
        .unwrap();
    }

    let MarkdownRoot {
        documents,
        diagnostics,
    } = markdown::load_root(&root).unwrap();
    assert_eq!(documents.len(), markdown::MAX_MARKDOWN_DEFINITIONS);
    assert_eq!(diagnostics.len(), 2);
}

#[test]
fn accepts_exact_definition_limit_while_reporting_later_invalid_entries() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");
    fs::create_dir_all(&root).unwrap();

    for index in 0..markdown::MAX_MARKDOWN_DEFINITIONS {
        let name = format!("definition-{index:03}");
        fs::write(
            root.join(format!("{name}.md")),
            format!("---\nname: {name}\n---\nbody\n"),
        )
        .unwrap();
    }
    fs::write(root.join("unrelated.txt"), "ignored").unwrap();
    fs::write(
        root.join("z-malformed.md"),
        "---\nname: {not: text}\n---\nbody\n",
    )
    .unwrap();
    fs::write(root.join("z-not-utf8.md"), [0xff, 0xfe]).unwrap();

    let MarkdownRoot {
        documents,
        diagnostics,
    } = markdown::load_root(&root).unwrap();

    assert_eq!(documents.len(), markdown::MAX_MARKDOWN_DEFINITIONS);
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message())
            .collect::<Vec<_>>(),
        vec![
            "frontmatter field name must be a string or string list",
            "file is not UTF-8: invalid utf-8 sequence of 1 bytes from index 0",
        ]
    );
}

#[test]
fn rejects_extra_definitions_once_without_hiding_later_diagnostics() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");
    fs::create_dir_all(&root).unwrap();

    for index in 0..=markdown::MAX_MARKDOWN_DEFINITIONS {
        let name = format!("definition-{index:03}");
        fs::write(
            root.join(format!("{name}.md")),
            format!("---\nname: {name}\n---\nbody\n"),
        )
        .unwrap();
    }
    fs::write(root.join("z-invalid.md"), [0xff]).unwrap();

    let MarkdownRoot {
        documents,
        diagnostics,
    } = markdown::load_root(&root).unwrap();

    assert_eq!(documents.len(), markdown::MAX_MARKDOWN_DEFINITIONS);
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message())
            .collect::<Vec<_>>(),
        vec![
            "accepted definition limit exceeded",
            "file is not UTF-8: invalid utf-8 sequence of 1 bytes from index 0",
        ]
    );
}

#[cfg(unix)]
#[test]
fn rejects_a_symbolic_link_root_even_when_it_points_outside_the_confinement() {
    let temporary = TemporaryDirectory::new();
    let outside = temporary.path.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(
        outside.join("outside.md"),
        "---\nname: outside\n---\nbody\n",
    )
    .unwrap();
    let root = temporary.path.join("root");
    std::os::unix::fs::symlink(&outside, &root).unwrap();

    assert_eq!(
        markdown::load_root(&root),
        Err("markdown root must be a non-symbolic-link directory".into())
    );
}

#[test]
fn discovers_agents_with_deterministic_precedence_modes_and_diagnostics() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    let project = temporary.path.join("project");
    fs::create_dir_all(&global).unwrap();
    fs::create_dir_all(&project).unwrap();
    write_agent(&global, "shared", "global", "all");
    write_agent(&global, "worker", "worker", "subagent");
    write_agent(&project, "shared", "project", "primary");
    fs::write(project.join("broken.md"), "---\nname: broken\n---\n").unwrap();

    let built_in = AgentDefinition {
        name: "shared".into(),
        description: "built-in".into(),
        mode: AgentMode::Primary,
        model: None,
        system_prompt: "built-in".into(),
        permission_rules: vec![],
        skills: vec![],
    };
    let discovery = AgentCatalog::discover(&[built_in], &global, &project).unwrap();

    assert_eq!(
        discovery.catalog().agent("shared").unwrap().description,
        "project"
    );
    assert_eq!(discovery.catalog().primary_or_all().count(), 1);
    assert_eq!(discovery.catalog().subagents().count(), 1);
    assert_eq!(discovery.shadowed().len(), 2);
    assert_eq!(discovery.diagnostics().len(), 1);
}

#[test]
fn discovers_commands_with_precedence_isolated_diagnostics_and_trimmed_arguments() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    let project = temporary.path.join("project");
    fs::create_dir_all(&global).unwrap();
    fs::create_dir_all(&project).unwrap();
    write_command(&global, "shared", "global", "run $ARGUMENTS now");
    write_command(&project, "shared", "project", "project:$ARGUMENTS");
    fs::write(
        project.join("broken.md"),
        "---\nname: {bad: value}\n---\nbody\n",
    )
    .unwrap();

    let built_in = CommandDefinition::new("shared", "built-in", "built-in:$ARGUMENTS").unwrap();
    let discovery = CommandCatalog::discover(&[built_in], &global, &project).unwrap();

    assert_eq!(
        discovery.catalog().command("shared").unwrap().description(),
        "project"
    );
    assert_eq!(
        discovery
            .catalog()
            .command("shared")
            .unwrap()
            .expand("  hello  "),
        "project:hello"
    );
    assert_eq!(discovery.shadowed().len(), 2);
    assert_eq!(discovery.diagnostics().len(), 1);
}

#[test]
fn command_catalog_accepts_missing_roots_and_preserves_literal_templates() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("missing-global");
    let project = temporary.path.join("missing-project");
    let command = CommandDefinition::new("literal", "literal", "$ARGUMENTS + $ARGUMENTS").unwrap();

    let discovery = CommandCatalog::discover(&[command], &global, &project).unwrap();

    assert_eq!(discovery.catalog().len(), 1);
    assert_eq!(
        discovery
            .catalog()
            .command("literal")
            .unwrap()
            .expand(" value "),
        "value + value"
    );
    assert!(discovery.diagnostics().is_empty());
}

#[test]
fn command_catalog_enforces_the_shared_definition_limit_deterministically() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    fs::create_dir_all(&global).unwrap();

    for index in 0..=markdown::MAX_MARKDOWN_DEFINITIONS {
        let name = format!("command-{index:03}");
        write_command(&global, &name, "description", "body");
    }

    let discovery = CommandCatalog::discover(&[], &global, temporary.path.join("missing")).unwrap();

    assert_eq!(
        discovery.catalog().len(),
        markdown::MAX_MARKDOWN_DEFINITIONS
    );
    assert_eq!(discovery.diagnostics().len(), 1);
    assert_eq!(
        discovery.diagnostics()[0].message(),
        "accepted definition limit exceeded"
    );
}

#[test]
fn command_catalog_counts_only_valid_definitions_before_reporting_overflow() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    fs::create_dir_all(&global).unwrap();

    for index in 0..markdown::MAX_MARKDOWN_DEFINITIONS {
        let name = format!("invalid-{index:03}");
        fs::write(
            global.join(format!("{name}.md")),
            format!("---\nname: {name}\n---\nbody\n"),
        )
        .unwrap();
    }
    for index in 0..markdown::MAX_MARKDOWN_DEFINITIONS {
        let name = format!("valid-{index:03}");
        write_command(&global, &name, "valid", "body");
    }
    write_command(&global, "z-overflow", "overflow", "body");

    let discovery = CommandCatalog::discover(&[], &global, temporary.path.join("missing")).unwrap();

    assert_eq!(
        discovery.catalog().len(),
        markdown::MAX_MARKDOWN_DEFINITIONS
    );
    assert!(discovery.catalog().command("valid-127").is_some());
    assert_eq!(discovery.diagnostics().len(), 129);
    assert_eq!(
        discovery.diagnostics()[128].message(),
        "accepted definition limit exceeded"
    );
}

#[cfg(unix)]
#[test]
fn command_catalog_rejects_a_symbolic_link_root() {
    let temporary = TemporaryDirectory::new();
    let outside = temporary.path.join("outside");
    fs::create_dir_all(&outside).unwrap();
    write_command(&outside, "outside", "outside", "outside");
    let global = temporary.path.join("global");
    std::os::unix::fs::symlink(&outside, &global).unwrap();

    assert!(CommandCatalog::discover(&[], &global, temporary.path.join("missing")).is_err());
}

#[test]
fn isolates_semantically_invalid_agents_without_consuming_the_catalog_limit() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    let project = temporary.path.join("project");
    fs::create_dir_all(&global).unwrap();
    fs::create_dir_all(&project).unwrap();

    for index in 0..markdown::MAX_MARKDOWN_DEFINITIONS {
        let name = format!("invalid-{index:03}");
        fs::write(
            global.join(format!("{name}.md")),
            format!("---\nname: {name}\nmode: primary\n---\nbody\n"),
        )
        .unwrap();
    }
    for index in 0..markdown::MAX_MARKDOWN_DEFINITIONS {
        let name = format!("valid-{index:03}");
        write_agent(&global, &name, "valid", "primary");
    }
    write_agent(&global, "z-overflow", "overflow", "primary");
    fs::write(
        global.join("zz-invalid.md"),
        "---\nname: zz-invalid\ndescription: invalid\nmode: unsupported\n---\nbody\n",
    )
    .unwrap();

    let discovery = AgentCatalog::discover(&[], &global, &project).unwrap();

    assert_eq!(discovery.catalog().primary_or_all().count(), 128);
    assert!(discovery.catalog().agent("valid-127").is_some());
    assert_eq!(discovery.diagnostics().len(), 130);
    assert_eq!(
        discovery.diagnostics()[128].message(),
        "accepted agent definition limit exceeded"
    );
    assert_eq!(
        discovery.diagnostics().last().unwrap().message(),
        "agent mode must be primary, subagent, or all"
    );
}

#[test]
fn isolates_unsafe_mismatched_and_oversized_agent_documents() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    let project = temporary.path.join("project");
    fs::create_dir_all(&global).unwrap();
    fs::create_dir_all(&project).unwrap();
    write_agent(&global, "duplicate", "first", "primary");
    fs::write(
        global.join("duplicate-copy.md"),
        "---\nname: duplicate\ndescription: second\nmode: primary\n---\nbody\n",
    )
    .unwrap();
    fs::write(global.join("model.md"), "---\nname: model\ndescription: model\nmode: all\nmodel: unknown\nskills:\n  - skill\npermissions:\n  - allow native::read\n---\nbody\n").unwrap();
    fs::write(
        project.join("large.md"),
        "x".repeat(markdown::MAX_MARKDOWN_FILE_BYTES + 1),
    )
    .unwrap();

    let duplicate = AgentDefinition {
        name: "duplicate".into(),
        description: "built-in".into(),
        mode: AgentMode::Primary,
        model: None,
        system_prompt: "built-in".into(),
        permission_rules: vec![],
        skills: vec![],
    };
    let discovery =
        AgentCatalog::discover(&[duplicate.clone(), duplicate], &global, &project).unwrap();

    assert_eq!(
        discovery.catalog().agent("duplicate").unwrap().description,
        "first"
    );
    let model = discovery.catalog().agent("model").unwrap();
    assert_eq!(model.model.as_deref(), Some("unknown"));
    assert_eq!(model.system_prompt, "body");
    assert_eq!(model.permission_rules.len(), 1);
    assert_eq!(discovery.diagnostics().len(), 4);
}

#[test]
fn catalog_isolates_models_rejected_by_the_tools_owned_validator() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    let project = temporary.path.join("project");
    fs::create_dir_all(&global).unwrap();
    fs::create_dir_all(&project).unwrap();
    fs::write(
        global.join("allowed.md"),
        "---\nname: allowed\ndescription: allowed\nmode: primary\nmodel: supported\n---\nbody\n",
    )
    .unwrap();
    fs::write(global.join("rejected.md"), "---\nname: rejected\ndescription: rejected\nmode: primary\nmodel: unsupported\n---\nbody\n").unwrap();

    let discovery =
        AgentCatalog::discover_with_model_validator(&[], &global, &project, &SupportedModels)
            .unwrap();

    assert!(discovery.catalog().agent("allowed").is_some());
    assert!(discovery.catalog().agent("rejected").is_none());
    assert_eq!(
        discovery.diagnostics()[0].message(),
        "agent model is unavailable"
    );
}

#[test]
fn task_dispatch_resolves_only_subagents_and_validated_requested_configuration() {
    let temporary = TemporaryDirectory::new();
    let agents = temporary.path.join("agents");
    let skills = temporary.path.join("skills");
    fs::create_dir_all(&agents).unwrap();
    fs::create_dir_all(&skills).unwrap();
    write_agent(&agents, "all", "all agent", "all");
    write_agent(&agents, "primary", "primary agent", "primary");
    fs::write(
        agents.join("zmissing.md"),
        "---\nname: zmissing\ndescription: missing skill\nmode: subagent\nskills:\n  - absent\n---\nmissing instructions\n",
    )
    .unwrap();
    fs::write(
        agents.join("worker.md"),
        "---\nname: worker\ndescription: worker agent\nmode: subagent\nmodel: worker-model\nskills:\n  - allowed\n---\nworker instructions\n",
    )
    .unwrap();
    write_agent(&agents, "zfallback", "fallback agent", "subagent");
    fs::create_dir_all(skills.join("allowed")).unwrap();
    fs::write(
        skills.join("allowed/SKILL.md"),
        "---\nname: allowed\ndescription: allowed skill\n---\nallowed instructions\n",
    )
    .unwrap();

    let agent_catalog =
        AgentCatalog::discover(&[], &agents, &temporary.path.join("missing")).unwrap();
    let skill_catalog = SkillCatalog::discover(&skills, temporary.path.join("missing")).unwrap();
    let mut task = TaskTool::from_catalogs_with_model_validator(
        agent_catalog.catalog().clone(),
        skill_catalog.catalog().clone(),
        "parent-model",
        TaskModels,
        RecordingTaskRunner,
    );
    let context = ToolExecutionContext::new(
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        std::time::Duration::from_secs(1),
    );

    assert_eq!(agent_catalog.catalog().subagents().count(), 4);
    assert_eq!(
        task.permission_target(&serde_json::json!({"description":"default task"}))
            .unwrap(),
        "worker"
    );
    assert_eq!(
        task.execute(
            &context,
            serde_json::json!({"agent":"worker","model":"override-model","skills":["allowed"],"description":"inspect the repository"}),
        )
        .unwrap(),
        ToolOutput::success("worker:worker agent:override-model:allowed:inspect the repository")
    );
    assert_eq!(
        task.execute(
            &context,
            serde_json::json!({"agent":"worker","description":"load agent defaults"}),
        )
        .unwrap(),
        ToolOutput::success("worker:worker agent:worker-model:allowed:load agent defaults")
    );
    assert_eq!(
        task.execute(
            &context,
            serde_json::json!({"agent":"all","description":"reject all"}),
        )
        .unwrap(),
        ToolOutput::failure("task: requested agent is unavailable")
    );
    assert_eq!(
        task.execute(
            &context,
            serde_json::json!({"agent":"zmissing","description":"do not run"}),
        )
        .unwrap(),
        ToolOutput::failure("task: requested skill is unavailable")
    );
    assert_eq!(
        task.execute(
            &context,
            serde_json::json!({"agent":"zfallback","description":"use parent defaults"}),
        )
        .unwrap(),
        ToolOutput::success("zfallback:fallback agent:parent-model:none:use parent defaults")
    );
    assert_eq!(
        task.execute(
            &context,
            serde_json::json!({"agent":"primary","description":"reject me"}),
        )
        .unwrap(),
        ToolOutput::failure("task: requested agent is unavailable")
    );
    assert_eq!(
        task.execute(
            &context,
            serde_json::json!({"agent":"worker","model":"unavailable","description":"reject model"}),
        )
        .unwrap(),
        ToolOutput::failure("task: requested model is unavailable")
    );
    assert_eq!(
        task.execute(
            &context,
            serde_json::json!({"agent":"worker","skills":["unavailable"],"description":"reject skill"}),
        )
        .unwrap(),
        ToolOutput::failure("task: requested skill is unavailable")
    );
    assert!(
        TaskInvocation::from_value(serde_json::json!({"description":"x","unexpected":true}))
            .is_err()
    );
    assert_eq!(
        TaskTool::<RecordingTaskRunner>::input_schema(),
        serde_json::json!({"type":"object","additionalProperties":false,"required":["description"],"properties":{"agent":{"type":"string","minLength":1,"maxLength":64},"description":{"type":"string","minLength":1,"maxLength":16384},"model":{"type":"string","minLength":1,"maxLength":64},"skills":{"type":"array","maxItems":128,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":64}}}})
    );
}

#[test]
fn effective_capabilities_normalize_aliases_globs_projects_and_last_matches() {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::files_read", ToolAccess::ReadOnly, InertTool)
        .unwrap();
    dispatcher
        .register_native("native::files_write", ToolAccess::Write, InertTool)
        .unwrap();
    let agent = agent_with_rules(vec![
        PermissionRule::global(
            PermissionDecision::Deny,
            PermissionPattern::Exact("files_read".into()),
            PermissionPattern::Any,
        ),
        PermissionRule::global(
            PermissionDecision::Allow,
            PermissionPattern::Exact("native::files_read".into()),
            PermissionPattern::Any,
        ),
        PermissionRule::project(
            "other",
            PermissionDecision::Allow,
            PermissionPattern::glob("*").unwrap(),
            PermissionPattern::glob("project/*").unwrap(),
        ),
        PermissionRule::project(
            "project",
            PermissionDecision::Ask,
            PermissionPattern::glob("*").unwrap(),
            PermissionPattern::glob("project/*").unwrap(),
        ),
    ]);

    let set = EffectiveCapabilitySet::from_agent(&agent, "project", &dispatcher);

    assert_eq!(set.descriptors().len(), 2);
    assert_eq!(set.descriptors()[0].decision(), PermissionDecision::Allow);
    assert_eq!(set.descriptors()[1].decision(), PermissionDecision::Ask);
    assert!(set.descriptors()[1].matches_identity("native:10:files_read"));
    assert!(set.descriptors()[1].matches_identity("native:11:files_write"));
}

#[test]
fn effective_capability_expansion_detects_only_declared_broadenings() {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::files_read", ToolAccess::ReadOnly, InertTool)
        .unwrap();
    let deny = capability_set(&dispatcher, PermissionDecision::Deny);
    let ask = capability_set(&dispatcher, PermissionDecision::Ask);
    let allow = capability_set(&dispatcher, PermissionDecision::Allow);
    let empty =
        EffectiveCapabilitySet::from_agent(&agent_with_rules(vec![]), "project", &dispatcher);

    assert!(allow.is_expansion_from(&ask));
    assert!(allow.is_expansion_from(&deny));
    assert!(empty.is_expansion_from(&deny));
    assert!(!ask.is_expansion_from(&allow));
    assert!(!deny.is_expansion_from(&ask));
    assert!(!deny.is_expansion_from(&empty));
}

#[test]
fn parsed_literal_aliases_resolve_while_globs_remain_distinct_descriptors() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    let project = temporary.path.join("project");
    fs::create_dir_all(&global).unwrap();
    fs::create_dir_all(&project).unwrap();
    fs::write(
        global.join("agent.md"),
        "---\nname: agent\ndescription: agent\nmode: primary\npermissions:\n  - deny files_read\n  - allow native::files_read\n  - ask native:*:files_*\n---\nbody\n",
    )
    .unwrap();

    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::files_read", ToolAccess::ReadOnly, InertTool)
        .unwrap();
    dispatcher
        .register_native("native::files_write", ToolAccess::Write, InertTool)
        .unwrap();

    let discovery = AgentCatalog::discover(&[], &global, &project).unwrap();
    let set = EffectiveCapabilitySet::from_agent(
        discovery.catalog().agent("agent").unwrap(),
        "project",
        &dispatcher,
    );

    assert_eq!(set.descriptors().len(), 2);
    assert_eq!(
        set.descriptors()
            .iter()
            .filter(|descriptor| descriptor.decision() == PermissionDecision::Allow)
            .count(),
        1
    );
    assert!(set.descriptors()[0].matches_identity("native:10:files_read"));
    assert!(set.descriptors()[1].matches_identity("native:10:files_read"));
    assert!(set.descriptors()[1].matches_identity("native:11:files_write"));
}

#[test]
fn capability_descriptors_are_ordered_independently_of_rule_insertion() {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::files_read", ToolAccess::ReadOnly, InertTool)
        .unwrap();
    dispatcher
        .register_native("native::files_write", ToolAccess::Write, InertTool)
        .unwrap();

    let read = PermissionRule::global(
        PermissionDecision::Allow,
        PermissionPattern::Exact("files_read".into()),
        PermissionPattern::Any,
    );
    let write = PermissionRule::global(
        PermissionDecision::Deny,
        PermissionPattern::Exact("files_write".into()),
        PermissionPattern::Any,
    );
    let glob = PermissionRule::global(
        PermissionDecision::Ask,
        PermissionPattern::glob("native:*:files_*").unwrap(),
        PermissionPattern::glob("project/*").unwrap(),
    );

    let forward = EffectiveCapabilitySet::from_agent(
        &agent_with_rules(vec![read.clone(), write.clone(), glob.clone()]),
        "project",
        &dispatcher,
    );
    let reverse = EffectiveCapabilitySet::from_agent(
        &agent_with_rules(vec![glob, write, read]),
        "project",
        &dispatcher,
    );

    assert_eq!(forward.descriptors(), reverse.descriptors());
    assert_eq!(forward.descriptors().len(), 3);
}

#[test]
fn capability_builder_input_excludes_safety_grants_and_bypass_layers() {
    let builder: fn(&AgentDefinition, &str, &ToolDispatcher) -> EffectiveCapabilitySet =
        EffectiveCapabilitySet::from_agent;
    let dispatcher = ToolDispatcher::new();
    let declared_policy = builder(&agent_with_rules(vec![]), "project", &dispatcher);

    assert_eq!(declared_policy.descriptors(), &[]);
}

#[test]
fn effective_capability_expansion_table_covers_all_decision_transitions() {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::files_read", ToolAccess::ReadOnly, InertTool)
        .unwrap();

    let empty =
        EffectiveCapabilitySet::from_agent(&agent_with_rules(vec![]), "project", &dispatcher);
    let decisions = [
        PermissionDecision::Allow,
        PermissionDecision::Ask,
        PermissionDecision::Deny,
    ];

    for prior in decisions {
        for candidate in decisions {
            let expected = matches!(
                (prior, candidate),
                (PermissionDecision::Ask, PermissionDecision::Allow)
                    | (
                        PermissionDecision::Deny,
                        PermissionDecision::Ask | PermissionDecision::Allow
                    )
            );
            assert_eq!(
                capability_set(&dispatcher, candidate)
                    .is_expansion_from(&capability_set(&dispatcher, prior)),
                expected,
                "{prior:?} -> {candidate:?}"
            );
        }
    }

    assert!(capability_set(&dispatcher, PermissionDecision::Allow).is_expansion_from(&empty));
    assert!(!empty.is_expansion_from(&capability_set(&dispatcher, PermissionDecision::Allow)));
    assert!(empty.is_expansion_from(&capability_set(&dispatcher, PermissionDecision::Deny)));
    assert!(!capability_set(&dispatcher, PermissionDecision::Deny).is_expansion_from(&empty));
}

struct SupportedModels;

impl AgentModelValidator for SupportedModels {
    fn validate_model(&self, model: &str) -> Result<(), AgentModelValidationError> {
        (model == "supported")
            .then_some(())
            .ok_or(AgentModelValidationError::Unavailable)
    }
}

struct TaskModels;

impl AgentModelValidator for TaskModels {
    fn validate_model(&self, model: &str) -> Result<(), AgentModelValidationError> {
        matches!(model, "parent-model" | "worker-model" | "override-model")
            .then_some(())
            .ok_or(AgentModelValidationError::Unavailable)
    }
}

struct InertTool;

impl DispatchTool for InertTool {
    fn execute(&mut self, _: &ToolExecutionContext, _: Value) -> Result<ToolOutput, Error> {
        Ok(ToolOutput::success("unused"))
    }
}

struct RecordingTaskRunner;

impl TaskRunner for RecordingTaskRunner {
    fn run(&mut self, request: TaskTurnRequest) -> Result<ToolOutput, Error> {
        Ok(ToolOutput::success(format!(
            "{}:{}:{}:{}:{}",
            request.agent_name(),
            request.agent_description(),
            request.model(),
            request
                .skills()
                .first()
                .map(TaskSkill::name)
                .unwrap_or("none"),
            request.description()
        )))
    }
}

fn agent_with_rules(permission_rules: Vec<PermissionRule>) -> AgentDefinition {
    AgentDefinition {
        name: "agent".into(),
        description: "agent".into(),
        mode: AgentMode::Primary,
        model: None,
        system_prompt: "body".into(),
        permission_rules,
        skills: vec![],
    }
}

fn capability_set(
    dispatcher: &ToolDispatcher,
    decision: PermissionDecision,
) -> EffectiveCapabilitySet {
    EffectiveCapabilitySet::from_agent(
        &agent_with_rules(vec![PermissionRule::global(
            decision,
            PermissionPattern::Exact("native::files_read".into()),
            PermissionPattern::Any,
        )]),
        "project",
        dispatcher,
    )
}

fn write_agent(root: &std::path::Path, name: &str, description: &str, mode: &str) {
    fs::write(root.join(format!("{name}.md")), format!("---\nname: {name}\ndescription: {description}\nmode: {mode}\npermissions: []\n---\nbody\n")).unwrap();
}

fn write_command(root: &std::path::Path, name: &str, description: &str, body: &str) {
    fs::write(
        root.join(format!("{name}.md")),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    )
    .unwrap();
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new() -> Self {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("agens-extensions-{name}"));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
