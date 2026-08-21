//! The `direct` command: queues a message for a session that is already running.
//!
//! Writing into the terminal a session is drawing on is not a channel — it
//! races whatever the person is typing, and nothing durable records that the
//! run was steered. This writes a row the turn collects at its own boundary.

use agens_core::IntraTurnInputSource;
use agens_error::CliError;
use agens_store::{DirectiveGrain, DirectiveStore, DirectiveTarget, SessionStore};

use crate::CliDependencies;
use crate::deps::bootstrap;

pub(crate) fn run_direct(
    session: Option<String>,
    child: Option<String>,
    at_turn_end: bool,
    as_supervisor: bool,
    message: Vec<String>,
    dependencies: &CliDependencies,
) -> Result<String, CliError> {
    let message = message.join(" ");
    let message = message.trim();
    if message.is_empty() {
        return Err(CliError::usage("direct requires a message"));
    }

    let bootstrap = bootstrap(dependencies)?;
    let (target, addressee) = match (session, child) {
        (_, Some(child)) => {
            let child = child.trim();
            if child.is_empty() {
                return Err(CliError::usage("direct requires a child reference"));
            }
            // Unverifiable on purpose. A delegated turn lives only inside the
            // process running it and leaves no row behind, so there is nothing
            // to look the reference up in — the reference itself is what the
            // child published when it started.
            (
                DirectiveTarget::Child(child.to_owned()),
                format!("child {child}"),
            )
        }
        (Some(session), None) => {
            let session_id = session
                .parse::<i64>()
                .map_err(|_| CliError::usage("direct requires a numeric session id"))?;
            // Refuse an unknown session here rather than letting the row sit in
            // the queue forever: a message addressed to nothing is a typo, and
            // the only moment anyone is present to hear about it is now.
            SessionStore::open(&bootstrap.data_directory)
                .map_err(|_| CliError::storage("sessions database is unavailable"))?
                .load_session_for_resume(session_id)
                .map_err(|_| CliError::usage("no session by that id"))?;
            (
                DirectiveTarget::Session(session_id),
                format!("session {session_id}"),
            )
        }
        (None, None) => {
            return Err(CliError::usage("direct requires a session id or --child"));
        }
    };

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
        .and_then(|mut store| store.enqueue(&target, source, grain, message))
        .map_err(|_| CliError::storage("the directive could not be queued"))?;

    Ok(format!(
        "Queued for {addressee}, delivered at the next {}.\n",
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
    use agens_store::{DirectiveGrain, DirectiveStore, DirectiveTarget, SessionStore};

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
            Some(session_id.to_string()),
            None,
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
            .drain(
                &DirectiveTarget::Session(session_id),
                DirectiveGrain::ToolCall,
            )
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

    /// A child turn is the longest stretch of a delegation and the one a
    /// supervisor most needs to reach, so it is addressable by the reference
    /// its own `turn_started` diagnostic published — without a session id,
    /// which a delegation does not have.
    #[test]
    fn a_child_turn_is_addressable_by_the_reference_it_published() {
        let data_home = std::env::temp_dir().join(format!(
            "agens-direct-child-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&data_home).ok();
        let data_directory = data_home.join("agens");
        std::fs::create_dir_all(&data_directory).unwrap();

        let message = run_direct(
            None,
            Some("a1b2c3d4".to_owned()),
            false,
            true,
            vec!["read the manifest first".to_owned()],
            &dependencies(&data_home),
        )
        .unwrap();

        assert_eq!(
            message,
            "Queued for child a1b2c3d4, delivered at the next tool batch.\n"
        );
        let queued = DirectiveStore::open(&data_directory)
            .unwrap()
            .drain(
                &DirectiveTarget::Child("a1b2c3d4".to_owned()),
                DirectiveGrain::ToolCall,
            )
            .unwrap();
        assert_eq!(
            queued
                .iter()
                .map(|directive| (directive.source, directive.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(IntraTurnInputSource::Supervisor, "read the manifest first")]
        );

        std::fs::remove_dir_all(&data_home).ok();
    }

    /// Naming neither addressee is refused rather than defaulted. Guessing
    /// which running turn a message meant would deliver it to the wrong one,
    /// and a directive is only worth having if it lands where it was aimed.
    #[test]
    fn a_directive_addressed_to_nothing_is_refused() {
        let data_home = std::env::temp_dir().join(format!(
            "agens-direct-unaddressed-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&data_home).ok();
        std::fs::create_dir_all(data_home.join("agens")).unwrap();

        let error = run_direct(
            None,
            None,
            false,
            false,
            vec!["change course".to_owned()],
            &dependencies(&data_home),
        )
        .expect_err("an unaddressed directive is refused");
        assert!(error.to_string().contains("--child"), "{error}");

        std::fs::remove_dir_all(&data_home).ok();
    }
}
