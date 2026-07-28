pub(crate) mod auth;
pub(crate) mod chat;
pub(crate) mod config;
pub(crate) mod models;
pub(crate) mod serve;
pub(crate) mod sessions;

use agens_core::HeadlessTurnCancellation;

use crate::CliDependencies;
use crate::cli;
use crate::tui::run_tui;
use agens_error::CliError;
use auth::run_auth;
use chat::run_chat;
use config::run_config;
use models::run_models;
use serve::run_serve;
use sessions::run_sessions;

pub(crate) fn dispatch(
    parsed: cli::Cli,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    match parsed.command {
        None => run_tui(dependencies, parsed.resume.flatten()),
        Some(cli::Command::Config { action }) => run_config(action, dependencies),
        Some(cli::Command::Auth { action }) => run_auth(action, dependencies, cancellation),
        Some(cli::Command::Chat(chat_arguments)) => {
            run_chat(chat_arguments, dependencies, cancellation)
        }
        Some(cli::Command::Models) => run_models(),
        Some(cli::Command::Serve) => run_serve(dependencies, cancellation),
        Some(cli::Command::Sessions { action }) => run_sessions(action, dependencies),
        Some(cli::Command::Version) => Ok(format!("agens {}\n", env!("CARGO_PKG_VERSION"))),
    }
}
