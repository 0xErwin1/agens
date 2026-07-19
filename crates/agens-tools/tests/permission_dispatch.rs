use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use agens_core::{
    Error, PermissionDecision, PermissionMode, PermissionPattern, PermissionPolicy, PermissionRule,
    PermissionSession, ProjectPermissionGrant, ToolAccess,
};
use agens_tools::{
    DispatchTool, RemoteToolAccess, RemoteToolMetadata, ToolDispatchRequest, ToolDispatcher,
    ToolEvaluationOutcome, ToolExecutionContext, ToolOutput,
};
use serde_json::json;

struct CountingTool(Arc<AtomicUsize>, Result<ToolOutput, Error>);

impl DispatchTool for CountingTool {
    fn execute(
        &mut self,
        _: &ToolExecutionContext,
        _: serde_json::Value,
    ) -> Result<ToolOutput, Error> {
        self.0.fetch_add(1, Ordering::AcqRel);
        self.1.clone()
    }
}

fn request(project: &str, tool: &str, target: &str) -> ToolDispatchRequest {
    ToolDispatchRequest::new(
        project,
        tool,
        json!({"target": target, "secret": "SECRET_SENTINEL"}),
    )
}

#[test]
fn deny_and_ask_never_return_an_executable_capability() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::edit",
            ToolAccess::Write,
            CountingTool(Arc::clone(&calls), Ok(ToolOutput::success("ran"))),
        )
        .unwrap();
    let deny = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Deny,
            PermissionPattern::Exact("native::edit".into()),
            PermissionPattern::Any,
        )],
    );
    assert!(matches!(
        dispatcher.evaluate(
            &deny,
            &[],
            &PermissionSession::new(),
            request("p", "native::edit", "a")
        ),
        Ok(ToolEvaluationOutcome::Denied)
    ));
    assert!(matches!(
        dispatcher.evaluate(
            &PermissionPolicy::new(PermissionMode::Edit, vec![]),
            &[],
            &PermissionSession::new(),
            request("p", "native::edit", "a")
        ),
        Ok(ToolEvaluationOutcome::PromptRequired(_))
    ));
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn grant_authorizes_once_and_execution_receives_sanitized_infrastructure_failure() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::edit",
            ToolAccess::Write,
            CountingTool(
                Arc::clone(&calls),
                Err(Error::Extension("SECRET_SENTINEL stderr".into())),
            ),
        )
        .unwrap();
    let grant = ProjectPermissionGrant::allow(
        "p",
        PermissionPattern::Exact("native::edit".into()),
        PermissionPattern::Any,
    );
    let ToolEvaluationOutcome::Authorized(handle) = dispatcher
        .evaluate(
            &PermissionPolicy::new(PermissionMode::Edit, vec![]),
            &[grant],
            &PermissionSession::new(),
            request("p", "native::edit", "a"),
        )
        .unwrap()
    else {
        panic!("grant should authorize");
    };
    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert_eq!(
        dispatcher
            .execute(
                handle,
                &ToolExecutionContext::with_timeout(Duration::from_secs(1))
            )
            .unwrap(),
        ToolOutput::failure("tool infrastructure failure")
    );
    assert_eq!(calls.load(Ordering::Acquire), 1);
}

#[test]
fn temporary_bypass_does_not_override_chat_write_restrictions() {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::edit",
            ToolAccess::Write,
            CountingTool(
                Arc::new(AtomicUsize::new(0)),
                Ok(ToolOutput::success("ran")),
            ),
        )
        .unwrap();
    assert!(matches!(
        dispatcher.evaluate(
            &PermissionPolicy::new(PermissionMode::Chat, vec![]),
            &[],
            &PermissionSession::with_temporary_bypass(),
            request("p", "native::edit", "a")
        ),
        Ok(ToolEvaluationOutcome::Denied)
    ));
}

#[test]
fn missing_project_cannot_consume_a_grant() {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::edit",
            ToolAccess::Write,
            CountingTool(
                Arc::new(AtomicUsize::new(0)),
                Ok(ToolOutput::success("ran")),
            ),
        )
        .unwrap();
    let grant = ProjectPermissionGrant::allow("", PermissionPattern::Any, PermissionPattern::Any);
    assert!(matches!(
        dispatcher.evaluate(
            &PermissionPolicy::new(PermissionMode::Edit, vec![]),
            &[grant],
            &PermissionSession::new(),
            request("", "native::edit", "a")
        ),
        Ok(ToolEvaluationOutcome::PromptRequired(_))
    ));
}

#[test]
fn unknown_tools_are_rejected_before_policy_evaluation() {
    assert!(
        ToolDispatcher::new()
            .evaluate(
                &PermissionPolicy::new(PermissionMode::Edit, vec![]),
                &[],
                &PermissionSession::new(),
                request("p", "missing", "a")
            )
            .is_err()
    );
}

#[test]
fn later_mcp_registration_wins_a_native_model_alias_collision() {
    let native_calls = Arc::new(AtomicUsize::new(0));
    let mcp_calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::files_read",
            ToolAccess::ReadOnly,
            CountingTool(Arc::clone(&native_calls), Ok(ToolOutput::success("native"))),
        )
        .unwrap();
    let metadata = RemoteToolMetadata {
        qualified_name: "files::read".into(),
        server_name: "files".into(),
        tool_name: "read".into(),
        description: None,
        input_schema: json!({}),
        access: RemoteToolAccess::ReadOnly,
    };
    dispatcher
        .register_mcp(
            &metadata,
            CountingTool(Arc::clone(&mcp_calls), Ok(ToolOutput::success("mcp"))),
        )
        .unwrap();

    let identity = dispatcher
        .canonical_identity("files::read")
        .expect("legacy alias must resolve")
        .to_owned();
    assert_eq!(dispatcher.canonical_identity("files_read"), Some(&identity));
    assert_ne!(identity.as_str(), "files::read");

    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Allow,
            PermissionPattern::Exact("files::read".into()),
            PermissionPattern::Any,
        )],
    );
    let ToolEvaluationOutcome::Authorized(handle) = dispatcher
        .evaluate(
            &policy,
            &[ProjectPermissionGrant::allow(
                "project",
                PermissionPattern::Exact("files_read".into()),
                PermissionPattern::Any,
            )],
            &PermissionSession::new(),
            request("project", "files_read", "target"),
        )
        .expect("the later MCP registration should own the model alias")
    else {
        panic!("read-only MCP tool should be authorized");
    };

    assert_eq!(
        dispatcher
            .execute(
                handle,
                &ToolExecutionContext::with_timeout(Duration::from_secs(1)),
            )
            .unwrap(),
        ToolOutput::success("mcp")
    );
    assert_eq!(native_calls.load(Ordering::Acquire), 0);
    assert_eq!(mcp_calls.load(Ordering::Acquire), 1);
}
