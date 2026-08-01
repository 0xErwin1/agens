//! What the turn is doing right now, as a type rather than a UI string.
//!
//! Every surface that names the current activity — the footer's status
//! segment and the transcript's live status row — derives its text from the
//! same value, so the two can never disagree and no label is written at the
//! render site.

use std::borrow::Cow;
use std::time::Duration;

use agens_core::{TurnRetryReason, TurnState};

/// A transient provider failure the runtime is currently waiting out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryActivity {
    pub attempt: u8,
    pub max_attempts: Option<u8>,
    pub delay: Option<Duration>,
    pub reason: TurnRetryReason,
}

/// The single vocabulary of turn activity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnActivity<'a> {
    Ready,
    LoadingSession,
    /// The turn is running but has not reached a state worth naming yet.
    Working,
    /// The request is out and nothing has come back.
    Waiting,
    /// Reasoning tokens are arriving; `elapsed` is how long they have been.
    Thinking {
        elapsed: Option<Duration>,
    },
    Responding,
    UsingTool {
        name: Option<&'a str>,
    },
    Retrying(RetryActivity),
    Cancelling,
    Failed,
    Completed,
}

/// The inputs an activity is derived from, gathered from the TUI state.
pub struct ActivityInputs<'a> {
    pub turn_state: Option<TurnState>,
    pub running: bool,
    pub session_loading: bool,
    pub active_tool: Option<&'a str>,
    pub retry: Option<RetryActivity>,
    /// How long the current reasoning stretch has been running, when one is.
    pub reasoning_elapsed: Option<Duration>,
}

impl<'a> TurnActivity<'a> {
    /// A pending retry outranks the turn state it interrupts: during a backoff
    /// the state is still whatever the failed attempt left behind, and naming
    /// that instead would show the same label as an ordinary response.
    pub fn derive(inputs: ActivityInputs<'a>) -> Self {
        if inputs.session_loading {
            return Self::LoadingSession;
        }
        if let Some(retry) = inputs.retry {
            return Self::Retrying(retry);
        }

        match inputs.turn_state {
            Some(TurnState::Requesting) => match inputs.reasoning_elapsed {
                Some(elapsed) => Self::Thinking {
                    elapsed: Some(elapsed),
                },
                None => Self::Waiting,
            },
            Some(TurnState::Streaming) => Self::Responding,
            Some(TurnState::Dispatching) => Self::UsingTool {
                name: inputs.active_tool,
            },
            Some(TurnState::Cancelled) => Self::Cancelling,
            Some(TurnState::Failed) => Self::Failed,
            Some(TurnState::Completed) => Self::Completed,
            _ if inputs.running => Self::Working,
            _ => Self::Ready,
        }
    }

    /// The compact form the footer's status segment carries.
    pub fn footer_label(self) -> Cow<'a, str> {
        match self {
            Self::Ready => Cow::Borrowed("Ready"),
            Self::LoadingSession => Cow::Borrowed("Loading session…"),
            Self::Working => Cow::Borrowed("Working"),
            Self::Waiting => Cow::Borrowed("Waiting"),
            Self::Thinking { .. } => Cow::Borrowed("Reasoning"),
            Self::Responding => Cow::Borrowed("Responding"),
            Self::UsingTool { .. } => Cow::Borrowed("Using tool"),
            Self::Retrying(retry) => Cow::Owned(format!("Retrying {}", retry.attempts())),
            Self::Cancelling => Cow::Borrowed("Cancelling"),
            Self::Failed => Cow::Borrowed("Failed"),
            Self::Completed => Cow::Borrowed("Completed"),
        }
    }

    /// The fuller form the transcript's live status row carries.
    pub fn status_label(self) -> String {
        match self {
            Self::Working => "Working…".to_owned(),
            Self::Waiting => "Waiting for the model…".to_owned(),
            Self::Thinking { elapsed } => match elapsed {
                Some(elapsed) => format!("Reasoning… {}", reasoning_duration_label(elapsed)),
                None => "Reasoning…".to_owned(),
            },
            Self::Responding => "Responding…".to_owned(),
            Self::UsingTool { name } => match name {
                Some(name) => format!("Using {name}…"),
                None => "Using tool…".to_owned(),
            },
            Self::Retrying(retry) => retry.status_label(),
            _ => self.footer_label().into_owned(),
        }
    }
}

impl RetryActivity {
    fn attempts(self) -> String {
        match self.max_attempts {
            Some(max) => format!("({}/{max})", self.attempt),
            None => format!("({})", self.attempt),
        }
    }

    fn status_label(self) -> String {
        let mut label = format!(
            "Retrying {} — {}",
            self.attempts(),
            reason_label(self.reason)
        );
        if let Some(delay) = self.delay {
            label.push_str(&format!(" · retrying in {}", delay_label(delay)));
        }
        label
    }
}

const fn reason_label(reason: TurnRetryReason) -> &'static str {
    match reason {
        TurnRetryReason::RateLimited => "rate limited",
        TurnRetryReason::ServerError => "provider error",
        TurnRetryReason::Network => "network unreachable",
        TurnRetryReason::Timeout => "timed out",
        TurnRetryReason::Transient => "transient failure",
    }
}

/// Sub-second backoffs are the common case, so the label keeps one decimal
/// rather than rounding every wait below a second to `0s`.
fn delay_label(delay: Duration) -> String {
    if delay.as_secs() > 0 {
        format!("{:.1}s", delay.as_secs_f64())
    } else {
        format!("{}ms", delay.as_millis())
    }
}

fn reasoning_duration_label(elapsed: Duration) -> String {
    if elapsed.as_secs() > 0 {
        format!("{}s", elapsed.as_secs())
    } else {
        format!("{}ms", elapsed.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>() -> ActivityInputs<'a> {
        ActivityInputs {
            turn_state: None,
            running: false,
            session_loading: false,
            active_tool: None,
            retry: None,
            reasoning_elapsed: None,
        }
    }

    #[test]
    fn retry_names_the_attempt_the_reason_and_the_wait() {
        let activity = TurnActivity::derive(ActivityInputs {
            turn_state: Some(TurnState::Requesting),
            running: true,
            retry: Some(RetryActivity {
                attempt: 2,
                max_attempts: Some(3),
                delay: Some(Duration::from_millis(1500)),
                reason: TurnRetryReason::RateLimited,
            }),
            ..inputs()
        });

        assert_eq!(
            activity.status_label(),
            "Retrying (2/3) — rate limited · retrying in 1.5s"
        );
        assert_eq!(activity.footer_label(), "Retrying (2/3)");
    }

    #[test]
    fn retry_without_a_ceiling_or_a_known_wait_still_names_the_attempt() {
        let activity = TurnActivity::Retrying(RetryActivity {
            attempt: 7,
            max_attempts: None,
            delay: None,
            reason: TurnRetryReason::Network,
        });

        assert_eq!(
            activity.status_label(),
            "Retrying (7) — network unreachable"
        );
    }

    #[test]
    fn sub_second_waits_keep_their_resolution() {
        let activity = TurnActivity::Retrying(RetryActivity {
            attempt: 1,
            max_attempts: Some(3),
            delay: Some(Duration::from_millis(250)),
            reason: TurnRetryReason::ServerError,
        });

        assert_eq!(
            activity.status_label(),
            "Retrying (1/3) — provider error · retrying in 250ms"
        );
    }

    #[test]
    fn a_backoff_is_distinguishable_from_an_ordinary_response() {
        let responding = TurnActivity::derive(ActivityInputs {
            turn_state: Some(TurnState::Streaming),
            running: true,
            ..inputs()
        });
        let retrying = TurnActivity::derive(ActivityInputs {
            turn_state: Some(TurnState::Streaming),
            running: true,
            retry: Some(RetryActivity {
                attempt: 1,
                max_attempts: Some(3),
                delay: None,
                reason: TurnRetryReason::Timeout,
            }),
            ..inputs()
        });

        assert_ne!(responding.status_label(), retrying.status_label());
        assert_eq!(responding.status_label(), "Responding…");
    }

    #[test]
    fn reasoning_reports_its_own_duration() {
        let activity = TurnActivity::derive(ActivityInputs {
            turn_state: Some(TurnState::Requesting),
            running: true,
            reasoning_elapsed: Some(Duration::from_secs(12)),
            ..inputs()
        });

        assert_eq!(activity.status_label(), "Reasoning… 12s");
    }

    #[test]
    fn a_dispatching_turn_names_the_tool_it_is_running() {
        let activity = TurnActivity::derive(ActivityInputs {
            turn_state: Some(TurnState::Dispatching),
            running: true,
            active_tool: Some("read"),
            ..inputs()
        });

        assert_eq!(activity.status_label(), "Using read…");
        assert_eq!(activity.footer_label(), "Using tool");
    }

    #[test]
    fn loading_a_session_outranks_everything_else() {
        let activity = TurnActivity::derive(ActivityInputs {
            session_loading: true,
            running: true,
            turn_state: Some(TurnState::Streaming),
            ..inputs()
        });

        assert_eq!(activity.status_label(), "Loading session…");
    }
}
