use std::future::{Future, ready};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agens_core::{
    CompletedTurnRepository, CompletedTurnSnapshot, CompletedTurnStoreError,
    HeadlessPermissionGate, HeadlessPermissionResolver, HeadlessToolCall, HeadlessToolDispatcher,
    HeadlessToolOutput, HeadlessTurnCancellation, HeadlessTurnPortError, MessagePart,
    PermissionDecision, TurnEvent, TurnProvider, run_headless_turn,
};

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
