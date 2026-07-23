//! Conversation block presentation builders (thinking, tool rows).

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::{ExpandMode, ExpandableBody, RolePalette};

/// Thinking header + optional body driven by shared expand modes.
pub(crate) struct ThinkingBlock;

impl ThinkingBlock {
    /// Resolve presentation mode for a reasoning block.
    pub(crate) const fn mode(streaming: bool, collapsed: bool) -> ExpandMode {
        if streaming {
            ExpandMode::begin_stream()
        } else if collapsed {
            ExpandMode::Collapsed
        } else {
            ExpandMode::Expanded
        }
    }

    /// Title line for thinking chrome.
    pub(crate) fn title(mode: ExpandMode) -> Line<'static> {
        let body = ExpandableBody::new(mode);
        let title = if body.is_visible() {
            "Thinking"
        } else {
            "Thinking · collapsed"
        };
        Line::from(Span::styled(
            title,
            Style::default()
                .fg(RolePalette::thinking())
                .add_modifier(Modifier::BOLD),
        ))
    }
}

/// Primary tool row: name + args always; call_id stays off the scan path.
pub(crate) struct ToolRow;

impl ToolRow {
    /// Header line showing tool name only (no call_id).
    pub(crate) fn header(name: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled("┌ ", Style::default().fg(RolePalette::tool())),
            Span::styled(
                name.to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ])
    }

    /// Arguments line (always visible for audit).
    pub(crate) fn args(input: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled("│ input ", Style::default().fg(RolePalette::muted())),
            Span::raw(input.to_owned()),
        ])
    }

    /// Result footer without call_id as the primary label.
    pub(crate) fn result_footer(status: &str, color: ratatui::style::Color) -> Line<'static> {
        Line::from(vec![
            Span::styled("└ ", Style::default().fg(color)),
            Span::styled(
                status.to_owned(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ])
    }

    /// Collapsed output placeholder.
    pub(crate) fn collapsed_output() -> Line<'static> {
        Line::from(Span::styled(
            "output collapsed; expand to recover",
            Style::default().fg(RolePalette::chrome()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_mode_streams_then_collapses_unless_expanded() {
        assert_eq!(
            ThinkingBlock::mode(true, true),
            ExpandMode::Streaming
        );
        assert_eq!(
            ThinkingBlock::mode(false, true),
            ExpandMode::Collapsed
        );
        assert_eq!(
            ThinkingBlock::mode(false, false),
            ExpandMode::Expanded
        );
    }

    #[test]
    fn tool_row_header_is_name_only_without_call_id() {
        let line = ToolRow::header("native::read");
        let text: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "┌ native::read");
        assert!(!text.contains("call"));
        assert!(!text.contains("read-1"));
    }

    #[test]
    fn tool_row_args_and_collapsed_marker_are_stable() {
        let args = ToolRow::args("src/lib.rs");
        let args_text: String = args.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(args_text.contains("input"));
        assert!(args_text.contains("src/lib.rs"));

        let collapsed = ToolRow::collapsed_output();
        let collapsed_text: String = collapsed
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(collapsed_text.contains("output collapsed"));
    }
}
