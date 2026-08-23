//! Chat sessions the daemon hosts on an attached client's behalf.
//!
//! An ordinary chat turn used to run inside the terminal process, which made
//! the terminal the thing that had to stay alive for the turn to finish. Hosted
//! here it is a session like any other: the client sends a prompt, the daemon
//! runs the turn, and the events it produces reach whoever is subscribed —
//! including nobody, which is what detaching means.
//!
//! A hosted chat is a peer of the sessions the scheduler starts, not a new kind
//! of thing. It is admitted through [`SessionAdmission`] and started through
//! [`SessionSupervisor`], so the registry's capacity, its cancellation and the
//! daemon's shutdown join already cover it, and `serve stop` drains a chat the
//! same way it drains a worker.
//!
//! What a turn is made of — the model, the prompt, the tools, the history it
//! resumes — never enters this module. That is the composition root's
//! knowledge, and it arrives through [`ChatSessionFactory`], the same seam the
//! scheduler takes its workers from.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agens_core::{HeadlessTurnCancellation, TurnEvent, TurnProgressSink};

use crate::sessions::{
    SessionAdmission, SessionId, SessionOutcome, SessionRegistryError, SessionRuntime,
    SessionSupervisor,
};

/// How many prompts may wait while a turn is running.
///
/// One, because a chat is a conversation: the reply to the second prompt
/// depends on what the first one did, so accepting a queue of them would let a
/// client build up work whose meaning changes underneath it. A client that
/// wants to say something to a running turn is not sending another prompt.
const PROMPT_BACKLOG: usize = 1;

/// How long a hosted chat waits on its inbox before looking at its cancellation
/// again. Cancellation is a flag rather than a channel, so a session that is
/// parked on input polls it the way the rest of the daemon does.
const PROMPT_POLL: Duration = Duration::from_millis(50);

/// How many events one subscriber may have waiting before the fan-out gives up
/// on it.
///
/// A turn emits an event per provider part, so this is thousands of tokens of
/// slack for a client that stalls briefly and still a bound the daemon's memory
/// respects when one stops reading for good.
const SUBSCRIBER_BACKLOG: usize = 2048;

/// What a client asked for when it opened a chat session.
pub struct ChatSessionRequest {
    /// The checkout the chat's tools run in.
    ///
    /// A path rather than a repository id, for the reason a run's creation
    /// takes one: the identity is derived from the checkout, and a repository
    /// that has never had a run has no id for a client to name. One daemon
    /// serves N projects, and a chat without one is a chat whose tools have no
    /// root to run in.
    pub checkout: PathBuf,
    /// The stored session to continue, when the client is resuming one.
    pub resume: Option<i64>,
}

/// How one prompt is turned into a turn.
///
/// The daemon owns when a turn runs and who hears about it; this owns what
/// running one means. Taken as a trait object so that stays outside this crate:
/// a turn needs models, prompts, skills and a project root, and the control
/// plane deliberately knows none of them.
pub trait ChatTurns: Send {
    /// Runs one prompt to completion, reporting progress through `progress`.
    ///
    /// `cancellation` belongs to this turn and to no other. It is not the
    /// session's: a person stopping the answer they are reading is stopping
    /// that answer, and a chat that ended because of it would be a chat nobody
    /// asked to close. The session's own cancellation still ends everything,
    /// which is what shutting the daemon down uses.
    fn run(
        &mut self,
        prompt: &str,
        runtime: &SessionRuntime,
        cancellation: &HeadlessTurnCancellation,
        progress: &TurnProgressSink,
    ) -> ChatTurnOutcome;
}

/// How one turn of a hosted chat ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatTurnOutcome {
    Completed(String),
    Failed(String),
}

/// What opening one chat session takes.
pub struct ChatSession {
    pub admission: SessionAdmission,
    pub turns: Box<dyn ChatTurns>,
}

/// How the daemon turns a client's request into a chat session.
pub type ChatSessionFactory =
    Arc<dyn Fn(&ChatSessionRequest) -> Result<ChatSession, ChatError> + Send + Sync>;

/// What a subscriber to a hosted chat sees.
///
/// [`TurnEvent`] carries the turn itself — the provider's parts, the tool calls
/// and their results — and is passed through rather than re-described, so the
/// surface rendering a hosted turn renders exactly what it renders for a local
/// one. The other three are the session's own facts, which a turn's events do
/// not carry: how the turn ended, and that there will be no more of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatEvent {
    Progress(TurnEvent),
    TurnCompleted { text: String },
    TurnFailed { detail: String },
    Closed,
}

/// Why a chat operation could not be served.
#[derive(Debug, PartialEq, Eq)]
pub enum ChatError {
    /// No chat session with this id is open. A session that ended is reported
    /// this way too: what the caller can do about either is the same.
    Unknown,
    /// A turn is already running and one prompt is already waiting behind it.
    Busy,
    Unavailable(String),
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => formatter.write_str("no such chat session"),
            Self::Busy => formatter.write_str("the chat session is already running a turn"),
            Self::Unavailable(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for ChatError {}

/// The subscribers of one hosted chat.
#[derive(Default)]
struct Subscribers(Mutex<Vec<SyncSender<ChatEvent>>>);

impl Subscribers {
    /// Hands one event to every subscriber, dropping the ones that are gone and
    /// the ones that fell too far behind.
    ///
    /// A full backlog ends that subscription rather than blocking or skipping.
    /// Blocking would let one client that stopped reading stall the turn for
    /// every other client and for the provider call behind it; skipping would
    /// leave a hole in a turn's event sequence with nothing in the stream to
    /// say so. Ending it is the one outcome a client can act on.
    fn publish(&self, event: &ChatEvent) {
        let Ok(mut subscribers) = self.0.lock() else {
            return;
        };

        subscribers.retain(|outbound| match outbound.try_send(event.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        });
    }

    fn add(&self) -> Result<Receiver<ChatEvent>, ChatError> {
        let (outbound, inbound) = sync_channel(SUBSCRIBER_BACKLOG);

        self.0
            .lock()
            .map_err(|_| unusable("the chat fan-out"))?
            .push(outbound);

        Ok(inbound)
    }
}

/// One open chat, as the daemon holds it.
struct OpenChat {
    /// Dropping this ends the session's loop, which is what closing a chat is.
    prompts: SyncSender<String>,
    subscribers: Arc<Subscribers>,
    /// The turn running right now, when one is. Replaced per turn, so
    /// cancelling reaches the answer being produced and never the one before
    /// it.
    running: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
}

/// The chat sessions this daemon is hosting.
pub struct ChatSessions {
    supervisor: SessionSupervisor,
    open_chat: ChatSessionFactory,
    open: Mutex<BTreeMap<SessionId, OpenChat>>,
}

impl ChatSessions {
    #[must_use]
    pub fn new(supervisor: SessionSupervisor, open_chat: ChatSessionFactory) -> Self {
        Self {
            supervisor,
            open_chat,
            open: Mutex::new(BTreeMap::new()),
        }
    }

    /// Opens a chat session and starts it, so a prompt has somewhere to arrive.
    ///
    /// The session is recorded as open before it is started, because between
    /// those two the client already holds nothing: a prompt that arrived in
    /// that window would be refused for a session that exists. A start that
    /// fails takes the record back out.
    pub fn open(&self, request: &ChatSessionRequest) -> Result<SessionId, ChatError> {
        let ChatSession {
            admission,
            mut turns,
        } = (self.open_chat)(request)?;

        let session = admission.session();
        let (prompts, inbox) = sync_channel(PROMPT_BACKLOG);
        let subscribers = Arc::new(Subscribers::default());
        let published = Arc::clone(&subscribers);
        let running = Arc::new(Mutex::new(None));
        let turn = Arc::clone(&running);

        let mut open = self.locked()?;
        // Chats come and go for the life of the daemon, so the ones that have
        // already ended are dropped here rather than accumulating. Pruning
        // where sessions are opened is what the supervisor does with its own
        // workers, and doing it on the same beat is what keeps the two from
        // disagreeing about which sessions exist.
        open.retain(|session, _| self.is_running(*session));
        open.insert(
            session,
            OpenChat {
                prompts,
                subscribers,
                running,
            },
        );
        drop(open);

        let started = self.supervisor.start(admission, move |runtime| {
            serve_prompts(turns.as_mut(), &runtime, &inbox, &published, &turn)
        });

        if let Err(error) = started {
            self.locked()?.remove(&session);
            return Err(ChatError::Unavailable(describe(error).to_owned()));
        }

        Ok(session)
    }

    /// Hands a prompt to an open chat, without waiting for the turn it starts.
    pub fn prompt(&self, session: SessionId, prompt: String) -> Result<(), ChatError> {
        let open = self.locked()?;
        let chat = open.get(&session).ok_or(ChatError::Unknown)?;

        chat.prompts.try_send(prompt).map_err(|error| match error {
            TrySendError::Full(_) => ChatError::Busy,
            // The loop is gone, so the session behind this record has ended.
            TrySendError::Disconnected(_) => ChatError::Unknown,
        })
    }

    /// Opens a stream of one chat's events, live from now.
    ///
    /// It is not a replay: a client that wants what it missed while it was
    /// detached asks for the session's stored history, which is the projection
    /// built for exactly that.
    pub fn subscribe(&self, session: SessionId) -> Result<Receiver<ChatEvent>, ChatError> {
        self.locked()?
            .get(&session)
            .ok_or(ChatError::Unknown)?
            .subscribers
            .add()
    }

    /// Stops the turn a chat is running, leaving the session open.
    ///
    /// It trips that turn's own cancellation rather than the session's. A
    /// person stopping the answer they are reading is stopping that answer:
    /// signalling the session would end the chat, and the next prompt would
    /// have nowhere to arrive. Ending the session is what `close` and the
    /// daemon's shutdown do.
    ///
    /// A chat with no turn running is already in the state this asks for, so it
    /// is not an error.
    pub fn cancel(&self, session: SessionId) -> Result<(), ChatError> {
        let open = self.locked()?;
        let chat = open.get(&session).ok_or(ChatError::Unknown)?;

        if let Ok(running) = chat.running.lock()
            && let Some(turn) = running.as_ref()
        {
            turn.cancel();
        }

        Ok(())
    }

    /// Closes a chat session.
    ///
    /// Dropping the daemon's end of the inbox is the whole of it: the session's
    /// loop sees a channel nobody can send on again and ends, which lets the
    /// turn it may be running finish rather than tearing it off mid-provider
    /// call. A client that wants the turn stopped cancels first.
    pub fn close(&self, session: SessionId) -> Result<(), ChatError> {
        self.locked()?
            .remove(&session)
            .map(|_| ())
            .ok_or(ChatError::Unknown)
    }

    /// How many chats are open. Read by the tests, and by nothing that decides.
    #[must_use]
    pub fn open_chats(&self) -> usize {
        self.open.lock().map_or(0, |open| open.len())
    }

    fn is_running(&self, session: SessionId) -> bool {
        self.supervisor
            .status(session)
            .is_some_and(|status| status.state.terminal().is_none())
    }

    fn locked(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<SessionId, OpenChat>>, ChatError> {
        self.open.lock().map_err(|_| unusable("the chat registry"))
    }
}

/// One hosted chat's life: take a prompt, run it, publish what it did, repeat.
fn serve_prompts(
    turns: &mut dyn ChatTurns,
    runtime: &SessionRuntime,
    inbox: &Receiver<String>,
    subscribers: &Arc<Subscribers>,
    running: &Arc<Mutex<Option<HeadlessTurnCancellation>>>,
) -> SessionOutcome {
    let progress = progress_sink(subscribers);

    loop {
        if runtime.cancellation().is_cancelled() {
            subscribers.publish(&ChatEvent::Closed);
            return SessionOutcome::Cancelled;
        }

        let prompt = match inbox.recv_timeout(PROMPT_POLL) {
            Ok(prompt) => prompt,
            Err(RecvTimeoutError::Timeout) => continue,
            // The client closed the chat. Its end of the inbox is gone, which
            // is the only signal a hosted session gets that nobody will prompt
            // it again, and the only one it needs.
            Err(RecvTimeoutError::Disconnected) => {
                subscribers.publish(&ChatEvent::Closed);
                return SessionOutcome::Completed;
            }
        };

        if !runtime.budget().consume_turn() {
            subscribers.publish(&ChatEvent::TurnFailed {
                detail: "the chat session has spent its turns".to_owned(),
            });
            continue;
        }

        // A fresh one per turn, so a cancellation that arrived while the
        // previous answer was still being read cannot stop the next one.
        let cancellation = HeadlessTurnCancellation::new();
        if let Ok(mut turn) = running.lock() {
            *turn = Some(cancellation.clone());
        }

        let event = match turns.run(&prompt, runtime, &cancellation, &progress) {
            ChatTurnOutcome::Completed(text) => ChatEvent::TurnCompleted { text },
            ChatTurnOutcome::Failed(detail) => ChatEvent::TurnFailed { detail },
        };

        if let Ok(mut turn) = running.lock() {
            *turn = None;
        }

        subscribers.publish(&event);
    }
}

/// The sink a turn reports progress through: every [`TurnEvent`] it emits
/// reaches the chat's subscribers as it happens, rather than at the end.
fn progress_sink(subscribers: &Arc<Subscribers>) -> TurnProgressSink {
    let subscribers = Arc::clone(subscribers);

    Arc::new(move |event| subscribers.publish(&ChatEvent::Progress(event)))
}

fn unusable(component: &str) -> ChatError {
    ChatError::Unavailable(format!("{component} is unusable after a failed send"))
}

const fn describe(error: SessionRegistryError) -> &'static str {
    match error {
        SessionRegistryError::AlreadyLive => "a session with this id is already live",
        SessionRegistryError::AtCapacity => "the daemon holds as many sessions as it admits",
        SessionRegistryError::Unknown => "no such session",
        SessionRegistryError::Terminal => "the session already ended",
    }
}
