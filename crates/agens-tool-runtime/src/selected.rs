//! Launching the subagent a caller armed for the next prompt.
//!
//! Arming is a two-step: a caller selects a subagent, then the next submission
//! runs it instead of the primary agent. That second step needs the task runtime
//! and the lifecycle bridge, which is why it lives here rather than with the
//! dispatch table.

use std::sync::{Arc, Mutex};

use agens_core::{
    HeadlessTurnCancellation, HeadlessTurnError, HeadlessTurnPortError, SessionMessage,
};
use agens_dispatch::{TaskLaunchOutcome, TaskLaunchRequest, TuiSelectedTaskLaunch};
use agens_error::{CliError, ExitStatus};
use agens_session::context::SessionContext;
use agens_tools::TaskLaunchMode;

use crate::runner::TuiTaskLifecycleBridge;
use crate::task::ProductionTuiTaskRuntime;

pub fn launch_selected_task(
    runtime: &mut ProductionTuiTaskRuntime,
    session: &Arc<Mutex<SessionContext>>,
    description: &str,
    background: bool,
    cancellation: &HeadlessTurnCancellation,
) -> Result<TuiSelectedTaskLaunch, CliError> {
    if description.trim().is_empty() {
        return Ok(TuiSelectedTaskLaunch::Rejected(
            TaskLaunchOutcome::RejectedEmptyInput,
        ));
    }
    let user_message = SessionMessage::try_from(agens_core::Message {
        role: agens_core::Role::User,
        parts: vec![agens_core::MessagePart::Text(description.to_owned())],
    })
    .map_err(|_| CliError::usage("subagent task is empty"))?;
    launch_selected_task_message(
        runtime,
        session,
        description,
        user_message,
        background,
        cancellation,
    )
}

fn launch_selected_task_message(
    runtime: &mut ProductionTuiTaskRuntime,
    session: &Arc<Mutex<SessionContext>>,
    description: &str,
    user_message: SessionMessage,
    background: bool,
    cancellation: &HeadlessTurnCancellation,
) -> Result<TuiSelectedTaskLaunch, CliError> {
    let agent = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?
        .selected_subagent
        .take();
    let Some(agent) = agent else {
        return Ok(TuiSelectedTaskLaunch::NotSelected);
    };

    match runtime.authorized.launch(
        TaskLaunchRequest {
            agent: &agent,
            description,
            background,
            user_message,
        },
        cancellation,
    ) {
        Ok(TaskLaunchOutcome::Dispatched(output)) if !output.is_error => {
            Ok(TuiSelectedTaskLaunch::Dispatched)
        }
        Ok(TaskLaunchOutcome::Dispatched(_)) if cancellation.is_cancelled() => {
            Err(CliError::runtime(HeadlessTurnError::Cancelled))
        }
        Ok(TaskLaunchOutcome::Dispatched(_)) if cancellation.is_expired() => {
            Err(CliError::runtime(HeadlessTurnError::TimedOut))
        }
        Ok(outcome) => Ok(TuiSelectedTaskLaunch::Rejected(outcome)),
        Err(HeadlessTurnPortError::Cancelled) => {
            Err(CliError::runtime(HeadlessTurnError::Cancelled))
        }
        Err(HeadlessTurnPortError::TimedOut) => Err(CliError::runtime(HeadlessTurnError::TimedOut)),
        Err(_) => Err(CliError::runtime(HeadlessTurnError::Tool)),
    }
}

pub fn selected_task_skips_parent(
    launch: Result<TuiSelectedTaskLaunch, CliError>,
    lifecycle: &TuiTaskLifecycleBridge,
) -> Result<bool, CliError> {
    match launch? {
        TuiSelectedTaskLaunch::NotSelected => Ok(false),
        TuiSelectedTaskLaunch::Dispatched => {
            Ok(lifecycle.mode() == Some(TaskLaunchMode::Background))
        }
        TuiSelectedTaskLaunch::Rejected(outcome) => Err(selected_task_launch_error(outcome)),
    }
}

fn selected_task_launch_error(outcome: TaskLaunchOutcome) -> CliError {
    match outcome {
        TaskLaunchOutcome::RejectedEmptyInput => CliError::usage("subagent task is empty"),
        TaskLaunchOutcome::RejectedCancelled => CliError::runtime(HeadlessTurnError::Cancelled),
        TaskLaunchOutcome::Denied => CliError::runtime(HeadlessTurnError::Permission),
        TaskLaunchOutcome::Dispatched(_) => CliError::runtime(HeadlessTurnError::Tool),
    }
}
