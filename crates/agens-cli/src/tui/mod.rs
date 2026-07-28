pub(crate) mod dialogs;
pub(crate) mod engine;
pub(crate) mod extensions;
pub(crate) mod files;
pub(crate) mod metrics;
pub(crate) mod models;
pub(crate) mod resume;
pub(crate) mod router;
pub(crate) mod session;
pub(crate) mod turn;

use crate::CliDependencies;
use crate::deps::bootstrap;
use agens_error::CliError;

pub(crate) fn run_tui(
    dependencies: &CliDependencies,
    resume: Option<i64>,
) -> Result<String, CliError> {
    let bootstrap = bootstrap(dependencies)?;
    let output = (dependencies.tui_launcher)(&bootstrap, resume)?;
    Ok(format!("{output}\n"))
}
