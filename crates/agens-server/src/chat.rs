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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use agens_core::ask_user::{AskUserAnswer, AskUserReply, AskUserRequest, AskUserUnavailable};
use agens_core::{
    HeadlessTurnCancellation, Message, MessagePart, SessionMessage, TurnEvent, TurnProgressSink,
};

use crate::sessions::{
    SessionAdmission, SessionId, SessionOutcome, SessionRegistryError, SessionRuntime,
    SessionSupervisor,
};

/// How often a turn waiting on a permission answer looks again at whether
/// anybody is still there to give one.
const ANSWER_POLL: Duration = Duration::from_millis(50);

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
/// A permission decision a hosted turn cannot make for itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatPermissionRequest {
    /// The bare tool name, as a person reads it.
    pub tool: String,
    pub target: String,
    pub access: String,
    pub reason: String,
}

/// What came back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatPermissionAnswer {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
    /// Nobody answered, and nobody is going to: every client watching this chat
    /// went away while the question was open.
    Unheard,
}

impl ChatPermissionAnswer {
    /// The name this answer crosses the wire under.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::AllowAlways => "allow_always",
            Self::DenyOnce => "deny_once",
            Self::DenyAlways => "deny_always",
            Self::Unheard => "unheard",
        }
    }

    /// The answer a client named, or `None` for a name this daemon does not
    /// know — which is refused rather than read as a permissive one.
    ///
    /// Not `FromStr`: that returns a `Result` whose error would have to say
    /// something, and there is nothing to say beyond the name being unknown,
    /// which the absence already says.
    #[must_use]
    pub fn parse(answer: &str) -> Option<Self> {
        match answer {
            "allow_once" => Some(Self::AllowOnce),
            "allow_always" => Some(Self::AllowAlways),
            "deny_once" => Some(Self::DenyOnce),
            "deny_always" => Some(Self::DenyAlways),
            _ => None,
        }
    }
}

/// How a hosted turn asks the person attached to it.
///
/// Handed to the turn rather than reached for, so a turn cannot ask a chat
/// other than its own.
pub trait ChatAsks: Send + Sync {
    /// Publishes a permission question and waits for the answer.
    ///
    /// It blocks, without a deadline, for the reason the terminal's own bridge
    /// does: a permission question is not something to time out and proceed
    /// past. What ends the wait besides an answer is everybody who could give
    /// one going away — a question nobody can hear is refused rather than left
    /// holding the turn forever.
    fn permission(&self, request: &ChatPermissionRequest) -> ChatPermissionAnswer;

    fn ask_user(&self, request: &AskUserRequest) -> AskUserReply;
}

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
        message: &SessionMessage,
        runtime: &SessionRuntime,
        cancellation: &HeadlessTurnCancellation,
        asks: &Arc<dyn ChatAsks>,
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

/// Where a chat's conversation is read back from.
///
/// A port for the same reason the factory is one: what a session's history is
/// made of — which messages are kept, which turns are somebody else's — belongs
/// to whoever writes it, and the control plane stores none of it.
pub type ChatHistorySource =
    Arc<dyn Fn(SessionId) -> Result<Vec<Message>, ChatError> + Send + Sync>;

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
    /// A decision the turn cannot make for itself, and the id an answer names.
    PermissionAsked {
        prompt_id: u64,
        request: ChatPermissionRequest,
    },
    TurnCompleted {
        text: String,
    },
    TurnFailed {
        detail: String,
    },
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
    /// An answer named a question this chat is not waiting on, or an answer
    /// this daemon does not know.
    NotAsked,
    /// The submitted user content is malformed or references unavailable media.
    InvalidMessage,
    Unavailable(String),
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => formatter.write_str("no such chat session"),
            Self::Busy => formatter.write_str("the chat session is already running a turn"),
            Self::NotAsked => formatter
                .write_str("the chat is not waiting on that question, or that is not an answer"),
            Self::InvalidMessage => formatter.write_str("the attached message is invalid"),
            Self::Unavailable(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for ChatError {}

/// One client's stream of a chat's events.
///
/// A guard rather than a bare `Receiver`, because the fan-out has to know a
/// client left without waiting for the next publish to discover it. A turn
/// stopped on a permission question publishes nothing while it waits, so a
/// count that only refreshed on publish would never notice that the person who
/// could answer had gone.
pub struct ChatSubscription {
    events: Receiver<ChatEvent>,
    /// Held for exactly as long as this subscription is, and counted through
    /// the weak handle the fan-out keeps.
    _listening: Arc<()>,
}

impl ChatSubscription {
    /// The next event, or what stopped it arriving.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<ChatEvent, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }
}

/// One client, and the handle that says whether it is still there.
struct Subscriber {
    outbound: SyncSender<ChatEvent>,
    listening: Weak<()>,
}

/// The subscribers of one hosted chat.
#[derive(Default)]
struct Subscribers(Mutex<Vec<Subscriber>>);

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

        subscribers.retain(
            |subscriber| match subscriber.outbound.try_send(event.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
            },
        );
    }

    /// How many clients are still listening.
    ///
    /// Read off the guards each subscription holds rather than off the channel,
    /// so it is true between publishes as well as after one.
    fn listeners(&self) -> usize {
        self.0.lock().map_or(0, |subscribers| {
            subscribers
                .iter()
                .filter(|subscriber| subscriber.listening.strong_count() > 0)
                .count()
        })
    }

    fn add(&self) -> Result<ChatSubscription, ChatError> {
        let (outbound, inbound) = sync_channel(SUBSCRIBER_BACKLOG);
        let listening = Arc::new(());

        self.0
            .lock()
            .map_err(|_| unusable("the chat fan-out"))?
            .push(Subscriber {
                outbound,
                listening: Arc::downgrade(&listening),
            });

        Ok(ChatSubscription {
            events: inbound,
            _listening: listening,
        })
    }
}

/// One open chat, as a client that came back sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenChatSummary {
    pub session_id: SessionId,
    /// The checkout it was opened against, which is how a terminal recognizes
    /// the chat that belongs to the project it is sitting in.
    pub checkout: PathBuf,
    /// Whether a turn is running right now, so a client that reattaches
    /// mid-answer can say so rather than looking idle.
    pub answering: bool,
}

/// A question this chat is waiting on an answer to.
struct PendingQuestion {
    answer: SyncSender<String>,
    admissible_answers: Vec<String>,
}

/// One open chat, as the daemon holds it.
struct OpenChat {
    /// Dropping this ends the session's loop, which is what closing a chat is.
    prompts: SyncSender<SessionMessage>,
    subscribers: Arc<Subscribers>,
    /// What the chat was opened against. Held so a client that comes back can
    /// find the chat belonging to the project it is in without having written
    /// its session id down.
    checkout: PathBuf,
    /// The turn running right now, when one is. Replaced per turn, so
    /// cancelling reaches the answer being produced and never the one before
    /// it.
    running: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
    /// The questions this chat is waiting on, by the id an answer names.
    asked: Arc<Mutex<BTreeMap<u64, PendingQuestion>>>,
}

/// The chat sessions this daemon is hosting.
pub struct ChatSessions {
    supervisor: SessionSupervisor,
    open_chat: ChatSessionFactory,
    history: ChatHistorySource,
    open: Mutex<BTreeMap<SessionId, OpenChat>>,
    media_store: Option<PathBuf>,
}

impl ChatSessions {
    #[must_use]
    pub fn new(
        supervisor: SessionSupervisor,
        open_chat: ChatSessionFactory,
        history: ChatHistorySource,
    ) -> Self {
        Self {
            supervisor,
            open_chat,
            history,
            open: Mutex::new(BTreeMap::new()),
            media_store: None,
        }
    }

    /// Enables validation of durable media references before inbox admission.
    #[must_use]
    pub fn with_media_store(mut self, data_directory: PathBuf) -> Self {
        self.media_store = Some(data_directory);
        self
    }

    /// What one chat has said so far.
    ///
    /// Read from where the session is stored rather than from the running
    /// chat's own memory: the turn owns that memory while it runs, and a reader
    /// that waited for it would be a reader blocked for as long as the answer
    /// takes. The store is behind the turn by at most the turn in flight, which
    /// is exactly the part `Subscribe` is already delivering.
    pub fn history(&self, session: SessionId) -> Result<Vec<Message>, ChatError> {
        if !self.locked()?.contains_key(&session) {
            return Err(ChatError::Unknown);
        }

        (self.history)(session)
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
        let asked = Arc::new(Mutex::new(BTreeMap::new()));
        let questions = Arc::clone(&asked);
        let checkout = request.checkout.clone();

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
                checkout,
                running,
                asked,
            },
        );
        drop(open);

        let started = self.supervisor.start(admission, move |runtime| {
            serve_prompts(
                turns.as_mut(),
                &runtime,
                &inbox,
                &published,
                &turn,
                &questions,
            )
        });

        if let Err(error) = started {
            self.locked()?.remove(&session);
            return Err(ChatError::Unavailable(describe(error).to_owned()));
        }

        Ok(session)
    }

    /// Hands a prompt to an open chat, without waiting for the turn it starts.
    pub fn prompt(&self, session: SessionId, message: SessionMessage) -> Result<(), ChatError> {
        if !self.locked()?.contains_key(&session) {
            return Err(ChatError::Unknown);
        }
        self.validate_media(&message)?;
        let open = self.locked()?;
        let chat = open.get(&session).ok_or(ChatError::Unknown)?;

        chat.prompts.try_send(message).map_err(|error| match error {
            TrySendError::Full(_) => ChatError::Busy,
            // The loop is gone, so the session behind this record has ended.
            TrySendError::Disconnected(_) => ChatError::Unknown,
        })
    }

    fn validate_media(&self, message: &SessionMessage) -> Result<(), ChatError> {
        let Some(data_directory) = self.media_store.as_deref() else {
            return Ok(());
        };

        for part in &message.as_message().parts {
            let MessagePart::Media { media_id, mime } = part else {
                continue;
            };
            if !agens_store::is_media_mime(mime) {
                return Err(ChatError::InvalidMessage);
            }
            let (stored_mime, _) = agens_store::open_media(data_directory, *media_id)
                .map_err(|_| ChatError::InvalidMessage)?;
            if stored_mime != *mime {
                return Err(ChatError::InvalidMessage);
            }
        }
        Ok(())
    }

    /// Answers a question a chat's turn is waiting on.
    ///
    /// A question this chat is not waiting on is refused rather than ignored:
    /// an answer to a question that already resolved is a person answering
    /// something other than what they are looking at, and letting it through
    /// silently would apply it to nothing.
    pub fn answer(
        &self,
        session: SessionId,
        prompt_id: u64,
        answer: ChatPermissionAnswer,
    ) -> Result<(), ChatError> {
        self.answer_value(session, prompt_id, answer.as_str())
    }

    pub fn answer_value(
        &self,
        session: SessionId,
        prompt_id: u64,
        answer: &str,
    ) -> Result<(), ChatError> {
        let open = self.locked()?;
        let chat = open.get(&session).ok_or(ChatError::Unknown)?;

        let mut asked = chat
            .asked
            .lock()
            .map_err(|_| unusable("the chat's open questions"))?;
        let admissible = asked
            .get(&prompt_id)
            .ok_or(ChatError::NotAsked)?
            .admissible_answers
            .iter()
            .any(|candidate| candidate == answer);
        if !admissible {
            return Err(ChatError::NotAsked);
        }
        let question = asked.remove(&prompt_id).ok_or(ChatError::NotAsked)?;

        question
            .answer
            .try_send(answer.to_owned())
            .map_err(|_| ChatError::NotAsked)
    }

    /// Opens a stream of one chat's events, live from now.
    ///
    /// It is not a replay: a client that wants what it missed while it was
    /// detached asks for the session's stored history, which is the projection
    /// built for exactly that.
    pub fn subscribe(&self, session: SessionId) -> Result<ChatSubscription, ChatError> {
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

    /// The chats open against `checkout`, newest first.
    ///
    /// Newest first because that is the order a person means. A terminal coming
    /// back to a project reattaches to the conversation it was last having
    /// there, and the daemon's ids are assigned in the order the chats were
    /// opened, so the largest is the last one.
    ///
    /// Scoped to a checkout, never listed whole: one daemon serves N projects,
    /// and a terminal offered another project's conversation would be a
    /// terminal that can attach to the wrong one.
    pub fn open_against(&self, checkout: &Path) -> Result<Vec<OpenChatSummary>, ChatError> {
        let open = self.locked()?;

        let mut chats = open
            .iter()
            .filter(|(_, chat)| chat.checkout == checkout)
            .filter(|(session, _)| self.is_running(**session))
            .map(|(session, chat)| OpenChatSummary {
                session_id: *session,
                checkout: chat.checkout.clone(),
                answering: chat.running.lock().is_ok_and(|running| running.is_some()),
            })
            .collect::<Vec<_>>();

        chats.sort_by_key(|chat| std::cmp::Reverse(chat.session_id.value()));

        Ok(chats)
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

/// The asking half of one chat, handed to each of its turns.
///
/// It owns the publishing and the waiting so a turn owns neither: what a turn
/// decides is what to ask, never who hears it or for how long.
struct ChatQuestions {
    subscribers: Arc<Subscribers>,
    asked: Arc<Mutex<BTreeMap<u64, PendingQuestion>>>,
    next_id: AtomicU64,
}

impl ChatAsks for ChatQuestions {
    fn permission(&self, request: &ChatPermissionRequest) -> ChatPermissionAnswer {
        let prompt_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = sync_channel(1);
        let admissible_answers = vec![
            ChatPermissionAnswer::AllowOnce.as_str().to_owned(),
            ChatPermissionAnswer::AllowAlways.as_str().to_owned(),
            ChatPermissionAnswer::DenyOnce.as_str().to_owned(),
            ChatPermissionAnswer::DenyAlways.as_str().to_owned(),
        ];

        let Ok(mut asked) = self.asked.lock() else {
            return ChatPermissionAnswer::Unheard;
        };
        asked.insert(
            prompt_id,
            PendingQuestion {
                answer,
                admissible_answers,
            },
        );
        drop(asked);

        self.subscribers.publish(&ChatEvent::PermissionAsked {
            prompt_id,
            request: request.clone(),
        });

        let outcome = self
            .wait_for(&answered)
            .and_then(|answer| ChatPermissionAnswer::parse(&answer))
            .unwrap_or(ChatPermissionAnswer::Unheard);

        if let Ok(mut asked) = self.asked.lock() {
            asked.remove(&prompt_id);
        }

        outcome
    }

    fn ask_user(&self, request: &AskUserRequest) -> AskUserReply {
        let [question] = request.questions() else {
            return AskUserReply::Unavailable(AskUserUnavailable::NoInteractiveSurface);
        };
        let admissible_answers = question
            .options()
            .iter()
            .map(|option| option.id().to_owned())
            .collect::<Vec<_>>();
        if admissible_answers.is_empty() {
            return AskUserReply::Unavailable(AskUserUnavailable::NoInteractiveSurface);
        }
        let prompt_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = sync_channel(1);
        let Ok(mut asked) = self.asked.lock() else {
            return AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed);
        };
        asked.insert(
            prompt_id,
            PendingQuestion {
                answer,
                admissible_answers,
            },
        );
        drop(asked);
        self.subscribers.publish(&ChatEvent::PermissionAsked {
            prompt_id,
            request: ChatPermissionRequest {
                tool: "ask_user".into(),
                target: String::new(),
                access: "ask_user".into(),
                reason: String::new(),
            },
        });

        let outcome = self.wait_for(&answered).map_or(
            AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed),
            |value| {
                AskUserReply::Answered(vec![AskUserAnswer {
                    question_id: question.id().to_owned(),
                    selected: vec![value],
                    other: None,
                    note: None,
                }])
            },
        );
        if let Ok(mut asked) = self.asked.lock() {
            asked.remove(&prompt_id);
        }
        outcome
    }
}

impl ChatQuestions {
    /// Waits for the answer, checking between waits that anybody is still there
    /// to give one.
    ///
    /// The subscriber count is read after the publish that would have dropped a
    /// client that went away, so it is the count of clients that could still
    /// answer rather than the count that existed when the question was asked.
    fn wait_for(&self, answered: &Receiver<String>) -> Option<String> {
        loop {
            match answered.recv_timeout(ANSWER_POLL) {
                Ok(answer) => return Some(answer),
                Err(RecvTimeoutError::Disconnected) => return None,
                Err(RecvTimeoutError::Timeout) if self.subscribers.listeners() == 0 => {
                    return None;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

/// One hosted chat's life: take a prompt, run it, publish what it did, repeat.
fn serve_prompts(
    turns: &mut dyn ChatTurns,
    runtime: &SessionRuntime,
    inbox: &Receiver<SessionMessage>,
    subscribers: &Arc<Subscribers>,
    running: &Arc<Mutex<Option<HeadlessTurnCancellation>>>,
    asked: &Arc<Mutex<BTreeMap<u64, PendingQuestion>>>,
) -> SessionOutcome {
    let progress = progress_sink(subscribers);
    let questions: Arc<dyn ChatAsks> = Arc::new(ChatQuestions {
        subscribers: Arc::clone(subscribers),
        asked: Arc::clone(asked),
        next_id: AtomicU64::new(0),
    });

    loop {
        if runtime.cancellation().is_cancelled() {
            subscribers.publish(&ChatEvent::Closed);
            return SessionOutcome::Cancelled;
        }

        let message = match inbox.recv_timeout(PROMPT_POLL) {
            Ok(message) => message,
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

        let event = match turns.run(&message, runtime, &cancellation, &questions, &progress) {
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
