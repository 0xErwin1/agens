//! What the run worker's floor and its denylist do to a call at the gate,
//! under exactly the configuration a worker runs with.
//!
//! A worker runs with `dangerously_allow_all`, which is `unmatched_allow` here
//! and a bypassing session there. Every case below sets both, and a grant that
//! allows the tool outright on top of them, so nothing in the permission
//! configuration is the reason a call is stopped.

use agens_core::{
    Denylist, DenylistClass, Error, PermissionMode, PermissionPattern, PermissionPolicy,
    PermissionSession, ProjectPermissionGrant, ToolAccess,
};
use agens_tools::{
    DispatchTool, ToolDispatchRequest, ToolDispatcher, ToolEvaluationOutcome, ToolExecutionContext,
    ToolOutput,
};
use serde_json::{Value, json};

const WORKTREE: &str = "/work/runs/42";
const PROJECT: &str = "project";

/// A tool that is only ever evaluated, never run: what is under test is the
/// decision, and the projection every native shares is the default one.
struct Projected;

impl DispatchTool for Projected {
    fn execute(&mut self, _: &ToolExecutionContext, _: Value) -> Result<ToolOutput, Error> {
        Ok(ToolOutput::success("executed"))
    }
}

fn dispatcher(confined: bool) -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new();

    for name in ["native::bash", "native::write"] {
        dispatcher
            .register_native(name, ToolAccess::Write, Projected)
            .expect("the native tool registers");
    }

    if confined {
        dispatcher.enforce_denylist(WORKTREE);
    }

    dispatcher
}

/// The one grant a worker could plausibly be carrying, allowing both tools
/// outright, so no case below is decided by the absence of an authorization.
fn grants() -> Vec<ProjectPermissionGrant> {
    ["native::bash", "native::write"]
        .into_iter()
        .map(|tool| {
            ProjectPermissionGrant::allow(
                PROJECT,
                PermissionPattern::Exact(tool.to_owned()),
                PermissionPattern::Any,
            )
        })
        .collect()
}

fn evaluate(dispatcher: &ToolDispatcher, tool: &str, target: &str) -> ToolEvaluationOutcome {
    dispatcher
        .evaluate_with_policy_override(
            &PermissionPolicy::new(PermissionMode::Edit, Vec::new()),
            &grants(),
            &PermissionSession::with_temporary_bypass(),
            ToolDispatchRequest::new(PROJECT, tool, json!({"target": target})),
            true,
        )
        .expect("the call is evaluable")
}

fn denylist_class(outcome: &ToolEvaluationOutcome) -> Option<DenylistClass> {
    match outcome {
        ToolEvaluationOutcome::PromptRequired(context) => context.denylist,
        _ => None,
    }
}

#[test]
fn the_floor_denies_a_write_outside_the_worktree_that_every_authorization_allows() {
    let outcome = evaluate(&dispatcher(true), "native::write", "/elsewhere/notes.md");

    assert!(
        matches!(outcome, ToolEvaluationOutcome::Denied),
        "a write outside the run's worktree is denied by the confinement floor: {outcome:?}"
    );
}

/// The same call in a session that is not executing a run is decided by its
/// authorizations alone. The floor is the worker's, not everyone's: an
/// interactive session may still write outside its root after a person says so.
#[test]
fn the_floor_belongs_to_the_run_worker_rather_than_to_every_session() {
    let outcome = evaluate(&dispatcher(false), "native::write", "/elsewhere/notes.md");

    assert!(
        matches!(outcome, ToolEvaluationOutcome::Authorized(_)),
        "an unconfined session keeps the reach its grants give it: {outcome:?}"
    );
}

#[test]
fn a_denylisted_command_is_escalated_rather_than_allowed_by_the_widened_default() {
    let dispatcher = dispatcher(true);

    for (command, class) in [
        ("git push origin head", DenylistClass::GitPush),
        (
            "sudo systemctl restart nginx",
            DenylistClass::PrivilegeEscalation,
        ),
        (
            "rm -rf /work/runs/41",
            DenylistClass::DeletionOutsideWorktree,
        ),
        ("cat ../secrets/.env", DenylistClass::SecretsAccess),
        ("terraform destroy", DenylistClass::IrreversibleOperation),
        ("agens team stop", DenylistClass::ServerLifecycle),
    ] {
        let outcome = evaluate(&dispatcher, "native::bash", command);

        assert_eq!(
            denylist_class(&outcome),
            Some(class),
            "{command} must be escalated as {}: {outcome:?}",
            class.id()
        );
    }
}

#[test]
fn an_ordinary_call_inside_the_worktree_still_reaches_the_widened_default() {
    for (tool, target) in [
        ("native::bash", "cargo test --workspace"),
        ("native::write", "crates/agens-core/src/lib.rs"),
    ] {
        let outcome = evaluate(&dispatcher(true), tool, target);

        assert!(
            matches!(outcome, ToolEvaluationOutcome::Authorized(_)),
            "{tool} on {target} is in-worktree, reversible and in scope: {outcome:?}"
        );
    }
}

#[test]
fn a_confined_dispatcher_reports_the_worktree_it_measures_scope_against() {
    let dispatcher = dispatcher(true);

    assert_eq!(dispatcher.denylist(), Some(&Denylist::new(WORKTREE)));
    assert_eq!(self::dispatcher(false).denylist(), None);
}
