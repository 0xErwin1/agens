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
    /// Header line showing short tool name only (no call_id).
    pub(crate) fn header(name: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled("┌ ", Style::default().fg(RolePalette::tool())),
            Span::styled(
                short_tool_name(name),
                Style::default()
                    .fg(RolePalette::tool())
                    .add_modifier(Modifier::BOLD),
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

    /// Result footer with tool name and status (no call_id on the scan path).
    pub(crate) fn result_footer(
        tool_name: &str,
        status: &str,
        color: ratatui::style::Color,
    ) -> Line<'static> {
        Line::from(vec![
            Span::styled("└ ", Style::default().fg(color)),
            Span::styled(
                short_tool_name(tool_name),
                Style::default()
                    .fg(RolePalette::tool())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(RolePalette::muted())),
            Span::styled(
                status.to_owned(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ])
    }

    /// Collapsed successful output placeholder.
    pub(crate) fn collapsed_output() -> Line<'static> {
        Line::from(Span::styled(
            "  output collapsed · Ctrl+O to expand",
            Style::default().fg(RolePalette::chrome()),
        ))
    }

    /// Collapsed failure always keeps a short reason visible.
    pub(crate) fn collapsed_failure(preview: &str) -> Line<'static> {
        let preview = preview.trim();
        let preview = if preview.is_empty() {
            "failed (no details)"
        } else {
            preview
        };
        let preview = preview.lines().next().unwrap_or(preview);
        let mut preview = preview.chars().take(120).collect::<String>();
        if preview.chars().count() >= 120 {
            preview.push('…');
        }
        Line::from(vec![
            Span::styled("  reason ", Style::default().fg(RolePalette::muted())),
            Span::styled(preview, Style::default().fg(RolePalette::error())),
            Span::styled(
                " · Ctrl+O for full output",
                Style::default().fg(RolePalette::chrome()),
            ),
        ])
    }
}

fn short_tool_name(name: &str) -> String {
    name.strip_prefix("native::")
        .or_else(|| name.strip_prefix("mcp::"))
        .unwrap_or(name)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_mode_streams_then_collapses_unless_expanded() {
        assert_eq!(ThinkingBlock::mode(true, true), ExpandMode::Streaming);
        assert_eq!(ThinkingBlock::mode(false, true), ExpandMode::Collapsed);
        assert_eq!(ThinkingBlock::mode(false, false), ExpandMode::Expanded);
    }

    #[test]
    fn tool_row_header_is_name_only_without_call_id() {
        let line = ToolRow::header("native::read");
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "┌ read");
        assert!(!text.contains("call"));
        assert!(!text.contains("read-1"));
        assert!(!text.contains("native::"));
    }

    #[test]
    fn tool_row_args_and_collapsed_marker_are_stable() {
        let args = ToolRow::args("src/lib.rs");
        let args_text: String = args
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(args_text.contains("input"));
        assert!(args_text.contains("src/lib.rs"));

        let collapsed = ToolRow::collapsed_output();
        let collapsed_text: String = collapsed
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(collapsed_text.contains("output collapsed"));

        let failure = ToolRow::collapsed_failure("glob: deadline exceeded\nmore");
        let failure_text: String = failure
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(failure_text.contains("glob: deadline exceeded"));
        assert!(!failure_text.contains("more"));
        assert!(failure_text.contains("Ctrl+O"));
    }
}
