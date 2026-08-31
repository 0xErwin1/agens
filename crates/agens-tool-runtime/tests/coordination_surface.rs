//! Which sessions are offered the `team_*` group, and which are not.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use agens_core::coordination::{CoordinationPort, UnavailableCoordinationPort};
use agens_core::run_introspection::{RunIntrospectionPort, UnavailableRunIntrospectionPort};
use agens_providers::OpenAiFunctionTool;
use agens_tools::{SkillCatalog, TaskRunner, TeamVerb};

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

fn parent_tools(
    label: &str,
    executing_a_run: bool,
    managing_a_team: bool,
) -> Vec<OpenAiFunctionTool> {
    let temporary = agens_fixtures::session_directory(label);
    let bootstrap = agens_fixtures::session_bootstrap(&temporary, &[]);
    let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);

    let (tools, _) = agens_tool_runtime::runtime::production_tool_runtime_with_team_surfaces(
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
            Arc::new(|| Box::new(UnavailableRunIntrospectionPort) as Box<dyn RunIntrospectionPort>)
                as agens_tool_runtime::runtime::RunIntrospectionFactory
        }),
        managing_a_team.then(|| {
            Arc::new(|| Box::new(UnavailableCoordinationPort) as Box<dyn CoordinationPort>)
                as agens_tool_runtime::runtime::CoordinationFactory
        }),
    )
    .expect("the parent runtime must build");

    fs::remove_dir_all(temporary).unwrap();

    tools
}

/// The group is exclusive to the session that manages a team. Everything else
/// would be offered ten tools whose every call answers that it manages none.
#[test]
fn a_session_that_manages_no_team_is_offered_none_of_the_group() {
    let tools = parent_tools("coordination-surface-off", false, false);
    let names = tool_names(&tools);

    for verb in TeamVerb::ALL {
        assert!(!names.contains(&verb.tool_name()), "{names:?}");
    }
}

#[test]
fn a_session_that_manages_a_team_is_offered_all_ten() {
    let tools = parent_tools("coordination-surface-on", false, true);
    let names = tool_names(&tools);

    for verb in TeamVerb::ALL {
        assert!(names.contains(&verb.tool_name()), "{names:?}");
    }
}

/// The two surfaces are independent: a worker executing a run reports through
/// `checkpoint` and `ask`, and manages nobody.
#[test]
fn a_worker_executing_a_run_manages_no_team() {
    let tools = parent_tools("coordination-surface-worker", true, false);
    let names = tool_names(&tools);

    assert!(names.contains(&"checkpoint"), "{names:?}");
    assert!(names.contains(&"ask"), "{names:?}");
    assert!(!names.contains(&"team_status"), "{names:?}");
    assert!(!names.contains(&"team_merge"), "{names:?}");
}

/// The description a caller reads is the tool's own, so the request verbs say
/// what they really do at the point the decision is made.
#[test]
fn the_group_carries_its_own_descriptions_onto_the_surface() {
    let tools = parent_tools("coordination-surface-descriptions", false, true);
    let merge = tools
        .iter()
        .find(|tool| tool.name() == "team_merge")
        .expect("team_merge is offered");

    assert_eq!(merge.description(), TeamVerb::Merge.description());
}
