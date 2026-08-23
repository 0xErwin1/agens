//! Turning what the chat plane sends back into the domain's own types.
//!
//! The inverse of the daemon's projection, and deliberately strict about it: a
//! field the wire left unset is reported rather than filled in with a default,
//! because a `MessagePart` with no part is not empty text, it is a client and a
//! daemon disagreeing about the wire. Guessing at that point would render
//! something the turn never produced.
//!
//! The one place a default is correct is a name this client does not know. Turn
//! states and retry reasons cross as strings so the two sides can be one
//! version apart without either refusing to talk, and a state a newer daemon
//! learned reads here as the nearest thing this client can say rather than as a
//! broken stream.

use std::time::Duration;

use agens_core::{
    IntraTurnInputSource, Message, MessagePart, Role, TurnEvent, TurnRetryReason, TurnState, Usage,
};

use crate::ClientError;
use crate::proto;

/// One thing a hosted chat did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostedChatEvent {
    /// Exactly what a locally run turn produces, so a surface renders it with
    /// the code it already has.
    Progress(TurnEvent),
    /// A decision the turn cannot make for itself. The turn is stopped on this
    /// until it is answered, so a client that renders one has to answer it.
    PermissionAsked(PermissionQuestion),
    TurnCompleted {
        text: String,
    },
    TurnFailed {
        detail: String,
    },
    /// The chat has ended and will publish nothing further.
    Closed,
}

/// A permission question a hosted turn is stopped on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionQuestion {
    /// What an answer names. Unique within one chat, while the question is
    /// open.
    pub prompt_id: u64,
    pub tool: String,
    pub target: String,
    pub access: String,
    pub reason: String,
}

/// What a client may answer a permission question with.
///
/// Deliberately without an "unheard": that is what the daemon concludes when
/// nobody answers, never something a client says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
}

impl PermissionDecision {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::AllowAlways => "allow_always",
            Self::DenyOnce => "deny_once",
            Self::DenyAlways => "deny_always",
        }
    }
}

pub(crate) fn session_event(event: proto::SessionEvent) -> Result<HostedChatEvent, ClientError> {
    match required(event.event, "session event")? {
        proto::session_event::Event::Progress(progress) => {
            Ok(HostedChatEvent::Progress(turn_event(progress)?))
        }
        proto::session_event::Event::TurnCompleted(completed) => {
            Ok(HostedChatEvent::TurnCompleted {
                text: completed.text,
            })
        }
        proto::session_event::Event::TurnFailed(failed) => Ok(HostedChatEvent::TurnFailed {
            detail: failed.detail,
        }),
        proto::session_event::Event::PermissionAsked(asked) => {
            Ok(HostedChatEvent::PermissionAsked(PermissionQuestion {
                prompt_id: asked.prompt_id,
                tool: asked.tool,
                target: asked.target,
                access: asked.access,
                reason: asked.reason,
            }))
        }
        proto::session_event::Event::Closed(_) => Ok(HostedChatEvent::Closed),
    }
}

/// One stored message, as the client reads it back.
pub(crate) fn message(message: proto::Message) -> Result<Message, ClientError> {
    Ok(Message {
        role: role(&message.role)?,
        parts: message
            .parts
            .into_iter()
            .map(message_part)
            .collect::<Result<Vec<_>, _>>()?,
    })
}

/// A role, refused rather than guessed at.
///
/// Unlike a turn state, this one is not forgiving of a name it does not know.
/// Who spoke decides how a message is rendered and what authority it carries,
/// and a message attributed to the wrong speaker is worse than a message that
/// did not arrive.
fn role(role: &str) -> Result<Role, ClientError> {
    match role {
        "system" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        "supervisor" => Ok(Role::Supervisor),
        other => Err(ClientError::Unreadable(format!(
            "a message came back from a speaker this client does not know: {other}"
        ))),
    }
}

fn turn_event(progress: proto::TurnProgress) -> Result<TurnEvent, ClientError> {
    use proto::turn_progress::Event;

    Ok(match required(progress.event, "turn progress")? {
        Event::State(state) => TurnEvent::StateChanged(turn_state(&state)),
        Event::ProviderPart(part) => TurnEvent::ProviderPart(message_part(part)?),
        Event::Usage(usage) => TurnEvent::Usage(Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            context_window: usage.context_window,
        }),
        Event::ToolCallRequested(call) => TurnEvent::ToolCallRequested {
            id: call.id,
            name: call.name,
            input: call.input,
        },
        Event::ToolResult(part) => TurnEvent::ToolResult(message_part(part)?),
        Event::ProviderRetry(retry) => TurnEvent::ProviderRetry {
            attempt: attempt(retry.attempt),
            max_attempts: retry.max_attempts.map(attempt),
            delay: retry.delay_millis.map(Duration::from_millis),
            reason: retry_reason(&retry.reason),
        },
        Event::IntraTurnInput(input) => TurnEvent::IntraTurnInput {
            source: input_source(&input.source),
            text: input.text,
        },
    })
}

fn message_part(part: proto::MessagePart) -> Result<MessagePart, ClientError> {
    use proto::message_part::Part;

    Ok(match required(part.part, "message part")? {
        Part::Text(text) => MessagePart::Text(text),
        Part::Reasoning(text) => MessagePart::Reasoning(text),
        Part::ToolCall(call) => MessagePart::ToolCall {
            id: call.id,
            name: call.name,
            input: call.input,
        },
        Part::ToolResult(result) => MessagePart::ToolResult {
            tool_call_id: result.tool_call_id,
            content: result.content,
            is_error: result.is_error,
        },
        Part::Media(media) => MessagePart::Media {
            media_id: media.media_id,
            mime: media.mime,
        },
    })
}

/// A turn state a newer daemon may know and this client may not.
///
/// `Streaming` is the fallback rather than `Idle` or `Failed`: what an unknown
/// state means for a surface is "the turn is doing something", and the two
/// alternatives would each assert something specific and possibly false — that
/// nothing is happening, or that it went wrong.
fn turn_state(state: &str) -> TurnState {
    match state {
        "idle" => TurnState::Idle,
        "requesting" => TurnState::Requesting,
        "dispatching" => TurnState::Dispatching,
        "completed" => TurnState::Completed,
        "cancelled" => TurnState::Cancelled,
        "failed" => TurnState::Failed,
        _ => TurnState::Streaming,
    }
}

/// A retry reason falls back to `Transient`, which is what every reason on this
/// enum has in common: the turn is about to try again.
fn retry_reason(reason: &str) -> TurnRetryReason {
    match reason {
        "rate_limited" => TurnRetryReason::RateLimited,
        "server_error" => TurnRetryReason::ServerError,
        "network" => TurnRetryReason::Network,
        "timeout" => TurnRetryReason::Timeout,
        _ => TurnRetryReason::Transient,
    }
}

/// Anything that is not explicitly a person is read as a supervisor.
///
/// The two do not carry the same authority — a person can widen what a turn is
/// allowed to do — so an unreadable source is taken as the narrower of them.
fn input_source(source: &str) -> IntraTurnInputSource {
    if source == "human" {
        IntraTurnInputSource::Human
    } else {
        IntraTurnInputSource::Supervisor
    }
}

/// Attempt counts are small and the wire's are wider, so a value past what the
/// domain holds is saturated rather than wrapped: a twelfth attempt reported as
/// the first would read as a turn starting over.
fn attempt(value: u32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

fn required<T>(value: Option<T>, subject: &str) -> Result<T, ClientError> {
    value.ok_or_else(|| ClientError::Unreadable(format!("a {subject} carried nothing")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streamed_part_comes_back_as_the_part_the_turn_produced() {
        let event = proto::SessionEvent {
            session_id: 1,
            event: Some(proto::session_event::Event::Progress(proto::TurnProgress {
                event: Some(proto::turn_progress::Event::ProviderPart(
                    proto::MessagePart {
                        part: Some(proto::message_part::Part::Text("hello".to_owned())),
                    },
                )),
            })),
        };

        assert_eq!(
            session_event(event).unwrap(),
            HostedChatEvent::Progress(TurnEvent::ProviderPart(MessagePart::Text(
                "hello".to_owned()
            ))),
        );
    }

    /// A daemon one version ahead is not a broken stream. The state it named is
    /// read as the nearest thing this client can say rather than refused.
    #[test]
    fn a_turn_state_this_client_does_not_know_is_read_as_the_turn_working() {
        assert_eq!(turn_state("verifying"), TurnState::Streaming);
        assert_eq!(turn_state("completed"), TurnState::Completed);
    }

    /// A field the wire left unset is a disagreement about the wire, not an
    /// empty value: filling it in would render something no turn produced.
    #[test]
    fn an_event_carrying_nothing_is_reported_rather_than_guessed_at() {
        let empty = proto::SessionEvent {
            session_id: 1,
            event: None,
        };

        assert!(matches!(
            session_event(empty),
            Err(ClientError::Unreadable(_))
        ));

        let hollow = proto::SessionEvent {
            session_id: 1,
            event: Some(proto::session_event::Event::Progress(proto::TurnProgress {
                event: Some(proto::turn_progress::Event::ProviderPart(
                    proto::MessagePart { part: None },
                )),
            })),
        };

        assert!(matches!(
            session_event(hollow),
            Err(ClientError::Unreadable(_))
        ));
    }

    /// An attempt past what the domain counts is saturated, never wrapped: a
    /// twelfth attempt reported as the first would read as a turn starting over.
    #[test]
    fn an_attempt_count_past_the_domains_width_saturates() {
        assert_eq!(attempt(3), 3);
        assert_eq!(attempt(4_000), u8::MAX);
    }

    /// Who spoke decides how a message is rendered and what authority it
    /// carries, so an unknown speaker is refused rather than guessed at.
    #[test]
    fn a_message_from_a_speaker_this_client_does_not_know_is_refused() {
        assert!(matches!(role("assistant"), Ok(Role::Assistant)));
        assert!(matches!(role("oracle"), Err(ClientError::Unreadable(_))));
    }
}
