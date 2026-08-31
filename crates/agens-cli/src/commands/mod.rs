pub(crate) mod auth;
pub(crate) mod chat;
pub(crate) mod config;
pub(crate) mod direct;
pub(crate) mod models;
pub(crate) mod serve;
pub(crate) mod sessions;
pub(crate) mod team;

use agens_core::HeadlessTurnCancellation;

use crate::CliDependencies;
use crate::cli;
use crate::tui::run_tui;
use agens_error::CliError;
use auth::run_auth;
use chat::run_chat;
use config::run_config;
use direct::run_direct;
use models::run_models;
use serve::run_serve;
use sessions::run_sessions;
use team::{run_team_action, run_team_ls, run_team_show};

pub(crate) fn dispatch(
    parsed: cli::Cli,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    let tui_mode = parsed.tui_mode();
    match parsed.command {
        None => {
            let daemon_startup = (!parsed.local && !parsed.attach)
                .then_some(serve::DaemonStartupRequest::ExplicitAttached);
            run_tui(
                dependencies,
                parsed.resume.flatten(),
                tui_mode,
                None,
                daemon_startup,
            )
        }
        Some(cli::Command::Config { action }) => run_config(action, dependencies),
        Some(cli::Command::Auth { action }) => run_auth(action, dependencies, cancellation),
        Some(cli::Command::Chat(chat_arguments)) => {
            run_chat(chat_arguments, dependencies, cancellation)
        }
        Some(cli::Command::Models) => run_models(),
        Some(cli::Command::Attach { target }) => run_tui(
            dependencies,
            target,
            crate::tui::TuiMode::Attached,
            None,
            None,
        ),
        // Bare `agens team` is `team ls`: the command's identity is
        // inspecting the fleet, and the chat has exactly one door, bare
        // `agens`.
        Some(cli::Command::Team { operation }) if operation.is_empty() => {
            run_team_ls(false, dependencies)
        }
        Some(cli::Command::Team { operation })
            if operation.first().is_some_and(|action| {
                matches!(
                    action.as_str(),
                    "answer" | "permission" | "merge" | "cancel"
                )
            }) =>
        {
            run_team_action(&operation, dependencies)
        }
        Some(cli::Command::Team { operation })
            if matches!(operation.as_slice(), [action] if action == "ls")
                || matches!(operation.as_slice(), [action, json] if action == "ls" && json == "--json") =>
        {
            run_team_ls(
                matches!(operation.as_slice(), [_, json] if json == "--json"),
                dependencies,
            )
        }
        Some(cli::Command::Team { operation })
            if matches!(operation.as_slice(), [action, _] if action == "show")
                || matches!(operation.as_slice(), [action, _, follow] if action == "show" && follow == "--follow") =>
        {
            run_team_show(
                &operation[1],
                matches!(operation.as_slice(), [_, _, follow] if follow == "--follow"),
                dependencies,
            )
        }
        // Anything else is a mistyped fleet operation and is refused with
        // the accepted forms; team never opens a chat.
        Some(cli::Command::Team { .. }) => Err(CliError::usage(team::FLEET_USAGE)),
        Some(cli::Command::Serve { foreground, action }) => {
            run_serve(foreground, action, dependencies, cancellation)
        }
        Some(cli::Command::Sessions { action }) => run_sessions(action, dependencies),
        Some(cli::Command::Direct {
            session,
            child,
            answer,
            at_turn_end,
            as_supervisor,
            message,
        }) => run_direct(
            session,
            child,
            answer,
            at_turn_end,
            as_supervisor,
            message,
            dependencies,
        ),
        Some(cli::Command::Version) => Ok(format!("agens {}\n", env!("CARGO_PKG_VERSION"))),
    }
}
