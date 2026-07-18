use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use agens_core::{
    Error, PermissionMode, PermissionPolicy, PermissionRequest, PermissionSession, ToolAccess,
};
use agens_tools::{
    DispatchTool, ToolDispatchRequest, ToolDispatcher, ToolEvaluationOutcome, ToolExecutionContext,
    ToolOutput,
};
use serde_json::json;

struct CountingTool(Arc<AtomicUsize>);

impl DispatchTool for CountingTool {
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

fn request() -> ToolDispatchRequest {
    ToolDispatchRequest::new(
        PermissionRequest::new(
            "project",
            "native::read",
            "src/lib.rs",
            ToolAccess::ReadOnly,
        ),
        json!({"credential": "SECRET_SENTINEL"}),
    )
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
