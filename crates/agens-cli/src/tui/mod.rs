pub(crate) mod engine;
pub(crate) mod extensions;
pub(crate) mod metrics;
pub(crate) mod provider;
pub(crate) mod router;
pub(crate) mod session;

use crate::CliDependencies;
use crate::bootstrap::bootstrap;
use crate::error::CliError;

pub(crate) fn run_tui(
    dependencies: &CliDependencies,
    resume: Option<i64>,
) -> Result<String, CliError> {
    let bootstrap = bootstrap(dependencies)?;
    let output = (dependencies.tui_launcher)(&bootstrap, resume)?;
    Ok(format!("{output}\n"))
}
