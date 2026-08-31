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
        Some(cli::Command::Team { prompt })
            if prompt.first().is_some_and(|action| {
                matches!(
                    action.as_str(),
                    "answer" | "permission" | "merge" | "cancel"
                )
            }) =>
        {
            run_team_action(&prompt, dependencies)
        }
        Some(cli::Command::Team { prompt })
            if matches!(prompt.as_slice(), [action] if action == "ls")
                || matches!(prompt.as_slice(), [action, json] if action == "ls" && json == "--json") =>
        {
            run_team_ls(
                matches!(prompt.as_slice(), [_, json] if json == "--json"),
                dependencies,
            )
        }
        Some(cli::Command::Team { prompt })
            if matches!(prompt.as_slice(), [action, _] if action == "show")
                || matches!(prompt.as_slice(), [action, _, follow] if action == "show" && follow == "--follow") =>
        {
            run_team_show(
                &prompt[1],
                matches!(prompt.as_slice(), [_, _, follow] if follow == "--follow"),
                dependencies,
            )
        }
        // A first token that looks like a flag, or a fleet verb whose
        // arguments did not match any accepted form, is a mistyped fleet
        // operation. Opening a chat with it as the prompt would silently
        // swallow the mistake, so it is refused instead. A prompt whose first
        // token is a plain word stays a chat prompt on purpose.
        Some(cli::Command::Team { prompt })
            if prompt.first().is_some_and(|first| {
                first.starts_with('-')
                    || matches!(
                        first.as_str(),
                        "ls" | "show" | "answer" | "permission" | "merge" | "cancel"
                    )
            }) =>
        {
            Err(CliError::usage(team::FLEET_USAGE))
        }
        Some(cli::Command::Team { prompt }) => run_tui(
            dependencies,
            None,
            crate::tui::TuiMode::Attached,
            (!prompt.is_empty()).then(|| prompt.join(" ")),
            Some(serve::DaemonStartupRequest::ExplicitAttached),
        ),
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
