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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// How long a command waits for the chat's serialized state to take it.
///
/// A command runs where the turns run, so it can only run between them. The
/// wait is bounded rather than open-ended because the caller is a terminal:
/// one that parks on this answer for the length of a turn is one that has
/// stopped drawing, and a refusal it can act on is worth more than an answer
/// it cannot wait for. Long enough for the loop's next look at its inbox,
/// short enough that nobody watches it.
const COMMAND_ANSWER: Duration = Duration::from_millis(500);

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
    /// Executes a command against this chat's daemon-owned session state.
    fn command(&mut self, _command: &str) -> Result<String, ChatError> {
        Err(ChatError::Unavailable(
            "the command is not supported".to_owned(),
        ))
    }

    /// How the session is configured right now.
    ///
    /// Read after a command has run, so a client that changed the model or the
    /// reasoning effort learns the new selection from the session that holds
    /// it rather than from the reply text. `None` for a chat that describes
    /// nothing, which a client reads as unchanged.
    fn presentation(&self) -> Option<ChatPresentation> {
        None
    }

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
    /// How the session is configured, for the open answer to describe.
    pub presentation: ChatPresentation,
}

/// The configuration a chat session holds, as the open answer describes it.
///
/// Computed by whoever builds the session — the factory reads the persisted
/// selection the same way a turn will — so an attaching surface renders the
/// configuration the session actually speaks with, not one the client guessed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChatPresentation {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub context_window: Option<u64>,
    /// Whether the session currently bypasses Ask permission prompts.
    pub bypass_permissions: bool,
    /// Whether the session currently runs in dangerous mode.
    pub dangerous_mode: bool,
}

/// What a hosted command answered, and how the session is configured after it.
///
/// The description is the session's own, read once the command has run, so a
/// command that changed the selection carries the change rather than leaving
/// the client to parse it out of the message.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChatCommandOutcome {
    pub message: String,
    pub presentation: Option<ChatPresentation>,
}

/// A chat session the daemon opened, and how it described it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenedChat {
    pub session: SessionId,
    pub presentation: ChatPresentation,
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
    AskUserAsked {
        prompt_id: u64,
        request: AskUserRequest,
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

    /// Adds a subscriber whose stream starts with `pending`, ahead of
    /// anything published after it joins.
    fn add_behind(&self, pending: Vec<ChatEvent>) -> Result<ChatSubscription, ChatError> {
        let (outbound, inbound) = sync_channel(SUBSCRIBER_BACKLOG);
        let listening = Arc::new(());

        for event in pending {
            outbound
                .try_send(event)
                .map_err(|_| unusable("the chat fan-out"))?;
        }

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
enum PendingQuestion {
    Permission {
        answer: SyncSender<String>,
        admissible_answers: Vec<String>,
    },
    AskUser {
        answer: SyncSender<AskUserReply>,
        request: AskUserRequest,
    },
}

enum ChatInput {
    Prompt(SessionMessage),
    Command {
        command: String,
        result: SyncSender<Result<ChatCommandOutcome, ChatError>>,
        /// Set when the caller stopped waiting for the answer. A command whose
        /// caller was told the chat is busy must not run later on its own:
        /// nobody would see what it did, and what it did would be a change
        /// they were told had not happened.
        abandoned: Arc<AtomicBool>,
    },
}

/// One open chat, as the daemon holds it.
struct OpenChat {
    /// Dropping this ends the session's loop, which is what closing a chat is.
    inputs: SyncSender<ChatInput>,
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
    pub fn open(&self, request: &ChatSessionRequest) -> Result<OpenedChat, ChatError> {
        let ChatSession {
            admission,
            mut turns,
            presentation,
        } = (self.open_chat)(request)?;

        let session = admission.session();
        let (inputs, inbox) = sync_channel(PROMPT_BACKLOG);
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
        // The daemon's chats outlive every client, so a client that comes back
        // opens an id that is usually still live. Opening it again from the
        // same checkout is the same conversation: the existing record and its
        // loop are kept as they are, because a fresh record here would hand
        // channels to nobody while the live loop keeps reading the old inbox.
        // The same id from another checkout is a genuine conflict, refused
        // without touching the live record. Both are decided under the lock
        // the pruning above ran under, so a loop that ended between a client's
        // listing and this open has already fallen out and restarts below.
        if let Some(existing) = open.get(&session) {
            if existing.checkout == request.checkout {
                // The description still comes from the fresh build above: the
                // factory reads the persisted selection, which a live session
                // keeps current every time its selection changes.
                return Ok(OpenedChat {
                    session,
                    presentation,
                });
            }
            return Err(ChatError::Unavailable(
                describe(SessionRegistryError::AlreadyLive).to_owned(),
            ));
        }
        open.insert(
            session,
            OpenChat {
                inputs,
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

        Ok(OpenedChat {
            session,
            presentation,
        })
    }

    /// Hands a prompt to an open chat, without waiting for the turn it starts.
    pub fn prompt(&self, session: SessionId, message: SessionMessage) -> Result<(), ChatError> {
        if !self.locked()?.contains_key(&session) {
            return Err(ChatError::Unknown);
        }
        self.validate_media(&message)?;
        let open = self.locked()?;
        let chat = open.get(&session).ok_or(ChatError::Unknown)?;

        chat.inputs
            .try_send(ChatInput::Prompt(message))
            .map_err(|error| match error {
                TrySendError::Full(_) => ChatError::Busy,
                // The loop is gone, so the session behind this record has ended.
                TrySendError::Disconnected(_) => ChatError::Unknown,
            })
    }

    /// Executes a command on the same serialized state that owns hosted turns.
    pub fn command(
        &self,
        session: SessionId,
        command: String,
    ) -> Result<ChatCommandOutcome, ChatError> {
        let (inputs, running) = {
            let open = self.locked()?;
            let chat = open.get(&session).ok_or(ChatError::Unknown)?;
            (chat.inputs.clone(), Arc::clone(&chat.running))
        };

        // A turn holds the state this command would run on for as long as it
        // takes to answer, so a chat that is answering is refused here rather
        // than queued behind the turn.
        if running.lock().is_ok_and(|turn| turn.is_some()) {
            return Err(ChatError::Busy);
        }

        let (result, answered) = sync_channel(1);
        let abandoned = Arc::new(AtomicBool::new(false));

        inputs
            .try_send(ChatInput::Command {
                command,
                result,
                abandoned: Arc::clone(&abandoned),
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => ChatError::Busy,
                TrySendError::Disconnected(_) => ChatError::Unknown,
            })?;

        match answered.recv_timeout(COMMAND_ANSWER) {
            Ok(answer) => answer,
            // A turn that started between the check above and this send owns
            // the loop now, so the command is withdrawn rather than left to
            // run whenever that turn ends.
            Err(RecvTimeoutError::Timeout) => {
                abandoned.store(true, Ordering::SeqCst);
                Err(ChatError::Busy)
            }
            Err(RecvTimeoutError::Disconnected) => Err(ChatError::Unknown),
        }
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
        match asked.get(&prompt_id) {
            Some(PendingQuestion::Permission {
                admissible_answers, ..
            }) => {
                if !admissible_answers
                    .iter()
                    .any(|candidate| candidate == answer)
                {
                    return Err(ChatError::NotAsked);
                }

                let Some(PendingQuestion::Permission { answer: sender, .. }) =
                    asked.remove(&prompt_id)
                else {
                    return Err(ChatError::NotAsked);
                };

                sender
                    .try_send(answer.to_owned())
                    .map_err(|_| ChatError::NotAsked)
            }
            Some(PendingQuestion::AskUser { request, .. }) => {
                let reply = bare_ask_user_reply(request, answer).ok_or(ChatError::NotAsked)?;

                let Some(PendingQuestion::AskUser { answer: sender, .. }) =
                    asked.remove(&prompt_id)
                else {
                    return Err(ChatError::NotAsked);
                };

                sender.try_send(reply).map_err(|_| ChatError::NotAsked)
            }
            None => Err(ChatError::NotAsked),
        }
    }

    pub fn answer_ask_user(
        &self,
        session: SessionId,
        prompt_id: u64,
        answer: AskUserReply,
    ) -> Result<(), ChatError> {
        let open = self.locked()?;
        let chat = open.get(&session).ok_or(ChatError::Unknown)?;
        let mut asked = chat
            .asked
            .lock()
            .map_err(|_| unusable("the chat's open questions"))?;
        let Some(PendingQuestion::AskUser { request, .. }) = asked.get(&prompt_id) else {
            return Err(ChatError::NotAsked);
        };
        request
            .validate_reply(&answer)
            .map_err(|_| ChatError::NotAsked)?;
        let Some(PendingQuestion::AskUser { answer: sender, .. }) = asked.remove(&prompt_id) else {
            return Err(ChatError::NotAsked);
        };

        sender.try_send(answer).map_err(|_| ChatError::NotAsked)
    }

    /// Opens a stream of one chat's events, live from now.
    ///
    /// It is not a replay: a client that wants what it missed while it was
    /// detached asks for the session's stored history, which is the projection
    /// built for exactly that. The one exception is a question the turn is
    /// still stopped on, which greets the subscriber so it stays answerable
    /// after everyone who heard it asked has detached.
    pub fn subscribe(&self, session: SessionId) -> Result<ChatSubscription, ChatError> {
        let open = self.locked()?;
        let chat = open.get(&session).ok_or(ChatError::Unknown)?;

        // Snapshotted and added under the questions lock, which asking also
        // publishes under: an open question is either in this snapshot or
        // published to the subscription after it joins, never both and never
        // neither.
        let asked = chat
            .asked
            .lock()
            .map_err(|_| unusable("the chat's open questions"))?;
        let pending = asked
            .iter()
            .filter_map(|(prompt_id, question)| match question {
                PendingQuestion::AskUser { request, .. } => Some(ChatEvent::AskUserAsked {
                    prompt_id: *prompt_id,
                    request: request.clone(),
                }),
                PendingQuestion::Permission { .. } => None,
            })
            .collect();

        chat.subscribers.add_behind(pending)
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
    /// Scoped to a checkout: one daemon serves N projects, and a terminal
    /// offered another project's conversation would be a terminal that can
    /// attach to the wrong one. The machine-wide view for the operator's
    /// read-only board is [`Self::open_chats_snapshot`], and nothing attaches
    /// through it.
    pub fn open_against(&self, checkout: &Path) -> Result<Vec<OpenChatSummary>, ChatError> {
        self.summaries(|chat| chat.checkout == checkout)
    }

    /// Every open chat on the machine, newest first.
    ///
    /// This exists for the operator's fleet board and for nothing that
    /// attaches: a terminal picking a conversation to join keeps going through
    /// [`Self::open_against`], which is scoped to the checkout it sits in.
    pub fn open_chats_snapshot(&self) -> Result<Vec<OpenChatSummary>, ChatError> {
        self.summaries(|_| true)
    }

    fn summaries(
        &self,
        wanted: impl Fn(&OpenChat) -> bool,
    ) -> Result<Vec<OpenChatSummary>, ChatError> {
        let open = self.locked()?;

        let mut chats = open
            .iter()
            .filter(|(_, chat)| wanted(chat))
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

    /// How many chats have a turn running right now, across every checkout.
    ///
    /// Machine-wide on purpose, unlike every listing: what reads it is the
    /// attach handshake deciding whether this daemon may be replaced, and a
    /// turn running for another project is exactly as much a reason not to as
    /// one running here.
    #[must_use]
    pub fn answering_chats(&self) -> usize {
        self.open.lock().map_or(0, |open| {
            open.values()
                .filter(|chat| chat.running.lock().is_ok_and(|running| running.is_some()))
                .count()
        })
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
    /// The session's own cancellation, which ends every question with it.
    session: HeadlessTurnCancellation,
    /// The cancellation of the turn currently running, when one is.
    running: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
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
            PendingQuestion::Permission {
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
        let prompt_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = sync_channel(1);
        let Ok(mut asked) = self.asked.lock() else {
            return AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed);
        };
        asked.insert(
            prompt_id,
            PendingQuestion::AskUser {
                answer,
                request: request.clone(),
            },
        );

        // Published while the questions lock is held, which `subscribe` also
        // snapshots under, so a joining subscriber cannot miss this question
        // or hear it twice.
        self.subscribers.publish(&ChatEvent::AskUserAsked {
            prompt_id,
            request: request.clone(),
        });
        drop(asked);

        // Held rather than given up when the last listener detaches: detaching
        // is not declining, and the question stays answerable through a later
        // subscriber or the fleet console. Only the turn ending calls it off.
        let outcome = loop {
            match answered.recv_timeout(ANSWER_POLL) {
                Ok(answer) => break answer,
                Err(RecvTimeoutError::Disconnected) => {
                    break AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed);
                }
                Err(RecvTimeoutError::Timeout) if self.asking_turn_ended() => {
                    break AskUserReply::Cancelled;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        };
        if let Ok(mut asked) = self.asked.lock() {
            asked.remove(&prompt_id);
        }
        outcome
    }
}

impl ChatQuestions {
    /// Whether the turn these questions belong to was called off, alone or
    /// with its whole session.
    fn asking_turn_ended(&self) -> bool {
        if self.session.is_cancelled() {
            return true;
        }

        self.running.lock().map_or(true, |running| {
            running
                .as_ref()
                .is_none_or(HeadlessTurnCancellation::is_cancelled)
        })
    }

    /// Waits for the answer, checking between waits that anybody is still there
    /// to give one.
    ///
    /// The subscriber count is read after the publish that would have dropped a
    /// client that went away, so it is the count of clients that could still
    /// answer rather than the count that existed when the question was asked.
    fn wait_for<T>(&self, answered: &Receiver<T>) -> Option<T> {
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
    inbox: &Receiver<ChatInput>,
    subscribers: &Arc<Subscribers>,
    running: &Arc<Mutex<Option<HeadlessTurnCancellation>>>,
    asked: &Arc<Mutex<BTreeMap<u64, PendingQuestion>>>,
) -> SessionOutcome {
    let progress = progress_sink(subscribers);
    let questions: Arc<dyn ChatAsks> = Arc::new(ChatQuestions {
        subscribers: Arc::clone(subscribers),
        asked: Arc::clone(asked),
        next_id: AtomicU64::new(0),
        session: runtime.cancellation().clone(),
        running: Arc::clone(running),
    });

    loop {
        if runtime.cancellation().is_cancelled() {
            subscribers.publish(&ChatEvent::Closed);
            return SessionOutcome::Cancelled;
        }

        let message = match inbox.recv_timeout(PROMPT_POLL) {
            Ok(ChatInput::Command {
                command,
                result,
                abandoned,
            }) => {
                if abandoned.load(Ordering::SeqCst) {
                    continue;
                }
                let answered = turns.command(&command).map(|message| ChatCommandOutcome {
                    message,
                    presentation: turns.presentation(),
                });
                drop(result.send(answered));
                continue;
            }
            Ok(ChatInput::Prompt(message)) => message,
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

/// Reads a bare value as the whole answer to a single-question ask.
///
/// A request with several questions has no single value that answers it, so
/// only the structured wire can resolve one of those.
fn bare_ask_user_reply(request: &AskUserRequest, value: &str) -> Option<AskUserReply> {
    let [question] = request.questions() else {
        return None;
    };

    let reply = AskUserReply::Answered(vec![AskUserAnswer {
        question_id: question.id().to_owned(),
        selected: vec![value.to_owned()],
        other: None,
        note: None,
    }]);

    request.validate_reply(&reply).ok().map(|()| reply)
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
