//! Which sessions are offered the two introspection tools, and what a worker
//! reads about the one that costs it something.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use agens_core::run_introspection::{UnavailableRunIntrospectionPort, WORKER_CHECKPOINT_PROMPT};
use agens_providers::OpenAiFunctionTool;
use agens_tools::{SkillCatalog, TaskRunner};

/// Never reached: these tests only read the tool list the runtime built.
struct InertTaskRunner;

impl TaskRunner for InertTaskRunner {
    fn run(
        &self,
        request: agens_tools::TaskTurnRequest,
        _: &agens_tools::TaskRunContext,
    ) -> Result<agens_tools::TaskTurnResult, agens_tools::TaskRunnerError> {
        Ok(agens_tools::TaskTurnResult {
            output: request.description().to_owned(),
        })
    }
}

fn tool_names(tools: &[OpenAiFunctionTool]) -> Vec<&str> {
    tools.iter().map(OpenAiFunctionTool::name).collect()
}

fn parent_tools(label: &str, executing_a_run: bool) -> Vec<OpenAiFunctionTool> {
    let temporary = agens_fixtures::session_directory(label);
    let bootstrap = agens_fixtures::session_bootstrap(&temporary, &[]);
    let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);

    let (tools, _) = agens_tool_runtime::runtime::production_tool_runtime_with_run_introspection(
        &bootstrap,
        &project_root,
        Some(&SkillCatalog::default()),
        "gpt-5.5".to_owned(),
        agens_core::RequestConfig::default(),
        None,
        InertTaskRunner,
        Box::new(agens_core::ask_user::UnavailableAskUserPort),
        None,
        Arc::new(AtomicBool::new(false)),
        executing_a_run.then(|| {
            Arc::new(|| {
                Box::new(UnavailableRunIntrospectionPort)
                    as Box<dyn agens_core::run_introspection::RunIntrospectionPort>
            }) as agens_tool_runtime::runtime::RunIntrospectionFactory
        }),
    )
    .expect("the parent runtime must build");

    fs::remove_dir_all(temporary).unwrap();

    tools
}

/// An ordinary session has no run to checkpoint against and no run to park, so
/// it is not offered two tools whose every call would answer that.
#[test]
fn a_session_that_is_not_executing_a_run_is_offered_neither_tool() {
    let tools = parent_tools("introspection-surface-off", false);
    let names = tool_names(&tools);

    assert!(!names.contains(&"checkpoint"), "{names:?}");
    assert!(!names.contains(&"ask"), "{names:?}");
    assert!(
        names.contains(&"ask_user"),
        "ask_user is a different tool and stays: {names:?}"
    );
}

#[test]
fn a_session_executing_a_run_is_offered_both() {
    let tools = parent_tools("introspection-surface-on", true);
    let names = tool_names(&tools);

    assert!(names.contains(&"checkpoint"), "{names:?}");
    assert!(names.contains(&"ask"), "{names:?}");
}

/// The cost statement is the deliverable, not the schema: a worker that never
/// reads it has no reason to classify a claim honestly, because the class only
/// looks like metadata until somebody says what it does.
#[test]
fn the_checkpoint_tool_tells_the_worker_what_an_unproven_claim_costs() {
    let tools = parent_tools("introspection-surface-prompt", true);
    let checkpoint = tools
        .iter()
        .find(|tool| tool.name() == "checkpoint")
        .expect("the checkpoint tool is offered");

    assert_eq!(checkpoint.description(), WORKER_CHECKPOINT_PROMPT);
    assert!(
        checkpoint
            .description()
            .contains("Only a `deterministic` claim credits progress")
    );
}
