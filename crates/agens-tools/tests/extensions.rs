use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use agens_core::{
    AgentDefinition, AgentMode, AgentModelSource, Error, HeadlessTaskTerminal, PermissionDecision,
    PermissionMode, PermissionPattern, PermissionPolicy, PermissionRule, PermissionSession,
    PermissionTargetKind, ReasoningEffort, RequestConfig, ToolAccess,
};
use agens_tools::{
    AgentCatalog, AgentModelValidationError, AgentModelValidator, CommandCatalog,
    CommandDefinition, DispatchTool, EffectiveCapabilityDescriptor, EffectiveCapabilitySet,
    SkillCatalog, TaskControlAction, TaskControlTool, TaskExecutionEvent, TaskExecutionId,
    TaskExecutionLifecycle, TaskExecutionRegistry, TaskExecutionSnapshot, TaskInvocation,
    TaskLaunchMode, TaskMessageSource, TaskMessageTarget, TaskMessageTool,
    TaskModelResolutionError, TaskRunContext, TaskRunner, TaskRunnerError, TaskSkill,
    TaskTerminalState, TaskTool, TaskTurnRequest, TaskTurnResult, ToolDispatchRequest,
    ToolDispatcher, ToolEvaluationOutcome, ToolExecutionContext, ToolOutput,
    markdown::{self, FrontmatterValue, MarkdownRoot},
};
use serde_json::Value;

/// Overall budget for a spawned child to be admitted and reach the blocking
/// runner.
///
/// It only has to stay above the scheduler latency of starting a handful of
/// threads, and is bounded so that permit accounting which never admits a child
/// fails here loudly instead of hanging the test binary.
const CHILD_ADMISSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline handed to a child that a test parks — in [`BlockingTaskRunner`] or
/// in a publication hook — and releases itself.
///
/// No assertion expects these children to expire, yet the deadline starts
/// running when the parent context is built, which is before the tool is
/// constructed from disk and before the calling thread is even scheduled. It
/// therefore has to outlast all of that plus the parked window; a one-second
/// budget does not on a saturated machine, and an expiry there does not merely
/// slow the test down, it takes a different branch of the tool and never runs
/// the runner at all.
const PARKED_CHILD_DEADLINE: Duration = Duration::from_secs(30);

/// Deadline for the one background task whose expiry is the behaviour under
/// test.
///
/// Unlike [`PARKED_CHILD_DEADLINE`] this one has to fire — but not before the
/// worker thread has reached the runner and reported the deadline it inherited.
/// Both ends are wall-clock, because a parent deadline is anchored when the
/// context is built and no API lets a test start it at child entry, so the only
/// available lever is a margin wide enough that thread start-up cannot beat it.
const EXPIRING_PARENT_DEADLINE: Duration = Duration::from_secs(1);

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
        model_source: None,
        reasoning_effort: None,
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
        "built-in"
    );
    assert_eq!(
        discovery
            .catalog()
            .command("shared")
            .unwrap()
            .expand("  hello  "),
        "built-in:hello"
    );
    assert_eq!(
        discovery
            .catalog()
            .iter()
            .map(CommandDefinition::name)
            .collect::<Vec<_>>(),
        vec!["shared"]
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
        model_source: None,
        reasoning_effort: None,
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
fn agent_catalog_parses_valid_effort_and_diagnoses_invalid_effort() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    fs::create_dir_all(&global).unwrap();
    fs::write(
        global.join("low-effort.md"),
        "---\nname: low-effort\ndescription: low effort\nmode: subagent\neffort: low\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        global.join("invalid-effort.md"),
        "---\nname: invalid-effort\ndescription: invalid effort\nmode: subagent\neffort: opus\n---\nbody\n",
    )
    .unwrap();

    let discovery = AgentCatalog::discover(&[], &global, &temporary.path.join("missing")).unwrap();

    assert_eq!(
        discovery
            .catalog()
            .agent("low-effort")
            .unwrap()
            .reasoning_effort,
        Some(ReasoningEffort::Low)
    );
    assert!(discovery.catalog().agent("invalid-effort").is_none());
    assert!(discovery.diagnostics()[0].message().contains("effort"));
}

#[test]
fn catalog_preserves_models_rejected_by_the_tools_owned_validator() {
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
    assert_eq!(
        discovery
            .catalog()
            .agent("rejected")
            .unwrap()
            .model
            .as_deref(),
        Some("unsupported")
    );
    assert_eq!(
        discovery.diagnostics()[0].message(),
        "agent model \"unsupported\" is unavailable"
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
        ToolOutput::failure(
            "task: requested skill is unavailable [skill: absent; not in the skill catalog]"
        )
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
        ToolOutput::success(
            "warning: agent worker requested unavailable model unavailable; using parent-model\nworker:worker agent:parent-model:allowed:reject model"
        )
    );
    assert_eq!(
        task.execute(
            &context,
            serde_json::json!({"agent":"worker","skills":["unavailable"],"description":"reject skill"}),
        )
        .unwrap(),
        ToolOutput::failure(
            "task: requested skill is unavailable [skill: unavailable; not declared by the agent]"
        )
    );
    assert!(
        TaskInvocation::from_value(serde_json::json!({"description":"x","unexpected":true}))
            .is_err()
    );
    assert_eq!(
        TaskTool::<RecordingTaskRunner>::input_schema(),
        serde_json::json!({"type":"object","additionalProperties":false,"required":["description"],"properties":{"agent":{"type":"string","minLength":1,"maxLength":64},"background":{"type":"boolean","description":"Run this call in the background and return immediately. Only background calls run concurrently: several foreground calls issued together are executed one after another."},"description":{"type":"string","minLength":1,"maxLength":16384},"model":{"type":"string","minLength":1,"maxLength":64,"description":"Omit this. The agent's configured profile then decides the model, falling back to this thread's model. Send it only when the user explicitly asked for a specific model for this call: an explicit value overrides the configured profile. A model this run cannot reach falls back to this thread's model and says so."},"skills":{"type":"array","maxItems":128,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":64}}}})
    );
    assert_eq!(
        task.catalog_input_schema()["properties"]["agent"]["enum"],
        serde_json::json!(["worker", "zfallback", "zmissing"])
    );
    assert_eq!(
        task.catalog_input_schema()["properties"]["agent"]["description"],
        "Eligible subagents:\n- worker: worker agent\n- zfallback: fallback agent\n- zmissing: missing skill"
    );
}

#[test]
fn task_inherits_parent_model_and_effort_but_validates_explicit_overrides() {
    let temporary = TemporaryDirectory::new();
    let agents = temporary.path.join("agents");
    let missing = temporary.path.join("missing");
    fs::create_dir_all(&agents).unwrap();
    write_agent(&agents, "inherited", "inherited agent", "subagent");
    fs::write(
        agents.join("explicit.md"),
        "---\nname: explicit\ndescription: explicit agent\nmode: subagent\nmodel: worker-model\neffort: low\n---\nexplicit instructions\n",
    )
    .unwrap();
    let agents = AgentCatalog::discover(&[], &agents, &missing)
        .unwrap()
        .catalog()
        .clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let parent_config = RequestConfig::with_reasoning_effort("high").unwrap();
    let mut task = TaskTool::from_catalogs_with_parent_config(
        agents,
        SkillCatalog::default(),
        "parent-model",
        parent_config,
        TaskModels,
        CapturingTaskRunner(Arc::clone(&calls)),
    );
    let context = task_context();

    assert!(
        !task
            .execute(
                &context,
                serde_json::json!({"agent":"inherited","description":"inherit"}),
            )
            .unwrap()
            .is_error
    );
    assert!(
        !task
            .execute(
                &context,
                serde_json::json!({"agent":"explicit","description":"override"}),
            )
            .unwrap()
            .is_error
    );

    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            ("parent-model".to_owned(), Some(ReasoningEffort::High)),
            ("worker-model".to_owned(), Some(ReasoningEffort::Low)),
        ]
    );
    assert!(
        task.catalog_input_schema()["properties"]["model"]["enum"].is_null(),
        "the bundled catalog is a snapshot, so it must not be published as a closed set"
    );
    assert!(
        task.catalog_input_schema()["properties"]["model"]["description"]
            .as_str()
            .is_some_and(|description| description.starts_with("Omit this."))
    );
}

#[test]
fn explicit_model_overriding_a_configured_profile_announces_the_override() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut task = task_tool_with_configured_worker_model(Arc::clone(&calls));

    let output = task
        .execute(
            &task_context(),
            serde_json::json!({"agent":"worker","description":"override","model":"override-model"}),
        )
        .unwrap();

    assert_eq!(
        output,
        ToolOutput::success(
            "warning: agent worker is configured to run worker-model; the explicit model argument override-model overrides that configuration\ncaptured"
        )
    );
    assert_eq!(
        *calls.lock().unwrap(),
        vec![("override-model".to_owned(), None)]
    );
}

#[test]
fn an_explicit_model_matching_configuration_or_inheritance_announces_nothing() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut task = task_tool_with_configured_worker_model(Arc::clone(&calls));

    assert_eq!(
        task.execute(
            &task_context(),
            serde_json::json!({"agent":"worker","description":"agree","model":"worker-model"}),
        )
        .unwrap(),
        ToolOutput::success("captured")
    );
    assert_eq!(
        task.execute(
            &task_context(),
            serde_json::json!({"agent":"inherited","description":"inherit","model":"override-model"}),
        )
        .unwrap(),
        ToolOutput::success("captured")
    );
}

fn task_tool_with_configured_worker_model(
    calls: CapturedTaskModels,
) -> TaskTool<CapturingTaskRunner> {
    let temporary = TemporaryDirectory::new();
    let agents = temporary.path.join("agents");
    let missing = temporary.path.join("missing");
    fs::create_dir_all(&agents).unwrap();
    write_agent(&agents, "worker", "worker agent", "subagent");
    write_agent(&agents, "inherited", "inherited agent", "subagent");
    let agents = AgentCatalog::discover(&[], &agents, &missing)
        .unwrap()
        .catalog()
        .map_agents(|agent| {
            let mut agent = agent.clone();
            let configured = agent.name == "worker";
            agent.model = Some(if configured {
                "worker-model".to_owned()
            } else {
                "parent-model".to_owned()
            });
            agent.model_source = Some(if configured {
                AgentModelSource::ConfiguredProfile
            } else {
                AgentModelSource::SessionInherited
            });
            agent
        });

    TaskTool::from_catalogs_with_parent_config(
        agents,
        SkillCatalog::default(),
        "parent-model",
        RequestConfig::default(),
        TaskModels,
        CapturingTaskRunner(calls),
    )
}

#[test]
fn unavailable_agent_model_degrades_to_the_parent_and_records_a_diagnostic() {
    let temporary = TemporaryDirectory::new();
    let agents = temporary.path.join("agents");
    let missing = temporary.path.join("missing");
    fs::create_dir_all(&agents).unwrap();
    fs::write(
        agents.join("worker.md"),
        "---\nname: worker\ndescription: worker agent\nmode: subagent\nmodel: unavailable\n---\nworker instructions\n",
    )
    .unwrap();
    let agents = AgentCatalog::discover(&[], &agents, &missing)
        .unwrap()
        .catalog()
        .clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let diagnostic_probe = Arc::clone(&diagnostics);
    let mut task = TaskTool::from_catalogs_with_parent_config(
        agents,
        SkillCatalog::default(),
        "parent-model",
        RequestConfig::with_reasoning_effort("high").unwrap(),
        TaskModels,
        CapturingTaskRunner(Arc::clone(&calls)),
    )
    .with_model_resolution_diagnostics(move |error| {
        diagnostic_probe.lock().unwrap().push(error);
        Some("abc12345".to_owned())
    });

    let output = task
        .execute(
            &task_context(),
            serde_json::json!({"agent":"worker","description":"degrade"}),
        )
        .unwrap();

    assert_eq!(
        output,
        ToolOutput::success(
            "warning: agent worker requested unavailable model unavailable; using parent-model [ref: abc12345]\ncaptured"
        )
    );
    assert_eq!(
        *calls.lock().unwrap(),
        vec![("parent-model".to_owned(), Some(ReasoningEffort::High))]
    );
    assert_eq!(
        *diagnostics.lock().unwrap(),
        vec![TaskModelResolutionError::ModelUnavailable {
            agent: "worker".to_owned(),
            requested_model: "unavailable".to_owned(),
            fallback_model: "parent-model".to_owned(),
        }]
    );
}

#[test]
fn an_oversized_result_is_truncated_with_a_visible_marker_rather_than_discarded() {
    let mut task = task_tool(OversizedTaskRunner);

    let output = task.execute(&task_context(), task_arguments()).unwrap();

    assert!(
        !output.is_error,
        "a subagent that finished its work must not be reported as a failure"
    );
    assert!(output.content.starts_with(&"x".repeat(65_536)));
    assert_eq!(
        output.content.chars().count(),
        65_536 + TRUNCATION_MARKER.chars().count()
    );
    assert!(output.content.ends_with(TRUNCATION_MARKER));
}

#[test]
fn a_result_at_the_output_limit_carries_no_truncation_marker() {
    let mut task = task_tool(SizedTaskRunner(65_536));

    let output = task.execute(&task_context(), task_arguments()).unwrap();

    assert_eq!(output, ToolOutput::success("x".repeat(65_536)));
}

#[test]
fn a_rejected_model_is_retried_once_on_the_parent_model() {
    let temporary = TemporaryDirectory::new();
    let agents = temporary.path.join("agents");
    fs::create_dir_all(&agents).unwrap();
    write_agent(&agents, "worker", "worker agent", "subagent");
    let agents = AgentCatalog::discover(&[], &agents, &temporary.path.join("missing"))
        .unwrap()
        .catalog()
        .map_agents(|agent| {
            let mut agent = agent.clone();
            agent.model = Some("worker-model".to_owned());
            agent.model_source = Some(AgentModelSource::ConfiguredProfile);
            agent
        });
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut task = TaskTool::from_catalogs_with_parent_config(
        agents,
        SkillCatalog::default(),
        "parent-model",
        RequestConfig::default(),
        TaskModels,
        RejectingTaskRunner {
            rejected: "worker-model".into(),
            calls: Arc::clone(&calls),
        },
    );

    let output = task
        .execute(
            &task_context(),
            serde_json::json!({"agent":"worker","description":"retry"}),
        )
        .unwrap();

    assert!(!output.is_error);
    assert_eq!(
        output.content,
        "warning: the provider rejected model worker-model for agent worker; this result came from parent-model\nretried"
    );
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["worker-model".to_owned(), "parent-model".to_owned()]
    );
}

#[test]
fn a_rejection_on_the_parent_model_itself_is_not_retried() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut task = task_tool(RejectingTaskRunner {
        rejected: "parent-model".into(),
        calls: Arc::clone(&calls),
    });

    let output = task.execute(&task_context(), task_arguments()).unwrap();

    assert_eq!(
        output,
        ToolOutput::failure("task: provider failure [cause: request rejected]")
    );
    assert_eq!(*calls.lock().unwrap(), vec!["parent-model".to_owned()]);
}

#[test]
fn an_explicit_model_this_run_cannot_reach_falls_back_instead_of_failing() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut task = task_tool(CapturingTaskRunner(Arc::clone(&calls)));

    let output = task
        .execute(
            &task_context(),
            serde_json::json!({"description":"explicit","model":"withdrawn-model"}),
        )
        .unwrap();

    assert!(!output.is_error);
    assert_eq!(
        output.content,
        "warning: agent worker requested unavailable model withdrawn-model; using parent-model\ncaptured"
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .first()
            .map(|(model, _)| model.clone()),
        Some("parent-model".to_owned())
    );
}

#[test]
fn a_task_naming_no_agent_runs_the_general_subagent_not_the_alphabetically_first_one() {
    let temporary = TemporaryDirectory::new();
    let agents = temporary.path.join("agents");
    fs::create_dir_all(&agents).unwrap();
    write_agent(&agents, "explore", "read-only exploration", "subagent");
    write_agent(&agents, "general", "general delegated work", "subagent");
    let agents = AgentCatalog::discover(&[], &agents, &temporary.path.join("missing"))
        .unwrap()
        .catalog()
        .clone();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut task = TaskTool::from_catalogs_with_parent_config(
        agents,
        SkillCatalog::default(),
        "parent-model",
        RequestConfig::default(),
        TaskModels,
        AgentNamingTaskRunner(Arc::clone(&observed)),
    );

    assert!(
        !task
            .execute(&task_context(), task_arguments())
            .unwrap()
            .is_error
    );

    assert_eq!(*observed.lock().unwrap(), vec!["general".to_owned()]);
}

#[test]
fn a_task_naming_no_agent_falls_back_to_the_first_subagent_when_no_general_one_exists() {
    let temporary = TemporaryDirectory::new();
    let agents = temporary.path.join("agents");
    fs::create_dir_all(&agents).unwrap();
    write_agent(&agents, "explore", "read-only exploration", "subagent");
    write_agent(&agents, "zeta", "last by name", "subagent");
    let agents = AgentCatalog::discover(&[], &agents, &temporary.path.join("missing"))
        .unwrap()
        .catalog()
        .clone();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut task = TaskTool::from_catalogs_with_parent_config(
        agents,
        SkillCatalog::default(),
        "parent-model",
        RequestConfig::default(),
        TaskModels,
        AgentNamingTaskRunner(Arc::clone(&observed)),
    );

    assert!(
        !task
            .execute(&task_context(), task_arguments())
            .unwrap()
            .is_error
    );

    assert_eq!(*observed.lock().unwrap(), vec!["explore".to_owned()]);
}

#[test]
fn task_schema_exposes_only_sanitized_subagent_metadata() {
    let temporary = TemporaryDirectory::new();
    let agents = temporary.path.join("agents");
    let skills = temporary.path.join("skills");
    fs::create_dir_all(&agents).unwrap();
    fs::create_dir_all(&skills).unwrap();
    fs::write(
        agents.join("alpha.md"),
        "---\nname: alpha\ndescription: contains token=PRIVATE_SCHEMA_SENTINEL\nmode: subagent\nskills:\n  - internal\n---\nPRIVATE_PROMPT_SENTINEL\n",
    )
    .unwrap();
    write_agent(&agents, "all", "all agent", "all");
    write_agent(&agents, "primary", "primary agent", "primary");
    write_agent(&agents, "zeta", &"z".repeat(256), "subagent");
    fs::create_dir_all(skills.join("internal")).unwrap();
    fs::write(
        skills.join("internal/SKILL.md"),
        "---\nname: internal\ndescription: INTERNAL_SKILL_SENTINEL\n---\nPRIVATE_SKILL_BODY_SENTINEL\n",
    )
    .unwrap();

    let agents = AgentCatalog::discover(&[], &agents, &temporary.path.join("missing"))
        .unwrap()
        .catalog()
        .clone();
    let skills = SkillCatalog::discover(&skills, temporary.path.join("missing"))
        .unwrap()
        .catalog()
        .clone();
    let task = TaskTool::from_catalogs_with_model_validator(
        agents,
        skills,
        "parent-model",
        TaskModels,
        RecordingTaskRunner,
    );

    let schema = task.catalog_input_schema();
    assert_eq!(
        schema["properties"]["agent"]["enum"],
        serde_json::json!(["alpha", "zeta"])
    );
    let rendered = schema.to_string();
    for private in [
        "PRIVATE_SCHEMA_SENTINEL",
        "PRIVATE_PROMPT_SENTINEL",
        "INTERNAL_SKILL_SENTINEL",
        "PRIVATE_SKILL_BODY_SENTINEL",
        temporary.path.to_str().unwrap(),
    ] {
        assert!(!rendered.contains(private), "schema leaked {private}");
    }
    assert!(rendered.contains("[redacted]"));
    assert!(!rendered.contains(&"z".repeat(161)));
}

#[test]
fn task_dispatcher_preserves_late_validation_errors_without_running_the_child() {
    let temporary = TemporaryDirectory::new();
    let agents = temporary.path.join("agents");
    let skills = temporary.path.join("skills");
    fs::create_dir_all(&agents).unwrap();
    fs::create_dir_all(skills.join("allowed")).unwrap();
    write_agent(&agents, "worker", "worker agent", "subagent");
    fs::write(
        agents.join("missing.md"),
        "---\nname: missing\ndescription: missing skill\nmode: subagent\nskills:\n  - absent\n---\nmissing instructions\n",
    )
    .unwrap();
    fs::write(
        skills.join("allowed/SKILL.md"),
        "---\nname: allowed\ndescription: allowed skill\n---\nallowed instructions\n",
    )
    .unwrap();

    let agents = AgentCatalog::discover(&[], &agents, &temporary.path.join("missing-root"))
        .unwrap()
        .catalog()
        .clone();
    let skills = SkillCatalog::discover(&skills, temporary.path.join("missing-root"))
        .unwrap()
        .catalog()
        .clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::task",
            ToolAccess::ReadOnly,
            TaskTool::from_catalogs_with_model_validator(
                agents,
                skills,
                "parent-model",
                TaskModels,
                CountingTaskRunner(Arc::clone(&calls)),
            ),
        )
        .unwrap();
    let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
    let session = PermissionSession::with_temporary_bypass();
    let context = ToolExecutionContext::with_timeout(std::time::Duration::from_secs(1));

    for (arguments, expected) in [
        (
            serde_json::json!({"agent":"worker","skills":["unavailable"],"description":"reject disallowed skill"}),
            "task: requested skill is unavailable [skill: unavailable; not declared by the agent]",
        ),
        (
            serde_json::json!({"agent":"missing","description":"reject missing skill"}),
            "task: requested skill is unavailable [skill: absent; not in the skill catalog]",
        ),
    ] {
        let ToolEvaluationOutcome::Authorized(handle) = dispatcher
            .evaluate(
                &policy,
                &[],
                &session,
                ToolDispatchRequest::new("project", "native::task", arguments),
            )
            .unwrap()
        else {
            panic!("task call should be authorized before late validation");
        };

        assert_eq!(
            dispatcher.execute(handle, &context).unwrap(),
            ToolOutput::failure(expected)
        );
        assert_eq!(calls.load(Ordering::Acquire), 0);
    }

    match dispatcher.evaluate(
        &policy,
        &[],
        &session,
        ToolDispatchRequest::new(
            "project",
            "native::task",
            serde_json::json!({"agent":"unknown","description":"reject agent"}),
        ),
    ) {
        Err(Error::Tool(message)) => {
            assert_eq!(message, "task: requested agent is unavailable");
        }
        Err(error) => panic!("unexpected task error: {error}"),
        Ok(_) => panic!("ineligible task agent should not be authorized"),
    }
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn task_rejects_an_oversized_unicode_result() {
    let temporary = TemporaryDirectory::new();
    let agents = temporary.path.join("agents");
    let skills = temporary.path.join("skills");
    fs::create_dir_all(&agents).unwrap();
    fs::create_dir_all(&skills).unwrap();
    write_agent(&agents, "worker", "worker agent", "subagent");

    let missing = temporary.path.join("missing");
    let agents = AgentCatalog::discover(&[], &agents, &missing)
        .unwrap()
        .catalog()
        .clone();
    let skills = SkillCatalog::discover(&skills, &missing)
        .unwrap()
        .catalog()
        .clone();
    let mut task = TaskTool::from_catalogs_with_model_validator(
        agents,
        skills,
        "parent-model",
        TaskModels,
        OversizedTaskRunner,
    );

    let output = task
        .execute(
            &ToolExecutionContext::with_timeout(std::time::Duration::from_secs(1)),
            serde_json::json!({"description":"😀".repeat(16_384)}),
        )
        .unwrap();

    assert!(!output.is_error);
    assert_eq!(
        output.content.chars().count(),
        65_536 + TRUNCATION_MARKER.chars().count(),
        "the budget counts unicode scalars, not the bytes they encode to"
    );
    assert!(output.content.ends_with(TRUNCATION_MARKER));
}

#[test]
fn task_description_limit_counts_unicode_scalars_not_bytes() {
    let accepted = "😀".repeat(16_384);
    assert!(accepted.len() > 16_384);

    let mut task = task_tool(TerminalTaskRunner::Success);
    assert_eq!(
        task.execute(
            &task_context(),
            serde_json::json!({"description": accepted}),
        )
        .unwrap(),
        ToolOutput::success("done")
    );

    let rejected = "😀".repeat(16_385);
    assert!(rejected.len() > 16_385);
    assert_eq!(
        task.execute(
            &task_context(),
            serde_json::json!({"description": rejected}),
        )
        .unwrap(),
        ToolOutput::failure("task: input exceeds configured bounds")
    );
}

#[test]
fn task_reports_exact_terminal_taxonomy_without_runner_details() {
    for (runner, expected, terminal) in [
        (
            TerminalTaskRunner::Provider,
            "task: provider failure [cause: response protocol]",
            HeadlessTaskTerminal::ProviderFailure,
        ),
        (
            TerminalTaskRunner::Child,
            "task: child execution failed",
            HeadlessTaskTerminal::ChildFailure,
        ),
        (
            TerminalTaskRunner::Panic,
            "task: child execution failed",
            HeadlessTaskTerminal::ChildFailure,
        ),
        (
            TerminalTaskRunner::DeclarationRejected,
            "task: agent permission declaration rejected \
             [declaration: native::bash; the configuration denies it]",
            HeadlessTaskTerminal::DeclarationRejected,
        ),
    ] {
        let mut task = task_tool(runner);
        let output = task.execute(&task_context(), task_arguments()).unwrap();
        assert!(output.is_error);
        assert_eq!(output.content, expected);
        assert_eq!(output.terminal(), Some(terminal));
        assert!(!output.content.contains("secret panic payload"));
    }
}

/// A rejected declaration is the one failure the operator can act on directly,
/// so the offending name reaches the model-visible result instead of only the
/// diagnostics log. A name that is not shaped like a tool identity is withheld,
/// because a terminal message bypasses the failure sanitizer entirely.
#[test]
fn a_rejected_declaration_names_the_offending_tool_and_withholds_an_unsafe_one() {
    let mut task = task_tool(TerminalTaskRunner::DeclarationRejectedWithUnsafeName);
    let output = task.execute(&task_context(), task_arguments()).unwrap();

    assert!(output.is_error);
    assert_eq!(
        output.content,
        "task: agent permission declaration rejected \
         [declaration: [redacted]; the parent does not hold it]"
    );
    assert!(!output.content.contains("secret"));
}

#[test]
fn task_shares_four_permits_only_across_clones() {
    let (started_sender, started_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let task = task_tool(BlockingTaskRunner {
        started: started_sender,
        release: Mutex::new(release_receiver),
    });
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut calls = Vec::new();

    for _ in 0..4 {
        let mut clone = task.clone();
        let cancellation = Arc::clone(&cancellation);
        calls.push(thread::spawn(move || {
            clone
                .execute(
                    &ToolExecutionContext::new(cancellation, PARKED_CHILD_DEADLINE),
                    task_arguments(),
                )
                .unwrap()
        }));
    }

    wait_until_children_started(&started_receiver, 4);

    assert_eq!(
        task.clone()
            .execute(&task_context(), task_arguments())
            .unwrap(),
        ToolOutput::failure("task: concurrent child limit reached")
    );

    let mut independent = task_tool(TerminalTaskRunner::Success);
    assert_eq!(
        independent
            .execute(&task_context(), task_arguments())
            .unwrap(),
        ToolOutput::success("done")
    );

    for _ in 0..4 {
        release_sender.send(()).unwrap();
    }
    for call in calls {
        assert_eq!(call.join().unwrap(), ToolOutput::success("done"));
    }
}

#[test]
fn task_cancellation_wins_and_holds_permit_until_worker_exit() {
    let (started_sender, started_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let task = task_tool(BlockingTaskRunner {
        started: started_sender,
        release: Mutex::new(release_receiver),
    });
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut clone = task.clone();
    let worker_cancellation = Arc::clone(&cancellation);
    let call = thread::spawn(move || {
        clone
            .execute(
                &ToolExecutionContext::new(worker_cancellation, Duration::from_secs(1)),
                task_arguments(),
            )
            .unwrap()
    });

    wait_until_children_started(&started_receiver, 1);

    cancellation.store(true, Ordering::Release);
    assert_eq!(call.join().unwrap(), ToolOutput::failure("task: cancelled"));

    let mut clones = vec![task.clone(), task.clone(), task.clone()];
    let mut callers = Vec::new();
    for clone in &mut clones {
        let mut clone = clone.clone();
        callers.push(thread::spawn(move || {
            clone
                .execute(
                    &ToolExecutionContext::with_timeout(PARKED_CHILD_DEADLINE),
                    task_arguments(),
                )
                .unwrap()
        }));
    }
    wait_until_children_started(&started_receiver, 3);

    assert_eq!(
        task.clone()
            .execute(&task_context(), task_arguments())
            .unwrap(),
        ToolOutput::failure("task: concurrent child limit reached")
    );

    for _ in 0..4 {
        release_sender.send(()).unwrap();
    }
    for caller in callers {
        assert_eq!(caller.join().unwrap(), ToolOutput::success("done"));
    }
}

#[test]
fn foreground_task_inherits_the_parent_deadline_and_times_out_deterministically() {
    let (started_sender, started_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let task = task_tool(BlockingTaskRunner {
        started: started_sender,
        release: Mutex::new(release_receiver),
    });
    let mut clone = task.clone();
    let call = thread::spawn(move || {
        clone
            .execute(
                &ToolExecutionContext::with_timeout(Duration::from_millis(10)),
                task_arguments(),
            )
            .unwrap()
    });

    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let started = Instant::now();
    assert_eq!(call.join().unwrap(), ToolOutput::failure("task: timed out"));
    assert!(started.elapsed() < Duration::from_millis(250));
    release_sender.send(()).unwrap();
}

#[test]
fn background_task_retains_the_inherited_parent_deadline() {
    let (observed_sender, observed_receiver) = mpsc::channel();
    let mut task = task_tool(DeadlineAwareTaskRunner {
        observed: observed_sender,
    });

    let output = task
        .execute_with_launch_mode(
            &ToolExecutionContext::with_timeout(EXPIRING_PARENT_DEADLINE),
            task_arguments(),
            TaskLaunchMode::Background,
        )
        .unwrap();

    assert_eq!(
        output,
        ToolOutput::success("Subagent #1 running in background")
    );

    let (context_deadline, child_deadline) = observed_receiver
        .recv_timeout(CHILD_ADMISSION_TIMEOUT)
        .expect("the background worker never reached the runner before its deadline");
    assert!(context_deadline.is_some());
    assert_eq!(child_deadline, context_deadline);

    let deadline = Instant::now() + EXPIRING_PARENT_DEADLINE + CHILD_ADMISSION_TIMEOUT;
    loop {
        let snapshot = task
            .execution_registry()
            .snapshot(TaskExecutionId::from_value(1))
            .unwrap();
        if snapshot.terminal == Some(TaskTerminalState::Failed) {
            assert_eq!(
                snapshot.result,
                Some(ToolOutput::failure("task: timed out"))
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background task did not time out"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn background_task_retains_inherited_parent_cancellation() {
    let (observed_sender, observed_receiver) = mpsc::channel();
    let mut task = task_tool(DeadlineAwareTaskRunner {
        observed: observed_sender,
    });
    let parent_cancellation = Arc::new(AtomicBool::new(false));

    let output = task
        .execute_with_launch_mode(
            &ToolExecutionContext::new(Arc::clone(&parent_cancellation), Duration::from_secs(1)),
            task_arguments(),
            TaskLaunchMode::Background,
        )
        .unwrap();
    assert_eq!(
        output,
        ToolOutput::success("Subagent #1 running in background")
    );
    observed_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    parent_cancellation.store(true, Ordering::Release);

    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let snapshot = task
            .execution_registry()
            .snapshot(TaskExecutionId::from_value(1))
            .unwrap();
        if snapshot.terminal == Some(TaskTerminalState::Cancelled) {
            assert_eq!(
                snapshot.result,
                Some(ToolOutput::failure("task: cancelled"))
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background task did not inherit cancellation"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn u15_lifecycle_shared_id_modes_and_terminal_ownership() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut task = task_tool(LifecycleTaskRunner {
        calls: Arc::clone(&calls),
        observed: Arc::clone(&observed),
        terminal: None,
    });
    let foreground_output = task
        .execute_with_launch_mode(
            &task_context(),
            task_arguments(),
            TaskLaunchMode::Foreground,
        )
        .unwrap();
    assert!(matches!(
        foreground_output.content.as_str(),
        "done" | "Subagent #1 running in background"
    ));
    let mut background = task.clone();
    assert_eq!(
        background
            .execute_with_launch_mode(
                &task_context(),
                task_arguments(),
                TaskLaunchMode::Background
            )
            .unwrap(),
        ToolOutput::success("Subagent #2 running in background")
    );
    assert!(
        task.execution_registry()
            .wait_for_idle(Duration::from_secs(1))
    );
    let observed = observed.lock().unwrap().clone();
    let foreground = &observed[0];
    let background = &observed[1];

    let cancelled_observed = Arc::new(Mutex::new(Vec::new()));
    let mut cancelled = task_tool(LifecycleTaskRunner {
        calls: Arc::clone(&calls),
        observed: Arc::clone(&cancelled_observed),
        terminal: Some(TaskRunnerError::Cancelled),
    });
    assert_eq!(
        cancelled
            .execute_with_launch_mode(
                &task_context(),
                task_arguments(),
                TaskLaunchMode::Foreground
            )
            .unwrap(),
        ToolOutput::failure("task: cancelled")
    );
    let cancelled = cancelled_observed.lock().unwrap().pop().unwrap();

    assert_eq!(calls.load(Ordering::Acquire), 3);
    assert_ne!(foreground.id(), background.id());
    assert_eq!(foreground.mode(), TaskLaunchMode::Background);
    assert_eq!(
        foreground.events(),
        vec![
            TaskExecutionEvent::Admitted(foreground.id(), TaskLaunchMode::Foreground),
            TaskExecutionEvent::Backgrounded(foreground.id()),
            TaskExecutionEvent::Completed(foreground.id()),
        ]
    );
    assert_eq!(
        background.events(),
        vec![
            TaskExecutionEvent::Admitted(background.id(), TaskLaunchMode::Background),
            TaskExecutionEvent::Completed(background.id()),
        ]
    );
    assert!(!cancelled.finish(TaskTerminalState::Completed));
    assert_eq!(
        cancelled.events(),
        vec![
            TaskExecutionEvent::Admitted(cancelled.id(), TaskLaunchMode::Foreground),
            TaskExecutionEvent::Cancelled(cancelled.id()),
        ]
    );
}

#[test]
fn u15_background_task_invocation_uses_the_shared_task_lifecycle() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut task = task_tool(LifecycleTaskRunner {
        calls: Arc::new(AtomicUsize::new(0)),
        observed: Arc::clone(&observed),
        terminal: None,
    });

    assert_eq!(
        task.execute(
            &task_context(),
            serde_json::json!({
                "agent": "worker",
                "description": "background task",
                "background": true,
            }),
        )
        .unwrap(),
        ToolOutput::success("Subagent #1 running in background")
    );
    assert!(
        task.execution_registry()
            .wait_for_idle(Duration::from_secs(1))
    );

    let lifecycle = observed.lock().unwrap().pop().unwrap();
    assert_eq!(lifecycle.mode(), TaskLaunchMode::Background);
    assert_eq!(
        lifecycle.events(),
        vec![
            TaskExecutionEvent::Admitted(lifecycle.id(), TaskLaunchMode::Background),
            TaskExecutionEvent::Completed(lifecycle.id()),
        ]
    );
}

#[test]
fn u15_lifecycle_terminal_follows_cancellation_publication_winner() {
    let context =
        ToolExecutionContext::new(Arc::new(AtomicBool::new(false)), PARKED_CHILD_DEADLINE);
    let (paused_sender, paused_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let observed = Arc::new(Mutex::new(None));
    let cancellation = context.cancellation_handle();
    let mut task = task_tool(PublicationPausedTaskRunner {
        paused: paused_sender,
        release: Mutex::new(Some(release_receiver)),
        observed: Arc::clone(&observed),
    });
    let registry = task.execution_registry().clone();
    let call = thread::spawn(move || task.execute(&context, task_arguments()).unwrap());

    paused_receiver
        .recv_timeout(CHILD_ADMISSION_TIMEOUT)
        .expect("the child never paused before publishing its result");

    cancellation.store(true, Ordering::Release);

    assert_eq!(call.join().unwrap(), ToolOutput::failure("task: cancelled"));
    release_sender.send(()).unwrap();
    assert!(registry.wait_for_idle(CHILD_ADMISSION_TIMEOUT));

    let lifecycle = observed.lock().unwrap().clone().unwrap();
    assert_eq!(
        lifecycle.events(),
        vec![
            TaskExecutionEvent::Admitted(lifecycle.id(), TaskLaunchMode::Foreground),
            TaskExecutionEvent::Cancelled(lifecycle.id()),
        ]
    );
}

#[test]
fn task_registry_owns_session_ids_global_capacity_and_terminal_results() {
    let registry = TaskExecutionRegistry::new();
    let mut ids = Vec::new();

    for _ in 0..4 {
        ids.push(
            registry
                .admit(TaskLaunchMode::Foreground)
                .expect("registry capacity"),
        );
    }

    assert!(registry.admit(TaskLaunchMode::Background).is_none());
    assert_eq!(
        ids.iter().map(|id| id.value()).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );

    assert!(registry.finish(
        ids[0],
        TaskTerminalState::Completed,
        ToolOutput::success("done")
    ));
    assert!(!registry.finish(
        ids[0],
        TaskTerminalState::Failed,
        ToolOutput::failure("late failure"),
    ));
    assert_eq!(registry.result(ids[0]), Some(ToolOutput::success("done")));

    let replacement = registry
        .admit(TaskLaunchMode::Background)
        .expect("terminal execution releases capacity");
    assert_eq!(replacement.value(), 5);
}

#[test]
fn task_registry_snapshots_keep_active_work_ordered_and_report_pending_cancellation() {
    let registry = TaskExecutionRegistry::new();
    let foreground = registry.admit(TaskLaunchMode::Foreground).unwrap();
    let background = registry.admit(TaskLaunchMode::Background).unwrap();
    let terminal = registry.admit(TaskLaunchMode::Background).unwrap();

    assert!(registry.cancel(background));
    assert!(registry.finish(
        terminal,
        TaskTerminalState::Cancelled,
        ToolOutput::failure("task: cancelled"),
    ));

    assert_eq!(
        registry.active_snapshots(),
        vec![
            TaskExecutionSnapshot {
                id: foreground,
                mode: TaskLaunchMode::Foreground,
                cancellation_requested: false,
                terminal: None,
                result: None,
            },
            TaskExecutionSnapshot {
                id: background,
                mode: TaskLaunchMode::Background,
                cancellation_requested: true,
                terminal: None,
                result: None,
            },
        ]
    );

    assert!(!registry.cancel(terminal));
    assert!(!registry.cancel(TaskExecutionId::from_value(99)));
    assert_eq!(registry.active_snapshots().len(), 2);
}

#[test]
fn task_registry_cancel_all_reports_only_live_execution_ids() {
    let registry = TaskExecutionRegistry::new();
    let foreground = registry.admit(TaskLaunchMode::Foreground).unwrap();
    let background = registry.admit(TaskLaunchMode::Background).unwrap();
    let terminal = registry.admit(TaskLaunchMode::Background).unwrap();

    assert!(registry.finish(
        terminal,
        TaskTerminalState::Cancelled,
        ToolOutput::failure("task: cancelled"),
    ));

    assert_eq!(registry.cancel_all(), vec![foreground, background]);
    assert!(registry.is_cancelled(foreground));
    assert!(registry.is_cancelled(background));
    assert!(!registry.is_cancelled(terminal));
}

#[test]
fn background_tasks_return_immediately_and_admitted_children_run_concurrently() {
    let registry = TaskExecutionRegistry::new();
    let (started, started_rx) = mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let task = task_tool(ConcurrentTaskRunner {
        started,
        release: Arc::clone(&release),
    })
    .with_execution_registry(registry.clone());
    let mut calls = Vec::new();

    for _ in 0..4 {
        let mut task = task.clone();
        calls.push(thread::spawn(move || {
            task.execute(
                &task_context(),
                serde_json::json!({"description":"concurrent","background":true}),
            )
            .unwrap()
        }));
    }

    for _ in 0..4 {
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("all admitted children should start without runner serialization");
    }
    let mut outputs = calls
        .into_iter()
        .map(|call| call.join().unwrap().content)
        .collect::<Vec<_>>();
    outputs.sort();
    assert_eq!(
        outputs,
        (1..=4)
            .map(|id| format!("Subagent #{id} running in background"))
            .collect::<Vec<_>>(),
    );

    let (lock, wake) = &*release;
    *lock.lock().unwrap() = true;
    wake.notify_all();
    assert!(registry.wait_for_idle(Duration::from_secs(1)));
}

#[test]
fn task_registry_detach_and_cancel_control_live_execution_without_parent_timeout() {
    let registry = TaskExecutionRegistry::new();
    let id = registry.admit(TaskLaunchMode::Foreground).unwrap();

    assert_eq!(
        registry.control(TaskMessageSource::Main, id, TaskControlAction::Background),
        Ok(())
    );
    assert_eq!(
        registry.control(TaskMessageSource::Main, id, TaskControlAction::Cancel),
        Ok(())
    );
    assert!(registry.is_cancelled(id));
    assert!(registry.finish(
        id,
        TaskTerminalState::Cancelled,
        ToolOutput::failure("task: cancelled"),
    ));
    assert!(
        !registry
            .lifecycle(id)
            .unwrap()
            .finish(TaskTerminalState::Completed)
    );
}

#[test]
fn task_mailboxes_are_typed_fifo_bounded_and_reject_siblings_or_terminal_targets() {
    let registry = TaskExecutionRegistry::new();
    let first = registry.admit(TaskLaunchMode::Background).unwrap();
    let sibling = registry.admit(TaskLaunchMode::Background).unwrap();

    registry
        .send_message(
            TaskMessageSource::Main,
            TaskMessageTarget::Execution(first),
            "one".into(),
        )
        .unwrap();
    registry
        .send_message(
            TaskMessageSource::User,
            TaskMessageTarget::Execution(first),
            "two".into(),
        )
        .unwrap();
    assert!(
        registry
            .send_message(
                TaskMessageSource::Execution(first),
                TaskMessageTarget::Execution(sibling),
                "forbidden".into(),
            )
            .is_err()
    );
    registry
        .send_message(
            TaskMessageSource::Execution(first),
            TaskMessageTarget::Main,
            "child reply".into(),
        )
        .unwrap();

    let inbox = registry.drain_messages(TaskMessageTarget::Execution(first));
    assert_eq!(
        inbox
            .iter()
            .map(|message| (message.source(), message.content()))
            .collect::<Vec<_>>(),
        vec![
            (TaskMessageSource::Main, "one"),
            (TaskMessageSource::User, "two"),
        ]
    );
    assert_eq!(
        registry.drain_messages(TaskMessageTarget::Main)[0].content(),
        "child reply"
    );

    assert!(
        registry
            .send_message(
                TaskMessageSource::Main,
                TaskMessageTarget::Execution(first),
                "x".repeat(8 * 1024 + 1),
            )
            .is_err()
    );
    registry.finish(
        first,
        TaskTerminalState::Completed,
        ToolOutput::success("done"),
    );
    assert!(
        registry
            .send_message(
                TaskMessageSource::Main,
                TaskMessageTarget::Execution(first),
                "late".into(),
            )
            .is_err()
    );
}

#[test]
fn terminal_execution_notices_reach_the_main_mailbox_within_its_bounds() {
    let registry = TaskExecutionRegistry::new();
    let id = registry.admit(TaskLaunchMode::Background).unwrap();
    registry.finish(
        id,
        TaskTerminalState::Completed,
        ToolOutput::success("done"),
    );

    assert!(
        registry
            .send_message(
                TaskMessageSource::Execution(id),
                TaskMessageTarget::Main,
                "terminal source is refused".into(),
            )
            .is_err()
    );
    registry
        .notify_main(id, "subagent #1 finished".into())
        .unwrap();
    assert!(
        registry
            .notify_main(TaskExecutionId::from_value(99), "unknown".into())
            .is_err()
    );
    assert!(registry.notify_main(id, String::new()).is_err());
    assert!(registry.notify_main(id, "x".repeat(8 * 1024 + 1)).is_err());

    let inbox = registry.drain_messages(TaskMessageTarget::Main);
    assert_eq!(
        inbox
            .iter()
            .map(|message| (message.source(), message.content()))
            .collect::<Vec<_>>(),
        vec![(TaskMessageSource::Execution(id), "subagent #1 finished")]
    );
}

#[test]
fn task_control_and_message_tools_share_registry_and_enforce_caller_routes() {
    let registry = TaskExecutionRegistry::new();
    let first = registry.admit(TaskLaunchMode::Foreground).unwrap();
    let sibling = registry.admit(TaskLaunchMode::Background).unwrap();
    let mut parent_control = TaskControlTool::new(registry.clone(), TaskMessageSource::Main);
    let mut parent_message = TaskMessageTool::new(registry.clone(), TaskMessageSource::Main);
    let mut child_message =
        TaskMessageTool::new(registry.clone(), TaskMessageSource::Execution(first));

    assert_eq!(
        parent_control
            .execute(
                &task_context(),
                serde_json::json!({"action":"background","id":first.value()}),
            )
            .unwrap(),
        ToolOutput::success(format!("Subagent #{} moved to background", first.value())),
    );
    assert_eq!(
        parent_message
            .execute(
                &task_context(),
                serde_json::json!({"target":first.value(),"message":"continue"}),
            )
            .unwrap(),
        ToolOutput::success("task message queued"),
    );
    assert_eq!(
        child_message
            .execute(
                &task_context(),
                serde_json::json!({"target":"main","message":"report"}),
            )
            .unwrap(),
        ToolOutput::success("task message queued"),
    );
    assert!(
        child_message
            .execute(
                &task_context(),
                serde_json::json!({"target":sibling.value(),"message":"forbidden"}),
            )
            .unwrap()
            .is_error
    );
    assert!(registry.finish(
        first,
        TaskTerminalState::Completed,
        ToolOutput::success("final result"),
    ));
    assert_eq!(
        parent_control
            .execute(
                &task_context(),
                serde_json::json!({"action":"status","id":first.value()}),
            )
            .unwrap(),
        ToolOutput::success(format!(
            "Subagent #{}: completed\nfinal result",
            first.value()
        )),
    );
    assert_eq!(
        TaskControlTool::input_schema()["properties"]["action"]["enum"],
        serde_json::json!(["background", "cancel", "status"]),
    );
    assert_eq!(
        TaskMessageTool::input_schema()["properties"]["message"]["maxLength"],
        8192,
    );
}

/// Two rules that resolve to the same selector and target collapse into one
/// descriptor carrying the prevailing decision, not the later one: `files_read`
/// and `native::files_read` name the same tool, so the deny holds against the
/// allow that follows it.
#[test]
fn effective_capabilities_normalize_aliases_globs_projects_and_prevailing_decisions() {
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
    assert_eq!(set.descriptors()[0].decision(), PermissionDecision::Deny);
    assert_eq!(set.descriptors()[1].decision(), PermissionDecision::Ask);
    assert!(set.descriptors()[1].matches_identity("native:10:files_read"));
    assert!(set.descriptors()[1].matches_identity("native:11:files_write"));
}

#[test]
fn effective_capabilities_round_trip_a_declared_bash_target_as_free_form_text() {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::bash", ToolAccess::Write, InertTool)
        .unwrap();
    let agent = agent_with_rules(vec![PermissionRule::global(
        PermissionDecision::Deny,
        PermissionPattern::Exact("native::bash".into()),
        PermissionPattern::glob_for_target_kind("rm*", PermissionTargetKind::FreeFormText).unwrap(),
    )]);

    let set = EffectiveCapabilitySet::from_agent(&agent, "project", &dispatcher);
    let rules = set.permission_rules();
    assert_eq!(
        rules.len(),
        1,
        "the declared bash rule should survive the round trip"
    );
    let rebuilt = &rules[0];

    assert!(
        rebuilt.target.matches("rm -rf /tmp/x"),
        "a bash target reconstructed from its selector's tool identity must stay free-form, \
         not fall back to path-shaped segment discipline"
    );
}

#[test]
fn a_wildcard_tool_pattern_spanning_differing_target_kinds_keeps_each_kind_of_its_own() {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::bash", ToolAccess::Write, InertTool)
        .unwrap();
    dispatcher
        .register_native("native::write", ToolAccess::Write, InertTool)
        .unwrap();

    let agent = agent_with_rules(vec![PermissionRule::global(
        PermissionDecision::Deny,
        PermissionPattern::glob("*").unwrap(),
        PermissionPattern::glob("rm*").unwrap(),
    )]);

    let set = EffectiveCapabilitySet::from_agent(&agent, "project", &dispatcher);
    let rules = set.permission_rules();

    let bash_rule = rules
        .iter()
        .find(|rule| rule.tool.matches("native:4:bash"))
        .expect("bash must still have a reconstructed rule");
    let write_rule = rules
        .iter()
        .find(|rule| rule.tool.matches("native:5:write"))
        .expect("write must still have a reconstructed rule");

    assert!(
        bash_rule.target.matches("rm -rf /tmp/x"),
        "bash's own target must stay free-form even though it shares a wildcard with a \
         path-shaped tool"
    );
    assert!(
        !write_rule.target.matches("rm -rf /tmp/x"),
        "write's own target must stay path-shaped even though it shares a wildcard with bash"
    );
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
        0,
        "the allow collapses into the deny that names the same tool"
    );
    assert_eq!(set.descriptors()[0].decision(), PermissionDecision::Deny);
    assert!(set.descriptors()[0].matches_identity("native:10:files_read"));
    assert!(set.descriptors()[1].matches_identity("native:10:files_read"));
    assert!(set.descriptors()[1].matches_identity("native:11:files_write"));
}

#[test]
fn a_partial_wildcard_tool_pattern_still_produces_a_descriptor_on_the_parent_path() {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::bash", ToolAccess::Write, InertTool)
        .unwrap();
    dispatcher
        .register_native("native::files_write", ToolAccess::Write, InertTool)
        .unwrap();

    let agent = agent_with_rules(vec![PermissionRule::global(
        PermissionDecision::Deny,
        PermissionPattern::glob("bas*").unwrap(),
        PermissionPattern::Any,
    )]);

    let set = EffectiveCapabilitySet::from_agent(&agent, "project", &dispatcher);

    assert_eq!(
        set.descriptors().len(),
        1,
        "a partial-wildcard tool pattern that matches a known native tool must not vanish"
    );
    assert!(set.descriptors()[0].matches_identity("native:4:bash"));
    assert_eq!(set.descriptors()[0].decision(), PermissionDecision::Deny);
}

/// A `deny` or `ask` naming a tool the dispatcher does not hold must survive
/// resolution, symmetrically with the delegated-child surface. The same
/// `permission_rules` are shared by both paths, so a rule naming an MCP tool
/// absent from one of them has to keep meaning the same thing on the other.
#[test]
fn a_deny_naming_a_tool_the_dispatcher_lacks_is_retained_rather_than_dropped() {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::files_read", ToolAccess::ReadOnly, InertTool)
        .unwrap();

    for decision in [PermissionDecision::Deny, PermissionDecision::Ask] {
        let set = EffectiveCapabilitySet::from_agent(
            &agent_with_rules(vec![PermissionRule::global(
                decision,
                PermissionPattern::glob("mcp::github::create_*").unwrap(),
                PermissionPattern::Any,
            )]),
            "project",
            &dispatcher,
        );

        assert_eq!(set.descriptors().len(), 1, "{decision:?} must be retained");
        let rules = set.permission_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].decision, decision);
        assert!(rules[0].tool.matches("mcp::github::create_issue"));
        assert!(!rules[0].tool.matches("native:10:files_read"));
    }
}

/// An `allow` for a tool the dispatcher does not hold grants nothing, so it is
/// dropped rather than retained — the one asymmetry with a `deny`, and the
/// direction that can never widen anything.
#[test]
fn an_allow_naming_a_tool_the_dispatcher_lacks_is_dropped() {
    let dispatcher = ToolDispatcher::new();
    let set = EffectiveCapabilitySet::from_agent(
        &agent_with_rules(vec![PermissionRule::global(
            PermissionDecision::Allow,
            PermissionPattern::glob("mcp::github::create_*").unwrap(),
            PermissionPattern::Any,
        )]),
        "project",
        &dispatcher,
    );

    assert!(set.descriptors().is_empty());
}

/// Descriptor order is authoring order, deliberately: these descriptors are
/// read back by the surfaces that display an agent's declarations, and a set
/// reordered by selector would show the operator something they did not write.
/// It decides nothing — precedence is read off what each rule selects — so this
/// pins presentation, not policy.
#[test]
fn capability_descriptors_follow_declaration_order() {
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

    assert_eq!(forward.descriptors().len(), 3);
    assert_eq!(reverse.descriptors().len(), 3);
    assert_eq!(
        forward
            .descriptors()
            .iter()
            .map(EffectiveCapabilityDescriptor::decision)
            .collect::<Vec<_>>(),
        vec![
            PermissionDecision::Allow,
            PermissionDecision::Deny,
            PermissionDecision::Ask,
        ]
    );
    assert_eq!(
        reverse
            .descriptors()
            .iter()
            .map(EffectiveCapabilityDescriptor::decision)
            .collect::<Vec<_>>(),
        vec![
            PermissionDecision::Ask,
            PermissionDecision::Deny,
            PermissionDecision::Allow,
        ]
    );
    assert!(!forward.is_expansion_from(&reverse));
    assert!(!reverse.is_expansion_from(&forward));
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
    fn run(
        &self,
        request: TaskTurnRequest,
        _: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        Ok(TaskTurnResult {
            output: format!(
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
            ),
        })
    }
}

struct CountingTaskRunner(Arc<AtomicUsize>);

impl TaskRunner for CountingTaskRunner {
    fn run(
        &self,
        _: TaskTurnRequest,
        _: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Ok(TaskTurnResult {
            output: "unexpected child execution".into(),
        })
    }
}

type CapturedTaskModels = Arc<Mutex<Vec<(String, Option<ReasoningEffort>)>>>;

struct CapturingTaskRunner(CapturedTaskModels);

impl TaskRunner for CapturingTaskRunner {
    fn run(
        &self,
        request: TaskTurnRequest,
        _: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        self.0.lock().unwrap().push((
            request.model().to_owned(),
            request.request_config().reasoning_effort(),
        ));
        Ok(TaskTurnResult {
            output: "captured".into(),
        })
    }
}

const TRUNCATION_MARKER: &str =
    "\n[truncated: only the first 65536 characters of this subagent result were returned]";

struct SizedTaskRunner(usize);

impl TaskRunner for SizedTaskRunner {
    fn run(
        &self,
        _: TaskTurnRequest,
        _: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        Ok(TaskTurnResult {
            output: "x".repeat(self.0),
        })
    }
}

/// Refuses exactly one model identifier the way a provider refuses one it no
/// longer serves, and records every model it was asked to run.
struct RejectingTaskRunner {
    rejected: String,
    calls: Arc<Mutex<Vec<String>>>,
}

impl TaskRunner for RejectingTaskRunner {
    fn run(
        &self,
        request: TaskTurnRequest,
        _: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        self.calls.lock().unwrap().push(request.model().to_owned());
        if request.model() == self.rejected {
            return Err(TaskRunnerError::ProviderFailure(
                agens_tools::TaskProviderFailure::Rejected,
            ));
        }
        Ok(TaskTurnResult {
            output: "retried".into(),
        })
    }
}

struct AgentNamingTaskRunner(Arc<Mutex<Vec<String>>>);

impl TaskRunner for AgentNamingTaskRunner {
    fn run(
        &self,
        request: TaskTurnRequest,
        _: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        self.0.lock().unwrap().push(request.agent_name().to_owned());
        Ok(TaskTurnResult {
            output: "done".into(),
        })
    }
}

struct OversizedTaskRunner;

impl TaskRunner for OversizedTaskRunner {
    fn run(
        &self,
        _: TaskTurnRequest,
        _: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        Ok(TaskTurnResult {
            output: "x".repeat(65_537),
        })
    }
}

enum TerminalTaskRunner {
    Success,
    Provider,
    Child,
    Panic,
    DeclarationRejected,
    DeclarationRejectedWithUnsafeName,
}

impl TaskRunner for TerminalTaskRunner {
    fn run(
        &self,
        _: TaskTurnRequest,
        _: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        match self {
            Self::Success => Ok(TaskTurnResult {
                output: "done".into(),
            }),
            Self::Provider => Err(TaskRunnerError::ProviderFailure(
                agens_tools::TaskProviderFailure::Protocol,
            )),
            Self::Child => Err(TaskRunnerError::ChildFailure),
            Self::Panic => panic!("secret panic payload"),
            Self::DeclarationRejected => Err(TaskRunnerError::DeclarationRejected {
                reason: agens_tools::TaskDeclarationRejection::ConfigurationDenies,
                tool: "native::bash".into(),
            }),
            Self::DeclarationRejectedWithUnsafeName => Err(TaskRunnerError::DeclarationRejected {
                reason: agens_tools::TaskDeclarationRejection::ExceedsParentSurface,
                tool: "/home/secret user/tools".into(),
            }),
        }
    }
}

struct BlockingTaskRunner {
    started: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

struct DeadlineAwareTaskRunner {
    observed: mpsc::Sender<(Option<Instant>, Option<Instant>)>,
}

impl TaskRunner for DeadlineAwareTaskRunner {
    fn run(
        &self,
        _: TaskTurnRequest,
        context: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        let child_cancellation = context.turn_cancellation();
        self.observed
            .send((
                context.deadline(),
                child_cancellation.adapter_view().deadline(),
            ))
            .unwrap();
        while !child_cancellation.is_cancelled() && !child_cancellation.is_expired() {
            thread::sleep(Duration::from_millis(1));
        }
        if child_cancellation.is_expired() {
            Err(TaskRunnerError::TimedOut)
        } else {
            Err(TaskRunnerError::Cancelled)
        }
    }
}

struct ConcurrentTaskRunner {
    started: mpsc::Sender<()>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl TaskRunner for ConcurrentTaskRunner {
    fn run(
        &self,
        _: TaskTurnRequest,
        _: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        self.started.send(()).unwrap();
        let (lock, wake) = &*self.release;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = wake.wait(released).unwrap();
        }

        Ok(TaskTurnResult {
            output: "done".into(),
        })
    }
}

struct LifecycleTaskRunner {
    calls: Arc<AtomicUsize>,
    observed: Arc<Mutex<Vec<TaskExecutionLifecycle>>>,
    terminal: Option<TaskRunnerError>,
}

struct PublicationPausedTaskRunner {
    paused: mpsc::Sender<()>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
    observed: Arc<Mutex<Option<TaskExecutionLifecycle>>>,
}

impl TaskRunner for PublicationPausedTaskRunner {
    fn run(
        &self,
        _: TaskTurnRequest,
        context: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        let lifecycle = context.execution().cloned().expect("admitted lifecycle");
        self.observed.lock().unwrap().replace(lifecycle);
        let paused = self.paused.clone();
        let release = self
            .release
            .lock()
            .unwrap()
            .take()
            .expect("single publication pause");
        context.set_before_publication_hook(move || {
            paused.send(()).unwrap();
            release.recv().unwrap();
        });
        Ok(TaskTurnResult {
            output: "done".into(),
        })
    }
}

impl TaskRunner for LifecycleTaskRunner {
    fn run(
        &self,
        _: TaskTurnRequest,
        context: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let lifecycle = context.execution().cloned().expect("admitted lifecycle");
        if self.terminal.is_none() && lifecycle.mode() == TaskLaunchMode::Foreground {
            assert!(lifecycle.transition_to_background());
        }
        self.observed.lock().unwrap().push(lifecycle);
        if let Some(error) = self.terminal.clone() {
            return Err(error);
        }
        Ok(TaskTurnResult {
            output: "done".into(),
        })
    }
}

impl TaskRunner for BlockingTaskRunner {
    fn run(
        &self,
        _: TaskTurnRequest,
        _: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        self.started.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        Ok(TaskTurnResult {
            output: "done".into(),
        })
    }
}

/// Blocks until `count` further children have entered [`BlockingTaskRunner`].
///
/// The task tool admits a child — taking its concurrency permit — before
/// spawning the worker that calls the runner, and [`BlockingTaskRunner`] then
/// parks until it is released. Draining that many start signals therefore
/// proves that many permits are held simultaneously, which a sleep cannot: a
/// sleep only bounds how long the children were *given* to start, so on a
/// loaded machine it asserts against children that were never admitted.
fn wait_until_children_started(started: &mpsc::Receiver<()>, count: usize) {
    for admitted in 0..count {
        started
            .recv_timeout(CHILD_ADMISSION_TIMEOUT)
            .unwrap_or_else(|_| {
                panic!("only {admitted} of {count} children reached the blocking runner")
            });
    }
}

fn task_tool<R: TaskRunner>(runner: R) -> TaskTool<R> {
    let temporary = TemporaryDirectory::new();
    let agents = temporary.path.join("agents");
    let skills = temporary.path.join("skills");
    fs::create_dir_all(&agents).unwrap();
    fs::create_dir_all(&skills).unwrap();
    write_agent(&agents, "worker", "worker agent", "subagent");
    let missing = temporary.path.join("missing");
    let agents = AgentCatalog::discover(&[], &agents, &missing)
        .unwrap()
        .catalog()
        .clone();
    let skills = SkillCatalog::discover(&skills, &missing)
        .unwrap()
        .catalog()
        .clone();
    TaskTool::from_catalogs_with_model_validator(agents, skills, "parent-model", TaskModels, runner)
}

fn task_context() -> ToolExecutionContext {
    ToolExecutionContext::with_timeout(Duration::from_secs(1))
}

fn task_arguments() -> Value {
    serde_json::json!({"description":"test task"})
}

fn agent_with_rules(permission_rules: Vec<PermissionRule>) -> AgentDefinition {
    AgentDefinition {
        name: "agent".into(),
        description: "agent".into(),
        mode: AgentMode::Primary,
        model: None,
        model_source: None,
        reasoning_effort: None,
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

/// Distinguishes directories created within the same process, where the pid is
/// shared and the clock can report the same nanosecond twice.
static NEXT_TEMPORARY_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    /// A private temporary directory keyed on the pid, a process-local
    /// sequence number and the wall clock.
    ///
    /// No two live processes share a pid and no two calls in one process share
    /// a sequence number, so concurrent runs cannot collide; the timestamp only
    /// separates a fresh directory from one a killed process left behind under
    /// a since-recycled pid. `create_dir` rather than `create_dir_all` keeps
    /// any residual collision loud instead of silently sharing state.
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_TEMPORARY_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agens-extensions-{}-{sequence}-{timestamp}",
            std::process::id()
        ));

        fs::create_dir(&path).unwrap();

        Self { path }
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
