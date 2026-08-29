use std::future::{Future, ready};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agens_core::{
    AttemptKey, CompletedTurnRepository, CompletedTurnSnapshot, CompletedTurnStoreError, FactPath,
    HeadlessIntraTurnInbox, HeadlessPermissionGate, HeadlessPermissionResolver, HeadlessToolCall,
    HeadlessToolDispatcher, HeadlessToolOutput, HeadlessTurnCancellation, HeadlessTurnError,
    HeadlessTurnPortError, IntraTurnInputSource, IntraTurnSteeringQueue, Message, MessagePart,
    PendingIntraTurnInput, PermissionDecision, Role, ToolOutcome, ToolResultFacts, TurnEvent,
    TurnProgressSink, TurnProvider, TurnState, run_headless_turn, run_headless_turn_with_inbox,
    run_headless_turn_with_max_iterations, run_headless_turn_with_progress,
};

#[test]
fn progress_sink_receives_state_and_provider_events_before_completion() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let progress: TurnProgressSink = {
        let observed = Arc::clone(&observed);
        Arc::new(move |event| observed.lock().unwrap().push(event))
    };
    let mut provider = Provider::new(vec![Ok(vec![MessagePart::Text("visible early".into())])]);
    let mut gate = PermissionGate::default();
    let mut resolver = PermissionResolver::default();
    let mut dispatcher = ToolDispatcher::default();
    let mut repository = Repository::default();

    block_on_ready(run_headless_turn_with_progress(
        &mut provider,
        &mut gate,
        &mut resolver,
        &mut dispatcher,
        &mut repository,
        &HeadlessTurnCancellation::new(),
        Some(&progress),
        None,
    ))
    .unwrap();

    assert_eq!(
        *observed.lock().unwrap(),
        vec![
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("visible early".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ]
    );
}

#[test]
fn a_fact_emitted_in_a_real_attempt_carries_that_attempts_identity() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let progress: TurnProgressSink = {
        let observed = Arc::clone(&observed);
        Arc::new(move |event| observed.lock().unwrap().push(event))
    };
    let mut provider = Provider::new(vec![
        Ok(vec![MessagePart::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            input: "{\"command\":\"exit 0\"}".into(),
        }]),
        Ok(vec![MessagePart::Text("complete".into())]),
    ]);
    let mut gate = PermissionGate {
        decisions: vec![PermissionDecision::Allow],
        denial_facts: None,
    };
    let mut resolver = PermissionResolver::default();
    let mut dispatcher = ToolDispatcher {
        outputs: vec![Ok(HeadlessToolOutput::success("exit 0").with_facts(
            ToolResultFacts::Bash {
                outcome: ToolOutcome::Succeeded,
                exit_code: Some(0),
            },
        ))],
        ..ToolDispatcher::default()
    };
    let mut repository = Repository::default();
    let attempt = AttemptKey::new(7, 3).unwrap();

    block_on_ready(run_headless_turn_with_progress(
        &mut provider,
        &mut gate,
        &mut resolver,
        &mut dispatcher,
        &mut repository,
        &HeadlessTurnCancellation::new(),
        Some(&progress),
        Some(attempt),
    ))
    .unwrap();

    let observed = observed.lock().unwrap();
    let identity = observed
        .iter()
        .find_map(|event| match event {
            TurnEvent::ToolResultFacts { identity, .. } => Some(identity),
            _ => None,
        })
        .expect("a facts event must be observed");

    assert_eq!(identity.session_id, Some(7));
    assert_eq!(identity.attempt_id, Some(3));
}

#[test]
fn headless_turn_forwards_tool_result_facts_to_the_progress_sink() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let progress: TurnProgressSink = {
        let observed = Arc::clone(&observed);
        Arc::new(move |event| observed.lock().unwrap().push(event))
    };
    let mut provider = Provider::new(vec![
        Ok(vec![MessagePart::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            input: "{\"command\":\"exit 2\"}".into(),
        }]),
        Ok(vec![MessagePart::Text("complete".into())]),
    ]);
    let mut gate = PermissionGate {
        decisions: vec![PermissionDecision::Allow],
        denial_facts: None,
    };
    let mut resolver = PermissionResolver::default();
    let mut dispatcher = ToolDispatcher {
        outputs: vec![Ok(HeadlessToolOutput::success("exit 2").with_facts(
            ToolResultFacts::Bash {
                outcome: ToolOutcome::Failed,
                exit_code: Some(2),
            },
        ))],
        ..ToolDispatcher::default()
    };
    let mut repository = Repository::default();

    block_on_ready(run_headless_turn_with_progress(
        &mut provider,
        &mut gate,
        &mut resolver,
        &mut dispatcher,
        &mut repository,
        &HeadlessTurnCancellation::new(),
        Some(&progress),
        None,
    ))
    .unwrap();

    let observed = observed.lock().unwrap();
    let tool_result_index = observed
        .iter()
        .position(|event| matches!(event, TurnEvent::ToolResult(_)))
        .expect("tool result event must reach the progress sink");

    match observed.get(tool_result_index + 1) {
        Some(TurnEvent::ToolResultFacts { identity, facts }) => {
            assert_eq!(identity.tool_call_id, "call-1");
            assert_eq!(
                *facts,
                ToolResultFacts::Bash {
                    outcome: ToolOutcome::Failed,
                    exit_code: Some(2)
                }
            );
        }
        other => panic!("expected a facts event after the tool result, got {other:?}"),
    }
}

#[test]
fn an_invalid_permission_target_becomes_a_recoverable_tool_result() {
    struct InvalidInputGate;

    impl HeadlessPermissionGate for InvalidInputGate {
        fn evaluate(
            &mut self,
            _call: &HeadlessToolCall,
            _cancellation: &HeadlessTurnCancellation,
        ) -> impl Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send
        {
            ready(Err(HeadlessTurnPortError::Tool))
        }
    }

    let mut provider = Provider::new(vec![
        Ok(vec![MessagePart::ToolCall {
            id: "invalid".into(),
            name: "git_read".into(),
            input: r#"{"operation":"diff"}"#.into(),
        }]),
        Ok(vec![MessagePart::Text("recovered".into())]),
    ]);
    let mut dispatcher = ToolDispatcher::default();
    let mut repository = Repository::default();

    let snapshot = block_on_ready(run_headless_turn(
        &mut provider,
        &mut InvalidInputGate,
        &mut PermissionResolver::default(),
        &mut dispatcher,
        &mut repository,
        &HeadlessTurnCancellation::new(),
    ))
    .expect("invalid tool arguments should be returned to the provider");

    assert!(dispatcher.calls.is_empty());
    assert!(snapshot.events().iter().any(|event| {
        matches!(
            event,
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id,
                is_error: true,
                ..
            }) if tool_call_id == "invalid"
        )
    }));
}

#[test]
fn a_denied_call_carries_the_gates_denial_facts() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let progress: TurnProgressSink = {
        let observed = Arc::clone(&observed);
        Arc::new(move |event| observed.lock().unwrap().push(event))
    };
    let mut provider = Provider::new(vec![
        Ok(vec![MessagePart::ToolCall {
            id: "call-1".into(),
            name: "write".into(),
            input: "{\"path\":\"secret.txt\"}".into(),
        }]),
        Ok(vec![MessagePart::Text("complete".into())]),
    ]);
    let mut gate = PermissionGate {
        decisions: vec![PermissionDecision::Deny],
        denial_facts: Some(ToolResultFacts::Write {
            path: FactPath::new("secret.txt"),
            outcome: ToolOutcome::Denied,
            written: None,
        }),
    };
    let mut resolver = PermissionResolver::default();
    let mut dispatcher = ToolDispatcher::default();
    let mut repository = Repository::default();

    block_on_ready(run_headless_turn_with_progress(
        &mut provider,
        &mut gate,
        &mut resolver,
        &mut dispatcher,
        &mut repository,
        &HeadlessTurnCancellation::new(),
        Some(&progress),
        None,
    ))
    .unwrap();

    let observed = observed.lock().unwrap();
    let facts = observed
        .iter()
        .find_map(|event| match event {
            TurnEvent::ToolResultFacts { facts, .. } => Some(facts.clone()),
            _ => None,
        })
        .expect("a denied call must still carry the gate's denial facts");

    assert_eq!(
        facts,
        ToolResultFacts::Write {
            path: FactPath::new("secret.txt"),
            outcome: ToolOutcome::Denied,
            written: None,
        }
    );
}

#[test]
fn a_gate_with_no_denial_facts_leaves_a_denied_call_pathless() {
    let mut provider = Provider::new(vec![
        Ok(vec![MessagePart::ToolCall {
            id: "call-1".into(),
            name: "write".into(),
            input: "{\"path\":\"secret.txt\"}".into(),
        }]),
        Ok(vec![MessagePart::Text("complete".into())]),
    ]);
    let mut gate = PermissionGate {
        decisions: vec![PermissionDecision::Deny],
        denial_facts: None,
    };
    let mut resolver = PermissionResolver::default();
    let mut dispatcher = ToolDispatcher::default();
    let mut repository = Repository::default();

    let snapshot = block_on_ready(run_headless_turn(
        &mut provider,
        &mut gate,
        &mut resolver,
        &mut dispatcher,
        &mut repository,
        &HeadlessTurnCancellation::new(),
    ))
    .expect("headless turn should complete");

    assert!(
        !snapshot
            .events()
            .iter()
            .any(|event| matches!(event, TurnEvent::ToolResultFacts { .. })),
        "a gate reporting no denial facts must not synthesize a facts event"
    );
}

#[test]
fn headless_turn_snapshot_omits_tool_result_facts() {
    let mut provider = Provider::new(vec![
        Ok(vec![MessagePart::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            input: "{\"command\":\"exit 2\"}".into(),
        }]),
        Ok(vec![MessagePart::Text("complete".into())]),
    ]);
    let mut gate = PermissionGate {
        decisions: vec![PermissionDecision::Allow],
        denial_facts: None,
    };
    let mut resolver = PermissionResolver::default();
    let mut dispatcher = ToolDispatcher {
        outputs: vec![Ok(HeadlessToolOutput::success("exit 2").with_facts(
            ToolResultFacts::Bash {
                outcome: ToolOutcome::Failed,
                exit_code: Some(2),
            },
        ))],
        ..ToolDispatcher::default()
    };
    let mut repository = Repository::default();

    let snapshot = block_on_ready(run_headless_turn(
        &mut provider,
        &mut gate,
        &mut resolver,
        &mut dispatcher,
        &mut repository,
        &HeadlessTurnCancellation::new(),
    ))
    .expect("headless turn should complete");

    assert!(
        !snapshot
            .events()
            .iter()
            .any(|event| matches!(event, TurnEvent::ToolResultFacts { .. })),
        "completed turn snapshot must omit live-only facts events"
    );
    assert_eq!(repository.snapshots, vec![snapshot]);
}

#[derive(Default)]
struct Provider {
    iterations: Vec<Result<Vec<MessagePart>, HeadlessTurnPortError>>,
    /// What the turn handed over for the next request, in the order it did.
    queued: Arc<Mutex<Vec<Message>>>,
}

impl Provider {
    fn new(iterations: Vec<Result<Vec<MessagePart>, HeadlessTurnPortError>>) -> Self {
        Self {
            iterations,
            queued: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl TurnProvider for Provider {
    fn queue_user_messages(&mut self, messages: Vec<Message>) -> Result<(), HeadlessTurnPortError> {
        self.queued.lock().unwrap().extend(messages);
        Ok(())
    }

    fn next_parts(
        &mut self,
        _events: &[TurnEvent],
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<Vec<MessagePart>, HeadlessTurnPortError>> + Send {
        ready(self.iterations.remove(0))
    }
}

#[derive(Default)]
struct PermissionGate {
    decisions: Vec<PermissionDecision>,
    denial_facts: Option<ToolResultFacts>,
}

impl HeadlessPermissionGate for PermissionGate {
    fn evaluate(
        &mut self,
        _call: &HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send {
        ready(Ok(self.decisions.remove(0)))
    }

    fn denial_facts(&self, _call: &HeadlessToolCall) -> Option<ToolResultFacts> {
        self.denial_facts.clone()
    }
}

#[derive(Default)]
struct PermissionResolver {
    decisions: Vec<PermissionDecision>,
}

impl HeadlessPermissionResolver for PermissionResolver {
    fn resolve(
        &mut self,
        _call: &HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send {
        ready(Ok(self.decisions.remove(0)))
    }
}

#[derive(Default)]
struct ToolDispatcher {
    outputs: Vec<Result<HeadlessToolOutput, HeadlessTurnPortError>>,
    calls: Vec<String>,
}

impl HeadlessToolDispatcher for ToolDispatcher {
    fn dispatch(
        &mut self,
        call: HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<HeadlessToolOutput, HeadlessTurnPortError>> + Send {
        self.calls.push(call.name);
        ready(self.outputs.remove(0))
    }
}

#[derive(Default)]
struct Repository {
    snapshots: Vec<CompletedTurnSnapshot>,
    failure: Option<CompletedTurnStoreError>,
}

impl CompletedTurnRepository for Repository {
    fn persist_completed_turn(
        &mut self,
        snapshot: CompletedTurnSnapshot,
    ) -> impl Future<Output = Result<(), CompletedTurnStoreError>> + Send {
        if let Some(error) = self.failure.clone() {
            return ready(Err(error));
        }

        self.snapshots.push(snapshot);
        ready(Ok(()))
    }
}

struct PendingUntilCancelled {
    cancellation: HeadlessTurnCancellation,
}

impl Future for PendingUntilCancelled {
    type Output = Result<Vec<MessagePart>, HeadlessTurnPortError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if self.cancellation.is_cancelled() {
            std::task::Poll::Ready(Err(HeadlessTurnPortError::Cancelled))
        } else {
            std::task::Poll::Pending
        }
    }
}

struct InFlightProvider {
    started: Arc<AtomicBool>,
}

impl TurnProvider for InFlightProvider {
    fn next_parts(
        &mut self,
        _events: &[TurnEvent],
        cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<Vec<MessagePart>, HeadlessTurnPortError>> + Send {
        self.started.store(true, Ordering::Release);
        PendingUntilCancelled {
            cancellation: cancellation.clone(),
        }
    }
}

fn block_on_ready<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

    match future.as_mut().poll(context) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("test ports must complete immediately"),
    }
}

#[test]
fn runs_ordered_provider_tool_iterations_and_persists_one_completed_snapshot() {
    let mut provider = Provider::new(vec![
        Ok(vec![
            MessagePart::Text("planning".into()),
            MessagePart::ToolCall {
                id: "ask".into(),
                name: "read".into(),
                input: "file.txt".into(),
            },
            MessagePart::ToolCall {
                id: "deny".into(),
                name: "write".into(),
                input: "file.txt".into(),
            },
            MessagePart::ToolCall {
                id: "allow".into(),
                name: "search".into(),
                input: "needle".into(),
            },
        ]),
        Ok(vec![MessagePart::Text("complete".into())]),
    ]);
    let mut gate = PermissionGate {
        decisions: vec![
            PermissionDecision::Ask,
            PermissionDecision::Deny,
            PermissionDecision::Allow,
        ],
        denial_facts: None,
    };
    let mut resolver = PermissionResolver {
        decisions: vec![PermissionDecision::Allow],
    };
    let mut dispatcher = ToolDispatcher {
        outputs: vec![
            Ok(HeadlessToolOutput::success("asked result")),
            Ok(HeadlessToolOutput::success("allowed result")),
        ],
        ..ToolDispatcher::default()
    };
    let mut repository = Repository::default();

    let snapshot = block_on_ready(run_headless_turn(
        &mut provider,
        &mut gate,
        &mut resolver,
        &mut dispatcher,
        &mut repository,
        &HeadlessTurnCancellation::new(),
    ))
    .expect("headless turn should complete");

    assert_eq!(repository.snapshots, vec![snapshot.clone()]);
    assert_eq!(provider.iterations.len(), 0);
    assert_eq!(dispatcher.calls, ["read", "search"]);
    assert_eq!(snapshot.events().len(), 17);
    assert_eq!(
        snapshot.events()[10],
        TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "ask".into(),
            content: "asked result".into(),
            is_error: false,
        })
    );
    assert_eq!(
        snapshot.events()[11],
        TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "deny".into(),
            content: "permission denied".into(),
            is_error: true,
        })
    );
    assert_eq!(
        snapshot.events()[12],
        TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "allow".into(),
            content: "allowed result".into(),
            is_error: false,
        })
    );
    assert_eq!(
        snapshot.events(),
        repository.snapshots[0].events(),
        "the persisted turn must be the completed ordered event stream"
    );
}

#[test]
fn cancellation_provider_tool_and_store_failures_are_typed_and_never_persist_partial_turns() {
    let mut cancelled_provider = Provider::new(vec![Ok(vec![MessagePart::Text("ignored".into())])]);
    let mut cancelled_repository = Repository::default();
    let cancelled = block_on_ready(run_headless_turn(
        &mut cancelled_provider,
        &mut PermissionGate::default(),
        &mut PermissionResolver::default(),
        &mut ToolDispatcher::default(),
        &mut cancelled_repository,
        &{
            let cancellation = HeadlessTurnCancellation::new();
            cancellation.cancel();
            cancellation
        },
    ));
    assert_eq!(cancelled, Err(agens_core::HeadlessTurnError::Cancelled));
    assert!(cancelled_repository.snapshots.is_empty());

    let mut provider_failure = Provider::new(vec![Err(HeadlessTurnPortError::Provider)]);
    let mut provider_repository = Repository::default();
    let provider_result = block_on_ready(run_headless_turn(
        &mut provider_failure,
        &mut PermissionGate::default(),
        &mut PermissionResolver::default(),
        &mut ToolDispatcher::default(),
        &mut provider_repository,
        &HeadlessTurnCancellation::new(),
    ));
    assert_eq!(
        provider_result,
        Err(agens_core::HeadlessTurnError::Provider)
    );
    assert!(provider_repository.snapshots.is_empty());

    let mut tool_provider = Provider::new(vec![Ok(vec![MessagePart::ToolCall {
        id: "tool".into(),
        name: "read".into(),
        input: "file.txt".into(),
    }])]);
    let mut tool_repository = Repository::default();
    let tool_result = block_on_ready(run_headless_turn(
        &mut tool_provider,
        &mut PermissionGate {
            decisions: vec![PermissionDecision::Allow],
            denial_facts: None,
        },
        &mut PermissionResolver::default(),
        &mut ToolDispatcher {
            outputs: vec![Err(HeadlessTurnPortError::Tool)],
            ..ToolDispatcher::default()
        },
        &mut tool_repository,
        &HeadlessTurnCancellation::new(),
    ));
    assert_eq!(tool_result, Err(agens_core::HeadlessTurnError::Tool));
    assert!(tool_repository.snapshots.is_empty());

    let mut store_provider = Provider::new(vec![Ok(vec![MessagePart::Text("complete".into())])]);
    let mut store_repository = Repository {
        failure: Some(CompletedTurnStoreError::new("database unavailable")),
        ..Repository::default()
    };
    let store_result = block_on_ready(run_headless_turn(
        &mut store_provider,
        &mut PermissionGate::default(),
        &mut PermissionResolver::default(),
        &mut ToolDispatcher::default(),
        &mut store_repository,
        &HeadlessTurnCancellation::new(),
    ));
    assert_eq!(store_result, Err(agens_core::HeadlessTurnError::Store));
    assert!(store_repository.snapshots.is_empty());
}

#[test]
fn authentication_port_failures_are_typed_failed_and_never_persist_partial_turns() {
    let mut provider = Provider::new(vec![Err(HeadlessTurnPortError::Authentication)]);
    let mut repository = Repository::default();

    let result = block_on_ready(run_headless_turn(
        &mut provider,
        &mut PermissionGate::default(),
        &mut PermissionResolver::default(),
        &mut ToolDispatcher::default(),
        &mut repository,
        &HeadlessTurnCancellation::new(),
    ));

    assert_eq!(result, Err(HeadlessTurnError::Authentication));
    assert!(repository.snapshots.is_empty());
}

#[test]
fn cancellation_reaches_an_in_flight_provider_and_suppresses_persistence() {
    let started = Arc::new(AtomicBool::new(false));
    let cancellation = HeadlessTurnCancellation::new();
    let canceller = cancellation.clone();
    let mut provider = InFlightProvider {
        started: Arc::clone(&started),
    };
    let mut gate = PermissionGate::default();
    let mut resolver = PermissionResolver::default();
    let mut dispatcher = ToolDispatcher::default();
    let mut repository = Repository::default();

    let result = {
        let mut turn = std::pin::pin!(run_headless_turn(
            &mut provider,
            &mut gate,
            &mut resolver,
            &mut dispatcher,
            &mut repository,
            &cancellation,
        ));
        let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

        assert!(matches!(
            turn.as_mut().poll(context),
            std::task::Poll::Pending
        ));
        assert!(started.load(Ordering::Acquire));
        canceller.cancel();
        turn.as_mut().poll(context)
    };

    assert_eq!(
        result,
        std::task::Poll::Ready(Err(agens_core::HeadlessTurnError::Cancelled))
    );
    assert!(repository.snapshots.is_empty());
}

#[test]
fn expired_deadline_is_a_distinct_failure_and_never_persists_a_partial_turn() {
    let mut provider = Provider::new(vec![Ok(vec![MessagePart::Text("late".into())])]);
    let mut repository = Repository::default();
    let cancellation = HeadlessTurnCancellation::with_deadline(std::time::Duration::ZERO);

    let result = block_on_ready(run_headless_turn(
        &mut provider,
        &mut PermissionGate::default(),
        &mut PermissionResolver::default(),
        &mut ToolDispatcher::default(),
        &mut repository,
        &cancellation,
    ));

    assert_eq!(result, Err(agens_core::HeadlessTurnError::TimedOut));
    assert!(repository.snapshots.is_empty());
}

#[test]
fn permission_evaluation_distinguishes_unresolved_asks_without_exposing_tool_input() {
    let secret_input = "credential=do-not-expose";
    let mut provider = Provider::new(vec![Ok(vec![MessagePart::ToolCall {
        id: "permission-needed".into(),
        name: "read".into(),
        input: secret_input.into(),
    }])]);
    let mut repository = Repository::default();

    let result = block_on_ready(run_headless_turn(
        &mut provider,
        &mut PermissionGate {
            decisions: vec![PermissionDecision::Ask],
            denial_facts: None,
        },
        &mut PermissionResolver {
            decisions: vec![PermissionDecision::Ask],
        },
        &mut ToolDispatcher::default(),
        &mut repository,
        &HeadlessTurnCancellation::new(),
    ));

    assert_eq!(result, Err(HeadlessTurnError::PermissionRequired));
    assert!(
        !HeadlessTurnError::PermissionRequired
            .to_string()
            .contains(secret_input)
    );
    assert!(provider.iterations.is_empty());
    assert!(repository.snapshots.is_empty());
}

#[test]
fn denied_permissions_emit_sanitized_tool_results_and_continue_without_dispatch() {
    for (gate_decision, resolver_decision) in [
        (PermissionDecision::Deny, None),
        (PermissionDecision::Ask, Some(PermissionDecision::Deny)),
    ] {
        let mut provider = Provider::new(vec![
            Ok(vec![MessagePart::ToolCall {
                id: "denied".into(),
                name: "read".into(),
                input: "credential=do-not-expose".into(),
            }]),
            Ok(vec![MessagePart::Text("complete".into())]),
        ]);
        let mut repository = Repository::default();
        let mut resolver = PermissionResolver {
            decisions: resolver_decision.into_iter().collect(),
        };

        let snapshot = block_on_ready(run_headless_turn(
            &mut provider,
            &mut PermissionGate {
                decisions: vec![gate_decision],
                denial_facts: None,
            },
            &mut resolver,
            &mut ToolDispatcher::default(),
            &mut repository,
            &HeadlessTurnCancellation::new(),
        ))
        .expect("denied tool call should let the provider continue");

        assert!(provider.iterations.is_empty());
        assert_eq!(repository.snapshots, vec![snapshot.clone()]);
        assert!(
            snapshot
                .events()
                .contains(&TurnEvent::ToolResult(MessagePart::ToolResult {
                    tool_call_id: "denied".into(),
                    content: "permission denied".into(),
                    is_error: true,
                }))
        );
    }
}

#[test]
fn gate_permission_errors_still_end_the_turn() {
    struct FailingGate;

    impl HeadlessPermissionGate for FailingGate {
        fn evaluate(
            &mut self,
            _call: &HeadlessToolCall,
            _cancellation: &HeadlessTurnCancellation,
        ) -> impl Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send
        {
            ready(Err(HeadlessTurnPortError::Permission))
        }
    }

    let mut provider = Provider::new(vec![Ok(vec![MessagePart::ToolCall {
        id: "permission-error".into(),
        name: "read".into(),
        input: "credential=do-not-expose".into(),
    }])]);
    let mut repository = Repository::default();
    let mut failing_gate = FailingGate;
    let mut resolver = PermissionResolver::default();

    let result = block_on_ready(run_headless_turn(
        &mut provider,
        &mut failing_gate,
        &mut resolver,
        &mut ToolDispatcher::default(),
        &mut repository,
        &HeadlessTurnCancellation::new(),
    ));

    assert_eq!(result, Err(HeadlessTurnError::PermissionEvaluation));
    assert!(repository.snapshots.is_empty());
}

#[test]
fn resolver_permission_errors_refuse_the_call_and_let_the_agent_continue() {
    struct FailingResolver;

    impl HeadlessPermissionResolver for FailingResolver {
        fn resolve(
            &mut self,
            _call: &HeadlessToolCall,
            _cancellation: &HeadlessTurnCancellation,
        ) -> impl Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send
        {
            ready(Err(HeadlessTurnPortError::Permission))
        }
    }

    let mut provider = Provider::new(vec![
        Ok(vec![MessagePart::ToolCall {
            id: "permission-error".into(),
            name: "read".into(),
            input: "credential=do-not-expose".into(),
        }]),
        Ok(vec![MessagePart::Text(
            "recovered after tool refusal".into(),
        )]),
    ]);
    let mut repository = Repository::default();
    let mut gate = PermissionGate {
        decisions: vec![PermissionDecision::Ask],
        denial_facts: None,
    };
    let mut failing_resolver = FailingResolver;

    let snapshot = block_on_ready(run_headless_turn(
        &mut provider,
        &mut gate,
        &mut failing_resolver,
        &mut ToolDispatcher::default(),
        &mut repository,
        &HeadlessTurnCancellation::new(),
    ))
    .expect("approval-path failure must not abort the turn");

    let tool_results: Vec<_> = snapshot
        .events()
        .iter()
        .filter_map(|event| match event {
            TurnEvent::ToolResult(MessagePart::ToolResult {
                content, is_error, ..
            }) => Some((content.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1, "{tool_results:?}");
    assert!(tool_results[0].1, "refusal is an error tool result");
    assert!(
        tool_results[0].0.contains("permission approval"),
        "model-visible refusal: {}",
        tool_results[0].0
    );
    assert!(
        snapshot.events().iter().any(|event| matches!(
            event,
            TurnEvent::ProviderPart(MessagePart::Text(text))
                if text.contains("recovered after tool refusal")
        )),
        "agent must get another iteration: {:?}",
        snapshot.events()
    );
    assert_eq!(repository.snapshots.len(), 1);
}

#[test]
fn an_expired_unattended_permission_question_names_the_cause_to_the_agent() {
    struct ExpiredResolver;

    impl HeadlessPermissionResolver for ExpiredResolver {
        fn resolve(
            &mut self,
            _call: &HeadlessToolCall,
            _cancellation: &HeadlessTurnCancellation,
        ) -> impl Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send
        {
            ready(Err(HeadlessTurnPortError::PermissionExpired))
        }
    }

    let mut provider = Provider::new(vec![
        Ok(vec![MessagePart::ToolCall {
            id: "expired-question".into(),
            name: "write".into(),
            input: r#"{"path":"notes.md","contents":"hi"}"#.into(),
        }]),
        Ok(vec![MessagePart::Text("recovered".into())]),
    ]);
    let mut repository = Repository::default();
    let mut gate = PermissionGate {
        decisions: vec![PermissionDecision::Ask],
        denial_facts: None,
    };

    let snapshot = block_on_ready(run_headless_turn(
        &mut provider,
        &mut gate,
        &mut ExpiredResolver,
        &mut ToolDispatcher::default(),
        &mut repository,
        &HeadlessTurnCancellation::new(),
    ))
    .expect("an expired question must be a recoverable tool refusal");

    assert!(snapshot.events().iter().any(|event| matches!(
        event,
        TurnEvent::ToolResult(MessagePart::ToolResult { content, is_error: true, .. })
            if content.contains("unattended permission question expired")
    )));
}

#[test]
fn max_iterations_stops_before_a_second_provider_request_without_persisting() {
    let mut provider = Provider::new(vec![
        Ok(vec![MessagePart::ToolCall {
            id: "continue".into(),
            name: "read".into(),
            input: "file.txt".into(),
        }]),
        Ok(vec![MessagePart::Text("must not be requested".into())]),
    ]);
    let mut repository = Repository::default();

    let result = block_on_ready(run_headless_turn_with_max_iterations(
        &mut provider,
        &mut PermissionGate {
            decisions: vec![PermissionDecision::Allow],
            denial_facts: None,
        },
        &mut PermissionResolver::default(),
        &mut ToolDispatcher {
            outputs: vec![Ok(HeadlessToolOutput::success("read result"))],
            ..ToolDispatcher::default()
        },
        &mut repository,
        &HeadlessTurnCancellation::new(),
        1,
    ));

    assert_eq!(result, Err(HeadlessTurnError::MaxIterations));
    assert_eq!(provider.iterations.len(), 1);
    assert!(repository.snapshots.is_empty());
}

#[test]
fn preflights_a_provider_batch_before_sequential_dispatch_and_continues_denials() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut provider = Provider::new(vec![
        Ok(vec![
            tool_call("allow", "read"),
            tool_call("deny", "write"),
            tool_call("after", "search"),
        ]),
        Ok(vec![MessagePart::Text("complete".into())]),
    ]);
    let mut gate = RecordingGate {
        decisions: vec![
            PermissionDecision::Ask,
            PermissionDecision::Deny,
            PermissionDecision::Allow,
        ],
        observed: Arc::clone(&observed),
    };
    let mut resolver = RecordingResolver {
        decisions: vec![PermissionDecision::Allow],
        observed: Arc::clone(&observed),
    };
    let mut dispatcher = RecordingDispatcher {
        outputs: vec![
            Ok(HeadlessToolOutput::success("read result")),
            Ok(HeadlessToolOutput::success("search result")),
        ],
        observed: Arc::clone(&observed),
        cancellation: None,
    };
    let mut repository = Repository::default();

    let snapshot = block_on_ready(run_headless_turn(
        &mut provider,
        &mut gate,
        &mut resolver,
        &mut dispatcher,
        &mut repository,
        &HeadlessTurnCancellation::new(),
    ))
    .expect("the allowed calls should complete after the full preflight");

    assert_eq!(
        *observed.lock().unwrap(),
        [
            "gate:read",
            "resolve:read",
            "gate:write",
            "gate:search",
            "dispatch:read",
            "dispatch:search",
        ]
    );
    assert_eq!(
        snapshot.events(),
        repository.snapshots[0].events(),
        "the completed batch must remain persistable"
    );
    assert!(
        snapshot
            .events()
            .contains(&TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "deny".into(),
                content: "permission denied".into(),
                is_error: true,
            }))
    );
}

#[test]
fn cancellation_during_preflight_runs_nothing_and_during_execution_keeps_completed_results() {
    let preflight_cancellation = HeadlessTurnCancellation::new();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut provider = Provider::new(vec![Ok(vec![
        tool_call("first", "read"),
        tool_call("second", "search"),
    ])]);
    let mut gate = CancellingGate {
        decisions: vec![PermissionDecision::Allow, PermissionDecision::Allow],
        cancellation: preflight_cancellation.clone(),
        observed: Arc::clone(&observed),
    };
    let mut repository = Repository::default();

    let preflight_result = block_on_ready(run_headless_turn(
        &mut provider,
        &mut gate,
        &mut PermissionResolver::default(),
        &mut RecordingDispatcher {
            outputs: Vec::new(),
            observed: Arc::clone(&observed),
            cancellation: None,
        },
        &mut repository,
        &preflight_cancellation,
    ));

    assert_eq!(preflight_result, Err(HeadlessTurnError::Cancelled));
    assert_eq!(*observed.lock().unwrap(), ["gate:read"]);
    assert!(repository.snapshots.is_empty());

    let execution_cancellation = HeadlessTurnCancellation::new();
    let mut provider = Provider::new(vec![Ok(vec![
        tool_call("first", "read"),
        tool_call("second", "search"),
    ])]);
    let mut dispatcher = RecordingDispatcher {
        outputs: vec![Ok(HeadlessToolOutput::success("first result"))],
        observed: Arc::new(Mutex::new(Vec::new())),
        cancellation: Some(execution_cancellation.clone()),
    };
    let mut repository = Repository::default();
    let progress = Arc::new(Mutex::new(Vec::new()));
    let progress_sink: TurnProgressSink = {
        let progress = Arc::clone(&progress);
        Arc::new(move |event| progress.lock().unwrap().push(event))
    };

    let execution_result = block_on_ready(run_headless_turn_with_progress(
        &mut provider,
        &mut PermissionGate {
            decisions: vec![PermissionDecision::Allow, PermissionDecision::Allow],
            denial_facts: None,
        },
        &mut PermissionResolver::default(),
        &mut dispatcher,
        &mut repository,
        &execution_cancellation,
        Some(&progress_sink),
        None,
    ));

    assert_eq!(execution_result, Err(HeadlessTurnError::Cancelled));
    assert_eq!(dispatcher.calls(), ["read"]);
    assert!(repository.snapshots.is_empty());
    assert!(
        progress
            .lock()
            .unwrap()
            .contains(&TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "first".into(),
                content: "first result".into(),
                is_error: false,
            }))
    );
}

fn tool_call(id: &str, name: &str) -> MessagePart {
    MessagePart::ToolCall {
        id: id.into(),
        name: name.into(),
        input: "{}".into(),
    }
}

struct RecordingGate {
    decisions: Vec<PermissionDecision>,
    observed: Arc<Mutex<Vec<String>>>,
}

impl HeadlessPermissionGate for RecordingGate {
    fn evaluate(
        &mut self,
        call: &HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send {
        self.observed
            .lock()
            .unwrap()
            .push(format!("gate:{}", call.name));
        ready(Ok(self.decisions.remove(0)))
    }
}

struct CancellingGate {
    decisions: Vec<PermissionDecision>,
    cancellation: HeadlessTurnCancellation,
    observed: Arc<Mutex<Vec<String>>>,
}

impl HeadlessPermissionGate for CancellingGate {
    fn evaluate(
        &mut self,
        call: &HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send {
        self.observed
            .lock()
            .unwrap()
            .push(format!("gate:{}", call.name));
        self.cancellation.cancel();
        ready(Ok(self.decisions.remove(0)))
    }
}

struct RecordingResolver {
    decisions: Vec<PermissionDecision>,
    observed: Arc<Mutex<Vec<String>>>,
}

impl HeadlessPermissionResolver for RecordingResolver {
    fn resolve(
        &mut self,
        call: &HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send {
        self.observed
            .lock()
            .unwrap()
            .push(format!("resolve:{}", call.name));
        ready(Ok(self.decisions.remove(0)))
    }
}

struct RecordingDispatcher {
    outputs: Vec<Result<HeadlessToolOutput, HeadlessTurnPortError>>,
    observed: Arc<Mutex<Vec<String>>>,
    cancellation: Option<HeadlessTurnCancellation>,
}

impl RecordingDispatcher {
    fn calls(&self) -> Vec<String> {
        self.observed
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| event.strip_prefix("dispatch:").map(ToOwned::to_owned))
            .collect()
    }
}

impl HeadlessToolDispatcher for RecordingDispatcher {
    fn dispatch(
        &mut self,
        call: HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<HeadlessToolOutput, HeadlessTurnPortError>> + Send {
        self.observed
            .lock()
            .unwrap()
            .push(format!("dispatch:{}", call.name));
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
        ready(self.outputs.remove(0))
    }
}

/// Input handed to a running turn lands after the tool batch it interrupted,
/// as its own event, and the turn keeps going.
#[test]
fn a_running_turn_collects_input_after_the_tool_batch() {
    struct OnceInbox(Option<PendingIntraTurnInput>);

    impl HeadlessIntraTurnInbox for OnceInbox {
        fn drain(
            &mut self,
        ) -> impl std::future::Future<
            Output = Result<Vec<PendingIntraTurnInput>, HeadlessTurnPortError>,
        > + Send {
            std::future::ready(Ok(self.0.take().into_iter().collect()))
        }
    }

    let mut provider = Provider::new(vec![
        Ok(vec![MessagePart::ToolCall {
            id: "call-1".into(),
            name: "git_read".into(),
            input: r#"{"operation":"diff"}"#.into(),
        }]),
        Ok(vec![MessagePart::Text("acknowledged".into())]),
    ]);
    let mut inbox = OnceInbox(Some(PendingIntraTurnInput {
        source: IntraTurnInputSource::Supervisor,
        text: "prefer the manifest".into(),
    }));

    let snapshot = block_on_ready(run_headless_turn_with_inbox(
        &mut provider,
        &mut PermissionGate {
            decisions: vec![PermissionDecision::Allow],
            denial_facts: None,
        },
        &mut PermissionResolver::default(),
        &mut ToolDispatcher {
            outputs: vec![Ok(HeadlessToolOutput::success("diff"))],
            calls: Vec::new(),
        },
        &mut Repository::default(),
        &HeadlessTurnCancellation::new(),
        None,
        None,
        None,
        &mut inbox,
    ))
    .expect("the turn completes after collecting input");

    let events = snapshot.events();
    let input_at = events
        .iter()
        .position(|event| matches!(event, TurnEvent::IntraTurnInput { .. }))
        .expect("the input is recorded");
    let result_at = events
        .iter()
        .position(|event| matches!(event, TurnEvent::ToolResult(_)))
        .expect("the tool ran");

    assert!(input_at > result_at, "{events:?}");
    assert_eq!(
        events[input_at],
        TurnEvent::IntraTurnInput {
            source: IntraTurnInputSource::Supervisor,
            text: "prefer the manifest".into(),
        }
    );
}

/// Recording the steer is not delivering it. The point of the tool-call grain
/// is that the message lands before the next request goes out, so the turn has
/// to hand it to the provider as well as write it down — otherwise the model
/// only reads it a whole turn later, which is the wait this grain exists to
/// avoid.
#[test]
fn collected_input_reaches_the_provider_before_the_next_request() {
    struct OnceInbox(Option<PendingIntraTurnInput>);

    impl HeadlessIntraTurnInbox for OnceInbox {
        fn drain(
            &mut self,
        ) -> impl std::future::Future<
            Output = Result<Vec<PendingIntraTurnInput>, HeadlessTurnPortError>,
        > + Send {
            std::future::ready(Ok(self.0.take().into_iter().collect()))
        }
    }

    let mut provider = Provider::new(vec![
        Ok(vec![MessagePart::ToolCall {
            id: "call-1".into(),
            name: "git_read".into(),
            input: r#"{"operation":"diff"}"#.into(),
        }]),
        Ok(vec![MessagePart::Text("acknowledged".into())]),
    ]);
    let queued = Arc::clone(&provider.queued);
    let mut inbox = OnceInbox(Some(PendingIntraTurnInput {
        source: IntraTurnInputSource::Supervisor,
        text: "prefer the manifest".into(),
    }));

    block_on_ready(run_headless_turn_with_inbox(
        &mut provider,
        &mut PermissionGate {
            decisions: vec![PermissionDecision::Allow],
            denial_facts: None,
        },
        &mut PermissionResolver::default(),
        &mut ToolDispatcher {
            outputs: vec![Ok(HeadlessToolOutput::success("diff"))],
            calls: Vec::new(),
        },
        &mut Repository::default(),
        &HeadlessTurnCancellation::new(),
        None,
        None,
        None,
        &mut inbox,
    ))
    .expect("the turn completes after collecting input");

    let queued = queued.lock().unwrap();
    assert_eq!(
        queued
            .iter()
            .map(|message| (message.role, message.parts.clone()))
            .collect::<Vec<_>>(),
        vec![(
            Role::Supervisor,
            vec![MessagePart::Text("prefer the manifest".into())]
        )],
        "the supervisor's message is handed over as its own speaker"
    );
}

/// An empty inbox is the common case and must cost the turn nothing.
#[test]
fn an_empty_inbox_leaves_a_turn_byte_for_byte_unchanged() {
    struct EmptyInbox;

    impl HeadlessIntraTurnInbox for EmptyInbox {
        fn drain(
            &mut self,
        ) -> impl std::future::Future<
            Output = Result<Vec<PendingIntraTurnInput>, HeadlessTurnPortError>,
        > + Send {
            std::future::ready(Ok(Vec::new()))
        }
    }

    let iterations = || {
        vec![
            Ok(vec![MessagePart::ToolCall {
                id: "call-1".into(),
                name: "git_read".into(),
                input: r#"{"operation":"diff"}"#.into(),
            }]),
            Ok(vec![MessagePart::Text("done".into())]),
        ]
    };

    let with_inbox = block_on_ready(run_headless_turn_with_inbox(
        &mut Provider::new(iterations()),
        &mut PermissionGate {
            decisions: vec![PermissionDecision::Allow],
            denial_facts: None,
        },
        &mut PermissionResolver::default(),
        &mut ToolDispatcher {
            outputs: vec![Ok(HeadlessToolOutput::success("diff"))],
            calls: Vec::new(),
        },
        &mut Repository::default(),
        &HeadlessTurnCancellation::new(),
        None,
        None,
        None,
        &mut EmptyInbox,
    ))
    .expect("the turn completes");
    let without_inbox = block_on_ready(run_headless_turn(
        &mut Provider::new(iterations()),
        &mut PermissionGate {
            decisions: vec![PermissionDecision::Allow],
            denial_facts: None,
        },
        &mut PermissionResolver::default(),
        &mut ToolDispatcher {
            outputs: vec![Ok(HeadlessToolOutput::success("diff"))],
            calls: Vec::new(),
        },
        &mut Repository::default(),
        &HeadlessTurnCancellation::new(),
    ))
    .expect("the turn completes");

    assert_eq!(with_inbox.events(), without_inbox.events());
}

/// The in-process steering queue is an inbox like any other: a message pushed
/// while the turn runs lands after the tool batch, is recorded as intra-turn
/// input, and is handed to the provider before the next request goes out.
#[test]
fn a_steering_queue_message_reaches_the_provider_after_a_tool_batch() {
    let steering = IntraTurnSteeringQueue::default();
    steering.push(7, IntraTurnInputSource::Human, "focus on the tests".into());

    let mut provider = Provider::new(vec![
        Ok(vec![MessagePart::ToolCall {
            id: "call-1".into(),
            name: "git_read".into(),
            input: r#"{"operation":"diff"}"#.into(),
        }]),
        Ok(vec![MessagePart::Text("acknowledged".into())]),
    ]);
    let queued = Arc::clone(&provider.queued);

    let mut inbox = steering.clone();
    let snapshot = block_on_ready(run_headless_turn_with_inbox(
        &mut provider,
        &mut PermissionGate {
            decisions: vec![PermissionDecision::Allow],
            denial_facts: None,
        },
        &mut PermissionResolver::default(),
        &mut ToolDispatcher {
            outputs: vec![Ok(HeadlessToolOutput::success("diff"))],
            calls: Vec::new(),
        },
        &mut Repository::default(),
        &HeadlessTurnCancellation::new(),
        None,
        None,
        None,
        &mut inbox,
    ))
    .expect("the turn completes after collecting the steer");

    let events = snapshot.events();
    let input_at = events
        .iter()
        .position(|event| matches!(event, TurnEvent::IntraTurnInput { .. }))
        .expect("the steer is recorded");
    let result_at = events
        .iter()
        .position(|event| matches!(event, TurnEvent::ToolResult(_)))
        .expect("the tool ran");
    assert!(input_at > result_at, "{events:?}");
    assert_eq!(
        events[input_at],
        TurnEvent::IntraTurnInput {
            source: IntraTurnInputSource::Human,
            text: "focus on the tests".into(),
        }
    );

    assert_eq!(
        *queued.lock().unwrap(),
        vec![Message {
            role: Role::User,
            parts: vec![MessagePart::Text("focus on the tests".into())],
        }]
    );
}

/// A withdrawn steer never reaches the turn, and clearing the queue empties
/// every remaining entry at once.
#[test]
fn withdrawn_and_cleared_steering_messages_are_never_delivered() {
    let steering = IntraTurnSteeringQueue::default();
    steering.push(1, IntraTurnInputSource::Human, "first".into());
    steering.push(2, IntraTurnInputSource::Human, "second".into());

    assert!(steering.withdraw(1));
    assert!(!steering.withdraw(1));

    steering.push(3, IntraTurnInputSource::Human, "third".into());
    steering.clear();

    let mut inbox = steering;
    let drained = block_on_ready(HeadlessIntraTurnInbox::drain(&mut inbox))
        .expect("an empty steering queue drains cleanly");
    assert!(drained.is_empty());
}
