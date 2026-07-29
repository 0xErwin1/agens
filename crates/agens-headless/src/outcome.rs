//! What a headless turn produced, or failed with.
//!
//! A failure carries the history the attempt already persisted, so a caller can
//! adopt the session it belongs to instead of starting a new one.

use agens_core::{Message, SessionMetadata};

use agens_error::CliError;
use agens_session::attempt::PartialTurnRecord;

pub struct HeadlessChatCompletion {
    pub text: String,
    pub metadata: SessionMetadata,
    pub messages: Vec<Message>,
}

/// Failed turn plus any history the attempt already persisted, so the caller can adopt the
/// session the failed attempt belongs to instead of starting a new one on the next turn.
#[derive(Debug)]
pub struct HeadlessChatFailure {
    pub error: CliError,
    pub partial: Option<Box<PartialTurnRecord>>,
}

impl HeadlessChatFailure {
    pub fn into_error(self) -> CliError {
        self.error
    }

    pub(crate) fn map_error(self, map: impl FnOnce(CliError) -> CliError) -> Self {
        Self {
            error: map(self.error),
            partial: self.partial,
        }
    }
}

impl From<CliError> for HeadlessChatFailure {
    fn from(error: CliError) -> Self {
        Self {
            error,
            partial: None,
        }
    }
}
