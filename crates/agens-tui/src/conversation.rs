//! Typed, lossless source projection for one visible conversation turn.

use crate::{TuiExecutionState, TuiSubagentEvent, TuiSubagentTerminal, bridge::TuiSubagentUpdate};
use agens_core::{Message, MessagePart, Role};

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
pub struct DiffLine {
    pub number: u32,
    pub kind: DiffLineKind,
    pub text: String,
}

impl DiffLine {
    pub fn new(number: u32, kind: DiffLineKind, text: impl Into<String>) -> Self {
        Self {
            number,
            kind,
            text: text.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Added,
    Removed,
    Context,
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
    pub terminal: Option<TuiSubagentTerminal>,
    pub tool_calls: Vec<ToolCall>,
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
    pub fn from_messages(messages: &[Message]) -> Result<Vec<Self>, ConversationError> {
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
                        result: None,
                    });
                self.items.push(ConversationItem::ToolCall {
                    call_id,
                    name,
                    input,
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
    pub(crate) fn apply_subagent(&mut self, event: TuiSubagentEvent) {
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
                    terminal: None,
                    tool_calls: Vec::new(),
                });
                self.items.push(ConversationItem::SubagentCard(event.id));
            }
            TuiSubagentUpdate::ToolCall {
                call_id,
                name,
                input,
            } => {
                if let Some(card) = self
                    .subagent_cards
                    .iter_mut()
                    .find(|card| card.id == event.id && card.terminal.is_none())
                    && card.tool_calls.iter().all(|call| call.call_id != call_id)
                {
                    card.tool_calls.push(ToolCall {
                        call_id,
                        name,
                        input,
                        result: None,
                    });
                }
            }
            TuiSubagentUpdate::ToolResult {
                call_id,
                output,
                is_error,
            } => {
                if let Some(call) = self
                    .subagent_cards
                    .iter_mut()
                    .find(|card| card.id == event.id && card.terminal.is_none())
                    .and_then(|card| {
                        card.tool_calls
                            .iter_mut()
                            .find(|call| call.call_id == call_id && call.result.is_none())
                    })
                {
                    call.result = Some(ToolResult { output, is_error });
                }
            }
            TuiSubagentUpdate::Terminal(terminal) => {
                if let Some(card) = self
                    .subagent_cards
                    .iter_mut()
                    .find(|card| card.id == event.id && card.terminal.is_none())
                {
                    card.terminal = Some(terminal);
                }
            }
            _ => {}
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
