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

    dispatcher
        .replace_native(
            "native::read",
            ToolAccess::ReadOnly,
            CountingTool(Arc::clone(&replacement_calls)),
        )
        .unwrap();
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

/// A tool that finishes normally, and can make the turn's deadline expire or
/// its cancellation fire from inside its own run — the shape of a call that
/// outlives the budget it was given, without spending real time to prove it.
struct SlowTool {
    cancel_midway: Option<Arc<AtomicBool>>,
}

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
        if let Some(cancellation) = self.cancel_midway.as_ref() {
            cancellation.store(true, Ordering::Release);
        } else {
            // Just past the deadline the caller sets below: the point is to
            // return AFTER it, which is what the pre-check cannot catch.
            std::thread::sleep(Duration::from_millis(4));
        }
        Ok(ToolOutput::success("the work is done"))
    }
}

fn slow_dispatch(tool: SlowTool) -> (ToolDispatcher, agens_tools::AuthorizedToolCall) {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::read", ToolAccess::ReadOnly, tool)
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
    let (mut dispatcher, handle) = slow_dispatch(SlowTool {
        cancel_midway: None,
    });

    // The budget is gone by the time the tool returns, which is what a call
    // slower than its deadline leaves behind.
    let output = dispatcher
        .execute(
            handle,
            &ToolExecutionContext::with_timeout(Duration::from_millis(1)),
        )
        .unwrap();

    assert_eq!(output.content, "the work is done");
    assert!(!output.is_error);
}

/// Cancellation is a decision, not a deadline: its result must not be acted on.
#[test]
fn a_cancelled_call_still_reports_the_cancellation_instead_of_its_result() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let (mut dispatcher, handle) = slow_dispatch(SlowTool {
        cancel_midway: Some(Arc::clone(&cancellation)),
    });
    let context = ToolExecutionContext::new(cancellation, Duration::from_secs(30));

    let output = dispatcher.execute(handle, &context).unwrap();

    assert!(output.is_error, "{output:?}");
    assert_eq!(output.content, "tool execution cancelled");
}
