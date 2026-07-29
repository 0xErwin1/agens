//! Tests for the `task` tool: parent selection, model inheritance and registration.
//!
//! They live in the binary crate rather than in `agens-tool-runtime` because
//! they drive the TUI permission bridge to answer a prompt, and a logic crate
//! must not gain a surface even in its test build.
#![cfg(test)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_core::{
    HeadlessTurnCancellation, PermissionDecision, PermissionMode, PermissionPattern,
    PermissionPolicy, PermissionRule, PermissionSession, ToolAccess,
};
use agens_tool_runtime::runner::ProductionTaskRunner;
use agens_tool_runtime::runtime::production_tool_runtime_with_parent_task_runner;
use agens_tools::{SkillCatalog, TaskLaunchMode};

use agens_tools::{ToolDispatchRequest, ToolEvaluationOutcome, ToolExecutionContext};

use agens_fixtures::{
    session_bootstrap as tui_session_bootstrap, session_directory as tui_session_directory,
};
use agens_tool_runtime::task::*;
use agens_tui_app::permission_prompt::{TuiPermissionPrompter, production_tui_permission_bridge};

#[test]
fn a_task_runtimes_permission_policy_is_scoped_to_its_own_root_not_the_bootstraps_process_root() {
    use agens_core::PermissionRequest;

    use crate::CliDependencies;
    use crate::deps::bootstrap;

    let temporary = std::env::temp_dir().join(format!(
        "agens-task-runtime-permission-scope-{}",
        std::process::id()
    ));
    let config_home = temporary.join("config");
    let root_b = temporary.join("root-b/project");
    let root_a = temporary.join("root-a/project");

    // `config_reader` in this fixture answers for ANY path present in this map, mirroring
    // the production `read_file` capability, which can re-read a different root's document
    // on demand rather than only the one path `bootstrap()` itself resolved.
    let mut files = BTreeMap::new();
    files.insert(
        config_home.join("config.toml"),
        "[provider]\ntype = \"openai-api\"\nmodel = \"gpt-4.1\"\n".to_owned(),
    );
    files.insert(
        root_b.join(".agens/config.toml"),
        "[permissions]\nallow = [\"write\"]\n".to_owned(),
    );
    files.insert(
        root_a.join(".agens/config.toml"),
        "[permissions]\nallow = [\"write\"]\n".to_owned(),
    );

    let bootstrap_from_root_b = bootstrap(&CliDependencies::for_test(
        root_b.clone(),
        Some(temporary.join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        files,
    ))
    .unwrap();

    let evaluate_write_decision = |root: &Path| {
        std::fs::create_dir_all(root).unwrap();
        let runtime = production_tui_task_runtime_with_runner_and_parent_config(
            &bootstrap_from_root_b,
            root,
            &SkillCatalog::default(),
            Box::new(TuiPermissionPrompter(production_tui_permission_bridge().0)),
            ProductionTaskRunner::new(bootstrap_from_root_b.clone(), root.to_path_buf()),
            agens_core::RequestConfig::default(),
            None,
        )
        .unwrap();
        let write_identity = runtime
            .dispatcher
            .lock()
            .unwrap()
            .canonical_identity("native::write")
            .unwrap()
            .as_str()
            .to_owned();
        runtime.authorized.gate.policy.evaluate(
            &PermissionRequest::new(
                root.display().to_string(),
                write_identity,
                "notes.md",
                ToolAccess::Write,
            ),
            &[],
            &PermissionSession::new(),
        )
    };

    // `bootstrap_from_root_b` discovered its process-scoped configuration from root B, which
    // grants `write`. Building a runtime for root A — a DIFFERENT recorded root, which also
    // happens to grant `write` in ITS OWN config — must authorize from root A's own document,
    // never from the bootstrap's process-captured one.
    assert_eq!(
        evaluate_write_decision(&root_a),
        PermissionDecision::Allow,
        "a permission rule written for THIS root's own project config must still authorize"
    );

    // Removing root A's own grant must remove the authorization too, even though the
    // bootstrap's process-captured rules (from root B) still grant `write` unconditionally.
    // If the runtime were still reading `bootstrap.permission_rules()`, this would stay
    // `Allow`.
    let root_a_without_its_own_grant = temporary.join("root-a-bare/project");
    assert_eq!(
        evaluate_write_decision(&root_a_without_its_own_grant),
        PermissionDecision::Ask,
        "a permission rule written for a DIFFERENT project root's config must not silently \
         auto-authorize a tool call in this root's task runtime"
    );

    std::fs::remove_dir_all(&temporary).ok();
    std::fs::remove_dir_all(bootstrap_from_root_b.data_directory()).ok();
}

#[test]
fn u15_a1b1_production_task_runtime_assembles_current_turn_registration() {
    let temporary = tui_session_directory("production-task-runtime");
    let mut bootstrap = tui_session_bootstrap(
        &temporary,
        &[(
            "reviewer",
            "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
        )],
    );
    bootstrap.model = Some("gpt-5.6-sol".into());
    let probe = Arc::new(Mutex::new(Vec::new()));
    let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);
    let runtime = production_tui_task_runtime_with_runner_and_parent_config(
        &bootstrap,
        &project_root,
        &SkillCatalog::default(),
        Box::new(TuiPermissionPrompter(production_tui_permission_bridge().0)),
        ProductionTaskRunner::with_probe(
            bootstrap.clone(),
            project_root.clone(),
            Arc::clone(&probe),
        ),
        agens_core::RequestConfig::with_reasoning_effort("high").unwrap(),
        None,
    )
    .unwrap();

    assert!(
        runtime
            .provider_tools
            .iter()
            .any(|tool| tool.name() == "task")
    );
    let mut dispatcher = runtime.dispatcher.lock().unwrap();
    assert_eq!(
        dispatcher.canonical_identity("task"),
        dispatcher.canonical_identity("native::task")
    );
    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Allow,
            PermissionPattern::Exact("native::task".into()),
            PermissionPattern::Any,
        )],
    );
    let ToolEvaluationOutcome::Authorized(handle) = dispatcher
        .evaluate(
            &policy,
            &[],
            &PermissionSession::new(),
            ToolDispatchRequest::new(
                "project",
                "native::task",
                serde_json::json!({"agent":"reviewer","description":"probe"}),
            ),
        )
        .unwrap()
    else {
        panic!("registered task should authorize");
    };
    let cancellation = HeadlessTurnCancellation::new();
    let output = dispatcher
        .execute(
            handle,
            &ToolExecutionContext::from_headless_adapter(cancellation.adapter_view()),
        )
        .unwrap();
    assert_eq!(output.content, "probe");
    let probe = probe.lock().unwrap();
    assert_eq!(probe.len(), 1);
    assert_eq!(probe[0].1, TaskLaunchMode::Foreground);
    assert_eq!(probe[0].2, "gpt-5.6-sol");
    assert_eq!(probe[0].3, Some(agens_core::ReasoningEffort::High));

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_and_headless_task_tool_construction_delegate_profiles_identically() {
    let temporary = tui_session_directory("profile-parity");
    let mut bootstrap = tui_session_bootstrap(
        &temporary,
        &[(
            "reviewer",
            "---\nname: reviewer\ndescription: reviewer\nmode: subagent\nmodel: unavailable-model\neffort: low\n---\nReview work.\n",
        )],
    );
    bootstrap.model = Some("gpt-5.6-sol".into());
    bootstrap.debug = true;
    std::fs::create_dir_all(bootstrap.data_directory()).unwrap();
    let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);
    let parent_config = agens_core::RequestConfig::with_reasoning_effort("high").unwrap();
    let tui_probe = Arc::new(Mutex::new(Vec::new()));
    let tui = production_tui_task_runtime_with_runner_and_parent_config(
        &bootstrap,
        &project_root,
        &SkillCatalog::default(),
        Box::new(TuiPermissionPrompter(production_tui_permission_bridge().0)),
        ProductionTaskRunner::with_probe(
            bootstrap.clone(),
            project_root.clone(),
            Arc::clone(&tui_probe),
        ),
        parent_config.clone(),
        Some("abcd1234".into()),
    )
    .unwrap();
    let headless_probe = Arc::new(Mutex::new(Vec::new()));
    let (_, headless) = production_tool_runtime_with_parent_task_runner(
        &bootstrap,
        &project_root,
        Some(&SkillCatalog::default()),
        "gpt-5.6-sol".into(),
        parent_config,
        Some("abcd1234".into()),
        ProductionTaskRunner::with_probe(
            bootstrap.clone(),
            project_root.clone(),
            Arc::clone(&headless_probe),
        ),
    )
    .unwrap();
    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Allow,
            PermissionPattern::Exact("native::task".into()),
            PermissionPattern::Any,
        )],
    );
    let dispatch = |dispatcher: &agens_permissions::SharedToolDispatcher| {
        let mut dispatcher = dispatcher.lock().unwrap();
        let ToolEvaluationOutcome::Authorized(handle) = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new(
                    "project",
                    "native::task",
                    serde_json::json!({"agent":"reviewer","description":"parity"}),
                ),
            )
            .unwrap()
        else {
            panic!("task should authorize");
        };
        dispatcher
            .execute(
                handle,
                &ToolExecutionContext::with_timeout(std::time::Duration::from_secs(1)),
            )
            .unwrap()
    };

    let tui_output = dispatch(&tui.dispatcher);
    let headless_output = dispatch(&headless);

    assert_eq!(tui_output, headless_output);
    assert_eq!(*tui_probe.lock().unwrap(), *headless_probe.lock().unwrap());
    assert_eq!(
        *tui_probe.lock().unwrap(),
        vec![(
            agens_tools::TaskExecutionId::from_value(1),
            TaskLaunchMode::Foreground,
            "gpt-5.6-sol".to_owned(),
            Some(agens_core::ReasoningEffort::Low),
        )]
    );
    let diagnostics = std::fs::read_to_string(
        bootstrap
            .data_directory()
            .join("diagnostics")
            .join(format!("agens-{}.jsonl", std::process::id())),
    )
    .unwrap()
    .lines()
    .map(|line| {
        let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
        value.as_object_mut().unwrap().remove("timestamp_ms");
        value
    })
    .collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(diagnostics[0], diagnostics[1]);
    assert_eq!(diagnostics[0]["agent"], "reviewer");
    assert_eq!(diagnostics[0]["requested_model"], "unavailable-model");
    assert_eq!(diagnostics[0]["fallback_model"], "gpt-5.6-sol");

    std::fs::remove_dir_all(temporary).unwrap();
}
