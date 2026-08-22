//! The `serve` command: a thin adapter onto the daemon crate.
//!
//! It resolves the data directory and the team-mode settings, and nothing else.
//! In particular it resolves no project: one daemon serves N of them (AGN-80),
//! and a project only enters through a run.
//!
//! The daemon is composed by `agens-server`, not here. What this supplies is the
//! configuration the operator wrote and the worker factory, which is the seam a
//! run's session is built from — and which no surface fills in yet, so it
//! refuses by name rather than pretending to execute a run.

use agens_config::TeamSettings;
use agens_core::HeadlessTurnCancellation;
use agens_server::{CoordinatorSettings, ServerError, TimerSettings, unwired_worker};

use crate::CliDependencies;
use crate::deps::bootstrap;
use agens_error::CliError;

pub(crate) fn run_serve(
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    let bootstrap = bootstrap(dependencies)?;
    let team = TeamSettings::from(bootstrap.settings());

    let settings = CoordinatorSettings {
        timers: TimerSettings {
            checkpoint_grace_percent: team.checkpoint_grace_percent,
        },
        ..CoordinatorSettings::default()
    };

    let shutdown = agens_server::serve_until_shutdown(
        bootstrap.data_directory(),
        &settings,
        unwired_worker(),
        // The unix socket alone. A loopback listener is a second address to
        // reach the control plane on, and nothing asks for one yet.
        None,
        cancellation,
    )
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
