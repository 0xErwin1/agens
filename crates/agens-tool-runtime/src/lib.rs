//! Assembling and running the tools a turn can call.
//!
//! Building the native tool catalog and the MCP registry for a project root,
//! registering the subagent `task` tool, running a delegated task in a confined
//! child process, and launching the subagent a caller armed for the next prompt.
//!
//! Nothing here knows who asked. A headless run, a terminal session and a
//! daemon worker assemble the same runtime.

pub mod child;
pub mod child_catalog;
pub mod external_permission;
pub mod mcp;
pub mod rotation;
pub mod runner;
pub mod runtime;
pub mod task;

mod blocking;
mod selected;

pub use blocking::block_on_headless_turn;
pub use selected::{launch_selected_task, selected_task_skips_parent};
