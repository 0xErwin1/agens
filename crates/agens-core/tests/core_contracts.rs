use std::{
    future::{Future, ready},
    task::{Context, Poll, Waker},
};

use agens_core::{
    CompletedTurnPersistenceError, CompletedTurnRepository, CompletedTurnSnapshot,
    CompletedTurnStoreError, Error, ErrorCategory, Message, MessagePart, PermissionDecision,
    PermissionMode, PermissionPattern, PermissionPolicy, PermissionRequest, PermissionRule,
    PermissionSession, ProjectPermissionGrant, Role, ToolAccess, TurnCoordinator, TurnEvent,
    TurnEventError, TurnState, TurnTransitionError,
};

#[derive(Default)]
struct RecordingCompletedTurnRepository {
    calls: usize,
    snapshots: Vec<CompletedTurnSnapshot>,
    failure: Option<CompletedTurnStoreError>,
}

impl CompletedTurnRepository for RecordingCompletedTurnRepository {
    fn persist_completed_turn(
        &mut self,
        snapshot: CompletedTurnSnapshot,
    ) -> impl Future<Output = Result<(), CompletedTurnStoreError>> + Send {
        self.calls += 1;

        if let Some(error) = self.failure.clone() {
            return ready(Err(error));
        }

        self.snapshots.push(snapshot);
        ready(Ok(()))
    }
}

fn block_on_ready<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = std::pin::pin!(future);

    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test repository must complete immediately"),
    }
}

fn completed_coordinator() -> TurnCoordinator {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::Text("complete".into()))
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();

    coordinator
}

#[test]
fn message_preserves_each_closed_part_payload() {
    let message = Message {
        role: Role::Assistant,
        parts: vec![
            MessagePart::Text("answer".into()),
            MessagePart::Reasoning("considering options".into()),
            MessagePart::ToolCall {
                id: "call-1".into(),
                name: "search".into(),
                input: "{\"query\":\"agens\"}".into(),
            },
            MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "result".into(),
                is_error: false,
            },
        ],
    };

    assert_eq!(message.role, Role::Assistant);
    assert_eq!(message.parts.len(), 4);
    assert_eq!(
        message.parts[2],
        MessagePart::ToolCall {
            id: "call-1".into(),
            name: "search".into(),
            input: "{\"query\":\"agens\"}".into(),
        }
    );
}

#[test]
fn terminal_turn_states_are_distinct_from_active_states() {
    assert!(TurnState::Completed.is_terminal());
    assert!(TurnState::Cancelled.is_terminal());
    assert!(TurnState::Failed.is_terminal());
    assert!(!TurnState::Requesting.is_terminal());
    assert!(!TurnState::Streaming.is_terminal());
    assert!(!TurnState::Dispatching.is_terminal());
}

#[test]
fn typed_errors_keep_their_category_and_context() {
    let provider_error = Error::Provider("invalid response frame".into());
    let cancelled = Error::Cancelled;

    assert_eq!(provider_error.category(), ErrorCategory::Provider);
    assert_eq!(
        provider_error.to_string(),
        "provider: invalid response frame"
    );
    assert_eq!(cancelled.category(), ErrorCategory::Cancelled);
    assert_eq!(cancelled.to_string(), "cancelled");
}

#[test]
fn turn_state_advances_through_a_tool_iteration_to_completion() {
    let state = TurnState::Idle
        .transition_to(TurnState::Requesting)
        .unwrap()
        .transition_to(TurnState::Streaming)
        .unwrap()
        .transition_to(TurnState::Dispatching)
        .unwrap()
        .transition_to(TurnState::Requesting)
        .unwrap()
        .transition_to(TurnState::Completed)
        .unwrap();

    assert_eq!(state, TurnState::Completed);
}

#[test]
fn every_active_turn_state_can_be_cancelled_or_failed() {
    for state in [
        TurnState::Requesting,
        TurnState::Streaming,
        TurnState::Dispatching,
    ] {
        assert_eq!(
            state.transition_to(TurnState::Cancelled),
            Ok(TurnState::Cancelled)
        );
        assert_eq!(
            state.transition_to(TurnState::Failed),
            Ok(TurnState::Failed)
        );
    }
}

#[test]
fn invalid_and_terminal_turn_transitions_return_typed_source_and_target_errors() {
    let invalid = TurnState::Idle.transition_to(TurnState::Streaming);

    assert_eq!(
        invalid,
        Err(TurnTransitionError {
            source: TurnState::Idle,
            target: TurnState::Streaming,
        })
    );

    for source in [
        TurnState::Completed,
        TurnState::Cancelled,
        TurnState::Failed,
    ] {
        let transition = source.transition_to(TurnState::Requesting);

        assert_eq!(
            transition,
            Err(TurnTransitionError {
                source,
                target: TurnState::Requesting,
            })
        );
        assert_eq!(
            transition.unwrap_err().to_string(),
            format!("invalid turn state transition: {source:?} -> Requesting")
        );
    }
}

#[test]
fn coordinator_emits_deterministic_events_for_two_tool_iterations() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::Reasoning("inspect the repository".into()))
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "search".into(),
            input: "{\"query\":\"core\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result("call-1", "found core".into(), false)
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::Text("continue".into()))
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-2".into(),
            name: "read".into(),
            input: "{\"path\":\"Cargo.toml\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result("call-2", "package manifest".into(), false)
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::Text("complete".into()))
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();

    assert_eq!(coordinator.state(), TurnState::Completed);
    assert_eq!(
        coordinator.events(),
        &[
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Reasoning("inspect the repository".into())),
            TurnEvent::ProviderPart(MessagePart::ToolCall {
                id: "call-1".into(),
                name: "search".into(),
                input: "{\"query\":\"core\"}".into(),
            }),
            TurnEvent::StateChanged(TurnState::Dispatching),
            TurnEvent::ToolCallRequested {
                id: "call-1".into(),
                name: "search".into(),
                input: "{\"query\":\"core\"}".into(),
            },
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "found core".into(),
                is_error: false,
            }),
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("continue".into())),
            TurnEvent::ProviderPart(MessagePart::ToolCall {
                id: "call-2".into(),
                name: "read".into(),
                input: "{\"path\":\"Cargo.toml\"}".into(),
            }),
            TurnEvent::StateChanged(TurnState::Dispatching),
            TurnEvent::ToolCallRequested {
                id: "call-2".into(),
                name: "read".into(),
                input: "{\"path\":\"Cargo.toml\"}".into(),
            },
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-2".into(),
                content: "package manifest".into(),
                is_error: false,
            }),
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("complete".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ]
    );
}

#[test]
fn coordinator_rejects_out_of_order_and_uncorrelated_tool_results() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    assert_eq!(
        coordinator.accept_tool_result("call-1", "result".into(), false),
        Err(TurnEventError::UnexpectedToolResult {
            tool_call_id: "call-1".into(),
        })
    );

    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "search".into(),
            input: "{}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();

    assert_eq!(
        coordinator.accept_tool_result("call-2", "result".into(), false),
        Err(TurnEventError::UnexpectedToolResult {
            tool_call_id: "call-2".into(),
        })
    );
}

#[test]
fn coordinator_rejects_provider_tool_results_without_mutating_state_or_events() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    let events_before_rejection = coordinator.events().to_vec();

    assert_eq!(
        coordinator.accept_provider_part(MessagePart::ToolResult {
            tool_call_id: "call-1".into(),
            content: "result".into(),
            is_error: false,
        }),
        Err(TurnEventError::InvalidProviderPart)
    );
    assert_eq!(coordinator.state(), TurnState::Requesting);
    assert_eq!(coordinator.events(), events_before_rejection);
}

#[test]
fn coordinator_rejects_duplicate_pending_tool_call_ids_without_mutating_state_or_events() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "search".into(),
            input: "{}".into(),
        })
        .unwrap();
    let events_before_rejection = coordinator.events().to_vec();

    assert_eq!(
        coordinator.accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            input: "{}".into(),
        }),
        Err(TurnEventError::DuplicateToolCallId {
            id: "call-1".into(),
        })
    );
    assert_eq!(coordinator.state(), TurnState::Streaming);
    assert_eq!(coordinator.events(), events_before_rejection);
}

#[test]
fn cancellation_and_failure_reject_all_further_events() {
    let mut cancelled = TurnCoordinator::new();

    cancelled.begin().unwrap();
    cancelled.cancel().unwrap();
    assert_eq!(
        cancelled.accept_provider_part(MessagePart::Text("late".into())),
        Err(TurnEventError::Transition(TurnTransitionError {
            source: TurnState::Cancelled,
            target: TurnState::Streaming,
        }))
    );

    let mut failed = TurnCoordinator::new();

    failed.begin().unwrap();
    failed.fail().unwrap();
    assert_eq!(
        failed.finish_provider_iteration(),
        Err(TurnEventError::Transition(TurnTransitionError {
            source: TurnState::Failed,
            target: TurnState::Streaming,
        }))
    );
}

#[test]
fn completed_turn_is_persisted_once_with_its_ordered_events() {
    let mut coordinator = completed_coordinator();
    let mut repository = RecordingCompletedTurnRepository::default();

    block_on_ready(coordinator.persist_completed_turn(&mut repository)).unwrap();

    assert_eq!(repository.snapshots.len(), 1);
    assert_eq!(repository.calls, 1);
    assert_eq!(
        repository.snapshots[0].events(),
        &[
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("complete".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ]
    );
    assert!(coordinator.has_persisted_completed_turn());
    assert_eq!(
        block_on_ready(coordinator.persist_completed_turn(&mut repository)),
        Err(CompletedTurnPersistenceError::AlreadyPersisted)
    );
    assert_eq!(repository.snapshots.len(), 1);
    assert_eq!(repository.calls, 1);
}

#[test]
fn restores_a_completed_snapshot_from_live_ordered_events() {
    let events = completed_coordinator().events().to_vec();

    let snapshot = CompletedTurnSnapshot::from_persisted_events(events.clone()).unwrap();

    assert_eq!(snapshot.events(), events);
}

#[test]
fn rejects_non_completed_or_invalid_persisted_snapshots() {
    let invalid_event_sequences = [
        vec![TurnEvent::StateChanged(TurnState::Requesting)],
        vec![
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Cancelled),
        ],
        vec![
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Failed),
        ],
        vec![
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "unexpected".into(),
                is_error: false,
            }),
        ],
    ];

    for events in invalid_event_sequences {
        assert!(CompletedTurnSnapshot::from_persisted_events(events).is_err());
    }
}

#[test]
fn non_completed_turns_never_invoke_completed_turn_persistence() {
    let mut active = TurnCoordinator::new();
    active.begin().unwrap();

    for mut coordinator in [TurnCoordinator::new(), active] {
        let mut repository = RecordingCompletedTurnRepository::default();

        assert_eq!(
            block_on_ready(coordinator.persist_completed_turn(&mut repository)),
            Err(CompletedTurnPersistenceError::NotCompleted {
                state: coordinator.state(),
            })
        );
        assert_eq!(repository.calls, 0);
        assert!(repository.snapshots.is_empty());
    }

    for mut coordinator in [
        {
            let mut coordinator = TurnCoordinator::new();
            coordinator.begin().unwrap();
            coordinator.cancel().unwrap();
            coordinator
        },
        {
            let mut coordinator = TurnCoordinator::new();
            coordinator.begin().unwrap();
            coordinator.fail().unwrap();
            coordinator
        },
    ] {
        let mut repository = RecordingCompletedTurnRepository::default();

        assert_eq!(
            block_on_ready(coordinator.persist_completed_turn(&mut repository)),
            Err(CompletedTurnPersistenceError::NotCompleted {
                state: coordinator.state(),
            })
        );
        assert_eq!(repository.calls, 0);
        assert!(repository.snapshots.is_empty());
    }
}

#[test]
fn rejected_turn_events_never_invoke_completed_turn_persistence() {
    let mut coordinator = TurnCoordinator::new();
    let mut repository = RecordingCompletedTurnRepository::default();

    coordinator.begin().unwrap();
    assert_eq!(
        coordinator.accept_provider_part(MessagePart::ToolResult {
            tool_call_id: "call-1".into(),
            content: "rejected".into(),
            is_error: false,
        }),
        Err(TurnEventError::InvalidProviderPart)
    );
    assert_eq!(
        block_on_ready(coordinator.persist_completed_turn(&mut repository)),
        Err(CompletedTurnPersistenceError::NotCompleted {
            state: TurnState::Requesting,
        })
    );
    assert_eq!(repository.calls, 0);
    assert!(repository.snapshots.is_empty());
}

#[test]
fn completed_turn_persistence_failure_is_typed_and_does_not_claim_success() {
    let mut coordinator = completed_coordinator();
    let failure = CompletedTurnStoreError::new("database unavailable");
    let mut repository = RecordingCompletedTurnRepository {
        calls: 0,
        snapshots: Vec::new(),
        failure: Some(failure.clone()),
    };

    assert_eq!(
        block_on_ready(coordinator.persist_completed_turn(&mut repository)),
        Err(CompletedTurnPersistenceError::Store(failure))
    );
    assert_eq!(repository.calls, 1);
    assert!(!coordinator.has_persisted_completed_turn());
    assert!(repository.snapshots.is_empty());
    assert_eq!(
        block_on_ready(coordinator.persist_completed_turn(&mut repository)),
        Err(CompletedTurnPersistenceError::AlreadyAttempted)
    );
    assert_eq!(repository.calls, 1);
}

fn write_request(project: &str, target: &str) -> PermissionRequest {
    PermissionRequest::new(project, "edit", target, ToolAccess::Write)
}

#[test]
fn permission_global_deny_and_chat_mode_cannot_be_weakened() {
    let policy = PermissionPolicy::new(
        PermissionMode::Chat,
        vec![
            PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::Exact("edit".into()),
                PermissionPattern::Any,
            ),
            PermissionRule::project(
                "project-a",
                PermissionDecision::Allow,
                PermissionPattern::Exact("edit".into()),
                PermissionPattern::Any,
            ),
        ],
    );
    let session = PermissionSession::with_temporary_bypass();
    let grant = ProjectPermissionGrant::allow(
        "project-a",
        PermissionPattern::Exact("edit".into()),
        PermissionPattern::Any,
    );

    assert_eq!(
        policy.evaluate(
            &write_request("project-a", "src/lib.rs"),
            &[grant],
            &session
        ),
        PermissionDecision::Deny
    );

    let mode_only_policy = PermissionPolicy::new(PermissionMode::Chat, Vec::new());

    assert_eq!(
        mode_only_policy.evaluate(
            &write_request("project-a", "src/lib.rs"),
            &[ProjectPermissionGrant::allow(
                "project-a",
                PermissionPattern::Exact("edit".into()),
                PermissionPattern::Any,
            )],
            &session,
        ),
        PermissionDecision::Deny
    );
}

#[test]
fn permission_static_rules_are_scoped_deterministic_and_deny_wins_conflicts() {
    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![
            PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("read".into()),
                PermissionPattern::Any,
            ),
            PermissionRule::project(
                "project-a",
                PermissionDecision::Allow,
                PermissionPattern::Exact("edit".into()),
                PermissionPattern::Exact("src/lib.rs".into()),
            ),
            PermissionRule::project(
                "project-a",
                PermissionDecision::Deny,
                PermissionPattern::Exact("edit".into()),
                PermissionPattern::Exact("src/lib.rs".into()),
            ),
        ],
    );
    let session = PermissionSession::new();

    assert_eq!(
        policy.evaluate(
            &PermissionRequest::new("project-a", "read", "README.md", ToolAccess::ReadOnly),
            &[],
            &session,
        ),
        PermissionDecision::Allow
    );
    assert_eq!(
        policy.evaluate(&write_request("project-a", "src/lib.rs"), &[], &session),
        PermissionDecision::Deny
    );
}

#[test]
fn permission_grants_follow_static_rules_and_precede_session_bypass() {
    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Ask,
            PermissionPattern::Exact("edit".into()),
            PermissionPattern::Exact("src/lib.rs".into()),
        )],
    );
    let request = write_request("project-a", "src/lib.rs");
    let grants = [ProjectPermissionGrant::allow(
        "project-a",
        PermissionPattern::Exact("edit".into()),
        PermissionPattern::Exact("src/lib.rs".into()),
    )];

    assert_eq!(
        policy.evaluate(&request, &grants, &PermissionSession::new()),
        PermissionDecision::Allow
    );
    assert_eq!(
        policy.evaluate(
            &request,
            &grants,
            &PermissionSession::with_temporary_bypass(),
        ),
        PermissionDecision::Allow
    );
}

#[test]
fn permission_project_static_rules_require_their_exact_project() {
    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::project(
            "project-a",
            PermissionDecision::Allow,
            PermissionPattern::Exact("edit".into()),
            PermissionPattern::Exact("src/lib.rs".into()),
        )],
    );
    let session = PermissionSession::new();

    assert_eq!(
        policy.evaluate(&write_request("project-a", "src/lib.rs"), &[], &session),
        PermissionDecision::Allow
    );
    assert_eq!(
        policy.evaluate(&write_request("project-b", "src/lib.rs"), &[], &session),
        PermissionDecision::Ask
    );
    assert_eq!(
        policy.evaluate(&write_request("", "src/lib.rs"), &[], &session),
        PermissionDecision::Ask
    );
}

#[test]
fn permission_project_grants_match_their_project_and_input_without_persistence() {
    let policy = PermissionPolicy::new(PermissionMode::Edit, Vec::new());
    let session = PermissionSession::new();
    let grants = [
        ProjectPermissionGrant::allow(
            "project-a",
            PermissionPattern::Exact("edit".into()),
            PermissionPattern::Exact("src/lib.rs".into()),
        ),
        ProjectPermissionGrant::new(
            "project-a",
            PermissionDecision::Deny,
            PermissionPattern::Exact("edit".into()),
            PermissionPattern::Exact("secrets.env".into()),
        ),
    ];

    assert_eq!(
        policy.evaluate(&write_request("project-a", "src/lib.rs"), &grants, &session),
        PermissionDecision::Allow
    );
    assert_eq!(
        policy.evaluate(
            &write_request("project-a", "src/main.rs"),
            &grants,
            &session
        ),
        PermissionDecision::Ask
    );
    assert_eq!(
        policy.evaluate(&write_request("project-b", "src/lib.rs"), &grants, &session),
        PermissionDecision::Ask
    );
    assert_eq!(
        policy.evaluate(
            &write_request("project-a", "secrets.env"),
            &grants,
            &session
        ),
        PermissionDecision::Deny
    );
}

#[test]
fn permission_temporary_bypass_only_resolves_otherwise_ask_for_its_session() {
    let policy = PermissionPolicy::new(PermissionMode::Edit, Vec::new());
    let disabled = PermissionSession::new();
    let bypassed = PermissionSession::with_temporary_bypass();
    let request = write_request("project-a", "src/lib.rs");

    assert_eq!(
        policy.evaluate(&request, &[], &disabled),
        PermissionDecision::Ask
    );
    assert_eq!(
        policy.evaluate(&request, &[], &bypassed),
        PermissionDecision::Allow
    );
}
