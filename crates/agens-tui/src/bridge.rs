use agens_core::SubagentErrorKind;

/// How long a bridge waits between polls while a reply is pending, shared by
/// [`TuiPermissionBridge`] and [`TuiAskUserBridge`].
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
use agens_core::ask_user::{AskUserReply, AskUserRequest, AskUserUnavailable};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiPermissionReply {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
    Cancelled,
    DeadlineExpired,
}

/// Who a parked prompt belongs to.
///
/// `None` on a request means the main thread. A delegated execution names
/// itself, because with several subagents running a question you cannot
/// attribute is a question you cannot answer responsibly — you would be
/// approving `bash` without knowing which agent is about to run it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptOrigin {
    pub execution: u64,
    pub agent: String,
}

pub struct TuiPermissionRequest {
    id: u64,
    tool: String,
    target: String,
    access: String,
    reason: Option<String>,
    origin: Option<PromptOrigin>,
}

impl TuiPermissionRequest {
    pub const fn id(&self) -> u64 {
        self.id
    }

    pub const fn origin(&self) -> Option<&PromptOrigin> {
        self.origin.as_ref()
    }

    pub fn details(&self) -> (&str, &str) {
        (&self.tool, &self.target)
    }

    pub fn access(&self) -> &str {
        &self.access
    }

    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

/// A parked request, kept with the origin that raised it so a refusal can find
/// its siblings.
struct Parked<T> {
    origin: Option<PromptOrigin>,
    sender: Sender<T>,
}

/// Collects the other requests raised by `origin`, so refusing one refuses
/// them all.
///
/// Turning a subagent down stops that subagent; the questions it had already
/// queued are about work that is no longer happening, and answering them one
/// by one only asks the reader to re-decide something they already decided.
/// Only an attributed origin groups: `None` is the main thread, which never
/// has more than one prompt open, so treating "no origin" as a group would
/// invent a relationship rather than describe one.
fn siblings_of<T>(pending: &BTreeMap<u64, Parked<T>>, origin: Option<&PromptOrigin>) -> Vec<u64> {
    let Some(origin) = origin else {
        return Vec::new();
    };
    pending
        .iter()
        .filter(|(_, parked)| parked.origin.as_ref() == Some(origin))
        .map(|(id, _)| *id)
        .collect()
}

struct PermissionBridgeState {
    closed: AtomicBool,
    next_id: AtomicU64,
    pending: Mutex<BTreeMap<u64, Parked<TuiPermissionReply>>>,
}

impl PermissionBridgeState {
    fn pending(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Parked<TuiPermissionReply>>> {
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

    /// Parks the calling thread until the request is answered, cancelled, or
    /// the surface disconnects.
    ///
    /// The deadline `cancellation` may carry is deliberately never read. A
    /// permission question is blocking without qualification: the only things
    /// that may end one are the person answering it, the person cancelling,
    /// and the surface going away. This holds even when the caller hands over
    /// a cancellation whose deadline has already passed, so no future caller
    /// can reintroduce a timeout by supplying one.
    ///
    /// `tool` is the bare name a rule is written against; `target` is already
    /// sanitized for display; `access` is a short access label (e.g. `Write`).
    pub fn wait_for_reply(
        &self,
        tool: impl Into<String>,
        target: impl Into<String>,
        access: impl Into<String>,
        reason: Option<String>,
        origin: Option<PromptOrigin>,
        cancellation: &HeadlessTurnCancellation,
    ) -> TuiPermissionReply {
        if cancellation.is_cancelled() || self.state.closed.load(Ordering::Acquire) {
            return TuiPermissionReply::Cancelled;
        }

        let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.state.pending().insert(
            id,
            Parked {
                origin: origin.clone(),
                sender,
            },
        );
        let request = TuiPermissionRequest {
            id,
            tool: tool.into(),
            target: target.into(),
            access: access.into(),
            reason,
            origin,
        };
        if self.requests.send(request).is_err() {
            let _ = self.reply(id, TuiPermissionReply::Cancelled);
        }

        loop {
            if cancellation.is_cancelled() || self.state.closed.load(Ordering::Acquire) {
                let _ = self.reply(id, TuiPermissionReply::Cancelled);
                return TuiPermissionReply::Cancelled;
            }

            match receiver.recv_timeout(RETRY_QUANTUM) {
                Ok(reply) => return reply,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return TuiPermissionReply::Cancelled,
            }
        }
    }

    /// Resolves one request, and every other one from the same execution when
    /// the answer was a refusal — see [`siblings_of`].
    pub fn reply(&self, id: u64, reply: TuiPermissionReply) -> bool {
        let mut pending = self.state.pending();
        let Some(parked) = pending.remove(&id) else {
            return false;
        };
        let delivered = parked.sender.send(reply).is_ok();

        if matches!(
            reply,
            TuiPermissionReply::DenyOnce | TuiPermissionReply::DenyAlways
        ) {
            for sibling in siblings_of(&pending, parked.origin.as_ref()) {
                if let Some(parked) = pending.remove(&sibling) {
                    let _ = parked.sender.send(TuiPermissionReply::DenyOnce);
                }
            }
        }

        delivered
    }

    pub fn is_pending(&self, id: u64) -> bool {
        self.state.pending().contains_key(&id)
    }

    pub fn close(&self) -> bool {
        self.state.closed.store(true, Ordering::Release);
        let pending = std::mem::take(&mut *self.state.pending());
        let had_pending = !pending.is_empty();
        for parked in pending.into_values() {
            let _ = parked.sender.send(TuiPermissionReply::Cancelled);
        }
        had_pending
    }
}

/// A parked `ask_user` request, carried from the tool thread to the event
/// loop through [`TuiAskUserBridge::channel`].
pub struct TuiAskUserRequest {
    id: u64,
    request: AskUserRequest,
    origin: Option<PromptOrigin>,
}

impl TuiAskUserRequest {
    pub const fn id(&self) -> u64 {
        self.id
    }

    pub const fn origin(&self) -> Option<&PromptOrigin> {
        self.origin.as_ref()
    }

    pub const fn request(&self) -> &AskUserRequest {
        &self.request
    }
}

struct AskUserBridgeState {
    closed: AtomicBool,
    next_id: AtomicU64,
    pending: Mutex<BTreeMap<u64, Parked<AskUserReply>>>,
}

impl AskUserBridgeState {
    fn pending(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Parked<AskUserReply>>> {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// The tool-side port to the `ask_user` interaction surface.
///
/// Built from the same primitives as [`TuiPermissionBridge`] but sharing none
/// of its types: an ask-user reply is never a permission reply. Bridge
/// disconnect deliberately maps to [`AskUserReply::Unavailable`], not
/// [`AskUserReply::Cancelled`] — ask-user has an explicit unavailable status
/// the permission bridge does not, and collapsing "the surface is gone" into
/// "the user declined" would lose that information.
#[derive(Clone)]
pub struct TuiAskUserBridge {
    requests: Sender<TuiAskUserRequest>,
    state: Arc<AskUserBridgeState>,
}

impl TuiAskUserBridge {
    pub fn channel() -> (Self, Receiver<TuiAskUserRequest>) {
        let (requests, receiver) = mpsc::channel();
        let state = Arc::new(AskUserBridgeState {
            closed: AtomicBool::new(false),
            next_id: AtomicU64::new(0),
            pending: Mutex::new(BTreeMap::new()),
        });
        (Self { requests, state }, receiver)
    }

    /// Parks the calling thread until the request is answered, cancelled, or
    /// the surface disconnects.
    ///
    /// The deadline `cancellation` may carry is deliberately never read. A
    /// question is blocking without qualification: the only things that may
    /// end one are the person answering it, the person cancelling, and the
    /// surface going away. This holds even when the caller hands over a
    /// cancellation whose deadline has already passed, so no future caller can
    /// reintroduce a timeout by supplying one.
    ///
    /// Exactly-once resolution falls out of `reply` removing the pending
    /// sender under one mutex: whichever caller — this loop reacting to
    /// cancellation, or an external `reply` carrying the user's answer —
    /// reaches `pending.remove` first is the only one whose value is ever
    /// sent.
    pub fn wait_for_reply(
        &self,
        request: AskUserRequest,
        origin: Option<PromptOrigin>,
        cancellation: &HeadlessTurnCancellation,
    ) -> AskUserReply {
        if self.state.closed.load(Ordering::Acquire) {
            return AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed);
        }
        if cancellation.is_cancelled() {
            return AskUserReply::Cancelled;
        }

        let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.state.pending().insert(
            id,
            Parked {
                origin: origin.clone(),
                sender,
            },
        );
        let tui_request = TuiAskUserRequest {
            id,
            request,
            origin,
        };
        if self.requests.send(tui_request).is_err() {
            let _ = self.reply(
                id,
                AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed),
            );
        }

        loop {
            if self.state.closed.load(Ordering::Acquire) {
                let outcome = AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed);
                return self.resolve_or_defer_to_committed(id, outcome, &receiver);
            }
            if cancellation.is_cancelled() {
                return self.resolve_or_defer_to_committed(id, AskUserReply::Cancelled, &receiver);
            }

            match receiver.recv_timeout(RETRY_QUANTUM) {
                Ok(reply) => return reply,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed);
                }
            }
        }
    }

    /// Resolves this request with `outcome`, unless a reply already
    /// committed a moment earlier — in which case that value, sitting
    /// unread in `receiver`, is returned instead.
    ///
    /// Without this, a self-triggered resolution (cancellation or a closed
    /// surface observed by this very loop) would call `reply` purely to release the
    /// pending entry, ignore whether it actually won, and return its own
    /// `outcome` regardless. If an external `reply` had committed a moment
    /// earlier — the real answer already sitting in `receiver` — that value
    /// would be silently discarded even though the spec requires an
    /// already-committed reply to win. `reply` holds the state mutex across
    /// its `send`, so by the time this call observes its own `reply` losing,
    /// the winning value is unconditionally already in `receiver`.
    fn resolve_or_defer_to_committed(
        &self,
        id: u64,
        outcome: AskUserReply,
        receiver: &Receiver<AskUserReply>,
    ) -> AskUserReply {
        if self.reply(id, outcome.clone()) {
            return outcome;
        }
        receiver.try_recv().unwrap_or(outcome)
    }

    /// Resolves a pending request, at most once. A second call for the same
    /// id — from any source — finds nothing left to remove and returns
    /// `false`, which is the whole of this bridge's exactly-once guarantee.
    ///
    /// The mutex is held across `sender.send`: `pending.remove` and the send
    /// happen as one atomic step from every other caller's point of view, so
    /// a losing caller can rely on the winning value already being in the
    /// channel the instant its own `remove` fails. This is only safe because
    /// the channel is unbounded — `send` on an unbounded `mpsc` channel never
    /// blocks, so the lock is never held across a wait.
    pub fn reply(&self, id: u64, reply: AskUserReply) -> bool {
        let mut pending = self.state.pending();
        let Some(parked) = pending.remove(&id) else {
            return false;
        };
        let delivered = parked.sender.send(reply.clone()).is_ok();

        // Cancelling a subagent's question cancels the rest of that
        // subagent's, for the same reason a refused permission does: the work
        // they were about is over.
        if matches!(reply, AskUserReply::Cancelled) {
            for sibling in siblings_of(&pending, parked.origin.as_ref()) {
                if let Some(parked) = pending.remove(&sibling) {
                    let _ = parked.sender.send(AskUserReply::Cancelled);
                }
            }
        }

        delivered
    }

    pub fn is_pending(&self, id: u64) -> bool {
        self.state.pending().contains_key(&id)
    }

    /// Releases every parked waiter as `Unavailable(SurfaceClosed)` so no
    /// tool thread can outlive the surface while parked.
    pub fn close(&self) -> bool {
        self.state.closed.store(true, Ordering::Release);
        let pending = std::mem::take(&mut *self.state.pending());
        let had_pending = !pending.is_empty();
        for parked in pending.into_values() {
            let _ = parked
                .sender
                .send(AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed));
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
            Self::ReplayBudget => "Subagent session history outgrew the replay budget.",
            Self::Network => "Subagent network request failed.",
            Self::Provider => "Subagent provider request failed.",
            Self::Protocol => "Subagent provider response protocol failed.",
            Self::RateLimited => "Subagent provider request was rate limited.",
            Self::Rejected => "Subagent provider request was rejected.",
            Self::Server => "Subagent provider service failed.",
            Self::Tool => "Subagent tool execution failed.",
            Self::Runtime => "Subagent runtime failed.",
            Self::ResultDelivery => "Subagent finished; its result was not delivered.",
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Authentication => "Check provider credentials, then retry.",
            Self::Context => "Reduce the task context, then retry.",
            Self::ReplayBudget => "Inspect replay-budget diagnostics, then retry.",
            Self::Network => "Check network connectivity, then retry.",
            Self::Provider => "Retry the subagent request.",
            Self::Protocol => "Retry the subagent request or inspect diagnostics.",
            Self::RateLimited => "Wait before retrying the subagent request.",
            Self::Rejected => "Review the request configuration, then retry.",
            Self::Server => "Retry after the provider service recovers.",
            Self::Tool => "Review the tool call and retry.",
            Self::Runtime => "Retry the subagent request or inspect diagnostics.",
            Self::ResultDelivery => "Read the result in the subagent panel; inspect diagnostics.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Parked, PromptOrigin, SubagentErrorPresentation, TuiAskUserBridge};
    use agens_core::SubagentErrorKind;
    use agens_core::ask_user::{
        AskUserAnswer, AskUserMode, AskUserOption, AskUserQuestion, AskUserReply, AskUserRequest,
        AskUserUnavailable,
    };
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn replay_budget_card_does_not_blame_the_model_context_window() {
        let message = SubagentErrorKind::ReplayBudget.message();
        let action = SubagentErrorKind::ReplayBudget.action();

        assert!(message.contains("replay budget"), "{message}");
        assert!(!message.contains("context window"), "{message}");
        assert!(action.contains("replay-budget"), "{action}");
    }

    fn single_question_request() -> AskUserRequest {
        let options = vec![
            AskUserOption::new("a", "Option A", None, None),
            AskUserOption::new("b", "Option B", None, None),
        ];
        let question = AskUserQuestion::new(
            "plan",
            "Which plan?",
            None,
            AskUserMode::Single,
            options,
            false,
            false,
            false,
        );
        AskUserRequest::new(None, vec![question]).expect("valid request")
    }

    /// Waits for the waiter thread to register `id` as pending, and gives up
    /// rather than spinning forever.
    ///
    /// The bound is what makes this usable as a regression guard: a bridge that
    /// resolved the request behind the test's back would never register it, and
    /// an unbounded spin would hang the suite instead of reporting which
    /// property broke.
    fn await_pending(bridge: &TuiAskUserBridge, id: u64) {
        for _ in 0..1_000 {
            if bridge.is_pending(id) {
                return;
            }
            thread::sleep(super::RETRY_QUANTUM);
        }
        panic!("the request never parked: it was resolved without anyone answering it");
    }

    fn answered_reply() -> AskUserReply {
        AskUserReply::Answered(vec![AskUserAnswer {
            question_id: "plan".into(),
            selected: vec!["a".into()],
            other: None,
            note: None,
        }])
    }

    #[test]
    fn a_closed_bridge_answers_unavailable_rather_than_cancelled() {
        let (bridge, _receiver) = TuiAskUserBridge::channel();
        let cancellation = agens_core::HeadlessTurnCancellation::new();

        assert!(!bridge.close());

        let reply = bridge.wait_for_reply(single_question_request(), None, &cancellation);

        assert_eq!(
            reply,
            AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed)
        );
    }

    /// The deadline here is already in the past before the request is even
    /// sent, which is the harshest form of the guarantee: not "the clock had
    /// not run out yet" but "the clock ran out and it changed nothing". Every
    /// poll of the wait loop sees an expired cancellation and must still park.
    #[test]
    fn an_expired_deadline_never_resolves_a_parked_question() {
        let (bridge, receiver) = TuiAskUserBridge::channel();
        let cancellation =
            agens_core::HeadlessTurnCancellation::with_deadline(std::time::Duration::ZERO);
        let waiting_bridge = bridge.clone();

        let waiter = thread::spawn(move || {
            waiting_bridge.wait_for_reply(single_question_request(), None, &cancellation)
        });

        let request = receiver
            .recv()
            .expect("an already-expired deadline must not stop the request from parking");
        await_pending(&bridge, request.id());

        thread::sleep(super::RETRY_QUANTUM * 20);
        assert!(
            bridge.is_pending(request.id()),
            "the question must still be waiting on a person long after its deadline passed"
        );

        assert!(
            bridge.reply(request.id(), answered_reply()),
            "the person's answer must still be accepted once the deadline is irrelevant"
        );
        assert_eq!(
            waiter.join().expect("waiter thread should not panic"),
            answered_reply()
        );
    }

    #[test]
    fn cancellation_still_ends_a_question_carrying_an_expired_deadline() {
        let (bridge, _receiver) = TuiAskUserBridge::channel();
        let cancellation =
            agens_core::HeadlessTurnCancellation::with_deadline(std::time::Duration::ZERO);
        cancellation.cancel();

        let reply = bridge.wait_for_reply(single_question_request(), None, &cancellation);

        assert_eq!(reply, AskUserReply::Cancelled);
    }

    #[test]
    fn close_releases_a_parked_waiter_as_unavailable_surface_closed() {
        let (bridge, receiver) = TuiAskUserBridge::channel();
        let cancellation = agens_core::HeadlessTurnCancellation::new();
        let waiting_bridge = bridge.clone();

        let waiter = thread::spawn(move || {
            waiting_bridge.wait_for_reply(single_question_request(), None, &cancellation)
        });

        let request = receiver
            .recv()
            .expect("the parked request should reach the receiver");
        while !bridge.is_pending(request.id()) {
            thread::yield_now();
        }

        assert!(bridge.close());
        assert_eq!(
            waiter.join().expect("waiter thread should not panic"),
            AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed)
        );
    }

    #[test]
    fn a_resolved_request_rejects_every_later_reply() {
        let (bridge, receiver) = TuiAskUserBridge::channel();
        let cancellation = agens_core::HeadlessTurnCancellation::new();
        let waiting_bridge = bridge.clone();

        let waiter = thread::spawn(move || {
            waiting_bridge.wait_for_reply(single_question_request(), None, &cancellation)
        });

        let request = receiver
            .recv()
            .expect("the parked request should reach the receiver");
        assert!(bridge.reply(request.id(), answered_reply()));

        assert_eq!(
            waiter.join().expect("waiter thread should not panic"),
            answered_reply()
        );
        assert!(!bridge.reply(request.id(), AskUserReply::Cancelled));
        assert!(!bridge.is_pending(request.id()));
    }

    #[test]
    fn exactly_one_outcome_commits_under_a_submit_and_close_race() {
        let (bridge, receiver) = TuiAskUserBridge::channel();
        let cancellation = agens_core::HeadlessTurnCancellation::new();
        let waiting_bridge = bridge.clone();

        let waiter = thread::spawn(move || {
            waiting_bridge.wait_for_reply(single_question_request(), None, &cancellation)
        });

        let request = receiver
            .recv()
            .expect("the parked request should reach the receiver");
        let id = request.id();
        let barrier = Arc::new(Barrier::new(2));

        let reply_barrier = Arc::clone(&barrier);
        let reply_bridge = bridge.clone();
        let reply_thread = thread::spawn(move || {
            reply_barrier.wait();
            reply_bridge.reply(id, answered_reply())
        });

        let close_barrier = Arc::clone(&barrier);
        let close_bridge = bridge.clone();
        let close_thread = thread::spawn(move || {
            close_barrier.wait();
            close_bridge.close()
        });

        let reply_won = reply_thread.join().expect("reply thread should not panic");
        let close_had_pending = close_thread.join().expect("close thread should not panic");
        let outcome = waiter.join().expect("waiter thread should not panic");

        assert_ne!(
            reply_won, close_had_pending,
            "exactly one caller must commit"
        );
        assert!(!bridge.is_pending(id));
        if reply_won {
            assert_eq!(outcome, answered_reply());
        } else {
            assert_eq!(
                outcome,
                AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed)
            );
        }
    }

    /// The spec's "submission races cancellation" scenario, built the same
    /// way as the submit/close race: a `Barrier(2)` against a genuinely
    /// parked waiter, asserting an XOR on who committed. This is also
    /// exactly the path where a committed reply could be silently discarded
    /// by the waiter's own cancellation branch if that branch ignored
    /// whether its own `reply` call actually won — proving that fix.
    #[test]
    fn a_submitted_answer_racing_external_cancellation_yields_exactly_one_outcome() {
        let (bridge, receiver) = TuiAskUserBridge::channel();
        let cancellation = agens_core::HeadlessTurnCancellation::new();
        let waiting_cancellation = cancellation.clone();
        let waiting_bridge = bridge.clone();

        let waiter = thread::spawn(move || {
            waiting_bridge.wait_for_reply(single_question_request(), None, &waiting_cancellation)
        });

        let request = receiver
            .recv()
            .expect("the parked request should reach the receiver");
        let id = request.id();
        let barrier = Arc::new(Barrier::new(2));

        let reply_barrier = Arc::clone(&barrier);
        let reply_bridge = bridge.clone();
        let reply_thread = thread::spawn(move || {
            reply_barrier.wait();
            reply_bridge.reply(id, answered_reply())
        });

        let cancel_barrier = Arc::clone(&barrier);
        let cancel_thread = thread::spawn(move || {
            cancel_barrier.wait();
            cancellation.cancel();
        });

        let reply_won = reply_thread.join().expect("reply thread should not panic");
        cancel_thread
            .join()
            .expect("cancel thread should not panic");
        let outcome = waiter.join().expect("waiter thread should not panic");

        assert!(!bridge.is_pending(id));
        if reply_won {
            assert_eq!(
                outcome,
                answered_reply(),
                "an already-committed reply must win over a later cancellation, not be \
                 silently discarded by the waiter's own cancellation branch"
            );
        } else {
            assert_eq!(outcome, AskUserReply::Cancelled);
        }
        assert!(
            !bridge.reply(id, AskUserReply::Cancelled),
            "whichever outcome committed, the request must resolve at most once"
        );
    }

    /// A fully deterministic, non-racy pin for W5-1: with the pending entry
    /// already committed by another caller before this call ever runs, the
    /// self-triggered branch must return what actually committed, not the
    /// outcome it was about to declare on its own. Complements the racy
    /// end-to-end test above, which exercises the real scheduling-dependent
    /// path but cannot force this exact interleaving on every run.
    /// Turning a subagent down ends its queue. Otherwise the reader answers
    /// "no" and is immediately asked the next question of the same work they
    /// just stopped — while a different subagent's question, which they have
    /// not decided anything about, has to survive untouched.
    #[test]
    fn refusing_one_subagent_refuses_the_rest_of_its_questions_and_nobody_elses() {
        let (bridge, _requests) = super::TuiPermissionBridge::channel();
        let reviewer = PromptOrigin {
            execution: 7,
            agent: "reviewer".into(),
        };
        let builder = PromptOrigin {
            execution: 9,
            agent: "builder".into(),
        };

        let park = |origin: Option<PromptOrigin>| {
            let id = bridge
                .state
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (sender, receiver) = mpsc::channel();
            bridge.state.pending().insert(id, Parked { origin, sender });
            (id, receiver)
        };

        let (first, _first_rx) = park(Some(reviewer.clone()));
        let (second, second_rx) = park(Some(reviewer));
        let (other, other_rx) = park(Some(builder));
        let (main, main_rx) = park(None);

        assert!(bridge.reply(first, super::TuiPermissionReply::DenyOnce));

        assert!(
            !bridge.is_pending(second),
            "the same subagent's other question must be refused with it"
        );
        assert_eq!(
            second_rx.try_recv(),
            Ok(super::TuiPermissionReply::DenyOnce)
        );

        assert!(
            bridge.is_pending(other),
            "a different subagent decided nothing and must still be waiting"
        );
        assert!(other_rx.try_recv().is_err());
        assert!(bridge.is_pending(main), "and neither did the main thread");
        assert!(main_rx.try_recv().is_err());
    }

    /// Approving is not stopping: the rest of that subagent's queue is still
    /// live work the reader has not decided about.
    #[test]
    fn allowing_one_question_leaves_the_rest_of_that_subagents_queue_alone() {
        let (bridge, _requests) = super::TuiPermissionBridge::channel();
        let origin = PromptOrigin {
            execution: 3,
            agent: "worker".into(),
        };

        let park = || {
            let id = bridge
                .state
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (sender, receiver) = mpsc::channel();
            bridge.state.pending().insert(
                id,
                Parked {
                    origin: Some(origin.clone()),
                    sender,
                },
            );
            (id, receiver)
        };

        let (first, _first_rx) = park();
        let (second, second_rx) = park();

        assert!(bridge.reply(first, super::TuiPermissionReply::AllowOnce));

        assert!(bridge.is_pending(second));
        assert!(second_rx.try_recv().is_err());
    }

    #[test]
    fn a_losing_self_reply_defers_to_whatever_already_committed() {
        let (bridge, _requests) = TuiAskUserBridge::channel();
        let (sender, receiver) = mpsc::channel();
        let id = 42;
        bridge.state.pending().insert(
            id,
            Parked {
                origin: None,
                sender,
            },
        );
        assert!(bridge.reply(id, answered_reply()));

        let outcome = bridge.resolve_or_defer_to_committed(id, AskUserReply::Cancelled, &receiver);

        assert_eq!(outcome, answered_reply());
    }
}
