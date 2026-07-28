//! What is left of dispatch once the table itself moved out: the two functions
//! that launch the subagent a surface armed for the next prompt. They stay here
//! because they reach into the task runtime and the lifecycle bridge, which the
//! dispatch table deliberately does not know about.

use std::sync::{Arc, Mutex};

use agens_core::{HeadlessTurnCancellation, HeadlessTurnError, HeadlessTurnPortError};
use agens_dispatch::{TaskLaunchOutcome, TaskLaunchRequest, TuiSelectedTaskLaunch};
use agens_tools::TaskLaunchMode;

use crate::tools::runner::TuiTaskLifecycleBridge;
use crate::tools::task::ProductionTuiTaskRuntime;
use agens_error::{CliError, ExitStatus};
use agens_session::context::SessionContext;

pub(crate) fn launch_selected_tui_task(
    runtime: &mut ProductionTuiTaskRuntime,
    session: &Arc<Mutex<SessionContext>>,
    description: &str,
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

pub(crate) fn selected_tui_task_skips_parent(
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

#[cfg(test)]
mod tests {
    use agens_config::ToolLimitSettings;
    use agens_core::{
        HeadlessToolCall, HeadlessToolDispatcher, HeadlessToolOutput, MessagePart,
        PermissionDecision, PermissionMode, PermissionPattern, PermissionPolicy, PermissionRule,
        PermissionSession, SubmitOrigin, ToolAccess, TurnEvent,
    };
    use agens_store::PermissionGrantStore;
    use agens_tools::{
        DispatchTool, SkillCatalog, ToolDispatcher, ToolExecutionContext, ToolOutput,
    };
    use agens_tui::{
        BridgeTx, TuiExecutionEvent, TuiPermissionReply, TuiProviderOutcome, TuiRuntimeEvent,
        TuiSubagentEvent,
    };

    use super::*;
    use crate::CliError;
    use crate::permission_prompt::{TuiPermissionPrompter, production_tui_permission_bridge};
    use crate::test_support::{
        BatchTool, ProductionBatchInput, RecordingPrompt, native_batch_call, run_production_batch,
        run_production_batch_with_policy, tui_session_bootstrap, tui_session_directory,
    };
    use crate::tools::runner::{ProductionTaskRunner, TuiTaskControls};
    use crate::tools::runtime::production_dangerous_child_tool_runtime;
    use crate::tools::task::production_tui_task_runtime_with_runner;
    use agens_agents::ensure_active_agent_runtime;
    use agens_agents::select_subagent;
    use agens_core::HeadlessPermissionGate;
    use agens_dispatch::{
        AuthorizedNativeTaskRuntime, ProductionToolDispatcher, origin_launches_selected_subagent,
        poll_permission_port, sanitized_native_tool_failure,
    };
    use agens_permissions::{
        PermissionPromptAnswer, PermissionPrompter, ProductionPermissionGate,
        ProductionPermissionResolver, ProductionPromptAuthorization, SharedToolDispatcher,
    };
    use agens_session::context::SessionContext;
    use std::collections::BTreeMap;
    use std::path::Path;

    #[test]
    fn u15_authorization_model_and_tui_launch_share_one_native_task_path() {
        struct RecordingTaskTool(Arc<std::sync::atomic::AtomicUsize>);

        impl DispatchTool for RecordingTaskTool {
            fn permission_target(
                &self,
                arguments: &serde_json::Value,
            ) -> Result<String, agens_core::Error> {
                arguments
                    .get("agent")
                    .and_then(serde_json::Value::as_str)
                    .filter(|agent| !agent.is_empty())
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| agens_core::Error::Tool("missing agent".into()))
            }

            fn execute(
                &mut self,
                _: &ToolExecutionContext,
                _: serde_json::Value,
            ) -> Result<ToolOutput, agens_core::Error> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ToolOutput::success("executed"))
            }
        }

        fn authorized_native_task_runtime<P: PermissionPrompter>(
            directory: &Path,
            policy: PermissionPolicy,
            dispatcher: SharedToolDispatcher,
            prompt: P,
        ) -> AuthorizedNativeTaskRuntime<P> {
            let grants = Arc::new(Mutex::new(Vec::new()));
            let allowed = Arc::new(Mutex::new(BTreeMap::new()));
            let prompts = Arc::new(Mutex::new(BTreeMap::new()));
            let gate = ProductionPermissionGate::new(
                policy.clone(),
                Arc::clone(&grants),
                PermissionSession::new(),
                "project".into(),
                Arc::clone(&dispatcher),
                Arc::clone(&allowed),
                Arc::clone(&prompts),
            );
            let resolver = ProductionPermissionResolver::new(
                prompt,
                PermissionGrantStore::open(directory).unwrap(),
                grants,
                prompts,
                ProductionPromptAuthorization {
                    policy,
                    session: PermissionSession::new(),
                    project: "project".into(),
                    dispatcher: Arc::clone(&dispatcher),
                    allowed: Arc::clone(&allowed),
                },
            );

            AuthorizedNativeTaskRuntime {
                gate,
                resolver,
                dispatcher: ProductionToolDispatcher::new(dispatcher, allowed),
                next_call_id: 0,
            }
        }

        fn launch_request<'a>(
            agent: &'a str,
            description: &'a str,
            background: bool,
        ) -> TaskLaunchRequest<'a> {
            TaskLaunchRequest {
                agent,
                description,
                background,
            }
        }

        let directory =
            std::env::temp_dir().join(format!("agens-u15-authorization-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
        dispatcher
            .lock()
            .unwrap()
            .register_native(
                "native::task",
                agens_core::ToolAccess::Write,
                RecordingTaskTool(Arc::clone(&executions)),
            )
            .unwrap();

        let ask_policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Ask,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let mut model = authorized_native_task_runtime(
            &directory,
            PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    PermissionDecision::Allow,
                    PermissionPattern::Exact("native::task".into()),
                    PermissionPattern::Any,
                )],
            ),
            Arc::clone(&dispatcher),
            RecordingPrompt {
                answers: vec![PermissionPromptAnswer::AllowOnce],
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );
        let mut tui = authorized_native_task_runtime(
            &directory,
            ask_policy,
            Arc::clone(&dispatcher),
            RecordingPrompt {
                answers: vec![PermissionPromptAnswer::AllowOnce],
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );
        let cancellation = HeadlessTurnCancellation::new();

        assert_eq!(
            model.launch(
                launch_request("reviewer", "model task", false),
                &cancellation
            ),
            Ok(TaskLaunchOutcome::Dispatched(HeadlessToolOutput::success(
                "executed"
            )))
        );
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            tui.launch(launch_request("reviewer", "TUI task", true), &cancellation),
            Ok(TaskLaunchOutcome::Dispatched(HeadlessToolOutput::success(
                "executed"
            )))
        );
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 2);

        let mut denied = authorized_native_task_runtime(
            &directory,
            PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    PermissionDecision::Deny,
                    PermissionPattern::Exact("native::task".into()),
                    PermissionPattern::Any,
                )],
            ),
            Arc::clone(&dispatcher),
            RecordingPrompt {
                answers: Vec::new(),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );
        assert_eq!(
            denied.launch(launch_request("reviewer", "denied", false), &cancellation),
            Ok(TaskLaunchOutcome::Denied)
        );
        assert_eq!(
            tui.launch(launch_request("", "invalid", false), &cancellation),
            Ok(TaskLaunchOutcome::RejectedEmptyInput)
        );
        assert_eq!(
            tui.launch(launch_request("reviewer", "", false), &cancellation),
            Ok(TaskLaunchOutcome::RejectedEmptyInput)
        );
        cancellation.cancel();
        assert_eq!(
            tui.launch(
                launch_request("reviewer", "cancelled", false),
                &cancellation
            ),
            Ok(TaskLaunchOutcome::RejectedCancelled)
        );
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 2);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_runtime_scheduled_turn_never_consumes_the_armed_subagent() {
        let temporary = tui_session_directory("auto-turn-armed-subagent");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        assert_eq!(
            select_subagent(&bootstrap, "reviewer", &session),
            Ok("Subagent: reviewer.".to_owned())
        );

        assert!(origin_launches_selected_subagent(SubmitOrigin::User));
        assert!(origin_launches_selected_subagent(SubmitOrigin::Background));
        assert!(!origin_launches_selected_subagent(
            SubmitOrigin::SubagentCompletion
        ));
        assert_eq!(
            session.lock().unwrap().selected_subagent.as_deref(),
            Some("reviewer")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn u15_c1c_backgrounded_success_skips_the_parent_provider_and_history_path() {
        let temporary = tui_session_directory("selected-background-launch");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (events, receiver) = BridgeTx::bounded(8);
        let controls = TuiTaskControls::default();
        let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, controls.clone());
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
            &SkillCatalog::default(),
            Box::new(TuiPermissionPrompter(production_tui_permission_bridge().0)),
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
                Arc::clone(&probe),
            )
            .with_lifecycle_bridge(lifecycle_bridge.clone()),
        )
        .unwrap();
        runtime.authorized.gate.policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let session = Arc::new(Mutex::new(SessionContext {
            selected_subagent: Some("reviewer".into()),
            ..SessionContext::fresh()
        }));
        ensure_active_agent_runtime(&bootstrap, &session, &runtime.dispatcher).unwrap();
        let cancellation = HeadlessTurnCancellation::new();
        let parent_runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let next_event = |timeout| receiver.recv_timeout(timeout).unwrap().into_parts().1;
        let worker = std::thread::spawn({
            let session = Arc::clone(&session);
            let cancellation = cancellation.clone();
            let lifecycle_bridge = lifecycle_bridge.clone();
            let parent_runs = Arc::clone(&parent_runs);
            move || {
                let skips_parent = selected_tui_task_skips_parent(
                    launch_selected_tui_task(
                        &mut runtime,
                        &session,
                        "review task",
                        false,
                        &cancellation,
                    ),
                    &lifecycle_bridge,
                )?;
                if skips_parent {
                    Ok(TuiProviderOutcome::Backgrounded)
                } else {
                    parent_runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err(CliError::runtime(HeadlessTurnError::Provider))
                }
            }
        });
        assert_eq!(
            next_event(std::time::Duration::from_secs(1)),
            TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: TuiExecutionEvent::ForegroundStarted { id: 1 },
            }
        );
        assert_eq!(
            next_event(std::time::Duration::from_secs(1)),
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::started(
                1,
                "reviewer",
                "review task",
                agens_tui::TuiExecutionState::ForegroundRunning,
            ))
        );
        assert!(controls.transition_to_background(1));
        assert_eq!(
            next_event(std::time::Duration::from_secs(1)),
            TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: TuiExecutionEvent::Backgrounded { id: 1 },
            }
        );
        assert_eq!(worker.join().unwrap(), Ok(TuiProviderOutcome::Backgrounded));
        assert_eq!(
            next_event(std::time::Duration::from_secs(1)),
            TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: TuiExecutionEvent::Completed { id: 1 },
            }
        );
        let probe = probe.lock().unwrap();
        assert_eq!(probe.len(), 1);
        assert_eq!(parent_runs.load(std::sync::atomic::Ordering::SeqCst), 0);
        let session = session.lock().unwrap();
        assert!(session.messages.is_empty());
        assert_eq!(
            session
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn u15_a1b2_permission_cardinality_is_exact_for_allow_ask_and_deny() {
        fn policy(decision: PermissionDecision) -> PermissionPolicy {
            PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    decision,
                    PermissionPattern::Exact("native::task".into()),
                    PermissionPattern::Any,
                )],
            )
        }

        let temporary = tui_session_directory("selected-task-cardinality");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (bridge, requests) = production_tui_permission_bridge();
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
            &SkillCatalog::default(),
            Box::new(TuiPermissionPrompter(bridge.clone())),
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
                Arc::clone(&probe),
            ),
        )
        .unwrap();
        let cancellation = HeadlessTurnCancellation::new();
        let selected = || {
            Arc::new(Mutex::new(SessionContext {
                selected_subagent: Some("reviewer".into()),
                ..SessionContext::fresh()
            }))
        };

        runtime.authorized.gate.policy = policy(PermissionDecision::Allow);
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "allow", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Dispatched)
        );
        assert_eq!(probe.lock().unwrap().len(), 1);
        assert!(requests.try_recv().is_err());

        let ask = policy(PermissionDecision::Ask);
        runtime.authorized.gate.policy = ask.clone();
        runtime.authorized.resolver.authorization.policy = ask;
        let reply_bridge = bridge.clone();
        let reply = std::thread::spawn(move || {
            let request = requests
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("ask should prompt once");
            reply_bridge.reply(request.id(), TuiPermissionReply::AllowOnce)
        });
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "ask", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Dispatched)
        );
        assert!(reply.join().unwrap());
        assert_eq!(probe.lock().unwrap().len(), 2);

        runtime.authorized.gate.policy = policy(PermissionDecision::Deny);
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "deny", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Rejected(TaskLaunchOutcome::Denied))
        );
        assert_eq!(probe.lock().unwrap().len(), 2);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn u15_a1b2_rejections_leave_the_concrete_runner_and_grants_unchanged() {
        let temporary = tui_session_directory("selected-task-rejections");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (bridge, requests) = production_tui_permission_bridge();
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
            &SkillCatalog::default(),
            Box::new(TuiPermissionPrompter(bridge.clone())),
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
                Arc::clone(&probe),
            ),
        )
        .unwrap();
        let selected = || {
            Arc::new(Mutex::new(SessionContext {
                selected_subagent: Some("reviewer".into()),
                ..SessionContext::fresh()
            }))
        };
        let cancellation = HeadlessTurnCancellation::new();

        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Rejected(
                TaskLaunchOutcome::RejectedEmptyInput
            ))
        );
        cancellation.cancel();
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "cancelled", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Rejected(
                TaskLaunchOutcome::RejectedCancelled
            ))
        );
        assert_eq!(probe.lock().unwrap().len(), 0);
        assert!(requests.try_recv().is_err());
        assert!(runtime.authorized.gate.grants.lock().unwrap().is_empty());

        runtime.authorized.gate.policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let unavailable = Arc::new(Mutex::new(SessionContext {
            selected_subagent: Some("missing".into()),
            ..SessionContext::fresh()
        }));
        assert_eq!(
            launch_selected_tui_task(
                &mut runtime,
                &unavailable,
                "missing",
                false,
                &HeadlessTurnCancellation::new(),
            ),
            Err(CliError::runtime(HeadlessTurnError::Tool))
        );
        assert_eq!(probe.lock().unwrap().len(), 0);

        let expired = HeadlessTurnCancellation::with_deadline(std::time::Duration::ZERO);
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "expired", false, &expired),
            Err(CliError::runtime(HeadlessTurnError::TimedOut))
        );
        assert_eq!(probe.lock().unwrap().len(), 0);

        let ask = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Ask,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        runtime.authorized.gate.policy = ask.clone();
        runtime.authorized.resolver.authorization.policy = ask;
        let active = HeadlessTurnCancellation::new();
        let reply_bridge = bridge.clone();
        let reply = std::thread::spawn(move || {
            [TuiPermissionReply::DenyOnce, TuiPermissionReply::Cancelled]
                .into_iter()
                .map(|answer| {
                    let request = requests
                        .recv_timeout(std::time::Duration::from_secs(1))
                        .expect("asked rejection should prompt once");
                    reply_bridge.reply(request.id(), answer)
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "deny once", false, &active),
            Ok(TuiSelectedTaskLaunch::Rejected(TaskLaunchOutcome::Denied))
        );
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "cancel ask", false, &active),
            Err(CliError::runtime(HeadlessTurnError::Cancelled))
        );
        assert!(reply.join().unwrap().into_iter().all(|replied| replied));
        assert_eq!(probe.lock().unwrap().len(), 0);
        assert!(runtime.authorized.gate.grants.lock().unwrap().is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
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
}
