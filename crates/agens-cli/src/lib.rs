#[cfg(test)]
use std::collections::BTreeMap;
use std::ffi::OsString;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::{Arc, Mutex};

use clap::Parser as _;

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
use agens_core::{HeadlessTurnError, Message, MessagePart};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_store::SessionStore;
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
use agens_tools::{TaskExecutionRegistry, TaskMessageSource, TaskMessageTarget};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use agens_tools::{TaskLaunchMode, ToolDispatchRequest, ToolEvaluationOutcome};
use agens_tui::TuiSubagentErrorKind;

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

use bootstrap::effective_max_iterations;
use diagnostics::{
    next_diagnostic_reference, operation_diagnostics, record_parent_terminal,
    record_subagent_terminal,
};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use dispatch::{ProductionToolDispatcher, poll_permission_port};
use error::cancellation_result;
use headless::block_on_headless_turn;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use headless::{provider_messages, run_production_headless_chat_with_progress};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use permissions::{ProductionPermissionGate, permission_policy};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use test_support::{
    BatchTool, ProductionBatchInput, native_batch_call, persist_tui_session, rotation_agent,
    rotation_dispatcher, run_production_batch_with_policy, tui_project, tui_session_bootstrap,
    tui_session_directory, tui_session_messages,
};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tools::runtime::production_dangerous_child_tool_runtime;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use tui::agents::BundledModelValidator;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls this unqualified. Remove this re-export once the test module moves.
use tui::engine::run_tui_prompt;
use tui::models::tui_model_source;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::provider::TuiCredentialResolver;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::resume::{ensure_active_tui_agent_runtime, resume_tui_session};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::router::TuiRuntimeRouter;
use tui::run_tui;
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::session::{ActiveAgentRuntime, TuiSessionContext};
#[cfg(test)]
// Scaffolding for Phase 3: `mod tests` still opens with `use super::*;` and
// calls these unqualified. Remove this re-export once the test module moves.
use tui::session::{AgentRotationError, rotate_active_agent};
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
        CompletedTurnRepository, CompletedTurnSnapshot, PermissionRule, ToolAccess, TurnState,
    };

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
    fn the_removed_tool_output_key_is_no_longer_accepted() {
        let document = parse_toml_document("[ui]\ntruncate_tool_output = true\n").unwrap();

        assert!(validate_toml_document(&document).is_err());
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
}
