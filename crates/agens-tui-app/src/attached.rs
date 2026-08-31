//! The terminal as a client of a daemon rather than as the thing running the
//! turn.
//!
//! The difference from the local engine is only who runs the turn. The surface,
//! the conversation projection and every widget are the same, because what
//! arrives over the wire is [`agens_core::TurnEvent`] — the same value a local
//! turn reports — and the renderer never learns where it came from.
//!
//! What follows from that is the point of the change: closing the terminal ends
//! the client and not the turn. The daemon keeps the session running, and a
//! terminal that attaches again finds it where it was left — by asking the
//! daemon what is already open for this checkout, rather than by anybody having
//! written a session id down, and by drawing the conversation the daemon has
//! been having in the meantime.
//!
//! A permission question the daemon's turn is stopped on comes back on the same
//! stream and is answered on the same connection, so confirming a tool call
//! reads no differently from confirming one this process was running.
//!
//! This mode is still narrower than the local one and stays opt-in until it is
//! not. Hosted model and named-agent commands run in the daemon, while the skill
//! palette, the file picker and delegation are not wired here. A submission this
//! mode cannot serve is reported as such rather than silently doing nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use agens_bootstrap::Bootstrap;
use agens_coordinator_client::{
    ChatClient, ClientError, Coordinator, HostedChatEvent, PermissionDecision, PermissionQuestion,
};
use agens_core::hosted::{
    CatalogKind, CatalogResult, FileError, HostedControlCommand, HostedControlKind,
    HostedControlResult, HostedMcpAction, HostedMcpResult, HostedTaskReplay, TaskControlError,
    WorkspaceFile, WorkspaceFileContent,
};
use agens_core::{
    HeadlessTurnCancellation, Message, MessagePart, SessionMessage, SubmitOrigin, TurnEvent,
};
use agens_error::{CliError, ExitStatus};
use agens_session::context::SessionContext;
use agens_tui::{
    Engine, Tui, TuiAskUserBridge, TuiPermissionBridge, TuiPermissionReply,
    run_with_default_progress_submit_with_permissions_task_controls_and_ask_user,
};
use tokio::runtime::Runtime;
use tokio_stream::{Stream, StreamExt};

#[cfg(test)]
use agens_tui::{TuiRouteRequest, TuiSubmissionOutcome};

use crate::router::{AttachedRouteBackend, TuiRuntimeRouter, tui_provider_outcome};

/// What this mode cannot serve yet, said the same way everywhere it comes up.
#[cfg(test)]
const UNSUPPORTED: &str =
    "not available while attached to a daemon yet; start without attaching for this";

type Events = std::pin::Pin<Box<dyn Stream<Item = Result<HostedChatEvent, ClientError>> + Send>>;

static NEXT_ATTACHMENT_NONCE: AtomicU64 = AtomicU64::new(1);

fn control_command_id(session: i64, nonce: u64, sequence: u64) -> String {
    format!("attached-tui-{session}-{nonce}-{sequence}")
}

/// The connection the terminal holds while it is attached.
///
/// The event stream is opened once and drained per turn rather than
/// resubscribed per prompt. A subscription is live from the moment it opens, so
/// opening one after sending the prompt is a race the client loses by missing
/// the turn's first events.
struct Attachment {
    runtime: Arc<Runtime>,
    chat: Mutex<ChatClient>,
    events: Mutex<Events>,
    session_id: i64,
    checkout: PathBuf,
    attachment_nonce: u64,
    next_control_id: AtomicU64,
    /// Where a permission question the daemon asked is put to the person, and
    /// where their answer comes back from.
    permissions: TuiPermissionBridge,
    ask_user: TuiAskUserBridge,
    staging: Arc<Mutex<SessionContext>>,
}

impl Attachment {
    fn command(&self, command: &str) -> Result<String, CliError> {
        let mut chat = self
            .chat
            .lock()
            .map_err(|_| unavailable("the connection to the daemon is unusable"))?;

        self.runtime
            .block_on(chat.command(self.session_id, command))
            .map_err(refused)
    }

    /// Sends one prompt and reports what the turn did, forwarding everything it
    /// produced to the surface as it happens.
    fn take_turn(
        &self,
        message: &Message,
        progress: &Sender<TurnEvent>,
    ) -> Result<String, CliError> {
        let asking = HeadlessTurnCancellation::new();
        let mut chat = self
            .chat
            .lock()
            .map_err(|_| unavailable("the connection to the daemon is unusable"))?;
        let mut events = self
            .events
            .lock()
            .map_err(|_| unavailable("the chat's event stream is unusable"))?;

        let message = SessionMessage::try_from(message.clone())
            .map_err(|_| CliError::usage("submitted message is invalid"))?;
        self.runtime
            .block_on(chat.prompt_message(self.session_id, &message))
            .map_err(rejected)?;
        self.claim_accepted_media(&message);

        self.follow_turn(&mut chat, &mut events, &asking, progress)
    }

    /// Adopts the turn the daemon is already running, without prompting.
    ///
    /// A terminal attaching mid-turn finds the turn's events — including a
    /// question it is stopped on, which the subscription greeting replays —
    /// waiting on the stream it subscribed to. Following them from here is
    /// what makes the arrival read as if this terminal had been attached all
    /// along.
    fn adopt_turn(&self, progress: &Sender<TurnEvent>) -> Result<String, CliError> {
        let asking = HeadlessTurnCancellation::new();
        let mut chat = self
            .chat
            .lock()
            .map_err(|_| unavailable("the connection to the daemon is unusable"))?;
        let mut events = self
            .events
            .lock()
            .map_err(|_| unavailable("the chat's event stream is unusable"))?;

        self.follow_turn(&mut chat, &mut events, &asking, progress)
    }

    /// Follows one turn's events to its end, forwarding progress and putting
    /// its questions to the person. The same loop whether this client started
    /// the turn or adopted one already running.
    fn follow_turn(
        &self,
        chat: &mut ChatClient,
        events: &mut Events,
        asking: &HeadlessTurnCancellation,
        progress: &Sender<TurnEvent>,
    ) -> Result<String, CliError> {
        loop {
            let event = self
                .runtime
                .block_on(events.next())
                .ok_or_else(|| unavailable("the daemon stopped publishing this chat"))?
                .map_err(refused)?;

            match event {
                HostedChatEvent::Progress(event) => {
                    // A surface that has gone away is not a turn that has to
                    // stop. The daemon is running it either way, and all this
                    // client does about it is stop forwarding.
                    if progress.send(event).is_err() {
                        return Err(unavailable("the terminal stopped listening"));
                    }
                }
                HostedChatEvent::PermissionAsked(question) => {
                    // Answered on this thread, which is the one draining the
                    // stream. The daemon's turn is stopped on the question, so
                    // there is nothing else on this chat to miss while a person
                    // decides — and the surface is drawing the overlay from its
                    // own thread meanwhile.
                    let decision = self.decide(&question, asking);

                    self.runtime
                        .block_on(chat.answer_permission(
                            self.session_id,
                            question.prompt_id,
                            decision,
                        ))
                        .map_err(refused)?;
                }
                HostedChatEvent::AskUserAsked { prompt_id, request } => {
                    let reply = self.ask_user.wait_for_reply(request, None, asking);

                    // A question someone else already resolved — through the
                    // fleet console, or replayed from before a reattach — is
                    // not this turn failing. The daemon says which it was, and
                    // the turn's remaining events are still coming.
                    match self.runtime.block_on(chat.answer_ask_user(
                        self.session_id,
                        prompt_id,
                        reply,
                    )) {
                        Ok(()) => {}
                        Err(error) if error.refused_precondition() => {}
                        Err(error) => return Err(refused(error)),
                    }
                }
                HostedChatEvent::TurnCompleted { text } => return Ok(text),
                HostedChatEvent::TurnFailed { detail } => {
                    return Err(CliError::new(ExitStatus::Failure, "provider", detail));
                }
                HostedChatEvent::Closed => {
                    return Err(unavailable(
                        "the chat was closed while the turn was running",
                    ));
                }
            }
        }
    }
}

impl Attachment {
    fn claim_accepted_media(&self, message: &SessionMessage) {
        let Ok(mut staging) = self.staging.lock() else {
            return;
        };
        for part in &message.as_message().parts {
            let MessagePart::Media { media_id, mime } = part else {
                continue;
            };
            if let Some(index) = staging
                .pending_media_ids
                .iter()
                .zip(&staging.pending_media_mimes)
                .position(|(candidate_id, candidate_mime)| {
                    candidate_id == media_id && candidate_mime == mime
                })
            {
                staging.pending_media_ids.remove(index);
                staging.pending_media_mimes.remove(index);
            }
        }
    }

    /// Puts one question to the person and waits for their answer.
    ///
    /// A cancelled prompt is answered as a refusal rather than left open. The
    /// daemon's turn is stopped on it either way, and the cancellation the
    /// person asked for reaches the turn through `cancel`, not through a
    /// question nobody resolves.
    fn decide(
        &self,
        question: &PermissionQuestion,
        asking: &HeadlessTurnCancellation,
    ) -> PermissionDecision {
        let reply = self.permissions.wait_for_reply(
            question.tool.clone(),
            question.target.clone(),
            question.access.clone(),
            Some(question.reason.clone()),
            None,
            asking,
        );

        match reply {
            TuiPermissionReply::AllowOnce => PermissionDecision::AllowOnce,
            TuiPermissionReply::AllowAlways => PermissionDecision::AllowAlways,
            TuiPermissionReply::DenyAlways => PermissionDecision::DenyAlways,
            TuiPermissionReply::DenyOnce
            | TuiPermissionReply::Cancelled
            | TuiPermissionReply::DeadlineExpired => PermissionDecision::DenyOnce,
        }
    }
}

/// Cancellation, for a turn that is not running in this process.
///
/// It holds a chat client of its own rather than sharing the one a turn is
/// blocked on: the surface asks to cancel from the thread drawing it while the
/// turn is parked on the stream, and reaching through the same lock would mean
/// the cancellation waits for the thing it is cancelling.
impl AttachedRouteBackend for Attachment {
    fn catalog(&self, kind: CatalogKind) -> Result<CatalogResult, CliError> {
        let mut chat = self
            .chat
            .lock()
            .map_err(|_| unavailable("the connection to the daemon is unusable"))?;
        self.runtime
            .block_on(chat.catalog(kind, None))
            .map_err(refused)
    }

    fn command(&self, command: &str) -> Result<String, CliError> {
        self.command(command)
    }

    fn list_files(
        &self,
        selector: &Path,
    ) -> Result<Result<Vec<WorkspaceFile>, FileError>, CliError> {
        let mut chat = self
            .chat
            .lock()
            .map_err(|_| unavailable("the connection to the daemon is unusable"))?;
        self.runtime
            .block_on(chat.list_workspace_files(&self.checkout, selector))
            .map_err(refused)
    }

    fn read_file(
        &self,
        selector: &Path,
    ) -> Result<Result<WorkspaceFileContent, FileError>, CliError> {
        let mut chat = self
            .chat
            .lock()
            .map_err(|_| unavailable("the connection to the daemon is unusable"))?;
        self.runtime
            .block_on(chat.read_workspace_file(&self.checkout, selector))
            .map_err(refused)
    }

    fn mcp_status(&self) -> Result<HostedMcpResult, CliError> {
        let mut chat = self
            .chat
            .lock()
            .map_err(|_| unavailable("the connection to the daemon is unusable"))?;
        self.runtime.block_on(chat.mcp_status()).map_err(refused)
    }

    fn mcp_control(
        &self,
        server: &str,
        action: HostedMcpAction,
    ) -> Result<HostedMcpResult, CliError> {
        let mut chat = self
            .chat
            .lock()
            .map_err(|_| unavailable("the connection to the daemon is unusable"))?;
        self.runtime
            .block_on(chat.mcp_control(server, action))
            .map_err(refused)
    }

    fn task_snapshot(&self) -> Result<Result<HostedTaskReplay, TaskControlError>, CliError> {
        let mut chat = self
            .chat
            .lock()
            .map_err(|_| unavailable("the connection to the daemon is unusable"))?;
        self.runtime
            .block_on(chat.task_snapshot(self.session_id))
            .map_err(refused)
    }

    fn task_control(
        &self,
        kind: HostedControlKind,
        task_id: Option<u64>,
    ) -> Result<Result<HostedControlResult, TaskControlError>, CliError> {
        let command_id = control_command_id(
            self.session_id,
            self.attachment_nonce,
            self.next_control_id.fetch_add(1, Ordering::Relaxed),
        );
        let command = HostedControlCommand::new(
            self.session_id,
            task_id.map(|id| id.to_string()),
            command_id,
            kind,
        );
        let mut chat = self
            .chat
            .lock()
            .map_err(|_| unavailable("the connection to the daemon is unusable"))?;
        self.runtime
            .block_on(chat.task_control(&command))
            .map_err(refused)
    }
}

struct AttachedEngine {
    runtime: Arc<Runtime>,
    chat: Mutex<ChatClient>,
    session_id: i64,
}

impl Engine for AttachedEngine {
    /// Stops the answer, never the conversation. The daemon's `cancel` ends the
    /// running turn and leaves the session open, so the next prompt still has
    /// somewhere to arrive.
    fn cancel(&mut self) {
        if let Ok(mut chat) = self.chat.lock() {
            // Nothing to report it to. This trait hands the surface no way back,
            // and a cancellation the daemon refused is one the person can see
            // for themselves: the answer keeps arriving, and Esc is still
            // there. A chat with no turn running already answers this as done.
            let _ = self.runtime.block_on(chat.cancel(self.session_id));
        }
    }
}

/// What a submission means in this mode.
///
/// Only an ordinary prompt becomes a turn. Supported stateful commands execute
/// against the daemon-owned chat. Everything else the surface can route belongs
/// to the local router, and a command that quietly did nothing would read as the
/// terminal being broken.
#[cfg(test)]
fn route(
    request: &TuiRouteRequest,
    execute_command: impl Fn(&str) -> Result<String, CliError>,
) -> TuiSubmissionOutcome {
    match request {
        TuiRouteRequest::Input(input) => {
            let trimmed = input.trim();

            if trimmed.is_empty() {
                return TuiSubmissionOutcome::LocalInfo(String::new());
            }

            if trimmed == "/agents"
                || trimmed.starts_with("/agent ")
                || trimmed.starts_with("/effort ")
                || trimmed.starts_with("/model ")
            {
                return match execute_command(trimmed) {
                    Ok(message) => TuiSubmissionOutcome::LocalInfo(message),
                    Err(error) => TuiSubmissionOutcome::LocalActionableError {
                        message: error.to_string(),
                        action: "Correct the command or runtime condition, then retry.".to_owned(),
                    },
                };
            }

            if trimmed.starts_with('/') {
                return TuiSubmissionOutcome::LocalInfo(format!("commands are {UNSUPPORTED}"));
            }

            TuiSubmissionOutcome::ProviderTurn {
                display: input.clone(),
                prompt: input.clone(),
            }
        }
        // A prompt sent while a turn is running is refused with the draft kept,
        // because the daemon accepts one waiting prompt and no more; queueing
        // it here would only move the refusal later.
        TuiRouteRequest::BusyInput(_) => TuiSubmissionOutcome::BusyRefusal(
            "the daemon is still answering; wait for it or press Esc".to_owned(),
        ),
        _ => TuiSubmissionOutcome::LocalInfo(format!("that is {UNSUPPORTED}")),
    }
}

/// Attaches to the daemon on `socket` and runs the terminal against it.
///
/// `resume` names a stored session to continue. Absent, this comes back to the
/// chat already open for this checkout, and opens a fresh one only when there
/// is none.
pub fn run_attached_tui(
    bootstrap: &Bootstrap,
    socket: &Path,
    resume: Option<i64>,
) -> Result<String, CliError> {
    run_attached_tui_with_prompt(bootstrap, socket, resume, None, None)
}

/// `startup_notice` is what the launch has to say before the arrival does:
/// the one thing it carries today is that this launch just started the daemon
/// the arrival is about to describe.
pub fn run_attached_tui_with_prompt(
    bootstrap: &Bootstrap,
    socket: &Path,
    resume: Option<i64>,
    initial_prompt: Option<&str>,
    startup_notice: Option<&str>,
) -> Result<String, CliError> {
    let checkout = bootstrap
        .project_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));

    let staging = Arc::new(Mutex::new(SessionContext::fresh()));
    let (permissions, permission_requests) = TuiPermissionBridge::channel();
    let (ask_user, ask_user_requests) = TuiAskUserBridge::channel();
    let (attachment, engine, arrival) = attach(
        socket,
        &checkout,
        resume,
        permissions,
        ask_user,
        Arc::clone(&staging),
    )?;

    let mut tui = Tui::new(engine);
    tui.adopt_environment();
    tui.set_collapse_thinking(bootstrap.collapse_thinking);
    // Dressed before the first frame, so the footer opens naming the model the
    // daemon is actually speaking to, the way a local launch opens naming its
    // own. A daemon that did not describe the chat leaves the placeholders.
    if let Some(presentation) = arrival_presentation(&arrival) {
        tui.apply_presentation(presentation);
    }
    for notice in opening_notices(startup_notice, &arrival) {
        tui.add_info(notice);
    }
    let attachment = Arc::new(attachment);
    let router =
        TuiRuntimeRouter::attached(bootstrap.clone(), Arc::clone(&staging), attachment.clone());
    tui.set_palette_entries(router.attached_palette_entries()?);
    tui.set_file_candidates(router.attached_file_candidates()?);
    if let Some(prompt) = initial_prompt {
        tui.set_composer_draft(prompt);
    }

    // Drawn through the surface's own projection, so a conversation the daemon
    // held reads exactly as one this process held. A hosted chat delegates
    // nothing yet, so there are no out-of-band subagent turns to filter — the
    // unit that gives it a task runtime is the one that will need that.
    if let Err(error) = tui.replace_history(&arrival.history) {
        tui.add_info(format!(
            "the conversation so far could not be drawn: {error:?}"
        ));
    }

    for event in router.attached_task_events()? {
        tui.apply_runtime_event(event);
    }

    // A chat still answering has a turn to adopt: its remaining events are on
    // the subscription already opened, and the runtime drains them from its
    // first frame — so progress renders and a question the turn is stopped on
    // opens the overlay without a submission from this terminal.
    if arrival.adopts_live_turn() {
        tui.adopt_running_turn();
    }

    let permissions = attachment.permissions.clone();
    let asks = attachment.ask_user.clone();
    let route_router = router.clone();
    let background_router = router.clone();
    let cancel_router = router.clone();
    let cancel_all_router = router.clone();
    let message_router = router.clone();
    run_with_default_progress_submit_with_permissions_task_controls_and_ask_user(
        &mut tui,
        move |request, progress, cancellation| {
            route_router.route_attached_request(request, progress, cancellation)
        },
        move |message, origin, progress, _metrics| {
            if origin == SubmitOrigin::Adopted {
                attached_provider_outcome(attachment.adopt_turn(&progress))
            } else if let Some(rejection) = unsupported_attached_origin(origin) {
                rejection
            } else {
                attached_provider_outcome(attachment.take_turn(&message, &progress))
            }
        },
        move |id| background_router.attached_background_task(id),
        move |id| cancel_router.attached_cancel_task(id),
        move || cancel_all_router.attached_cancel_all_tasks(),
        move |id, message| message_router.attached_send_task_message(id, message),
        Some((permissions, permission_requests)),
        Some((asks, ask_user_requests)),
    )
    .map_err(|error| CliError::new(ExitStatus::Failure, "ui", error.to_string()))?;

    Ok(String::new())
}

/// The lines the terminal shows before the first prompt, in order.
///
/// The daemon-startup notice comes first when there is one, so a person reads
/// that the daemon exists before they read where they landed on it.
fn opening_notices(startup_notice: Option<&str>, arrival: &Arrival) -> Vec<String> {
    let mut notices = Vec::new();

    if let Some(notice) = startup_notice {
        notices.push(notice.to_owned());
    }

    notices.push(arrival.describe());
    notices.push(
        "attached mode uses daemon-owned commands, skills, files, MCP state, and tasks".to_owned(),
    );

    notices
}

/// How this terminal came to the chat it is now on, and what was already
/// said there.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Arrival {
    session_id: i64,
    landing: Landing,
    /// What the chat has said so far. Empty for one this terminal just opened.
    history: Vec<Message>,
    /// What the daemon said the chat is configured as when it was opened.
    presentation: HostedPresentation,
}

/// The hosted session's active configuration, as the open answer carried it.
///
/// Every field is optional because a daemon that predates the description
/// answers with none of them, and an arrival that invented values for such a
/// daemon would dress the footer with a configuration nobody holds.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct HostedPresentation {
    provider: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    context_window: Option<u64>,
    bypass_permissions: bool,
    dangerous_mode: bool,
}

/// The footer presentation this arrival earns, or `None` when the daemon did
/// not describe the chat and the placeholders are the honest rendering.
///
/// Everything shown comes from the daemon's description and nothing else: the
/// daemon owns the session's configuration, so a window it did not hold stays
/// empty rather than being looked up in this client's own model registry. The
/// provider falls back to the same placeholder a local launch uses, and the
/// session label is the same `session #id`.
fn arrival_presentation(arrival: &Arrival) -> Option<agens_tui::TuiPresentation> {
    let described = &arrival.presentation;
    let model = described
        .model
        .as_deref()
        .filter(|model| !model.is_empty())?;
    let provider = described
        .provider
        .as_deref()
        .filter(|provider| !provider.is_empty())
        .unwrap_or("provider");

    let window = described.context_window;
    let mut presentation = agens_tui::TuiPresentation::new(
        provider,
        model,
        format!("session #{}", arrival.session_id),
    )
    .with_context_window(window)
    .with_bypass(described.bypass_permissions)
    .with_dangerous_mode(described.dangerous_mode);

    if let Some(effort) = &described.reasoning_effort {
        presentation = presentation.with_effort(effort.clone());
    }

    Some(presentation)
}

/// Whether this terminal started the conversation or came back to it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Landing {
    Opened,
    CameBack { answering: bool },
}

impl Arrival {
    fn opened(session_id: i64) -> Self {
        Self {
            session_id,
            landing: Landing::Opened,
            history: Vec::new(),
            presentation: HostedPresentation::default(),
        }
    }

    /// Whether this terminal landed on a turn that is still running — the one
    /// it adopts instead of waiting for a submission of its own.
    const fn adopts_live_turn(&self) -> bool {
        matches!(self.landing, Landing::CameBack { answering: true })
    }

    fn describe(&self) -> String {
        let session_id = self.session_id;

        match self.landing {
            Landing::Opened => {
                format!("attached to the daemon as session {session_id}; leaving does not stop it")
            }
            Landing::CameBack { answering: false } => {
                format!("back on session {session_id}, where you left it")
            }
            Landing::CameBack { answering: true } => {
                format!("back on session {session_id}, which is still answering")
            }
        }
    }
}

fn attach(
    socket: &Path,
    checkout: &Path,
    resume: Option<i64>,
    permissions: TuiPermissionBridge,
    ask_user: TuiAskUserBridge,
    staging: Arc<Mutex<SessionContext>>,
) -> Result<(Attachment, AttachedEngine, Arrival), CliError> {
    let runtime = Arc::new(
        Runtime::new()
            .map_err(|error| CliError::new(ExitStatus::Failure, "ui", error.to_string()))?,
    );

    let coordinator = runtime
        .block_on(Coordinator::attach(socket))
        .map_err(refused)?;
    let mut chat = coordinator.chat();

    // A named session is what the person asked for and is not second-guessed;
    // otherwise the chat already open for this checkout is the one they mean,
    // because a terminal that opened a second one beside it would leave the
    // first answering into a stream nobody reads.
    let mut arrival = match resume {
        // A named session is the person saying which conversation they want, so
        // it is not second-guessed and not looked up in the listing first.
        Some(session_id) => {
            let opened = runtime
                .block_on(chat.open(checkout, Some(session_id)))
                .map_err(refused)?;

            Arrival {
                session_id: opened.session_id,
                landing: Landing::CameBack { answering: false },
                history: Vec::new(),
                presentation: described_presentation(&opened),
            }
        }
        None => rejoin_or_open(&runtime, &mut chat, checkout)?,
    };

    // Read after the arrival is settled and before the subscription opens, so
    // nothing said between the two is drawn twice or missed: what is in the
    // history is what the store already holds, and everything after it arrives
    // on the stream.
    arrival.history = runtime
        .block_on(chat.history(arrival.session_id))
        .map_err(refused)?;

    let session_id = arrival.session_id;
    let events = runtime
        .block_on(chat.subscribe(session_id))
        .map_err(refused)?;

    // Whether the chat is still answering is re-read after the subscription
    // opened, which closes the adoption race: a turn seen running here
    // publishes everything it still does — including its end — onto the
    // stream just opened, while a turn that ended before the subscription can
    // no longer be seen answering and is not adopted.
    if matches!(arrival.landing, Landing::CameBack { .. }) {
        let answering = runtime
            .block_on(chat.open_against(checkout))
            .map_err(refused)?
            .iter()
            .any(|open| open.session_id == session_id && open.answering);
        arrival.landing = Landing::CameBack { answering };
    }

    let engine = AttachedEngine {
        runtime: Arc::clone(&runtime),
        chat: Mutex::new(coordinator.chat()),
        session_id,
    };

    Ok((
        Attachment {
            runtime,
            chat: Mutex::new(chat),
            events: Mutex::new(Box::pin(events)),
            session_id,
            checkout: checkout.to_path_buf(),
            attachment_nonce: NEXT_ATTACHMENT_NONCE.fetch_add(1, Ordering::Relaxed),
            next_control_id: AtomicU64::new(1),
            permissions,
            ask_user,
            staging,
        },
        engine,
        arrival,
    ))
}

/// Comes back to this checkout's chat, or opens one when there is none.
///
/// The newest is the one taken when several are open. That is the order the
/// daemon lists them in and the one a person means: the conversation they were
/// last having here.
fn rejoin_or_open(
    runtime: &Runtime,
    chat: &mut ChatClient,
    checkout: &Path,
) -> Result<Arrival, CliError> {
    let open = runtime
        .block_on(chat.open_against(checkout))
        .map_err(refused)?;

    if let Some(existing) = open.first() {
        // Opening it by id rather than trusting the listing: between the two
        // calls the chat can end, and `open` is what settles that — either it
        // returns the same session, or it opens a fresh one and this terminal
        // is where a new conversation starts.
        let opened = runtime
            .block_on(chat.open(checkout, Some(existing.session_id)))
            .map_err(refused)?;

        if opened.session_id == existing.session_id {
            return Ok(Arrival {
                session_id: opened.session_id,
                landing: Landing::CameBack {
                    answering: existing.answering,
                },
                history: Vec::new(),
                presentation: described_presentation(&opened),
            });
        }

        return Ok(Arrival {
            presentation: described_presentation(&opened),
            ..Arrival::opened(opened.session_id)
        });
    }

    let opened = runtime
        .block_on(chat.open(checkout, None))
        .map_err(refused)?;

    Ok(Arrival {
        presentation: described_presentation(&opened),
        ..Arrival::opened(opened.session_id)
    })
}

/// What the open answer said about the chat's configuration, in the arrival's
/// own shape.
fn described_presentation(opened: &agens_coordinator_client::OpenedChat) -> HostedPresentation {
    HostedPresentation {
        provider: opened.provider.clone(),
        model: opened.model.clone(),
        reasoning_effort: opened.reasoning_effort.clone(),
        context_window: opened.context_window,
        bypass_permissions: opened.bypass_permissions,
        dangerous_mode: opened.dangerous_mode,
    }
}

fn unsupported_attached_origin(origin: SubmitOrigin) -> Option<agens_tui::TuiProviderOutcome> {
    (origin != SubmitOrigin::User).then(|| agens_tui::TuiProviderOutcome::Failed {
        message: "selected subagent and background submissions are not supported while attached"
            .into(),
        action: "Select the main conversation and submit an ordinary queued prompt.".into(),
    })
}

fn attached_provider_outcome(result: Result<String, CliError>) -> agens_tui::TuiProviderOutcome {
    match result {
        Err(error) if error.category == "attached_rejected" => {
            agens_tui::TuiProviderOutcome::Rejected {
                message: error.to_string(),
                action: "Correct the attachment or retry after updating the daemon.".into(),
            }
        }
        result => tui_provider_outcome(result),
    }
}

fn rejected(error: ClientError) -> CliError {
    let category = if error.definitively_rejected_prompt() {
        "attached_rejected"
    } else {
        "provider"
    };
    CliError::new(ExitStatus::Failure, category, error.to_string())
}

/// A daemon that is not there is reported as unavailable rather than as a
/// failed request: nothing was wrong with what was asked, there is simply
/// nothing running to ask.
fn refused(error: ClientError) -> CliError {
    match error {
        ClientError::NotRunning(_) => CliError::unavailable(
            "no daemon is running; start one with `agens serve`, or run `agens --local`",
        ),
        // The rest is the daemon speaking — a refusal, an unreadable answer, a
        // request this client would not send — and none of it is a provider
        // turn failing. Those arrive on the event stream and keep their own
        // category.
        other => CliError::new(ExitStatus::Failure, "daemon", other.to_string()),
    }
}

fn unavailable(detail: &str) -> CliError {
    CliError::unavailable(detail.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_without_commands(request: &TuiRouteRequest) -> TuiSubmissionOutcome {
        route(request, |_| {
            panic!("this route does not execute a hosted command")
        })
    }

    #[test]
    fn an_ordinary_prompt_becomes_a_turn_for_the_daemon_to_run() {
        assert_eq!(
            route_without_commands(&TuiRouteRequest::Input("what changed here".to_owned())),
            TuiSubmissionOutcome::ProviderTurn {
                display: "what changed here".to_owned(),
                prompt: "what changed here".to_owned(),
            },
        );
    }

    #[test]
    fn effort_executes_as_a_hosted_command() {
        let outcome = route(
            &TuiRouteRequest::Input("/effort high".to_owned()),
            |command| Ok(format!("executed:{command}")),
        );

        assert_eq!(
            outcome,
            TuiSubmissionOutcome::LocalInfo("executed:/effort high".to_owned())
        );
    }

    #[test]
    fn agent_catalog_executes_as_a_hosted_command() {
        let outcome = route(&TuiRouteRequest::Input("/agents".to_owned()), |command| {
            Ok(format!("executed:{command}"))
        });

        assert_eq!(
            outcome,
            TuiSubmissionOutcome::LocalInfo("executed:/agents".to_owned())
        );
    }

    #[test]
    fn named_agent_executes_as_a_hosted_command() {
        let outcome = route(
            &TuiRouteRequest::Input("/agent all".to_owned()),
            |command| Ok(format!("executed:{command}")),
        );

        assert_eq!(
            outcome,
            TuiSubmissionOutcome::LocalInfo("executed:/agent all".to_owned())
        );
    }

    #[test]
    fn control_ids_do_not_collide_between_attachments() {
        assert_ne!(control_command_id(7, 1, 1), control_command_id(7, 2, 1));
    }

    #[test]
    fn qualified_model_executes_as_a_hosted_command() {
        let outcome = route(
            &TuiRouteRequest::Input("/model openai-api/gpt-4.1".to_owned()),
            |command| Ok(format!("executed:{command}")),
        );

        assert_eq!(
            outcome,
            TuiSubmissionOutcome::LocalInfo("executed:/model openai-api/gpt-4.1".to_owned())
        );
    }

    #[test]
    fn a_command_this_mode_cannot_serve_says_so_rather_than_doing_nothing() {
        let TuiSubmissionOutcome::LocalInfo(message) =
            route_without_commands(&TuiRouteRequest::Input("/model".to_owned()))
        else {
            panic!("a command is not a turn for the daemon to run");
        };

        assert!(message.contains("commands are"), "{message}");
        assert!(
            message.contains("--local") || message.contains("without attaching"),
            "{message}"
        );
    }

    #[test]
    fn a_prompt_sent_while_the_daemon_is_answering_keeps_the_draft() {
        assert!(matches!(
            route_without_commands(&TuiRouteRequest::BusyInput("and the tests".to_owned())),
            TuiSubmissionOutcome::BusyRefusal(_)
        ));
    }

    fn came_back_to(answering: bool) -> Arrival {
        Arrival {
            session_id: 7,
            landing: Landing::CameBack { answering },
            history: Vec::new(),
            presentation: HostedPresentation::default(),
        }
    }

    struct StubEngine;

    impl Engine for StubEngine {
        fn cancel(&mut self) {}
    }

    /// The arrival dresses the surface with the hosted session's own
    /// configuration, exactly as a local launch would: the footer names the
    /// model and its effort, and the context gauge has a window to measure
    /// against, before the first prompt is ever sent.
    #[test]
    fn the_arrival_renders_the_hosted_sessions_model_effort_and_context_window() {
        let arrival = Arrival {
            session_id: 7,
            landing: Landing::CameBack { answering: false },
            history: Vec::new(),
            presentation: HostedPresentation {
                provider: Some("openai-api".to_owned()),
                model: Some("gpt-4.1".to_owned()),
                reasoning_effort: Some("medium".to_owned()),
                context_window: Some(1_047_576),
                bypass_permissions: false,
                dangerous_mode: false,
            },
        };

        let presentation = arrival_presentation(&arrival).expect("the daemon described the chat");
        assert_eq!(
            presentation,
            agens_tui::TuiPresentation::new("openai-api", "gpt-4.1", "session #7")
                .with_effort("medium")
                .with_context_window(Some(1_047_576))
        );

        let mut tui = Tui::new(StubEngine);
        tui.apply_presentation(presentation);
        let view = tui.view();
        assert_eq!(view.provider_model, "openai-api / gpt-4.1");
        assert_eq!(view.reasoning_effort, Some("medium"));
        assert_eq!(view.context_window, Some(1_047_576));
    }

    /// A bypassed hosted session arrives with its footer already saying so,
    /// and the daemon's later toggle reply flips it without a re-attach.
    #[test]
    fn the_arrival_and_the_toggle_reply_both_drive_the_bypass_footer() {
        let arrival = Arrival {
            session_id: 9,
            landing: Landing::CameBack { answering: false },
            history: Vec::new(),
            presentation: HostedPresentation {
                provider: Some("openai-api".to_owned()),
                model: Some("gpt-4.1".to_owned()),
                reasoning_effort: None,
                context_window: None,
                bypass_permissions: true,
                dangerous_mode: false,
            },
        };

        let mut tui = Tui::new(StubEngine);
        tui.apply_presentation(
            arrival_presentation(&arrival).expect("the daemon described the chat"),
        );
        assert!(tui.view().bypass, "the arrival carries the bypassed state");

        tui.apply_submission_outcome(agens_tui::TuiSubmissionOutcome::BypassChanged {
            message: agens_core::hosted::BYPASS_OFF_REPLY.to_owned(),
            enabled: false,
        });
        assert!(!tui.view().bypass, "the daemon's reply flips the footer");
    }

    /// A daemon that predates the description leaves the placeholders rather
    /// than dressing the surface with invented values.
    #[test]
    fn an_arrival_the_daemon_did_not_describe_changes_nothing() {
        assert!(arrival_presentation(&Arrival::opened(7)).is_none());
    }

    /// A window the daemon does not hold stays empty rather than being looked
    /// up in this client's own model registry: the daemon owns the session's
    /// configuration, and a gauge invented here could disagree with what the
    /// daemon's turns actually measure against.
    #[test]
    fn a_described_model_without_a_window_leaves_the_gauge_empty() {
        let arrival = Arrival {
            session_id: 3,
            landing: Landing::Opened,
            history: Vec::new(),
            presentation: HostedPresentation {
                provider: Some("openai-api".to_owned()),
                model: Some("gpt-4.1".to_owned()),
                reasoning_effort: None,
                context_window: None,
                bypass_permissions: false,
                dangerous_mode: false,
            },
        };

        let presentation = arrival_presentation(&arrival).expect("the daemon described the chat");
        assert_eq!(
            presentation,
            agens_tui::TuiPresentation::new("openai-api", "gpt-4.1", "session #3")
                .with_context_window(None)
        );
    }

    /// When the daemon's window disagrees with what this client's own model
    /// registry would say, the daemon's wins: the session's configuration
    /// lives with the daemon, and two clients attached to it must render the
    /// same gauge whatever they know locally.
    #[test]
    fn the_daemon_described_window_wins_over_the_client_registry() {
        let locally_known = agens_models::context_window_for("gpt-4.1")
            .expect("the client registry knows this model");
        assert_ne!(locally_known, 10);

        let arrival = Arrival {
            session_id: 3,
            landing: Landing::Opened,
            history: Vec::new(),
            presentation: HostedPresentation {
                provider: Some("openai-api".to_owned()),
                model: Some("gpt-4.1".to_owned()),
                reasoning_effort: None,
                context_window: Some(10),
                bypass_permissions: false,
                dangerous_mode: false,
            },
        };

        let presentation = arrival_presentation(&arrival).expect("the daemon described the chat");
        assert_eq!(
            presentation,
            agens_tui::TuiPresentation::new("openai-api", "gpt-4.1", "session #3")
                .with_context_window(Some(10))
        );
    }

    /// A description without a provider still names the model, under the same
    /// placeholder provider local mode uses when none resolves.
    #[test]
    fn a_described_model_without_a_provider_uses_the_local_placeholder() {
        let arrival = Arrival {
            session_id: 3,
            landing: Landing::Opened,
            history: Vec::new(),
            presentation: HostedPresentation {
                provider: None,
                model: Some("gpt-4.1".to_owned()),
                reasoning_effort: None,
                context_window: Some(10),
                bypass_permissions: false,
                dangerous_mode: false,
            },
        };

        let presentation = arrival_presentation(&arrival).expect("the daemon described the chat");
        assert_eq!(
            presentation,
            agens_tui::TuiPresentation::new("provider", "gpt-4.1", "session #3")
                .with_context_window(Some(10))
        );
    }

    #[test]
    fn coming_back_says_where_you_landed_and_whether_it_is_mid_answer() {
        assert!(came_back_to(false).describe().contains("where you left it"));
        assert!(came_back_to(true).describe().contains("still answering"));
    }

    /// Only a chat still answering has a turn to adopt. A chat that is idle —
    /// or one this terminal just opened — waits for a prompt as before.
    #[test]
    fn only_a_chat_still_answering_is_adopted() {
        assert!(came_back_to(true).adopts_live_turn());
        assert!(!came_back_to(false).adopts_live_turn());
        assert!(!Arrival::opened(7).adopts_live_turn());
    }

    /// The launch that started the daemon says so exactly once, and before it
    /// says where it landed: the daemon the arrival describes is one this
    /// launch just brought up.
    #[test]
    fn a_startup_notice_opens_the_terminal_once_and_first() {
        let notices = opening_notices(Some("started the machine daemon"), &Arrival::opened(7));

        assert_eq!(notices[0], "started the machine daemon");
        assert!(notices[1].contains("session 7"), "{notices:?}");
        assert_eq!(
            notices
                .iter()
                .filter(|notice| notice.contains("started the machine daemon"))
                .count(),
            1
        );
    }

    /// A launch that found the daemon already running has nothing to announce,
    /// so the terminal opens straight on where it landed.
    #[test]
    fn without_a_startup_notice_the_terminal_opens_on_the_arrival() {
        let notices = opening_notices(None, &Arrival::opened(7));

        assert!(notices[0].contains("session 7"), "{notices:?}");
        assert!(!notices.iter().any(|notice| notice.contains("started")));
    }

    /// A fresh chat says that leaving does not stop it, because that is the one
    /// thing about this mode a person has to know before they close the window.
    #[test]
    fn a_fresh_chat_says_that_leaving_does_not_stop_it() {
        assert!(Arrival::opened(7).describe().contains("does not stop it"));
    }

    #[test]
    fn ambiguous_prompt_failures_do_not_claim_proven_rejection() {
        let outcome = attached_provider_outcome(Err(rejected(ClientError::Unreadable(
            "response lost after send".into(),
        ))));
        assert!(matches!(
            outcome,
            agens_tui::TuiProviderOutcome::Failed { .. }
        ));
    }

    #[test]
    fn local_preflight_refusals_are_restorable() {
        let outcome = attached_provider_outcome(Err(rejected(ClientError::InvalidRequest(
            "capability absent".into(),
        ))));
        assert!(matches!(
            outcome,
            agens_tui::TuiProviderOutcome::Rejected { .. }
        ));
    }

    /// A control-plane refusal is the daemon speaking, not a provider turn
    /// failing, and the error a person reads says which one it was. Only a
    /// daemon that is not there at all reads as unavailable.
    #[test]
    fn control_plane_refusals_are_reported_as_the_daemon_speaking() {
        let error = refused(ClientError::Unreadable("an answer off the wire".into()));
        assert_eq!(error.category, "daemon");
        assert!(error.message.contains("an answer off the wire"));

        assert_eq!(
            refused(ClientError::InvalidRequest("too wide for this wire".into())).category,
            "daemon",
        );
        assert_eq!(
            refused(ClientError::NotRunning("nobody on the socket".into())).category,
            "unavailable",
        );
    }

    #[test]
    fn selected_and_background_origins_are_explicitly_rejected() {
        assert!(unsupported_attached_origin(SubmitOrigin::User).is_none());
        for origin in [SubmitOrigin::Background, SubmitOrigin::SubagentCompletion] {
            let outcome = unsupported_attached_origin(origin).expect("unsupported origin rejects");
            assert!(matches!(
                outcome,
                agens_tui::TuiProviderOutcome::Failed { ref message, .. }
                    if message.contains("not supported while attached")
            ));
        }
    }
}
