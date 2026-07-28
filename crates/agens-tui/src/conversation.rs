//! Typed, lossless source projection for one visible conversation turn.

use std::time::Duration;

use crate::bridge::SubagentErrorPresentation;
use agens_core::{DiffLine, Message, MessagePart, Role, SubagentStatus, ToolInput};
use agens_core::{TuiExecutionState, TuiSubagentEvent, TuiSubagentUpdate};

/// A source event accepted by the conversation projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationEvent {
    Info(String),
    MarkdownDelta(String),
    MarkdownFinal(String),
    ReasoningDelta(String),
    ToolCall {
        call_id: String,
        name: String,
        input: String,
        parsed: ToolInput,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    Diff(Vec<DiffLine>),
    Error {
        message: String,
        action: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionableError {
    pub message: String,
    pub action: String,
}

impl ActionableError {
    fn sanitized(message: String, action: String) -> Self {
        Self {
            message: sanitize_error_message(message),
            action,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub input: String,
    pub parsed: ToolInput,
    pub result: Option<ToolResult>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolBatch {
    pub calls: Vec<ToolCall>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentCard {
    pub id: u64,
    pub agent: String,
    pub task_summary: String,
    pub presentation: TuiExecutionState,
    pub tool_calls: Vec<ToolCall>,
    pub tool_uses: usize,
    pub activities: Vec<String>,
    pub status: Option<SubagentStatus>,
    pub final_result: Option<String>,
    pub started_at: Option<Duration>,
    pub terminal_at: Option<Duration>,
    pub has_activity: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationError {
    OrphanToolResult(String),
    DuplicateToolCall(String),
    DuplicateToolResult(String),
    InvalidMessageOrder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ConversationItem {
    Info(String),
    User(String),
    Assistant(String),
    Reasoning(String),
    ToolCall {
        call_id: String,
        name: String,
        input: String,
        parsed: ToolInput,
        batch: Option<usize>,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    Diff(Vec<DiffLine>),
    Error(ActionableError),
    SubagentCard(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub user: String,
    pub info: Vec<String>,
    pub live_markdown: String,
    pub final_markdown: Option<String>,
    pub reasoning: String,
    pub tool_batches: Vec<ToolBatch>,
    pub diffs: Vec<DiffLine>,
    pub errors: Vec<ActionableError>,
    pub subagent_cards: Vec<SubagentCard>,
    pub(super) items: Vec<ConversationItem>,
    last_was_tool_call: bool,
}

impl Conversation {
    pub fn new(user: impl Into<String>) -> Self {
        let user = user.into();
        Self {
            items: (!user.is_empty())
                .then(|| ConversationItem::User(user.clone()))
                .into_iter()
                .collect(),
            user,
            info: Vec::new(),
            live_markdown: String::new(),
            final_markdown: None,
            reasoning: String::new(),
            tool_batches: Vec::new(),
            diffs: Vec::new(),
            errors: Vec::new(),
            subagent_cards: Vec::new(),
            last_was_tool_call: false,
        }
    }
    /// Reconstructs completed conversations from persisted messages.
    ///
    /// Restored tool calls degrade `parsed` to [`ToolInput::Other`] since no
    /// authoritative parser is available at this crate boundary. Use
    /// [`Self::from_messages_with_parser`] when an accurate `parsed` value is
    /// required (the production restore path in `agens-cli`).
    pub fn from_messages(messages: &[Message]) -> Result<Vec<Self>, ConversationError> {
        Self::from_messages_with_parser(messages, |name, input| ToolInput::Other {
            name: name.to_owned(),
            raw: input.to_owned(),
        })
    }

    /// Reconstructs completed conversations from persisted messages, deriving
    /// `parsed` for each restored tool call via `parse_tool_input`.
    ///
    /// Restored tool call names are qualified (e.g. `native::read`), unlike
    /// the bare names carried by the live event stream; callers that reuse a
    /// bare-name parser must strip the `native::`/`mcp::` prefix themselves.
    pub fn from_messages_with_parser(
        messages: &[Message],
        parse_tool_input: impl Fn(&str, &str) -> ToolInput,
    ) -> Result<Vec<Self>, ConversationError> {
        let mut conversations = Vec::new();
        let mut current: Option<Self> = None;
        let mut pending_system = Vec::new();
        for message in messages {
            match message.role {
                Role::System => {
                    if let Some(conversation) = current.take() {
                        conversations.push(conversation);
                    }
                    for part in &message.parts {
                        let MessagePart::Text(text) = part else {
                            return Err(ConversationError::InvalidMessageOrder);
                        };
                        pending_system.push(text.clone());
                    }
                }
                Role::User => {
                    if let Some(conversation) = current.take() {
                        conversations.push(conversation);
                    }
                    let mut conversation = Self::new(String::new());
                    for message in pending_system.drain(..) {
                        conversation.apply(ConversationEvent::Info(message))?;
                    }
                    for part in &message.parts {
                        let MessagePart::Text(text) = part else {
                            return Err(ConversationError::InvalidMessageOrder);
                        };
                        conversation.user.push_str(text);
                        let item = ConversationItem::User(text.clone());
                        conversation.items.push(item);
                    }
                    current = Some(conversation);
                }
                Role::Assistant => {
                    let conversation = current
                        .as_mut()
                        .ok_or(ConversationError::InvalidMessageOrder)?;
                    for part in &message.parts {
                        let event = match part {
                            MessagePart::Text(text) => {
                                ConversationEvent::MarkdownDelta(text.clone())
                            }
                            MessagePart::Reasoning(text) => {
                                ConversationEvent::ReasoningDelta(text.clone())
                            }
                            MessagePart::ToolCall { id, name, input } => {
                                ConversationEvent::ToolCall {
                                    call_id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                    parsed: parse_tool_input(name, input),
                                }
                            }
                            MessagePart::ToolResult { .. } => {
                                return Err(ConversationError::InvalidMessageOrder);
                            }
                        };
                        conversation.apply(event)?;
                    }
                }
                Role::Tool => {
                    let conversation = current
                        .as_mut()
                        .ok_or(ConversationError::InvalidMessageOrder)?;
                    for part in &message.parts {
                        let MessagePart::ToolResult {
                            tool_call_id,
                            content,
                            is_error,
                        } = part
                        else {
                            return Err(ConversationError::InvalidMessageOrder);
                        };
                        conversation.apply(ConversationEvent::ToolResult {
                            call_id: tool_call_id.clone(),
                            output: content.clone(),
                            is_error: *is_error,
                        })?;
                    }
                }
            }
        }
        if !pending_system.is_empty() {
            return Err(ConversationError::InvalidMessageOrder);
        }
        if let Some(conversation) = current {
            conversations.push(conversation);
        }
        Ok(conversations)
    }
    pub fn apply(&mut self, event: ConversationEvent) -> Result<(), ConversationError> {
        let is_tool_call = matches!(&event, ConversationEvent::ToolCall { .. });
        match event {
            ConversationEvent::Info(message) => {
                self.info.push(message.clone());
                self.items.push(ConversationItem::Info(message));
            }
            ConversationEvent::MarkdownDelta(delta) => {
                self.live_markdown.push_str(&delta);
                push_text_item(&mut self.items, delta, false);
            }
            ConversationEvent::MarkdownFinal(markdown) => {
                self.final_markdown = Some(markdown.clone());
                self.items
                    .retain(|item| !matches!(item, ConversationItem::Assistant(_)));
                self.items.push(ConversationItem::Assistant(markdown));
            }
            ConversationEvent::ReasoningDelta(delta) => {
                self.reasoning.push_str(&delta);
                push_text_item(&mut self.items, delta, true);
            }
            ConversationEvent::ToolCall {
                call_id,
                name,
                input,
                parsed,
            } => {
                if self.find_call(&call_id).is_some() {
                    return Err(ConversationError::DuplicateToolCall(call_id));
                }
                let batch = if !self.last_was_tool_call {
                    self.tool_batches.push(ToolBatch::default());
                    Some(self.tool_batches.len())
                } else {
                    None
                };
                self.tool_batches
                    .last_mut()
                    .expect("tool batch was created")
                    .calls
                    .push(ToolCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        parsed: parsed.clone(),
                        result: None,
                    });
                self.items.push(ConversationItem::ToolCall {
                    call_id,
                    name,
                    input,
                    parsed,
                    batch,
                });
            }
            ConversationEvent::ToolResult {
                call_id,
                output,
                is_error,
            } => {
                let Some(call) = self.find_call_mut(&call_id) else {
                    return Err(ConversationError::OrphanToolResult(call_id));
                };
                if call.result.is_some() {
                    return Err(ConversationError::DuplicateToolResult(call_id));
                }
                call.result = Some(ToolResult {
                    output: output.clone(),
                    is_error,
                });
                self.items.push(ConversationItem::ToolResult {
                    call_id,
                    output,
                    is_error,
                });
            }
            ConversationEvent::Diff(lines) => {
                self.diffs.extend(lines.clone());
                self.items.push(ConversationItem::Diff(lines));
            }
            ConversationEvent::Error { message, action } => {
                let error = ActionableError::sanitized(message, action);
                self.errors.push(error.clone());
                self.items.push(ConversationItem::Error(error));
            }
        }
        self.last_was_tool_call = is_tool_call;
        Ok(())
    }
    pub(crate) fn apply_subagent_summary(&mut self, event: TuiSubagentEvent, now: Duration) {
        match event.update {
            TuiSubagentUpdate::Started {
                agent,
                task_summary,
                presentation,
            } if self.subagent_cards.iter().all(|card| card.id != event.id) => {
                self.subagent_cards.push(SubagentCard {
                    id: event.id,
                    agent,
                    task_summary,
                    presentation,
                    tool_calls: Vec::new(),
                    tool_uses: 0,
                    activities: Vec::new(),
                    status: None,
                    final_result: None,
                    started_at: Some(now),
                    terminal_at: None,
                    has_activity: false,
                });
                self.items.push(ConversationItem::SubagentCard(event.id));
            }
            TuiSubagentUpdate::ToolCall { name, .. } => {
                if let Some(card) = self
                    .subagent_cards
                    .iter_mut()
                    .find(|card| card.id == event.id && card.status.is_none())
                {
                    card.tool_uses += 1;
                    card.has_activity = true;
                    if card.activities.len() < 3
                        && let Some(activity) = subagent_activity(&name)
                    {
                        card.activities.push(activity.into());
                    }
                }
            }
            TuiSubagentUpdate::Reasoning(_)
            | TuiSubagentUpdate::Text(_)
            | TuiSubagentUpdate::ToolResult { .. }
            | TuiSubagentUpdate::Error { .. } => {
                if let Some(card) = self
                    .subagent_cards
                    .iter_mut()
                    .find(|card| card.id == event.id && card.status.is_none())
                {
                    card.has_activity = true;
                }
            }
            TuiSubagentUpdate::Terminal {
                status,
                final_result,
            } => {
                if let Some(card) = self
                    .subagent_cards
                    .iter_mut()
                    .find(|card| card.id == event.id && card.status.is_none())
                {
                    card.status = Some(status);
                    card.final_result = Some(final_result);
                    card.terminal_at = Some(now);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn apply_child_event(&mut self, event: TuiSubagentEvent) {
        let result = match event.update {
            TuiSubagentUpdate::Started { .. } => return,
            TuiSubagentUpdate::Reasoning(delta) => {
                self.apply(ConversationEvent::ReasoningDelta(delta))
            }
            TuiSubagentUpdate::Text(delta) => self.apply(ConversationEvent::MarkdownDelta(delta)),
            TuiSubagentUpdate::ToolCall {
                call_id,
                name,
                input,
                parsed,
            } => self.apply(ConversationEvent::ToolCall {
                call_id,
                name,
                input,
                parsed,
            }),
            TuiSubagentUpdate::ToolResult {
                call_id,
                output,
                is_error,
            } => self.apply(ConversationEvent::ToolResult {
                call_id,
                output,
                is_error,
            }),
            TuiSubagentUpdate::Error { kind, reference } => self.apply(ConversationEvent::Error {
                message: reference.map_or_else(
                    || kind.message().into(),
                    |reference| format!("{} [ref: {reference}]", kind.message()),
                ),
                action: kind.action().into(),
            }),
            TuiSubagentUpdate::Terminal { final_result, .. } => {
                self.final_markdown = Some(final_result.clone());
                self.items.push(ConversationItem::Assistant(final_result));
                Ok(())
            }
        };
        let _ = result;
    }

    pub(crate) fn restore_completed_subagent(
        &mut self,
        id: u64,
        agent: String,
        task_summary: String,
        final_result: String,
        tool_uses: usize,
    ) {
        if self.subagent_cards.iter().any(|card| card.id == id) {
            return;
        }

        self.subagent_cards.push(SubagentCard {
            id,
            agent,
            task_summary,
            presentation: TuiExecutionState::CompletedRecent,
            tool_calls: Vec::new(),
            tool_uses,
            activities: Vec::new(),
            status: Some(SubagentStatus::Success),
            final_result: Some(final_result),
            started_at: None,
            terminal_at: None,
            has_activity: true,
        });
        self.items.push(ConversationItem::SubagentCard(id));
    }
    /// Corrects a live tool call's `parsed` value once the typed
    /// `TuiRuntimeEvent::ToolStarted` carrier arrives for `call_id`.
    ///
    /// The live projection path (raw `TurnEvent`) has no parser available, so
    /// it records a placeholder `ToolInput::Other` at call time; this is the
    /// single point that ever writes an authoritative `parsed` value for a
    /// live call.
    pub(crate) fn enrich_parsed_tool_input(&mut self, call_id: &str, parsed: ToolInput) {
        if let Some(call) = self.find_call_mut(call_id) {
            call.parsed = parsed.clone();
        }
        for item in &mut self.items {
            if let ConversationItem::ToolCall {
                call_id: id,
                parsed: slot,
                ..
            } = item
                && id == call_id
            {
                *slot = parsed;
                break;
            }
        }
    }

    fn find_call(&self, call_id: &str) -> Option<&ToolCall> {
        self.tool_batches
            .iter()
            .flat_map(|batch| &batch.calls)
            .find(|call| call.call_id == call_id)
    }

    fn find_call_mut(&mut self, call_id: &str) -> Option<&mut ToolCall> {
        self.tool_batches
            .iter_mut()
            .flat_map(|batch| &mut batch.calls)
            .find(|call| call.call_id == call_id)
    }
}

pub(crate) fn subagent_activity(name: &str) -> Option<&'static str> {
    match name.strip_prefix("native::").unwrap_or(name) {
        "read" => Some("Read files"),
        "grep" | "search" => Some("Search code"),
        "glob" | "list" => Some("List files"),
        "edit" | "write" => Some("Edit files"),
        "bash" => Some("Run command"),
        "skill" => Some("Load skill"),
        _ => None,
    }
}
fn push_text_item(items: &mut Vec<ConversationItem>, text: String, reasoning: bool) {
    match (items.last_mut(), reasoning) {
        (Some(ConversationItem::Reasoning(current)), true)
        | (Some(ConversationItem::Assistant(current)), false) => current.push_str(&text),
        (_, true) => items.push(ConversationItem::Reasoning(text)),
        (_, false) => items.push(ConversationItem::Assistant(text)),
    }
}

fn sanitize_error_message(message: String) -> String {
    let value = message.to_ascii_lowercase();
    let sensitive_markers = [
        "api_key",
        "authorization",
        "password",
        "secret",
        "token",
        "path:",
        "prompt:",
    ];

    if sensitive_markers
        .iter()
        .any(|marker| value.contains(marker))
    {
        "[redacted]".into()
    } else {
        message
    }
}
