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
                    let decision = self.decide(&question, &asking);

                    self.runtime
                        .block_on(chat.answer_permission(
                            self.session_id,
                            question.prompt_id,
                            decision,
                        ))
                        .map_err(refused)?;
                }
                HostedChatEvent::AskUserAsked { prompt_id, request } => {
                    let reply = self.ask_user.wait_for_reply(request, None, &asking);

                    self.runtime
                        .block_on(chat.answer_ask_user(self.session_id, prompt_id, reply))
                        .map_err(refused)?;
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
    run_attached_tui_with_prompt(bootstrap, socket, resume, None)
}

pub fn run_attached_tui_with_prompt(
    bootstrap: &Bootstrap,
    socket: &Path,
    resume: Option<i64>,
    initial_prompt: Option<&str>,
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
    tui.add_info(arrival.describe());
    tui.add_info("attached mode uses daemon-owned commands, skills, files, MCP state, and tasks");
    let attachment = Arc::new(attachment);
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        Arc::clone(&staging),
        Arc::new(Mutex::new(None)),
        Arc::new(agens_tools::CommandCatalog::default()),
        Arc::new(agens_tools::SkillCatalog::default()),
    )
    .with_attached_backend(attachment.clone());
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
            if let Some(rejection) = unsupported_attached_origin(origin) {
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

/// How this terminal came to the chat it is now on, and what was already
/// said there.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Arrival {
    session_id: i64,
    landing: Landing,
    /// What the chat has said so far. Empty for one this terminal just opened.
    history: Vec<Message>,
}

/// Whether this terminal started the conversation or came back to it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Landing {
    Opened,
    CameBack { answering: bool },
}

impl Arrival {
    const fn opened(session_id: i64) -> Self {
        Self {
            session_id,
            landing: Landing::Opened,
            history: Vec::new(),
        }
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
        Some(session_id) => Arrival {
            session_id: runtime
                .block_on(chat.open(checkout, Some(session_id)))
                .map_err(refused)?,
            landing: Landing::CameBack { answering: false },
            history: Vec::new(),
        },
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
        let session_id = runtime
            .block_on(chat.open(checkout, Some(existing.session_id)))
            .map_err(refused)?;

        if session_id == existing.session_id {
            return Ok(Arrival {
                session_id,
                landing: Landing::CameBack {
                    answering: existing.answering,
                },
                history: Vec::new(),
            });
        }

        return Ok(Arrival::opened(session_id));
    }

    Ok(Arrival::opened(
        runtime
            .block_on(chat.open(checkout, None))
            .map_err(refused)?,
    ))
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
        other => CliError::new(ExitStatus::Failure, "provider", other.to_string()),
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
        }
    }

    #[test]
    fn coming_back_says_where_you_landed_and_whether_it_is_mid_answer() {
        assert!(came_back_to(false).describe().contains("where you left it"));
        assert!(came_back_to(true).describe().contains("still answering"));
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
