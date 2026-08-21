//! The `direct` command: queues a message for a session that is already running.
//!
//! Writing into the terminal a session is drawing on is not a channel — it
//! races whatever the person is typing, and nothing durable records that the
//! run was steered. This writes a row the turn collects at its own boundary.

use agens_core::IntraTurnInputSource;
use agens_error::CliError;
use agens_store::{DirectiveGrain, DirectiveStore, SessionStore};

use crate::CliDependencies;
use crate::deps::bootstrap;

pub(crate) fn run_direct(
    session: String,
    at_turn_end: bool,
    as_supervisor: bool,
    message: Vec<String>,
    dependencies: &CliDependencies,
) -> Result<String, CliError> {
    let session_id = session
        .parse::<i64>()
        .map_err(|_| CliError::usage("direct requires a numeric session id"))?;
    let message = message.join(" ");
    let message = message.trim();
    if message.is_empty() {
        return Err(CliError::usage("direct requires a message"));
    }

    let bootstrap = bootstrap(dependencies)?;
    // Refuse an unknown session here rather than letting the row sit in the
    // queue forever: a message addressed to nothing is a typo, and the only
    // moment anyone is present to hear about it is now.
    SessionStore::open(&bootstrap.data_directory)
        .map_err(|_| CliError::storage("sessions database is unavailable"))?
        .load_session_for_resume(session_id)
        .map_err(|_| CliError::usage("no session by that id"))?;

    let grain = if at_turn_end {
        DirectiveGrain::Turn
    } else {
        DirectiveGrain::ToolCall
    };
    let source = if as_supervisor {
        IntraTurnInputSource::Supervisor
    } else {
        IntraTurnInputSource::Human
    };

    DirectiveStore::open(&bootstrap.data_directory)
        .and_then(|mut store| store.enqueue(session_id, source, grain, message))
        .map_err(|_| CliError::storage("the directive could not be queued"))?;

    Ok(format!(
        "Queued for session {session_id}, delivered at the next {}.\n",
        match grain {
            DirectiveGrain::ToolCall => "tool batch",
            DirectiveGrain::Turn => "turn end",
        }
    ))
}
