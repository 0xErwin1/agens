use std::{
    collections::BTreeMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

use agens_core::{HeadlessTurnCancellation, TurnState, Usage};

use crate::DiffLine;

const RETRY_QUANTUM: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiPermissionReply {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
    Cancelled,
    DeadlineExpired,
}

pub struct TuiPermissionRequest {
    id: u64,
    tool: String,
    target: String,
}

impl TuiPermissionRequest {
    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn details(&self) -> (&str, &str) {
        (&self.tool, &self.target)
    }
}

struct PermissionBridgeState {
    closed: AtomicBool,
    next_id: AtomicU64,
    pending: Mutex<BTreeMap<u64, Sender<TuiPermissionReply>>>,
}

impl PermissionBridgeState {
    fn pending(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Sender<TuiPermissionReply>>> {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

#[derive(Clone)]
pub struct TuiPermissionBridge {
    requests: Sender<TuiPermissionRequest>,
    state: Arc<PermissionBridgeState>,
}

impl TuiPermissionBridge {
    pub fn channel() -> (Self, Receiver<TuiPermissionRequest>) {
        let (requests, receiver) = mpsc::channel();
        let state = Arc::new(PermissionBridgeState {
            closed: AtomicBool::new(false),
            next_id: AtomicU64::new(0),
            pending: Mutex::new(BTreeMap::new()),
        });
        (Self { requests, state }, receiver)
    }

    pub fn wait_for_reply(
        &self,
        tool: impl Into<String>,
        target: impl Into<String>,
        cancellation: &HeadlessTurnCancellation,
    ) -> TuiPermissionReply {
        if cancellation.is_cancelled() || self.state.closed.load(Ordering::Acquire) {
            return TuiPermissionReply::Cancelled;
        }
        if cancellation.is_expired() {
            return TuiPermissionReply::DeadlineExpired;
        }

        let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.state.pending().insert(id, sender);
        let request = TuiPermissionRequest {
            id,
            tool: tool.into(),
            target: target.into(),
        };
        if self.requests.send(request).is_err() {
            let _ = self.reply(id, TuiPermissionReply::Cancelled);
        }

        loop {
            if cancellation.is_cancelled() || self.state.closed.load(Ordering::Acquire) {
                let _ = self.reply(id, TuiPermissionReply::Cancelled);
                return TuiPermissionReply::Cancelled;
            }
            if cancellation.is_expired() {
                let _ = self.reply(id, TuiPermissionReply::DeadlineExpired);
                return TuiPermissionReply::DeadlineExpired;
            }

            match receiver.recv_timeout(RETRY_QUANTUM) {
                Ok(reply) => return reply,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return TuiPermissionReply::Cancelled,
            }
        }
    }

    pub fn reply(&self, id: u64, reply: TuiPermissionReply) -> bool {
        self.state
            .pending()
            .remove(&id)
            .is_some_and(|sender| sender.send(reply).is_ok())
    }

    pub fn is_pending(&self, id: u64) -> bool {
        self.state.pending().contains_key(&id)
    }

    pub fn close(&self) -> bool {
        self.state.closed.store(true, Ordering::Release);
        let pending = std::mem::take(&mut *self.state.pending());
        let had_pending = !pending.is_empty();
        for sender in pending.into_values() {
            let _ = sender.send(TuiPermissionReply::Cancelled);
        }
        had_pending
    }
}

#[derive(Clone, Debug, Default)]
pub struct BridgeCancel(Arc<AtomicBool>);

impl BridgeCancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiEnvelope<T> {
    ordinal: u64,
    event: T,
}

impl<T> UiEnvelope<T> {
    pub fn into_parts(self) -> (u64, T) {
        (self.ordinal, self.event)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    Published { ordinal: u64 },
    Cancelled,
    DeadlineExpired,
    Disconnected,
    Closed,
}

/// Typed observational data emitted by the CLI runtime for later TUI rendering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiRuntimeEvent {
    TurnStarted,
    TurnEnded {
        status: TurnState,
        duration: Option<Duration>,
    },
    Usage(Usage),
    ToolStarted {
        call_id: String,
        name: String,
        input: String,
    },
    ToolEnded {
        call_id: String,
        duration: Option<Duration>,
        result: ToolResultState,
    },
    Diff {
        call_id: String,
        lines: Vec<DiffLine>,
    },
    TaskExecution {
        agent: String,
        event: TuiExecutionEvent,
    },
    SubagentExecution(TuiSubagentEvent),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiSubagentEvent {
    pub(crate) id: u64,
    pub(crate) update: TuiSubagentUpdate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TuiSubagentUpdate {
    Started {
        agent: String,
        task_summary: String,
        presentation: TuiExecutionState,
    },
    ToolCall {
        call_id: String,
        name: String,
        input: String,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
}

impl TuiSubagentEvent {
    pub fn started(
        id: u64,
        agent: impl AsRef<str>,
        task_summary: impl AsRef<str>,
        presentation: TuiExecutionState,
    ) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::Started {
                agent: sanitize_projection(agent.as_ref()),
                task_summary: sanitize_projection(task_summary.as_ref()),
                presentation,
            },
        }
    }
    pub fn tool_call(
        id: u64,
        call_id: impl AsRef<str>,
        name: impl AsRef<str>,
        input: impl AsRef<str>,
    ) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::ToolCall {
                call_id: sanitize_projection(call_id.as_ref()),
                name: sanitize_projection(name.as_ref()),
                input: sanitize_projection(input.as_ref()),
            },
        }
    }
    pub fn tool_result(
        id: u64,
        call_id: impl AsRef<str>,
        output: impl AsRef<str>,
        is_error: bool,
    ) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::ToolResult {
                call_id: sanitize_projection(call_id.as_ref()),
                output: sanitize_projection(output.as_ref()),
                is_error,
            },
        }
    }
}

fn sanitize_projection(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if [
        "api_key",
        "authorization",
        "password",
        "secret",
        "token",
        "prompt:",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        "[redacted]".into()
    } else {
        value.chars().take(256).collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiExecutionEvent {
    ForegroundStarted { id: u64 },
    BackgroundStarted { id: u64 },
    Backgrounded { id: u64 },
    Completed { id: u64 },
    Failed { id: u64 },
    Cancelled { id: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiExecutionState {
    ForegroundRunning,
    BackgroundRunning,
    CompletedRecent,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiExecution {
    pub(crate) id: u64,
    pub(crate) agent: String,
    pub(crate) state: TuiExecutionState,
    pub(crate) last_activity: Duration,
    pub(crate) terminal_at: Option<Duration>,
}

impl TuiExecution {
    pub const fn id(&self) -> u64 {
        self.id
    }

    pub const fn state(&self) -> TuiExecutionState {
        self.state
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolResultState {
    Success,
    Failure,
}

struct BridgeState {
    closed: AtomicBool,
    next_ordinal: AtomicU64,
    publication: Mutex<()>,
    wake_lock: Mutex<()>,
    wake: Condvar,
}

pub struct BridgeTx<T> {
    sender: SyncSender<UiEnvelope<T>>,
    state: Arc<BridgeState>,
}

impl<T> Clone for BridgeTx<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

impl<T> BridgeTx<T> {
    pub fn bounded(capacity: usize) -> (Self, Receiver<UiEnvelope<T>>) {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let state = Arc::new(BridgeState {
            closed: AtomicBool::new(false),
            next_ordinal: AtomicU64::new(0),
            publication: Mutex::new(()),
            wake_lock: Mutex::new(()),
            wake: Condvar::new(),
        });

        (Self { sender, state }, receiver)
    }

    pub fn close(&self) {
        self.state.closed.store(true, Ordering::Release);
        self.state.wake.notify_all();
    }

    pub fn publish(
        &self,
        event: T,
        cancellation: &BridgeCancel,
        deadline: Option<Instant>,
    ) -> PublishOutcome {
        let Some(_publication) = self.wait_for_source_order(cancellation, deadline) else {
            return self.unavailable_outcome(cancellation, deadline);
        };

        let ordinal = self.state.next_ordinal.fetch_add(1, Ordering::Relaxed);
        let mut envelope = UiEnvelope { ordinal, event };

        loop {
            if let Some(outcome) = self.unavailable() {
                return outcome;
            }
            if cancellation.is_cancelled() {
                return PublishOutcome::Cancelled;
            }
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                return PublishOutcome::DeadlineExpired;
            }

            match self.sender.try_send(envelope) {
                Ok(()) => return PublishOutcome::Published { ordinal },
                Err(TrySendError::Full(unsent)) => {
                    envelope = unsent;
                    self.wait(deadline);
                }
                Err(TrySendError::Disconnected(_)) => return PublishOutcome::Disconnected,
            }
        }
    }

    fn wait_for_source_order(
        &self,
        cancellation: &BridgeCancel,
        deadline: Option<Instant>,
    ) -> Option<std::sync::MutexGuard<'_, ()>> {
        loop {
            if self.unavailable().is_some() || cancellation.is_cancelled() {
                return None;
            }
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                return None;
            }

            match self.state.publication.try_lock() {
                Ok(guard) => return Some(guard),
                Err(std::sync::TryLockError::Poisoned(error)) => return Some(error.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) => self.wait(deadline),
            }
        }
    }

    fn unavailable_outcome(
        &self,
        cancellation: &BridgeCancel,
        deadline: Option<Instant>,
    ) -> PublishOutcome {
        self.unavailable().unwrap_or_else(|| {
            if cancellation.is_cancelled() {
                PublishOutcome::Cancelled
            } else if deadline.is_some_and(|limit| Instant::now() >= limit) {
                PublishOutcome::DeadlineExpired
            } else {
                PublishOutcome::Closed
            }
        })
    }

    fn unavailable(&self) -> Option<PublishOutcome> {
        self.state
            .closed
            .load(Ordering::Acquire)
            .then_some(PublishOutcome::Closed)
    }

    fn wait(&self, deadline: Option<Instant>) {
        let guard = self
            .state
            .wake_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let timeout = deadline.map_or(RETRY_QUANTUM, |limit| {
            RETRY_QUANTUM.min(limit.saturating_duration_since(Instant::now()))
        });
        let _ = self.state.wake.wait_timeout(guard, timeout);
    }
}
