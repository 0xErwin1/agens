#[cfg(test)]
use std::collections::BTreeMap;
use std::ffi::OsString;
#[cfg(test)]
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Arc, Mutex};

use clap::Parser as _;

#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use agens_config::McpTransport;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use agens_config::ToolLimitSettings;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_config::{parse_toml_document, validate_toml_document};
use agens_core::HeadlessTurnCancellation;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_core::PermissionMode;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use agens_core::Role;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use agens_core::SessionMetadata;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_core::TurnEvent;
#[cfg(test)]
use agens_core::{
    CompletedSessionTurn, HeadlessPermissionGate, HeadlessToolCall, HeadlessToolDispatcher,
    HeadlessTurnPortError, PermissionDecision, PermissionPattern, PermissionPolicy,
    PermissionSession, SessionMessage,
};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_core::{HeadlessTurnError, Message, MessagePart, RetryBoundary, SessionAttemptStatus};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use agens_providers::chatgpt_login::upsert_provider_entry;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_store::{ModelPreference, PreferenceStore};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_store::{SessionStore, StoredSession};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use agens_tools::McpRegistry;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use agens_tools::ToolDispatcher;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_tools::{CommandCatalog, SkillCatalog};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_tools::{
    McpServerDescriptor, McpServerSource, McpServerTransport, TaskLaunchMode, ToolDispatchRequest,
    ToolEvaluationOutcome, ToolExecutionContext,
};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_tools::{TaskExecutionRegistry, TaskMessageSource, TaskMessageTarget};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_tui::BridgeTx;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_tui::Tui;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use agens_tui::TuiPermissionBridge;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use agens_tui::TuiPresentation;
use agens_tui::TuiSubagentErrorKind;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_tui::{TuiProviderOutcome, TuiRouteProgress, TuiRouteRequest};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_tui::{TuiRouteCancellation, TuiRuntimeEvent, TuiSubmissionOutcome};

mod bootstrap;
mod chatgpt_auth;
mod cli;
mod commands;
mod deps;
mod diagnostics;
mod dispatch;
mod error;
mod headless;
mod mcp;
mod model_registry;
mod permissions;
mod session;
#[cfg(test)]
mod test_support;
mod tools;
mod tui;
mod turns;

#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use bootstrap::ProviderSource;
use bootstrap::effective_max_iterations;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use chatgpt_auth::{ChatGptAuthCoordinator, ChatGptAuthFlow, ChatGptAuthProgress};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use commands::chat::{chat_args_with_prompt, chat_request};
use diagnostics::{
    next_diagnostic_reference, operation_diagnostics, record_parent_terminal,
    record_subagent_terminal,
};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use dispatch::{ProductionToolDispatcher, poll_permission_port, sanitized_native_tool_failure};
use error::cancellation_result;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use headless::HeadlessChatCompletion;
use headless::block_on_headless_turn;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use headless::{provider_messages, run_production_headless_chat_with_progress};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use permissions::{PermissionPromptAnswer, ProductionPermissionGate, permission_policy};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use session::attempt::active_session_attempts;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use session::attempt::attempt_failure_status;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use test_support::{
    BatchTool, ProductionBatchInput, bootstrap_from_configuration, native_batch_call,
    render_tui_test_backend, reset_tui_resume_test_counters, rotation_agent, rotation_dispatcher,
    run_production_batch, run_production_batch_with_policy, tui_resume_test_counters,
    tui_session_bootstrap, tui_session_bootstrap_for_provider, tui_session_directory,
};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tools::runner::{TuiTaskControls, TuiTaskLifecycleBridge};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tools::runtime::{production_dangerous_child_tool_runtime, production_tool_runtime};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tools::task::production_tui_task_runtime;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::agents::{
    BundledModelValidator, initial_active_agent_name, list_tui_agents, rotate_tui_agent,
};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use tui::engine::ProductionTuiEngine;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::engine::{
    configure_tui_project_identity, report_tui_extension_collisions, run_tui_prompt,
    run_tui_prompt_with,
};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::extensions::{start_tui_commands, start_tui_skills};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::files::{
    expand_tui_file_reference, tui_file_candidates_with_limit, tui_picker_file_candidates,
};
use tui::models::tui_model_source;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::models::{apply_tui_effort, apply_tui_model, seed_remembered_tui_selection};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use tui::provider::restore_chatgpt_credentials;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::provider::{TuiCredentialResolver, TuiProvider};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::resume::{
    commit_tui_session_resume, ensure_active_tui_agent_runtime, list_tui_sessions,
    load_tui_session_for_resume, prepare_loaded_tui_session_resume, resume_tui_session,
    resumed_subagent_cards,
};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::router::{TUI_ERROR_ACTION, TuiRuntimeRouter, auth_route_outcome, tui_provider_outcome};
use tui::run_tui;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use tui::session::session_dialog_entry;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::session::{ActiveAgentRuntime, TuiSessionContext, resume_retry_notice};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::session::{AgentRotationError, rotate_active_agent};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use tui::turn::complete_tui_turn;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::turn::{effective_tui_model, tui_session_presentation};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use turns::completed_session_turn;

pub use bootstrap::{Bootstrap, bootstrap};
pub use deps::CliDependencies;
pub use error::{CliError, CommandResult, ExitStatus};
pub use headless::HeadlessChatRequest;
pub use model_registry::{TuiModelSelector, TuiModelSource};
pub use tui::files::tui_file_candidates;

pub fn execute<I, S>(arguments: I, dependencies: &CliDependencies) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_owned())
        .collect::<Vec<_>>();

    let cancellation = HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(120));
    execute_strings(arguments, dependencies, &cancellation)
}

pub fn execute_with_cancellation<I, S>(
    arguments: I,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_owned())
        .collect::<Vec<_>>();

    execute_strings(arguments, dependencies, cancellation)
}

pub fn execute_os<I, S>(arguments: I, dependencies: &CliDependencies) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| {
            argument
                .into()
                .into_string()
                .map_err(|_| CliError::usage("command arguments must be valid UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>();

    match arguments {
        Ok(arguments) => {
            let cancellation =
                HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(120));
            execute_strings(arguments, dependencies, &cancellation)
        }
        Err(error) => error_result(&[], error),
    }
}

pub fn execute_os_with_cancellation<I, S>(
    arguments: I,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| {
            argument
                .into()
                .into_string()
                .map_err(|_| CliError::usage("command arguments must be valid UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>();

    match arguments {
        Ok(arguments) => execute_strings(arguments, dependencies, cancellation),
        Err(error) => error_result(&[], error),
    }
}

fn execute_strings(
    arguments: Vec<String>,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> CommandResult {
    match execute_command(&arguments, dependencies, cancellation) {
        Ok(stdout) => CommandResult {
            status: ExitStatus::Success,
            stdout,
            stderr: String::new(),
        },
        Err(error) => error_result(&arguments, error),
    }
}

pub(crate) fn error_result(arguments: &[String], error: CliError) -> CommandResult {
    CommandResult {
        status: error.status(),
        stdout: if arguments == ["config", "doctor"] && error.status() == ExitStatus::Configuration
        {
            "Agens config doctor\nStatus:  invalid\n".to_owned()
        } else {
            String::new()
        },
        stderr: if error.is_preformatted() {
            error.message.clone()
        } else {
            format!("error: {error}\n")
        },
    }
}

fn execute_command(
    arguments: &[String],
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    if let Some(identifier) = cli::resume_shorthand(arguments) {
        return run_tui(dependencies, Some(identifier));
    }

    let parsed = match cli::Cli::try_parse_from(arguments.iter()) {
        Ok(parsed) => parsed,
        Err(error) => return cli::clap_outcome(error),
    };

    commands::dispatch(parsed, dependencies, cancellation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agens_core::{
        AgentMode, CompletedTurnRepository, CompletedTurnSnapshot, PermissionRule, ToolAccess,
        TurnState,
    };
    use agens_tui::{Action, Event, Key};
    use rusqlite::Connection;

    #[test]
    fn subagent_message_and_cancellation_leave_the_primary_agent_unchanged() {
        let registry = TaskExecutionRegistry::new();
        let id = registry.admit(TaskLaunchMode::Background).unwrap();
        let dispatcher = rotation_dispatcher();
        let primary = rotation_agent("primary", None, false);
        let active = ActiveAgentRuntime::build(
            &primary,
            Some("gpt-5.5"),
            "project",
            &dispatcher,
            &BundledModelValidator,
        )
        .unwrap();
        let session = TuiSessionContext {
            active_agent: Some(active),
            ..TuiSessionContext::fresh()
        };

        registry
            .send_message(
                TaskMessageSource::User,
                TaskMessageTarget::Execution(id),
                "continue".into(),
            )
            .unwrap();
        assert!(registry.cancel(id));

        assert_eq!(
            session
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );
        assert_eq!(
            session
                .active_agent
                .as_ref()
                .and_then(|agent| agent.model.as_deref()),
            Some("gpt-5.5")
        );
    }

    #[test]
    fn idle_agent_rotation_restores_runtime_and_queues_expansion_reminders_atomically() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-agent-rotation-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dispatcher = rotation_dispatcher();
        let primary = rotation_agent("primary", Some("gpt-4.1"), false);
        let reviewer = rotation_agent("reviewer", Some("gpt-4o"), true);
        let mut store = SessionStore::open(&temporary).unwrap();
        let metadata = SessionMetadata {
            id: 0,
            project: "project".into(),
            title: "title".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
        };
        let turn = CompletedSessionTurn::new(vec![
            SessionMessage::try_from(Message {
                role: Role::User,
                parts: vec![MessagePart::Text("first".into())],
            })
            .unwrap(),
        ])
        .unwrap();
        let metadata = store
            .persist_completed_session_turn(&metadata, &turn)
            .unwrap();
        let primary_runtime = ActiveAgentRuntime::build(
            &primary,
            None,
            "project",
            &dispatcher,
            &BundledModelValidator,
        )
        .unwrap();
        let mut context =
            TuiSessionContext::resumed(1, metadata.clone(), Vec::new(), primary_runtime);
        let original = context.clone();
        context.running = true;
        let busy_original = context.clone();

        let busy = rotate_active_agent(
            &mut context,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
        );
        assert_eq!(busy, Err(AgentRotationError::Busy));
        assert_eq!(context, busy_original);
        context.running = false;
        assert_eq!(
            SessionStore::open(&temporary)
                .unwrap()
                .load_session_for_resume(1)
                .unwrap()
                .metadata
                .active_agent,
            "primary"
        );

        let mut conflicting = metadata.clone();
        conflicting.title = "changed elsewhere".into();
        conflicting.updated_at = 2;
        let conflicting = store
            .persist_completed_session_turn(&conflicting, &turn)
            .unwrap();
        let rollback = rotate_active_agent(
            &mut context,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
        );
        assert_eq!(rollback, Err(AgentRotationError::Persistence));
        assert_eq!(context, original);

        context.metadata = Some(conflicting);
        rotate_active_agent(
            &mut context,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
        )
        .unwrap();
        assert_eq!(
            context.pending_system_reminder.as_deref(),
            Some("Agent capabilities expanded: primary -> reviewer.")
        );

        let request = context.apply_to(HeadlessChatRequest {
            prompt: "next".into(),
            history: Vec::new(),
            model: None,
            system_prompt: None,
            max_iterations: None,
            mode: PermissionMode::Edit,
            dangerously_allow_all: false,
            dangerous_mode: false,
            request_config: agens_core::RequestConfig::default(),
            session_reasoning_effort: None,
            session: None,
            active_agent: None,
            effective_capabilities: None,
            pending_system_reminder: None,
            skills: None,
        });
        assert_eq!(request.active_agent.as_deref(), Some("reviewer"));
        assert_eq!(request.model.as_deref(), Some("gpt-4o"));
        assert_eq!(request.system_prompt.as_deref(), Some("You are reviewer."));
        assert_eq!(
            request.effective_capabilities,
            context
                .active_agent
                .as_ref()
                .map(|agent| agent.capabilities.clone())
        );
        assert_eq!(
            provider_messages(&request, false),
            vec![
                Message {
                    role: Role::System,
                    parts: vec![MessagePart::Text(
                        "Agent capabilities expanded: primary -> reviewer.".into(),
                    )],
                },
                Message {
                    role: Role::User,
                    parts: vec![MessagePart::Text("next".into())],
                },
            ]
        );

        rotate_active_agent(
            &mut context,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
        )
        .unwrap();
        assert_eq!(
            context.pending_system_reminder.as_deref(),
            Some("Agent capabilities expanded: primary -> reviewer.")
        );

        let policy = permission_policy(
            &[],
            "project",
            PermissionMode::Edit,
            &Arc::new(Mutex::new(rotation_dispatcher())),
            request.effective_capabilities.as_ref(),
        )
        .unwrap();
        assert!(matches!(
            rotation_dispatcher()
                .evaluate(
                    &policy,
                    &[],
                    &PermissionSession::new(),
                    ToolDispatchRequest::new(
                        "project",
                        "native::read",
                        serde_json::json!({"target":"file"})
                    ),
                )
                .unwrap(),
            ToolEvaluationOutcome::Authorized(_)
        ));

        let snapshot = CompletedTurnSnapshot::from_persisted_events(vec![
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("answer".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ])
        .unwrap();
        let turn = completed_session_turn(
            "next",
            &snapshot,
            request.pending_system_reminder.as_deref(),
        )
        .unwrap();
        let persisted = store
            .persist_completed_session_turn(context.metadata.as_ref().unwrap(), &turn)
            .unwrap();
        context.metadata = Some(persisted);
        context.pending_system_reminder = None;
        let reopened = SessionStore::open(&temporary)
            .unwrap()
            .load_session_for_resume(1)
            .unwrap();
        assert_eq!(reopened.metadata.active_agent, "reviewer");
        let reminder = reopened
            .messages
            .iter()
            .find(|message| message.role == Role::System)
            .unwrap();
        assert_eq!(
            reminder.parts,
            vec![MessagePart::Text(
                "Agent capabilities expanded: primary -> reviewer.".into()
            )]
        );
        assert!(context.pending_system_reminder.is_none());

        let mut no_expansion = TuiSessionContext::resumed(
            1,
            reopened.metadata,
            reopened.messages,
            context.active_agent.clone().unwrap(),
        );
        no_expansion.metadata = None;
        rotate_active_agent(
            &mut no_expansion,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            None,
        )
        .unwrap();
        assert!(no_expansion.pending_system_reminder.is_none());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn completed_tui_turn_clears_reminders_only_after_successful_persistence() {
        let metadata = SessionMetadata {
            id: 1,
            project: "project".into(),
            title: "title".into(),
            active_agent: "reviewer".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 2,
            completed_turn_count: 2,
            resumable: true,
        };
        let mut context = TuiSessionContext::fresh();
        context.pending_system_reminder = Some("reminder".into());

        assert_eq!(
            complete_tui_turn(
                &mut context,
                Ok(HeadlessChatCompletion {
                    text: "answer".into(),
                    metadata: metadata.clone(),
                    messages: Vec::new(),
                }),
                true,
            )
            .unwrap(),
            "answer"
        );
        assert_eq!(context.metadata, Some(metadata));
        assert!(context.pending_system_reminder.is_none());

        context.pending_system_reminder = Some("reminder".into());
        assert!(
            complete_tui_turn(&mut context, Err(CliError::storage("failed").into()), true).is_err()
        );
        assert_eq!(context.pending_system_reminder.as_deref(), Some("reminder"));
    }

    #[test]
    fn p1c4_completing_a_turn_keeps_a_subagent_turn_persisted_mid_flight() {
        let subagent_turn = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("review the patch".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::ToolCall {
                        id: "subagent:1".into(),
                        name: "native::task".into(),
                        input: r#"{"agent":"reviewer","description":"review the patch"}"#.into(),
                    },
                    MessagePart::Reasoning("3 tool uses".into()),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "subagent:1".into(),
                    content: "approved".into(),
                    is_error: false,
                }],
            },
        ];
        let foreground_turn = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("summarize the patch".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text("summary".into())],
            },
        ];
        let mut session = TuiSessionContext {
            identifier: Some(7),
            messages: subagent_turn.clone(),
            ..TuiSessionContext::fresh()
        };
        let completion = HeadlessChatCompletion {
            text: "summary".into(),
            metadata: SessionMetadata {
                id: 7,
                project: "project".into(),
                title: "conversation".into(),
                active_agent: "primary".into(),
                provider_id: None,
                model_id: None,
                reasoning_effort: None,
                created_at: 1,
                updated_at: 1,
                completed_turn_count: 1,
                resumable: true,
            },
            messages: foreground_turn.clone(),
        };

        assert_eq!(
            complete_tui_turn(&mut session, Ok(completion), false).unwrap(),
            "summary"
        );

        let mut expected = foreground_turn;
        expected.extend(subagent_turn);
        assert_eq!(session.messages, expected);
    }

    #[test]
    fn p1c2_resume_parser_restores_only_complete_standard_subagent_turns() {
        let messages = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("review the patch".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::ToolCall {
                        id: "subagent:42".into(),
                        name: "native::task".into(),
                        input: r#"{"agent":"reviewer","description":"review the patch"}"#.into(),
                    },
                    MessagePart::Reasoning("3 tool uses".into()),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "subagent:42".into(),
                    content: "approved".into(),
                    is_error: false,
                }],
            },
        ];

        assert_eq!(
            resumed_subagent_cards(&messages),
            vec![TuiRuntimeEvent::RestoredCompletedSubagent {
                id: 42,
                agent: "reviewer".into(),
                task_summary: "review the patch".into(),
                final_result: "approved".into(),
                tool_uses: 3,
            }]
        );

        let mut duplicate = messages.clone();
        duplicate.extend(messages.clone());
        assert_eq!(resumed_subagent_cards(&duplicate).len(), 1);

        let mut failed = messages;
        failed[2].parts = vec![MessagePart::ToolResult {
            tool_call_id: "subagent:42".into(),
            content: "failed".into(),
            is_error: true,
        }];
        assert!(resumed_subagent_cards(&failed).is_empty());

        let mut malformed = duplicate[..3].to_vec();
        malformed[1].parts[0] = MessagePart::ToolCall {
            id: "subagent:43".into(),
            name: "native::task".into(),
            input: "not json".into(),
        };
        assert!(resumed_subagent_cards(&malformed).is_empty());

        let incomplete = duplicate[..2].to_vec();
        assert!(resumed_subagent_cards(&incomplete).is_empty());

        let mut transient = duplicate[..3].to_vec();
        transient[2].parts = vec![MessagePart::ToolResult {
            tool_call_id: "subagent:43".into(),
            content: "cancelled".into(),
            is_error: true,
        }];
        assert!(resumed_subagent_cards(&transient).is_empty());
    }

    mod model_registry {
        use super::*;

        #[test]
        fn parses_tolerant_snapshot_filters_and_sorts_models() {
            let snapshot = br#"{
                "source": "https://models.dev",
                "revision": "test",
                "models": [
                    {"id":"z-model","name":"Z","context":4,"input_price":1.5,"output_price":2.5,"supported":true,"future":true},
                    {"id":"a-model","supported":true},
                    {"id":"unsupported","supported":false},
                    {"name":"missing-id","supported":true}
                ]
            }"#;

            let models = crate::model_registry::parse_models(snapshot).expect("snapshot parses");

            assert_eq!(models.len(), 2);
            assert_eq!(models[0].id, "a-model");
            assert_eq!(models[0].name, None);
            assert_eq!(models[0].context, None);
            assert_eq!(models[0].input_price, None);
            assert_eq!(models[0].output_price, None);
            assert_eq!(models[1].id, "z-model");
        }

        #[test]
        fn validates_bundled_snapshot_checksum_and_schema() {
            let models =
                crate::model_registry::bundled_openai_models().expect("bundled snapshot is valid");

            assert_eq!(
                crate::model_registry::bundled_snapshot_checksum(),
                "75086c4979636664367c3031c023b20479fb66296b197fe612b2b624696b5984"
            );
            assert_eq!(
                models.first().map(|model| model.id.as_str()),
                Some("gpt-4.1")
            );
            assert_eq!(
                models.last().map(|model| model.id.as_str()),
                Some("o4-mini")
            );
        }

        #[test]
        fn rejects_snapshot_schema_without_a_model_collection() {
            let result = crate::model_registry::parse_models(
                br#"{"source":"https://models.dev","revision":"test"}"#,
            );

            assert!(result.is_err());
        }

        #[test]
        fn formats_four_columns_and_an_explicit_empty_result() {
            let output = crate::model_registry::format_models(&[
                crate::model_registry::ModelMetadata {
                    id: "missing".to_owned(),
                    name: None,
                    context: None,
                    output: None,
                    reasoning: None,
                    input_price: None,
                    output_price: Some(0.6),
                },
                crate::model_registry::ModelMetadata {
                    id: "known".to_owned(),
                    name: Some("Known".to_owned()),
                    context: Some(128000),
                    output: None,
                    reasoning: None,
                    input_price: Some(2.5),
                    output_price: Some(10.0),
                },
            ]);

            assert_eq!(
                output,
                "ID\tNAME\tCONTEXT\tPRICE\nmissing\t-\t-\t-/$0.60\nknown\tKnown\t128000\t$2.50/$10.00\n"
            );
            assert_eq!(
                crate::model_registry::format_models(&[]),
                "No supported models.\n"
            );
        }

        #[test]
        fn context_window_for_returns_registry_value_or_none() {
            assert_eq!(
                crate::model_registry::context_window_for("gpt-4.1"),
                Some(1_047_576)
            );
            assert_eq!(
                crate::model_registry::context_window_for("gpt-5.5"),
                Some(272_000)
            );
            assert_eq!(
                crate::model_registry::context_window_for("not-a-real-model-xyz"),
                None
            );
        }

        #[test]
        fn models_command_uses_the_bundled_registry() {
            let result = execute_strings(
                vec!["models".to_owned()],
                &CliDependencies::for_test(
                    PathBuf::from("/workspace"),
                    None,
                    BTreeMap::new(),
                    BTreeMap::new(),
                ),
                &HeadlessTurnCancellation::new(),
            );

            assert_eq!(result.status, ExitStatus::Success);
            assert_eq!(
                result.stdout,
                "ID\tNAME\tCONTEXT\tPRICE\ngpt-4.1\tGPT-4.1\t1047576\t$2.00/$8.00\ngpt-4.1-mini\tGPT-4.1 mini\t1047576\t$0.40/$1.60\ngpt-4.1-nano\tGPT-4.1 nano\t1047576\t$0.10/$0.40\ngpt-4o\tGPT-4o\t128000\t$2.50/$10.00\ngpt-4o-mini\tGPT-4o mini\t128000\t$0.15/$0.60\no3\to3\t200000\t$2.00/$8.00\no4-mini\to4-mini\t200000\t$1.10/$4.40\n"
            );
        }
    }

    #[test]
    fn fresh_tui_presentation_projects_resolved_model_effort_and_context() {
        let known_root = tui_session_directory("fresh-presentation-known");
        let known_bootstrap =
            tui_session_bootstrap_for_provider(&known_root, &[], "openai-api", "gpt-5.6-sol");
        let mut known_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        known_tui.apply_presentation(tui_session_presentation(
            &known_bootstrap,
            &TuiSessionContext::fresh(),
        ));
        configure_tui_project_identity(&mut known_tui, &known_bootstrap);
        let known = render_tui_test_backend(&known_tui, 140, 14);

        assert!(
            known.contains("gpt-5.6-sol · medium · 0/1.1m (0%)"),
            "{known:?}"
        );
        assert!(!known.contains("model · default · ctx —"), "{known:?}");

        let unknown_root = tui_session_directory("fresh-presentation-unknown");
        let unknown_bootstrap =
            tui_session_bootstrap_for_provider(&unknown_root, &[], "openai-api", "gpt-future-1");
        let mut unknown_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        unknown_tui.apply_presentation(tui_session_presentation(
            &unknown_bootstrap,
            &TuiSessionContext::fresh(),
        ));
        let unknown = render_tui_test_backend(&unknown_tui, 140, 14);

        assert!(
            unknown.contains("gpt-future-1 · effort — · ctx —"),
            "{unknown:?}"
        );
        assert!(
            !unknown.contains("gpt-future-1 · effort — · 0/"),
            "{unknown:?}"
        );

        std::fs::remove_dir_all(known_root).unwrap();
        std::fs::remove_dir_all(unknown_root).unwrap();
    }

    #[test]
    fn dangerous_mode_is_visible_press_once_and_next_turn_only() {
        let temporary = tui_session_directory("dangerous-mode");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        tui.set_presentation("openai-api", "gpt-4.1", "new session");

        assert!(!render_tui_test_backend(&tui, 120, 24).contains("agens safe"));

        let Action::OpenDialog(route_id) = tui.handle(Event::Key(Key::CtrlShiftD)) else {
            panic!("Ctrl+Shift+D should route through the dangerous-mode router path");
        };
        assert_eq!(route_id, "dangerous");
        assert!(
            tui.apply_submission_outcome(router.route_request(
                TuiRouteRequest::OpenDialog(route_id),
                std::sync::mpsc::channel().0,
            ))
            .is_none()
        );
        assert!(session.lock().unwrap().dangerous_mode);
        assert!(render_tui_test_backend(&tui, 120, 24).contains("danger"));

        assert!(
            tui.apply_submission_outcome(router.route("/dangerous".into()))
                .is_none()
        );
        assert!(!session.lock().unwrap().dangerous_mode);
        assert!(!render_tui_test_backend(&tui, 120, 24).contains("agens safe"));

        tui.apply_submission_outcome(router.route("/dangerous".into()));
        let result = run_tui_prompt_with(&bootstrap, "next request", &session, None, |request| {
            assert!(request.dangerous_mode);
            assert!(matches!(
                router.route("/dangerous".into()),
                TuiSubmissionOutcome::ContextChanged { .. }
            ));
            assert!(request.dangerous_mode);
            Ok(HeadlessChatCompletion {
                text: "captured".into(),
                metadata: SessionMetadata {
                    id: 1,
                    project: "project".into(),
                    title: "captured".into(),
                    active_agent: "primary".into(),
                    provider_id: None,
                    model_id: None,
                    reasoning_effort: None,
                    created_at: 1,
                    updated_at: 1,
                    completed_turn_count: 1,
                    resumable: true,
                },
                messages: Vec::new(),
            })
        });
        assert!(result.is_ok());
        assert!(!session.lock().unwrap().dangerous_mode);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn dangerous_override_never_precedes_hard_safety_or_reuses_authorization() {
        let ordinary_deny = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::Exact("native::write".into()),
                PermissionPattern::Any,
            )],
        );
        let ordinary = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "dangerous-ordinary-deny",
                Vec::new(),
                vec![native_batch_call(
                    "ordinary",
                    "native::write",
                    serde_json::json!({"path":"notes.md","content":"allowed"}),
                )],
            )
            .with_policy(ordinary_deny)
            .with_dangerous_override(),
        );
        assert!(ordinary.result.is_ok());
        assert!(ordinary.prompts.is_empty());
        assert_eq!(ordinary.executions, ["notes.md"]);

        let hard_global_deny = PermissionPolicy::with_safety_predicates(
            PermissionMode::Edit,
            Vec::new(),
            vec![agens_core::SafetyPredicate::GlobalDeny(Box::new(
                agens_core::GlobalDenyPredicate {
                    tool: PermissionPattern::Exact("native::write".into()),
                    target: PermissionPattern::Any,
                },
            ))],
        );
        let global = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "dangerous-global-deny",
                Vec::new(),
                vec![native_batch_call(
                    "global",
                    "native::write",
                    serde_json::json!({"path":"blocked.md","content":"blocked"}),
                )],
            )
            .with_policy(hard_global_deny)
            .with_dangerous_override(),
        );
        assert!(global.result.is_ok());
        assert!(global.prompts.is_empty());
        assert!(global.executions.is_empty());

        let chat = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "dangerous-chat-write",
                Vec::new(),
                vec![native_batch_call(
                    "chat",
                    "native::write",
                    serde_json::json!({"path":"blocked.md","content":"blocked"}),
                )],
            )
            .with_policy(PermissionPolicy::new(PermissionMode::Chat, Vec::new()))
            .with_dangerous_override(),
        );
        assert!(chat.result.is_ok());
        assert!(chat.prompts.is_empty());
        assert!(chat.executions.is_empty());

        for (name, input) in [
            ("native::write", "{malformed"),
            (
                "native::task",
                r#"{"agent":"worker","description":"recursive"}"#,
            ),
            ("mcp::server::tool", r#"{}"#),
            ("native::unregistered", r#"{}"#),
        ] {
            let rejected = run_production_batch_with_policy(
                ProductionBatchInput::new(
                    "dangerous-invalid",
                    Vec::new(),
                    vec![MessagePart::ToolCall {
                        id: "rejected".into(),
                        name: name.into(),
                        input: input.into(),
                    }],
                )
                .with_dangerous_override(),
            );
            assert_eq!(
                rejected.result,
                Err(HeadlessTurnError::PermissionEvaluation),
                "{name} must be rejected before policy bypass"
            );
            assert!(rejected.prompts.is_empty());
            assert!(rejected.executions.is_empty());
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
        dispatcher
            .lock()
            .unwrap()
            .register_native(
                "native::write",
                ToolAccess::Write,
                BatchTool {
                    name: "native::write".into(),
                    calls: Arc::clone(&calls),
                    cancellation: None,
                },
            )
            .unwrap();
        let allowed = Arc::new(Mutex::new(BTreeMap::new()));
        let mut gate = ProductionPermissionGate::new(
            PermissionPolicy::new(PermissionMode::Edit, Vec::new()),
            Arc::new(Mutex::new(Vec::new())),
            PermissionSession::new(),
            "project".into(),
            Arc::clone(&dispatcher),
            Arc::clone(&allowed),
            Arc::new(Mutex::new(BTreeMap::new())),
        )
        .with_dangerous_override(true);
        let mut tool_dispatcher = ProductionToolDispatcher::new(dispatcher, allowed);
        let call = HeadlessToolCall {
            id: "once".into(),
            name: "native::write".into(),
            input: r#"{"path":"once.md","content":"once"}"#.into(),
        };
        let cancellation = HeadlessTurnCancellation::default();

        assert_eq!(
            poll_permission_port(gate.evaluate(&call, &cancellation)),
            Ok(PermissionDecision::Allow)
        );
        assert!(
            poll_permission_port(tool_dispatcher.dispatch(call.clone(), &cancellation)).is_ok()
        );
        assert_eq!(
            poll_permission_port(tool_dispatcher.dispatch(call, &cancellation)),
            Err(HeadlessTurnPortError::Tool)
        );
        assert_eq!(*calls.lock().unwrap(), ["once.md"]);

        let oversized = "x".repeat(agens_core::MAX_PERMISSION_TARGET_BYTES + 1);
        let oversized_call = HeadlessToolCall {
            id: "oversized".into(),
            name: "native::write".into(),
            input: serde_json::json!({"path": oversized, "content": "blocked"}).to_string(),
        };
        assert_eq!(
            poll_permission_port(gate.evaluate(&oversized_call, &cancellation)),
            Err(HeadlessTurnPortError::Permission)
        );

        let temporary = tui_session_directory("dangerous-confined-write");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let (_, dispatcher) =
            production_dangerous_child_tool_runtime(&project_root, ToolLimitSettings::default())
                .unwrap();
        let allowed = Arc::new(Mutex::new(BTreeMap::new()));
        let mut gate = ProductionPermissionGate::new(
            PermissionPolicy::new(PermissionMode::Edit, Vec::new()),
            Arc::new(Mutex::new(Vec::new())),
            PermissionSession::new(),
            "project".into(),
            Arc::clone(&dispatcher),
            Arc::clone(&allowed),
            Arc::new(Mutex::new(BTreeMap::new())),
        )
        .with_dangerous_override(true);
        let mut tool_dispatcher = ProductionToolDispatcher::new(dispatcher, allowed);
        let escape = HeadlessToolCall {
            id: "escape".into(),
            name: "native::write".into(),
            input: r#"{"path":"../escape.txt","content":"blocked"}"#.into(),
        };

        assert_eq!(
            poll_permission_port(gate.evaluate(&escape, &cancellation)),
            Ok(PermissionDecision::Allow)
        );
        assert!(
            poll_permission_port(tool_dispatcher.dispatch(escape, &cancellation))
                .expect("confined dispatcher should return a sanitized tool failure")
                .is_error
        );
        assert!(!temporary.join("escape.txt").exists());
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_list_filters_current_project_and_resume_preserves_typed_history() {
        let temporary = tui_session_directory("filter-resume");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let current = persist_tui_session(&mut store, &tui_project(&temporary), "current");
        persist_tui_session(
            &mut store,
            &temporary.join("other").display().to_string(),
            "other",
        );

        assert_eq!(list_tui_sessions(&bootstrap).unwrap(), "1\t1 event(s)");

        reset_tui_resume_test_counters();
        let resumed = resume_tui_session(
            &bootstrap,
            current.id,
            &SkillCatalog::default(),
            &TuiCredentialResolver::production(),
        )
        .unwrap();
        assert_eq!(resumed.identifier, Some(current.id));
        assert_eq!(resumed.metadata, Some(current));
        assert_eq!(resumed.messages, tui_session_messages());
        assert!(resumed.active_agent.is_none());
        assert_eq!(resumed.restored_history.len(), 1);
        assert_eq!(tui_resume_test_counters(), (1, 1, 0, 0));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    fn session_attempt_count(store: &SessionStore) -> i64 {
        Connection::open(store.database_path())
            .unwrap()
            .query_row("SELECT COUNT(*) FROM session_attempts", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn assert_restored_retry_draft_ui(outcome: TuiSubmissionOutcome, retry_prompt: &str) {
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        assert!(tui.begin_session_load());
        assert!(tui.apply_submission_outcome(outcome).is_none());
        let view = tui.view();
        assert_eq!(view.input, retry_prompt);
        assert_eq!(view.focus, agens_tui::TranscriptFocus::Composer);
        assert!(view.following_bottom);
        assert_eq!(
            view.status,
            Some("Recovered failed prompt · Enter retry · Esc discard")
        );
        assert!(view.completed_conversations.is_empty());
        assert!(!view.running);
        let rendered = render_tui_test_backend(&tui, 120, 24);
        assert!(rendered.contains(retry_prompt), "{rendered:?}");
        assert!(
            rendered.contains("Recovered failed prompt · Enter retry · Esc discard"),
            "{rendered:?}"
        );
    }

    #[test]
    fn zero_turn_failed_tui_resume_restores_draft_without_runtime_or_attempt_creation() {
        let temporary = tui_session_directory("failed-draft-resume");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = SessionMetadata {
            id: 0,
            project: tui_project(&temporary),
            title: "failed".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 10,
            updated_at: 20,
            completed_turn_count: 0,
            resumable: false,
        };
        let retry_prompt = "retry exact café 🙂";
        let attempt = store
            .begin_session_attempt(&metadata, retry_prompt.into())
            .unwrap();
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::Failed, 30)
            .unwrap();
        let attempt_count = session_attempt_count(&store);
        drop(store);

        reset_tui_resume_test_counters();
        let loaded = load_tui_session_for_resume(&bootstrap, attempt.key().session_id()).unwrap();
        assert_eq!(
            loaded.retry_boundary.as_ref().map(RetryBoundary::prompt),
            Some(retry_prompt)
        );
        let prepared = prepare_loaded_tui_session_resume(
            &bootstrap,
            attempt.key().session_id(),
            loaded,
            &TuiCredentialResolver::production(),
        )
        .unwrap();
        assert_eq!(prepared.resume_draft.as_deref(), Some(retry_prompt));
        assert!(!format!("{prepared:?}").contains(retry_prompt));
        assert_eq!(
            prepared.note(),
            "Recovered failed prompt · Enter retry · Esc discard"
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let expected = session.lock().unwrap().clone();
        let outcome = commit_tui_session_resume(
            &bootstrap,
            &session,
            &expected,
            prepared,
            &TuiRouteCancellation::new(),
        )
        .unwrap();
        assert!(session.lock().unwrap().resume_draft.is_none());
        assert_restored_retry_draft_ui(outcome.clone(), retry_prompt);
        let TuiSubmissionOutcome::SessionResumed {
            message,
            history,
            draft,
            ..
        } = outcome
        else {
            panic!("expected resumed outcome");
        };
        assert_eq!(
            message,
            "Recovered failed prompt · Enter retry · Esc discard"
        );
        assert!(history.is_empty());
        assert_eq!(draft.as_deref(), Some(retry_prompt));
        assert_eq!(tui_resume_test_counters(), (1, 1, 0, 0));

        let reopened = SessionStore::open(bootstrap.data_directory()).unwrap();
        let unchanged_attempt_count = session_attempt_count(&reopened);
        assert_eq!(unchanged_attempt_count, attempt_count);
        assert_eq!(
            reopened
                .load_session_for_resume(attempt.key().session_id())
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            SessionAttemptStatus::Failed
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn completed_history_resume_adds_failed_draft_without_duplicate_user_message() {
        let temporary = tui_session_directory("history-failed-draft");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "history");
        let retry_prompt = "failed next prompt";
        let attempt = store
            .begin_session_attempt(&metadata, retry_prompt.into())
            .unwrap();
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::ProviderError, 40)
            .unwrap();
        drop(store);

        let loaded = load_tui_session_for_resume(&bootstrap, metadata.id).unwrap();
        let prepared = prepare_loaded_tui_session_resume(
            &bootstrap,
            metadata.id,
            loaded,
            &TuiCredentialResolver::production(),
        )
        .unwrap();
        assert_eq!(prepared.messages, tui_session_messages());
        assert_eq!(prepared.restored_history.len(), 1);
        assert_eq!(prepared.resume_draft.as_deref(), Some(retry_prompt));
        assert_eq!(
            prepared.note(),
            "Recovered failed prompt · Enter retry · Esc discard"
        );
        assert!(
            prepared
                .messages
                .iter()
                .all(|message| message.role != Role::User
                    || message.parts != [MessagePart::Text(retry_prompt.into())])
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn completed_resume_without_retry_draft_and_cancelled_timeout_taxonomy_stay_explicit() {
        let temporary = tui_session_directory("completed-no-draft");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "completed");
        drop(store);

        let loaded = load_tui_session_for_resume(&bootstrap, metadata.id).unwrap();
        assert!(loaded.retry_boundary.is_none());
        let prepared = prepare_loaded_tui_session_resume(
            &bootstrap,
            metadata.id,
            loaded,
            &TuiCredentialResolver::production(),
        )
        .unwrap();
        assert!(prepared.resume_draft.is_none());
        assert!(prepared.note().starts_with("Resumed session"));
        assert_eq!(
            resume_retry_notice(SessionAttemptStatus::Cancelled),
            Some("Recovered failed prompt · Enter retry · Esc discard")
        );
        assert_eq!(
            attempt_failure_status(&CliError::runtime(HeadlessTurnError::TimedOut)),
            SessionAttemptStatus::Cancelled
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_resume_commit_discards_cancelled_stale_and_invalid_preparations() {
        let temporary = tui_session_directory("atomic-resume");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "atomic");
        let attempt = store
            .begin_session_attempt(&metadata, "atomic preserved draft".into())
            .unwrap();
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::Failed, 30)
            .unwrap();
        drop(store);
        let credentials = TuiCredentialResolver::production();
        let prepared = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &credentials,
        )
        .unwrap();
        assert_eq!(
            prepared.resume_draft.as_deref(),
            Some("atomic preserved draft")
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let original = session.lock().unwrap().clone();

        let cancelled = TuiRouteCancellation::new();
        cancelled.cancel();
        assert_eq!(
            commit_tui_session_resume(
                &bootstrap,
                &session,
                &original,
                prepared.clone(),
                &cancelled,
            )
            .unwrap(),
            TuiSubmissionOutcome::RouteCancelled
        );
        assert_eq!(*session.lock().unwrap(), original);

        session.lock().unwrap().dangerous_mode = true;
        let newer = session.lock().unwrap().clone();
        assert_eq!(
            commit_tui_session_resume(
                &bootstrap,
                &session,
                &original,
                prepared.clone(),
                &TuiRouteCancellation::new(),
            )
            .unwrap(),
            TuiSubmissionOutcome::RouteCancelled
        );
        assert_eq!(*session.lock().unwrap(), newer);

        *session.lock().unwrap() = original.clone();
        let accepted = TuiRouteCancellation::new();
        assert!(matches!(
            commit_tui_session_resume(&bootstrap, &session, &original, prepared, &accepted,)
                .unwrap(),
            TuiSubmissionOutcome::SessionResumed { .. }
        ));
        assert!(!accepted.cancel());
        let committed = session.lock().unwrap();
        assert_eq!(committed.identifier, Some(metadata.id));
        assert_eq!(committed.messages, tui_session_messages());
        assert!(committed.restored_history.is_empty());
        drop(committed);

        let mut invalid = load_tui_session_for_resume(&bootstrap, metadata.id).unwrap();
        invalid.session.messages = vec![Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("orphan".into())],
        }];
        let before_error = session.lock().unwrap().clone();
        assert!(
            prepare_loaded_tui_session_resume(&bootstrap, metadata.id, invalid, &credentials,)
                .is_err()
        );
        assert_eq!(*session.lock().unwrap(), before_error);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn first_runtime_materialization_after_resume_preserves_permission_denial() {
        let temporary = tui_session_directory("lazy-resume-runtime");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "lazy");
        drop(store);
        let skills = SkillCatalog::default();
        reset_tui_resume_test_counters();
        let resumed = resume_tui_session(
            &bootstrap,
            metadata.id,
            &skills,
            &TuiCredentialResolver::production(),
        )
        .unwrap();
        assert_eq!(tui_resume_test_counters(), (1, 1, 0, 0));
        let session = Arc::new(Mutex::new(resumed));
        let (permission_bridge, _) = TuiPermissionBridge::channel();
        let (events, _) = BridgeTx::bounded(8);
        let runtime = production_tui_task_runtime(
            &bootstrap,
            &skills,
            permission_bridge,
            TuiTaskLifecycleBridge::new(events, TuiTaskControls::default()),
            agens_core::RequestConfig::default(),
            "abc12345".to_owned(),
        )
        .unwrap();
        ensure_active_tui_agent_runtime(&bootstrap, &session, &runtime.dispatcher).unwrap();
        assert_eq!(tui_resume_test_counters(), (1, 1, 1, 0));
        assert_eq!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );

        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let outcome = runtime
            .dispatcher
            .lock()
            .unwrap()
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new(
                    tui_project(&temporary),
                    "task",
                    serde_json::json!({"agent":"explore","description":"inspect"}),
                ),
            )
            .unwrap();
        assert!(matches!(outcome, ToolEvaluationOutcome::Denied));
        assert_eq!(tui_resume_test_counters(), (1, 1, 1, 0));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn resumed_primary_inherits_every_effective_pinned_model_and_compatible_effort() {
        for provider in ["openai-api", "openai-chatgpt"] {
            for model in [
                "gpt-5.5",
                "gpt-5.6",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
            ] {
                let temporary =
                    tui_session_directory(&format!("resume-primary-{provider}-{model}"));
                let bootstrap =
                    tui_session_bootstrap_for_provider(&temporary, &[], provider, model);
                let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
                let mut metadata =
                    persist_tui_session(&mut store, &tui_project(&temporary), "inherited");
                metadata.provider_id = Some(provider.into());
                metadata.model_id = Some(model.into());
                metadata.reasoning_effort = Some(agens_core::ReasoningEffort::High);
                store.update_session_selection(&metadata).unwrap();
                drop(store);

                let resumed = resume_tui_session(
                    &bootstrap,
                    metadata.id,
                    &SkillCatalog::default(),
                    &TuiCredentialResolver::production(),
                )
                .unwrap();
                assert!(resumed.active_agent.is_none());
                let session = Arc::new(Mutex::new(resumed));
                let dispatcher = Arc::new(Mutex::new(rotation_dispatcher()));

                ensure_active_tui_agent_runtime(&bootstrap, &session, &dispatcher).unwrap();

                let context = session.lock().unwrap();
                let active = context.active_agent.as_ref().unwrap();
                assert_eq!(active.name, "primary", "{provider} {model}");
                assert_eq!(active.model.as_deref(), Some(model), "{provider} {model}");
                let request = context
                    .apply_to(chat_request(chat_args_with_prompt("first submission")).unwrap());
                assert_eq!(request.model.as_deref(), Some(model), "{provider} {model}");
                assert_eq!(
                    request.request_config.reasoning_effort(),
                    Some(agens_core::ReasoningEffort::High),
                    "{provider} {model}"
                );
                drop(context);

                std::fs::remove_dir_all(temporary).unwrap();
            }
        }
    }

    fn remember(bootstrap: &Bootstrap, model: &str, effort: Option<agens_core::ReasoningEffort>) {
        PreferenceStore::open(bootstrap.data_directory())
            .unwrap()
            .remember_model(&ModelPreference::new(model, effort))
            .unwrap();
    }

    #[test]
    fn a_new_session_inherits_the_remembered_model_and_its_effort() {
        let temporary = tui_session_directory("remembered-selection-fresh");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.model = None;
        remember(
            &bootstrap,
            "gpt-5.5",
            Some(agens_core::ReasoningEffort::High),
        );
        let mut context = TuiSessionContext::fresh();

        assert_eq!(
            seed_remembered_tui_selection(&bootstrap, &mut context),
            None
        );

        assert_eq!(effective_tui_model(&bootstrap, &context), "gpt-5.5");
        let request = context.apply_to(chat_request(chat_args_with_prompt("work")).unwrap());
        assert_eq!(request.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(
            request.session_reasoning_effort,
            Some(agens_core::ReasoningEffort::High)
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn a_configured_or_flagged_model_outranks_the_remembered_one() {
        let temporary = tui_session_directory("remembered-selection-outranked");
        let configured = tui_session_bootstrap(&temporary, &[]);
        remember(
            &configured,
            "gpt-5.5",
            Some(agens_core::ReasoningEffort::High),
        );
        let mut context = TuiSessionContext::fresh();

        assert_eq!(
            seed_remembered_tui_selection(&configured, &mut context),
            None
        );
        assert!(context.selection.is_none());
        assert_eq!(effective_tui_model(&configured, &context), "gpt-4.1");

        // A model flag reaches the same resolved slot as a configured model, so it outranks the
        // remembered pick through the same branch.
        let mut flagged = configured.clone();
        flagged.model = Some("o3".into());
        let mut context = TuiSessionContext::fresh();

        assert_eq!(seed_remembered_tui_selection(&flagged, &mut context), None);
        assert!(context.selection.is_none());
        assert_eq!(effective_tui_model(&flagged, &context), "o3");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn an_unavailable_remembered_model_falls_back_visibly() {
        let temporary = tui_session_directory("remembered-selection-unavailable");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.model = None;
        remember(&bootstrap, "gpt-5.4", None);
        let mut context = TuiSessionContext::fresh();

        assert_eq!(
            seed_remembered_tui_selection(&bootstrap, &mut context),
            Some(
                "Remembered model gpt-5.4 is unavailable for OpenAI API; using gpt-4.1.".to_owned()
            )
        );
        assert!(context.selection.is_none());
        assert_eq!(effective_tui_model(&bootstrap, &context), "gpt-4.1");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn an_effort_the_remembered_model_lost_falls_back_visibly() {
        let temporary = tui_session_directory("remembered-selection-effort");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.model = None;
        remember(
            &bootstrap,
            "gpt-4.1",
            Some(agens_core::ReasoningEffort::High),
        );
        let mut context = TuiSessionContext::fresh();

        assert_eq!(
            seed_remembered_tui_selection(&bootstrap, &mut context),
            Some(
                "Remembered reasoning effort is unsupported by gpt-4.1; using Default.".to_owned()
            )
        );
        let selection = context.selection.as_ref().unwrap();
        assert_eq!(selection.model(), "gpt-4.1");
        assert_eq!(selection.reasoning_effort(), None);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn choosing_a_model_and_an_effort_remembers_both_for_the_next_session() {
        let temporary = tui_session_directory("remembered-selection-write");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.model = None;
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));

        apply_tui_model(&bootstrap, "gpt-5.5", &session).unwrap();
        apply_tui_effort(&bootstrap, "high", &session).unwrap();

        let remembered = PreferenceStore::open(bootstrap.data_directory())
            .unwrap()
            .remembered_model()
            .unwrap()
            .unwrap();
        assert_eq!(remembered.model(), "gpt-5.5");
        assert_eq!(
            remembered.reasoning_effort(),
            Some(agens_core::ReasoningEffort::High)
        );

        let mut context = TuiSessionContext::fresh();
        assert_eq!(
            seed_remembered_tui_selection(&bootstrap, &mut context),
            None
        );
        assert_eq!(effective_tui_model(&bootstrap, &context), "gpt-5.5");
        assert_eq!(
            context
                .selection
                .as_ref()
                .and_then(TuiModelSelector::reasoning_effort),
            Some("high")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn model_switch_invalidates_and_rematerializes_inherited_primary_without_stale_model() {
        let temporary = tui_session_directory("active-agent-model-switch");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let dispatcher = Arc::new(Mutex::new(rotation_dispatcher()));
        ensure_active_tui_agent_runtime(&bootstrap, &session, &dispatcher).unwrap();
        assert_eq!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .and_then(|agent| agent.model.as_deref()),
            Some("gpt-5.5")
        );

        apply_tui_model(&bootstrap, "gpt-5.6-sol", &session).unwrap();
        assert!(session.lock().unwrap().active_agent.is_none());
        ensure_active_tui_agent_runtime(&bootstrap, &session, &dispatcher).unwrap();

        let context = session.lock().unwrap();
        assert_eq!(
            context
                .active_agent
                .as_ref()
                .and_then(|agent| agent.model.as_deref()),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            context
                .selection
                .as_ref()
                .unwrap()
                .reasoning_effort_default(),
            Some("medium")
        );
        drop(context);
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn stale_persisted_agent_falls_back_to_primary_warns_and_persists_correction() {
        let temporary = tui_session_directory("stale-active-agent-fallback");
        let stale_definition = "---\nname: retired\ndescription: retired\nmode: primary\npermissions:\n  - allow native::read\n---\nRetired work.\n";
        let bootstrap = tui_session_bootstrap(&temporary, &[("retired", stale_definition)]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session_metadata(
            &mut store,
            &tui_project(&temporary),
            "stale",
            "retired",
            100,
        );
        drop(store);
        std::fs::remove_file(
            bootstrap
                .paths
                .global_config
                .with_file_name("agents")
                .join("retired.md"),
        )
        .unwrap();

        let resumed = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &TuiCredentialResolver::production(),
        )
        .unwrap();

        assert_eq!(
            resumed.note(),
            "Agent 'retired' is unavailable; resumed with primary."
        );
        assert_eq!(resumed.metadata.as_ref().unwrap().active_agent, "primary");
        assert!(resumed.active_agent.is_none());
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .active_agent,
            "retired"
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let expected = session.lock().unwrap().clone();
        let outcome = commit_tui_session_resume(
            &bootstrap,
            &session,
            &expected,
            resumed,
            &TuiRouteCancellation::new(),
        )
        .unwrap();
        assert!(matches!(
            outcome,
            TuiSubmissionOutcome::SessionResumed { message, .. }
                if message == "Agent 'retired' is unavailable; resumed with primary."
        ));
        ensure_active_tui_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .active_agent,
            "primary"
        );
        assert!(!session.lock().unwrap().agent_correction_pending);
        assert!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .unwrap()
                .capabilities
                .descriptors()
                .is_empty()
        );
        let diagnostics = std::fs::read_to_string(
            bootstrap
                .data_directory()
                .join("diagnostics")
                .join(format!("agens-{}.jsonl", std::process::id())),
        )
        .unwrap();
        assert!(diagnostics.contains(r#""event":"agent_fallback""#));
        assert!(!diagnostics.contains("Retired work"));
        assert!(!diagnostics.contains(&tui_project(&temporary)));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn explicit_unavailable_agent_model_and_ineligible_primary_are_hard_errors() {
        for (case, definition, active_agent, expected) in [
            (
                "explicit-model",
                "---\nname: reviewer\ndescription: reviewer\nmode: primary\nmodel: gpt-4o\npermissions: []\n---\nReview.\n",
                "reviewer",
                "agent model is unavailable",
            ),
            (
                "ineligible-primary",
                "---\nname: primary\ndescription: primary\nmode: subagent\npermissions: []\n---\nWrong mode.\n",
                "primary",
                "primary agent is unavailable",
            ),
        ] {
            let temporary = tui_session_directory(case);
            let bootstrap = tui_session_bootstrap_for_provider(
                &temporary,
                &[(active_agent, definition)],
                "openai-chatgpt",
                "gpt-5.5",
            );
            let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
            let metadata = persist_tui_session_metadata(
                &mut store,
                &tui_project(&temporary),
                case,
                active_agent,
                100,
            );
            drop(store);

            let error = resume_tui_session(
                &bootstrap,
                metadata.id,
                &SkillCatalog::default(),
                &TuiCredentialResolver::production(),
            )
            .unwrap_err();
            assert_eq!(error.message, expected, "{case}");
            assert_eq!(
                SessionStore::open(bootstrap.data_directory())
                    .unwrap()
                    .load_session_for_resume(metadata.id)
                    .unwrap()
                    .metadata
                    .active_agent,
                active_agent,
                "{case}"
            );
            let diagnostics = std::fs::read_to_string(
                bootstrap
                    .data_directory()
                    .join("diagnostics")
                    .join(format!("agens-{}.jsonl", std::process::id())),
            )
            .unwrap();
            assert!(diagnostics.contains(r#""event":"agent_unavailable""#));
            assert!(!diagnostics.contains(definition));

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    #[test]
    fn explicit_agent_models_use_the_provider_aware_effective_registry() {
        for (provider, model, expected_effort) in [
            ("openai-api", "gpt-4o", None),
            ("openai-chatgpt", "gpt-5.4", None),
            ("openai-api", "gpt-5.6-luna", None),
            ("openai-chatgpt", "gpt-5.6-luna", None),
            (
                "openai-api",
                "gpt-5.5",
                Some(agens_core::ReasoningEffort::High),
            ),
            (
                "openai-chatgpt",
                "gpt-5.5",
                Some(agens_core::ReasoningEffort::High),
            ),
        ] {
            let temporary = tui_session_directory(&format!("explicit-{provider}-{model}"));
            let definition = format!(
                "---\nname: reviewer\ndescription: reviewer\nmode: primary\nmodel: {model}\npermissions: []\n---\nReview.\n"
            );
            let bootstrap = tui_session_bootstrap_for_provider(
                &temporary,
                &[("reviewer", &definition)],
                provider,
                "gpt-5.5",
            );
            let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
            let mut metadata = persist_tui_session_metadata(
                &mut store,
                &tui_project(&temporary),
                "explicit",
                "reviewer",
                100,
            );
            metadata.provider_id = Some(provider.into());
            metadata.model_id = Some("gpt-5.5".into());
            metadata.reasoning_effort = Some(agens_core::ReasoningEffort::High);
            store.update_session_selection(&metadata).unwrap();
            drop(store);
            let resumed = resume_tui_session(
                &bootstrap,
                metadata.id,
                &SkillCatalog::default(),
                &TuiCredentialResolver::production(),
            )
            .unwrap();
            let session = Arc::new(Mutex::new(resumed));

            ensure_active_tui_agent_runtime(
                &bootstrap,
                &session,
                &Arc::new(Mutex::new(rotation_dispatcher())),
            )
            .unwrap();

            let context = session.lock().unwrap();
            assert_eq!(context.active_agent.as_ref().unwrap().name, "reviewer");
            assert_eq!(
                context.active_agent.as_ref().unwrap().model.as_deref(),
                Some(model),
                "{provider} {model}"
            );
            let request = context.apply_to(chat_request(chat_args_with_prompt("review")).unwrap());
            assert_eq!(request.model.as_deref(), Some(model), "{provider} {model}");
            assert_eq!(
                request.request_config.reasoning_effort(),
                expected_effort,
                "{provider} {model}"
            );
            drop(context);
            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    #[test]
    fn explicit_agent_missing_keeps_active_primary_and_persisted_metadata_unchanged() {
        let temporary = tui_session_directory("explicit-agent-missing");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "primary");
        drop(store);
        let resumed = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &TuiCredentialResolver::production(),
        )
        .unwrap();
        let session = Arc::new(Mutex::new(resumed));
        ensure_active_tui_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();
        let before = session.lock().unwrap().clone();

        let error = rotate_tui_agent(&bootstrap, "missing", &session, &SkillCatalog::default())
            .unwrap_err();

        assert_eq!(error.category, "usage");
        assert_eq!(*session.lock().unwrap(), before);
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .active_agent,
            "primary"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn barrier_resume_loader_is_local_and_discards_its_late_cancelled_result() {
        let temporary = tui_session_directory("barrier-resume");
        let stale_definition = "---\nname: retired\ndescription: retired\nmode: primary\npermissions: []\n---\nRetired.\n";
        let bootstrap = tui_session_bootstrap(&temporary, &[("retired", stale_definition)]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session_metadata(
            &mut store,
            &tui_project(&temporary),
            "barrier",
            "retired",
            100,
        );
        drop(store);
        std::fs::remove_file(
            bootstrap
                .paths
                .global_config
                .with_file_name("agents")
                .join("retired.md"),
        )
        .unwrap();
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let original = session.lock().unwrap().clone();
        let cancellation = TuiRouteCancellation::new();
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let bootstrap = bootstrap.clone();
            let session = Arc::clone(&session);
            let original = original.clone();
            let cancellation = cancellation.clone();
            move || {
                reset_tui_resume_test_counters();
                started_sender.send(()).unwrap();
                release_receiver.recv().unwrap();
                let prepared = resume_tui_session(
                    &bootstrap,
                    metadata.id,
                    &SkillCatalog::default(),
                    &TuiCredentialResolver::production(),
                )
                .unwrap();
                let outcome = commit_tui_session_resume(
                    &bootstrap,
                    &session,
                    &original,
                    prepared,
                    &cancellation,
                )
                .unwrap();
                (outcome, tui_resume_test_counters())
            }
        });
        started_receiver.recv().unwrap();

        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        tui.set_presentation("old-provider", "old-model", "session #1");
        tui.begin_submission("old prompt");
        tui.finish_submission(Ok("old answer".into()));
        for character in "preserved draft".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        assert!(tui.begin_session_load());
        assert!(tui.view().session_loading);
        assert!(!tui.view().running);
        assert_eq!(tui.view().conversation.unwrap().user, "old prompt");

        assert!(cancellation.cancel());
        tui.cancel_session_load();
        release_sender.send(()).unwrap();
        let (outcome, counters) = worker.join().unwrap();
        assert_eq!(outcome, TuiSubmissionOutcome::RouteCancelled);
        assert_eq!(counters, (1, 1, 0, 0));
        assert_eq!(*session.lock().unwrap(), original);
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .active_agent,
            "retired"
        );
        assert_eq!(tui.view().provider_model, "old-provider / old-model");
        assert_eq!(tui.input(), "preserved draft");
        assert_eq!(tui.view().conversation.unwrap().user, "old prompt");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_resume_fails_closed_for_cross_project_missing_and_legacy_records() {
        let temporary = tui_session_directory("fail-closed");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        persist_tui_session(
            &mut store,
            &temporary.join("other").display().to_string(),
            "other",
        );
        let saved_sessions = store.list_sessions().unwrap();
        drop(store);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let original = session.lock().unwrap().clone();

        for command in ["/resume 1", "/resume 2"] {
            assert_eq!(
                run_tui_prompt(
                    &bootstrap,
                    command,
                    &HeadlessTurnCancellation::new(),
                    &session,
                    None,
                )
                .unwrap_err()
                .to_string(),
                "store: saved session is unavailable"
            );
            assert_eq!(*session.lock().unwrap(), original);
            assert_eq!(
                SessionStore::open(bootstrap.data_directory())
                    .unwrap()
                    .list_sessions()
                    .unwrap(),
                saved_sessions
            );
        }

        let legacy_temporary = tui_session_directory("legacy-fail-closed");
        let legacy_bootstrap = tui_session_bootstrap(&legacy_temporary, &[]);
        let mut legacy_store = SessionStore::open(legacy_bootstrap.data_directory()).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(
                legacy_store.persist_completed_turn(
                    CompletedTurnSnapshot::from_persisted_events(vec![
                        TurnEvent::StateChanged(TurnState::Requesting),
                        TurnEvent::StateChanged(TurnState::Streaming),
                        TurnEvent::ProviderPart(MessagePart::Text("legacy answer".into())),
                        TurnEvent::StateChanged(TurnState::Completed),
                    ])
                    .unwrap(),
                ),
            )
            .unwrap();
        drop(legacy_store);
        let legacy_session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let legacy_original = legacy_session.lock().unwrap().clone();
        assert_eq!(
            run_tui_prompt(
                &legacy_bootstrap,
                "/resume 1",
                &HeadlessTurnCancellation::new(),
                &legacy_session,
                None,
            )
            .unwrap_err()
            .to_string(),
            "store: saved session is unavailable"
        );
        assert_eq!(*legacy_session.lock().unwrap(), legacy_original);

        std::fs::remove_dir_all(temporary).unwrap();
        std::fs::remove_dir_all(legacy_temporary).unwrap();
    }

    #[test]
    fn tui_session_busy_resume_and_subagent_commands_leave_context_unchanged() {
        let temporary = tui_session_directory("busy");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext {
            identifier: Some(7),
            selected_subagent: Some("reviewer".into()),
            running: true,
            ..TuiSessionContext::fresh()
        }));
        let original = session.lock().unwrap().clone();

        for command in ["/resume 1", "/subagent reviewer"] {
            assert_eq!(
                run_tui_prompt(
                    &bootstrap,
                    command,
                    &HeadlessTurnCancellation::new(),
                    &session,
                    None,
                )
                .unwrap_err()
                .to_string(),
                "runtime: headless turn entered an invalid state"
            );
            assert_eq!(*session.lock().unwrap(), original);
        }

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_agent_selectors_expose_only_eligible_deterministic_options() {
        let temporary = tui_session_directory("agent-selectors");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[
                (
                    "all",
                    "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
                ),
                (
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                ),
            ],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));

        assert_eq!(
            list_tui_agents(&bootstrap, &session, AgentMode::Primary).unwrap(),
            "Active agent: none. Available: primary, all."
        );
        assert_eq!(
            list_tui_agents(&bootstrap, &session, AgentMode::Subagent).unwrap(),
            "Subagent: none. Available: explore, general, reviewer."
        );

        let no_agents_temporary = tui_session_directory("no-agent-selectors");
        let no_subagents = tui_session_bootstrap(&no_agents_temporary, &[]);
        assert_eq!(
            list_tui_agents(&no_subagents, &session, AgentMode::Subagent).unwrap(),
            "Subagent: none. Available: explore, general."
        );

        std::fs::remove_dir_all(temporary).unwrap();
        std::fs::remove_dir_all(no_agents_temporary).unwrap();
    }

    #[test]
    fn tui_session_agent_command_rotates_to_an_eligible_primary_agent() {
        let temporary = tui_session_directory("agent-command");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "all",
                "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
            )],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/agent all",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap(),
            "Active agent: all."
        );
        assert_eq!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("all")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_subagent_command_selects_an_exact_subagent() {
        let temporary = tui_session_directory("subagent-command");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        ensure_active_tui_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/subagent reviewer",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap(),
            "Subagent: reviewer."
        );
        assert_eq!(
            session.lock().unwrap().selected_subagent.as_deref(),
            Some("reviewer")
        );
        assert_eq!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn u15_c1a_subagent_overlay_and_alias_expose_only_eligible_agents() {
        let temporary = tui_session_directory("u15-c1a-subagents");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[
                (
                    "all",
                    "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
                ),
                (
                    "primary",
                    "---\nname: primary\ndescription: primary\nmode: primary\npermissions: []\n---\nPrimary work.\n",
                ),
                (
                    "invalid-model",
                    "---\nname: invalid-model\ndescription: invalid\nmode: subagent\nmodel: unavailable\npermissions: []\n---\nInvalid work.\n",
                ),
                (
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                ),
            ],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        assert!(
            router
                .palette_entries()
                .iter()
                .any(|entry| entry.name() == "subagent")
        );

        assert!(matches!(
            router.route("/subagent".into()),
            TuiSubmissionOutcome::SafeDialog(_)
        ));
        tui.set_running(true);
        assert!(
            tui.apply_submission_outcome(router.route("/subagent".into()))
                .is_none()
        );
        assert!(tui.view().running);
        let overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(!overlay.contains("main"));
        assert!(overlay.contains("explore"));
        assert!(overlay.contains("general"));
        assert!(overlay.contains("reviewer"));
        assert!(!overlay.contains("all"));
        assert!(!overlay.contains("primary"));
        assert!(!overlay.contains("invalid-model"));
        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::DialogAction("subagent:explore".into())
        );
        assert!(tui.transcript().is_empty());

        assert!(matches!(
            router.route("/subagent reviewer".into()),
            TuiSubmissionOutcome::ContextChanged { .. }
        ));
        assert_eq!(
            session.lock().unwrap().selected_subagent.as_deref(),
            Some("reviewer")
        );
        assert!(matches!(
            router.route("/subagent all".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ));

        let unavailable_bootstrap = tui_session_bootstrap_for_provider(
            &temporary,
            &[(
                "unavailable-provider",
                "---\nname: unavailable-provider\ndescription: unavailable\nmode: subagent\npermissions: []\n---\nUnavailable work.\n",
            )],
            "unavailable-provider",
            "gpt-4.1",
        );
        let unavailable_session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let unavailable_router = TuiRuntimeRouter::new(
            unavailable_bootstrap.clone(),
            Arc::clone(&unavailable_session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );

        assert!(
            !unavailable_router
                .palette_entries()
                .iter()
                .any(|entry| entry.name() == "subagent")
        );

        let unavailable_selection =
            unavailable_router.route("/subagent unavailable-provider".into());
        assert!(matches!(
            &unavailable_selection,
            TuiSubmissionOutcome::LocalActionableError { message, .. }
                if message.contains("No eligible subagents")
        ));
        assert!(
            unavailable_session
                .lock()
                .unwrap()
                .selected_subagent
                .is_none()
        );
        assert!(unavailable_session.lock().unwrap().messages.is_empty());

        let mut unavailable_tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        let captured = Arc::new(Mutex::new(Vec::new()));
        submit_tui_command(
            &mut unavailable_tui,
            &unavailable_router,
            &unavailable_bootstrap,
            "/subagent unavailable-provider",
            &captured,
        );
        assert!(captured.lock().unwrap().is_empty());
        assert!(!unavailable_tui.view().running);

        let empty_selection =
            unavailable_tui.apply_submission_outcome(unavailable_router.route("/subagent".into()));
        assert_eq!(empty_selection, None);
        let unavailable_overlay = render_tui_test_backend(&unavailable_tui, 80, 24);
        assert!(
            unavailable_overlay.contains("No eligible subagents are available."),
            "{unavailable_overlay:?}"
        );
        assert_eq!(
            unavailable_tui.handle(Event::Key(Key::Enter)),
            Action::Render
        );

        unavailable_tui.apply_submission_outcome(unavailable_router.route("/subagent".into()));
        assert_eq!(
            unavailable_tui.handle(Event::Key(Key::Escape)),
            Action::Render
        );
        assert!(unavailable_tui.transcript().is_empty());
        let unavailable_context = unavailable_session.lock().unwrap();
        assert!(unavailable_context.selected_subagent.is_none());
        assert!(unavailable_context.messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn plural_subagents_command_opens_the_transcript_picker_without_changing_next_type() {
        let temporary = tui_session_directory("plural-subagents");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext {
            selected_subagent: Some("explore".into()),
            ..TuiSessionContext::fresh()
        }));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );

        assert!(matches!(
            router.route("/subagents".into()),
            TuiSubmissionOutcome::TranscriptDialog
        ));
        assert_eq!(
            session.lock().unwrap().selected_subagent.as_deref(),
            Some("explore")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_new_command_synchronizes_idle_context() {
        let temporary = tui_session_directory("new-command");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let dispatcher = rotation_dispatcher();
        let active_agent = ActiveAgentRuntime::build(
            &rotation_agent("primary", Some("gpt-4.1"), true),
            None,
            &tui_project(&temporary),
            &dispatcher,
            &BundledModelValidator,
        )
        .unwrap();
        let session = Arc::new(Mutex::new(TuiSessionContext {
            identifier: Some(7),
            metadata: Some(SessionMetadata {
                id: 7,
                project: tui_project(&temporary),
                title: "conversation".into(),
                active_agent: "primary".into(),
                provider_id: None,
                model_id: None,
                reasoning_effort: None,
                created_at: 1,
                updated_at: 1,
                completed_turn_count: 1,
                resumable: true,
            }),
            messages: tui_session_messages(),
            active_agent: Some(active_agent),
            pending_system_reminder: Some("previous reminder".into()),
            selection: Some(TuiModelSelector::new("gpt-4.1")),
            selected_subagent: Some("reviewer".into()),
            ..TuiSessionContext::fresh()
        }));

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/new",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap(),
            "Started a new session."
        );
        assert_eq!(*session.lock().unwrap(), TuiSessionContext::fresh());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_enter_routes_unknown_slash_and_local_output_without_provider_history() {
        let temporary = tui_session_directory("enter-local-routing");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "current");
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine { cancellation });
        let input = enter_tui_input(&mut tui, "/unknown");
        let provider_invocations =
            usize::from(tui.apply_submission_outcome(router.route(input)).is_some());
        assert_eq!(provider_invocations, 0);
        assert!(tui.transcript().is_empty());
        assert!(tui.view().dialog.is_some());

        session.lock().unwrap().running = true;
        let input = enter_tui_input(&mut tui, "/new");
        tui.apply_submission_outcome(router.route(input));
        assert!(tui.view().dialog.is_some());

        session.lock().unwrap().running = false;
        let input = enter_tui_input(&mut tui, "/new");
        tui.apply_submission_outcome(router.route(input));
        assert!(tui.transcript().is_empty());
        assert_eq!(tui.view().status, Some("Started a new session."));

        let input = enter_tui_input(&mut tui, &format!("/resume {}", metadata.id));
        tui.apply_submission_outcome(router.route(input));
        assert_eq!(tui.view().session, format!("session #{}", metadata.id));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_startup_commands_route_real_enter_to_captured_provider_requests() {
        let temporary = tui_session_directory("declarative-commands");
        let config_home = temporary.join("config");
        let global_commands = config_home.join("commands");
        let project_commands = temporary.join("project/.agens/commands");
        std::fs::create_dir_all(&global_commands).unwrap();
        std::fs::create_dir_all(&project_commands).unwrap();
        for (root, name, description, template) in [
            (&global_commands, "shared", "global", "global:$ARGUMENTS"),
            (
                &global_commands,
                "global-only",
                "global only",
                "Keep literal text [$ARGUMENTS]",
            ),
            (
                &global_commands,
                "slash-template",
                "literal slash",
                "/literal $ARGUMENTS",
            ),
            (
                &global_commands,
                "connect",
                "collision",
                "must not run $ARGUMENTS",
            ),
            (&project_commands, "shared", "project", "project:$ARGUMENTS"),
        ] {
            write_tui_command(root, name, description, template);
        }
        std::fs::write(
            project_commands.join("broken.md"),
            "---\ndescription: [invalid\n---\nbroken\n",
        )
        .unwrap();

        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let commands = start_tui_commands(&mut tui, &bootstrap).unwrap();
        assert!(tui.view().dialog.is_some());
        assert!(tui.transcript().is_empty());
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            cancellation,
            commands,
            Arc::new(SkillCatalog::default()),
        );
        let captured = Arc::new(Mutex::new(Vec::new()));

        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/shared   hello world   ",
            &captured,
        );
        assert!(tui.transcript().contains(&agens_tui::TranscriptEntry::User(
            "/shared   hello world   ".into()
        )));
        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/global-only   value   ",
            &captured,
        );
        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/slash-template text",
            &captured,
        );

        let requests = captured.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.prompt.as_str())
                .collect::<Vec<_>>(),
            vec![
                "project:hello world",
                "Keep literal text [value]",
                "/literal text",
            ]
        );
        drop(requests);

        for input in ["/connect custom", "/unknown"] {
            submit_tui_command(&mut tui, &router, &bootstrap, input, &captured);
        }
        assert_eq!(captured.lock().unwrap().len(), 3);
        assert!(tui.view().dialog.is_some());
        assert!(session.lock().unwrap().messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_startup_skills_reach_parent_context_and_tool_with_builtin_subagents() {
        let temporary = tui_session_directory("parent-skills");
        let config_home = temporary.join("config");
        let global_skills = config_home.join("skills");
        let project_skills = temporary.join("project/.agens/skills");
        write_tui_skill(
            &global_skills,
            "alpha",
            "global alpha",
            "GLOBAL_ALPHA_BODY_SENTINEL",
        );
        write_tui_skill(
            &global_skills,
            "shared",
            "global shared",
            "GLOBAL_SHARED_BODY_SENTINEL",
        );
        write_tui_skill(
            &global_skills,
            "invoke",
            "global invoke",
            "GLOBAL_INVOKE_BODY_SENTINEL",
        );
        write_tui_skill(
            &project_skills,
            "shared",
            "project shared",
            "PROJECT_SHARED_BODY_SENTINEL",
        );
        write_tui_skill(
            &project_skills,
            "invoke",
            "project invoke",
            "PROJECT_INVOKE_BODY_SENTINEL",
        );
        write_tui_skill(
            &project_skills,
            "broken",
            "broken after startup",
            "BROKEN_BODY_SENTINEL",
        );
        let global_commands = config_home.join("commands");
        std::fs::create_dir_all(&global_commands).unwrap();
        write_tui_command(
            &global_commands,
            "shared",
            "command wins",
            "COMMAND:$ARGUMENTS",
        );
        std::fs::create_dir_all(project_skills.join("shared/references")).unwrap();
        std::fs::write(
            project_skills.join("shared/references/guide.md"),
            "RESOURCE_SENTINEL",
        )
        .unwrap();

        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let commands = start_tui_commands(&mut tui, &bootstrap).unwrap();
        let skills = start_tui_skills(&mut tui, &bootstrap).unwrap();
        report_tui_extension_collisions(&mut tui, &commands, &skills);
        assert!(tui.view().dialog.is_some());
        assert!(tui.transcript().is_empty());
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            session,
            cancellation,
            commands,
            Arc::clone(&skills),
        );
        let captured = Arc::new(Mutex::new(Vec::new()));

        submit_tui_command(&mut tui, &router, &bootstrap, "normal prompt", &captured);

        let request = captured.lock().unwrap()[0].clone();
        let context = request.system_prompt.unwrap();
        assert_eq!(context.matches("## Available skills").count(), 1);
        assert!(context.contains("- alpha: global alpha"));
        assert!(context.contains("- invoke: project invoke"));
        assert!(context.contains("- shared: project shared"));
        for secret in [
            "GLOBAL_ALPHA_BODY_SENTINEL",
            "GLOBAL_SHARED_BODY_SENTINEL",
            "GLOBAL_INVOKE_BODY_SENTINEL",
            "PROJECT_SHARED_BODY_SENTINEL",
            "PROJECT_INVOKE_BODY_SENTINEL",
            "BROKEN_BODY_SENTINEL",
            "RESOURCE_SENTINEL",
        ] {
            assert!(!context.contains(secret));
        }

        let (tools, dispatcher) = production_tool_runtime(
            &bootstrap,
            bootstrap.project_root().unwrap(),
            Some(skills.as_ref()),
        )
        .unwrap();
        assert!(tools.iter().any(|tool| tool.name() == "skill"));
        assert!(tools.iter().any(|tool| tool.name() == "task"));
        assert!(
            dispatcher
                .lock()
                .unwrap()
                .canonical_identity("skill")
                .is_some()
        );
        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::skill".into()),
                PermissionPattern::Any,
            )],
        );
        let mut dispatcher = dispatcher.lock().unwrap();
        let ToolEvaluationOutcome::Authorized(call) = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new("project", "skill", serde_json::json!({"skill":"shared"})),
            )
            .unwrap()
        else {
            panic!("skill tool should pass normal authorization");
        };
        assert_eq!(
            dispatcher
                .execute(
                    call,
                    &ToolExecutionContext::with_timeout(std::time::Duration::from_secs(1)),
                )
                .unwrap()
                .content,
            "PROJECT_SHARED_BODY_SENTINEL"
        );
        drop(dispatcher);

        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/invoke   explicit arguments   ",
            &captured,
        );
        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/shared command arguments",
            &captured,
        );
        std::fs::remove_file(project_skills.join("broken/SKILL.md")).unwrap();
        submit_tui_command(&mut tui, &router, &bootstrap, "/broken args", &captured);

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[1].prompt,
            "## Skill: invoke\nPROJECT_INVOKE_BODY_SENTINEL\n\n## User arguments\nexplicit arguments"
        );
        assert_eq!(requests[2].prompt, "COMMAND:command arguments");
        assert!(tui.transcript().contains(&agens_tui::TranscriptEntry::User(
            "/invoke   explicit arguments   ".into()
        )));
        assert!(tui.view().dialog.is_some());
        drop(requests);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_palette_uses_the_resolved_surface_and_renders_inside_a_narrow_resize() {
        let temporary = tui_session_directory("resolved-palette");
        let config_home = temporary.join("config");
        let global_commands = config_home.join("commands");
        let project_commands = temporary.join("project/.agens/commands");
        let global_skills = config_home.join("skills");
        let project_skills = temporary.join("project/.agens/skills");
        std::fs::create_dir_all(&global_commands).unwrap();
        std::fs::create_dir_all(&project_commands).unwrap();
        write_tui_command(&global_commands, "shared", "global command", "global");
        write_tui_command(&project_commands, "shared", "project command", "project");
        write_tui_command(
            &project_commands,
            "review",
            "review changes",
            "review:$ARGUMENTS",
        );
        write_tui_command(&project_commands, "connect", "reserved collision", "wrong");
        write_tui_skill(&global_skills, "shared", "shadowed skill", "wrong");
        write_tui_skill(&project_skills, "inspect", "inspect code", "INSPECT");
        std::fs::write(
            project_commands.join("broken.md"),
            "---\ndescription: [invalid\n---\nbroken\n",
        )
        .unwrap();

        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let commands = start_tui_commands(&mut tui, &bootstrap).unwrap();
        let skills = start_tui_skills(&mut tui, &bootstrap).unwrap();
        report_tui_extension_collisions(&mut tui, &commands, &skills);
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            commands,
            skills,
        );
        let entries = router.palette_entries();

        assert_eq!(
            entries.iter().map(|entry| entry.name()).collect::<Vec<_>>(),
            vec![
                "connect",
                "disconnect",
                "diagnostics",
                "new",
                "sessions",
                "resume",
                "agent",
                "provider",
                "model",
                "effort",
                "help",
                "mcp",
                "select",
                "quit",
                "subagent",
                "subagents",
                "review",
                "shared",
                "inspect",
            ]
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.name() == "shared")
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.name() == "shared")
                .unwrap()
                .kind(),
            agens_tui::PaletteEntryKind::Command
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.name() == "shared")
                .unwrap()
                .description(),
            "project command"
        );
        assert!(entries.iter().any(|entry| entry.name() == "subagent"));
        assert!(tui.transcript().is_empty());
        assert!(tui.view().dialog.is_some());

        tui.set_palette_entries(entries.to_vec());
        for character in "/sha".chars() {
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
        }
        tui.handle(agens_tui::Event::Resize {
            width: 20,
            height: 6,
        });
        let terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6)).unwrap();
        let mut renderer = agens_tui::RatatuiRenderer::new(terminal);
        agens_tui::Renderer::render(&mut renderer, tui.view()).unwrap();
        let text = renderer
            .terminal()
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("commands"), "{text:?}");
        assert!(text.contains("/shared"), "{text:?}");
        assert!(!text.contains("inspect"), "{text:?}");

        let original = session.lock().unwrap().clone();
        assert_eq!(
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Escape)),
            agens_tui::Action::Render
        );
        assert_eq!(tui.input(), "/sha");
        assert_eq!(*session.lock().unwrap(), original);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_palette_enter_routes_built_in_command_skill_help_quit_and_unknown_once() {
        let temporary = tui_session_directory("palette-routing");
        let config_home = temporary.join("config");
        let project_commands = temporary.join("project/.agens/commands");
        let project_skills = temporary.join("project/.agens/skills");
        std::fs::create_dir_all(config_home.join("commands")).unwrap();
        std::fs::create_dir_all(&project_commands).unwrap();
        write_tui_command(
            &project_commands,
            "review",
            "review changes",
            "REVIEW:$ARGUMENTS",
        );
        write_tui_skill(&project_skills, "inspect", "inspect code", "INSPECT_BODY");

        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let commands = start_tui_commands(&mut tui, &bootstrap).unwrap();
        let skills = start_tui_skills(&mut tui, &bootstrap).unwrap();
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            commands,
            skills,
        );
        tui.set_palette_entries(router.palette_entries().to_vec());
        let mut provider_prompts = Vec::new();

        for (input, expected) in [
            ("/review target", "REVIEW:target"),
            (
                "/inspect src",
                "## Skill: inspect\nINSPECT_BODY\n\n## User arguments\nsrc",
            ),
        ] {
            let input = enter_tui_input(&mut tui, input);
            let prompt = tui.apply_submission_outcome(router.route(input)).unwrap();
            provider_prompts.push(prompt.clone());
            tui.finish_provider_turn(TuiProviderOutcome::Completed("captured".into()));
            assert_eq!(prompt, expected);
        }

        let sessions = router.open_dialog("sessions").unwrap();
        assert!(matches!(sessions, TuiSubmissionOutcome::Dialog(_)));
        assert!(matches!(
            router.route("/help".into()),
            TuiSubmissionOutcome::Dialog(_)
        ));
        assert!(matches!(
            router.route("/mouse".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ));

        let unknown = enter_tui_input(&mut tui, "/unknown");
        assert!(
            tui.apply_submission_outcome(router.route(unknown))
                .is_none()
        );
        assert_eq!(provider_prompts.len(), 2);
        assert!(session.lock().unwrap().messages.is_empty());

        let quit = enter_tui_input(&mut tui, "/quit");
        assert_eq!(router.route(quit), TuiSubmissionOutcome::Quit);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn dialog_recovery_is_confirmed_private_local_safe_and_retryable() {
        let temporary = tui_session_directory("recovery-dialog");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            Arc::clone(&cancellation),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let metadata = SessionMetadata {
            id: 1,
            project: tui_project(&temporary),
            title: "Interrupted session".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 7,
            completed_turn_count: 0,
            resumable: false,
        };
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let attempt = store
            .begin_session_attempt(&metadata, "SENTINEL_PRIVATE_RETRY".into())
            .unwrap();
        drop(store);

        let confirmation = router.route_dialog_action("session:1", std::sync::mpsc::channel().0);
        let confirmation_debug = format!("{confirmation:?}");
        let mut tui = Tui::new(ProductionTuiEngine { cancellation });
        assert!(tui.apply_submission_outcome(confirmation).is_none());
        let confirmation_text = render_tui_test_backend(&tui, 100, 24);
        assert!(confirmation_text.contains("Recover interrupted attempt"));
        assert!(confirmation_text.contains("Interrupted session"));
        assert!(confirmation_text.contains("ID: 1"));
        assert!(confirmation_text.contains("Status: running"));
        assert!(confirmation_text.contains("Started: 7"));
        assert!(
            confirmation_debug
                .contains("This may invalidate an attempt still running in another process.")
        );
        assert!(!confirmation_debug.contains("SENTINEL_PRIVATE_RETRY"));

        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        let store = SessionStore::open(bootstrap.data_directory()).unwrap();
        assert_eq!(
            store
                .load_session_for_resume(1)
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Running
        );
        drop(store);

        let locally_active_metadata = SessionMetadata {
            id: 2,
            ..metadata.clone()
        };
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let locally_active = active_session_attempts()
            .begin_and_register(
                &mut store,
                &locally_active_metadata,
                "local private retry".into(),
            )
            .unwrap();
        drop(store);
        let local_refusal = router.route_dialog_action(
            &format!(
                "session:recover:{}:{}",
                locally_active.key().session_id(),
                locally_active.key().attempt_id()
            ),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(local_refusal, TuiSubmissionOutcome::Dialog(_)));
        let store = SessionStore::open(bootstrap.data_directory()).unwrap();
        assert_eq!(
            store
                .load_session_for_resume(locally_active.key().session_id())
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Running
        );
        drop(store);
        active_session_attempts().unregister(locally_active.key());

        let recovered = router.route_dialog_action(
            &format!(
                "session:recover:{}:{}",
                attempt.key().session_id(),
                attempt.key().attempt_id()
            ),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(
            recovered,
            TuiSubmissionOutcome::ProviderTurn { ref display, ref prompt }
                if display == "Retrying recovered attempt." && prompt == "SENTINEL_PRIVATE_RETRY"
        ));
        assert_eq!(session.lock().unwrap().identifier, Some(1));
        let store = SessionStore::open(bootstrap.data_directory()).unwrap();
        assert_eq!(
            store
                .load_session_for_resume(1)
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Interrupted
        );

        let stale = router.route_dialog_action(
            &format!(
                "session:recover:{}:{}",
                attempt.key().session_id(),
                attempt.key().attempt_id()
            ),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(stale, TuiSubmissionOutcome::Dialog(_)));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_model_effort_and_help_palette_routes_open_local_overlays_and_dispatch_once() {
        let temporary = tui_session_directory("local-overlays");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        tui.set_palette_entries(router.palette_entries().to_vec());
        let (progress, _) = std::sync::mpsc::channel();

        for (prefix, route_id, expected) in [
            ("/mo", "model", ["Choose model", "gpt-4.1 (current)"]),
            ("/ef", "effort", ["Choose effort", "Default"]),
            ("/he", "help", ["Commands and skills", "/connect"]),
        ] {
            for character in prefix.chars() {
                tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
            }
            let agens_tui::Action::OpenDialog(actual_route) =
                tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter))
            else {
                panic!("palette Enter should open the selected overlay");
            };
            assert_eq!(actual_route, route_id);
            let outcome = router.route_request(
                agens_tui::TuiRouteRequest::OpenDialog(actual_route),
                progress.clone(),
            );
            assert!(tui.apply_submission_outcome(outcome).is_none());
            let text = render_tui_test_backend(&tui, 80, 24);
            assert!(text.contains(expected[0]), "{route_id}: {text:?}");
            assert!(text.contains(expected[1]), "{route_id}: {text:?}");

            if route_id == "help" {
                assert_eq!(
                    tui.handle(agens_tui::Event::Key(agens_tui::Key::CtrlC)),
                    agens_tui::Action::Render
                );
                continue;
            }
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Down));
            let agens_tui::Action::DialogAction(action_id) =
                tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter))
            else {
                panic!("dialog Enter should emit one action ID");
            };
            let outcome = router.route_request(
                agens_tui::TuiRouteRequest::DialogAction(action_id),
                progress.clone(),
            );
            assert!(tui.apply_submission_outcome(outcome).is_none());
            assert!(tui.view().dialog.is_none());
        }

        assert!(session.lock().unwrap().messages.is_empty());
        assert!(
            tui.transcript()
                .iter()
                .all(|entry| !matches!(entry, agens_tui::TranscriptEntry::User(_)))
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_mcp_overlay_is_local_safe_refreshable_and_includes_disabled_servers() {
        let temporary = tui_session_directory("mcp-overlay");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.mcp_servers = vec![
            agens_config::McpServerConfig {
                name: "files".into(),
                disabled: false,
                transport: McpTransport::Stdio,
                command: Some("/private/bin/files-server".into()),
                args: vec!["SENTINEL_ARG_SECRET".into()],
                environment: BTreeMap::from([("TOKEN".into(), "SENTINEL_ENV_SECRET".into())]),
                cwd: None,
                url: None,
                headers: BTreeMap::new(),
                max_retries: 0,
                timeout_ms: 250,
            },
            agens_config::McpServerConfig {
                name: "disabled".into(),
                disabled: true,
                transport: McpTransport::Sse,
                command: None,
                args: Vec::new(),
                environment: BTreeMap::new(),
                cwd: None,
                url: Some("https://user:SENTINEL_URL_SECRET@example.test/mcp?token=secret".into()),
                headers: BTreeMap::from([(
                    "Authorization".into(),
                    "SENTINEL_HEADER_SECRET".into(),
                )]),
                max_retries: 0,
                timeout_ms: 500,
            },
        ];
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        assert!(
            tui.apply_submission_outcome(router.route("/mcp".into()))
                .is_none()
        );
        for character in "idle".chars() {
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
        }
        let filtered = render_tui_test_backend(&tui, 90, 24);
        assert!(filtered.contains("files") && !filtered.contains("disabled  sse"));
        tui.handle(agens_tui::Event::Key(agens_tui::Key::Escape));
        tui.apply_submission_outcome(router.open_dialog("mcp").unwrap());
        tui.handle(agens_tui::Event::Key(agens_tui::Key::Down));
        tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter));
        let text = render_tui_test_backend(&tui, 90, 24);
        assert!(text.contains("stdio"), "{text:?}");
        assert!(text.contains("enabled/idle"), "{text:?}");
        assert!(text.contains("disabled"), "{text:?}");
        assert!(text.contains("Source: global"), "{text:?}");
        assert!(text.contains("files-server"), "{text:?}");
        assert!(text.contains("250ms"), "{text:?}");
        for secret in [
            "SENTINEL_ARG_SECRET",
            "SENTINEL_ENV_SECRET",
            "SENTINEL_URL_SECRET",
            "SENTINEL_HEADER_SECRET",
        ] {
            assert!(!text.contains(secret), "{secret}: {text:?}");
        }

        let mut live = McpRegistry::with_status_handle(router.mcp_status.clone());
        live.register_disabled_server(McpServerDescriptor::new(
            "later",
            McpServerSource::Global,
            McpServerTransport::Stdio,
            false,
            std::time::Duration::from_secs(10),
            None,
        ))
        .unwrap();
        let agens_tui::Action::OpenDialog(route_id) =
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char('r')))
        else {
            panic!("MCP refresh should remain local");
        };
        let refreshed = router.open_dialog(&route_id).unwrap();
        tui.apply_submission_outcome(refreshed);
        assert!(render_tui_test_backend(&tui, 90, 24).contains("later"));
        assert!(session.lock().unwrap().messages.is_empty());
        assert!(tui.transcript().is_empty());
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_model_overlay_labels_source_metadata_current_and_compatible_sets() {
        for (provider, source, included, excluded) in [
            ("openai-api", "OpenAI API", "gpt-4o", "gpt-5.4"),
            (
                "openai-chatgpt",
                "ChatGPT subscription",
                "gpt-5.4",
                "gpt-4o",
            ),
        ] {
            let temporary = tui_session_directory(&format!("model-source-{provider}"));
            let bootstrap =
                tui_session_bootstrap_for_provider(&temporary, &[], provider, "gpt-5.5");
            let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
            let cancellation = Arc::new(Mutex::new(None));
            let mut tui = Tui::new(ProductionTuiEngine {
                cancellation: Arc::clone(&cancellation),
            });
            let router = TuiRuntimeRouter::new(
                bootstrap,
                Arc::clone(&session),
                cancellation,
                Arc::new(CommandCatalog::default()),
                Arc::new(SkillCatalog::default()),
            );
            let (progress, _) = std::sync::mpsc::channel();

            assert!(
                tui.apply_submission_outcome(
                    router.route_request(TuiRouteRequest::OpenDialog("model".into()), progress)
                )
                .is_none()
            );
            let text = render_tui_test_backend(&tui, 140, 60);

            assert!(text.contains(source), "{provider}: {text:?}");
            assert!(text.contains("gpt-5.5 (current)"), "{provider}: {text:?}");
            assert!(text.contains(included), "{provider}: {text:?}");
            assert!(!text.contains(excluded), "{provider}: {text:?}");
            assert!(text.contains("272K context"), "{provider}: {text:?}");
            assert!(text.contains("128K output"), "{provider}: {text:?}");
            assert!(text.contains("reasoning"), "{provider}: {text:?}");

            let source = if provider == "openai-chatgpt" {
                TuiModelSource::ChatGptSubscription
            } else {
                TuiModelSource::OpenAiApi
            };
            let models = TuiModelSelector::for_source("gpt-5.5", source)
                .models()
                .unwrap();
            let family = models
                .iter()
                .filter(|model| model.id.starts_with("gpt-5.6"))
                .map(|model| {
                    (
                        model.id.as_str(),
                        model.name.as_deref(),
                        model.context,
                        model.output,
                        model.reasoning,
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                family,
                [
                    (
                        "gpt-5.6",
                        Some("GPT-5.6 (Sol alias)"),
                        Some(1_050_000),
                        Some(128_000),
                        Some(true)
                    ),
                    (
                        "gpt-5.6-luna",
                        Some("GPT-5.6 Luna"),
                        Some(1_050_000),
                        Some(128_000),
                        Some(true)
                    ),
                    (
                        "gpt-5.6-sol",
                        Some("GPT-5.6 Sol"),
                        Some(1_050_000),
                        Some(128_000),
                        Some(true)
                    ),
                    (
                        "gpt-5.6-terra",
                        Some("GPT-5.6 Terra"),
                        Some(1_050_000),
                        Some(128_000),
                        Some(true)
                    ),
                ],
                "official OpenAI GPT-5.6 catalog metadata for {provider}"
            );
            for model in &family {
                assert_eq!(
                    models
                        .iter()
                        .filter(|candidate| candidate.id == model.0)
                        .count(),
                    1,
                    "duplicate {} in {provider}",
                    model.0
                );
            }
            assert!(text.contains("gpt-5.6"), "{provider}: {text:?}");
            assert!(text.contains("gpt-5.6-luna"), "{provider}: {text:?}");
            assert!(
                !text.contains("unverified metadata"),
                "{provider}: {text:?}"
            );

            for _ in 0..4 {
                tui.handle(Event::Key(Key::Down));
            }
            let scrolled = render_tui_test_backend(&tui, 80, 24);
            assert!(scrolled.contains("gpt-5.6-sol"), "{provider}: {scrolled:?}");
            assert!(
                scrolled.contains("gpt-5.6-terra"),
                "{provider}: {scrolled:?}"
            );
            tui.handle(Event::Key(Key::Up));
            tui.handle(Event::Key(Key::Up));
            tui.handle(Event::Key(Key::Up));
            let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
                panic!("verified gpt-5.6 alias should be selectable");
            };
            let outcome = router.route_request(
                TuiRouteRequest::DialogAction(action_id),
                std::sync::mpsc::channel().0,
            );
            assert!(matches!(
                &outcome,
                TuiSubmissionOutcome::ContextChanged { message, presentation }
                    if message == "Model: gpt-5.6."
                        && presentation
                            == &TuiPresentation::new(provider, "gpt-5.6", "new session")
                                .with_effort("medium")
                                .with_context_window(Some(1_050_000))
            ));
            tui.apply_submission_outcome(outcome);
            let selection = session.lock().unwrap().selection.clone().unwrap();
            assert!(selection.metadata_known());
            assert_eq!(selection.reasoning_effort_default(), Some("medium"));
            assert_eq!(
                selection.reasoning_effort_values(),
                ["default", "none", "low", "medium", "high", "xhigh", "max"]
            );

            tui.apply_submission_outcome(router.open_dialog("model").unwrap());
            for character in "gpt-5.6-sol".chars() {
                tui.handle(Event::Key(Key::Char(character)));
            }
            let filtered = render_tui_test_backend(&tui, 80, 24);
            assert!(filtered.contains("gpt-5.6-sol"), "{provider}: {filtered:?}");
            assert!(
                !filtered.contains("unverified metadata"),
                "{provider}: {filtered:?}"
            );

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    #[test]
    fn tui_provider_overlay_filters_unavailable_entries_and_switches_without_history() {
        let temporary = tui_session_directory("provider-overlay");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        std::fs::write(
            &bootstrap.paths.credentials,
            r#"{"openai-chatgpt":{"access_token":"secret-access","refresh_token":"secret-refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::with_credential_resolver(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            TuiCredentialResolver::with_environment(BTreeMap::new()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        let (progress, _) = std::sync::mpsc::channel();
        tui.apply_submission_outcome(router.route_request(
            TuiRouteRequest::OpenDialog("provider".into()),
            progress.clone(),
        ));
        let overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(
            overlay.contains("Current: OpenAI API · credential required"),
            "{overlay:?}"
        );
        assert!(overlay.contains("❯ ChatGPT subscription"), "{overlay:?}");
        assert!(overlay.contains("ready"), "{overlay:?}");
        assert!(!overlay.contains("OpenAI API (current)"), "{overlay:?}");
        assert!(!overlay.contains("secret-"), "{overlay:?}");

        dispatch_tui_dialog_selection(&mut tui, &router, progress);
        assert_eq!(tui.view().provider_model, "openai-chatgpt / gpt-5.5");
        tui.apply_submission_outcome(router.open_dialog("model").unwrap());
        let model_overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(model_overlay.contains("Source: ChatGPT subscription"));
        assert!(model_overlay.contains("gpt-5.5 (current)"));
        assert!(tui.transcript().is_empty());
        assert!(session.lock().unwrap().messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_provider_switch_reconciles_compatible_incompatible_and_busy_state_atomically() {
        let temporary = tui_session_directory("provider-reconcile");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        std::fs::write(
            &bootstrap.paths.credentials,
            r#"{"openai-chatgpt":{"access_token":"access","refresh_token":"refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::with_credential_resolver(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            TuiCredentialResolver::with_environment(BTreeMap::from([(
                "OPENAI_API_KEY".into(),
                "api-secret".into(),
            )])),
        );

        let retained = router.route("/provider openai-chatgpt".into());
        assert!(
            matches!(retained, TuiSubmissionOutcome::ContextChanged { ref message, .. } if message.contains("Model retained: gpt-5.5"))
        );
        router.route("/model gpt-5.4".into());
        router.route("/effort high".into());
        let reset = router.route("/provider openai-api".into());
        assert!(
            matches!(reset, TuiSubmissionOutcome::ContextChanged { ref message, .. } if message.contains("Model reset to gpt-4.1") && message.contains("Default"))
        );
        let idle = session.lock().unwrap().clone();
        assert_eq!(idle.selection.as_ref().unwrap().model(), "gpt-4.1");
        assert_eq!(idle.selection.as_ref().unwrap().reasoning_effort(), None);
        let mut context = session.lock().unwrap();
        context.messages = tui_session_messages();
        context.running = true;
        drop(context);
        let busy = session.lock().unwrap().clone();
        assert!(matches!(
            router.route("/provider openai-chatgpt".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ));
        assert_eq!(*session.lock().unwrap(), busy);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_turn_bootstrap_resolves_changed_and_removed_credentials_without_stale_reuse() {
        let temporary = tui_session_directory("fresh-turn-credentials");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let configured_provider = bootstrap.provider_type.clone();
        let credentials = bootstrap.paths.credentials.clone();
        let environment = Arc::new(Mutex::new(BTreeMap::new()));
        let resolver = TuiCredentialResolver::with_environment_resolver({
            let environment = Arc::clone(&environment);
            move || environment.lock().unwrap().clone()
        });
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::with_credential_resolver(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            resolver,
        );

        std::fs::write(&credentials, r#"{"openai-api":{"api_key":"file-one"}}"#).unwrap();
        assert_eq!(
            router.turn_bootstrap().unwrap().openai_api_key.as_deref(),
            Some("file-one")
        );
        std::fs::write(&credentials, r#"{"openai-api":{"api_key":"file-two"}}"#).unwrap();
        assert_eq!(
            router.turn_bootstrap().unwrap().openai_api_key.as_deref(),
            Some("file-two")
        );
        environment
            .lock()
            .unwrap()
            .insert("OPENAI_API_KEY".into(), "env-current".into());
        assert_eq!(
            router.turn_bootstrap().unwrap().openai_api_key.as_deref(),
            Some("env-current")
        );
        environment.lock().unwrap().clear();
        std::fs::remove_file(&credentials).unwrap();
        assert!(router.turn_bootstrap().is_err());

        session.lock().unwrap().provider = Some(TuiProvider::OpenAiChatGpt);
        std::fs::write(
            &credentials,
            r#"{"openai-chatgpt":{"access_token":"chat-access","refresh_token":"chat-refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(
            router.turn_bootstrap().unwrap().provider_type(),
            Some("openai-chatgpt")
        );
        std::fs::remove_file(&credentials).unwrap();
        assert!(router.turn_bootstrap().is_err());
        assert!(matches!(
            router.route("/provider openai-chatgpt".into()),
            TuiSubmissionOutcome::LocalActionableError { ref message, .. }
                if message.contains("run /connect")
        ));
        assert_eq!(
            router.bootstrap().unwrap().provider_type,
            configured_provider
        );
        assert!(session.lock().unwrap().messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn persisted_selection_updates_and_resume_are_atomic_and_credential_fresh() {
        let temporary = tui_session_directory("persisted-selection");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let mut metadata = persist_tui_session(&mut store, &tui_project(&temporary), "selection");
        metadata.provider_id = Some("openai-api".into());
        metadata.model_id = Some("gpt-5.5".into());
        metadata.reasoning_effort = Some(agens_core::ReasoningEffort::High);
        store.update_session_selection(&metadata).unwrap();
        drop(store);
        let resolver = TuiCredentialResolver::with_environment(BTreeMap::from([(
            "OPENAI_API_KEY".into(),
            "fresh-secret".into(),
        )]));
        let resumed =
            resume_tui_session(&bootstrap, metadata.id, &SkillCatalog::default(), &resolver)
                .unwrap();
        assert_eq!(resumed.selection.as_ref().unwrap().model(), "gpt-5.5");
        assert_eq!(
            resumed.selection.as_ref().unwrap().reasoning_effort(),
            Some("high")
        );
        let session = Arc::new(Mutex::new(resumed));
        let router = TuiRuntimeRouter::with_credential_resolver(
            bootstrap.clone(),
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            resolver,
        );
        assert_eq!(router.turn_bootstrap().unwrap().model(), Some("gpt-5.5"));
        assert_eq!(
            router
                .task_parent_request_config()
                .unwrap()
                .reasoning_effort(),
            Some(agens_core::ReasoningEffort::High)
        );
        assert!(matches!(
            router.route("/model gpt-4.1".into()),
            TuiSubmissionOutcome::ContextChanged { .. }
        ));
        assert_eq!(router.turn_bootstrap().unwrap().model(), Some("gpt-4.1"));
        assert_eq!(
            router
                .task_parent_request_config()
                .unwrap()
                .reasoning_effort(),
            None
        );
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .model_id
                .as_deref(),
            Some("gpt-4.1")
        );

        let database = SessionStore::open(bootstrap.data_directory())
            .unwrap()
            .database_path();
        rusqlite::Connection::open(database)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_selection BEFORE UPDATE OF provider_id ON sessions
             BEGIN SELECT RAISE(ABORT, 'reject selection'); END;",
            )
            .unwrap();
        let before = session.lock().unwrap().clone();
        assert!(matches!(
            router.route("/effort default".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ));
        assert_eq!(*session.lock().unwrap(), before);

        let unavailable = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &TuiCredentialResolver::with_environment(BTreeMap::new()),
        )
        .unwrap();
        assert_eq!(unavailable.messages, before.messages);
        assert_eq!(
            unavailable.resume_error.as_deref(),
            Some("connect or choose provider")
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_model_overlay_selects_exact_future_id_with_unknown_metadata_and_default_effort() {
        let temporary = tui_session_directory("unverified-model");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let (progress, _) = std::sync::mpsc::channel();
        assert!(matches!(
            router.route("/effort xhigh".into()),
            TuiSubmissionOutcome::ContextChanged { .. }
        ));
        tui.apply_submission_outcome(router.route_request(
            TuiRouteRequest::OpenDialog("model".into()),
            progress.clone(),
        ));

        for character in "gpt-future-1".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        let overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(
            overlay.contains("Use gpt-future-1 (unverified metadata)"),
            "{overlay:?}"
        );
        let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("unverified model should dispatch a local action");
        };
        let outcome = router.route_request(TuiRouteRequest::DialogAction(action_id), progress);
        let TuiSubmissionOutcome::ContextChanged {
            message,
            presentation,
        } = &outcome
        else {
            panic!("unverified model should update session context");
        };
        assert_eq!(
            message,
            "Model: gpt-future-1 (unverified metadata). Reasoning effort reset to Default."
        );
        assert_eq!(
            presentation,
            &TuiPresentation::new("openai-api", "gpt-future-1", "new session")
        );
        tui.apply_submission_outcome(outcome);

        let selection = session.lock().unwrap().selection.clone().unwrap();
        assert_eq!(selection.model(), "gpt-future-1");
        assert!(!selection.metadata_known());
        assert_eq!(selection.reasoning_effort(), None);
        assert_eq!(
            selection.request_config(),
            &agens_core::RequestConfig::default()
        );
        assert!(session.lock().unwrap().messages.is_empty());
        assert!(tui.transcript().is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_effort_overlay_and_model_change_use_grounded_sets_and_atomic_reset() {
        let temporary = tui_session_directory("effort-capabilities");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let (progress, _) = std::sync::mpsc::channel();

        assert_eq!(
            router.route("/effort xhigh".into()),
            TuiSubmissionOutcome::ContextChanged {
                message: "Reasoning effort: xhigh.".into(),
                presentation: router.presentation().unwrap(),
            }
        );
        assert!(
            tui.apply_submission_outcome(
                router.route_request(TuiRouteRequest::OpenDialog("effort".into()), progress)
            )
            .is_none()
        );
        let overlay = render_tui_test_backend(&tui, 80, 24);
        assert!(overlay.contains("Default"), "{overlay:?}");
        assert!(overlay.contains("xhigh (current)"), "{overlay:?}");
        assert!(!overlay.contains("minimal"), "{overlay:?}");

        let reset = router.route("/model gpt-4.1".into());
        let TuiSubmissionOutcome::ContextChanged { message, .. } = reset else {
            panic!("model change should be local context information");
        };
        assert_eq!(
            message,
            "Model: gpt-4.1. Reasoning effort reset to Default because xhigh is unsupported."
        );
        let selection = session.lock().unwrap().selection.clone().unwrap();
        assert_eq!(selection.model(), "gpt-4.1");
        assert_eq!(selection.reasoning_effort(), None);
        assert_eq!(selection.request_config().reasoning_effort(), None);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_sessions_resume_and_agent_overlays_filter_navigate_cancel_and_apply_typed_outcomes() {
        let temporary = tui_session_directory("session-agent-overlays");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[
                (
                    "all",
                    "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
                ),
                (
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                ),
            ],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        tui.set_palette_entries(router.palette_entries().to_vec());
        let (progress, _) = std::sync::mpsc::channel();

        let empty = router.route_request(
            agens_tui::TuiRouteRequest::OpenDialog("sessions".into()),
            progress.clone(),
        );
        tui.apply_submission_outcome(empty);
        assert!(
            render_tui_test_backend(&tui, 80, 24)
                .contains("No resumable sessions in current project.")
        );
        tui.handle(Event::Key(Key::Escape));

        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let current = persist_tui_session(&mut store, &tui_project(&temporary), "current");
        let other = persist_tui_session(
            &mut store,
            &temporary.join("other").display().to_string(),
            "other",
        );
        drop(store);

        open_tui_palette_dialog(&mut tui, &router, "/se", "sessions", progress.clone());
        let sessions = render_tui_test_backend(&tui, 80, 24);
        assert!(sessions.contains(&format!("#{} current", current.id)));
        assert!(!sessions.contains(&format!("#{} other", other.id)));
        let original = session.lock().unwrap().clone();
        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        assert_eq!(*session.lock().unwrap(), original);

        open_tui_palette_dialog(&mut tui, &router, "/re", "sessions", progress.clone());
        dispatch_tui_dialog_selection(&mut tui, &router, progress.clone());
        assert_eq!(tui.view().session, format!("session #{}", current.id));
        assert!(tui.transcript().is_empty());
        assert!(
            tui.view()
                .status
                .is_some_and(|status| status.contains("Resumed session"))
        );

        open_tui_palette_dialog(&mut tui, &router, "/ag", "agent", progress.clone());
        let agents = render_tui_test_backend(&tui, 80, 24);
        assert!(agents.contains("primary (current)"), "{agents:?}");
        tui.handle(Event::Key(Key::Down));
        dispatch_tui_dialog_selection(&mut tui, &router, progress);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn session_overlay_uses_real_metadata_scope_search_sort_clock_and_atomic_failure() {
        let temporary = tui_session_directory("session-metadata-overlay");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = tui_project(&temporary);
        let other_project = temporary.join("other-root").display().to_string();
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let old = persist_tui_session_metadata(&mut store, &project, "Alpha", "primary", 9_900);
        let other =
            persist_tui_session_metadata(&mut store, &other_project, "Beta", "build", 9_950);
        let mut current =
            persist_tui_session_metadata(&mut store, &project, "Gamma", "reviewer", 9_950);
        current.provider_id = Some("openai-chatgpt".into());
        current.model_id = Some("gpt-5.5".into());
        current.reasoning_effort = Some(agens_core::ReasoningEffort::High);
        store.update_session_selection(&current).unwrap();
        drop(store);

        let session = Arc::new(Mutex::new(TuiSessionContext {
            identifier: Some(current.id),
            ..TuiSessionContext::fresh()
        }));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        tui.set_presentation("openai-api", "gpt-4.1", format!("session #{}", current.id));
        tui.replace_history(&tui_session_messages()).unwrap();
        let router = TuiRuntimeRouter::with_clock(
            bootstrap.clone(),
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            || 10_000,
        );
        tui.set_palette_entries(router.palette_entries().to_vec());
        let (progress, _) = std::sync::mpsc::channel();
        let original_context = session.lock().unwrap().clone();

        open_tui_palette_dialog(&mut tui, &router, "/se", "sessions", progress.clone());
        let project_rows = render_tui_test_backend(&tui, 100, 26);
        assert!(project_rows.contains("Resume session · Current project"));
        assert!(project_rows.contains(&format!("#{} Gamma", current.id)));
        assert!(project_rows.contains(&format!("#{} Alpha", old.id)));
        assert!(
            project_rows.contains("1 turn · 50s ago"),
            "{project_rows:?}"
        );
        assert!(!project_rows.contains("reviewer"), "{project_rows:?}");
        assert!(!project_rows.contains("Provider:"), "{project_rows:?}");
        assert!(!project_rows.contains("Model:"), "{project_rows:?}");
        assert!(!project_rows.contains("Effort:"), "{project_rows:?}");
        assert!(!project_rows.contains("Updated:"), "{project_rows:?}");
        tui.handle(Event::Key(Key::CtrlO));
        let project_details = render_tui_test_backend(&tui, 100, 26);
        assert!(
            project_details.contains("Provider: openai-chatgpt · Model: gpt-5.5"),
            "{project_details:?}"
        );
        assert!(
            project_details.contains("Effort: high · Updated: 9950 (50s ago)"),
            "{project_details:?}"
        );
        let old_details = format!(
            "{:?}",
            session_dialog_entry(
                &StoredSession {
                    metadata: old.clone(),
                    messages: Vec::new(),
                    latest_attempt: None,
                },
                None,
                false,
                10_000,
            )
        );
        assert!(old_details.contains("Provider: current runtime"));
        assert!(old_details.contains("Model: current runtime"));
        assert!(old_details.contains("Effort: current runtime"));
        assert!(project_rows.find("Gamma").unwrap() < project_rows.find("Alpha").unwrap());
        assert!(!project_rows.contains("Beta"));

        let global_action = tui.handle(Event::Key(Key::LineStart));
        dispatch_tui_session_page(&mut tui, &router, global_action, progress.clone());
        let global_rows = render_tui_test_backend(&tui, 100, 24);
        assert!(global_rows.contains("Resume session · All projects"));
        assert!(global_rows.contains(&format!("#{} Beta", other.id)));
        assert!(!global_rows.contains("root="), "{global_rows:?}");
        assert!(!global_rows.contains("other-root"), "{global_rows:?}");
        assert!(global_rows.find("Gamma").unwrap() < global_rows.find("Beta").unwrap());
        assert!(global_rows.find("Beta").unwrap() < global_rows.find("Alpha").unwrap());

        let mut search_action = Action::Render;
        for character in "reviewer".chars() {
            search_action = tui.handle(Event::Key(Key::Char(character)));
        }
        dispatch_tui_session_page(&mut tui, &router, search_action, progress.clone());
        let agent_search = render_tui_test_backend(&tui, 100, 24);
        assert!(agent_search.contains("Gamma"));
        assert!(!agent_search.contains("Alpha"));
        assert!(!agent_search.contains("Beta"));
        tui.handle(Event::Key(Key::Escape));
        tui.apply_submission_outcome(router.open_dialog("sessions").unwrap());
        let global_action = tui.handle(Event::Key(Key::LineStart));
        dispatch_tui_session_page(&mut tui, &router, global_action, progress.clone());
        let mut search_action = Action::Render;
        for character in "other-root".chars() {
            search_action = tui.handle(Event::Key(Key::Char(character)));
        }
        dispatch_tui_session_page(&mut tui, &router, search_action, progress.clone());
        let root_search = render_tui_test_backend(&tui, 100, 24);
        assert!(root_search.contains("Beta"));
        assert!(!root_search.contains("Gamma"));
        assert_eq!(*session.lock().unwrap(), original_context);

        tui.handle(Event::Key(Key::Escape));
        tui.apply_submission_outcome(router.open_dialog("sessions").unwrap());
        SessionStore::open(bootstrap.data_directory())
            .unwrap()
            .delete_session(current.id)
            .unwrap();
        let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("session Enter should dispatch through the router");
        };
        let outcome = router.route_request(TuiRouteRequest::DialogAction(action_id), progress);
        tui.apply_submission_outcome(outcome);
        assert_eq!(tui.view().session, format!("session #{}", current.id));
        assert_eq!(*session.lock().unwrap(), original_context);
        assert!(render_tui_test_backend(&tui, 100, 24).contains("saved session is unavailable"));
        tui.handle(Event::Key(Key::Escape));
        assert!(render_tui_test_backend(&tui, 100, 24).contains("previous request"));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_resume_overlay_restores_appends_reopens_and_resets_complete_history() {
        let temporary = tui_session_directory("resume-production-path");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let first = persist_tui_session(&mut store, &tui_project(&temporary), "history");
        let restored =
            append_tui_session_turn(&mut store, &first, "second request", "second answer");
        let restored_messages = store.load_session_for_resume(restored.id).unwrap().messages;
        drop(store);

        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        tui.set_palette_entries(router.palette_entries().to_vec());
        let (progress, _) = std::sync::mpsc::channel();

        open_tui_palette_dialog(&mut tui, &router, "/re", "sessions", progress.clone());
        dispatch_tui_dialog_selection(&mut tui, &router, progress.clone());
        let restored_render = render_tui_test_backend(&tui, 120, 50);
        for expected in [
            "previous request",
            "Thought",
            "previous answer",
            "persisted reminder",
            "second request",
            "second answer",
        ] {
            assert!(restored_render.contains(expected), "{restored_render:?}");
            assert_eq!(
                restored_render.matches(expected).count(),
                1,
                "{restored_render:?}"
            );
        }
        // Tool name appears on header and result footer; assert the card chrome once.
        assert!(restored_render.contains("read {}"), "{restored_render:?}");
        assert_eq!(
            restored_render.matches("read {}").count(),
            1,
            "{restored_render:?}"
        );
        assert!(
            restored_render.contains("output collapsed"),
            "{restored_render:?}"
        );
        assert!(
            !restored_render.contains("previous reasoning"),
            "{restored_render:?}"
        );
        assert!(
            !restored_render.contains("previous result"),
            "{restored_render:?}"
        );
        assert!(
            !restored_render.contains("resume-call"),
            "{restored_render:?}"
        );

        tui.handle(Event::Key(Key::PageUp));
        let restored_anchor = (
            tui.view().following_bottom,
            tui.view().scroll_offset,
            tui.view().focus,
        );

        // Ctrl+O is thinking-first: expand collapsed reasoning before tool bodies.
        tui.handle(Event::Key(Key::CtrlO));
        assert_eq!(
            (
                tui.view().following_bottom,
                tui.view().scroll_offset,
                tui.view().focus,
            ),
            restored_anchor
        );
        let thinking_expanded = render_tui_test_backend(&tui, 120, 50);
        assert!(
            thinking_expanded.contains("previous reasoning"),
            "{thinking_expanded:?}"
        );
        assert!(
            !thinking_expanded.contains("previous result"),
            "{thinking_expanded:?}"
        );

        tui.handle(Event::Key(Key::CtrlO));
        assert_eq!(
            (
                tui.view().following_bottom,
                tui.view().scroll_offset,
                tui.view().focus,
            ),
            restored_anchor
        );
        let tools_expanded = render_tui_test_backend(&tui, 120, 50);
        assert!(
            tools_expanded.contains("previous result"),
            "{tools_expanded:?}"
        );

        tui.handle(Event::Key(Key::CtrlO));
        assert_eq!(
            (
                tui.view().following_bottom,
                tui.view().scroll_offset,
                tui.view().focus,
            ),
            restored_anchor
        );
        // Completes the Collapsed -> Truncated -> Expanded -> Collapsed
        // cycle (S1 renders Truncated and Expanded identically).
        tui.handle(Event::Key(Key::CtrlO));
        assert_eq!(
            (
                tui.view().following_bottom,
                tui.view().scroll_offset,
                tui.view().focus,
            ),
            restored_anchor
        );
        assert_eq!(
            tui.view().tool_display_modes.get("resume-call"),
            Some(&agens_tui::DisplayMode::Collapsed)
        );
        tui.handle(Event::Key(Key::End));

        assert_eq!(tui.view().session, format!("session #{}", restored.id));
        assert!(tui.transcript().is_empty());
        assert!(!restored_render.contains("INFO      Resumed session"));

        let before_failure = session.lock().unwrap().clone();
        let input = enter_tui_input(&mut tui, "/resume 999");
        tui.apply_submission_outcome(router.route(input));
        let failed = render_tui_test_backend(&tui, 120, 50);
        assert!(
            failed.contains("saved session is unavailable"),
            "{failed:?}"
        );
        assert!(failed.contains("Action:"), "{failed:?}");
        assert_eq!(tui.view().session, format!("session #{}", restored.id));
        assert_eq!(*session.lock().unwrap(), before_failure);
        assert!(tui.transcript().is_empty());

        tui.handle(Event::Key(Key::Escape));
        let prompt = enter_tui_input(&mut tui, "third request");
        let prompt = tui.apply_submission_outcome(router.route(prompt)).unwrap();
        let result = run_tui_prompt_with(
            &bootstrap,
            &prompt,
            &router.session,
            Some(Arc::clone(&router.skills)),
            |request| {
                assert_eq!(request.history, restored_messages);
                let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
                let metadata = append_tui_session_turn(
                    &mut store,
                    request.session.as_ref().unwrap(),
                    "third request",
                    "third answer",
                );
                let messages = store.load_session_for_resume(metadata.id).unwrap().messages;
                Ok(HeadlessChatCompletion {
                    text: "third answer".into(),
                    metadata,
                    messages,
                })
            },
        );
        tui.finish_provider_turn(tui_provider_outcome(result));
        let reopened = SessionStore::open(bootstrap.data_directory())
            .unwrap()
            .load_session_for_resume(restored.id)
            .unwrap();
        assert_eq!(session.lock().unwrap().messages, reopened.messages);

        open_tui_palette_dialog(&mut tui, &router, "/re", "sessions", progress);
        dispatch_tui_dialog_selection(&mut tui, &router, std::sync::mpsc::channel().0);
        let reopened_render = render_tui_test_backend(&tui, 120, 60);
        for expected in [
            "previous request",
            "second request",
            "third request",
            "third answer",
        ] {
            assert_eq!(
                reopened_render.matches(expected).count(),
                1,
                "{reopened_render:?}"
            );
        }

        for _ in 0..20 {
            tui.handle(Event::Key(Key::PageUp));
        }
        assert!(render_tui_test_backend(&tui, 60, 14).contains("previous request"));

        let input = enter_tui_input(&mut tui, "/new");
        tui.apply_submission_outcome(router.route(input));
        let reset = render_tui_test_backend(&tui, 120, 24);
        assert_eq!(tui.view().session, "new session");
        assert!(!reset.contains("previous request"), "{reset:?}");
        assert!(!reset.contains("INFO"), "{reset:?}");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_connect_and_disconnect_overlays_select_flows_and_cancel_without_credentials_mutation() {
        let temporary = tui_session_directory("auth-overlays");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        let initial_credentials = r#"{"openai-api":{"api_key":"preserved"}}"#;
        std::fs::write(&credentials_path, initial_credentials).unwrap();
        let flows = Arc::new(Mutex::new(Vec::new()));
        let coordinator = ChatGptAuthCoordinator::with_authenticator({
            let flows = Arc::clone(&flows);
            move |flow, _, publish| {
                flows.lock().unwrap().push(flow);
                publish(ChatGptAuthProgress::BrowserUrl("auth-url".into()));
                Ok(test_chatgpt_credentials("new-access"))
            }
        });
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::with_auth_coordinator(
            tui_session_bootstrap(&temporary, &[]),
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            coordinator,
        );
        tui.set_palette_entries(router.palette_entries().to_vec());
        let (progress, _) = std::sync::mpsc::channel();

        for (prefix, down, flow) in [
            ("/co", false, ChatGptAuthFlow::Browser),
            ("/co", true, ChatGptAuthFlow::Device),
        ] {
            open_tui_palette_dialog(&mut tui, &router, prefix, "connect", progress.clone());
            if down {
                tui.handle(Event::Key(Key::Down));
            }
            dispatch_tui_dialog_selection(&mut tui, &router, progress.clone());
            assert_eq!(flows.lock().unwrap().last(), Some(&flow));
        }

        open_tui_palette_dialog(&mut tui, &router, "/di", "disconnect", progress.clone());
        let connected = std::fs::read_to_string(&credentials_path).unwrap();
        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        let after_cancel = std::fs::read_to_string(&credentials_path).unwrap();
        assert_eq!(after_cancel, connected);
        open_tui_palette_dialog(&mut tui, &router, "/di", "disconnect", progress);
        dispatch_tui_dialog_selection(&mut tui, &router, std::sync::mpsc::channel().0);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_router_connect_device_disconnect_uses_coordinator_without_provider_history() {
        let temporary = tui_session_directory("auth-router");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"openai-api":{"api_key":"preserved"},"other":{"value":"kept"}}"#,
        )
        .unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::Auto;
        bootstrap.provider_type = Some("openai-api".into());
        bootstrap.openai_api_key = Some("preserved".into());
        let flows = Arc::new(Mutex::new(Vec::new()));
        let coordinator = ChatGptAuthCoordinator::with_authenticator({
            let flows = Arc::clone(&flows);
            move |flow, _, publish| {
                flows.lock().unwrap().push(flow);
                publish(ChatGptAuthProgress::BrowserUrl("auth-url".into()));
                Ok(test_chatgpt_credentials("new-access"))
            }
        });
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::with_auth_coordinator(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            coordinator,
        );
        let (progress_tx, progress_rx) = std::sync::mpsc::channel();

        assert!(matches!(
            router.route_with_progress("/connect --device-auth".into(), progress_tx),
            TuiSubmissionOutcome::LocalInfo(_)
        ));
        assert_eq!(progress_rx.try_iter().count(), 1);
        assert_eq!(*flows.lock().unwrap(), vec![ChatGptAuthFlow::Device]);
        let context = session.lock().unwrap();
        assert_eq!(context.provider, Some(TuiProvider::OpenAiChatGpt));
        assert!(context.messages.is_empty());
        drop(context);
        let configured = router.bootstrap().unwrap();
        assert_eq!(configured.provider_type(), Some("openai-api"));
        let connected = std::fs::read_to_string(&credentials_path).unwrap();
        assert!(connected.contains("new-access"));

        assert!(router.disconnect().is_ok());
        assert_eq!(
            session.lock().unwrap().provider,
            Some(TuiProvider::OpenAiApi)
        );
        let stored = std::fs::read_to_string(&credentials_path).unwrap();
        assert!(stored.contains("preserved"));
        assert!(stored.contains("kept"));
        assert!(!stored.contains("new-access"));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_preserves_intervening_unrelated_provider_write() {
        let temporary = tui_session_directory("refresh-rollback");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        let before = br#"{"openai-api":{"api_key":"preserved"},"openai-chatgpt":{"access_token":"old-access","refresh_token":"old-refresh","account_id":"old-account","expires_at":"2099-01-01T00:00:00Z"}}"#;
        std::fs::write(&credentials_path, before).unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::Auto;
        bootstrap.provider_type = Some("openai-api".into());
        bootstrap.openai_api_key = Some("preserved".into());
        let session = Arc::new(Mutex::new(TuiSessionContext {
            running: true,
            ..TuiSessionContext::fresh()
        }));
        let original_runtime = session.lock().unwrap().clone();
        let router = TuiRuntimeRouter::with_auth_coordinator(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            ChatGptAuthCoordinator::with_authenticator(|_, _, _| {
                Ok(test_chatgpt_credentials("new-access"))
            }),
        )
        .with_credential_restorer(|path, snapshot| {
            upsert_provider_entry(path, "other-provider", serde_json::json!({"key": "kept"}))
                .map_err(|_| CliError::storage("unrelated provider write failed"))?;
            restore_chatgpt_credentials(path, snapshot)
        });

        assert!(
            router
                .connect(ChatGptAuthFlow::Browser, std::sync::mpsc::channel().0)
                .is_err()
        );
        let mut expected = serde_json::from_slice::<serde_json::Value>(before).unwrap();
        expected
            .as_object_mut()
            .unwrap()
            .insert("other-provider".into(), serde_json::json!({"key": "kept"}));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&credentials_path).unwrap())
                .unwrap(),
            expected
        );
        assert_eq!(*session.lock().unwrap(), original_runtime);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_disconnects_explicit_chatgpt_fail_closed() {
        let temporary = tui_session_directory("explicit-disconnect");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"openai-api":{"api_key":"preserved"},"openai-chatgpt":{"access_token":"old-access","refresh_token":"old-refresh","account_id":"old-account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::ExplicitChatGpt;
        bootstrap.provider_type = Some("openai-chatgpt".into());
        let session = Arc::new(Mutex::new(TuiSessionContext {
            provider: Some(TuiProvider::OpenAiChatGpt),
            ..TuiSessionContext::fresh()
        }));
        ensure_active_tui_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );

        assert!(router.disconnect().is_ok());
        assert_eq!(session.lock().unwrap().provider, None);
        assert!(session.lock().unwrap().chatgpt_unavailable);
        assert!(session.lock().unwrap().active_agent.is_none());
        let error = match router.turn_bootstrap() {
            Ok(_) => panic!("disconnected ChatGPT runtime must be unavailable"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "auth: ChatGPT credentials are unavailable; run /connect"
        );
        assert!(
            !std::fs::read_to_string(&credentials_path)
                .unwrap()
                .contains("old-access")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_fails_closed_when_credential_restore_fails() {
        let temporary = tui_session_directory("restore-failure");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"openai-api":{"api_key":"preserved"}}"#,
        )
        .unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::Auto;
        bootstrap.provider_type = Some("openai-api".into());
        bootstrap.openai_api_key = Some("preserved".into());
        let session = Arc::new(Mutex::new(TuiSessionContext {
            running: true,
            ..TuiSessionContext::fresh()
        }));
        let router = TuiRuntimeRouter::with_auth_coordinator(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            ChatGptAuthCoordinator::with_authenticator(|_, _, _| {
                Ok(test_chatgpt_credentials("new-access"))
            }),
        )
        .with_credential_restorer(|_, _| Err(CliError::storage("injected restore failure")));

        let outcome = auth_route_outcome(
            router.connect(ChatGptAuthFlow::Browser, std::sync::mpsc::channel().0),
        );
        assert!(matches!(
            outcome,
            TuiSubmissionOutcome::LocalActionableError { message, .. }
                if message == "store: ChatGPT credential recovery failed"
        ));
        assert!(session.lock().unwrap().chatgpt_unavailable);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_preserves_runtime_on_credential_write_failures() {
        let temporary = tui_session_directory("credential-write-failures");
        let config_home = temporary.join("config");
        std::fs::create_dir_all(&config_home).unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.paths.credentials = config_home.clone();
        let session = Arc::new(Mutex::new(TuiSessionContext {
            provider: Some(TuiProvider::OpenAiApi),
            ..TuiSessionContext::fresh()
        }));
        let original_runtime = session.lock().unwrap().clone();
        let router = TuiRuntimeRouter::with_auth_coordinator(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
            ChatGptAuthCoordinator::with_authenticator(|_, _, _| {
                Ok(test_chatgpt_credentials("new-access"))
            }),
        );

        for outcome in [
            auth_route_outcome(
                router.connect(ChatGptAuthFlow::Browser, std::sync::mpsc::channel().0),
            ),
            auth_route_outcome(router.disconnect()),
        ] {
            assert!(matches!(
                outcome,
                TuiSubmissionOutcome::LocalActionableError { message, .. }
                    if message == "ChatGPT credentials could not be saved"
            ));
            assert_eq!(*session.lock().unwrap(), original_runtime);
        }

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn runtime_chatgpt_refresh_atomicity_leaves_auto_unavailable_after_disconnect_rebuild_failure()
    {
        let temporary = tui_session_directory("auto-disconnect-failure");
        let config_home = temporary.join("config");
        let credentials_path = config_home.join("auth.json");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"openai-chatgpt":{"access_token":"old-access","refresh_token":"old-refresh","account_id":"old-account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.provider_source = ProviderSource::Auto;
        bootstrap.provider_type = Some("openai-chatgpt".into());
        let session = Arc::new(Mutex::new(TuiSessionContext {
            provider: Some(TuiProvider::OpenAiChatGpt),
            ..TuiSessionContext::fresh()
        }));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );

        assert!(router.disconnect().is_err());
        assert!(session.lock().unwrap().chatgpt_unavailable);
        assert!(
            !std::fs::read_to_string(&credentials_path)
                .unwrap()
                .contains("old-access")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    fn test_chatgpt_credentials(
        access_token: &str,
    ) -> agens_providers::chatgpt_login::ChatGptCredentials {
        agens_providers::chatgpt_login::ChatGptCredentials {
            access_token: access_token.into(),
            refresh_token: "refresh".into(),
            account_id: "account".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn tui_session_busy_agent_command_leaves_context_and_store_unchanged() {
        let temporary = tui_session_directory("busy-agent-command");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "all",
                "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
            )],
        );
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "current");
        let saved_sessions = store.list_sessions().unwrap();
        drop(store);
        let session = Arc::new(Mutex::new(TuiSessionContext {
            identifier: Some(metadata.id),
            metadata: Some(metadata),
            messages: tui_session_messages(),
            selected_subagent: Some("reviewer".into()),
            running: true,
            ..TuiSessionContext::fresh()
        }));
        let original = session.lock().unwrap().clone();

        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/agent all",
                &HeadlessTurnCancellation::new(),
                &session,
                None,
            )
            .unwrap_err()
            .to_string(),
            "runtime: headless turn entered an invalid state"
        );
        assert_eq!(*session.lock().unwrap(), original);
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .list_sessions()
                .unwrap(),
            saved_sessions
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_file_candidates_and_expansion_use_confined_reads() {
        let temporary = tui_session_directory("files");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = temporary.join("project");
        std::fs::write(project.join("zeta.txt"), "zeta").unwrap();
        std::fs::write(project.join("alpha.txt"), "alpha").unwrap();
        let oversized = vec![b'x'; 1024 * 1024 + 1];
        std::fs::write(project.join("large.txt"), oversized).unwrap();

        assert_eq!(
            tui_file_candidates(&bootstrap).unwrap(),
            vec!["alpha.txt".to_owned(), "zeta.txt".to_owned()]
        );
        assert_eq!(
            expand_tui_file_reference(&bootstrap, "review @alpha.txt please").unwrap(),
            "review <file path=\"alpha.txt\">\nalpha\n</file> please"
        );
        assert_eq!(
            expand_tui_file_reference(&bootstrap, "@../outside.txt")
                .unwrap_err()
                .to_string(),
            "file: path: traversal is not allowed"
        );
        assert_eq!(
            expand_tui_file_reference(&bootstrap, "@large.txt")
                .unwrap_err()
                .to_string(),
            "file: read: file exceeds 1048576 byte limit"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn the_file_picker_inserts_a_relative_path_the_confined_expansion_resolves() {
        let temporary = tui_session_directory("picker");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = temporary.join("project");
        std::fs::create_dir_all(project.join("nested/deep")).unwrap();
        std::fs::write(project.join("nested/deep/alpha.txt"), "alpha").unwrap();
        std::fs::write(project.join("zeta.txt"), "zeta").unwrap();

        let candidates = tui_picker_file_candidates(&bootstrap).unwrap();
        assert_eq!(
            candidates,
            vec!["nested/deep/alpha.txt".to_owned(), "zeta.txt".to_owned()]
        );

        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        tui.set_file_candidates(candidates);
        for character in "review @alpha".chars() {
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
        }
        assert_eq!(
            tui.view().file_picker.unwrap().matches(),
            vec!["nested/deep/alpha.txt"]
        );

        tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter));
        let prompt = tui.input().to_owned();

        assert_eq!(prompt, "review @nested/deep/alpha.txt");
        assert_eq!(
            expand_tui_file_reference(&bootstrap, &prompt).unwrap(),
            "review <file path=\"nested/deep/alpha.txt\">\nalpha\n</file>"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn picker_candidates_stay_capped_and_confined_to_the_project_root() {
        let temporary = tui_session_directory("picker-cap");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = temporary.join("project");
        std::fs::write(temporary.join("outside.txt"), "outside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(temporary.join("outside.txt"), project.join("escape.txt"))
            .unwrap();
        for index in 0..150 {
            std::fs::write(project.join(format!("file-{index:03}.txt")), "body").unwrap();
        }

        let capped = tui_file_candidates_with_limit(&bootstrap, 64).unwrap();

        assert_eq!(capped.len(), 64);
        assert_eq!(capped.first().map(String::as_str), Some("file-000.txt"));
        assert!(
            capped
                .iter()
                .all(|path| path.starts_with("file-") && !path.contains("..")),
            "{capped:?}"
        );
        assert_eq!(tui_picker_file_candidates(&bootstrap).unwrap().len(), 150);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn tui_native_select_preserves_running_turn_outcomes_and_terminal_cleanup() {
        use std::os::unix::fs::symlink;

        let temporary = tui_session_directory("native-select");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = temporary.join("project");
        let outside = temporary.join("outside.txt");
        std::fs::write(project.join("approved.txt"), "approved").unwrap();
        std::fs::create_dir(project.join("directory")).unwrap();
        std::fs::write(project.join("large.txt"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        std::fs::write(&outside, "outside").unwrap();
        symlink(&outside, project.join("escape.txt")).unwrap();
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        let mut control = TuiTerminalControl::default();
        let mut guard = agens_tui::TerminalModeGuard::enter(&mut control).unwrap();
        let transcript_count = open_running_tui_select(&mut tui, &router);
        assert!(render_tui_test_backend(&tui, 80, 24).contains("Select project file"));
        assert_eq!(
            tui.handle(Event::Key(Key::Escape)),
            Action::SafeDialogAction("select:cancel".into())
        );
        let cancelled = router.route_request(
            TuiRouteRequest::DialogAction("select:cancel".into()),
            std::sync::mpsc::channel().0,
        );
        assert_eq!(cancelled, TuiSubmissionOutcome::SelectionCancelled);
        assert!(tui.apply_submission_outcome(cancelled).is_none());
        assert!(tui.view().dialog.is_none());
        assert!(tui.view().running);
        assert_eq!(tui.transcript().len(), transcript_count);
        assert!(
            tui.apply_submission_outcome(router.route_request(
                TuiRouteRequest::DialogAction("select:cancel".into()),
                std::sync::mpsc::channel().0,
            ))
            .is_none()
        );
        assert_eq!(tui.transcript().len(), transcript_count);
        open_running_tui_select(&mut tui, &router);
        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
        assert!(tui.view().quit_armed);
        assert!(tui.view().dialog.is_some());
        assert_eq!(
            tui.handle(Event::Key(Key::Escape)),
            Action::SafeDialogAction("select:cancel".into())
        );
        assert_eq!(
            router.route_request(
                TuiRouteRequest::DialogAction("select:cancel".into()),
                std::sync::mpsc::channel().0,
            ),
            TuiSubmissionOutcome::SelectionCancelled
        );
        guard.restore(&mut control).unwrap();
        assert_tui_terminal_restored(&control);

        let mut control = TuiTerminalControl::default();
        let mut guard = agens_tui::TerminalModeGuard::enter(&mut control).unwrap();
        let transcript_count = open_running_tui_select(&mut tui, &router);
        let Action::SafeDialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("selection Enter should use the safe local action path");
        };
        let selected = router.route_request(
            TuiRouteRequest::DialogAction(action_id),
            std::sync::mpsc::channel().0,
        );
        assert_eq!(
            selected,
            TuiSubmissionOutcome::SelectionInfo("Selected file: approved.txt".into())
        );
        assert!(tui.apply_submission_outcome(selected).is_none());
        assert!(tui.view().running);
        assert_eq!(tui.transcript().len(), transcript_count);
        guard.restore(&mut control).unwrap();
        assert_tui_terminal_restored(&control);

        let mut control = TuiTerminalControl::default();
        let mut guard = agens_tui::TerminalModeGuard::enter(&mut control).unwrap();
        let transcript_count = open_running_tui_select(&mut tui, &router);
        let rejected = router.route_request(
            TuiRouteRequest::DialogAction("select:escape.txt".into()),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(
            rejected,
            TuiSubmissionOutcome::SelectionError { .. }
        ));
        assert!(tui.apply_submission_outcome(rejected).is_none());
        assert!(tui.view().running);
        assert_eq!(tui.transcript().len(), transcript_count);
        guard.restore(&mut control).unwrap();
        assert_tui_terminal_restored(&control);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[derive(Default)]
    struct TuiTerminalControl {
        operations: Vec<agens_tui::TerminalOperation>,
    }

    impl agens_tui::TerminalControl for TuiTerminalControl {
        fn apply(&mut self, operation: agens_tui::TerminalOperation) -> std::io::Result<()> {
            self.operations.push(operation);
            Ok(())
        }
    }

    fn assert_tui_terminal_restored(control: &TuiTerminalControl) {
        use agens_tui::TerminalOperation::*;

        assert_eq!(
            control.operations,
            vec![
                EnableRaw,
                EnterAlternate,
                HideCursor,
                EnableMouse,
                EnableKeyboardEnhancement,
                EnablePaste,
                DisablePaste,
                DisableKeyboardEnhancement,
                DisableMouse,
                ShowCursor,
                LeaveAlternate,
                DisableRaw,
            ]
        );
    }

    fn open_running_tui_select(
        tui: &mut Tui<ProductionTuiEngine>,
        router: &TuiRuntimeRouter,
    ) -> usize {
        tui.begin_submission("running");
        let transcript_count = tui.transcript().len();
        for character in "/select".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }

        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::OpenDialog("select".into())
        );
        let outcome = router.route_request(
            TuiRouteRequest::OpenDialog("select".into()),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(outcome, TuiSubmissionOutcome::SafeDialog(_)));
        assert!(tui.apply_submission_outcome(outcome).is_none());
        assert!(tui.view().running);
        assert_eq!(tui.transcript().len(), transcript_count);

        transcript_count
    }

    fn enter_tui_input(tui: &mut Tui<ProductionTuiEngine>, input: &str) -> String {
        for character in input.chars() {
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
        }
        let agens_tui::Action::Submit(input) =
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter))
        else {
            panic!("Enter should submit through the production TUI path");
        };
        input
    }

    fn open_tui_palette_dialog(
        tui: &mut Tui<ProductionTuiEngine>,
        router: &TuiRuntimeRouter,
        prefix: &str,
        expected_route: &str,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) {
        for character in prefix.chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        let Action::OpenDialog(route_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("palette Enter should open a dialog");
        };
        assert_eq!(route_id, expected_route);
        let outcome = router.route_request(TuiRouteRequest::OpenDialog(route_id), progress);
        assert!(tui.apply_submission_outcome(outcome).is_none());
    }

    fn dispatch_tui_dialog_selection(
        tui: &mut Tui<ProductionTuiEngine>,
        router: &TuiRuntimeRouter,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) {
        let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("dialog Enter should dispatch an action");
        };
        let outcome = router.route_request(TuiRouteRequest::DialogAction(action_id), progress);
        assert!(tui.apply_submission_outcome(outcome).is_none());
    }

    fn dispatch_tui_session_page(
        tui: &mut Tui<ProductionTuiEngine>,
        router: &TuiRuntimeRouter,
        action: Action,
        progress: std::sync::mpsc::Sender<TuiRouteProgress>,
    ) {
        let Action::LoadSessionPage(request) = action else {
            panic!("session dialog action should request a page");
        };
        let outcome = router.route_request(TuiRouteRequest::SessionPage(request), progress);
        assert!(tui.apply_submission_outcome(outcome).is_none());
    }

    fn write_tui_command(root: &Path, name: &str, description: &str, template: &str) {
        std::fs::write(
            root.join(format!("{name}.md")),
            format!("---\ndescription: {description}\n---\n{template}\n"),
        )
        .unwrap();
    }

    fn write_tui_skill(root: &Path, name: &str, description: &str, body: &str) {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
        )
        .unwrap();
    }

    fn submit_tui_command(
        tui: &mut Tui<ProductionTuiEngine>,
        router: &TuiRuntimeRouter,
        bootstrap: &Bootstrap,
        input: &str,
        captured: &Arc<Mutex<Vec<HeadlessChatRequest>>>,
    ) {
        let input = enter_tui_input(tui, input);
        let Some(prompt) = tui.apply_submission_outcome(router.route(input)) else {
            return;
        };
        let result = run_tui_prompt_with(
            bootstrap,
            &prompt,
            &router.session,
            Some(Arc::clone(&router.skills)),
            {
                let captured = Arc::clone(captured);
                move |request| {
                    captured.lock().unwrap().push(request);
                    Ok(HeadlessChatCompletion {
                        text: "captured".into(),
                        metadata: SessionMetadata {
                            id: 1,
                            project: "project".into(),
                            title: "captured".into(),
                            active_agent: "build".into(),
                            provider_id: None,
                            model_id: None,
                            reasoning_effort: None,
                            created_at: 1,
                            updated_at: 1,
                            completed_turn_count: 1,
                            resumable: true,
                        },
                        messages: Vec::new(),
                    })
                }
            },
        );
        tui.finish_provider_turn(tui_provider_outcome(result));
    }

    fn tui_project(temporary: &Path) -> String {
        temporary.join("project").display().to_string()
    }

    #[test]
    fn the_removed_tool_output_key_is_no_longer_accepted() {
        let document = parse_toml_document("[ui]\ntruncate_tool_output = true\n").unwrap();

        assert!(validate_toml_document(&document).is_err());
    }

    #[test]
    fn a_fresh_session_starts_from_the_configured_default_agent() {
        let configured = bootstrap_from_configuration(
            "config-default-agent",
            Some("[agent]\ndefault_agent = \"reviewer\"\n"),
            None,
        );
        let unconfigured = bootstrap_from_configuration("config-no-default-agent", None, None);
        let fresh = TuiSessionContext::fresh();

        assert_eq!(initial_active_agent_name(&fresh, &configured), "reviewer");
        assert_eq!(initial_active_agent_name(&fresh, &unconfigured), "primary");
    }

    #[test]
    fn a_resumed_session_keeps_its_persisted_agent_over_the_configured_default() {
        let configured = bootstrap_from_configuration(
            "config-default-agent-resumed",
            Some("[agent]\ndefault_agent = \"reviewer\"\n"),
            None,
        );
        let metadata = SessionMetadata {
            id: 7,
            project: "project".into(),
            title: "title".into(),
            active_agent: "planner".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: true,
        };
        let resumed = TuiSessionContext::restored(7, metadata, Vec::new(), Vec::new());

        assert_eq!(initial_active_agent_name(&resumed, &configured), "planner");
    }

    fn tui_session_messages() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("previous request".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::Reasoning("previous reasoning".into()),
                    MessagePart::ToolCall {
                        id: "resume-call".into(),
                        name: "read".into(),
                        input: "{}".into(),
                    },
                    MessagePart::Text("previous answer".into()),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "resume-call".into(),
                    content: "previous result".into(),
                    is_error: false,
                }],
            },
        ]
    }

    fn append_tui_session_turn(
        store: &mut SessionStore,
        metadata: &SessionMetadata,
        user: &str,
        answer: &str,
    ) -> SessionMetadata {
        let messages = vec![
            Message {
                role: Role::System,
                parts: vec![MessagePart::Text("persisted reminder".into())],
            },
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text(user.into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text(answer.into())],
            },
        ];
        let turn = CompletedSessionTurn::new(
            messages
                .into_iter()
                .map(SessionMessage::try_from)
                .collect::<Result<_, _>>()
                .unwrap(),
        )
        .unwrap();
        store
            .persist_completed_session_turn(metadata, &turn)
            .unwrap()
    }

    fn persist_tui_session(
        store: &mut SessionStore,
        project: &str,
        title: &str,
    ) -> SessionMetadata {
        let turn = CompletedSessionTurn::new(
            tui_session_messages()
                .into_iter()
                .map(SessionMessage::try_from)
                .collect::<Result<_, _>>()
                .unwrap(),
        )
        .unwrap();
        store
            .persist_completed_session_turn(
                &SessionMetadata {
                    id: 0,
                    project: project.into(),
                    title: title.into(),
                    active_agent: "primary".into(),
                    provider_id: None,
                    model_id: None,
                    reasoning_effort: None,
                    created_at: 1,
                    updated_at: 1,
                    completed_turn_count: 0,
                    resumable: false,
                },
                &turn,
            )
            .unwrap()
    }

    fn persist_tui_session_metadata(
        store: &mut SessionStore,
        project: &str,
        title: &str,
        active_agent: &str,
        updated_at: i64,
    ) -> SessionMetadata {
        let mut metadata = persist_tui_session(store, project, title);
        metadata.active_agent = active_agent.into();
        metadata.updated_at = updated_at;
        store.update_session(&metadata).unwrap();
        metadata
    }

    #[test]
    fn production_resumed_headless_turn_replays_typed_history_and_appends_to_the_same_session() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-resumed-headless-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos()
        ));
        let project_root = temporary.join("project");
        let config_home = temporary.join("config");
        let data_directory = temporary.join("data");
        std::fs::create_dir_all(project_root.join(".git"))
            .expect("project marker should be created");
        std::fs::create_dir_all(config_home.join("agents"))
            .expect("agent directory should be created");
        std::fs::write(
            config_home.join("agents/reviewer.md"),
            "---\nname: reviewer\ndescription: reviewer\nmode: primary\nmodel: gpt-4o\npermissions: []\n---\nYou are reviewer.\n",
        )
        .expect("reviewer agent should be written");

        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("mock provider should bind");
        let address = listener
            .local_addr()
            .expect("mock provider should have an address");
        let worker = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};

            let (mut stream, _) = listener
                .accept()
                .expect("mock provider should accept the resumed request");
            let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .expect("request line should be readable");
            assert_eq!(request_line, "POST /responses HTTP/1.1\r\n");

            let mut content_length = None;
            loop {
                let mut header = String::new();
                reader
                    .read_line(&mut header)
                    .expect("request header should be readable");
                if header == "\r\n" {
                    break;
                }
                if let Some(value) = header.strip_prefix("content-length: ") {
                    content_length = Some(
                        value
                            .trim()
                            .parse::<usize>()
                            .expect("content length should be numeric"),
                    );
                }
            }

            let mut body =
                vec![0_u8; content_length.expect("request should include content length")];
            std::io::Read::read_exact(&mut reader, &mut body)
                .expect("request body should be readable");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"second answer\"}\n\ndata: {\"type\":\"response.completed\"}\n\n")
                .expect("mock response should be written");

            serde_json::from_slice::<serde_json::Value>(&body)
                .expect("resumed provider request should be valid JSON")
        });

        let dependencies = CliDependencies::for_test(
            project_root.clone(),
            Some(temporary.join("home")),
            BTreeMap::from([
                (
                    "AGENS_CONFIG_HOME".to_owned(),
                    config_home.display().to_string(),
                ),
                ("OPENAI_API_KEY".to_owned(), "test-key".to_owned()),
            ]),
            BTreeMap::from([(
                config_home.join("config.toml"),
                format!(
                    "[provider]\ntype = \"openai-api\"\nmodel = \"gpt-4.1\"\nbase_url = \"http://{address}\"\n\n[options]\ndata_dir = \"{}\"\n",
                    data_directory.display()
                ),
            )]),
        );
        let bootstrap = bootstrap(&dependencies).expect("production bootstrap should be valid");
        let initial_messages = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("first input".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::Reasoning("first reasoning".into()),
                    MessagePart::ToolCall {
                        id: "call-history".into(),
                        name: "native::read".into(),
                        input: r#"{"path":"notes.md"}"#.into(),
                    },
                    MessagePart::Text("calling the tool".into()),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "call-history".into(),
                    content: "file contents".into(),
                    is_error: false,
                }],
            },
        ];
        let initial_turn = CompletedSessionTurn::new(
            initial_messages
                .clone()
                .into_iter()
                .map(SessionMessage::try_from)
                .collect::<Result<_, _>>()
                .expect("typed history should be a valid completed turn"),
        )
        .expect("typed history should be a valid completed turn");
        let metadata = SessionMetadata {
            id: 0,
            project: project_root.display().to_string(),
            title: "first input".into(),
            active_agent: "reviewer".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 10,
            updated_at: 10,
            completed_turn_count: 0,
            resumable: false,
        };
        SessionStore::open(&data_directory)
            .expect("session store should open")
            .persist_completed_session_turn(&metadata, &initial_turn)
            .expect("normalized session should persist");

        let mut request = resume_tui_session(
            &bootstrap,
            1,
            &SkillCatalog::default(),
            &TuiCredentialResolver::production(),
        )
        .expect("normalized session should resume")
        .apply_to(HeadlessChatRequest {
            prompt: "second input".into(),
            history: Vec::new(),
            model: None,
            system_prompt: None,
            max_iterations: None,
            mode: PermissionMode::Edit,
            dangerously_allow_all: false,
            dangerous_mode: false,
            request_config: agens_core::RequestConfig::default(),
            session_reasoning_effort: None,
            session: None,
            active_agent: None,
            effective_capabilities: None,
            pending_system_reminder: None,
            skills: None,
        });
        request.pending_system_reminder =
            Some("Agent capabilities expanded: primary -> reviewer.".into());
        let completion = run_production_headless_chat_with_progress(
            request,
            &bootstrap,
            &HeadlessTurnCancellation::new(),
            None,
            None,
            None,
            None,
        )
        .expect("resumed production turn should complete");
        let provider_request = worker.join().expect("mock provider should finish");
        let reopened = SessionStore::open(&data_directory)
            .expect("session store should reopen")
            .load_session_for_resume(1)
            .expect("same session should remain resumable");

        assert_eq!(completion.metadata.id, 1);
        assert_eq!(
            provider_request["input"],
            serde_json::json!([
                {"role": "user", "content": [{"type": "input_text", "text": "first input"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "first reasoning"}]},
                {"type": "function_call", "call_id": "call-history", "name": "native::read", "arguments": "{\"path\":\"notes.md\"}"},
                {"role": "assistant", "content": [{"type": "output_text", "text": "calling the tool"}]},
                {"type": "function_call_output", "call_id": "call-history", "output": "file contents"},
                {"role": "system", "content": [{"type": "input_text", "text": "Agent capabilities expanded: primary -> reviewer."}]},
                {"role": "user", "content": [{"type": "input_text", "text": "second input"}]},
            ])
        );
        assert_eq!(reopened.metadata.id, 1);
        assert_eq!(reopened.metadata.active_agent, "reviewer");
        assert_eq!(reopened.metadata.completed_turn_count, 2);
        assert_eq!(
            reopened
                .messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            vec![
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::System,
                Role::User,
                Role::Assistant
            ]
        );
        assert_eq!(reopened.messages[..3], initial_messages);
        assert_eq!(
            reopened.messages[3].parts,
            vec![MessagePart::Text(
                "Agent capabilities expanded: primary -> reviewer.".into()
            )]
        );
        assert_eq!(
            reopened.messages[4].parts,
            vec![MessagePart::Text("second input".into())]
        );
        assert_eq!(
            reopened.messages[5].parts,
            vec![MessagePart::Text("second answer".into())]
        );

        std::fs::remove_dir_all(temporary).expect("temporary files should be removed");
    }

    #[test]
    fn tui_model_and_effort_commands_reach_each_provider_with_latest_selection_only() {
        for provider_type in ["openai-api", "openai-chatgpt"] {
            for model in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
                let request = run_tui_model_effort_provider_case(provider_type, model);

                assert_eq!(request["model"], model, "{provider_type}: {model}");
                assert_eq!(request["reasoning"]["effort"], "max", "{request}");
                assert!(
                    !request["input"].to_string().contains("gpt-4.1"),
                    "{provider_type} request input retained the replaced model: {request}"
                );
            }
        }
    }

    fn run_tui_model_effort_provider_case(
        provider_type: &str,
        selected_model: &str,
    ) -> serde_json::Value {
        let temporary = std::env::temp_dir().join(format!(
            "agens-tui-model-effort-{provider_type}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos()
        ));
        let project_root = temporary.join("project");
        let config_home = temporary.join("config");
        let data_directory = temporary.join("data");
        std::fs::create_dir_all(project_root.join(".git"))
            .expect("project marker should be created");
        std::fs::create_dir_all(&config_home).expect("config directory should be created");

        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("mock provider should bind");
        let address = listener
            .local_addr()
            .expect("mock provider should have an address");
        let expected_path = match provider_type {
            "openai-chatgpt" => "POST /codex/responses HTTP/1.1\r\n",
            _ => "POST /responses HTTP/1.1\r\n",
        };
        let worker = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};

            let (mut stream, _) = listener
                .accept()
                .expect("mock provider should accept the selected request");
            let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .expect("request line should be readable");
            assert_eq!(request_line, expected_path);

            let mut content_length = None;
            loop {
                let mut header = String::new();
                reader
                    .read_line(&mut header)
                    .expect("request header should be readable");
                if header == "\r\n" {
                    break;
                }
                if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length: ") {
                    content_length = Some(
                        value
                            .trim()
                            .parse::<usize>()
                            .expect("content length should be numeric"),
                    );
                }
            }

            let mut body =
                vec![0_u8; content_length.expect("request should include content length")];
            std::io::Read::read_exact(&mut reader, &mut body)
                .expect("request body should be readable");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"selected answer\"}\n\ndata: {\"type\":\"response.completed\"}\n\n")
                .expect("mock response should be written");

            serde_json::from_slice::<serde_json::Value>(&body)
                .expect("provider request should be valid JSON")
        });

        if provider_type == "openai-chatgpt" {
            std::fs::write(
                config_home.join("auth.json"),
                r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature","refresh_token":"refresh","account_id":"account","expires_at":"2030-01-01T00:00:00Z"}}"#,
            )
            .expect("ChatGPT credentials should be written");
        } else {
            std::fs::write(
                config_home.join("auth.json"),
                r#"{"openai-api":{"api_key":"test-key"}}"#,
            )
            .expect("OpenAI API credentials should be written");
        }

        let dependencies = CliDependencies::for_test(
            project_root,
            Some(temporary.join("home")),
            BTreeMap::from([
                (
                    "AGENS_CONFIG_HOME".to_owned(),
                    config_home.display().to_string(),
                ),
                ("OPENAI_API_KEY".to_owned(), "test-key".to_owned()),
            ]),
            BTreeMap::from([(
                config_home.join("config.toml"),
                format!(
                    "[provider]\ntype = \"{provider_type}\"\nmodel = \"gpt-4.1\"\nbase_url = \"http://{address}\"\n\n[options]\ndata_dir = \"{}\"\n",
                    data_directory.display()
                ),
            )]),
        );
        let bootstrap = bootstrap(&dependencies).expect("production bootstrap should be valid");
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        let cancellation = HeadlessTurnCancellation::new();

        let previous_model = if provider_type == "openai-chatgpt" {
            "gpt-5.4"
        } else {
            "o3"
        };
        let commands = [
            (
                format!("/model {previous_model}"),
                format!("Model: {previous_model}."),
            ),
            (
                "/effort high".to_owned(),
                "Reasoning effort: high.".to_owned(),
            ),
            (
                format!("/model {selected_model}"),
                format!("Model: {selected_model}."),
            ),
            (
                "/effort max".to_owned(),
                "Reasoning effort: max.".to_owned(),
            ),
        ];
        for (command, expected) in commands {
            assert_eq!(
                run_tui_prompt(&bootstrap, &command, &cancellation, &session, None)
                    .expect("valid TUI selection should succeed"),
                expected
            );
        }
        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/model unavailable",
                &cancellation,
                &session,
                None
            )
            .expect_err("invalid model should be refused")
            .to_string(),
            format!(
                "config: model is unavailable for {}",
                if provider_type == "openai-chatgpt" {
                    "ChatGPT subscription"
                } else {
                    "OpenAI API"
                }
            )
        );
        assert_eq!(
            run_tui_prompt(
                &bootstrap,
                "/effort unsupported",
                &cancellation,
                &session,
                None
            )
            .expect_err("invalid effort should be refused")
            .to_string(),
            "config: reasoning effort is unsupported"
        );
        let runtime_bootstrap = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        )
        .turn_bootstrap()
        .expect("turn provider credentials should resolve freshly");
        assert_eq!(
            run_tui_prompt(
                &runtime_bootstrap,
                "next request",
                &cancellation,
                &session,
                None
            )
            .expect("selected prompt should complete"),
            "selected answer"
        );

        let persisted = SessionStore::open(&data_directory)
            .unwrap()
            .load_session_for_resume(1)
            .unwrap();
        assert_eq!(
            persisted.metadata.provider_id.as_deref(),
            Some(provider_type)
        );
        assert_eq!(persisted.metadata.model_id.as_deref(), Some(selected_model));
        assert_eq!(
            persisted
                .metadata
                .reasoning_effort
                .map(agens_core::ReasoningEffort::as_str),
            Some("max")
        );
        assert!(!format!("{persisted:?}").contains("test-key"));
        assert!(!format!("{persisted:?}").contains("refresh"));

        let reopened = resume_tui_session(
            &bootstrap,
            persisted.metadata.id,
            &SkillCatalog::default(),
            &TuiCredentialResolver::with_environment(BTreeMap::from([(
                "OPENAI_API_KEY".into(),
                "test-key".into(),
            )])),
        )
        .expect("persisted selection should reopen");
        let reopened_selection = reopened.selection.unwrap();
        assert_eq!(reopened_selection.model(), selected_model);
        assert!(reopened_selection.metadata_known());
        assert_eq!(reopened_selection.reasoning_effort(), Some("max"));

        let request = worker.join().expect("mock provider should finish");
        std::fs::remove_dir_all(temporary).expect("temporary files should be removed");
        request
    }

    #[test]
    fn permission_error_mapping_is_sanitized_and_fails_closed() {
        let secret_input = r#"{"command":"SENTINEL_COMMAND","token":"SENTINEL_TOKEN"}"#;
        for (name, input) in [
            ("native::read", "{malformed"),
            ("native::read", secret_input),
            ("native::unknown", r#"{"path":"SENTINEL_PATH"}"#),
        ] {
            let outcome = run_production_batch(
                "permission-evaluation-invalid",
                Vec::new(),
                vec![MessagePart::ToolCall {
                    id: "invalid".into(),
                    name: name.into(),
                    input: input.into(),
                }],
                None,
                None,
                false,
            );

            assert_eq!(outcome.result, Err(HeadlessTurnError::PermissionEvaluation));
            assert!(outcome.executions.is_empty());
        }

        for (turn_error, expected) in [
            (
                HeadlessTurnError::Permission,
                "permission: permission evaluation failed",
            ),
            (
                HeadlessTurnError::PermissionRequired,
                "permission: permission approval is required",
            ),
            (
                HeadlessTurnError::PermissionEvaluation,
                "permission: permission target could not be evaluated; correct the tool arguments and retry",
            ),
        ] {
            let error = CliError::runtime(turn_error);
            assert_eq!(error.category, "permission");
            assert_eq!(error.to_string(), expected);
            assert!(!error.to_string().contains("SENTINEL_COMMAND"));
            assert!(!error.to_string().contains("SENTINEL_TOKEN"));

            assert!(matches!(
                tui_provider_outcome(Err(error)),
                TuiProviderOutcome::Failed { message, action }
                    if message == expected && action == TUI_ERROR_ACTION
            ));
        }
    }

    #[test]
    fn provider_context_and_network_render_sanitized_actions() {
        for (turn_error, expected_message, expected_action) in [
            (
                HeadlessTurnError::ProviderContext,
                "provider: request exceeds the model context window",
                "Start a new session or shorten the prompt, then retry.",
            ),
            (
                HeadlessTurnError::ProviderNetwork,
                "provider: network request failed",
                "Check the network connection, then retry.",
            ),
        ] {
            let error = CliError::runtime(turn_error);

            assert!(matches!(
                tui_provider_outcome(Err(error)),
                TuiProviderOutcome::Failed { message, action }
                    if message == expected_message && action == expected_action
            ));
        }
    }

    #[test]
    fn production_dispatcher_preserves_safe_native_failure_reason() {
        let outcome = run_production_batch(
            "safe-native-failure",
            vec![PermissionPromptAnswer::AllowOnce],
            vec![MessagePart::ToolCall {
                id: "glob".into(),
                name: "native::glob".into(),
                input: serde_json::json!({
                    "pattern": "**/*.md",
                    "_inject_tool_failure": "glob: entry limit of 10000 exceeded",
                })
                .to_string(),
            }],
            None,
            None,
            false,
        );

        assert!(outcome.result.is_ok());
        assert!(outcome.progress.iter().any(|event| matches!(
            event,
            TurnEvent::ToolResult(MessagePart::ToolResult {
                content,
                is_error: true,
                ..
            }) if content == "glob: entry limit of 10000 exceeded"
        )));
        assert_eq!(
            sanitized_native_tool_failure(
                "glob: /home/user/private token=SECRET remote body details"
            ),
            "tool execution failed"
        );
        assert_eq!(
            sanitized_native_tool_failure("glob: path is outside project root"),
            "glob: path validation failed"
        );
    }
}
