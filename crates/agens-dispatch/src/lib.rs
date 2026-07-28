//! The tool dispatch table: what a tool call is bound to, how it reaches the
//! shared dispatcher, and how a subagent task is launched once permission has
//! been granted.
//!
//! This is the layer between a catalog of tools and a turn that wants to call
//! one. It knows nothing about who asked: the same table serves a headless run,
//! a terminal session and a daemon-driven worker.

mod dispatcher;
mod registered;
mod task_launch;

pub use dispatcher::{ProductionToolDispatcher, sanitized_native_tool_failure};
pub use registered::{RegisteredMcpTool, RegisteredNativeTool};
pub use task_launch::{
    AuthorizedNativeTaskRuntime, TaskLaunchOutcome, TaskLaunchRequest, TuiSelectedTaskLaunch,
    origin_launches_selected_subagent, poll_permission_port,
};
