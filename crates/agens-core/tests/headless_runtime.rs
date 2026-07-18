use std::future::{Future, ready};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agens_core::{
    CompletedTurnRepository, CompletedTurnSnapshot, CompletedTurnStoreError,
    HeadlessPermissionGate, HeadlessPermissionResolver, HeadlessToolCall, HeadlessToolDispatcher,
    HeadlessToolOutput, HeadlessTurnCancellation, HeadlessTurnError, HeadlessTurnPortError,
    MessagePart, PermissionDecision, TurnEvent, TurnProgressSink, TurnProvider, TurnState,
    run_headless_turn, run_headless_turn_with_max_iterations, run_headless_turn_with_progress,
};

#[test]
fn progress_sink_receives_state_and_provider_events_before_completion() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let progress: TurnProgressSink = {
        let observed = Arc::clone(&observed);
        Arc::new(move |event| observed.lock().unwrap().push(event))
    };
    let mut provider = Provider {
        iterations: vec![Ok(vec![MessagePart::Text("visible early".into())])],
    };
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

#[derive(Default)]
struct Provider {
    iterations: Vec<Result<Vec<MessagePart>, HeadlessTurnPortError>>,
}

impl TurnProvider for Provider {
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
}

impl HeadlessPermissionGate for PermissionGate {
    fn evaluate(
        &mut self,
        _call: &HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send {
        ready(Ok(self.decisions.remove(0)))
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
}

impl HeadlessToolDispatcher for ToolDispatcher {
    fn dispatch(
        &mut self,
        _call: HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<HeadlessToolOutput, HeadlessTurnPortError>> + Send {
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
    let mut provider = Provider {
        iterations: vec![
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
        ],
    };
    let mut gate = PermissionGate {
        decisions: vec![
            PermissionDecision::Ask,
            PermissionDecision::Deny,
            PermissionDecision::Allow,
        ],
    };
    let mut resolver = PermissionResolver {
        decisions: vec![PermissionDecision::Allow],
    };
    let mut dispatcher = ToolDispatcher {
        outputs: vec![
            Ok(HeadlessToolOutput::success("asked result")),
            Ok(HeadlessToolOutput::success("allowed result")),
        ],
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
    let mut cancelled_provider = Provider {
        iterations: vec![Ok(vec![MessagePart::Text("ignored".into())])],
    };
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

    let mut provider_failure = Provider {
        iterations: vec![Err(HeadlessTurnPortError::Provider)],
    };
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

    let mut tool_provider = Provider {
        iterations: vec![Ok(vec![MessagePart::ToolCall {
            id: "tool".into(),
            name: "read".into(),
            input: "file.txt".into(),
        }])],
    };
    let mut tool_repository = Repository::default();
    let tool_result = block_on_ready(run_headless_turn(
        &mut tool_provider,
        &mut PermissionGate {
            decisions: vec![PermissionDecision::Allow],
        },
        &mut PermissionResolver::default(),
        &mut ToolDispatcher {
            outputs: vec![Err(HeadlessTurnPortError::Tool)],
        },
        &mut tool_repository,
        &HeadlessTurnCancellation::new(),
    ));
    assert_eq!(tool_result, Err(agens_core::HeadlessTurnError::Tool));
    assert!(tool_repository.snapshots.is_empty());

    let mut store_provider = Provider {
        iterations: vec![Ok(vec![MessagePart::Text("complete".into())])],
    };
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
    let mut provider = Provider {
        iterations: vec![Err(HeadlessTurnPortError::Authentication)],
    };
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
    let mut provider = Provider {
        iterations: vec![Ok(vec![MessagePart::Text("late".into())])],
    };
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
fn unresolved_permission_ask_fails_closed_without_exposing_tool_input() {
    let secret_input = "credential=do-not-expose";
    let mut provider = Provider {
        iterations: vec![Ok(vec![MessagePart::ToolCall {
            id: "permission-needed".into(),
            name: "read".into(),
            input: secret_input.into(),
        }])],
    };
    let mut repository = Repository::default();

    let result = block_on_ready(run_headless_turn(
        &mut provider,
        &mut PermissionGate {
            decisions: vec![PermissionDecision::Ask],
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
        let mut provider = Provider {
            iterations: vec![
                Ok(vec![MessagePart::ToolCall {
                    id: "denied".into(),
                    name: "read".into(),
                    input: "credential=do-not-expose".into(),
                }]),
                Ok(vec![MessagePart::Text("complete".into())]),
            ],
        };
        let mut repository = Repository::default();
        let mut resolver = PermissionResolver {
            decisions: resolver_decision.into_iter().collect(),
        };

        let snapshot = block_on_ready(run_headless_turn(
            &mut provider,
            &mut PermissionGate {
                decisions: vec![gate_decision],
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
fn permission_port_errors_remain_distinct_from_unresolved_asks() {
    for resolver_error in [false, true] {
        let mut provider = Provider {
            iterations: vec![Ok(vec![MessagePart::ToolCall {
                id: "permission-error".into(),
                name: "read".into(),
                input: "file.txt".into(),
            }])],
        };
        let mut repository = Repository::default();
        let mut gate = PermissionGate {
            decisions: vec![PermissionDecision::Ask],
        };
        let mut resolver = PermissionResolver {
            decisions: vec![PermissionDecision::Allow],
        };

        let result = if resolver_error {
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

            let mut failing_resolver = FailingResolver;
            block_on_ready(run_headless_turn(
                &mut provider,
                &mut gate,
                &mut failing_resolver,
                &mut ToolDispatcher::default(),
                &mut repository,
                &HeadlessTurnCancellation::new(),
            ))
        } else {
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

            let mut failing_gate = FailingGate;
            block_on_ready(run_headless_turn(
                &mut provider,
                &mut failing_gate,
                &mut resolver,
                &mut ToolDispatcher::default(),
                &mut repository,
                &HeadlessTurnCancellation::new(),
            ))
        };

        assert_eq!(result, Err(HeadlessTurnError::Permission));
        assert!(repository.snapshots.is_empty());
    }
}

#[test]
fn max_iterations_stops_before_a_second_provider_request_without_persisting() {
    let mut provider = Provider {
        iterations: vec![
            Ok(vec![MessagePart::ToolCall {
                id: "continue".into(),
                name: "read".into(),
                input: "file.txt".into(),
            }]),
            Ok(vec![MessagePart::Text("must not be requested".into())]),
        ],
    };
    let mut repository = Repository::default();

    let result = block_on_ready(run_headless_turn_with_max_iterations(
        &mut provider,
        &mut PermissionGate {
            decisions: vec![PermissionDecision::Allow],
        },
        &mut PermissionResolver::default(),
        &mut ToolDispatcher {
            outputs: vec![Ok(HeadlessToolOutput::success("read result"))],
        },
        &mut repository,
        &HeadlessTurnCancellation::new(),
        1,
    ));

    assert_eq!(result, Err(HeadlessTurnError::MaxIterations));
    assert_eq!(provider.iterations.len(), 1);
    assert!(repository.snapshots.is_empty());
}
