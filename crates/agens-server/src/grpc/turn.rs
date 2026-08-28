//! A hosted chat's events, as the wire carries them.
//!
//! The domain's own [`TurnEvent`] is projected structurally rather than encoded
//! into a blob: a client draws these, and a blob would make every surface parse
//! the daemon's private encoding to render a line of text.
//!
//! Turn state and retry reasons cross as strings, the way every other
//! vocabulary on this wire does. Unlike those, these two have no stored spelling
//! to borrow — nothing writes a turn's state down — so the names are defined
//! here, which is the one place that has to agree with itself.
//!
//! One variant is deliberately not projected. [`TurnEvent::ToolResultFacts`] is
//! what the daemon feeds its own ingest with, not something a surface renders —
//! the terminal already discards it (`agens-tui-app/src/metrics.rs`) — and a
//! run's health is readable through the Feed. It is dropped at the boundary
//! rather than mapped onto a shape that does not mean it.

use agens_core::ask_user::{AskUserMode, AskUserOption, AskUserQuestion, AskUserRequest};
use agens_core::{
    IntraTurnInputSource, Message, MessagePart, Role, TurnEvent, TurnRetryReason, TurnState, Usage,
};

use crate::chat::ChatEvent;

use super::proto;

/// One chat event, or `None` for the one a client has no use for.
///
/// A skipped event is not an error and leaves no gap a client can observe:
/// nothing downstream of this counts events, and what was skipped carries no
/// state a later event depends on.
pub(super) fn session_event(session_id: i64, event: &ChatEvent) -> Option<proto::SessionEvent> {
    let event = match event {
        ChatEvent::Progress(progress) => {
            proto::session_event::Event::Progress(turn_progress(progress)?)
        }
        ChatEvent::TurnCompleted { text } => {
            proto::session_event::Event::TurnCompleted(proto::TurnCompleted { text: text.clone() })
        }
        ChatEvent::TurnFailed { detail } => {
            proto::session_event::Event::TurnFailed(proto::TurnFailed {
                detail: detail.clone(),
            })
        }
        ChatEvent::PermissionAsked { prompt_id, request } => {
            proto::session_event::Event::PermissionAsked(proto::PermissionAsked {
                prompt_id: *prompt_id,
                tool: request.tool.clone(),
                target: request.target.clone(),
                access: request.access.clone(),
                reason: request.reason.clone(),
            })
        }
        ChatEvent::AskUserAsked { prompt_id, request } => {
            proto::session_event::Event::AskUserAsked(ask_user_request(*prompt_id, request))
        }
        ChatEvent::Closed => proto::session_event::Event::Closed(proto::ChatClosed {}),
    };

    Some(proto::SessionEvent {
        session_id,
        event: Some(event),
    })
}

fn ask_user_request(prompt_id: u64, request: &AskUserRequest) -> proto::AskUserAsked {
    proto::AskUserAsked {
        prompt_id,
        title: request.title().map(str::to_owned),
        questions: request.questions().iter().map(ask_user_question).collect(),
    }
}

fn ask_user_question(question: &AskUserQuestion) -> proto::AskUserQuestion {
    proto::AskUserQuestion {
        id: question.id().to_owned(),
        prompt: question.prompt().to_owned(),
        explanation: question.explanation().map(str::to_owned),
        mode: match question.mode() {
            AskUserMode::Single => "single",
            AskUserMode::Multiple => "multiple",
        }
        .to_owned(),
        options: question.options().iter().map(ask_user_option).collect(),
        allow_other: question.allow_other(),
        allow_note: question.allow_note(),
        allow_discuss: question.allow_discuss(),
    }
}

fn ask_user_option(option: &AskUserOption) -> proto::AskUserOption {
    proto::AskUserOption {
        id: option.id().to_owned(),
        label: option.label().to_owned(),
        explanation: option.explanation().map(str::to_owned),
        context: option.context().map(str::to_owned),
    }
}

/// One stored message, as the wire carries it.
pub(super) fn message(message: &Message) -> proto::Message {
    proto::Message {
        role: role_name(message.role).to_owned(),
        parts: message.parts.iter().map(message_part).collect(),
    }
}

const fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::Supervisor => "supervisor",
    }
}

fn turn_progress(event: &TurnEvent) -> Option<proto::TurnProgress> {
    use proto::turn_progress::Event;

    let event = match event {
        TurnEvent::StateChanged(state) => Event::State(state_name(*state).to_owned()),
        TurnEvent::ProviderPart(part) => Event::ProviderPart(message_part(part)),
        TurnEvent::Usage(usage) => Event::Usage(self::usage(usage)),
        TurnEvent::ToolCallRequested { id, name, input } => {
            Event::ToolCallRequested(proto::ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            })
        }
        TurnEvent::ToolResult(part) => Event::ToolResult(message_part(part)),
        TurnEvent::ProviderRetry {
            attempt,
            max_attempts,
            delay,
            reason,
        } => Event::ProviderRetry(proto::ProviderRetry {
            attempt: u32::from(*attempt),
            max_attempts: max_attempts.map(u32::from),
            delay_millis: delay.map(|delay| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)),
            reason: retry_reason(*reason).to_owned(),
        }),
        TurnEvent::IntraTurnInput { source, text } => {
            Event::IntraTurnInput(proto::IntraTurnInput {
                source: source_name(*source).to_owned(),
                text: text.clone(),
            })
        }
        TurnEvent::ToolResultFacts { .. } => return None,
    };

    Some(proto::TurnProgress { event: Some(event) })
}

fn message_part(part: &MessagePart) -> proto::MessagePart {
    use proto::message_part::Part;

    let part = match part {
        MessagePart::Text(text) => Part::Text(text.clone()),
        MessagePart::Reasoning(text) => Part::Reasoning(text.clone()),
        MessagePart::ToolCall { id, name, input } => Part::ToolCall(proto::ToolCall {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        }),
        MessagePart::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => Part::ToolResult(proto::ToolResult {
            tool_call_id: tool_call_id.clone(),
            content: content.clone(),
            is_error: *is_error,
        }),
        MessagePart::Media { media_id, mime } => Part::Media(proto::Media {
            media_id: *media_id,
            mime: mime.clone(),
        }),
    };

    proto::MessagePart { part: Some(part) }
}

const fn usage(usage: &Usage) -> proto::Usage {
    proto::Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        context_window: usage.context_window,
    }
}

const fn state_name(state: TurnState) -> &'static str {
    match state {
        TurnState::Idle => "idle",
        TurnState::Requesting => "requesting",
        TurnState::Streaming => "streaming",
        TurnState::Dispatching => "dispatching",
        TurnState::Completed => "completed",
        TurnState::Cancelled => "cancelled",
        TurnState::Failed => "failed",
    }
}

const fn retry_reason(reason: TurnRetryReason) -> &'static str {
    match reason {
        TurnRetryReason::RateLimited => "rate_limited",
        TurnRetryReason::ServerError => "server_error",
        TurnRetryReason::Network => "network",
        TurnRetryReason::Timeout => "timeout",
        TurnRetryReason::Transient => "transient",
    }
}

const fn source_name(source: IntraTurnInputSource) -> &'static str {
    match source {
        IntraTurnInputSource::Human => "human",
        IntraTurnInputSource::Supervisor => "supervisor",
    }
}
