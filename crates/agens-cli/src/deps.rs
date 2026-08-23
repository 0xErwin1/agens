//! Composition root for `CliDependencies`: the side-effecting boundary the
//! rest of the crate is built against.
//!
//! `CliDependencies`'s fields are private, and Rust privacy is
//! module-and-descendants, so `production()` cannot live in a parent module
//! once the fields are private here — the whole `impl` block stays next to
//! the struct it constructs. This module also wires the production closures
//! from `commands`, `headless`, and `tui`, so it is extracted last.

use std::collections::BTreeMap;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agens_config::resolve_paths;
use agens_core::HeadlessTurnCancellation;

use crate::commands::auth::run_production_auth_login;
use crate::commands::config::{create_configuration_file, create_global_configuration_file};
use crate::commands::serve::DaemonStartupRequest;
use crate::headless::run_production_headless_chat;
use crate::tui::{TuiLaunch, run_production_tui};
use agens_bootstrap::{Bootstrap, HostEnvironment};
use agens_error::CliError;
use agens_headless::HeadlessChatRequest;

const UNAVAILABLE_MESSAGE: &str = "this command is not implemented yet";

/// Shared, not owned, because [`Bootstrap`] keeps its own clone of this closure so it can
/// re-read a session's own project configuration document later — after `bootstrap()` has
/// already returned — instead of only ever answering for the process's own discovered root.
/// Creates a configuration file, failing when one already exists.
type ConfigCreator = Box<dyn Fn(&Path, &str) -> Result<(), CliError>>;
type HeadlessChat = Box<
    dyn Fn(HeadlessChatRequest, &Bootstrap, &HeadlessTurnCancellation) -> Result<String, CliError>,
>;
type TuiLauncher = Box<dyn Fn(&Bootstrap, TuiLaunch) -> Result<String, CliError>>;
type DaemonEnsurer = Box<dyn Fn(&Bootstrap, DaemonStartupRequest) -> Result<bool, CliError>>;
type AuthLogin = Box<dyn Fn(&Path, bool, &HeadlessTurnCancellation) -> Result<String, CliError>>;
/// Whether the process's standard input is attached to a terminal.
///
/// This is a dependency rather than a direct `is_terminal()` call so a test
/// states the input context it exercises instead of inheriting whatever the
/// runner happens to provide: the same argv means a hidden prompt under a
/// terminal and a piped read without one.
type StdinIsTerminal = Box<dyn Fn() -> bool>;

pub struct CliDependencies {
    pub(crate) host: HostEnvironment,
    pub(crate) create_file: ConfigCreator,
    pub(crate) headless_chat: HeadlessChat,
    pub(crate) tui_launcher: TuiLauncher,
    pub(crate) daemon_ensurer: DaemonEnsurer,
    pub(crate) auth_login: AuthLogin,
    pub(crate) stdin_is_terminal: StdinIsTerminal,
}

impl CliDependencies {
    pub fn production() -> Self {
        Self {
            host: HostEnvironment {
                current_directory: Box::new(|| {
                    std::env::current_dir()
                        .map_err(|_| CliError::configuration("working directory is unavailable"))
                }),
                home_directory: Box::new(|| std::env::var_os("HOME").map(PathBuf::from)),
                environment: Box::new(|| {
                    std::env::vars()
                        .filter(|(key, _)| !key.is_empty())
                        .collect()
                }),
                read_file: Arc::new(|path| match fs::read_to_string(path) {
                    Ok(contents) => Ok(Some(contents)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(_) => Err(CliError::configuration("configuration file is unavailable")),
                }),
            },
            create_file: Box::new(|path, contents| {
                let home_directory = std::env::var_os("HOME").map(PathBuf::from);
                let environment: BTreeMap<String, String> = std::env::vars()
                    .filter(|(key, _)| !key.is_empty())
                    .collect();
                let global_config =
                    resolve_paths(Path::new(""), home_directory.as_deref(), &environment)
                        .global_config;

                if path == global_config {
                    create_global_configuration_file(path, contents)
                } else {
                    create_configuration_file(path, contents)
                }
            }),
            headless_chat: Box::new(run_production_headless_chat),
            tui_launcher: Box::new(run_production_tui),
            daemon_ensurer: Box::new(crate::commands::serve::ensure_daemon_running),
            auth_login: Box::new(run_production_auth_login),
            stdin_is_terminal: Box::new(|| std::io::stdin().is_terminal()),
        }
    }

    pub fn for_test(
        current_directory: PathBuf,
        home_directory: Option<PathBuf>,
        environment: BTreeMap<String, String>,
        files: BTreeMap<PathBuf, String>,
    ) -> Self {
        Self {
            host: HostEnvironment::fixed(current_directory, home_directory, environment, files),
            create_file: Box::new(|_, _| Err(CliError::unavailable(UNAVAILABLE_MESSAGE))),
            headless_chat: Box::new(|_, _, _| Err(CliError::unavailable(UNAVAILABLE_MESSAGE))),
            tui_launcher: Box::new(|_, _| Err(CliError::unavailable(UNAVAILABLE_MESSAGE))),
            daemon_ensurer: Box::new(|_, _| Ok(false)),
            auth_login: Box::new(|_, _, _| Err(CliError::unavailable(UNAVAILABLE_MESSAGE))),
            stdin_is_terminal: Box::new(|| false),
        }
    }

    pub fn with_create_file(
        mut self,
        handler: impl Fn(&Path, &str) -> Result<(), CliError> + 'static,
    ) -> Self {
        self.create_file = Box::new(handler);
        self
    }

    pub fn with_headless_chat(
        mut self,
        handler: impl Fn(
            HeadlessChatRequest,
            &Bootstrap,
            &HeadlessTurnCancellation,
        ) -> Result<String, CliError>
        + 'static,
    ) -> Self {
        self.headless_chat = Box::new(handler);
        self
    }

    pub fn with_tui_launcher(
        mut self,
        launcher: impl Fn(&Bootstrap, TuiLaunch) -> Result<String, CliError> + 'static,
    ) -> Self {
        self.tui_launcher = Box::new(launcher);
        self
    }

    pub fn with_daemon_ensurer(
        mut self,
        ensurer: impl Fn(&Bootstrap) -> Result<bool, CliError> + 'static,
    ) -> Self {
        self.daemon_ensurer = Box::new(move |bootstrap, _| ensurer(bootstrap));
        self
    }

    pub fn with_auth_login(
        mut self,
        login: impl Fn(&Path, bool, &HeadlessTurnCancellation) -> Result<String, CliError> + 'static,
    ) -> Self {
        self.auth_login = Box::new(login);
        self
    }

    pub fn with_stdin_is_terminal(mut self, is_terminal: bool) -> Self {
        self.stdin_is_terminal = Box::new(move || is_terminal);
        self
    }
}

/// The CLI's only job here: hand the resolver the host it should read.
pub fn bootstrap(dependencies: &CliDependencies) -> Result<Bootstrap, CliError> {
    agens_bootstrap::resolve(&dependencies.host)
}
