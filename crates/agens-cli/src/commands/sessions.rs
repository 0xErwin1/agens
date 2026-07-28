//! The `sessions` command: lists, shows, and removes saved sessions.

use agens_store::SessionStore;

use crate::CliDependencies;
use crate::cli;
use crate::deps::bootstrap;
use agens_error::CliError;

pub(crate) fn run_sessions(
    action: cli::SessionsAction,
    dependencies: &CliDependencies,
) -> Result<String, CliError> {
    match action {
        cli::SessionsAction::List => {
            let bootstrap = bootstrap(dependencies)?;
            let store = SessionStore::open(&bootstrap.data_directory)
                .map_err(|_| CliError::storage("sessions database is unavailable"))?;
            let sessions = store
                .list_sessions()
                .map_err(|_| CliError::storage("saved sessions could not be listed"))?;

            if sessions.is_empty() {
                return Ok("No saved sessions.\n".to_owned());
            }

            let rows = sessions
                .iter()
                .map(|session| {
                    format!(
                        "{}\t{}\t{}\t{}\t{}",
                        session.id,
                        session.project,
                        session.title,
                        session.active_agent,
                        session.completed_turn_count
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(format!("ID\tPROJECT\tTITLE\tAGENT\tTURNS\n{rows}\n"))
        }
        cli::SessionsAction::Show { identifier } => {
            let identifier = identifier
                .parse::<i64>()
                .map_err(|_| CliError::usage("sessions show requires a numeric id"))?;
            let bootstrap = bootstrap(dependencies)?;
            let store = SessionStore::open(&bootstrap.data_directory)
                .map_err(|_| CliError::storage("sessions database is unavailable"))?;
            let session = store
                .load_session_for_resume(identifier)
                .map_err(|_| CliError::storage("saved session is unavailable"))?;
            Ok(format!(
                "Session {identifier}: project={} title={} agent={} turns={} messages={}\n",
                session.metadata.project,
                session.metadata.title,
                session.metadata.active_agent,
                session.metadata.completed_turn_count,
                session.messages.len()
            ))
        }
        cli::SessionsAction::Rm { identifier } => {
            let identifier = identifier
                .parse::<i64>()
                .map_err(|_| CliError::usage("sessions rm requires a numeric id"))?;
            let bootstrap = bootstrap(dependencies)?;
            let mut store = SessionStore::open(&bootstrap.data_directory)
                .map_err(|_| CliError::storage("sessions database is unavailable"))?;
            store
                .delete_session(identifier)
                .map_err(|_| CliError::storage("saved session could not be removed"))?;
            Ok(format!("Removed session {identifier}.\n"))
        }
    }
}
