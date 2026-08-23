//! The `serve` command: a thin adapter onto the daemon crate.
//!
//! It resolves the data directory and the team-mode settings, and nothing else.
//! In particular it resolves no project: one daemon serves N of them (AGN-80),
//! and a project only enters through a run.
//!
//! The daemon is composed by `agens-server`, not here. What this supplies is the
//! configuration the operator wrote and the worker factory, which is the seam a
//! run's session is built from: everything a worker is made of is knowledge of
//! models, prompts, skills and worktrees, and the control plane deliberately
//! has none of it.

use std::time::{SystemTime, UNIX_EPOCH};

use agens_bootstrap::Bootstrap;
use agens_config::TeamSettings;
use agens_core::HeadlessTurnCancellation;
use agens_server::{CoordinatorSettings, PolicySettings, ServerError, TimerSettings};

mod lifecycle;

use crate::CliDependencies;
use crate::cli::ServeAction;
use crate::deps::bootstrap;
use crate::worker::run_worker;
use agens_error::CliError;

pub(crate) fn run_serve(
    foreground: bool,
    action: Option<ServeAction>,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    // `--foreground` says how to run the daemon, so it means nothing next to a
    // verb that does not run one. Refused rather than ignored: the operator who
    // typed it believed it was doing something.
    if foreground && action.is_some() {
        return Err(CliError::usage(
            "serve --foreground takes no subcommand: it is how the daemon runs, not a verb",
        ));
    }

    let bootstrap = bootstrap(dependencies)?;

    match action {
        Some(ServeAction::Trust { repository }) => run_trust(&bootstrap, &repository),
        Some(ServeAction::Stop) => lifecycle::stop(&bootstrap),
        Some(ServeAction::Status) => lifecycle::status(&bootstrap),
        // Detached by default: a daemon that holds the terminal it was started
        // from is one an operator has to dedicate a terminal to. `--foreground`
        // is the shape a process supervisor wants, where detaching is exactly
        // what would lose it the process it is supervising.
        None if foreground => run_daemon(&bootstrap, cancellation),
        None => lifecycle::start_detached(&bootstrap),
    }
}

/// Authorizes one repository's provisioning hooks.
///
/// It writes the register the daemon reads rather than talking to the daemon,
/// so it works whether or not one is running and takes effect on that
/// repository's next run either way.
fn run_trust(bootstrap: &Bootstrap, repository: &std::path::Path) -> Result<String, CliError> {
    let trusted = agens_server::trust_repository(
        bootstrap.data_directory(),
        policy_settings(bootstrap),
        repository,
        now(),
    )
    .map_err(|error| CliError::configuration(error.to_string()))?;

    Ok(format!(
        "the provisioning hooks of {} ({}) may now run with the daemon's environment",
        trusted.repository.display(),
        trusted.repo_id
    ))
}

/// The half of the repository policy the operator writes by hand.
fn policy_settings(bootstrap: &Bootstrap) -> PolicySettings {
    let team = TeamSettings::from(bootstrap.settings());

    PolicySettings {
        project_roots: team.project_roots,
        hook_exports: team.hook_exports,
        config_path: Some(bootstrap.paths().global_config.clone()),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

fn run_daemon(
    bootstrap: &Bootstrap,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    let team = TeamSettings::from(bootstrap.settings());

    let settings = CoordinatorSettings {
        timers: TimerSettings {
            checkpoint_grace_percent: team.checkpoint_grace_percent,
            first_checkpoint_seconds: team.first_checkpoint_seconds,
            quota_window_seconds: team.quota_window_seconds,
        },
        policy: policy_settings(bootstrap),
        // The same switch every other diagnostic is behind. A supervisor that
        // wants to follow this daemon without attaching a client starts it with
        // `--debug`; one that did not ask gets no file.
        diagnostics: bootstrap.debug(),
        ..CoordinatorSettings::default()
    };

    let shutdown = agens_server::serve_until_shutdown(
        bootstrap.data_directory(),
        &settings,
        run_worker(bootstrap),
        cancellation,
    )
    .map_err(|error| match error {
        ServerError::AlreadyRunning => CliError::unavailable(
            "a daemon is already running for this machine; attach to it instead of starting another",
        ),
        // The cause travels: a daemon that refused to start has no journal,
        // no facade and no diagnostics file for an operator to read it from
        // instead, so this line is the only place the reason appears.
        ServerError::Unavailable(cause) => {
            CliError::unavailable(format!("the daemon is unavailable: {cause}"))
        }
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
