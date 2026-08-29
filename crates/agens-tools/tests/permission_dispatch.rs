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
fn an_mcp_registration_that_would_reassign_a_native_alias_is_refused() {
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

    let error = dispatcher
        .register_mcp(
            &metadata,
            CountingTool(Arc::clone(&mcp_calls), Ok(ToolOutput::success("mcp"))),
        )
        .expect_err("a registration that would reassign the model alias must be refused");
    assert!(error.to_string().contains("files_read"), "{error}");

    assert_eq!(dispatcher.canonical_identity("files::read"), None);
    let native_identity = dispatcher
        .canonical_identity("native::files_read")
        .expect("the native tool must survive the refused registration")
        .to_owned();
    assert_eq!(
        dispatcher.canonical_identity("files_read"),
        Some(&native_identity)
    );

    let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
    let ToolEvaluationOutcome::Authorized(handle) = dispatcher
        .evaluate(
            &policy,
            &[],
            &PermissionSession::with_temporary_bypass(),
            request("project", "files_read", "target"),
        )
        .expect("the native owner should still answer to its model alias")
    else {
        panic!("read-only native tool should be authorized");
    };

    assert_eq!(
        dispatcher
            .execute(
                handle,
                &ToolExecutionContext::with_timeout(Duration::from_secs(1)),
            )
            .unwrap(),
        ToolOutput::success("native")
    );
    assert_eq!(native_calls.load(Ordering::Acquire), 1);
    assert_eq!(mcp_calls.load(Ordering::Acquire), 0);
}

#[test]
fn ambiguous_flattened_model_aliases_refuse_the_second_registration() {
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_mcp(
            &RemoteToolMetadata {
                qualified_name: "a_b::c".into(),
                server_name: "a_b".into(),
                tool_name: "c".into(),
                description: None,
                input_schema: json!({}),
                access: RemoteToolAccess::ReadOnly,
            },
            CountingTool(Arc::clone(&first_calls), Ok(ToolOutput::success("first"))),
        )
        .unwrap();
    let first_identity = dispatcher
        .canonical_identity("a_b::c")
        .expect("the first registration must resolve")
        .to_owned();

    let error = dispatcher
        .register_mcp(
            &RemoteToolMetadata {
                qualified_name: "a::b_c".into(),
                server_name: "a".into(),
                tool_name: "b_c".into(),
                description: None,
                input_schema: json!({}),
                access: RemoteToolAccess::ReadOnly,
            },
            CountingTool(Arc::clone(&second_calls), Ok(ToolOutput::success("second"))),
        )
        .expect_err("two pairs flattening onto one model alias must not both register");
    let message = error.to_string();
    assert!(message.contains(first_identity.as_str()), "{message}");
    assert!(message.contains("b_c"), "{message}");

    assert_eq!(dispatcher.canonical_identity("a::b_c"), None);
    assert_eq!(
        dispatcher.canonical_identity("a_b_c"),
        Some(&first_identity)
    );

    let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
    let ToolEvaluationOutcome::Authorized(handle) = dispatcher
        .evaluate(
            &policy,
            &[],
            &PermissionSession::with_temporary_bypass(),
            request("project", "a_b_c", "target"),
        )
        .expect("the first owner should still answer to the flattened alias")
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
        ToolOutput::success("first")
    );
    assert_eq!(first_calls.load(Ordering::Acquire), 1);
    assert_eq!(second_calls.load(Ordering::Acquire), 0);
}

#[test]
fn a_server_that_claims_the_native_namespace_is_refused() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();

    let error = dispatcher
        .register_mcp(
            &RemoteToolMetadata {
                qualified_name: "native::read".into(),
                server_name: "native".into(),
                tool_name: "read".into(),
                description: None,
                input_schema: json!({}),
                access: RemoteToolAccess::ReadOnly,
            },
            CountingTool(Arc::clone(&calls), Ok(ToolOutput::success("mcp"))),
        )
        .expect_err("a server named native must not claim the native namespace");
    assert!(error.to_string().contains("native"), "{error}");

    assert_eq!(dispatcher.canonical_identity("native::read"), None);
    assert_eq!(dispatcher.canonical_identity("native_read"), None);
}

#[test]
fn a_refused_registration_leaves_the_native_owner_and_its_handles_intact() {
    for (native_name, authorized_alias) in [
        ("native::files_read", "files_read"),
        ("native::files::read", "files::read"),
    ] {
        let native_calls = Arc::new(AtomicUsize::new(0));
        let mcp_calls = Arc::new(AtomicUsize::new(0));
        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register_native(
                native_name,
                ToolAccess::ReadOnly,
                CountingTool(Arc::clone(&native_calls), Ok(ToolOutput::success("native"))),
            )
            .unwrap();
        let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
        let ToolEvaluationOutcome::Authorized(handle) = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::with_temporary_bypass(),
                request("project", authorized_alias, "target"),
            )
            .unwrap()
        else {
            panic!("native tool should authorize before the collision attempt");
        };

        dispatcher
            .register_mcp(
                &RemoteToolMetadata {
                    qualified_name: "files::read".into(),
                    server_name: "files".into(),
                    tool_name: "read".into(),
                    description: None,
                    input_schema: json!({}),
                    access: RemoteToolAccess::ReadOnly,
                },
                CountingTool(Arc::clone(&mcp_calls), Ok(ToolOutput::success("mcp"))),
            )
            .expect_err("a registration colliding with a native alias must be refused");

        assert_eq!(
            dispatcher
                .execute(
                    handle,
                    &ToolExecutionContext::with_timeout(Duration::from_secs(1)),
                )
                .unwrap(),
            ToolOutput::success("native")
        );
        assert_eq!(native_calls.load(Ordering::Acquire), 1);
        assert_eq!(mcp_calls.load(Ordering::Acquire), 0);
    }
}
