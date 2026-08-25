//! Launching a subagent task through the same permission path a model-issued
//! tool call takes.
//!
//! A task the user selected is not privileged: it is turned into a
//! `native::task` call and put through the gate and the resolver like any
//! other, so one authorization path covers both.

use agens_core::{
    HeadlessPermissionGate, HeadlessPermissionResolver, HeadlessToolCall, HeadlessToolDispatcher,
    HeadlessToolOutput, HeadlessTurnCancellation, HeadlessTurnPortError, PermissionDecision,
    SessionMessage, SubmitOrigin,
};
use agens_permissions::{
    PermissionPrompter, ProductionPermissionGate, ProductionPermissionResolver,
};

use crate::dispatcher::ProductionToolDispatcher;

pub struct TaskLaunchRequest<'a> {
    pub agent: &'a str,
    pub description: &'a str,
    pub background: bool,
    pub user_message: SessionMessage,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TuiSelectedTaskLaunch {
    NotSelected,
    Dispatched,
    Rejected(TaskLaunchOutcome),
}

#[derive(Debug, PartialEq, Eq)]
pub enum TaskLaunchOutcome {
    Dispatched(HeadlessToolOutput),
    RejectedEmptyInput,
    RejectedCancelled,
    Denied,
}

pub struct AuthorizedNativeTaskRuntime<P> {
    pub gate: ProductionPermissionGate,
    pub resolver: ProductionPermissionResolver<P>,
    pub dispatcher: ProductionToolDispatcher,
    pub next_call_id: u64,
}

impl<P: PermissionPrompter> AuthorizedNativeTaskRuntime<P> {
    pub fn launch(
        &mut self,
        request: TaskLaunchRequest<'_>,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<TaskLaunchOutcome, HeadlessTurnPortError> {
        if request.agent.trim().is_empty() {
            return Ok(TaskLaunchOutcome::RejectedEmptyInput);
        }
        if cancellation.is_cancelled() {
            return Ok(TaskLaunchOutcome::RejectedCancelled);
        }
        if cancellation.is_expired() {
            return Err(HeadlessTurnPortError::TimedOut);
        }

        self.next_call_id += 1;
        let call = HeadlessToolCall {
            id: format!("tui-task-{}", self.next_call_id),
            name: "native::task".into(),
            input: serde_json::json!({
                "agent": request.agent,
                "description": if request.description.is_empty() {
                    "[selected media-only submission]"
                } else {
                    request.description
                },
                "background": request.background,
            })
            .to_string(),
        };
        let decision = poll_permission_port(self.gate.evaluate(&call, cancellation))?;
        let decision = if decision == PermissionDecision::Ask {
            poll_permission_port(self.resolver.resolve(&call, cancellation))?
        } else {
            decision
        };

        if decision == PermissionDecision::Deny {
            return Ok(TaskLaunchOutcome::Denied);
        }

        self.dispatcher
            .bind_trusted_task(&call, request.user_message)?;
        poll_permission_port(self.dispatcher.dispatch(call, cancellation))
            .map(TaskLaunchOutcome::Dispatched)
    }
}

/// A subagent armed for the user's next prompt must survive a turn the user never submitted, so a
/// runtime-scheduled turn leaves the arming in place and runs the main agent instead.
pub fn origin_launches_selected_subagent(origin: SubmitOrigin) -> bool {
    match origin {
        SubmitOrigin::User | SubmitOrigin::Background => true,
        SubmitOrigin::SubagentCompletion => false,
    }
}

pub fn poll_permission_port<T>(
    future: impl std::future::Future<Output = Result<T, HeadlessTurnPortError>>,
) -> Result<T, HeadlessTurnPortError> {
    let mut future = std::pin::pin!(future);
    let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

    match future.as_mut().poll(context) {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => Err(HeadlessTurnPortError::Permission),
    }
}
