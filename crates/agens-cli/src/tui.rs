//! Launching the terminal surface.
//!
//! The surface itself is `agens-tui-app`. What stays here is the one decision
//! the binary owns: resolving the run's configuration and handing it to the
//! launcher the composition root installed.

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
