use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use agens_core::{Error, PermissionMode, PermissionPolicy, PermissionSession, ToolAccess};
use agens_tools::{
    DispatchTool, ToolDispatchRequest, ToolDispatcher, ToolEvaluationOutcome, ToolExecutionContext,
    ToolOutput,
};
use serde_json::json;

struct CountingTool(Arc<AtomicUsize>);

struct UnicodeTool;

impl DispatchTool for CountingTool {
    fn permission_target(&self, arguments: &serde_json::Value) -> Result<String, Error> {
        arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::Tool("path is required".into()))
    }

    fn execute(
        &mut self,
        context: &ToolExecutionContext,
        _: serde_json::Value,
    ) -> Result<ToolOutput, Error> {
        context
            .check()
            .map_err(|error| Error::Tool(error.to_string()))?;
        self.0.fetch_add(1, Ordering::AcqRel);
        Ok(ToolOutput::success("executed"))
    }
}

impl DispatchTool for UnicodeTool {
    fn permission_target(&self, arguments: &serde_json::Value) -> Result<String, Error> {
        arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::Tool("path is required".into()))
    }

    fn execute(
        &mut self,
        _: &ToolExecutionContext,
        _: serde_json::Value,
    ) -> Result<ToolOutput, Error> {
        Ok(ToolOutput::success("界".repeat(10_000)))
    }
}

fn request() -> ToolDispatchRequest {
    ToolDispatchRequest::new("project", "native::read", json!({"path": "src/lib.rs"}))
}

#[test]
fn authorization_uses_the_registered_tool_projection_not_a_caller_target() {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::read",
            ToolAccess::ReadOnly,
            CountingTool(Arc::new(AtomicUsize::new(0))),
        )
        .unwrap();
    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![agens_core::PermissionRule::global(
            agens_core::PermissionDecision::Deny,
            agens_core::PermissionPattern::Exact("native::read".into()),
            agens_core::PermissionPattern::Exact("private.txt".into()),
        )],
    );

    assert!(matches!(
        dispatcher.evaluate(
            &policy,
            &[],
            &PermissionSession::with_temporary_bypass(),
            ToolDispatchRequest::new("project", "native::read", json!({"path": "private.txt"})),
        ),
        Ok(ToolEvaluationOutcome::Denied)
    ));
}

#[test]
fn default_dispatchers_reject_each_others_authorizations() {
    let mut first = ToolDispatcher::default();
    let mut second = ToolDispatcher::default();
    first
        .register_native(
            "native::read",
            ToolAccess::ReadOnly,
            CountingTool(Arc::new(AtomicUsize::new(0))),
        )
        .unwrap();
    second
        .register_native(
            "native::read",
            ToolAccess::ReadOnly,
            CountingTool(Arc::new(AtomicUsize::new(0))),
        )
        .unwrap();
    let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
    let ToolEvaluationOutcome::Authorized(handle) = first
        .evaluate(
            &policy,
            &[],
            &PermissionSession::with_temporary_bypass(),
            request(),
        )
        .unwrap()
    else {
        panic!("read-only tool should be authorized");
    };

    assert!(
        second
            .execute(
                handle,
                &ToolExecutionContext::with_timeout(Duration::from_secs(1))
            )
            .is_err()
    );
}

#[test]
fn dispatcher_preserves_large_unicode_tool_output() {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::read", ToolAccess::ReadOnly, UnicodeTool)
        .unwrap();
    let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
    let ToolEvaluationOutcome::Authorized(handle) = dispatcher
        .evaluate(
            &policy,
            &[],
            &PermissionSession::with_temporary_bypass(),
            request(),
        )
        .unwrap()
    else {
        panic!("read-only tool should be authorized");
    };

    let output = dispatcher
        .execute(
            handle,
            &ToolExecutionContext::with_timeout(Duration::from_secs(1)),
        )
        .unwrap();
    assert!(output.content.is_char_boundary(output.content.len()));
    assert_eq!(output.content, "界".repeat(10_000));
}

#[test]
fn authorization_is_separate_from_execution_and_handles_are_single_use() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::read",
            ToolAccess::ReadOnly,
            CountingTool(Arc::clone(&calls)),
        )
        .unwrap();
    let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);

    let ToolEvaluationOutcome::Authorized(handle) = dispatcher
        .evaluate(
            &policy,
            &[],
            &PermissionSession::with_temporary_bypass(),
            request(),
        )
        .unwrap()
    else {
        panic!("read-only tool should be authorized");
    };
    assert_eq!(calls.load(Ordering::Acquire), 0);

    assert_eq!(
        dispatcher.execute(
            handle,
            &ToolExecutionContext::with_timeout(Duration::from_secs(1))
        ),
        Ok(ToolOutput::success("executed"))
    );
    assert_eq!(calls.load(Ordering::Acquire), 1);
}

#[test]
fn cancelled_context_suppresses_late_execution() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::read",
            ToolAccess::ReadOnly,
            CountingTool(Arc::clone(&calls)),
        )
        .unwrap();
    let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
    let ToolEvaluationOutcome::Authorized(handle) = dispatcher
        .evaluate(
            &policy,
            &[],
            &PermissionSession::with_temporary_bypass(),
            request(),
        )
        .unwrap()
    else {
        panic!("read-only tool should be authorized");
    };
    let cancelled = Arc::new(AtomicBool::new(true));
    let context = ToolExecutionContext::new(cancelled, Duration::from_secs(1));

    let output = dispatcher.execute(handle, &context).unwrap();
    assert_eq!(output, ToolOutput::failure("tool execution cancelled"));
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn registry_replacement_invalidates_an_already_authorized_call() {
    let first_calls = Arc::new(AtomicUsize::new(0));
    let replacement_calls = Arc::new(AtomicUsize::new(0));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::read",
            ToolAccess::ReadOnly,
            CountingTool(Arc::clone(&first_calls)),
        )
        .unwrap();
    let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
    let ToolEvaluationOutcome::Authorized(handle) = dispatcher
        .evaluate(
            &policy,
            &[],
            &PermissionSession::with_temporary_bypass(),
            request(),
        )
        .unwrap()
    else {
        panic!("read-only tool should be authorized");
    };

    dispatcher.replace_native(
        "native::read",
        ToolAccess::ReadOnly,
        CountingTool(Arc::clone(&replacement_calls)),
    );
    assert!(
        dispatcher
            .execute(
                handle,
                &ToolExecutionContext::with_timeout(Duration::from_secs(1))
            )
            .is_err()
    );
    assert_eq!(first_calls.load(Ordering::Acquire), 0);
    assert_eq!(replacement_calls.load(Ordering::Acquire), 0);
}

/// A tool that finishes only after the turn's deadline has already passed —
/// the shape of a long subagent.
struct SlowTool;

impl DispatchTool for SlowTool {
    fn permission_target(&self, arguments: &serde_json::Value) -> Result<String, Error> {
        arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::Tool("path is required".into()))
    }

    fn execute(
        &mut self,
        _: &ToolExecutionContext,
        _: serde_json::Value,
    ) -> Result<ToolOutput, Error> {
        std::thread::sleep(Duration::from_millis(30));
        Ok(ToolOutput::success("the work is done"))
    }
}

fn slow_dispatch() -> (ToolDispatcher, agens_tools::AuthorizedToolCall) {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::read", ToolAccess::ReadOnly, SlowTool)
        .unwrap();
    let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
    let ToolEvaluationOutcome::Authorized(handle) = dispatcher
        .evaluate(
            &policy,
            &[],
            &PermissionSession::with_temporary_bypass(),
            request(),
        )
        .unwrap()
    else {
        panic!("read-only tool should be authorized");
    };
    (dispatcher, handle)
}

/// Reporting a timeout over a call that already produced its answer threw the
/// work away and told the model to try again — which is how one long subagent
/// became two.
#[test]
fn a_deadline_that_expires_while_a_tool_runs_does_not_discard_its_result() {
    let (mut dispatcher, handle) = slow_dispatch();

    let output = dispatcher
        .execute(
            handle,
            &ToolExecutionContext::with_timeout(Duration::from_millis(5)),
        )
        .unwrap();

    assert_eq!(output.content, "the work is done");
    assert!(!output.is_error);
}

/// Cancellation is a decision, not a deadline: its result must not be acted on.
#[test]
fn a_cancelled_call_still_reports_the_cancellation_instead_of_its_result() {
    let (mut dispatcher, handle) = slow_dispatch();
    let cancellation = Arc::new(AtomicBool::new(false));
    let context = ToolExecutionContext::new(Arc::clone(&cancellation), Duration::from_secs(30));
    let flag = Arc::clone(&cancellation);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        flag.store(true, Ordering::Release);
    });

    let output = dispatcher.execute(handle, &context).unwrap();

    assert!(output.is_error, "{output:?}");
    assert_eq!(output.content, "tool execution cancelled");
}
