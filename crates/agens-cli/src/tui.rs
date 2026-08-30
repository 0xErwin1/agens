//! Launching the terminal surface.
//!
//! The surface itself is `agens-tui-app`. What stays here is the one decision
//! the binary owns: resolving the run's configuration and handing it to the
//! launcher the composition root installed.
//!
//! There are two shapes it can run in, and which one is not decided here
//! either. Local runs the turn in this process, which is what it has always
//! done. Attached runs it in the daemon and renders what comes back, so closing
//! the terminal stops the client and not the work.

use crate::CliDependencies;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::deps::bootstrap;
use crate::profile_store::{
    AgentProfileStore as FileProfileStore, ProfileScope as FileProfileScope,
};
use agens_config::AgentProfilePatch;
use agens_error::CliError;
use agens_tui_app::profiles::{AgentProfileStore, ProfileScope};

struct TuiProfileStore(FileProfileStore);

impl AgentProfileStore for TuiProfileStore {
    fn save(
        &self,
        scope: ProfileScope,
        agent: &str,
        patch: &AgentProfilePatch,
    ) -> Result<(), String> {
        let scope = match scope {
            ProfileScope::Global => FileProfileScope::Global,
            ProfileScope::Project => FileProfileScope::Project,
        };
        let snapshot = self.0.read(scope).map_err(|error| error.to_string())?;
        self.0
            .save(scope, &snapshot, agent, patch)
            .map_err(|error| error.to_string())
    }
}

/// Where a terminal's turns run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TuiMode {
    /// In this process, which is what closing the terminal ends.
    Local,
    /// In the daemon, which keeps the session whether anybody is watching or
    /// not.
    #[default]
    Attached,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiLaunch {
    resume: Option<i64>,
    mode: TuiMode,
    initial_prompt: Option<String>,
    startup_notice: Option<String>,
}

impl TuiLaunch {
    pub const fn mode(&self) -> TuiMode {
        self.mode
    }

    pub const fn resume(&self) -> Option<i64> {
        self.resume
    }

    pub fn initial_prompt(&self) -> Option<&str> {
        self.initial_prompt.as_deref()
    }

    pub fn startup_notice(&self) -> Option<&str> {
        self.startup_notice.as_deref()
    }
}

pub(crate) fn run_production_tui(
    bootstrap: &agens_bootstrap::Bootstrap,
    launch: TuiLaunch,
) -> Result<String, CliError> {
    let socket = agens_server::socket_path(bootstrap.data_directory());
    if launch.mode == TuiMode::Attached {
        return agens_tui_app::attached::run_attached_tui_with_prompt(
            bootstrap,
            &socket,
            launch.resume,
            launch.initial_prompt.as_deref(),
            launch.startup_notice.as_deref(),
        );
    }

    let project_root = bootstrap
        .project_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    let store = TuiProfileStore(FileProfileStore::new(
        bootstrap.paths.global_config.clone(),
        project_root.join(".agens/config.toml"),
    ));
    let team_requested = Arc::new(AtomicBool::new(false));
    let output = agens_tui_app::engine::run_production_tui_with_options(
        bootstrap,
        launch.resume,
        Some(Arc::new(store)),
        launch.startup_notice.as_deref(),
        Arc::clone(&team_requested),
    )?;

    if !team_requested.load(Ordering::SeqCst) {
        return Ok(output);
    }

    crate::commands::serve::ensure_daemon_running(
        bootstrap,
        crate::commands::serve::DaemonStartupRequest::ExplicitAttached,
    )?;
    let notice = crate::commands::serve::check_daemon_build(
        bootstrap,
        crate::commands::serve::SkewPolicy::RestartWhenIdle,
    )?;
    agens_tui_app::attached::run_attached_tui_with_prompt(
        bootstrap,
        &socket,
        None,
        None,
        notice.as_deref(),
    )
}

pub(crate) fn run_tui(
    dependencies: &CliDependencies,
    resume: Option<i64>,
    mode: TuiMode,
    initial_prompt: Option<String>,
    daemon_startup: Option<crate::commands::serve::DaemonStartupRequest>,
) -> Result<String, CliError> {
    let bootstrap = bootstrap(dependencies)?;
    let started = daemon_startup
        .map(|request| (dependencies.daemon_ensurer)(&bootstrap, request))
        .transpose()
        .map_err(|error| attached_failure(mode, error))?
        .unwrap_or(false);

    // The handshake runs on every attached launch, after a missing daemon was
    // started and before the surface opens anything against the one serving.
    // Only the launch forms that start daemons may also replace an idle one;
    // an explicit attach reports and touches nothing.
    let handshake_notice = if mode == TuiMode::Attached {
        let policy = if daemon_startup.is_some() {
            crate::commands::serve::SkewPolicy::RestartWhenIdle
        } else {
            crate::commands::serve::SkewPolicy::ReportOnly
        };

        (dependencies.daemon_build_check)(&bootstrap, policy)
            .map_err(|error| attached_failure(mode, error))?
    } else {
        None
    };

    let startup_notice = match (
        started.then(|| "started the machine daemon".to_owned()),
        handshake_notice,
    ) {
        (Some(started), Some(handshake)) => Some(format!("{started}; {handshake}")),
        (Some(one), None) | (None, Some(one)) => Some(one),
        (None, None) => None,
    };
    let output = (dependencies.tui_launcher)(
        &bootstrap,
        TuiLaunch {
            resume,
            mode,
            initial_prompt,
            startup_notice,
        },
    )
    .map_err(|error| attached_failure(mode, error))?;
    Ok(format!("{output}\n"))
}

fn attached_failure(mode: TuiMode, error: CliError) -> CliError {
    if mode == TuiMode::Local || error.message.contains("agens --local") {
        return error;
    }

    CliError::new(
        error.status(),
        error.category,
        format!(
            "{}; run `agens --local` to use in-process mode",
            error.message
        ),
    )
}
