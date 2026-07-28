//! The `serve` command: a thin adapter onto the daemon crate.
//!
//! It resolves the data directory and nothing else. In particular it resolves no
//! project: one daemon serves N of them (AGN-80).

use agens_core::HeadlessTurnCancellation;
use agens_server::ServerError;

use crate::CliDependencies;
use crate::deps::bootstrap;
use agens_error::CliError;

pub(crate) fn run_serve(
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    let bootstrap = bootstrap(dependencies)?;

    agens_server::run_until_shutdown(bootstrap.data_directory(), cancellation).map_err(
        |error| match error {
            ServerError::AlreadyRunning => CliError::unavailable(
                "a daemon is already running for this machine; attach to it instead of starting another",
            ),
            ServerError::Unavailable(_) => CliError::unavailable("the daemon is unavailable"),
        },
    )?;

    Ok(String::new())
}
