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
use std::path::{Path, PathBuf};

use agens_config::resolve_paths;
use agens_core::HeadlessTurnCancellation;

use crate::bootstrap::Bootstrap;
use crate::commands::auth::run_production_auth_login;
use crate::commands::config::{create_configuration_file, create_global_configuration_file};
use crate::error::CliError;
use crate::headless::{HeadlessChatRequest, run_production_headless_chat};
use crate::tui::engine::run_production_tui;

const UNAVAILABLE_MESSAGE: &str = "this command is not implemented yet";

type CurrentDirectory = Box<dyn Fn() -> Result<PathBuf, CliError>>;
type HomeDirectory = Box<dyn Fn() -> Option<PathBuf>>;
type Environment = Box<dyn Fn() -> BTreeMap<String, String>>;
type ConfigReader = Box<dyn Fn(&Path) -> Result<Option<String>, CliError>>;
/// Creates a configuration file, failing when one already exists.
type ConfigCreator = Box<dyn Fn(&Path, &str) -> Result<(), CliError>>;
type HeadlessChat = Box<
    dyn Fn(HeadlessChatRequest, &Bootstrap, &HeadlessTurnCancellation) -> Result<String, CliError>,
>;
type TuiLauncher = Box<dyn Fn(&Bootstrap, Option<i64>) -> Result<String, CliError>>;
type AuthLogin = Box<dyn Fn(&Path, bool, &HeadlessTurnCancellation) -> Result<String, CliError>>;

pub struct CliDependencies {
    pub(crate) current_directory: CurrentDirectory,
    pub(crate) home_directory: HomeDirectory,
    pub(crate) environment: Environment,
    pub(crate) read_file: ConfigReader,
    pub(crate) create_file: ConfigCreator,
    pub(crate) headless_chat: HeadlessChat,
    pub(crate) tui_launcher: TuiLauncher,
    pub(crate) auth_login: AuthLogin,
}

impl CliDependencies {
    pub fn production() -> Self {
        Self {
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
            read_file: Box::new(|path| match fs::read_to_string(path) {
                Ok(contents) => Ok(Some(contents)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(_) => Err(CliError::configuration("configuration file is unavailable")),
            }),
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
            auth_login: Box::new(run_production_auth_login),
        }
    }

    pub fn for_test(
        current_directory: PathBuf,
        home_directory: Option<PathBuf>,
        environment: BTreeMap<String, String>,
        files: BTreeMap<PathBuf, String>,
    ) -> Self {
        Self {
            current_directory: Box::new(move || Ok(current_directory.clone())),
            home_directory: Box::new(move || home_directory.clone()),
            environment: Box::new(move || environment.clone()),
            read_file: Box::new(move |path| Ok(files.get(path).cloned())),
            create_file: Box::new(|_, _| Err(CliError::unavailable(UNAVAILABLE_MESSAGE))),
            headless_chat: Box::new(|_, _, _| Err(CliError::unavailable(UNAVAILABLE_MESSAGE))),
            tui_launcher: Box::new(|_, _| Err(CliError::unavailable(UNAVAILABLE_MESSAGE))),
            auth_login: Box::new(|_, _, _| Err(CliError::unavailable(UNAVAILABLE_MESSAGE))),
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
        launcher: impl Fn(&Bootstrap, Option<i64>) -> Result<String, CliError> + 'static,
    ) -> Self {
        self.tui_launcher = Box::new(launcher);
        self
    }

    pub fn with_auth_login(
        mut self,
        login: impl Fn(&Path, bool, &HeadlessTurnCancellation) -> Result<String, CliError> + 'static,
    ) -> Self {
        self.auth_login = Box::new(login);
        self
    }
}
