use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use agens_core::{
    Error, PermissionDecision, PermissionMode, PermissionPattern, PermissionPolicy,
    PermissionRequest, PermissionRule, PermissionSession, ProjectPermissionGrant, ToolAccess,
};
use agens_tools::{
    DispatchTool, RemoteToolAccess, RemoteToolMetadata, ToolDispatchOutcome, ToolDispatchRequest,
    ToolDispatcher, ToolOutput,
};
use serde_json::json;

struct CountingTool {
    calls: Arc<AtomicUsize>,
    result: Result<ToolOutput, Error>,
}

impl DispatchTool for CountingTool {
    fn execute(&mut self, _: serde_json::Value) -> Result<ToolOutput, Error> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.result.clone()
    }
}

fn request(project: &str, tool: &str, target: &str, access: ToolAccess) -> ToolDispatchRequest {
    ToolDispatchRequest::new(
        PermissionRequest::new(project, tool, target, access),
        json!({}),
    )
}

#[test]
fn deny_and_ask_do_not_execute_native_or_mcp_tools() {
    let native_calls = Arc::new(AtomicUsize::new(0));
    let mcp_calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::edit",
            ToolAccess::Write,
            CountingTool {
                calls: Arc::clone(&native_calls),
                result: Ok(ToolOutput::success("native")),
            },
        )
        .unwrap();
    dispatcher
        .register_mcp(
            &remote_metadata("server::write", RemoteToolAccess::Write),
            CountingTool {
                calls: Arc::clone(&mcp_calls),
                result: Ok(ToolOutput::success("mcp")),
            },
        )
        .unwrap();

    let deny_policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Deny,
            PermissionPattern::Exact("native::edit".into()),
            PermissionPattern::Any,
        )],
    );
    assert_eq!(
        dispatcher
            .dispatch(
                &deny_policy,
                &[],
                &PermissionSession::new(),
                request("project-a", "native::edit", "src/lib.rs", ToolAccess::Write),
            )
            .unwrap(),
        ToolDispatchOutcome::Denied
    );

    let ask_policy = PermissionPolicy::new(PermissionMode::Edit, Vec::new());
    assert_eq!(
        dispatcher
            .dispatch(
                &ask_policy,
                &[],
                &PermissionSession::new(),
                request("project-a", "server::write", "remote", ToolAccess::Write),
            )
            .unwrap(),
        ToolDispatchOutcome::PromptRequired(agens_tools::PermissionPromptContext {
            project_id: "project-a".into(),
            qualified_tool_name: "server::write".into(),
            target_identifier: "remote".into(),
            access: ToolAccess::Write,
            reason: "permission policy requires confirmation".into(),
        })
    );

    assert_eq!(native_calls.load(Ordering::Acquire), 0);
    assert_eq!(mcp_calls.load(Ordering::Acquire), 0);
}

#[test]
fn scoped_grants_and_session_bypass_dispatch_without_weakening_restrictions() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::edit",
            ToolAccess::Write,
            CountingTool {
                calls: Arc::clone(&calls),
                result: Ok(ToolOutput::failure("tool rejected input")),
            },
        )
        .unwrap();

    let policy = PermissionPolicy::new(PermissionMode::Edit, Vec::new());
    let grant = ProjectPermissionGrant::allow(
        "project-a",
        PermissionPattern::Exact("native::edit".into()),
        PermissionPattern::Exact("src/lib.rs".into()),
    );

    assert_eq!(
        dispatcher
            .dispatch(
                &policy,
                std::slice::from_ref(&grant),
                &PermissionSession::new(),
                request("project-a", "native::edit", "src/lib.rs", ToolAccess::Write),
            )
            .unwrap(),
        ToolDispatchOutcome::Executed(ToolOutput::failure("tool rejected input"))
    );
    assert_eq!(
        dispatcher
            .dispatch(
                &policy,
                &[grant],
                &PermissionSession::new(),
                request("project-b", "native::edit", "src/lib.rs", ToolAccess::Write),
            )
            .unwrap(),
        ToolDispatchOutcome::PromptRequired(agens_tools::PermissionPromptContext {
            project_id: "project-b".into(),
            qualified_tool_name: "native::edit".into(),
            target_identifier: "src/lib.rs".into(),
            access: ToolAccess::Write,
            reason: "permission policy requires confirmation".into(),
        })
    );
    assert_eq!(
        dispatcher
            .dispatch(
                &policy,
                &[],
                &PermissionSession::with_temporary_bypass(),
                request("project-b", "native::edit", "src/lib.rs", ToolAccess::Write),
            )
            .unwrap(),
        ToolDispatchOutcome::Executed(ToolOutput::failure("tool rejected input"))
    );

    let restricted_policy = PermissionPolicy::new(PermissionMode::Chat, Vec::new());
    let restricted_grant = ProjectPermissionGrant::allow(
        "project-a",
        PermissionPattern::Exact("native::edit".into()),
        PermissionPattern::Exact("src/lib.rs".into()),
    );
    assert_eq!(
        dispatcher
            .dispatch(
                &restricted_policy,
                &[restricted_grant],
                &PermissionSession::with_temporary_bypass(),
                request(
                    "project-a",
                    "native::edit",
                    "src/lib.rs",
                    ToolAccess::ReadOnly,
                ),
            )
            .unwrap(),
        ToolDispatchOutcome::Denied
    );

    assert_eq!(calls.load(Ordering::Acquire), 2);
}

#[test]
fn infrastructure_failures_remain_distinct_from_model_visible_tool_failures() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_mcp(
            &remote_metadata("server::status", RemoteToolAccess::ReadOnly),
            CountingTool {
                calls: Arc::clone(&calls),
                result: Err(Error::Extension("transport unavailable".into())),
            },
        )
        .unwrap();

    let policy = PermissionPolicy::new(PermissionMode::Edit, Vec::new());
    let grant = ProjectPermissionGrant::allow(
        "project-a",
        PermissionPattern::Exact("server::status".into()),
        PermissionPattern::Any,
    );

    assert_eq!(
        dispatcher.dispatch(
            &policy,
            &[grant],
            &PermissionSession::new(),
            request(
                "project-a",
                "server::status",
                "remote",
                ToolAccess::ReadOnly,
            ),
        ),
        Err(Error::Extension("transport unavailable".into()))
    );
    assert_eq!(calls.load(Ordering::Acquire), 1);
}

#[test]
fn missing_project_identity_cannot_consume_grants_and_prompt_context_excludes_arguments() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::edit",
            ToolAccess::Write,
            CountingTool {
                calls: Arc::clone(&calls),
                result: Ok(ToolOutput::success("edited")),
            },
        )
        .unwrap();

    let grant = ProjectPermissionGrant::allow(
        "",
        PermissionPattern::Exact("native::edit".into()),
        PermissionPattern::Exact("src/lib.rs".into()),
    );
    let request = ToolDispatchRequest::new(
        PermissionRequest::new("", "native::edit", "src/lib.rs", ToolAccess::Write),
        json!({"credential": "must-not-appear"}),
    );

    assert_eq!(
        dispatcher.dispatch(
            &PermissionPolicy::new(PermissionMode::Edit, Vec::new()),
            &[grant],
            &PermissionSession::new(),
            request,
        ),
        Ok(ToolDispatchOutcome::PromptRequired(
            agens_tools::PermissionPromptContext {
                project_id: "".into(),
                qualified_tool_name: "native::edit".into(),
                target_identifier: "src/lib.rs".into(),
                access: ToolAccess::Write,
                reason: "permission policy requires confirmation".into(),
            }
        ))
    );
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn mcp_registration_uses_conservative_remote_metadata_access() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_mcp(
            &remote_metadata("server::unspecified", RemoteToolAccess::Write),
            CountingTool {
                calls: Arc::clone(&calls),
                result: Ok(ToolOutput::success("ran")),
            },
        )
        .unwrap();

    assert_eq!(
        dispatcher
            .dispatch(
                &PermissionPolicy::new(PermissionMode::Chat, Vec::new()),
                &[],
                &PermissionSession::new(),
                request(
                    "project-a",
                    "server::unspecified",
                    "remote",
                    ToolAccess::ReadOnly,
                ),
            )
            .unwrap(),
        ToolDispatchOutcome::Denied
    );
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

fn remote_metadata(qualified_name: &str, access: RemoteToolAccess) -> RemoteToolMetadata {
    let (server_name, tool_name) = qualified_name.split_once("::").unwrap();
    RemoteToolMetadata {
        qualified_name: qualified_name.into(),
        server_name: server_name.into(),
        tool_name: tool_name.into(),
        description: None,
        input_schema: json!({"type": "object"}),
        access,
    }
}
