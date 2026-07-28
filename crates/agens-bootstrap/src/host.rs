//! How configuration resolution reaches the host it runs on.
//!
//! Separated from the CLI's injection table on purpose: resolving a run needs
//! the working directory, the home directory, the environment and a file
//! reader, and nothing about which command is running or how it reports.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agens_error::CliError;

pub type ConfigReader = Arc<dyn Fn(&Path) -> Result<Option<String>, CliError> + Send + Sync>;
type CurrentDirectory = Box<dyn Fn() -> Result<PathBuf, CliError>>;
type HomeDirectory = Box<dyn Fn() -> Option<PathBuf>>;
type Environment = Box<dyn Fn() -> BTreeMap<String, String>>;

pub struct HostEnvironment {
    pub current_directory: CurrentDirectory,
    pub home_directory: HomeDirectory,
    pub environment: Environment,
    pub read_file: ConfigReader,
}

impl HostEnvironment {
    /// A host whose every answer is fixed, for tests and for callers that must
    /// resolve against something other than the real machine.
    pub fn fixed(
        current_directory: PathBuf,
        home_directory: Option<PathBuf>,
        environment: BTreeMap<String, String>,
        files: BTreeMap<PathBuf, String>,
    ) -> Self {
        Self {
            current_directory: Box::new(move || Ok(current_directory.clone())),
            home_directory: Box::new(move || home_directory.clone()),
            environment: Box::new(move || environment.clone()),
            read_file: Arc::new(move |path| Ok(files.get(path).cloned())),
        }
    }
}
