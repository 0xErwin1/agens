//! Conversation block presentation builders (thinking, tool rows).

use std::time::Duration;

use agens_core::ToolInput;
use agens_core::redaction::{is_credential_key, redact_credential_values};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{DisplayMode, ExpandMode, Glyph, RolePalette, StatusGlyph, UnicodeLevel};

/// Columns reserved by the shared transcript gutter: one bullet cell plus one
/// separator cell.
///
/// Every transcript row spends them, whether or not it carries a bullet, so a
/// row's content column never depends on its kind or lifecycle state.
pub(crate) const GUTTER_WIDTH: usize = 2;

/// Column every transcript row reserves for its accent bar, left of the gutter.
///
/// It is carved out of the transcript's existing chrome padding, so introducing
/// the bar moves neither the bullet nor the content column.
pub(crate) const ACCENT_WIDTH: usize = 1;

/// Left accent bar marking the rows of a live or consequential block.
///
/// A row without an accent leaves the column blank. Motion is colour-only, so
/// animated lifecycle bars never change a row's shape as ticks advance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RowAccent {
    /// Full bar whose brightness breathes down the block.
    Wave(Color),
    /// Full bar held at its own colour.
    Still(Color),
    /// Thin dimmed bar standing in for a collapsed groupable run.
    Collapsed(Color),
}

impl RowAccent {
    /// Brightness percentages the wave cycles through, in travel order.
    const WAVE_LEVELS: [u16; 4] = [100, 84, 68, 84];
    const COLLAPSED_LEVEL: u16 = 50;
    /// Spinner periods one wave frame holds.
    ///
    /// The bar rides the surface's single tick clock and only slows it down:
    /// three periods land near four frames per second, which reads as breathing
    /// instead of strobing.
    const WAVE_PERIODS: u128 = 3;

    /// Bar painted for the `row`-th line of its block under the tick clock.
    pub(crate) fn span(self, row: usize, now: Duration, unicode: UnicodeLevel) -> Span<'static> {
        let (glyph, color) = match self {
            Self::Wave(color) => (Glyph::AccentBar, scaled(color, Self::wave_level(row, now))),
            Self::Still(color) => (Glyph::AccentBar, color),
            Self::Collapsed(color) => (Glyph::ThinAccentBar, scaled(color, Self::COLLAPSED_LEVEL)),
        };
        Span::styled(glyph.text(unicode), Style::default().fg(color))
    }

    fn wave_level(row: usize, now: Duration) -> u16 {
        let frame = now.as_millis() / (StatusGlyph::FRAME_PERIOD_MS * Self::WAVE_PERIODS);
        let index = (frame as usize).wrapping_add(row) % Self::WAVE_LEVELS.len();
        Self::WAVE_LEVELS[index]
    }
}

/// `color` held at `percent` of its own brightness; a non-RGB color has no scale.
fn scaled(color: Color, percent: u16) -> Color {
    let scale = |channel: u8| {
        u8::try_from(u16::from(channel).saturating_mul(percent) / 100).unwrap_or(u8::MAX)
    };
    match color {
        Color::Rgb(red, green, blue) => Color::Rgb(scale(red), scale(green), scale(blue)),
        other => other,
    }
}

/// Lifecycle a transcript bullet encodes through its colour alone.
///
/// Shape stays constant as a row settles: only the colour moves, so a finished
/// row never shifts or changes glyph under the reader's eye.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RowState {
    Running,
    Success,
    Failure,
    /// Content the reader cannot see from this row (elided behind a count).
    Muted,
}

impl RowState {
    pub(crate) const fn color(self) -> Color {
        match self {
            Self::Running => RolePalette::running(),
            Self::Success => RolePalette::success(),
            Self::Failure => RolePalette::error(),
            Self::Muted => RolePalette::muted(),
        }
    }
}

/// Leading glyph vocabulary for transcript rows.
#[derive(Clone, Copy, Debug)]
pub(crate) enum RowBullet {
    /// A single action: one tool call, one step.
    Activity(RowState),
    /// A header standing in for several rows (verb run, elided remainder).
    Group(RowState),
    /// Fixed identity glyph (user prompt, subagent card).
    Identity(Glyph, Color),
}

impl RowBullet {
    pub(crate) fn span(self, unicode: UnicodeLevel) -> Span<'static> {
        let (glyph, color) = match self {
            Self::Activity(state) => (Glyph::ActivityBullet.text(unicode), state.color()),
            Self::Group(state) => (Glyph::GroupBullet.text(unicode), state.color()),
            Self::Identity(glyph, color) => (glyph.text(unicode), color),
        };
        Span::styled(
            format!("{glyph} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    }
}

/// One presentation row within a [`BlockContent`], carrying its optional gutter
/// bullet, optional accent bar, optional row background (diff insert/delete
/// highlighting) and whether it may be folded into a verb-group summary.
pub(crate) struct BlockLine {
    pub(crate) line: Line<'static>,
    pub(crate) bullet: Option<RowBullet>,
    pub(crate) accent: Option<RowAccent>,
    // Consumed by the S2 diff painter; not yet read by the row painter.
    #[allow(dead_code)]
    pub(crate) background: Option<Color>,
    #[allow(dead_code)]
    pub(crate) groupable: bool,
}

impl BlockLine {
    /// Plain row with no bullet, no background and default groupability.
    pub(crate) fn new(line: Line<'static>) -> Self {
        Self {
            line,
            bullet: None,
            accent: None,
            background: None,
            groupable: true,
        }
    }

    /// Row leading the shared gutter with `bullet`.
    pub(crate) fn with_bullet(line: Line<'static>, bullet: RowBullet) -> Self {
        Self {
            bullet: Some(bullet),
            ..Self::new(line)
        }
    }

    /// Row carrying `accent` in the transcript's accent column.
    pub(crate) fn accented(mut self, accent: Option<RowAccent>) -> Self {
        self.accent = accent;
        self
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
    ///
    /// The recorded mode is a reader override, so this answers what the block
    /// shows before anyone asked for anything else.
    fn default_mode(&self) -> DisplayMode {
        DisplayMode::Collapsed
    }

    /// Accent color for the block's gutter/bullet.
    ///
    /// Not painted yet: `render.rs` still uses per-role styles directly.
    #[allow(dead_code)]
    fn accent(&self) -> Color;

    /// Whether consecutive collapsed instances of this block may fold into
    /// a verb-group summary row.
    ///
    /// Not consumed yet: no folding pass reads it.
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

    /// Accent bar the reasoning rows carry.
    ///
    /// A visible body breathes while it streams and holds still once the reader
    /// pinned it open; a hidden thought is finished chrome and carries none. A
    /// pinned-open thought is neither live nor an outcome, so its bar stays grey
    /// rather than claiming a state colour of its own.
    pub(crate) const fn accent(mode: ExpandMode) -> Option<RowAccent> {
        match mode {
            ExpandMode::Streaming => Some(RowAccent::Wave(RowState::Running.color())),
            // Collapsed keeps the rail. Every other block sits on the gutter, and
            // a single row that does not reads as belonging to something else
            // rather than as the quiet member of the same list.
            ExpandMode::Expanded | ExpandMode::Collapsed => {
                Some(RowAccent::Still(RolePalette::muted()))
            }
        }
    }

    /// Header introducing a visible reasoning body.
    pub(crate) fn title() -> Line<'static> {
        Self::row("Thinking".to_owned())
    }

    /// Single row standing in for a hidden reasoning body.
    ///
    /// The row names the finished thought instead of pairing a header with a
    /// separate "collapsed" summary, so hidden reasoning costs exactly one row.
    /// `elapsed` is rendered only when the caller measured it: no reasoning
    /// timing exists in the projection today, so the bare form is what ships
    /// rather than a fabricated duration.
    ///
    /// The row carries no key hint: it is the quietest thing in the transcript
    /// and stays that way. Ctrl+T is documented where keys are documented.
    pub(crate) fn collapsed_title(
        summary: Option<&str>,
        elapsed: Option<Duration>,
    ) -> Line<'static> {
        let label = elapsed.map_or_else(
            || "Thought".to_owned(),
            |elapsed| format!("Thought for {}", thought_duration(elapsed)),
        );

        let Some(summary) = summary.map(str::trim).filter(|summary| !summary.is_empty()) else {
            return Self::row(label);
        };

        // The label is what the row is; the summary is what it was about. They
        // carry different weight so the eye can skip the first and read the
        // second down a column of collapsed thoughts.
        Line::from(vec![
            Span::styled(
                format!("{label} · "),
                Style::default()
                    .fg(RolePalette::muted())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                summary.to_owned(),
                Style::default().fg(RolePalette::muted()),
            ),
        ])
    }

    fn row(label: String) -> Line<'static> {
        Line::from(Span::styled(
            label,
            Style::default()
                .fg(RolePalette::muted())
                .add_modifier(Modifier::BOLD),
        ))
    }
}

/// Human-scale reasoning duration: under a minute keeps one decimal, from a
/// minute up the remainder drops to whole seconds behind the minute count.
fn thought_duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{}m{:.0}s", elapsed.as_secs() / 60, seconds % 60.0)
    }
}

/// Primary tool row: name + args always; call_id stays off the scan path.
pub(crate) struct ToolRow;

impl ToolRow {
    /// Guttered detail line under a tool header (never a raw JSON dump label).
    pub(crate) fn detail(text: impl Into<String>) -> Line<'static> {
        Line::from(vec![
            Span::styled("│ ", Style::default().fg(RolePalette::muted())),
            Span::styled(text.into(), Style::default().fg(RolePalette::machine())),
        ])
    }

    /// Lifecycle metadata appended to the call header so one logical tool use
    /// keeps one header while it moves from running to a terminal outcome.
    pub(crate) fn lifecycle_suffix(
        status: &str,
        failed: bool,
        size: Option<&str>,
    ) -> Vec<Span<'static>> {
        let status_color = if failed {
            RolePalette::error()
        } else if status == "Running…" {
            RolePalette::running()
        } else if status.starts_with("Success") {
            RolePalette::success()
        } else {
            RolePalette::muted()
        };
        let mut spans = vec![
            Span::styled(" · ", Style::default().fg(RolePalette::muted())),
            Span::styled(
                status.to_owned(),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(size) = size {
            spans.push(Span::styled(
                format!(" · {size}"),
                Style::default().fg(RolePalette::muted()),
            ));
        }
        spans
    }
}

/// Typed verb + operand header for a tool call.
///
/// Renders `verb operand [suffix]` with a bold verb, neutral operand, and muted
/// trailing metadata. Lifecycle colours are reserved for the bullet, accent bar,
/// and explicit status so commands cannot be mistaken for running work.
/// Bash renders as a `$ command` shell
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
            .fg(RolePalette::machine())
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
        Style::default().fg(RolePalette::machine()),
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
/// Only non-destructive read-family calls fold. Bash, edit and write mutate the
/// world, so each keeps an individually auditable row, and unknown/MCP tools
/// stay separate because their effect is unknown.
pub(crate) fn tool_input_groupable(parsed: &ToolInput) -> bool {
    matches!(
        parsed,
        ToolInput::Read { .. }
            | ToolInput::List { .. }
            | ToolInput::Search { .. }
            | ToolInput::Glob { .. }
            | ToolInput::Grep { .. }
    )
}

/// Verb vocabulary for a folded run of consecutive read-family tool calls.
///
/// Membership mirrors [`tool_input_groupable`] exactly: a kind that may fold is
/// a kind that has a group verb.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VerbGroup {
    Read,
    List,
    Search,
    Glob,
    Grep,
}

impl VerbGroup {
    pub(crate) const fn of(parsed: &ToolInput) -> Option<Self> {
        match parsed {
            ToolInput::Read { .. } => Some(Self::Read),
            ToolInput::List { .. } => Some(Self::List),
            ToolInput::Search { .. } => Some(Self::Search),
            ToolInput::Glob { .. } => Some(Self::Glob),
            ToolInput::Grep { .. } => Some(Self::Grep),
            ToolInput::Write { .. }
            | ToolInput::Edit { .. }
            | ToolInput::Bash { .. }
            | ToolInput::WebFetch { .. }
            | ToolInput::Skill { .. }
            | ToolInput::Other { .. } => None,
        }
    }

    /// Tense-aware summary for `count` folded calls.
    pub(crate) fn label(self, count: usize, running: bool) -> String {
        let (present, past, noun) = match self {
            Self::Read => ("Reading", "Read", "files"),
            Self::List => ("Listing", "Listed", "directories"),
            Self::Search => ("Searching", "Searched", "paths"),
            Self::Glob => ("Matching", "Matched", "patterns"),
            Self::Grep => ("Searching", "Searched", "patterns"),
        };
        if running {
            format!("{present} {count} {noun}…")
        } else {
            format!("{past} {count} {noun}")
        }
    }
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
            operand: collapse_whitespace(command),
            suffix: None,
            shell: true,
        },
        ToolInput::WebFetch { url } => plain("Fetch", url.clone()),
        ToolInput::Skill { skill } => plain("Skill", skill.clone()),
        ToolInput::Other { name, raw } => HeaderParts {
            verb: "",
            operand: format!("{} {}", short_tool_name(name), summarize_args(name, raw))
                .trim()
                .to_owned(),
            suffix: None,
            shell: false,
        },
    }
}

/// Summarize an unknown tool's arguments by what was actually asked for.
///
/// The key shape alone (`{board, limit, status}`) is the same for every call to
/// the same tool, so a batch of them reads as one row repeated: it says which
/// tool ran and nothing about what it was told to do. The values are what tell
/// two calls apart, so scalars are shown — each redacted, collapsed and
/// bounded — while nested objects and arrays stay shape-only, since those are
/// the payloads worth dumping nowhere. Expanded detail uses the same summary
/// rather than dumping raw JSON.
///
/// `ask_user` is special-cased: a nested `questions` array would otherwise
/// collapse to `questions=[n]`, which tells the reader nothing about what was
/// asked. The human summary carries the count and a truncated first prompt.
fn summarize_args(tool_name: &str, raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.starts_with('{') {
        return redact_credential_values(&collapse_whitespace(trimmed));
    }

    let short = short_tool_name(tool_name);
    if is_ask_user_tool_name(&short)
        && let Some(summary) = summarize_ask_user_args(trimmed)
    {
        return summary;
    }

    tool_name
        .strip_prefix("mcp::atlas_")
        .and_then(|_| safe_resource_summary(trimmed))
        .or_else(|| summarize_arguments_by_value(trimmed))
        .unwrap_or_else(|| format_key_shape(&object_keys(trimmed)))
}

/// Whether a short or full tool name refers to the structured ask-user tool.
pub(crate) fn is_ask_user_tool_name(name: &str) -> bool {
    let short = short_tool_name(name);
    short == "ask_user" || short.ends_with("::ask_user") || short.ends_with("_ask_user")
}

/// `{1 question · …}` / `{3 questions · …}` from an ask_user input payload.
fn summarize_ask_user_args(raw: &str) -> Option<String> {
    let Value::Object(arguments) = serde_json::from_str(raw).ok()? else {
        return None;
    };
    let questions = arguments.get("questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }

    let count = questions.len();
    let count_label = if count == 1 {
        "1 question".to_owned()
    } else {
        format!("{count} questions")
    };

    let first_prompt = questions
        .first()
        .and_then(|question| question.get("prompt"))
        .and_then(Value::as_str)
        .map(collapse_whitespace)
        .filter(|prompt| !prompt.is_empty())
        .map(|prompt| truncate_operand(&prompt, MAX_SUMMARIZED_VALUE_WIDTH));

    Some(match first_prompt {
        Some(prompt) => format!("{{{count_label} · {prompt}}}"),
        None => format!("{{{count_label}}}"),
    })
}

/// Bounded `key=value` pairs for a JSON object's own members.
///
/// Returns `None` for anything that is not a parsable object, so the caller can
/// fall back to the shape scanner that tolerates malformed input.
fn summarize_arguments_by_value(raw: &str) -> Option<String> {
    let Value::Object(arguments) = serde_json::from_str(raw).ok()? else {
        return None;
    };
    if arguments.is_empty() {
        return None;
    }

    let mut parts = Vec::new();
    let mut width = 0usize;
    for (key, value) in arguments.iter().take(MAX_SUMMARIZED_KEYS) {
        let key = truncate_operand(key, MAX_SUMMARIZED_KEY_WIDTH);
        let rendered = match value {
            Value::Object(_) => "{…}".to_owned(),
            Value::Array(items) => format!("[{}]", items.len()),
            Value::Null => "null".to_owned(),
            // A value under a credential-shaped key is withheld on the key
            // alone. Judging it by its own shape asks the wrong question: a
            // short or low-entropy secret is still a secret, and the argument
            // called `token` announced what it holds.
            _ if is_credential_key(&key) => "[redacted]".to_owned(),
            Value::String(text) => {
                let text = redact_credential_values(&collapse_whitespace(text));
                truncate_operand(&text, MAX_SUMMARIZED_VALUE_WIDTH)
            }
            other => other.to_string(),
        };
        let part = format!("{key}={rendered}");
        width += part.width() + 2;
        if width > MAX_SUMMARIZED_ARGUMENTS_WIDTH && !parts.is_empty() {
            parts.push("…".to_owned());
            break;
        }
        parts.push(part);
    }

    (!parts.is_empty()).then(|| format!("{{{}}}", parts.join(", ")))
}

const MAX_RESOURCE_SUMMARY_WIDTH: usize = 48;
const MAX_DESTINATION_SUMMARY_WIDTH: usize = 32;

fn safe_resource_summary(raw: &str) -> Option<String> {
    let Value::Object(arguments) = serde_json::from_str(raw).ok()? else {
        return None;
    };
    let resource = ["readable_id", "slug"]
        .into_iter()
        .find_map(|key| arguments.get(key).and_then(Value::as_str))?;
    let resource = truncate_operand(&collapse_whitespace(resource), MAX_RESOURCE_SUMMARY_WIDTH);
    if resource.is_empty() {
        return None;
    }
    let destination = arguments
        .get("column")
        .and_then(Value::as_str)
        .map(collapse_whitespace)
        .filter(|value| !value.is_empty())
        .map(|value| truncate_operand(&value, MAX_DESTINATION_SUMMARY_WIDTH));
    Some(destination.map_or(resource.clone(), |column| format!("{resource} → {column}")))
}

fn collapse_whitespace(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

const MAX_SUMMARIZED_KEYS: usize = 6;
const MAX_SUMMARIZED_KEY_WIDTH: usize = 24;
const MAX_SUMMARIZED_VALUE_WIDTH: usize = 28;
/// Budget the whole argument summary shares before it elides the rest.
const MAX_SUMMARIZED_ARGUMENTS_WIDTH: usize = 72;

/// Top-level member names of a JSON-shaped object payload, in source order.
///
/// This is a shape scanner, not a parser: it tracks nesting depth and string
/// literals well enough to distinguish a depth-1 key from a value or a nested
/// member, and simply yields nothing for malformed input.
fn object_keys(raw: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut depth = 0usize;
    let mut pending: Option<String> = None;
    let mut characters = raw.chars();

    while let Some(character) = characters.next() {
        match character {
            '{' | '[' => {
                depth += 1;
                pending = None;
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                pending = None;
            }
            '"' => {
                let mut literal = String::new();
                let mut escaped = false;
                for character in characters.by_ref() {
                    if escaped {
                        literal.push(character);
                        escaped = false;
                    } else if character == '\\' {
                        escaped = true;
                    } else if character == '"' {
                        break;
                    } else {
                        literal.push(character);
                    }
                }
                pending = (depth == 1).then_some(literal);
            }
            ':' => {
                if depth == 1
                    && let Some(key) = pending.take()
                {
                    keys.push(key);
                }
            }
            character if character.is_whitespace() => {}
            _ => pending = None,
        }
    }

    keys
}

fn format_key_shape(keys: &[String]) -> String {
    let shown = keys
        .iter()
        .take(MAX_SUMMARIZED_KEYS)
        .map(|key| truncate_operand(&collapse_whitespace(key), MAX_SUMMARIZED_KEY_WIDTH))
        .collect::<Vec<_>>()
        .join(", ");
    if keys.len() > MAX_SUMMARIZED_KEYS {
        format!("{{{shown}, …}}")
    } else {
        format!("{{{shown}}}")
    }
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

/// Plain-text argument detail for overlays and Expanded rows (no JSON dump).
pub(crate) fn tool_argument_detail_text(parsed: &ToolInput, raw_input: &str) -> String {
    expanded_argument_texts(parsed, raw_input, usize::MAX).join("\n")
}

/// Expanded argument body: human-readable fields only, never raw JSON.
fn expanded_tool_argument_lines(
    parsed: &ToolInput,
    raw_input: &str,
    content_width: usize,
) -> Vec<Line<'static>> {
    let width = content_width.saturating_sub(2).max(1);
    expanded_argument_texts(parsed, raw_input, width)
        .into_iter()
        .map(ToolRow::detail)
        .collect()
}

fn expanded_argument_texts(parsed: &ToolInput, raw_input: &str, width: usize) -> Vec<String> {
    match parsed {
        ToolInput::Bash { command } => {
            let redacted = redact_credential_values(command);
            // Keep script structure; wrap each source line on its own.
            let mut lines = Vec::new();
            for line in redacted.lines() {
                if line.is_empty() {
                    lines.push(String::new());
                } else {
                    lines.extend(wrap_command_lines(line, width));
                }
            }
            if lines.is_empty() {
                lines.push(String::new());
            }
            lines
        }
        ToolInput::Read { path }
        | ToolInput::Write { path }
        | ToolInput::Edit { path }
        | ToolInput::List { path }
        | ToolInput::Search { path } => wrap_command_lines(&format!("path {path}"), width),
        ToolInput::Glob { pattern, path } => {
            let mut lines = wrap_command_lines(&format!("pattern {pattern}"), width);
            if let Some(path) = path {
                lines.extend(wrap_command_lines(&format!("path {path}"), width));
            }
            lines
        }
        ToolInput::Grep { pattern, path } => {
            let mut lines = wrap_command_lines(&format!("pattern {pattern}"), width);
            if let Some(path) = path {
                lines.extend(wrap_command_lines(&format!("path {path}"), width));
            }
            lines
        }
        ToolInput::WebFetch { url } => wrap_command_lines(&redact_credential_values(url), width),
        ToolInput::Skill { skill } => wrap_command_lines(skill, width),
        ToolInput::Other { name, raw } => {
            let summary = summarize_args(name, raw);
            if summary.is_empty() {
                // Non-JSON free text only — still never echo a JSON object body.
                let trimmed = raw_input.trim();
                if trimmed.starts_with('{') {
                    wrap_command_lines(&format_key_shape(&object_keys(trimmed)), width)
                } else {
                    wrap_command_lines(
                        &redact_credential_values(&collapse_whitespace(trimmed)),
                        width,
                    )
                }
            } else {
                wrap_command_lines(&summary, width)
            }
        }
    }
}

/// Greedy wrap of a shell command for transcript body rows (no mid-path ellipsis).
fn wrap_command_lines(command: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    if command.width() <= width {
        return vec![command.to_owned()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in command.split_whitespace() {
        let word_width = word.width();
        if current.is_empty() {
            if word_width <= width {
                current.push_str(word);
                current_width = word_width;
            } else {
                let mut rest = word;
                while !rest.is_empty() {
                    let mut take = 0usize;
                    let mut taken_width = 0usize;
                    for (index, character) in rest.char_indices() {
                        let character_width = character.width().unwrap_or(0);
                        if taken_width + character_width > width && take > 0 {
                            break;
                        }
                        take = index + character.len_utf8();
                        taken_width += character_width;
                    }
                    lines.push(rest[..take].to_owned());
                    rest = &rest[take..];
                }
            }
            continue;
        }

        if current_width + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
            current_width = word_width;
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Tool call header/args, wrapped as `BlockContent` with a typed per-tool header.
///
/// Neither the header nor Expanded detail dumps raw JSON argument payloads.
/// Typed tools show their authoritative fields; bash shows the command as a
/// shell script; unknown tools show a value summary. Secrets are redacted.
pub(crate) struct ToolCallBlock<'a> {
    pub(crate) input: &'a str,
    pub(crate) parsed: &'a ToolInput,
    pub(crate) batch: Option<usize>,
    pub(crate) content_width: usize,
    pub(crate) state: RowState,
    pub(crate) result: Option<&'a ToolResultBlock>,
}

impl ToolCallBlock<'_> {
    /// Accent bar every row of the call carries.
    ///
    /// A pending call breathes. A settled read-family call drops the bar and
    /// leaves its outcome to the bullet colour, while a settled destructive or
    /// opaque call keeps a still bar so it stays scannable after the fact.
    fn row_accent(&self) -> Option<RowAccent> {
        match self.state {
            RowState::Running => Some(RowAccent::Wave(RolePalette::running())),
            _ if tool_input_groupable(self.parsed) => None,
            state => Some(RowAccent::Still(state.color())),
        }
    }
}

impl BlockContent for ToolCallBlock<'_> {
    fn lines(&self, mode: DisplayMode) -> Vec<BlockLine> {
        let accent = self.row_accent();
        let mut lines = Vec::new();
        if let Some(batch) = self.batch {
            lines.push(
                BlockLine::with_bullet(
                    Line::from(Span::styled(
                        format!("Tools · batch {batch}"),
                        Style::default()
                            .fg(RolePalette::muted())
                            .add_modifier(Modifier::BOLD),
                    )),
                    RowBullet::Group(self.state),
                )
                .accented(accent),
            );
        }

        let (status, failed, size) = self.result.map_or_else(
            || match self.state {
                RowState::Running => ("Running…", false, None),
                RowState::Success => ("Success", false, None),
                RowState::Failure => ("Failure", true, None),
                RowState::Muted => ("Pending…", false, None),
            },
            |result| {
                (
                    result.status.as_str(),
                    result.failed,
                    Some(result.size.as_str()),
                )
            },
        );
        let full_header = tool_header(self.parsed, self.content_width);
        let minimum_header_width = full_header.width().min(6);
        let terminal_state = status.split(" · ").next().unwrap_or(status);
        let suffix = [(status, size), (status, None), (terminal_state, None)]
            .into_iter()
            .map(|(status, size)| ToolRow::lifecycle_suffix(status, failed, size))
            .find(|suffix| {
                minimum_header_width
                    + suffix
                        .iter()
                        .map(|span| span.content.width())
                        .sum::<usize>()
                    <= self.content_width
            })
            .unwrap_or_default();
        let suffix_width = suffix
            .iter()
            .map(|span| span.content.width())
            .sum::<usize>();
        let header_budget = self.content_width.saturating_sub(suffix_width).max(1);
        let mut header = tool_header(self.parsed, header_budget);
        header.spans.extend(suffix);
        lines
            .push(BlockLine::with_bullet(header, RowBullet::Activity(self.state)).accented(accent));

        if mode == DisplayMode::Expanded {
            for detail in
                expanded_tool_argument_lines(self.parsed, self.input, self.content_width.max(1))
            {
                lines.push(BlockLine::new(detail).accented(accent));
            }
        } else if mode == DisplayMode::Truncated {
            // When the bash header ellipsizes the command, keep the full
            // (secret-redacted) command on following rows so Truncated is still
            // auditable without forcing Expanded.
            if let ToolInput::Bash { command } = self.parsed {
                let collapsed = collapse_whitespace(command);
                let parts = header_parts(self.parsed);
                let verb_width = if parts.verb.is_empty() {
                    0
                } else {
                    parts.verb.width() + 1
                };
                let operand_budget = header_budget.saturating_sub(verb_width).max(1);
                if collapsed.width() > operand_budget {
                    let redacted = redact_credential_values(&collapsed);
                    for chunk in wrap_command_lines(&redacted, self.content_width.max(1)) {
                        lines.push(
                            BlockLine::new(Line::from(Span::styled(
                                chunk,
                                Style::default().fg(RolePalette::machine()),
                            )))
                            .accented(accent),
                        );
                    }
                }
            }
        }
        lines
    }

    /// A settled call costs one row and keeps its raw input behind the audit
    /// mode; a pending one keeps a bounded preview so live work stays visible
    /// while it happens. Settled `ask_user` defaults to Truncated so the
    /// human answer summary is visible without forcing a full expand.
    fn default_mode(&self) -> DisplayMode {
        if self.result.is_some() {
            if tool_input_is_ask_user(self.parsed) {
                DisplayMode::Truncated
            } else {
                DisplayMode::Collapsed
            }
        } else {
            DisplayMode::Truncated
        }
    }

    fn accent(&self) -> Color {
        self.state.color()
    }

    fn is_groupable(&self) -> bool {
        tool_input_groupable(self.parsed)
    }
}

/// Terminal metadata appended to the tool call's stable header.
pub(crate) struct ToolResultBlock {
    pub(crate) status: String,
    pub(crate) failed: bool,
    pub(crate) size: String,
}

const PREVIEW_HEAD_LINES: usize = 5;
const PREVIEW_TAIL_LINES: usize = 3;

/// Head/tail preview window for a [`DisplayMode::Truncated`] body.
///
/// Bodies short enough to fit the window are returned unchanged so the marker
/// never claims a truncation that did not happen.
pub(crate) fn bounded_tool_preview(body: &[Line<'static>]) -> Vec<Line<'static>> {
    if body.len() <= PREVIEW_HEAD_LINES + PREVIEW_TAIL_LINES + 1 {
        return body.to_vec();
    }

    let hidden = body.len() - PREVIEW_HEAD_LINES - PREVIEW_TAIL_LINES;
    let mut preview = body[..PREVIEW_HEAD_LINES].to_vec();
    preview.push(Line::from(Span::styled(
        format!("… {hidden} more lines · Ctrl+O for full output"),
        Style::default().fg(RolePalette::muted()),
    )));
    preview.extend_from_slice(&body[body.len() - PREVIEW_TAIL_LINES..]);
    preview
}

fn short_tool_name(name: &str) -> String {
    name.strip_prefix("native::")
        .or_else(|| name.strip_prefix("mcp::"))
        .unwrap_or(name)
        .to_owned()
}

fn tool_input_is_ask_user(parsed: &ToolInput) -> bool {
    matches!(parsed, ToolInput::Other { name, .. } if is_ask_user_tool_name(name))
}

/// Human-readable body lines for a settled `ask_user` tool result.
///
/// Maps option ids back to labels via the call's input when present. Returns
/// `None` when the payload is not a recognized ask_user envelope so the caller
/// can fall back to the ordinary tool-output renderer.
pub(crate) fn format_ask_user_result_lines(input: &str, output: &str) -> Option<Vec<String>> {
    let Value::Object(result) = serde_json::from_str(output.trim()).ok()? else {
        return None;
    };
    let status = result.get("status")?.as_str()?;

    match status {
        "cancelled" => Some(vec!["cancelled".to_owned()]),
        "unavailable" => {
            let reason = result
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("interactive surface unavailable");
            Some(vec![format!("unavailable · {reason}")])
        }
        "discuss" => {
            let question = result
                .get("question_id")
                .and_then(Value::as_str)
                .unwrap_or("question");
            let mut lines = vec![format!("discuss · {question}")];
            if let Some(note) = result.get("note").and_then(Value::as_str)
                && !note.trim().is_empty()
            {
                lines.push(format!("  note: {note}"));
            }
            Some(lines)
        }
        "answered" => {
            let answers = result.get("answers")?.as_array()?;
            let questions = parse_ask_user_input_questions(input).unwrap_or_default();
            let total = answers.len().max(questions.len());
            let answered_count = answers
                .iter()
                .filter(|answer| answer.get("answered").and_then(Value::as_bool) == Some(true))
                .count();
            let mut lines = vec![format!("answered · {answered_count}/{total}")];

            for (index, answer) in answers.iter().enumerate() {
                let question_id = answer
                    .get("question_id")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let question = questions
                    .iter()
                    .find(|question| question.id == question_id)
                    .or_else(|| questions.get(index));
                let prompt = question
                    .map(|question| question.prompt.as_str())
                    .filter(|prompt| !prompt.is_empty())
                    .unwrap_or(question_id);
                let answer_text = ask_user_result_answer_text(answer, question);
                lines.push(format!("  {prompt} → {answer_text}"));
                if let Some(note) = answer.get("note").and_then(Value::as_str)
                    && !note.trim().is_empty()
                {
                    lines.push(format!("    note: {note}"));
                }
            }
            Some(lines)
        }
        _ => None,
    }
}

#[derive(Clone, Debug)]
struct AskUserInputQuestion {
    id: String,
    prompt: String,
    options: Vec<(String, String)>,
}

fn parse_ask_user_input_questions(input: &str) -> Option<Vec<AskUserInputQuestion>> {
    let Value::Object(arguments) = serde_json::from_str(input.trim()).ok()? else {
        return None;
    };
    let questions = arguments.get("questions")?.as_array()?;
    let mut parsed = Vec::with_capacity(questions.len());
    for question in questions {
        let object = question.as_object()?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let prompt = object
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mut options = Vec::new();
        if let Some(items) = object.get("options").and_then(Value::as_array) {
            for option in items {
                let Some(option) = option.as_object() else {
                    continue;
                };
                let option_id = option
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let label = option
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or(option_id.as_str())
                    .to_owned();
                if !option_id.is_empty() {
                    options.push((option_id, label));
                }
            }
        }
        parsed.push(AskUserInputQuestion {
            id,
            prompt,
            options,
        });
    }
    Some(parsed)
}

fn ask_user_result_answer_text(answer: &Value, question: Option<&AskUserInputQuestion>) -> String {
    let selected_ids: Vec<&str> = answer
        .get("selected")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let other = answer
        .get("other")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut parts = Vec::new();
    for id in selected_ids {
        let label = question
            .and_then(|question| {
                question
                    .options
                    .iter()
                    .find(|(option_id, _)| option_id == id)
                    .map(|(_, label)| label.as_str())
            })
            .unwrap_or(id);
        parts.push(label.to_owned());
    }
    if let Some(other) = other {
        parts.push(other.to_owned());
    }

    if parts.is_empty() {
        "(skipped)".to_owned()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hidden thought names itself and what it was about. A column of rows
    /// reading only "Thought" is the shape of information without any: nothing
    /// there tells one from the next.
    #[test]
    fn collapsed_thinking_is_one_row_naming_the_thought_and_its_duration() {
        assert_eq!(line_text(&ThinkingBlock::title()), "Thinking");
        assert_eq!(
            line_text(&ThinkingBlock::collapsed_title(None, None)),
            "Thought"
        );
        assert_eq!(
            line_text(&ThinkingBlock::collapsed_title(
                Some("Investigating the timeout"),
                None
            )),
            "Thought · Investigating the timeout",
            "a collapsed thought says what it was about"
        );
        for (elapsed, expected) in [
            (Duration::from_millis(1_800), "Thought for 1.8s"),
            (Duration::from_millis(59_940), "Thought for 59.9s"),
            (Duration::from_secs(60), "Thought for 1m0s"),
            (Duration::from_secs(125), "Thought for 2m5s"),
        ] {
            assert_eq!(
                line_text(&ThinkingBlock::collapsed_title(None, Some(elapsed))),
                expected
            );
        }
    }

    #[test]
    fn thinking_mode_streams_then_collapses_unless_expanded() {
        assert_eq!(ThinkingBlock::mode(true, true), ExpandMode::Streaming);
        assert_eq!(ThinkingBlock::mode(false, true), ExpandMode::Collapsed);
        assert_eq!(ThinkingBlock::mode(false, false), ExpandMode::Expanded);
    }

    #[test]
    fn tool_row_detail_and_lifecycle_suffix_are_stable() {
        let detail = ToolRow::detail("src/lib.rs");
        let detail_text: String = detail
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(detail_text.starts_with("│ "), "{detail_text:?}");
        assert!(detail_text.contains("src/lib.rs"));
        assert!(
            !detail_text.contains("input"),
            "detail lines must not be labeled as raw JSON input: {detail_text:?}"
        );

        let success = ToolRow::lifecycle_suffix("Success · 12ms", false, Some("2 lines · 21 B"));
        let success_text = success
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(success_text, " · Success · 12ms · 2 lines · 21 B");
        assert_eq!(success[1].style.fg, Some(RolePalette::success()));

        let running = ToolRow::lifecycle_suffix("Running…", false, None);
        assert_eq!(running[1].style.fg, Some(RolePalette::running()));
        let failure = ToolRow::lifecycle_suffix("Failure", true, None);
        assert_eq!(failure[1].style.fg, Some(RolePalette::error()));
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
    fn ask_user_header_summarizes_question_count_and_first_prompt() {
        let one = header_of(&ToolInput::Other {
            name: "native::ask_user".into(),
            raw: r#"{"questions":[{"id":"q1","prompt":"¿Por dónde arrancamos la migración?","mode":"single","options":[{"id":"a","label":"A"}]}]}"#.into(),
        });
        assert!(one.starts_with("ask_user {1 question"), "{one:?}");
        assert!(
            one.contains("¿Por dónde arrancamos"),
            "first prompt must reach the header: {one:?}"
        );
        assert!(
            !one.contains("questions=[1]"),
            "nested arrays must not collapse to a bare count: {one:?}"
        );

        let three = header_of(&ToolInput::Other {
            name: "native::ask_user".into(),
            raw: r#"{"questions":[
                {"id":"q1","prompt":"One","mode":"single","options":[{"id":"a","label":"A"}]},
                {"id":"q2","prompt":"Two","mode":"single","options":[{"id":"a","label":"A"}]},
                {"id":"q3","prompt":"Three","mode":"single","options":[{"id":"a","label":"A"}]}
            ]}"#
            .into(),
        });
        assert!(three.contains("3 questions"), "{three:?}");
        assert!(three.contains("One"), "{three:?}");
    }

    #[test]
    fn ask_user_result_body_maps_selected_ids_to_labels() {
        let input = r#"{"questions":[{"id":"q1","prompt":"Pick a path","mode":"single","options":[{"id":"a","label":"Big bang"},{"id":"b","label":"Phased"}]}]}"#;
        let output = r#"{"status":"answered","answers":[{"question_id":"q1","answered":true,"selected":["b"],"other":null,"note":"careful"}]}"#;
        let lines = format_ask_user_result_lines(input, output).expect("answered envelope");
        let joined = lines.join("\n");
        assert!(joined.contains("answered · 1/1"), "{joined:?}");
        assert!(joined.contains("Pick a path → Phased"), "{joined:?}");
        assert!(joined.contains("note: careful"), "{joined:?}");
        assert!(
            !joined.contains(r#""status""#),
            "raw JSON must not be primary: {joined:?}"
        );

        let cancelled = format_ask_user_result_lines(input, r#"{"status":"cancelled"}"#)
            .expect("cancelled envelope");
        assert_eq!(cancelled, vec!["cancelled".to_owned()]);
    }

    #[test]
    fn settled_ask_user_defaults_to_truncated_display_mode() {
        let raw = r#"{"questions":[{"id":"q1","prompt":"Hi","mode":"single","options":[{"id":"a","label":"A"}]}]}"#;
        let parsed = ToolInput::Other {
            name: "native::ask_user".into(),
            raw: raw.into(),
        };
        let result = ToolResultBlock {
            status: "Success · 12ms".into(),
            failed: false,
            size: "1 line · 40 B".into(),
        };
        let block = ToolCallBlock {
            input: raw,
            parsed: &parsed,
            batch: None,
            content_width: 80,
            state: RowState::Success,
            result: Some(&result),
        };
        assert_eq!(
            block.default_mode(),
            DisplayMode::Truncated,
            "settled ask_user must show its human summary by default"
        );
    }

    #[test]
    fn unknown_and_mcp_headers_fall_back_to_name_and_summarized_args() {
        assert_eq!(
            header_of(&ToolInput::Other {
                name: "mcp::foo__bar".into(),
                raw: "{\"a\":1}".into(),
            }),
            "foo__bar {a=1}"
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
    fn fold_policy_groups_read_family_but_never_a_destructive_kind() {
        for groupable in [
            ToolInput::Read { path: "a".into() },
            ToolInput::List { path: "a".into() },
            ToolInput::Search { path: "a".into() },
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
            ToolInput::Write { path: "a".into() },
            ToolInput::Other {
                name: "x".into(),
                raw: "y".into(),
            },
        ] {
            assert!(!tool_input_groupable(&ungroupable), "{ungroupable:?}");
        }
    }

    #[test]
    fn every_groupable_kind_has_a_tense_aware_group_verb() {
        for parsed in [
            ToolInput::Read { path: "a".into() },
            ToolInput::List { path: "a".into() },
            ToolInput::Search { path: "a".into() },
            ToolInput::Glob {
                pattern: "*".into(),
                path: None,
            },
            ToolInput::Grep {
                pattern: "*".into(),
                path: None,
            },
            ToolInput::Write { path: "a".into() },
            ToolInput::Edit { path: "a".into() },
            ToolInput::Bash {
                command: "ls".into(),
            },
            ToolInput::WebFetch { url: "u".into() },
            ToolInput::Skill { skill: "s".into() },
            ToolInput::Other {
                name: "x".into(),
                raw: "y".into(),
            },
        ] {
            assert_eq!(
                VerbGroup::of(&parsed).is_some(),
                tool_input_groupable(&parsed),
                "fold policy and verb vocabulary must agree: {parsed:?}"
            );
        }

        assert_eq!(VerbGroup::Read.label(3, true), "Reading 3 files…");
        assert_eq!(VerbGroup::Read.label(3, false), "Read 3 files");
        assert_eq!(VerbGroup::List.label(2, false), "Listed 2 directories");
        assert_eq!(VerbGroup::Glob.label(2, true), "Matching 2 patterns…");
        assert_eq!(VerbGroup::Grep.label(4, false), "Searched 4 patterns");
    }

    #[test]
    fn write_calls_never_fold_eagerly_even_though_they_read_like_a_path_verb() {
        let write = ToolInput::Write {
            path: "src/generated.rs".into(),
        };
        let block = ToolCallBlock {
            input: "{\"path\":\"src/generated.rs\"}",
            parsed: &write,
            batch: None,
            content_width: 80,
            state: RowState::Running,
            result: None,
        };
        assert!(
            !block.is_groupable(),
            "a write is destructive and keeps its own row"
        );
    }

    /// A batch of calls to the same tool differs only in its values, so the
    /// summary carries them; nested payloads stay shape-only, which is what
    /// keeps a blob out of a one-line row.
    #[test]
    fn mcp_headers_summarize_what_was_asked_for_not_only_its_shape() {
        let header = header_of(&ToolInput::Other {
            name: "mcp::foo__bar".into(),
            raw: "{\"path\":\"/etc/hosts\",\"limit\":10,\"nested\":{\"deep\":true}}".into(),
        });
        assert_eq!(header, "foo__bar {limit=10, nested={…}, path=/etc/hosts}");
        assert!(!header.contains('"'), "no raw JSON punctuation: {header:?}");
        assert!(!header.contains("deep"), "no nested keys");

        let credential = header_of(&ToolInput::Other {
            name: "mcp::foo__bar".into(),
            raw: "{\"token\":\"sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"}".into(),
        });
        assert!(
            !credential.contains("sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            "an argument that looks like a credential never reaches the row: {credential:?}"
        );

        // Shape is not the only signal, and it is the weaker one: a short or
        // low-entropy secret looks like nothing, while the key that holds it
        // already said what it is.
        let named = header_of(&ToolInput::Other {
            name: "mcp::foo__bar".into(),
            raw: r#"{"token":"hunter2","password":"x","tokenizer":"bpe"}"#.into(),
        });
        assert_eq!(
            named, "foo__bar {password=[redacted], token=[redacted], tokenizer=bpe}",
            "a credential-shaped key withholds its value whatever the value looks like, \
             and a word that merely contains one does not"
        );

        let long = header_of(&ToolInput::Other {
            name: "mcp::foo__bar".into(),
            raw: format!("{{\"query\":\"{}\"}}", "x".repeat(200)),
        });
        assert!(long.width() < 80, "the summary stays bounded: {long:?}");

        assert_eq!(
            header_of(&ToolInput::Other {
                name: "native::custom".into(),
                raw: "{}".into(),
            }),
            "custom {}"
        );
        assert_eq!(
            header_of(&ToolInput::Other {
                name: "mcp::atlas_get_task".into(),
                raw: r#"{"detail":"full","readable_id":"AGN-37","workspace":"agens","token":"SECRET"}"#.into(),
            }),
            "atlas_get_task AGN-37"
        );
        assert_eq!(
            header_of(&ToolInput::Other {
                name: "mcp::atlas_move_task".into(),
                raw:
                    r#"{"board":"Work","column":"Done","readable_id":"AGN-92","workspace":"agens"}"#
                        .into(),
            }),
            "atlas_move_task AGN-92 → Done"
        );
        // Outside the Atlas shape the summary is generic rather than absent:
        // the values are what tell one call from the next, and they are the
        // same values the model was already given.
        let non_atlas = header_of(&ToolInput::Other {
            name: "mcp::other_lookup".into(),
            raw: r#"{"readable_id":"LOOKUP-ID","slug":"lookup-slug"}"#.into(),
        });
        assert_eq!(
            non_atlas,
            "other_lookup {readable_id=LOOKUP-ID, slug=lookup-slug}"
        );
        // Non-object payloads keep the collapsed single-line summary.
        assert_eq!(
            header_of(&ToolInput::Other {
                name: "native::custom".into(),
                raw: "line one\n  line two".into(),
            }),
            "custom line one line two"
        );
    }

    #[test]
    fn truncated_result_body_is_a_bounded_preview_while_expanded_shows_everything() {
        let full_body: Vec<Line<'static>> = (1..=40)
            .map(|index| Line::from(format!("row {index}")))
            .collect();
        let truncated = bounded_tool_preview(&full_body);
        let truncated_text: Vec<String> = truncated.iter().map(line_text).collect();
        assert!(
            truncated.len() < 15,
            "truncated stays bounded: {truncated_text:?}"
        );
        assert!(truncated_text.iter().any(|row| row == "row 1"));
        assert!(truncated_text.iter().any(|row| row == "row 5"));
        assert!(!truncated_text.iter().any(|row| row == "row 6"));
        assert!(truncated_text.iter().any(|row| row == "row 40"));
        assert!(
            truncated_text
                .iter()
                .any(|row| row.contains("32 more lines")),
            "elision marker names the hidden count: {truncated_text:?}"
        );

        assert_eq!(full_body.len(), 40, "expanded still shows the whole body");
    }

    #[test]
    fn narrow_tool_rows_keep_both_identity_and_lifecycle() {
        let parsed = ToolInput::Other {
            name: "mcp::atlas_get_document".into(),
            raw: r#"{"detail":"full","slug":"spec","workspace":"agens"}"#.into(),
        };
        let running = ToolCallBlock {
            input: "{}",
            parsed: &parsed,
            batch: None,
            content_width: 18,
            state: RowState::Running,
            result: None,
        };
        let running = line_text(&running.lines(DisplayMode::Collapsed)[0].line);
        assert!(running.contains("atlas"), "{running:?}");
        assert!(running.contains("Running"), "{running:?}");
        assert!(running.width() <= 18, "{running:?}");

        let result = ToolResultBlock {
            status: "Success · 673ms".into(),
            failed: false,
            size: "1 line · 13 B".into(),
        };
        let settled = ToolCallBlock {
            input: "{}",
            parsed: &parsed,
            batch: None,
            content_width: 18,
            state: RowState::Success,
            result: Some(&result),
        };
        let settled = line_text(&settled.lines(DisplayMode::Collapsed)[0].line);
        assert!(settled.contains("atlas"), "{settled:?}");
        assert!(settled.contains("Success"), "{settled:?}");
        assert!(settled.width() <= 18, "{settled:?}");
    }

    #[test]
    fn tool_call_block_defaults_truncated_and_shows_human_args_when_expanded() {
        let parsed = ToolInput::Read {
            path: "src/lib.rs".into(),
        };
        let block = ToolCallBlock {
            input: "{\"path\":\"src/lib.rs\"}",
            parsed: &parsed,
            batch: Some(2),
            content_width: 80,
            state: RowState::Running,
            result: None,
        };
        assert_eq!(block.default_mode(), DisplayMode::Truncated);
        assert!(block.is_groupable());

        let truncated = block.lines(DisplayMode::Truncated);
        assert_eq!(truncated.len(), 2);
        assert!(line_text(&truncated[0].line).contains("batch 2"));
        assert_eq!(line_text(&truncated[1].line), "Read src/lib.rs · Running…");
        assert!(
            truncated
                .iter()
                .all(|row| !line_text(&row.line).contains("{\"path\"")),
            "raw JSON must not appear in the collapsed/truncated header"
        );

        let expanded = block.lines(DisplayMode::Expanded);
        let expanded_text: String = expanded
            .iter()
            .map(|row| line_text(&row.line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            expanded
                .iter()
                .any(|row| line_text(&row.line).contains("path src/lib.rs")),
            "expanded detail must show the typed path, not JSON: {expanded_text:?}"
        );
        assert!(
            !expanded_text.contains("{\""),
            "expanded detail must not dump raw JSON: {expanded_text:?}"
        );
    }

    #[test]
    fn expanded_bash_shows_the_command_not_json() {
        let command = "rm -f conformance/manifest/.gitkeep && python3 - <<'PY'\nprint(1)\nPY";
        let parsed = ToolInput::Bash {
            command: command.into(),
        };
        let raw = format!(
            r#"{{"command":{}}}"#,
            serde_json::to_string(command).unwrap()
        );
        let block = ToolCallBlock {
            input: &raw,
            parsed: &parsed,
            batch: Some(1),
            content_width: 80,
            state: RowState::Failure,
            result: Some(&ToolResultBlock {
                status: "Failure · 2s".into(),
                failed: true,
                size: "1 lines · 132 B".into(),
            }),
        };

        let expanded = block.lines(DisplayMode::Expanded);
        let text: String = expanded
            .iter()
            .map(|row| line_text(&row.line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("rm -f conformance/manifest/.gitkeep"),
            "bash expanded must show the shell command: {text:?}"
        );
        assert!(
            text.contains("print(1)"),
            "bash expanded must keep script lines: {text:?}"
        );
        assert!(
            !text.contains(r#"{"command""#),
            "bash expanded must not show JSON wrapper: {text:?}"
        );
        assert!(
            !text.contains("│ input"),
            "bash expanded must not label rows as raw input: {text:?}"
        );
    }

    #[test]
    fn finished_tool_call_keeps_status_and_size_on_its_only_collapsed_row() {
        let parsed = ToolInput::Read {
            path: "src/lib.rs".into(),
        };
        let result = ToolResultBlock {
            status: "Success · 12ms".into(),
            failed: false,
            size: "2 lines · 21 B".into(),
        };
        let block = ToolCallBlock {
            input: "{\"path\":\"src/lib.rs\"}",
            parsed: &parsed,
            batch: None,
            content_width: 80,
            state: RowState::Success,
            result: Some(&result),
        };
        assert_eq!(
            block.default_mode(),
            DisplayMode::Collapsed,
            "a settled call defaults to its single collapsed row, not to the audit view"
        );

        let collapsed = block.lines(DisplayMode::Collapsed);
        assert_eq!(collapsed.len(), 1);
        let row = line_text(&collapsed[0].line);
        assert!(row.contains("Read src/lib.rs"), "{row:?}");
        assert!(row.contains("Success · 12ms · 2 lines · 21 B"), "{row:?}");

        let truncated = block.lines(DisplayMode::Truncated);
        assert_eq!(truncated.len(), 1);

        let expanded = block.lines(DisplayMode::Expanded);
        assert!(
            line_text(&expanded[1].line).contains("path src/lib.rs"),
            "{:?}",
            line_text(&expanded[1].line)
        );
        assert!(
            !line_text(&expanded[1].line).contains('{'),
            "expanded must not dump JSON: {:?}",
            line_text(&expanded[1].line)
        );
    }

    fn call_accents(
        parsed: &ToolInput,
        state: RowState,
        mode: DisplayMode,
    ) -> Vec<Option<RowAccent>> {
        ToolCallBlock {
            input: "{}",
            parsed,
            batch: Some(1),
            content_width: 80,
            state,
            result: None,
        }
        .lines(mode)
        .iter()
        .map(|row| row.accent)
        .collect()
    }

    #[test]
    fn accent_membership_follows_the_block_kind_and_its_lifecycle() {
        let read = ToolInput::Read {
            path: "a.rs".into(),
        };
        let bash = ToolInput::Bash {
            command: "cargo check".into(),
        };
        let running = Some(RowAccent::Wave(RolePalette::running()));

        for parsed in [&read, &bash] {
            assert_eq!(
                call_accents(parsed, RowState::Running, DisplayMode::Expanded),
                vec![running; 3],
                "every row of a pending call breathes: {parsed:?}"
            );
        }

        assert_eq!(
            call_accents(&read, RowState::Success, DisplayMode::Expanded),
            vec![None; 3],
            "a settled read leaves its outcome to the bullet"
        );
        assert_eq!(
            call_accents(&bash, RowState::Failure, DisplayMode::Truncated),
            vec![Some(RowAccent::Still(RolePalette::error())); 2],
            "a settled destructive call keeps a still bar in its own colour"
        );

        let result = ToolResultBlock {
            status: "Success".into(),
            failed: false,
            size: "1 line".into(),
        };
        let result_accents = |parsed: &ToolInput, state| {
            ToolCallBlock {
                input: "{}",
                parsed,
                batch: None,
                content_width: 80,
                state,
                result: Some(&result),
            }
            .lines(DisplayMode::Expanded)
            .iter()
            .map(|row| row.accent)
            .collect::<Vec<_>>()
        };
        assert_eq!(
            result_accents(&read, RowState::Success),
            vec![None; 2],
            "a read result agrees with its own bare call row"
        );
        assert_eq!(
            result_accents(&bash, RowState::Success),
            vec![Some(RowAccent::Still(RolePalette::success())); 2],
            "a destructive result keeps the bar its call row started"
        );

        assert_eq!(
            ThinkingBlock::accent(ExpandMode::Streaming),
            Some(RowAccent::Wave(RowState::Running.color())),
            "a streaming thought breathes in the live accent, not a hue of its own"
        );
        assert_eq!(
            ThinkingBlock::accent(ExpandMode::Expanded),
            Some(RowAccent::Still(RolePalette::muted())),
            "a pinned-open thought is neither live nor an outcome"
        );
        assert_eq!(
            ThinkingBlock::accent(ExpandMode::Collapsed),
            Some(RowAccent::Still(RolePalette::muted())),
            "a hidden thought keeps the gutter every other block sits on"
        );
    }

    #[test]
    fn the_accent_bar_moves_in_colour_only_and_dims_when_collapsed() {
        let wave = RowAccent::Wave(RolePalette::running());
        let first = wave.span(0, Duration::ZERO, UnicodeLevel::Extended);
        let later = wave.span(0, Duration::from_millis(240), UnicodeLevel::Extended);
        let travelled = wave.span(1, Duration::ZERO, UnicodeLevel::Extended);

        assert_eq!(first.content, later.content, "the glyph never changes");
        assert_eq!(
            first.style.fg,
            Some(RolePalette::running()),
            "the wave opens on the full running colour"
        );
        assert_ne!(
            first.style.fg, later.style.fg,
            "the bar breathes with ticks"
        );
        assert_eq!(
            later.style.fg, travelled.style.fg,
            "the wave travels down the block by one row per frame"
        );
        assert_eq!(
            wave.span(0, Duration::from_millis(239), UnicodeLevel::Extended)
                .style
                .fg,
            first.style.fg,
            "a frame holds for three spinner periods"
        );

        let still = RowAccent::Still(RolePalette::success()).span(
            0,
            Duration::from_millis(240),
            UnicodeLevel::Extended,
        );
        assert_eq!(still.content, first.content);
        assert_eq!(still.style.fg, Some(RolePalette::success()));

        let collapsed = RowAccent::Collapsed(RolePalette::success()).span(
            0,
            Duration::from_millis(240),
            UnicodeLevel::Extended,
        );
        assert_ne!(
            collapsed.content, first.content,
            "a collapsed run gets the thin variant"
        );
        assert_eq!(
            collapsed.style.fg,
            Some(Color::Rgb(0x55, 0x6c, 0x26)),
            "the thin bar is held at half its own brightness"
        );
    }

    #[test]
    fn collapsed_failure_is_one_row_and_expansion_adds_only_call_input() {
        let parsed = ToolInput::Bash {
            command: "missing-command".into(),
        };
        let result = ToolResultBlock {
            status: "Failure".into(),
            failed: true,
            size: "2 lines".into(),
        };
        let block = ToolCallBlock {
            input: "{}",
            parsed: &parsed,
            batch: None,
            content_width: 80,
            state: RowState::Failure,
            result: Some(&result),
        };

        let collapsed = block.lines(DisplayMode::Collapsed);
        assert_eq!(collapsed.len(), 1);
        assert!(line_text(&collapsed[0].line).contains("Failure"));
        assert!(!line_text(&collapsed[0].line).contains("command not found"));

        let expanded = block.lines(DisplayMode::Expanded);
        assert_eq!(expanded.len(), 2, "expanded call shows its raw input row");
    }

    #[test]
    fn truncated_bash_shows_the_full_command_when_the_header_ellipsizes_it() {
        let command = "cd /very/long/path/to/the/project/root && git commit -m 'long message that will not fit'";
        let parsed = ToolInput::Bash {
            command: command.into(),
        };
        let block = ToolCallBlock {
            input: r#"{"command":"unused"}"#,
            parsed: &parsed,
            batch: None,
            content_width: 40,
            state: RowState::Running,
            result: None,
        };

        let truncated = block.lines(DisplayMode::Truncated);
        assert!(
            truncated.len() > 1,
            "truncated bash with a long command must add body rows, got {} rows",
            truncated.len()
        );
        let body = truncated
            .iter()
            .skip(1)
            .map(|row| line_text(&row.line))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            body.contains("git commit") && body.contains("/very/long/path"),
            "full command must remain readable under Truncated: {body}"
        );
        assert!(
            !body.contains('…'),
            "body rows wrap rather than ellipsizing: {body}"
        );

        let short = ToolInput::Bash {
            command: "git status".into(),
        };
        let short_block = ToolCallBlock {
            input: "{}",
            parsed: &short,
            batch: None,
            content_width: 80,
            state: RowState::Running,
            result: None,
        };
        assert_eq!(
            short_block.lines(DisplayMode::Truncated).len(),
            1,
            "a command that fits the header needs no extra body row"
        );
    }
}
