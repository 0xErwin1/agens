use agens_core::{SubagentErrorKind, SubagentStatus};

/// How long the permission bridge waits between polls while a reply is pending.
const RETRY_QUANTUM: Duration = Duration::from_millis(5);
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    time::Duration,
};

use agens_core::{HeadlessTurnCancellation, TurnState, Usage};

use crate::DiffLine;

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
        parsed: agens_core::ToolInput,
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
    RestoredCompletedSubagent {
        id: u64,
        agent: String,
        task_summary: String,
        final_result: String,
        tool_uses: usize,
    },
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
    Reasoning(String),
    Text(String),
    ToolCall {
        call_id: String,
        name: String,
        input: String,
        parsed: agens_core::ToolInput,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    Error {
        kind: SubagentErrorKind,
        reference: Option<String>,
    },
    Terminal {
        status: SubagentStatus,
        final_result: String,
    },
}

/// Rendering for the shared taxonomy. The classification is a domain fact and
/// lives in `agens-core`; only these strings belong to a surface.
pub(crate) trait SubagentErrorPresentation {
    fn message(self) -> &'static str;
    fn action(self) -> &'static str;
}

impl SubagentErrorPresentation for SubagentErrorKind {
    fn message(self) -> &'static str {
        match self {
            Self::Authentication => "Subagent authentication failed.",
            Self::Context => "Subagent request exceeds the model context window.",
            Self::Network => "Subagent network request failed.",
            Self::Provider => "Subagent provider request failed.",
            Self::Protocol => "Subagent provider response protocol failed.",
            Self::RateLimited => "Subagent provider request was rate limited.",
            Self::Rejected => "Subagent provider request was rejected.",
            Self::Server => "Subagent provider service failed.",
            Self::Tool => "Subagent tool execution failed.",
            Self::Runtime => "Subagent runtime failed.",
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Authentication => "Check provider credentials, then retry.",
            Self::Context => "Reduce the task context, then retry.",
            Self::Network => "Check network connectivity, then retry.",
            Self::Provider => "Retry the subagent request.",
            Self::Protocol => "Retry the subagent request or inspect diagnostics.",
            Self::RateLimited => "Wait before retrying the subagent request.",
            Self::Rejected => "Review the request configuration, then retry.",
            Self::Server => "Retry after the provider service recovers.",
            Self::Tool => "Review the tool call and retry.",
            Self::Runtime => "Retry the subagent request or inspect diagnostics.",
        }
    }
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
    /// Records a child tool call with an unknown/default `parsed` payload.
    ///
    /// Prefer [`Self::tool_call_with_parsed`] when an accurate typed input is
    /// available at the call site; this constructor exists for callers that
    /// only need the raw name/input (e.g. most tests).
    pub fn tool_call(
        id: u64,
        call_id: impl AsRef<str>,
        name: impl AsRef<str>,
        input: impl AsRef<str>,
    ) -> Self {
        let name = name.as_ref();
        let input = input.as_ref();
        Self::tool_call_with_parsed(
            id,
            call_id,
            name,
            input,
            agens_core::ToolInput::Other {
                name: name.to_owned(),
                raw: input.to_owned(),
            },
        )
    }

    /// Records a child tool call with a caller-supplied typed `parsed` input.
    pub fn tool_call_with_parsed(
        id: u64,
        call_id: impl AsRef<str>,
        name: impl AsRef<str>,
        input: impl AsRef<str>,
        parsed: agens_core::ToolInput,
    ) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::ToolCall {
                call_id: sanitize_projection(call_id.as_ref()),
                name: sanitize_projection(name.as_ref()),
                input: sanitize_projection(input.as_ref()),
                parsed,
            },
        }
    }

    pub fn reasoning(id: u64, delta: impl AsRef<str>) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::Reasoning(sanitize_projection(delta.as_ref())),
        }
    }

    pub fn text(id: u64, delta: impl AsRef<str>) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::Text(sanitize_projection(delta.as_ref())),
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

    pub fn error(id: u64, kind: SubagentErrorKind) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::Error {
                kind,
                reference: None,
            },
        }
    }

    pub fn error_with_reference(
        id: u64,
        kind: SubagentErrorKind,
        reference: impl AsRef<str>,
    ) -> Self {
        let reference = reference.as_ref();
        let reference = (reference.len() == 8
            && reference
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then(|| reference.to_owned());
        Self {
            id,
            update: TuiSubagentUpdate::Error { kind, reference },
        }
    }

    pub fn terminal(id: u64, status: SubagentStatus, final_result: impl AsRef<str>) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::Terminal {
                status,
                final_result: sanitize_projection(final_result.as_ref()),
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
    pub(crate) started_at: Duration,
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
