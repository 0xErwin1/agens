//! The cause of an MCP infrastructure failure, in one closed vocabulary.
//!
//! An MCP call that fails on its connection rather than on its arguments used
//! to reach the model, the terminal and the diagnostics file as the single
//! fixed phrase `tool infrastructure failure`, so nobody could tell a dead
//! server from a rejected frame. The cause is produced where it is observed
//! (`agens-tools`), rendered where the model reads it (`agens-dispatch`), and
//! recorded where supervision reads it (`agens-headless`); this module is the
//! one place the encoding those three share is written down.
//!
//! Every value here is agens-authored: the class comes from the transport
//! error's own category and the detail from a closed set of phrases. No
//! server-supplied text, host path or credential is ever carried, which is
//! what lets the same value be both the model-visible result and a
//! diagnostics field.

/// What broke, at the granularity a reader can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpFailureClass {
    /// The connection itself: the server exited, the stream closed, the
    /// process was never reachable.
    Transport,
    /// The server answered, and the answer was not usable.
    Protocol,
    /// Every attempt this call was allowed reached the server and none
    /// produced an answer.
    RetriesExhausted,
    /// A remote transport answered with a failing HTTP status.
    HttpStatus,
}

impl McpFailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Protocol => "protocol",
            Self::RetriesExhausted => "retries_exhausted",
            Self::HttpStatus => "http_status",
        }
    }

    fn parse(token: &str) -> Option<Self> {
        match token {
            "transport" => Some(Self::Transport),
            "protocol" => Some(Self::Protocol),
            "retries_exhausted" => Some(Self::RetriesExhausted),
            "http_status" => Some(Self::HttpStatus),
            _ => None,
        }
    }
}

/// A detail longer than this is not a closed phrase any more, so it is
/// dropped rather than truncated: half a phrase reads like a whole one.
const MAX_DETAIL_CHARS: usize = 120;

/// What the model, the terminal and the diagnostics record all see when the
/// cause could not be established.
pub const FIXED_TOOL_RESULT: &str = "tool infrastructure failure";

/// The `Error::Extension` message an MCP call failure carries when it has no
/// cause attached, kept so a build that cannot classify a failure still
/// produces the result callers already handle.
pub const FIXED_ERROR_MESSAGE: &str = "mcp tool infrastructure failure";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpFailure {
    class: McpFailureClass,
    detail: String,
}

impl McpFailure {
    /// Rejects a detail that is empty, oversized, or carries anything but a
    /// single line of printable text, so a value that did not come from the
    /// closed vocabulary degrades to the fixed phrase instead of publishing
    /// whatever it held.
    pub fn new(class: McpFailureClass, detail: &str) -> Option<Self> {
        let detail = detail.trim();
        let usable = !detail.is_empty()
            && detail.chars().count() <= MAX_DETAIL_CHARS
            && detail
                .chars()
                .all(|character| !character.is_control() && character.is_ascii());

        usable.then(|| Self {
            class,
            detail: detail.to_owned(),
        })
    }

    pub const fn class(&self) -> McpFailureClass {
        self.class
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// `transport: call failed` — the class and its detail, as one phrase.
    pub fn cause(&self) -> String {
        format!("{}: {}", self.class.as_str(), self.detail)
    }

    /// The `Error::Extension` payload an MCP tool call fails with.
    pub fn error_message(&self) -> String {
        format!("{FIXED_ERROR_MESSAGE}: {}", self.cause())
    }

    /// The model-visible tool result.
    pub fn tool_result(&self) -> String {
        format!("{FIXED_TOOL_RESULT}: {}", self.cause())
    }

    pub fn from_error_message(message: &str) -> Option<Self> {
        Self::from_prefixed(message, FIXED_ERROR_MESSAGE)
    }

    pub fn from_tool_result(content: &str) -> Option<Self> {
        Self::from_prefixed(content, FIXED_TOOL_RESULT)
    }

    fn from_prefixed(text: &str, prefix: &str) -> Option<Self> {
        let cause = text.strip_prefix(prefix)?.strip_prefix(": ")?;
        let (class, detail) = cause.split_once(": ")?;

        Self::new(McpFailureClass::parse(class)?, detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cause_survives_both_encodings_unchanged() {
        let failure = McpFailure::new(McpFailureClass::Transport, "call failed").unwrap();

        assert_eq!(
            failure.error_message(),
            "mcp tool infrastructure failure: transport: call failed"
        );
        assert_eq!(
            failure.tool_result(),
            "tool infrastructure failure: transport: call failed"
        );
        assert_eq!(
            McpFailure::from_error_message(&failure.error_message()),
            Some(failure.clone())
        );
        assert_eq!(
            McpFailure::from_tool_result(&failure.tool_result()),
            Some(failure)
        );
    }

    /// The fixed phrase stays a valid message with no cause behind it: a
    /// reader that finds no cause must fall back rather than invent one.
    #[test]
    fn a_message_without_a_recognized_cause_carries_none() {
        for message in [
            FIXED_ERROR_MESSAGE,
            "mcp tool infrastructure failure: ",
            "mcp tool infrastructure failure: transport",
            "mcp tool infrastructure failure: invented: call failed",
            "unrelated failure: transport: call failed",
        ] {
            assert_eq!(McpFailure::from_error_message(message), None, "{message}");
        }
        assert_eq!(McpFailure::from_tool_result(FIXED_TOOL_RESULT), None);
    }

    /// A detail is only ever an agens-authored phrase, so anything that could
    /// carry remote text, a path fragment, or a second line is refused at
    /// construction rather than rendered.
    #[test]
    fn a_detail_outside_the_closed_shape_is_refused() {
        for detail in ["", "   ", "call\nfailed", "call\tfailed", "llamada fallída"] {
            assert_eq!(
                McpFailure::new(McpFailureClass::Transport, detail),
                None,
                "{detail:?}"
            );
        }
        assert_eq!(
            McpFailure::new(McpFailureClass::Protocol, &"x".repeat(MAX_DETAIL_CHARS + 1)),
            None
        );
        assert!(
            McpFailure::new(McpFailureClass::Protocol, &"x".repeat(MAX_DETAIL_CHARS)).is_some()
        );
    }
}
