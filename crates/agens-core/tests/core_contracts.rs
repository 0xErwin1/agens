use std::{
    future::{Future, ready},
    task::{Context, Poll, Waker},
};

use agens_core::{
    AttemptKey, CompletedTurnPersistenceError, CompletedTurnRepository, CompletedTurnSnapshot,
    CompletedTurnStoreError, Error, ErrorCategory, Message, MessagePart, PermissionDecision,
    PermissionMode, PermissionPattern, PermissionPolicy, PermissionRequest, PermissionRule,
    PermissionSession, ProjectPermissionGrant, Role, ToolAccess, ToolResultFacts, TurnCoordinator,
    TurnEvent, TurnEventError, TurnState, TurnTransitionError,
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
        .accept_tool_result("call-1", "found core".into(), false, None)
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
        .accept_tool_result("call-2", "package manifest".into(), false, None)
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
        coordinator.accept_tool_result("call-1", "result".into(), false, None),
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
        coordinator.accept_tool_result("call-2", "result".into(), false, None),
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

#[test]
fn completed_turn_with_tool_result_facts_still_persists_and_validates() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            input: "{\"command\":\"exit 1\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result(
            "call-1",
            "exit 1".into(),
            true,
            Some(ToolResultFacts::Bash { exit_code: Some(1) }),
        )
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::Text("complete".into()))
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();

    assert_eq!(coordinator.state(), TurnState::Completed);

    let events = coordinator.events();
    let tool_result_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                TurnEvent::ToolResult(MessagePart::ToolResult { tool_call_id, .. })
                    if tool_call_id == "call-1"
            )
        })
        .expect("tool result event must be present");
    match events.get(tool_result_index + 1) {
        Some(TurnEvent::ToolResultFacts { identity, facts }) => {
            assert_eq!(identity.tool_call_id, "call-1");
            assert_eq!(identity.session_id, None);
            assert_eq!(identity.attempt_id, None);
            assert_eq!(identity.sequence, 1);
            assert_eq!(identity.dispatch_id, None);
            assert_eq!(*facts, ToolResultFacts::Bash { exit_code: Some(1) });
        }
        other => panic!("expected a facts event immediately after the tool result, got {other:?}"),
    }

    let mut repository = RecordingCompletedTurnRepository::default();
    let persist_result = block_on_ready(coordinator.persist_completed_turn(&mut repository));
    assert_eq!(persist_result, Ok(()));

    let snapshot = repository
        .snapshots
        .first()
        .expect("snapshot must be recorded");
    assert!(
        !snapshot
            .events()
            .iter()
            .any(|event| matches!(event, TurnEvent::ToolResultFacts { .. })),
        "persisted snapshot must exclude live-only facts events"
    );

    let replayed = CompletedTurnSnapshot::from_persisted_events(snapshot.events().to_vec());
    assert!(
        replayed.is_ok(),
        "replay of persisted history must validate: {replayed:?}"
    );
}

#[test]
fn persisted_history_containing_tool_result_facts_is_rejected() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            input: "{\"command\":\"exit 0\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result("call-1", "exit 0".into(), false, None)
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::Text("complete".into()))
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();

    let mut events = coordinator.events().to_vec();
    let tool_result_index = events
        .iter()
        .position(|event| matches!(event, TurnEvent::ToolResult(_)))
        .expect("tool result event must be present");

    let mut facts_source = TurnCoordinator::new();
    facts_source.begin().unwrap();
    facts_source
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            input: "{\"command\":\"exit 0\"}".into(),
        })
        .unwrap();
    facts_source.finish_provider_iteration().unwrap();
    facts_source
        .accept_tool_result(
            "call-1",
            "exit 0".into(),
            false,
            Some(ToolResultFacts::Bash { exit_code: Some(0) }),
        )
        .unwrap();
    let facts_event = facts_source
        .events()
        .iter()
        .find(|event| matches!(event, TurnEvent::ToolResultFacts { .. }))
        .cloned()
        .expect("facts event must be present in the source coordinator");

    events.insert(tool_result_index + 1, facts_event);

    assert!(CompletedTurnSnapshot::from_persisted_events(events).is_err());
}

#[test]
fn accept_tool_result_emits_no_facts_event() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            input: "{\"path\":\"Cargo.toml\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result("call-1", "manifest".into(), false, None)
        .unwrap();

    assert_eq!(
        &coordinator.events()[coordinator.events().len() - 2..],
        &[
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "manifest".into(),
                is_error: false,
            }),
            TurnEvent::StateChanged(TurnState::Requesting),
        ]
    );
    assert!(!coordinator.events().iter().any(|event| matches!(
        event,
        TurnEvent::ToolResultFacts { identity, .. } if identity.tool_call_id == "call-1"
    )));
}

#[test]
fn accept_tool_result_orders_facts_between_result_and_state_change() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "write".into(),
            input: "{\"path\":\"a.txt\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result(
            "call-1",
            "wrote a.txt".into(),
            false,
            Some(ToolResultFacts::Write {
                path: "a.txt".into(),
                bytes_written: 3,
            }),
        )
        .unwrap();

    let tail = &coordinator.events()[coordinator.events().len() - 3..];
    assert_eq!(
        tail[0],
        TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "call-1".into(),
            content: "wrote a.txt".into(),
            is_error: false,
        })
    );
    match &tail[1] {
        TurnEvent::ToolResultFacts { identity, facts } => {
            assert_eq!(identity.tool_call_id, "call-1");
            assert_eq!(identity.session_id, None);
            assert_eq!(identity.attempt_id, None);
            assert_eq!(identity.sequence, 1);
            assert_eq!(identity.dispatch_id, None);
            assert_eq!(
                *facts,
                ToolResultFacts::Write {
                    path: "a.txt".into(),
                    bytes_written: 3,
                }
            );
        }
        other => panic!("expected a facts event, got {other:?}"),
    }
    assert_eq!(tail[2], TurnEvent::StateChanged(TurnState::Requesting));
}

#[test]
fn accept_tool_result_none_emits_no_facts_event() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            input: "{\"path\":\"Cargo.toml\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result("call-1", "manifest".into(), false, None)
        .unwrap();

    assert_eq!(
        &coordinator.events()[coordinator.events().len() - 2..],
        &[
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "manifest".into(),
                is_error: false,
            }),
            TurnEvent::StateChanged(TurnState::Requesting),
        ]
    );
    assert!(!coordinator.events().iter().any(|event| matches!(
        event,
        TurnEvent::ToolResultFacts { identity, .. } if identity.tool_call_id == "call-1"
    )));
}

#[test]
fn rejected_tool_result_with_facts_emits_nothing() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    let events_before_rejection = coordinator.events().to_vec();

    assert_eq!(
        coordinator.accept_tool_result(
            "call-1",
            "result".into(),
            false,
            Some(ToolResultFacts::Bash { exit_code: Some(0) }),
        ),
        Err(TurnEventError::UnexpectedToolResult {
            tool_call_id: "call-1".into(),
        })
    );
    assert_eq!(coordinator.events(), events_before_rejection);
}

#[test]
fn multiple_tool_calls_keep_each_facts_event_with_its_own_result() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            input: "{\"path\":\"a.txt\"}".into(),
        })
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-2".into(),
            name: "bash".into(),
            input: "{\"command\":\"exit 2\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();

    coordinator
        .accept_tool_result("call-1", "contents".into(), false, None)
        .unwrap();
    assert_eq!(coordinator.state(), TurnState::Dispatching);
    coordinator
        .accept_tool_result(
            "call-2",
            "exit 2".into(),
            true,
            Some(ToolResultFacts::Bash { exit_code: Some(2) }),
        )
        .unwrap();

    let tail = &coordinator.events()[coordinator.events().len() - 3..];
    assert_eq!(
        tail[0],
        TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "call-2".into(),
            content: "exit 2".into(),
            is_error: true,
        })
    );
    match &tail[1] {
        TurnEvent::ToolResultFacts { identity, facts } => {
            assert_eq!(identity.tool_call_id, "call-2");
            assert_eq!(identity.session_id, None);
            assert_eq!(identity.attempt_id, None);
            assert_eq!(identity.sequence, 1);
            assert_eq!(identity.dispatch_id, None);
            assert_eq!(*facts, ToolResultFacts::Bash { exit_code: Some(2) });
        }
        other => panic!("expected a facts event, got {other:?}"),
    }
    assert_eq!(tail[2], TurnEvent::StateChanged(TurnState::Requesting));
    assert!(!coordinator.events().iter().any(|event| matches!(
        event,
        TurnEvent::ToolResultFacts { identity, .. } if identity.tool_call_id == "call-1"
    )));
}

#[test]
fn fact_identity_defaults_to_none_for_a_freshly_constructed_coordinator() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            input: "{\"command\":\"exit 0\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result(
            "call-1",
            "exit 0".into(),
            false,
            Some(ToolResultFacts::Bash { exit_code: Some(0) }),
        )
        .unwrap();

    let identity = coordinator
        .events()
        .iter()
        .find_map(|event| match event {
            TurnEvent::ToolResultFacts { identity, .. } => Some(identity),
            _ => None,
        })
        .expect("facts event must be present");

    assert_eq!(identity.session_id, None);
    assert_eq!(identity.attempt_id, None);
    assert_eq!(identity.sequence, 1);
    assert_eq!(identity.dispatch_id, None);
    assert_eq!(identity.tool_call_id, "call-1");
}

#[test]
fn coordinator_for_attempt_fills_identity_with_session_and_attempt_ids() {
    let mut coordinator = TurnCoordinator::for_attempt(AttemptKey::new(7, 42).unwrap());

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            input: "{\"command\":\"exit 0\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result(
            "call-1",
            "exit 0".into(),
            false,
            Some(ToolResultFacts::Bash { exit_code: Some(0) }),
        )
        .unwrap();

    let identity = coordinator
        .events()
        .iter()
        .find_map(|event| match event {
            TurnEvent::ToolResultFacts { identity, .. } => Some(identity),
            _ => None,
        })
        .expect("facts event must be present");

    assert_eq!(identity.session_id, Some(7));
    assert_eq!(identity.attempt_id, Some(42));
    assert_eq!(identity.sequence, 1);
}

#[test]
fn fact_sequence_is_monotonic_and_gap_free_across_three_calls_in_one_turn() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    for id in ["call-1", "call-2", "call-3"] {
        coordinator
            .accept_provider_part(MessagePart::ToolCall {
                id: id.into(),
                name: "bash".into(),
                input: "{\"command\":\"exit 0\"}".into(),
            })
            .unwrap();
    }
    coordinator.finish_provider_iteration().unwrap();

    for id in ["call-1", "call-2", "call-3"] {
        coordinator
            .accept_tool_result(
                id,
                "exit 0".into(),
                false,
                Some(ToolResultFacts::Bash { exit_code: Some(0) }),
            )
            .unwrap();
    }

    let sequences: Vec<u64> = coordinator
        .events()
        .iter()
        .filter_map(|event| match event {
            TurnEvent::ToolResultFacts { identity, .. } => Some(identity.sequence),
            _ => None,
        })
        .collect();

    assert_eq!(sequences, vec![1, 2, 3]);
}

#[test]
fn no_second_acceptance_entry_point_symbol_exists() {
    let source = include_str!("../src/lib.rs");

    assert!(
        !source.contains("accept_tool_result_with_facts"),
        "a second facts-taking acceptance function must not exist; every call site must state \
         its choice explicitly through the single accept_tool_result entry point"
    );
}

#[test]
fn consume_generated_events_replay_path_emits_no_facts_event_for_a_persisted_tool_result() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            input: "{\"command\":\"exit 0\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();
    coordinator
        .accept_tool_result("call-1", "exit 0".into(), false, None)
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::Text("complete".into()))
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();

    let persisted = coordinator.events().to_vec();
    let replayed = CompletedTurnSnapshot::from_persisted_events(persisted)
        .expect("replay of a facts-free persisted stream must validate");

    assert!(
        !replayed
            .events()
            .iter()
            .any(|event| matches!(event, TurnEvent::ToolResultFacts { .. })),
        "replay must never regenerate a facts event for a persisted tool result"
    );
}

#[test]
fn two_concurrent_tool_calls_pin_the_full_result_facts_slice() {
    let mut coordinator = TurnCoordinator::new();

    coordinator.begin().unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-a".into(),
            name: "write".into(),
            input: "{\"path\":\"a.txt\"}".into(),
        })
        .unwrap();
    coordinator
        .accept_provider_part(MessagePart::ToolCall {
            id: "call-b".into(),
            name: "bash".into(),
            input: "{\"command\":\"exit 0\"}".into(),
        })
        .unwrap();
    coordinator.finish_provider_iteration().unwrap();

    coordinator
        .accept_tool_result(
            "call-a",
            "wrote a.txt".into(),
            false,
            Some(ToolResultFacts::Write {
                path: "a.txt".into(),
                bytes_written: 11,
            }),
        )
        .unwrap();
    coordinator
        .accept_tool_result(
            "call-b",
            "exit 0".into(),
            false,
            Some(ToolResultFacts::Bash { exit_code: Some(0) }),
        )
        .unwrap();

    let tail = &coordinator.events()[coordinator.events().len() - 5..];
    assert_eq!(
        tail[0],
        TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "call-a".into(),
            content: "wrote a.txt".into(),
            is_error: false,
        })
    );
    match &tail[1] {
        TurnEvent::ToolResultFacts { identity, facts } => {
            assert_eq!(identity.tool_call_id, "call-a");
            assert_eq!(identity.session_id, None);
            assert_eq!(identity.attempt_id, None);
            assert_eq!(identity.sequence, 1);
            assert_eq!(identity.dispatch_id, None);
            assert_eq!(
                *facts,
                ToolResultFacts::Write {
                    path: "a.txt".into(),
                    bytes_written: 11,
                }
            );
        }
        other => panic!("expected call-a's facts event, got {other:?}"),
    }
    assert_eq!(
        tail[2],
        TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "call-b".into(),
            content: "exit 0".into(),
            is_error: false,
        })
    );
    match &tail[3] {
        TurnEvent::ToolResultFacts { identity, facts } => {
            assert_eq!(identity.tool_call_id, "call-b");
            assert_eq!(identity.session_id, None);
            assert_eq!(identity.attempt_id, None);
            assert_eq!(identity.sequence, 2);
            assert_eq!(identity.dispatch_id, None);
            assert_eq!(*facts, ToolResultFacts::Bash { exit_code: Some(0) });
        }
        other => panic!("expected call-b's facts event, got {other:?}"),
    }
    assert_eq!(tail[4], TurnEvent::StateChanged(TurnState::Requesting));
}
