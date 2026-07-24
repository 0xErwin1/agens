//! Conversation block presentation builders (thinking, tool rows).

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::{DisplayMode, ExpandMode, ExpandableBody, RolePalette};

/// One presentation row within a [`BlockContent`], carrying optional row
/// background (diff insert/delete highlighting) and whether it may be
/// folded into a verb-group summary.
pub(crate) struct BlockLine {
    pub(crate) line: Line<'static>,
    // Consumed by the S2 diff painter and S3 verb-group pass; not yet read
    // by the S1 foundation this batch lands.
    #[allow(dead_code)]
    pub(crate) background: Option<Color>,
    #[allow(dead_code)]
    pub(crate) groupable: bool,
}

impl BlockLine {
    /// Plain row with no background and default groupability.
    pub(crate) fn new(line: Line<'static>) -> Self {
        Self {
            line,
            background: None,
            groupable: true,
        }
    }
}

/// Viewport-agnostic description of a conversation block's presentation.
///
/// Implementors describe *what* a block shows for a given [`DisplayMode`];
/// painting (gutters, padding, row backgrounds) happens in `render.rs`.
pub(crate) trait BlockContent {
    /// Rows to paint for the given display mode.
    fn lines(&self, mode: DisplayMode) -> Vec<BlockLine>;

    /// Mode used when no explicit mode is recorded for this block.
    fn default_mode(&self) -> DisplayMode {
        DisplayMode::Collapsed
    }

    /// Mode while the block's source content is still streaming.
    ///
    /// Not yet driving any per-tool policy branch; wired by S2's per-tool
    /// default-mode table (T19).
    #[allow(dead_code)]
    fn mode_while_streaming(&self) -> DisplayMode {
        DisplayMode::Truncated
    }

    /// Mode applied automatically once the block finishes.
    ///
    /// S1 achieves this policy implicitly (absent map entry = `default_mode`
    /// = Collapsed); wired explicitly once S2 differentiates per-tool policy.
    #[allow(dead_code)]
    fn mode_on_finish(&self) -> DisplayMode {
        DisplayMode::Collapsed
    }

    /// Next mode in the Ctrl+O cycle from `current`.
    #[allow(dead_code)]
    fn next_mode(&self, current: DisplayMode) -> DisplayMode {
        current.next()
    }

    /// Accent color for the block's gutter/bullet.
    ///
    /// Not yet painted by `render.rs`; wired by S2's gutter painting (T18).
    #[allow(dead_code)]
    fn accent(&self) -> Color;

    /// Whether consecutive collapsed instances of this block may fold into
    /// a verb-group summary row.
    ///
    /// Not yet consumed; wired by S3's verb-group folding pass (T24).
    #[allow(dead_code)]
    fn is_groupable(&self) -> bool {
        true
    }
}

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

/// Tool call header/args, wrapped as `BlockContent` reproducing today's output exactly.
pub(crate) struct ToolCallBlock<'a> {
    pub(crate) name: &'a str,
    pub(crate) input: &'a str,
    pub(crate) batch: Option<usize>,
}

impl BlockContent for ToolCallBlock<'_> {
    fn lines(&self, _mode: DisplayMode) -> Vec<BlockLine> {
        let mut lines = Vec::new();
        if let Some(batch) = self.batch {
            lines.push(BlockLine::new(Line::from(Span::styled(
                format!("Tools · batch {batch}"),
                Style::default()
                    .fg(RolePalette::tool())
                    .add_modifier(Modifier::BOLD),
            ))));
        }
        lines.push(BlockLine::new(ToolRow::header(self.name)));
        lines.push(BlockLine::new(ToolRow::args(self.input)));
        lines
    }

    fn accent(&self) -> Color {
        RolePalette::tool()
    }
}

/// Tool result footer/body, wrapped as `BlockContent` reproducing today's output exactly.
///
/// `collapsed_body` and `full_body` are pre-rendered by the caller (which owns
/// content width and markdown/syntax wrapping); this block only selects
/// between them by [`DisplayMode`].
pub(crate) struct ToolResultBlock {
    pub(crate) footer: Line<'static>,
    pub(crate) collapsed_body: Vec<Line<'static>>,
    pub(crate) full_body: Vec<Line<'static>>,
    #[allow(dead_code)]
    pub(crate) accent: Color,
}

impl BlockContent for ToolResultBlock {
    fn lines(&self, mode: DisplayMode) -> Vec<BlockLine> {
        let mut lines = vec![BlockLine::new(self.footer.clone())];
        let body = match mode {
            DisplayMode::Collapsed => &self.collapsed_body,
            DisplayMode::Truncated | DisplayMode::Expanded => &self.full_body,
        };
        lines.extend(body.iter().cloned().map(BlockLine::new));
        lines
    }

    // A finished call always gets an explicit `Collapsed` entry recorded at
    // completion (see `Tui::apply_conversation_event`); this fallback only
    // applies to calls whose entry was cleared by a new submission, which
    // must keep showing their retained output (never re-collapse silently).
    fn default_mode(&self) -> DisplayMode {
        DisplayMode::Expanded
    }

    fn mode_on_finish(&self) -> DisplayMode {
        DisplayMode::Collapsed
    }

    fn accent(&self) -> Color {
        self.accent
    }

    fn is_groupable(&self) -> bool {
        false
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

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn tool_call_block_reproduces_header_args_and_batch_marker() {
        let block = ToolCallBlock {
            name: "native::read",
            input: "src/lib.rs",
            batch: Some(2),
        };
        let lines = block.lines(DisplayMode::Collapsed);
        assert_eq!(lines.len(), 3);
        assert!(line_text(&lines[0].line).contains("batch 2"));
        assert_eq!(line_text(&lines[1].line), "┌ read");
        assert!(line_text(&lines[2].line).contains("src/lib.rs"));
        assert!(block.is_groupable());
    }

    #[test]
    fn tool_result_block_selects_body_by_display_mode() {
        let block = ToolResultBlock {
            footer: ToolRow::result_footer("read", "Success", RolePalette::success()),
            collapsed_body: vec![ToolRow::collapsed_output()],
            full_body: vec![Line::from("full output")],
            accent: RolePalette::tool(),
        };
        assert_eq!(block.mode_on_finish(), DisplayMode::Collapsed);
        assert!(!block.is_groupable());

        let collapsed = block.lines(DisplayMode::Collapsed);
        assert_eq!(collapsed.len(), 2);
        assert!(line_text(&collapsed[1].line).contains("output collapsed"));

        let truncated = block.lines(DisplayMode::Truncated);
        assert_eq!(line_text(&truncated[1].line), "full output");

        let expanded = block.lines(DisplayMode::Expanded);
        assert_eq!(line_text(&expanded[1].line), "full output");
    }
}
