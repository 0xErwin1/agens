use std::ffi::OsString;

use clap::Parser as _;

use agens_core::HeadlessTurnCancellation;

#[cfg(test)]
mod attempt_tests;
#[cfg(test)]
mod bootstrap_tests;
mod cli;
mod commands;
mod deps;

mod child_task_tests;
mod dispatch_tests;
mod headless;
mod headless_tests;

#[cfg(test)]
mod permission_tests;
pub mod profile_store;
#[cfg(test)]
mod provider_tests;
mod rotation_tests;
#[cfg(test)]
mod task_runner_tests;
mod task_tool_tests;
mod tui;
mod tui_tests;
#[cfg(test)]
mod turns_tests;

use tui::run_tui;

pub use agens_bootstrap::Bootstrap;
pub use agens_error::{CliError, CommandResult, ExitStatus};
pub use agens_headless::HeadlessChatRequest;
pub use agens_models::{ModelSelection, ModelSource};
pub use deps::CliDependencies;
pub use deps::bootstrap;

pub fn execute<I, S>(arguments: I, dependencies: &CliDependencies) -> CommandResult
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_owned())
        .collect::<Vec<_>>();

    let cancellation = HeadlessTurnCancellation::new();
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
            let cancellation = HeadlessTurnCancellation::new();
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
