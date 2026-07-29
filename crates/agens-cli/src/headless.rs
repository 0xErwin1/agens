//! Choosing a surface for a headless run.
//!
//! The turn itself lives in `agens-headless` and asks its questions through a
//! `PermissionPrompter` the caller supplies. Picking the terminal one is a
//! composition decision, so it stays here.

use agens_bootstrap::Bootstrap;
use agens_core::HeadlessTurnCancellation;
use agens_error::CliError;
use agens_headless::{
    HeadlessChatFailure, HeadlessChatRequest, run_production_headless_chat_with_progress,
};

use crate::permission_prompt::TtyPermissionPrompter;

pub(crate) fn run_production_headless_chat(
    request: HeadlessChatRequest,
    bootstrap: &Bootstrap,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    run_production_headless_chat_with_progress(
        request,
        bootstrap,
        cancellation,
        None,
        Box::new(TtyPermissionPrompter),
        None,
        None,
    )
    .map(|completion| completion.text)
    .map_err(HeadlessChatFailure::into_error)
}
