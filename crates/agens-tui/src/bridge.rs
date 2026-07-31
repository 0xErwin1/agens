use agens_core::SubagentErrorKind;

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

use agens_core::HeadlessTurnCancellation;

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
            Self::IterationLimit => "Subagent iteration limit reached.",
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
            Self::IterationLimit => "Increase subagents.max_iterations or narrow the task.",
            Self::Runtime => "Retry the subagent request or inspect diagnostics.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SubagentErrorPresentation;
    use agens_core::SubagentErrorKind;

    #[test]
    fn iteration_limit_has_an_exact_actionable_presentation() {
        assert_eq!(
            SubagentErrorKind::IterationLimit.message(),
            "Subagent iteration limit reached."
        );
        assert_eq!(
            SubagentErrorKind::IterationLimit.action(),
            "Increase subagents.max_iterations or narrow the task."
        );
    }
}
