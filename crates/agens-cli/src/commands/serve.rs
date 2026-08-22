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

    let shutdown = agens_server::run_until_shutdown(bootstrap.data_directory(), cancellation)
        .map_err(|error| match error {
            ServerError::AlreadyRunning => CliError::unavailable(
                "a daemon is already running for this machine; attach to it instead of starting another",
            ),
            ServerError::Unavailable(_) => CliError::unavailable("the daemon is unavailable"),
        })?;

    // A clean shutdown says nothing, so the ordinary case stays silent and the
    // one line the operator ever sees is the one that needs acting on.
    if shutdown.is_clean() {
        return Ok(String::new());
    }

    let abandoned = shutdown
        .abandoned
        .iter()
        .map(|session| session.value().to_string())
        .collect::<Vec<_>>()
        .join(", ");

    Ok(format!(
        "the daemon stopped without these sessions ending: {abandoned}"
    ))
}
