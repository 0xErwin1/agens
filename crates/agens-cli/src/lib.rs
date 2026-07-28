use std::ffi::OsString;

use clap::Parser as _;

use agens_core::HeadlessTurnCancellation;
use agens_core::SubagentErrorKind;

#[cfg(test)]
mod attempt_tests;
#[cfg(test)]
mod bootstrap_tests;
mod chatgpt_auth;
mod cli;
mod commands;
mod deps;
mod diagnostics;
mod dispatch;
mod headless;
mod mcp;
mod permission_prompt;
#[cfg(test)]
mod permission_tests;
#[cfg(test)]
mod provider_tests;
mod session;
#[cfg(test)]
mod test_support;
mod tools;
mod tui;
#[cfg(test)]
mod turns_tests;

use agens_bootstrap::effective_max_iterations;
use agens_error::cancellation_result;
use diagnostics::{
    next_diagnostic_reference, operation_diagnostics, record_parent_terminal,
    record_subagent_terminal,
};
use headless::block_on_headless_turn;
use tui::models::tui_model_source;
use tui::run_tui;

pub use agens_bootstrap::Bootstrap;
pub use agens_error::{CliError, CommandResult, ExitStatus};
pub use agens_models::{ModelSelection, ModelSource};
pub use deps::CliDependencies;
pub use deps::bootstrap;
pub use headless::HeadlessChatRequest;

pub fn execute<I, S>(arguments: I, dependencies: &CliDependencies) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_owned())
        .collect::<Vec<_>>();

    let cancellation = HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(120));
    execute_strings(arguments, dependencies, &cancellation)
}

pub fn execute_with_cancellation<I, S>(
    arguments: I,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_owned())
        .collect::<Vec<_>>();

    execute_strings(arguments, dependencies, cancellation)
}

pub fn execute_os<I, S>(arguments: I, dependencies: &CliDependencies) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| {
            argument
                .into()
                .into_string()
                .map_err(|_| CliError::usage("command arguments must be valid UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>();

    match arguments {
        Ok(arguments) => {
            let cancellation =
                HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(120));
            execute_strings(arguments, dependencies, &cancellation)
        }
        Err(error) => error_result(&[], error),
    }
}

pub fn execute_os_with_cancellation<I, S>(
    arguments: I,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| {
            argument
                .into()
                .into_string()
                .map_err(|_| CliError::usage("command arguments must be valid UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>();

    match arguments {
        Ok(arguments) => execute_strings(arguments, dependencies, cancellation),
        Err(error) => error_result(&[], error),
    }
}

fn execute_strings(
    arguments: Vec<String>,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> CommandResult {
    match execute_command(&arguments, dependencies, cancellation) {
        Ok(stdout) => CommandResult {
            status: ExitStatus::Success,
            stdout,
            stderr: String::new(),
        },
        Err(error) => error_result(&arguments, error),
    }
}

pub(crate) fn error_result(arguments: &[String], error: CliError) -> CommandResult {
    CommandResult {
        status: error.status(),
        stdout: if arguments == ["config", "doctor"] && error.status() == ExitStatus::Configuration
        {
            "Agens config doctor\nStatus:  invalid\n".to_owned()
        } else {
            String::new()
        },
        stderr: if error.is_preformatted() {
            error.message.clone()
        } else {
            format!("error: {error}\n")
        },
    }
}

fn execute_command(
    arguments: &[String],
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    let arguments = &cli::normalize_resume_equals_negative(arguments);

    if let Some(identifier) = cli::resume_shorthand(arguments) {
        return run_tui(dependencies, Some(identifier));
    }

    if let Some(error) = cli::root_shape_conflict(arguments) {
        return cli::clap_outcome(error);
    }

    let parsed = match cli::Cli::try_parse_from(arguments.iter()) {
        Ok(parsed) => parsed,
        Err(error) => return cli::clap_outcome(error),
    };

    commands::dispatch(parsed, dependencies, cancellation)
}
