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
//! written a session id down.
//!
//! This mode is narrower than the local one and stays opt-in until it is not. A
//! hosted chat cannot yet be asked a permission question and has no task runtime
//! behind it, so slash commands, the skill palette, the file picker and
//! delegation are not wired here. A submission this mode cannot serve is
//! reported as such rather than silently doing nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use agens_bootstrap::Bootstrap;
use agens_coordinator_client::{ChatClient, ClientError, Coordinator, HostedChatEvent};
use agens_core::TurnEvent;
use agens_error::{CliError, ExitStatus};
use agens_tui::{
    Engine, Tui, TuiRouteRequest, TuiSubmissionOutcome, run_with_default_progress_submit,
};
use tokio::runtime::Runtime;
use tokio_stream::{Stream, StreamExt};

use crate::router::tui_provider_outcome;

/// What this mode cannot serve yet, said the same way everywhere it comes up.
const UNSUPPORTED: &str =
    "not available while attached to a daemon yet; start without attaching for this";

type Events = std::pin::Pin<Box<dyn Stream<Item = Result<HostedChatEvent, ClientError>> + Send>>;

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
}

impl Attachment {
    /// Sends one prompt and reports what the turn did, forwarding everything it
    /// produced to the surface as it happens.
    fn take_turn(&self, prompt: &str, progress: &Sender<TurnEvent>) -> Result<String, CliError> {
        let mut chat = self
            .chat
            .lock()
            .map_err(|_| unavailable("the connection to the daemon is unusable"))?;
        let mut events = self
            .events
            .lock()
            .map_err(|_| unavailable("the chat's event stream is unusable"))?;

        self.runtime
            .block_on(chat.prompt(self.session_id, prompt))
            .map_err(refused)?;

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

/// Cancellation, for a turn that is not running in this process.
///
/// It holds a chat client of its own rather than sharing the one a turn is
/// blocked on: the surface asks to cancel from the thread drawing it while the
/// turn is parked on the stream, and reaching through the same lock would mean
/// the cancellation waits for the thing it is cancelling.
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
/// Only an ordinary prompt becomes a turn. Everything else the surface can
/// route — a slash command, a dialog, a clipboard image — belongs to the local
/// router, and the local router drives a local session. Saying so is the point:
/// a command that quietly did nothing would read as the terminal being broken.
fn route(request: &TuiRouteRequest) -> TuiSubmissionOutcome {
    match request {
        TuiRouteRequest::Input(input) => {
            let trimmed = input.trim();

            if trimmed.is_empty() {
                return TuiSubmissionOutcome::LocalInfo(String::new());
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
    let checkout = bootstrap
        .project_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));

    let (attachment, engine, arrival) = attach(socket, &checkout, resume)?;

    let mut tui = Tui::new(engine);
    tui.adopt_environment();
    tui.set_collapse_thinking(bootstrap.collapse_thinking);
    tui.add_info(arrival.describe());

    run_with_default_progress_submit(
        &mut tui,
        move |request, _progress| route(&request),
        move |prompt, _origin, progress, _metrics| {
            tui_provider_outcome(attachment.take_turn(&prompt, &progress))
        },
    )
    .map_err(|error| CliError::new(ExitStatus::Failure, "ui", error.to_string()))?;

    Ok(String::new())
}

/// How this terminal came to the chat it is now on, so it can say so.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Arrival {
    Opened(i64),
    CameBack { session_id: i64, answering: bool },
}

impl Arrival {
    fn describe(self) -> String {
        match self {
            Self::Opened(session_id) => {
                format!("attached to the daemon as session {session_id}; leaving does not stop it")
            }
            Self::CameBack {
                session_id,
                answering: false,
            } => format!("back on session {session_id}, where you left it"),
            Self::CameBack {
                session_id,
                answering: true,
            } => format!("back on session {session_id}, which is still answering"),
        }
    }

    const fn session_id(self) -> i64 {
        match self {
            Self::Opened(session_id) | Self::CameBack { session_id, .. } => session_id,
        }
    }
}

fn attach(
    socket: &Path,
    checkout: &Path,
    resume: Option<i64>,
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
    let arrival = match resume {
        Some(session_id) => Arrival::Opened(
            runtime
                .block_on(chat.open(checkout, Some(session_id)))
                .map_err(refused)?,
        ),
        None => rejoin_or_open(&runtime, &mut chat, checkout)?,
    };

    let session_id = arrival.session_id();
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
            return Ok(Arrival::CameBack {
                session_id,
                answering: existing.answering,
            });
        }

        return Ok(Arrival::Opened(session_id));
    }

    Ok(Arrival::Opened(
        runtime
            .block_on(chat.open(checkout, None))
            .map_err(refused)?,
    ))
}

/// A daemon that is not there is reported as unavailable rather than as a
/// failed request: nothing was wrong with what was asked, there is simply
/// nothing running to ask.
fn refused(error: ClientError) -> CliError {
    match error {
        ClientError::NotRunning(_) => CliError::unavailable(
            "no daemon is running; start one with `agens serve`, or run with `--local`",
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

    #[test]
    fn an_ordinary_prompt_becomes_a_turn_for_the_daemon_to_run() {
        assert_eq!(
            route(&TuiRouteRequest::Input("what changed here".to_owned())),
            TuiSubmissionOutcome::ProviderTurn {
                display: "what changed here".to_owned(),
                prompt: "what changed here".to_owned(),
            },
        );
    }

    /// A command that quietly did nothing would read as the terminal being
    /// broken, so this mode says what it cannot do instead.
    #[test]
    fn a_command_this_mode_cannot_serve_says_so_rather_than_doing_nothing() {
        let TuiSubmissionOutcome::LocalInfo(message) =
            route(&TuiRouteRequest::Input("/model".to_owned()))
        else {
            panic!("a command is not a turn for the daemon to run");
        };

        assert!(message.contains("commands are"), "{message}");
        assert!(
            message.contains("--local") || message.contains("without attaching"),
            "{message}"
        );
    }

    /// The daemon accepts one waiting prompt and no more, so queueing a second
    /// one here would only move the refusal later.
    #[test]
    fn a_prompt_sent_while_the_daemon_is_answering_keeps_the_draft() {
        assert!(matches!(
            route(&TuiRouteRequest::BusyInput("and the tests".to_owned())),
            TuiSubmissionOutcome::BusyRefusal(_)
        ));
    }

    #[test]
    fn coming_back_says_where_you_landed_and_whether_it_is_mid_answer() {
        assert!(
            Arrival::CameBack {
                session_id: 7,
                answering: false,
            }
            .describe()
            .contains("where you left it")
        );
        assert!(
            Arrival::CameBack {
                session_id: 7,
                answering: true,
            }
            .describe()
            .contains("still answering")
        );
    }

    /// A fresh chat says that leaving does not stop it, because that is the one
    /// thing about this mode a person has to know before they close the window.
    #[test]
    fn a_fresh_chat_says_that_leaving_does_not_stop_it() {
        assert!(Arrival::Opened(7).describe().contains("does not stop it"));
    }
}
