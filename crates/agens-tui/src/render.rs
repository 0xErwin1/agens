//! Rich presentation of typed runtime details without mutating their source data.

use std::time::Duration;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use std::collections::BTreeSet;

use crate::conversation::ConversationItem;
use crate::{Conversation, DiffLineKind, ToolResultState, TuiRuntimeEvent};

pub(super) fn conversation_lines(
    conversation: &Conversation,
    events: &[TuiRuntimeEvent],
    collapsed_tool_outputs: &BTreeSet<String>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    for item in &conversation.items {
        match item {
            ConversationItem::Info(text) => line(&mut lines, "INFO", Color::Yellow, text),
            ConversationItem::User(text) => line(&mut lines, "USER", Color::Green, text),
            ConversationItem::Assistant(text) => {
                markdown_lines(&mut lines, "ASSISTANT", text);
            }
            ConversationItem::Reasoning(text) => markdown_lines(&mut lines, "THINKING", text),
            ConversationItem::ToolCall {
                call_id,
                name,
                input,
                batch,
            } => {
                if let Some(batch) = batch {
                    line(
                        &mut lines,
                        "TOOLS",
                        Color::Magenta,
                        format!("batch {batch}"),
                    );
                }
                line(
                    &mut lines,
                    "TOOLS",
                    Color::Magenta,
                    format!("{call_id} {name}\n  input: {input}"),
                );
            }
            ConversationItem::ToolResult {
                call_id,
                output,
                is_error,
            } => {
                let (result_state, duration) = tool_state(events, call_id, *is_error);
                line(
                    &mut lines,
                    "TOOLS",
                    result_color(result_state),
                    format!("{call_id} {result_state:?}{}", duration_label(duration)),
                );
                if collapsed_tool_outputs.contains(call_id) {
                    line(
                        &mut lines,
                        "TOOLS",
                        Color::Gray,
                        "output collapsed; expand to recover",
                    );
                } else {
                    markdown_lines(&mut lines, "OUTPUT", output);
                }
            }
            ConversationItem::Diff(diff) => {
                for change in diff {
                    diff_line(&mut lines, change.number, change.kind, &change.text);
                }
            }
            ConversationItem::Error(error) => {
                line(&mut lines, "ERROR", Color::Red, &error.message);
                line(
                    &mut lines,
                    "ACTION",
                    Color::Yellow,
                    format!("Action: {}", error.action),
                );
            }
        }
    }
    lines
}

pub(super) fn detail_lines(
    events: &[TuiRuntimeEvent],
    conversation_is_authoritative: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    for event in events {
        match event {
            TuiRuntimeEvent::ToolStarted {
                call_id,
                name,
                input,
            } if !conversation_is_authoritative => line(
                &mut lines,
                "TOOLS",
                Color::Magenta,
                format!("┌ {call_id} {name}\n  input: {input}"),
            ),
            TuiRuntimeEvent::ToolEnded {
                call_id,
                duration,
                result,
            } if !conversation_is_authoritative => line(
                &mut lines,
                "TOOLS",
                result_color(*result),
                format!("└ {call_id} {result:?}{}", duration_label(*duration)),
            ),
            TuiRuntimeEvent::Diff {
                call_id,
                lines: diff,
            } if !conversation_is_authoritative => {
                line(&mut lines, "DIFF", Color::Yellow, format!("{call_id}:"));
                for change in diff {
                    diff_line(&mut lines, change.number, change.kind, &change.text);
                }
            }
            TuiRuntimeEvent::TurnStarted
            | TuiRuntimeEvent::TurnEnded { .. }
            | TuiRuntimeEvent::Usage(_)
            | TuiRuntimeEvent::ToolStarted { .. }
            | TuiRuntimeEvent::ToolEnded { .. }
            | TuiRuntimeEvent::Diff { .. } => {}
        }
    }

    lines
}

fn markdown_lines(lines: &mut Vec<Line<'static>>, label: &str, markdown: &str) {
    if !markdown.is_empty() {
        line(lines, label, Color::Cyan, markdown);
    }
}

fn diff_line(lines: &mut Vec<Line<'static>>, number: u32, kind: DiffLineKind, text: &str) {
    let (marker, color) = match kind {
        DiffLineKind::Added => ('+', Color::Green),
        DiffLineKind::Removed => ('-', Color::Red),
        DiffLineKind::Context => (' ', Color::Gray),
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {:>4} {marker} ", number),
            Style::default().fg(color),
        ),
        Span::raw(text.to_owned()),
    ]));
}

fn tool_state(
    events: &[TuiRuntimeEvent],
    call_id: &str,
    is_error: bool,
) -> (ToolResultState, Option<Duration>) {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            TuiRuntimeEvent::ToolEnded {
                call_id: event_call_id,
                duration,
                result,
            } if event_call_id == call_id => Some((*result, *duration)),
            _ => None,
        })
        .unwrap_or((
            if is_error {
                ToolResultState::Failure
            } else {
                ToolResultState::Success
            },
            None,
        ))
}

fn line(lines: &mut Vec<Line<'static>>, label: &str, color: Color, text: impl Into<String>) {
    for text_line in text.into().split('\n') {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  │ {label:<9} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(text_line.to_owned()),
        ]));
    }
    lines.push(Line::default());
}

fn duration_label(duration: Option<Duration>) -> String {
    duration.map_or_else(String::new, |value| {
        if value.as_secs() > 0 {
            format!(" · {}s", value.as_secs())
        } else {
            format!(" · {}ms", value.as_millis())
        }
    })
}

fn result_color(result: ToolResultState) -> Color {
    match result {
        ToolResultState::Success => Color::Green,
        ToolResultState::Failure => Color::Red,
    }
}
