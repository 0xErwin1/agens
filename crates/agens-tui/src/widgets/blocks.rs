//! Conversation block presentation builders (thinking, tool rows).

use agens_core::ToolInput;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    /// Arguments line (always visible for audit).
    pub(crate) fn args(input: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled("│ input ", Style::default().fg(RolePalette::muted())),
            Span::raw(input.to_owned()),
        ])
    }

    /// Result footer with tool name, status, and optional result-size metadata
    /// (no call_id on the scan path).
    ///
    /// `size` carries the computed result size (lines/bytes) as muted trailing
    /// metadata; it is never a fabricated per-call token count.
    pub(crate) fn result_footer(
        tool_name: &str,
        status: &str,
        color: ratatui::style::Color,
        size: Option<&str>,
    ) -> Line<'static> {
        let mut spans = vec![
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
        ];
        if let Some(size) = size {
            spans.push(Span::styled(
                format!(" · {size}"),
                Style::default().fg(RolePalette::muted()),
            ));
        }
        Line::from(spans)
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

/// Typed verb + operand header for a tool call.
///
/// Renders `verb operand [suffix]` with a bold verb, the operand in the path
/// accent, and muted trailing metadata. Bash renders as a `$ command` shell
/// prompt. Unknown and MCP tools fall back to the short tool name plus a
/// single-line, whitespace-collapsed summary of their arguments — never a raw
/// JSON dump. The operand is truncated with an ellipsis to fit `content_width`
/// display columns so narrow viewports never wrap or panic.
pub(crate) fn tool_header(parsed: &ToolInput, content_width: usize) -> Line<'static> {
    let parts = header_parts(parsed);

    let verb_style = if parts.shell {
        Style::default().fg(RolePalette::muted())
    } else {
        Style::default()
            .fg(RolePalette::tool())
            .add_modifier(Modifier::BOLD)
    };

    let verb_width = if parts.verb.is_empty() {
        0
    } else {
        parts.verb.width() + 1
    };
    let fixed_width = verb_width + parts.suffix.as_ref().map_or(0, |suffix| suffix.width() + 1);
    let operand_budget = content_width.saturating_sub(fixed_width).max(1);
    let operand = truncate_operand(&parts.operand, operand_budget);

    let mut spans = Vec::new();
    if !parts.verb.is_empty() {
        spans.push(Span::styled(parts.verb.to_owned(), verb_style));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        operand,
        Style::default().fg(RolePalette::path()),
    ));
    if let Some(suffix) = parts.suffix {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            suffix,
            Style::default().fg(RolePalette::muted()),
        ));
    }
    Line::from(spans)
}

/// Whether consecutive collapsed calls of this kind may fold into a verb-group.
///
/// Read-family and write calls fold; bash and edit never fold eagerly (their
/// side effects deserve individual rows), and unknown/MCP tools stay separate.
pub(crate) fn tool_input_groupable(parsed: &ToolInput) -> bool {
    matches!(
        parsed,
        ToolInput::Read { .. }
            | ToolInput::List { .. }
            | ToolInput::Search { .. }
            | ToolInput::Write { .. }
            | ToolInput::Glob { .. }
            | ToolInput::Grep { .. }
    )
}

struct HeaderParts {
    verb: &'static str,
    operand: String,
    suffix: Option<String>,
    shell: bool,
}

fn header_parts(parsed: &ToolInput) -> HeaderParts {
    let plain = |verb: &'static str, operand: String| HeaderParts {
        verb,
        operand,
        suffix: None,
        shell: false,
    };

    match parsed {
        ToolInput::Read { path } => plain("Read", path.clone()),
        ToolInput::Write { path } => plain("Write", path.clone()),
        ToolInput::Edit { path } => plain("Edit", path.clone()),
        ToolInput::List { path } => plain("List", path.clone()),
        ToolInput::Search { path } => plain("Search", path.clone()),
        ToolInput::Glob { pattern, .. } => plain("Glob", pattern.clone()),
        ToolInput::Grep { pattern, path } => HeaderParts {
            verb: "Grep",
            operand: pattern.clone(),
            suffix: path.as_ref().map(|path| format!("in {path}")),
            shell: false,
        },
        ToolInput::Bash { command } => HeaderParts {
            verb: "$",
            operand: summarize_args(command),
            suffix: None,
            shell: true,
        },
        ToolInput::WebFetch { url } => plain("Fetch", url.clone()),
        ToolInput::Skill { skill } => plain("Skill", skill.clone()),
        ToolInput::Other { name, raw } => HeaderParts {
            verb: "",
            operand: format!("{} {}", short_tool_name(name), summarize_args(raw))
                .trim()
                .to_owned(),
            suffix: None,
            shell: false,
        },
    }
}

/// Collapse any whitespace run (including newlines) to a single space so a
/// multi-line or JSON argument payload becomes a compact one-line summary.
fn summarize_args(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_operand(operand: &str, budget: usize) -> String {
    if operand.width() <= budget {
        return operand.to_owned();
    }
    if budget == 0 {
        return String::new();
    }

    let mut clipped = String::new();
    let mut width = 0usize;
    let target = budget.saturating_sub(1);
    for character in operand.chars() {
        let character_width = character.width().unwrap_or(0);
        if width + character_width > target {
            break;
        }
        clipped.push(character);
        width += character_width;
    }
    clipped.push('…');
    clipped
}

/// Tool call header/args, wrapped as `BlockContent` with a typed per-tool header.
///
/// The header never renders raw JSON; the complete raw `input` is exposed only
/// in [`DisplayMode::Expanded`] so the audit view stays reachable without
/// leaking a JSON payload into the always-visible header.
pub(crate) struct ToolCallBlock<'a> {
    pub(crate) input: &'a str,
    pub(crate) parsed: &'a ToolInput,
    pub(crate) batch: Option<usize>,
    pub(crate) content_width: usize,
}

impl BlockContent for ToolCallBlock<'_> {
    fn lines(&self, mode: DisplayMode) -> Vec<BlockLine> {
        let mut lines = Vec::new();
        if let Some(batch) = self.batch {
            lines.push(BlockLine::new(Line::from(Span::styled(
                format!("Tools · batch {batch}"),
                Style::default()
                    .fg(RolePalette::tool())
                    .add_modifier(Modifier::BOLD),
            ))));
        }
        lines.push(BlockLine::new(tool_header(self.parsed, self.content_width)));
        if mode == DisplayMode::Expanded {
            lines.push(BlockLine::new(ToolRow::args(self.input)));
        }
        lines
    }

    fn default_mode(&self) -> DisplayMode {
        DisplayMode::Truncated
    }

    fn accent(&self) -> Color {
        RolePalette::tool()
    }

    fn is_groupable(&self) -> bool {
        tool_input_groupable(self.parsed)
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

    fn header_of(parsed: &ToolInput) -> String {
        line_text(&tool_header(parsed, 80))
    }

    #[test]
    fn typed_headers_render_verb_and_operand_per_kind() {
        assert_eq!(
            header_of(&ToolInput::Read {
                path: "src/main.rs".into()
            }),
            "Read src/main.rs"
        );
        assert_eq!(
            header_of(&ToolInput::Write {
                path: "out.rs".into()
            }),
            "Write out.rs"
        );
        assert_eq!(
            header_of(&ToolInput::Edit {
                path: "lib.rs".into()
            }),
            "Edit lib.rs"
        );
        assert_eq!(
            header_of(&ToolInput::List {
                path: "crates".into()
            }),
            "List crates"
        );
        assert_eq!(
            header_of(&ToolInput::Search {
                path: "docs".into()
            }),
            "Search docs"
        );
        assert_eq!(
            header_of(&ToolInput::Glob {
                pattern: "**/*.rs".into(),
                path: Some("crates".into()),
            }),
            "Glob **/*.rs"
        );
        assert_eq!(
            header_of(&ToolInput::Grep {
                pattern: "needle".into(),
                path: Some("src".into()),
            }),
            "Grep needle in src"
        );
        assert_eq!(
            header_of(&ToolInput::Grep {
                pattern: "needle".into(),
                path: None,
            }),
            "Grep needle"
        );
        assert_eq!(
            header_of(&ToolInput::Bash {
                command: "cargo test".into()
            }),
            "$ cargo test"
        );
        assert_eq!(
            header_of(&ToolInput::WebFetch {
                url: "https://x.dev".into()
            }),
            "Fetch https://x.dev"
        );
        assert_eq!(
            header_of(&ToolInput::Skill {
                skill: "deploy".into()
            }),
            "Skill deploy"
        );
    }

    #[test]
    fn unknown_and_mcp_headers_fall_back_to_name_and_summarized_args() {
        assert_eq!(
            header_of(&ToolInput::Other {
                name: "mcp::foo__bar".into(),
                raw: "{\"a\":1}".into(),
            }),
            "foo__bar {\"a\":1}"
        );
        // Multi-line raw args collapse to a single summarized line (no JSON dump).
        let collapsed = header_of(&ToolInput::Other {
            name: "native::custom".into(),
            raw: "line one\n  line two".into(),
        });
        assert_eq!(collapsed, "custom line one line two");
        assert!(!collapsed.contains('\n'));
    }

    #[test]
    fn narrow_headers_truncate_the_operand_without_panicking() {
        let parsed = ToolInput::Read {
            path: "a/very/long/nested/path/to/file.rs".into(),
        };
        for width in [1usize, 2, 6, 12] {
            let header = line_text(&tool_header(&parsed, width));
            // The verb always stays; only the operand truncates, so the floor
            // is `"Read …"` (width 6) even at a one-column budget.
            assert!(
                UnicodeWidthStr::width(header.as_str()) <= width.max(6),
                "width {width}: {header:?}"
            );
            assert!(header.starts_with("Read "), "width {width}: {header:?}");
        }
        let unicode = ToolInput::Read {
            path: "café/über/naïve.rs".into(),
        };
        let _ = tool_header(&unicode, 1);
    }

    #[test]
    fn fold_policy_groups_read_family_but_never_bash_or_edit() {
        for groupable in [
            ToolInput::Read { path: "a".into() },
            ToolInput::List { path: "a".into() },
            ToolInput::Search { path: "a".into() },
            ToolInput::Write { path: "a".into() },
            ToolInput::Glob {
                pattern: "*".into(),
                path: None,
            },
            ToolInput::Grep {
                pattern: "*".into(),
                path: None,
            },
        ] {
            assert!(tool_input_groupable(&groupable), "{groupable:?}");
        }
        for ungroupable in [
            ToolInput::Bash {
                command: "ls".into(),
            },
            ToolInput::Edit { path: "a".into() },
            ToolInput::Other {
                name: "x".into(),
                raw: "y".into(),
            },
        ] {
            assert!(!tool_input_groupable(&ungroupable), "{ungroupable:?}");
        }
    }

    #[test]
    fn tool_call_block_defaults_truncated_and_reveals_raw_args_only_when_expanded() {
        let parsed = ToolInput::Read {
            path: "src/lib.rs".into(),
        };
        let block = ToolCallBlock {
            input: "{\"path\":\"src/lib.rs\"}",
            parsed: &parsed,
            batch: Some(2),
            content_width: 80,
        };
        assert_eq!(block.default_mode(), DisplayMode::Truncated);
        assert!(block.is_groupable());

        let truncated = block.lines(DisplayMode::Truncated);
        assert_eq!(truncated.len(), 2);
        assert!(line_text(&truncated[0].line).contains("batch 2"));
        assert_eq!(line_text(&truncated[1].line), "Read src/lib.rs");
        assert!(
            truncated
                .iter()
                .all(|row| !line_text(&row.line).contains("{\"path\"")),
            "raw JSON must not appear in the collapsed/truncated header"
        );

        let expanded = block.lines(DisplayMode::Expanded);
        assert!(
            expanded
                .iter()
                .any(|row| line_text(&row.line).contains("{\"path\":\"src/lib.rs\"}")),
            "expanded header must expose the full raw args for audit"
        );
    }

    #[test]
    fn tool_result_block_selects_body_by_display_mode_and_shows_result_size() {
        let footer = ToolRow::result_footer(
            "read",
            "Success",
            RolePalette::success(),
            Some("2 lines · 21 B"),
        );
        assert!(line_text(&footer).contains("2 lines · 21 B"));

        let block = ToolResultBlock {
            footer,
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

    #[test]
    fn collapsed_failure_keeps_a_visible_reason_line() {
        let footer = ToolRow::result_footer("bash", "Failure", RolePalette::error(), None);
        let block = ToolResultBlock {
            footer,
            collapsed_body: vec![ToolRow::collapsed_failure(
                "exit 1: command not found\ntrace",
            )],
            full_body: vec![Line::from("full stderr")],
            accent: RolePalette::error(),
        };
        let collapsed = block.lines(DisplayMode::Collapsed);
        let reason = line_text(&collapsed[1].line);
        assert!(reason.contains("exit 1: command not found"), "{reason:?}");
        assert!(
            !reason.contains("trace"),
            "only the first reason line shows"
        );
    }
}
