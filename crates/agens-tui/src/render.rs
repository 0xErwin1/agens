//! Rich presentation of typed runtime details without mutating their source data.

use std::time::Duration;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::{DiffLineKind, ToolResultState, TuiRuntimeEvent};

pub(super) fn detail_lines(events: &[TuiRuntimeEvent]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    for event in events {
        match event {
            TuiRuntimeEvent::TurnStarted => line(&mut lines, "TURN", Color::Cyan, "started"),
            TuiRuntimeEvent::TurnEnded { status, duration } => line(
                &mut lines,
                "TURN",
                turn_color(*status),
                format!("{status:?}{}", duration_label(*duration)),
            ),
            TuiRuntimeEvent::Usage(usage) => line(
                &mut lines,
                "USAGE",
                Color::Gray,
                format!(
                    "input {} · output {} · total {} · context {}",
                    optional_number(usage.input_tokens),
                    optional_number(usage.output_tokens),
                    optional_number(usage.total_tokens),
                    optional_number(usage.context_window),
                ),
            ),
            TuiRuntimeEvent::ToolStarted {
                call_id,
                name,
                input,
            } => line(
                &mut lines,
                "TOOLS",
                Color::Magenta,
                format!("┌ {call_id} {name}\n  input: {input}"),
            ),
            TuiRuntimeEvent::ToolEnded {
                call_id,
                duration,
                result,
            } => line(
                &mut lines,
                "TOOLS",
                result_color(*result),
                format!("└ {call_id} {result:?}{}", duration_label(*duration)),
            ),
            TuiRuntimeEvent::Diff {
                call_id,
                lines: diff,
            } => {
                line(&mut lines, "DIFF", Color::Yellow, format!("{call_id}:"));
                for change in diff {
                    let (marker, color) = match change.kind {
                        DiffLineKind::Added => ('+', Color::Green),
                        DiffLineKind::Removed => ('-', Color::Red),
                        DiffLineKind::Context => (' ', Color::Gray),
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {:>4} {marker} ", change.number),
                            Style::default().fg(color),
                        ),
                        Span::raw(change.text.clone()),
                    ]));
                }
            }
        }
    }

    lines
}

fn line(lines: &mut Vec<Line<'static>>, label: &str, color: Color, text: impl Into<String>) {
    lines.push(Line::from(vec![
        Span::styled(
            format!("  │ {label:<9} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(text.into()),
    ]));
    lines.push(Line::default());
}

fn optional_number(value: Option<u64>) -> String {
    value.map_or_else(|| "unavailable".into(), |number| number.to_string())
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

fn turn_color(state: agens_core::TurnState) -> Color {
    match state {
        agens_core::TurnState::Completed => Color::Green,
        agens_core::TurnState::Failed => Color::Red,
        agens_core::TurnState::Cancelled => Color::Yellow,
        _ => Color::Cyan,
    }
}
