//! Running a turn with no interface attached.
//!
//! One turn, driven to completion under the session attempt lifecycle: build the
//! provider for the configured backend, resolve the policy and prompt it runs
//! under, dispatch whatever tools it calls, and record the outcome.
//!
//! Nothing here prompts a person. Permission questions go out through a
//! `PermissionPrompter` the caller supplies, so the same turn runs behind a
//! terminal, a `--print` invocation or a daemon worker.

mod outcome;
mod request;
mod subagents;
mod turn;

pub use outcome::{HeadlessChatCompletion, HeadlessChatFailure};
pub use request::{
    HeadlessChatRequest, MediaPreflightError, apply_session_to_request,
    explicit_task_delegation_prompt, preflight_request_media, provider_messages,
    seed_configured_reasoning_effort,
};
pub use subagents::{
    RequestedSubagent, interrupted_turn_note, record_requested_subagent, record_tool_result_fact,
};
pub use turn::{
    PermissionPrompterFactory, RunExecution, headless_turn_own_system_prompt,
    headless_turn_permission_policy, headless_turn_project_root, headless_turn_provider_base_url,
    headless_turn_system_prompt, run_production_headless_chat_executing_run,
    run_production_headless_chat_with_progress,
    run_production_headless_chat_with_progress_and_ask_user,
    run_production_headless_chat_with_progress_and_steering,
};
