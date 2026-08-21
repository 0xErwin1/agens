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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use agens_core::{IntraTurnInputSource, SessionMetadata};
    use agens_store::{DirectiveGrain, DirectiveStore, SessionStore};

    use super::run_direct;
    use crate::CliDependencies;

    fn dependencies(data_home: &std::path::Path) -> CliDependencies {
        CliDependencies::for_test(
            PathBuf::from("/workspace"),
            None,
            BTreeMap::from([("XDG_DATA_HOME".to_owned(), data_home.display().to_string())]),
            BTreeMap::new(),
        )
    }

    fn fresh_metadata() -> SessionMetadata {
        SessionMetadata {
            id: 0,
            project: "/workspace".into(),
            title: "first turn".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
            parent_session_id: None,
            fork_message_count: None,
        }
    }

    /// The first turn is the longest and the least observable one, so it is the
    /// turn a supervisor most needs to reach. It is also the only turn with no
    /// completed history behind it, which is why this pins the reachability of
    /// a session that has begun an attempt and nothing more.
    #[test]
    fn a_session_is_reachable_while_its_first_turn_is_still_running() {
        let data_home = std::env::temp_dir().join(format!(
            "agens-direct-first-turn-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&data_home).ok();
        let data_directory = data_home.join("agens");
        std::fs::create_dir_all(&data_directory).unwrap();

        let session_id = SessionStore::open(&data_directory)
            .unwrap()
            .begin_session_attempt(&fresh_metadata(), "first prompt".into())
            .unwrap()
            .key()
            .session_id();

        let message = run_direct(
            session_id.to_string(),
            false,
            true,
            vec!["change course".to_owned()],
            &dependencies(&data_home),
        )
        .unwrap();

        assert_eq!(
            message,
            format!("Queued for session {session_id}, delivered at the next tool batch.\n")
        );
        let queued = DirectiveStore::open(&data_directory)
            .unwrap()
            .drain(session_id, DirectiveGrain::ToolCall)
            .unwrap();
        assert_eq!(
            queued
                .iter()
                .map(|directive| (directive.source, directive.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(IntraTurnInputSource::Supervisor, "change course")]
        );

        std::fs::remove_dir_all(&data_home).ok();
    }
}
