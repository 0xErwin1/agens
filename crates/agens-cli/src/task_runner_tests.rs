//! Tests for the subagent task runner: lifecycle, cancellation, backgrounding and the mailbox.
//!
//! They live in the binary crate rather than in `agens-tool-runtime` because
//! they drive the TUI permission bridge to answer a prompt, and a logic crate
//! must not gain a surface even in its test build.
#![cfg(test)]

use std::sync::{Arc, Mutex};

use agens_bus::BridgeTx;
use agens_core::ask_user::UnavailableAskUserPort;
use agens_core::{
    HeadlessTaskTerminal, HeadlessToolOutput, HeadlessTurnCancellation, HeadlessTurnError,
    MessagePart, PermissionDecision, PermissionMode, PermissionPattern, PermissionPolicy,
    PermissionRule, PermissionSession, SubagentErrorKind, SubagentStatus, TuiExecutionEvent,
    TuiRuntimeEvent, TuiSubagentEvent, TurnEvent,
};
use agens_dispatch::{TaskLaunchOutcome, TuiSelectedTaskLaunch};
use agens_session::context::CompletedSubagentTurn;
use agens_session::context::SessionContext;
use agens_tool_runtime::child::ChildRunError;
use agens_tool_runtime::child_catalog::ChildSurfaceRejection;
use agens_tool_runtime::launch_selected_task as launch_selected_tui_task;
use agens_tool_runtime::runner::{ProductionTaskRunner, TuiTaskControls, TuiTaskLifecycleBridge};
use agens_tools::TaskDeclarationRejection;
use agens_tools::{SkillCatalog, TaskLaunchMode, TaskProviderFailure, TaskRunnerError};

use agens_store::SessionStore;
use agens_tools::{
    TaskTerminalState, ToolDispatchRequest, ToolEvaluationOutcome, ToolExecutionContext,
};
use agens_tui::TuiPermissionReply;

use agens_fixtures::{
    session_bootstrap as tui_session_bootstrap, session_directory as tui_session_directory,
};
use agens_tool_runtime::runner::*;
use agens_tool_runtime::task::production_tui_task_runtime_with_runner;
use agens_tui_app::permission_prompt::{TuiPermissionPrompter, production_tui_permission_bridge};

#[test]
fn production_task_error_mapping_reserves_provider_for_provider_failures() {
    assert_eq!(
        map_task_turn_error(HeadlessTurnError::MaxIterations),
        TaskRunnerError::ChildFailure
    );
    assert_eq!(
        map_task_turn_error(HeadlessTurnError::Provider),
        TaskRunnerError::ProviderFailure(TaskProviderFailure::Protocol)
    );
    assert_eq!(
        map_task_turn_error(HeadlessTurnError::ProviderHistoryBudget),
        TaskRunnerError::ProviderFailure(TaskProviderFailure::ReplayBudget)
    );
    assert_eq!(
        map_task_turn_error(HeadlessTurnError::Tool),
        TaskRunnerError::ChildFailure
    );
}

#[test]
fn p1c1_terminal_observer_excludes_non_completed_matrix() {
    for (label, terminal) in [
        ("cancelled", Some(TaskTerminalState::Cancelled)),
        ("timed-out", Some(TaskTerminalState::Failed)),
        ("incomplete", None),
        ("failed", Some(TaskTerminalState::Failed)),
    ] {
        let temporary = tui_session_directory(&format!("p1c1-{label}"));
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let (events, _receiver) = BridgeTx::bounded(8);
        let controls = TuiTaskControls::default();
        let session = Arc::new(Mutex::new(SessionContext {
            selected_subagent: Some("reviewer".into()),
            ..SessionContext::fresh()
        }));
        let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, controls.clone())
            .with_session_writer(bootstrap.clone(), Arc::clone(&session));
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
            &SkillCatalog::default(),
            Box::new(TuiPermissionPrompter(
                production_tui_permission_bridge().0,
                None,
            )),
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
                Arc::new(Mutex::new(Vec::new())),
            )
            .with_lifecycle_bridge(lifecycle_bridge),
            Box::new(UnavailableAskUserPort),
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
        let cancellation = HeadlessTurnCancellation::new();
        let worker_session = Arc::clone(&session);
        let worker_cancellation = cancellation.clone();
        let worker = std::thread::spawn(move || {
            launch_selected_tui_task(
                &mut runtime,
                &worker_session,
                "review task",
                false,
                &worker_cancellation,
            )
        });
        let lifecycle =
            agens_tui_app::test_support::wait_for("the running task to be observed", || {
                controls
                    .0
                    .lifecycle(agens_tools::TaskExecutionId::from_value(1))
            });

        assert!(session.lock().unwrap().identifier.is_none());
        if terminal.is_some() {
            assert!(lifecycle.transition_to_background());
        }
        assert!(session.lock().unwrap().identifier.is_none());
        if let Some(terminal) = terminal {
            assert!(lifecycle.finish(terminal));
        }
        if label == "failed" {
            assert!(!lifecycle.finish(TaskTerminalState::Completed));
        }

        cancellation.cancel();
        let _ = worker.join().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        assert!(session.lock().unwrap().identifier.is_none());
        assert!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .list_sessions()
                .unwrap()
                .is_empty()
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }
}

#[test]
fn u15_a1b2_selected_launch_uses_the_registered_production_task_runner() {
    let temporary = tui_session_directory("selected-task-launch");
    let bootstrap = tui_session_bootstrap(
        &temporary,
        &[(
            "reviewer",
            "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
        )],
    );
    let probe = Arc::new(Mutex::new(Vec::new()));
    let (bridge, requests) = production_tui_permission_bridge();
    let reply_bridge = bridge.clone();
    let mut runtime = production_tui_task_runtime_with_runner(
        &bootstrap,
        &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
        &SkillCatalog::default(),
        Box::new(TuiPermissionPrompter(bridge, None)),
        ProductionTaskRunner::with_probe(
            bootstrap.clone(),
            agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
            Arc::clone(&probe),
        ),
        Box::new(UnavailableAskUserPort),
    )
    .unwrap();
    let session = Arc::new(Mutex::new(SessionContext {
        selected_subagent: Some("reviewer".into()),
        ..SessionContext::fresh()
    }));
    let cancellation = HeadlessTurnCancellation::new();
    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Allow,
            PermissionPattern::Exact("native::task".into()),
            PermissionPattern::Any,
        )],
    );
    let mut dispatcher = runtime.dispatcher.lock().unwrap();
    let ToolEvaluationOutcome::Authorized(handle) = dispatcher
        .evaluate(
            &policy,
            &[],
            &PermissionSession::new(),
            ToolDispatchRequest::new(
                "project",
                "native::task",
                serde_json::json!({
                    "agent": "reviewer",
                    "description": "model task",
                    "background": true,
                }),
            ),
        )
        .unwrap()
    else {
        panic!("registered model task should authorize");
    };
    assert_eq!(
        dispatcher
            .execute(
                handle,
                &ToolExecutionContext::from_headless_adapter(cancellation.adapter_view()),
            )
            .unwrap()
            .content,
        "Subagent #1 running in background"
    );
    drop(dispatcher);

    // The prompt this answers is raised by the launch below and by nothing
    // before it, so the answering thread starts here: a waiter spawned earlier
    // would spend its budget on setup instead of on the prompt.
    let reply = std::thread::spawn(move || {
        let request = agens_tui_app::test_support::wait_for(
            "the selected task to request permission",
            || requests.try_recv().ok(),
        );
        reply_bridge.reply(request.id(), TuiPermissionReply::AllowOnce)
    });

    assert_eq!(
        launch_selected_tui_task(&mut runtime, &session, "review task", false, &cancellation),
        Ok(TuiSelectedTaskLaunch::Dispatched)
    );
    // A background dispatch reaches the runner on its own thread while the
    // foreground one reaches it inline, so which lands first is not a
    // guarantee this design makes. Wait for both, then compare without
    // ordering.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while probe.lock().unwrap().len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "both task dispatches should reach the runner, saw {:?}",
            *probe.lock().unwrap()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut observed = probe.lock().unwrap().clone();
    observed.sort_by_key(|entry| entry.0);
    assert_eq!(observed[0].1, TaskLaunchMode::Background);
    assert_eq!(observed[1].1, TaskLaunchMode::Foreground);
    assert_ne!(observed[0].0, observed[1].0);
    assert!(reply.join().unwrap());

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn p1c1_p1b_authorized_runner_persists_one_completed_subagent_turn() {
    let temporary = tui_session_directory("p1b-child-events");
    let bootstrap = tui_session_bootstrap(
        &temporary,
        &[(
            "reviewer",
            "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
        )],
    );
    let probe = Arc::new(Mutex::new(Vec::new()));
    let (events, receiver) = BridgeTx::bounded(16);
    let controls = TuiTaskControls::default();
    let session = Arc::new(Mutex::new(SessionContext {
        selected_subagent: Some("reviewer".into()),
        ..SessionContext::fresh()
    }));
    let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, controls)
        .with_session_writer(bootstrap.clone(), Arc::clone(&session));
    let mut runtime = production_tui_task_runtime_with_runner(
        &bootstrap,
        &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
        &SkillCatalog::default(),
        Box::new(TuiPermissionPrompter(
            production_tui_permission_bridge().0,
            None,
        )),
        ProductionTaskRunner::with_progress_probe(
            bootstrap.clone(),
            agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
            Arc::clone(&probe),
            vec![
                TurnEvent::ProviderPart(MessagePart::Reasoning("inspect".into())),
                TurnEvent::ProviderPart(MessagePart::Text("partial".into())),
                TurnEvent::ToolCallRequested {
                    id: "read-1".into(),
                    name: "native::read".into(),
                    input: format!("authorization {}", "x".repeat(300)),
                },
                TurnEvent::ToolResult(MessagePart::ToolResult {
                    tool_call_id: "read-1".into(),
                    content: format!("token {}", "y".repeat(300)),
                    is_error: false,
                }),
            ],
        )
        .with_lifecycle_bridge(lifecycle_bridge),
        Box::new(UnavailableAskUserPort),
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
    let cancellation = HeadlessTurnCancellation::new();

    assert_eq!(
        launch_selected_tui_task(&mut runtime, &session, "review task", false, &cancellation),
        Ok(TuiSelectedTaskLaunch::Dispatched)
    );

    let mut received = Vec::new();
    for _ in 0..8 {
        match receiver.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(event) => received.push(event.into_parts().1),
            Err(error) => {
                panic!("child event should reach the TUI bridge: {received:?}: {error}")
            }
        }
    }
    assert_eq!(
        received,
        vec![
            TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: TuiExecutionEvent::ForegroundStarted { id: 1 },
            },
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::started_on(
                1,
                "reviewer",
                "review task",
                agens_core::TuiExecutionState::ForegroundRunning,
                Some("gpt-4.1"),
                None,
            )),
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::reasoning(1, "inspect")),
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::text(1, "partial")),
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::tool_call(
                1,
                "read-1",
                "native::read",
                "[redacted]",
            )),
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::tool_result(
                1,
                "read-1",
                "[redacted]",
                false,
            )),
            TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: TuiExecutionEvent::Completed { id: 1 },
            },
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::terminal(
                1,
                SubagentStatus::Success,
                "probe",
            )),
        ]
    );
    assert_eq!(probe.lock().unwrap().len(), 1);
    let session_id = agens_tui_app::test_support::wait_for(
        "the completed terminal to persist exactly one durable turn",
        || session.lock().unwrap().identifier,
    );
    let stored = SessionStore::open(bootstrap.data_directory())
        .unwrap()
        .load_session_for_resume(session_id)
        .unwrap();
    assert_eq!(stored.metadata.completed_turn_count, 1);
    assert_eq!(stored.messages.len(), 3);
    assert_eq!(
        stored.messages[2].parts[0],
        MessagePart::ToolResult {
            tool_call_id: "subagent:1".into(),
            content: "probe".into(),
            is_error: false,
        }
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn failed_subagent_turn_persistence_publishes_a_runtime_error() {
    let temporary = tui_session_directory("subagent-persistence-failure");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    std::fs::create_dir_all(bootstrap.data_directory().join("agens.db")).unwrap();
    let (events, receiver) = BridgeTx::bounded(4);
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let bridge = TuiTaskLifecycleBridge::new(events, TuiTaskControls::default())
        .with_session_writer(bootstrap.clone(), Arc::clone(&session));
    let persist = bridge
        .persist_completed
        .clone()
        .expect("session writer should be installed");

    persist(CompletedSubagentTurn {
        id: 7,
        agent: "reviewer".into(),
        task: "review task".into(),
        final_result: "done".into(),
        tool_uses: 1,
    });

    assert_eq!(
        receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("persistence failure should reach the TUI bridge")
            .into_parts()
            .1,
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::error(7, SubagentErrorKind::Runtime,))
    );
    assert!(session.lock().unwrap().identifier.is_none());

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn production_runner_error_publication_orders_sanitized_typed_failure_before_terminal() {
    for (
        source,
        expected_error,
        expected_kind,
        expected_execution,
        expected_status,
        expected_result,
    ) in [
        (
            ChildRunError::Authentication,
            TaskRunnerError::ProviderFailure(TaskProviderFailure::Authentication),
            Some(SubagentErrorKind::Authentication),
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
        (
            ChildRunError::Context,
            TaskRunnerError::ProviderFailure(TaskProviderFailure::Context),
            Some(SubagentErrorKind::Context),
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
        (
            ChildRunError::ReplayBudget,
            TaskRunnerError::ProviderFailure(TaskProviderFailure::ReplayBudget),
            Some(SubagentErrorKind::ReplayBudget),
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
        (
            ChildRunError::Network,
            TaskRunnerError::ProviderFailure(TaskProviderFailure::Network),
            Some(SubagentErrorKind::Network),
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
        (
            ChildRunError::Provider,
            TaskRunnerError::ProviderFailure(TaskProviderFailure::Protocol),
            Some(SubagentErrorKind::Provider),
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
        (
            ChildRunError::Protocol,
            TaskRunnerError::ProviderFailure(TaskProviderFailure::Protocol),
            Some(SubagentErrorKind::Protocol),
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
        (
            ChildRunError::RateLimited,
            TaskRunnerError::ProviderFailure(TaskProviderFailure::RateLimited),
            Some(SubagentErrorKind::RateLimited),
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
        (
            ChildRunError::Rejected,
            TaskRunnerError::ProviderFailure(TaskProviderFailure::Rejected),
            Some(SubagentErrorKind::Rejected),
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
        (
            ChildRunError::Server,
            TaskRunnerError::ProviderFailure(TaskProviderFailure::Server),
            Some(SubagentErrorKind::Server),
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
        (
            ChildRunError::Tool,
            TaskRunnerError::ChildFailure,
            Some(SubagentErrorKind::Tool),
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
        (
            ChildRunError::Runtime,
            TaskRunnerError::ChildFailure,
            Some(SubagentErrorKind::Runtime),
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
        (
            ChildRunError::DeclarationRejected(ChildSurfaceRejection {
                reason: TaskDeclarationRejection::ConfigurationDenies,
                tool: "native::bash".into(),
            }),
            TaskRunnerError::DeclarationRejected {
                reason: TaskDeclarationRejection::ConfigurationDenies,
                tool: "native::bash".into(),
            },
            Some(SubagentErrorKind::Runtime),
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
        (
            ChildRunError::Cancelled,
            TaskRunnerError::Cancelled,
            None,
            TuiExecutionEvent::Cancelled { id: 1 },
            SubagentStatus::Cancelled,
            "cancelled",
        ),
        (
            ChildRunError::TimedOut,
            TaskRunnerError::TimedOut,
            None,
            TuiExecutionEvent::Failed { id: 1 },
            SubagentStatus::Failure,
            "failed",
        ),
    ] {
        let temporary = tui_session_directory("runner-error-publication");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let (events, receiver) = BridgeTx::bounded(8);
        let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, TuiTaskControls::default());
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
            &SkillCatalog::default(),
            Box::new(TuiPermissionPrompter(
                production_tui_permission_bridge().0,
                None,
            )),
            ProductionTaskRunner::with_failure_probe(
                bootstrap.clone(),
                agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
                source.clone(),
                "provider-token=super-secret-error-detail",
            )
            .with_lifecycle_bridge(lifecycle_bridge),
            Box::new(UnavailableAskUserPort),
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

        let reported = match &expected_error {
            TaskRunnerError::Cancelled => HeadlessTaskTerminal::Cancelled.message().to_owned(),
            TaskRunnerError::TimedOut => HeadlessTaskTerminal::TimedOut.message().to_owned(),
            TaskRunnerError::ProviderFailure(cause) => format!(
                "{} [cause: {}]",
                HeadlessTaskTerminal::ProviderFailure.message(),
                cause.label()
            ),
            TaskRunnerError::ChildFailure => {
                HeadlessTaskTerminal::ChildFailure.message().to_owned()
            }
            TaskRunnerError::DeclarationRejected { reason, tool } => format!(
                "{} [declaration: {tool}; {}]",
                HeadlessTaskTerminal::DeclarationRejected.message(),
                reason.label()
            ),
        };
        assert_eq!(
            launch_selected_tui_task(
                &mut runtime,
                &session,
                "review task",
                false,
                &HeadlessTurnCancellation::new(),
            ),
            Ok(TuiSelectedTaskLaunch::Rejected(
                TaskLaunchOutcome::Dispatched(HeadlessToolOutput::failure(reported))
            ))
        );

        let mut expected = vec![
            TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: TuiExecutionEvent::ForegroundStarted { id: 1 },
            },
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::started_on(
                1,
                "reviewer",
                "review task",
                agens_core::TuiExecutionState::ForegroundRunning,
                Some("gpt-4.1"),
                None,
            )),
        ];
        if let Some(kind) = expected_kind {
            expected.push(TuiRuntimeEvent::SubagentExecution(
                TuiSubagentEvent::error_with_reference(1, kind, "abc12345"),
            ));
        }
        expected.push(TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: expected_execution,
        });
        expected.push(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::terminal(1, expected_status, expected_result),
        ));

        let received = (0..expected.len())
            .map(|_| {
                receiver
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .expect("runner failure should publish every bridge event")
                    .into_parts()
                    .1
            })
            .collect::<Vec<_>>();
        assert_eq!(received, expected);
        assert!(
            received
                .iter()
                .all(|event| !format!("{event:?}").contains("super-secret"))
        );
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err(),
            "runner failure must publish exactly one terminal"
        );
        assert_eq!(expected_error, source.task_runner_error());

        std::fs::remove_dir_all(temporary).unwrap();
    }
}
