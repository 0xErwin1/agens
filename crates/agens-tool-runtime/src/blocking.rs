//! Driving an async turn from synchronous code.
//!
//! Tool execution is synchronous and stays that way, but a turn is not, so
//! somewhere the two have to meet. Doing it in one named place keeps the runtime
//! construction identical everywhere and makes the crossing greppable.

use agens_core::HeadlessTurnError;
use agens_error::CliError;

pub fn block_on_headless_turn<T>(
    future: impl std::future::Future<Output = T>,
) -> Result<T, CliError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| CliError::runtime(HeadlessTurnError::Provider))?;

    Ok(runtime.block_on(future))
}
