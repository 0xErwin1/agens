//! Rich presentation of typed runtime details without mutating their source data.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    mem::size_of,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use arborium::{GrammarStore, Highlighter};
use arborium_highlight::spans_to_flat_tokens;
use arborium_theme::{
    Theme, capture_to_slot, slot_to_highlight_index, tag_to_name, theme::builtin,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::conversation::ConversationItem;
use crate::widgets::{
    ACCENT_WIDTH, BlockContent, BlockLine, DisplayMode, GUTTER_WIDTH, Glyph, RolePalette,
    RowAccent, RowBullet, RowState, StatusGlyph, ThinkingBlock, ToolCallBlock, ToolResultBlock,
    UnicodeLevel, VerbGroup,
};
use crate::{Conversation, DiffLine, DiffLineKind, ToolResultState, TuiRuntimeEvent};

const MAX_VISIBLE_TOOL_OUTPUT_BYTES: usize = 4 * 1024;
const VISIBLE_TOOL_OUTPUT_MARKER: &str = "\n… visible output truncated";
/// Closes a code-panel row whose source line was wider than the panel.
const CODE_CLIP_MARKER: &str = "…";
/// Columns a tool's output sits right of the call header that produced it.
const TOOL_BODY_INDENT: usize = 2;
const SETTLED_CONVERSATION_CACHE_MAX_ENTRIES: usize = 96;
const SYNTAX_CACHE_MAX_ENTRIES: usize = 64;
const SYNTAX_CACHE_MAX_BYTES: usize = 4 * 1024 * 1024;
const SYNTAX_DEFER_SOURCE_BYTES: usize = 4 * 1024;
const SYNTAX_MAX_SOURCE_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy)]
pub(super) struct ConversationRenderState<'a> {
    pub collapse_thinking: bool,
    pub thinking_streaming: bool,
    pub assistant_streaming: bool,
    pub now: Duration,
    /// Call id keyboard focus is standing on, when any.
    pub focused_call: Option<&'a str>,
    /// Glyph set this terminal can draw the transcript's chrome with.
    pub unicode: UnicodeLevel,
}

/// Identifies one settled conversation across frames.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct SettledConversation {
    pub generation: u64,
    pub transcript: crate::TranscriptId,
    pub index: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SettledConversationKey {
    conversation: SettledConversation,
    content_width: u16,
    collapse_thinking: bool,
    assistant_streaming: bool,
    display_modes: u64,
}

thread_local! {
    static SETTLED_CONVERSATION_CACHE: std::cell::RefCell<
        VecDeque<(SettledConversationKey, Arc<[Line<'static>]>)>,
    > = const { std::cell::RefCell::new(VecDeque::new()) };
}

/// Described lines for a conversation that can no longer change, reused across frames.
///
/// The transcript is rebuilt on every frame, so without this a long session pays
/// a full markdown parse and layout pass for its whole history on each animation
/// tick. A conversation only qualifies once nothing in it is still animating:
/// any unfinished tool call or subagent card keeps its rows tied to the frame
/// clock, and those are described live.
pub(super) fn settled_conversation_lines(
    identity: SettledConversation,
    conversation: &Conversation,
    tool_display_modes: &BTreeMap<String, DisplayMode>,
    content_width: u16,
    state: ConversationRenderState<'_>,
) -> Arc<[Line<'static>]> {
    if !is_settled(conversation) {
        return conversation_lines(conversation, &[], tool_display_modes, content_width, state)
            .into();
    }

    let mut hasher = DefaultHasher::new();
    for (call_id, mode) in tool_display_modes {
        call_id.hash(&mut hasher);
        std::mem::discriminant(mode).hash(&mut hasher);
    }
    // Focus paints a row, so a settled turn cached without it would keep
    // showing the mark after focus moved on.
    state.focused_call.hash(&mut hasher);
    let key = SettledConversationKey {
        conversation: identity,
        content_width,
        collapse_thinking: state.collapse_thinking,
        assistant_streaming: state.assistant_streaming,
        display_modes: hasher.finish(),
    };

    let cached = SETTLED_CONVERSATION_CACHE.with_borrow(|cache| {
        cache
            .iter()
            .find(|(entry, _)| *entry == key)
            .map(|(_, lines)| Arc::clone(lines))
    });
    if let Some(lines) = cached {
        return lines;
    }

    #[cfg(test)]
    SETTLED_CONVERSATION_RENDERS.with(|renders| renders.set(renders.get() + 1));

    let lines: Arc<[Line<'static>]> =
        conversation_lines(conversation, &[], tool_display_modes, content_width, state).into();
    SETTLED_CONVERSATION_CACHE.with_borrow_mut(|cache| {
        while cache.len() >= SETTLED_CONVERSATION_CACHE_MAX_ENTRIES {
            cache.pop_front();
        }
        cache.push_back((key, Arc::clone(&lines)));
    });

    lines
}

/// Whether every row of a conversation is frozen: no spinner, no live elapsed time.
fn is_settled(conversation: &Conversation) -> bool {
    conversation
        .subagent_cards
        .iter()
        .all(|card| card.status.is_some() && card.terminal_at.is_some())
        && conversation
            .tool_batches
            .iter()
            .flat_map(|batch| &batch.calls)
            .all(|call| call.result.is_some())
}

pub(super) fn conversation_lines(
    conversation: &Conversation,
    events: &[TuiRuntimeEvent],
    tool_display_modes: &BTreeMap<String, DisplayMode>,
    content_width: u16,
    state: ConversationRenderState<'_>,
) -> Vec<Line<'static>> {
    painted_conversation(
        conversation,
        events,
        tool_display_modes,
        content_width,
        state,
    )
    .lines
}

/// The same rows `conversation_lines` paints, each tagged with its owning call.
pub(super) fn conversation_call_rows(
    conversation: &Conversation,
    events: &[TuiRuntimeEvent],
    tool_display_modes: &BTreeMap<String, DisplayMode>,
    content_width: u16,
    state: ConversationRenderState<'_>,
) -> Vec<Option<String>> {
    painted_conversation(
        conversation,
        events,
        tool_display_modes,
        content_width,
        state,
    )
    .owners
}

fn painted_conversation(
    conversation: &Conversation,
    events: &[TuiRuntimeEvent],
    tool_display_modes: &BTreeMap<String, DisplayMode>,
    content_width: u16,
    state: ConversationRenderState<'_>,
) -> PaintedBlocks {
    let context = ItemContext {
        conversation,
        events,
        tool_display_modes,
        content_width: usize::from(content_width.max(1))
            .saturating_sub(ACCENT_WIDTH + GUTTER_WIDTH)
            .max(1),
        state,
    };
    let plan = plan_verb_groups(conversation, tool_display_modes);
    let mut blocks = Vec::new();

    for (index, item) in conversation.items.iter().enumerate() {
        if let Some(group) = plan.headers.get(&index) {
            blocks.push(verb_group_block(group));
            continue;
        }
        if plan.folded.contains(&index) {
            continue;
        }
        blocks.push(item_block(&context, item));
    }

    if let Some(rows) = turn_cost_rows(conversation) {
        blocks.push(RenderedBlock::plain(rows));
    }

    paint_blocks(blocks, state.now, state.unicode)
}

/// The closing row of a settled turn: what it took and what it billed.
///
/// A live turn already reports its elapsed time in the status row, which
/// disappears with the turn, so a settled turn keeps its own record rather
/// than leaving the reader with only the session-wide footer.
fn turn_cost_rows(conversation: &Conversation) -> Option<Vec<Line<'static>>> {
    if !conversation.is_settled() || conversation.cost.is_empty() {
        return None;
    }

    let cost = conversation.cost;
    let mut parts = Vec::new();
    if let Some(duration) = cost.duration {
        parts.push(elapsed_label(duration));
    }
    if let Some(input) = cost.input_tokens {
        parts.push(format!("{} in", compact_tokens(input)));
    }
    if let Some(output) = cost.output_tokens {
        parts.push(format!("{} out", compact_tokens(output)));
    }

    Some(vec![Line::from(Span::styled(
        format!("│ {}", parts.join(" · ")),
        Style::default().fg(RolePalette::muted()),
    ))])
}

fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}k tok", tokens as f64 / 1_000.0)
    } else {
        format!("{tokens} tok")
    }
}

/// Shared inputs every conversation item needs to describe its rows.
///
/// `content_width` is already reduced by [`ACCENT_WIDTH`] and [`GUTTER_WIDTH`],
/// so a block never has to know that the transcript reserves leading columns for
/// its accent bar and bullet.
struct ItemContext<'a> {
    conversation: &'a Conversation,
    events: &'a [TuiRuntimeEvent],
    tool_display_modes: &'a BTreeMap<String, DisplayMode>,
    content_width: usize,
    state: ConversationRenderState<'a>,
}

/// One conversation item's rows plus what the vertical-gap policy needs.
struct RenderedBlock {
    rows: Vec<BlockLine>,
    /// Whether the block is in its compact, single-summary form. A run of
    /// packing blocks renders with no blank row between them.
    packs: bool,
    /// Tool call this block belongs to, when any.
    call_id: Option<String>,
    /// Whether the block closes a call opened by an earlier block.
    closes_call: bool,
}

impl RenderedBlock {
    /// Block of rows that share the gutter without owning a bullet.
    fn plain(lines: Vec<Line<'static>>) -> Self {
        Self {
            rows: lines.into_iter().map(BlockLine::new).collect(),
            packs: false,
            call_id: None,
            closes_call: false,
        }
    }

    /// Block whose rows all carry the same accent bar and no bullet.
    fn accented(lines: Vec<Line<'static>>, accent: Option<RowAccent>) -> Self {
        Self {
            rows: lines
                .into_iter()
                .map(|line| BlockLine::new(line).accented(accent))
                .collect(),
            packs: false,
            call_id: None,
            closes_call: false,
        }
    }

    /// Block that renders nothing and is therefore transparent to the gap policy.
    fn hidden() -> Self {
        Self::plain(Vec::new())
    }
}

/// Metadata of the last painted block, kept so the gap policy can look back.
struct Neighbour {
    packs: bool,
    call_id: Option<String>,
}

/// Vertical rhythm of the transcript.
///
/// Consecutive compact blocks pack with no blank row; anything else is
/// separated by exactly one. A tool result never detaches from the call it
/// closes, even when the user expanded its body. Blocks that render nothing are
/// transparent, so a hidden item can neither force nor absorb a gap.
fn paint_blocks(blocks: Vec<RenderedBlock>, now: Duration, unicode: UnicodeLevel) -> PaintedBlocks {
    let mut lines = Vec::new();
    let mut owners = Vec::new();
    let mut previous: Option<Neighbour> = None;

    for block in blocks.into_iter().filter(|block| !block.rows.is_empty()) {
        if previous
            .as_ref()
            .is_some_and(|previous| separates(previous, &block))
        {
            lines.push(Line::default());
            owners.push(None);
        }
        let call_id = block.call_id;
        previous = Some(Neighbour {
            packs: block.packs,
            call_id: call_id.clone(),
        });
        for (row, line) in block.rows.into_iter().enumerate() {
            lines.push(painted_row(line, row, now, unicode));
            owners.push(call_id.clone());
        }
    }

    PaintedBlocks { lines, owners }
}

/// Painted rows and, per row, the tool call that owns it.
///
/// The ownership is kept beside the rows rather than derived from them later:
/// once a block becomes lines, nothing in a line says which call produced it,
/// and reconstructing that by reading the text back would be guessing.
pub(super) struct PaintedBlocks {
    pub lines: Vec<Line<'static>>,
    pub owners: Vec<Option<String>>,
}

fn separates(previous: &Neighbour, block: &RenderedBlock) -> bool {
    let closes_previous =
        block.closes_call && block.call_id.is_some() && block.call_id == previous.call_id;
    !closes_previous && !(previous.packs && block.packs)
}

/// Paints one row onto the shared gutter: its accent bar and bullet, or the same
/// width in blanks so content keeps a single column across every row type.
///
/// `row` is the row's index inside its own block, which is what makes the accent
/// wave travel down a block instead of blinking in unison.
fn painted_row(
    row: BlockLine,
    index: usize,
    now: Duration,
    unicode: UnicodeLevel,
) -> Line<'static> {
    if row.bullet.is_none()
        && row.accent.is_none()
        && row.line.spans.iter().all(|span| span.content.is_empty())
    {
        return Line::default();
    }

    let mut spans = vec![
        row.accent.map_or_else(
            || Span::raw(" ".repeat(ACCENT_WIDTH)),
            |accent| accent.span(index, now, unicode),
        ),
        row.bullet.map_or_else(
            || Span::raw(" ".repeat(GUTTER_WIDTH)),
            |bullet| bullet.span(unicode),
        ),
    ];
    spans.extend(row.line.spans);
    Line::from(spans)
}

/// Reserves the accent column on a transcript row that no conversation block
/// owns, so chrome rows keep the same content column as block rows.
pub(super) fn unaccented_row(line: Line<'static>) -> Line<'static> {
    if line.spans.iter().all(|span| span.content.is_empty()) {
        return line;
    }

    let mut spans = vec![Span::raw(" ".repeat(ACCENT_WIDTH))];
    spans.extend(line.spans);
    Line::from(spans)
}

/// Marks the header row of the block keyboard focus is standing on.
///
/// The mark is a repaint and never a character: the block has to look focused
/// without occupying a column it did not occupy before, or moving focus would
/// reflow the transcript under the reader's eyes.
fn mark_focused_row(rows: &mut [BlockLine]) {
    let Some(row) = rows.first_mut() else {
        return;
    };
    for span in &mut row.line.spans {
        span.style = span
            .style
            .fg(RolePalette::navigation())
            .add_modifier(Modifier::BOLD);
    }
}

/// The one row standing in for the settled turns the transcript folded away.
///
/// It carries its own key because it is the only row for which that key does
/// anything, and it exists only while something is actually hidden — so the
/// hint is never advertising a press that would do nothing.
pub(super) fn history_elision_row(
    elided: usize,
    row_width: u16,
    unicode: UnicodeLevel,
) -> Line<'static> {
    let noun = if elided == 1 { "turn" } else { "turns" };
    unaccented_row(Line::from(Span::styled(
        bounded_single_line(
            &format!(
                "{} {elided} earlier {noun} · ^Y to show",
                Glyph::Ellipsis.text(unicode)
            ),
            usize::from(row_width),
        ),
        Style::default().fg(RolePalette::muted()),
    )))
}

fn item_block(context: &ItemContext<'_>, item: &ConversationItem) -> RenderedBlock {
    match item {
        ConversationItem::Info(text) => {
            RenderedBlock::plain(label_lines("INFO", RolePalette::muted(), text))
        }
        ConversationItem::FailureNotice(text) => RenderedBlock::accented(
            labelled_lines(
                "NOTICE",
                RolePalette::error(),
                Style::default().fg(RolePalette::error()),
                text,
            ),
            Some(RowAccent::Still(RolePalette::error())),
        ),
        ConversationItem::User(text) => user_block(text, context.content_width),
        ConversationItem::Assistant(text) => assistant_block(
            text,
            context.content_width,
            !context.state.assistant_streaming,
        ),
        ConversationItem::Reasoning(text) => {
            let mut lines = Vec::new();
            let accent = thinking_lines(
                &mut lines,
                text,
                context.state.collapse_thinking,
                context.state.thinking_streaming,
                context.content_width,
            );
            RenderedBlock::accented(lines, accent)
        }
        ConversationItem::ToolCall {
            call_id,
            name,
            input,
            parsed,
            batch,
        } => tool_call_block(context, call_id, name, input, parsed, *batch),
        ConversationItem::ToolResult {
            call_id,
            output,
            is_error,
        } => tool_result_body_block(context, call_id, output, *is_error),
        ConversationItem::Diff {
            call_id,
            lines: diff,
        } => {
            let mut lines = Vec::new();
            render_diff(
                &mut lines,
                diff,
                diff_language(context.conversation, call_id),
                context.content_width,
            );
            RenderedBlock::plain(lines)
        }
        ConversationItem::Error(error) => {
            RenderedBlock::plain(error_lines(error, context.content_width))
        }
        ConversationItem::SubagentCard(id) => context
            .conversation
            .subagent_cards
            .iter()
            .find(|card| card.id == *id)
            .map_or_else(RenderedBlock::hidden, |card| {
                subagent_card_block(card, context.content_width, context.state.now)
            }),
    }
}

fn tool_call_block(
    context: &ItemContext<'_>,
    call_id: &str,
    name: &str,
    input: &str,
    parsed: &agens_core::ToolInput,
    batch: Option<usize>,
) -> RenderedBlock {
    if is_task_tool_name(name) {
        return RenderedBlock::hidden();
    }

    let result = call_result(context.conversation, call_id).map(|result| {
        let (result_state, duration) = tool_state(context.events, call_id, result.is_error);
        ToolResultBlock {
            status: format!("{result_state:?}{}", duration_label(duration)),
            failed: result_state == ToolResultState::Failure,
            size: result_size_label(&result.output),
        }
    });
    let block = ToolCallBlock {
        input,
        parsed,
        batch,
        content_width: context.content_width,
        state: call_row_state(context.conversation, call_id),
        result: result.as_ref(),
    };
    let mode = context
        .tool_display_modes
        .get(call_id)
        .copied()
        .unwrap_or_else(|| block.default_mode());

    let mut rows = block.lines(mode);
    if context.state.focused_call == Some(call_id) {
        mark_focused_row(&mut rows);
    }

    RenderedBlock {
        rows,
        // Any call showing a single summary row packs against its neighbours.
        // Packing used to require the call be verb-foldable, which is a
        // different question: an MCP call cannot fold into "Read files ×3" but
        // it is still one row, and a run of them separated by blank rows
        // spends half a screen saying nothing.
        packs: mode == DisplayMode::Collapsed || result.is_none(),
        call_id: Some(call_id.to_owned()),
        closes_call: false,
    }
}

/// Body rows a settled call contributes below its header row.
///
/// A body block only exists once the call carries a result, so an absent mode
/// entry has to resolve the same way `ToolCallBlock::default_mode` resolves it
/// for a settled call: hidden until the reader asks for it.
fn tool_result_body_block(
    context: &ItemContext<'_>,
    call_id: &str,
    output: &str,
    is_error: bool,
) -> RenderedBlock {
    if is_task_call(context.conversation, call_id) {
        return RenderedBlock::hidden();
    }

    let mode = context
        .tool_display_modes
        .get(call_id)
        .copied()
        .unwrap_or(DisplayMode::Collapsed);
    if mode == DisplayMode::Collapsed {
        return RenderedBlock::hidden();
    }

    let (result_state, _) = tool_state(context.events, call_id, is_error);
    let failed = result_state == ToolResultState::Failure;
    let body_width = context
        .content_width
        .saturating_sub(TOOL_BODY_INDENT)
        .max(1);
    let full_body = tool_result_body(call_id, output, body_width, failed);
    let body = match mode {
        DisplayMode::Collapsed => Vec::new(),
        DisplayMode::Truncated => crate::widgets::bounded_tool_preview(&full_body),
        DisplayMode::Expanded => full_body.to_vec(),
    };
    let accent = (!call_is_groupable(context.conversation, call_id))
        .then_some(RowAccent::Still(result_color(result_state)));

    RenderedBlock {
        rows: body
            .into_iter()
            .map(|line| BlockLine::new(indent_line(line, TOOL_BODY_INDENT)).accented(accent))
            .collect(),
        packs: false,
        call_id: Some(call_id.to_owned()),
        closes_call: true,
    }
}

fn assistant_block(text: &str, content_width: usize, highlight_syntax: bool) -> RenderedBlock {
    let mut lines = Vec::new();
    markdown_lines_with_syntax(
        &mut lines,
        text,
        Style::default().fg(RolePalette::assistant()),
        "",
        content_width,
        highlight_syntax,
    );
    let rows = lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                BlockLine::with_bullet(
                    line,
                    RowBullet::Identity(Glyph::AssistantBullet, RolePalette::assistant_identity()),
                )
            } else {
                BlockLine::new(line)
            }
        })
        .collect();

    RenderedBlock {
        rows,
        packs: false,
        call_id: None,
        closes_call: false,
    }
}

/// Fills a row's remaining width with the band background, so the prompt reads
/// as one block of colour rather than as a ragged right edge.
///
/// The band is what makes a turn findable without reading it. It is additive:
/// the identity rail and the `❯` bullet still carry the same meaning on a
/// terminal that renders no background at all.
fn band_row(line: Line<'static>, content_width: usize) -> Line<'static> {
    let padding = content_width.saturating_sub(line.width());
    if padding == 0 {
        return line;
    }

    let mut spans = line.spans;
    spans.push(Span::styled(
        " ".repeat(padding),
        Style::default().bg(RolePalette::user_band()),
    ));
    Line::from(spans)
}

/// Shifts a rendered row right, keeping any background it already carries.
fn indent_line(line: Line<'static>, columns: usize) -> Line<'static> {
    let style = line
        .spans
        .first()
        .and_then(|span| span.style.bg)
        .map_or_else(Style::default, |background| Style::default().bg(background));
    let mut spans = vec![Span::styled(" ".repeat(columns), style)];
    spans.extend(line.spans);
    Line::from(spans)
}

/// The user's own prompt, wrapped here rather than left to the transcript
/// widget.
///
/// A block that hands over-wide rows to the widget gets them wrapped without
/// its accent bar or its gutter, so the continuation rows fall out of the
/// turn's identity rail. Wrapping at the block's own width keeps every row
/// inside the block that owns it.
fn user_block(text: &str, content_width: usize) -> RenderedBlock {
    let mut rows = Vec::new();
    let accent = Some(RowAccent::Still(RolePalette::user_identity()));
    let style = Style::default()
        .fg(RolePalette::assistant())
        .bg(RolePalette::user_band())
        .add_modifier(Modifier::BOLD);

    for source_line in text.split('\n') {
        let wrapped = wrap_styled_spans(
            vec![Span::styled(source_line.to_owned(), style)],
            content_width,
            &[],
        );
        for line in wrapped
            .into_iter()
            .map(|line| band_row(line, content_width))
        {
            rows.push(
                if rows.is_empty() {
                    BlockLine::with_bullet(
                        line,
                        RowBullet::Identity(Glyph::UserBullet, RolePalette::user_identity()),
                    )
                } else {
                    BlockLine::new(line)
                }
                .accented(accent),
            );
        }
    }

    RenderedBlock {
        rows,
        packs: false,
        call_id: None,
        closes_call: false,
    }
}

/// The error card, wrapped so that every row of the message keeps the `│`
/// gutter that makes it part of the card.
fn error_lines(error: &crate::ActionableError, content_width: usize) -> Vec<Line<'static>> {
    let gutter_style = Style::default().fg(RolePalette::error());
    let gutter = vec![Span::styled("│ ", gutter_style)];
    let action_style = Style::default().fg(RolePalette::muted());

    let mut lines = vec![Line::from(Span::styled(
        "┌ Error",
        gutter_style.add_modifier(Modifier::BOLD),
    ))];

    for segment in error.message.split('\n') {
        let mut spans = gutter.clone();
        spans.push(Span::raw(segment.to_owned()));
        lines.extend(wrap_styled_spans(spans, content_width, &gutter));
    }

    lines.extend(wrap_styled_spans(
        vec![Span::styled(
            format!("└ Action: {}", error.action),
            action_style,
        )],
        content_width,
        &[Span::styled("  ", action_style)],
    ));

    lines
}

/// Inputs for the inline working indicator closing an active turn's transcript.
pub(super) struct TurnStatus<'a> {
    pub label: &'a str,
    pub now: Duration,
    pub elapsed: Option<Duration>,
    pub tokens: Option<u64>,
}

/// Working indicator rendered as the last row of an active turn's transcript.
///
/// Spinner and activity label sit on the left; elapsed time and the real
/// provider turn-token count are right-aligned. Token counts are never
/// fabricated — the field is simply absent until the provider reports usage.
/// The metadata keeps its columns and the label yields, so a narrow terminal
/// elides the activity name instead of pushing the row past the transcript.
pub(super) fn turn_status_line(status: TurnStatus<'_>, content_width: usize) -> Line<'static> {
    let mut left = StatusGlyph::decorate_status(true, status.label, status.now);
    let mut right = String::new();
    if let Some(elapsed) = status.elapsed {
        right.push_str(&elapsed_label(elapsed));
    }
    if let Some(tokens) = status.tokens {
        if !right.is_empty() {
            right.push_str(" · ");
        }
        right.push_str(&compact_tokens(tokens));
    }
    // Stopping is the one thing a reader may want from a turn while it runs, so
    // the row that reports the turn is where its key belongs.
    if !right.is_empty() {
        right.push_str(" · ");
    }
    right.push_str("esc stop");

    // The metadata never takes the whole row: the label keeps at least the
    // spinner cell and the gap that separates the two halves.
    let right = bounded_single_line(&right, content_width.saturating_sub(2));
    let label_budget = if right.is_empty() {
        content_width
    } else {
        content_width.saturating_sub(right.width().saturating_add(1))
    };
    left = bounded_single_line(&left, label_budget);

    let mut spans = vec![Span::styled(
        left.clone(),
        Style::default()
            .fg(RolePalette::running())
            .add_modifier(Modifier::BOLD),
    )];
    if !right.is_empty() {
        let used = left.width().saturating_add(right.width());
        let padding = content_width.saturating_sub(used).max(1);
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::styled(
            right,
            Style::default().fg(RolePalette::muted()),
        ));
    }
    Line::from(spans)
}

/// A duration a reader can size up without counting digits.
///
/// Past a minute the seconds stop being a quantity anyone reads — `253s` is
/// arithmetic, `4m 13s` is a fact — so the label changes unit rather than
/// growing.
pub(super) fn elapsed_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 3_600 {
        let minutes = (seconds % 3_600) / 60;
        format!("{}h {minutes}m", seconds / 3_600)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else if seconds > 0 {
        format!("{seconds}s")
    } else {
        format!("{}ms", elapsed.as_millis())
    }
}

/// A run of consecutive tool items folded behind one tense-aware header.
struct FoldedGroup {
    verb: VerbGroup,
    count: usize,
    failed: usize,
    running: bool,
}

impl FoldedGroup {
    const fn state(&self) -> RowState {
        if self.running {
            RowState::Running
        } else if self.failed > 0 {
            RowState::Failure
        } else {
            RowState::Success
        }
    }
}

#[derive(Default)]
struct FoldPlan {
    headers: BTreeMap<usize, FoldedGroup>,
    folded: BTreeSet<usize>,
}

/// Pure layout pass folding consecutive collapsed read-family calls into groups.
///
/// The plan is recomputed per frame rather than stored, so it can never drift
/// from the display modes and conversation it summarizes. A call folds while it
/// is still running (the group then reads in present tense) and once it settles
/// into `Collapsed`; any other mode means the user asked to see the individual
/// rows, so the run is rendered unfolded.
fn plan_verb_groups(
    conversation: &Conversation,
    tool_display_modes: &BTreeMap<String, DisplayMode>,
) -> FoldPlan {
    let mut plan = FoldPlan::default();
    let mut index = 0usize;

    while index < conversation.items.len() {
        match collect_group(conversation, tool_display_modes, index) {
            Some((group, end)) => {
                plan.folded.extend(index..end);
                plan.headers.insert(index, group);
                index = end;
            }
            None => index += 1,
        }
    }

    plan
}

const MIN_GROUP_CALLS: usize = 2;

fn collect_group(
    conversation: &Conversation,
    tool_display_modes: &BTreeMap<String, DisplayMode>,
    start: usize,
) -> Option<(FoldedGroup, usize)> {
    let verb = foldable_call(tool_display_modes, conversation.items.get(start)?)?;
    let mut members = BTreeSet::new();
    let mut count = 0usize;
    let mut failed = 0usize;
    let mut running = false;
    let mut end = start;

    for (offset, item) in conversation.items.iter().enumerate().skip(start) {
        match item {
            ConversationItem::ToolCall { name, .. } if is_task_tool_name(name) => {}
            ConversationItem::ToolCall { call_id, .. } => {
                if foldable_call(tool_display_modes, item) != Some(verb) {
                    break;
                }
                members.insert(call_id.as_str());
                count += 1;
                failed += usize::from(call_row_state(conversation, call_id) == RowState::Failure);
                running |= call_row_state(conversation, call_id) == RowState::Running;
            }
            ConversationItem::ToolResult { call_id, .. }
                if members.contains(call_id.as_str()) || is_task_call(conversation, call_id) => {}
            _ => break,
        }
        end = offset + 1;
    }

    (count >= MIN_GROUP_CALLS).then_some((
        FoldedGroup {
            verb,
            count,
            failed,
            running,
        },
        end,
    ))
}

fn foldable_call(
    tool_display_modes: &BTreeMap<String, DisplayMode>,
    item: &ConversationItem,
) -> Option<VerbGroup> {
    let ConversationItem::ToolCall {
        call_id, parsed, ..
    } = item
    else {
        return None;
    };
    let verb = VerbGroup::of(parsed)?;

    // An absent mode is not a state of its own: nothing recorded means nobody
    // expanded this call, so it folds. The arm cannot compare against
    // `default_mode()` instead — a running call defaults to `Truncated`
    // precisely so live work keeps a preview when it stands alone, and that
    // must not stop it from folding into a group.
    let folds = match tool_display_modes.get(call_id) {
        Some(mode) => *mode == DisplayMode::Collapsed,
        None => true,
    };
    folds.then_some(verb)
}

fn call_result<'a>(conversation: &'a Conversation, call_id: &str) -> Option<&'a crate::ToolResult> {
    conversation
        .tool_batches
        .iter()
        .flat_map(|batch| &batch.calls)
        .find(|call| call.call_id == call_id)
        .and_then(|call| call.result.as_ref())
}

fn call_is_groupable(conversation: &Conversation, call_id: &str) -> bool {
    conversation
        .tool_batches
        .iter()
        .flat_map(|batch| &batch.calls)
        .find(|call| call.call_id == call_id)
        .is_some_and(|call| VerbGroup::of(&call.parsed).is_some())
}

/// Lifecycle a call's bullet carries: pending, or settled by its own result.
fn call_row_state(conversation: &Conversation, call_id: &str) -> RowState {
    match call_result(conversation, call_id) {
        None if conversation.is_settled() => RowState::Failure,
        None => RowState::Running,
        Some(result) if result.is_error => RowState::Failure,
        Some(_) => RowState::Success,
    }
}

/// Header row standing in for a folded run of read-family calls.
///
/// A settled fold is the transcript's only collapsed groupable row, so it wears
/// the dimmed thin bar; while the run is still live the bar breathes instead.
fn verb_group_block(group: &FoldedGroup) -> RenderedBlock {
    let mut spans = vec![Span::styled(
        group.verb.label(group.count, group.running),
        Style::default()
            .fg(RolePalette::assistant())
            .add_modifier(Modifier::BOLD),
    )];
    if group.failed > 0 {
        spans.push(Span::styled(
            format!(" · {} failed", group.failed),
            Style::default().fg(RolePalette::error()),
        ));
    }
    if !group.running {
        spans.push(Span::styled(
            " · Ctrl+O to expand",
            Style::default().fg(RolePalette::muted()),
        ));
    }

    let accent = if group.running {
        RowAccent::Wave(RolePalette::running())
    } else {
        RowAccent::Collapsed(group.state().color())
    };

    RenderedBlock {
        rows: vec![
            BlockLine::with_bullet(Line::from(spans), RowBullet::Group(group.state()))
                .accented(Some(accent)),
        ],
        packs: true,
        call_id: None,
        closes_call: false,
    }
}

const TOOL_BODY_CACHE_MAX_ENTRIES: usize = 64;

struct ToolBodyKey {
    call_id: String,
    content_width: usize,
    output_hash: u64,
    failed: bool,
}

thread_local! {
    static TOOL_BODY_CACHE: std::cell::RefCell<VecDeque<(ToolBodyKey, Arc<[Line<'static>]>)>> =
        const { std::cell::RefCell::new(VecDeque::new()) };
}

/// Described lines for a tool result body, reused while the body is unchanged.
///
/// Keyed by call, content width, output hash and outcome, so an idle block is
/// described once instead of once per animation tick. Caching is safe here
/// because the description is a pure function of those inputs, so no partially
/// described frame can be frozen into the cache.
///
/// A failed body is painted in the error colour: muting it would make the text
/// the reader most needs the least readable thing on the row, and would leave
/// failure and success indistinguishable below the header.
fn tool_result_body(
    call_id: &str,
    output: &str,
    content_width: usize,
    failed: bool,
) -> Arc<[Line<'static>]> {
    let mut hasher = DefaultHasher::new();
    output.hash(&mut hasher);
    let output_hash = hasher.finish();

    let cached = TOOL_BODY_CACHE.with_borrow_mut(|cache| {
        cache
            .iter()
            .find(|(key, _)| {
                key.output_hash == output_hash
                    && key.content_width == content_width
                    && key.failed == failed
                    && key.call_id == call_id
            })
            .map(|(_, body)| Arc::clone(body))
    });
    if let Some(body) = cached {
        return body;
    }

    #[cfg(test)]
    TOOL_BODY_RENDERS.with(|renders| renders.set(renders.get() + 1));

    let color = if failed {
        RolePalette::error()
    } else {
        RolePalette::muted()
    };
    let described = tool_output_lines(
        &bounded_visible_tool_output(output),
        Style::default().fg(color),
        content_width,
    );
    let body: Arc<[Line<'static>]> = Arc::from(described);

    TOOL_BODY_CACHE.with_borrow_mut(|cache| {
        cache.retain(|(key, _)| key.call_id != call_id);
        while cache.len() >= TOOL_BODY_CACHE_MAX_ENTRIES {
            cache.pop_front();
        }
        cache.push_back((
            ToolBodyKey {
                call_id: call_id.to_owned(),
                content_width,
                output_hash,
                failed,
            },
            Arc::clone(&body),
        ));
    });

    body
}

/// Columns a tab advances to. Four keeps a narrow transcript readable where the
/// terminal's traditional eight would spend most of the width on indentation.
const TERMINAL_TAB_WIDTH: usize = 4;

/// Describes tool output as terminal text rather than as prose.
///
/// A command's stdout or a file's contents is what something already printed,
/// so no Markdown structure is inferred from it: an indented line keeps its
/// indentation, a `#` line stays a comment, and a `---` line stays three
/// dashes. ANSI colour and attributes the command emitted become styles; every
/// other escape sequence and control character is dropped instead of painted.
/// A row wider than the transcript is wrapped, never clipped, so no output ever
/// disappears off the right edge.
fn tool_output_lines(output: &str, base_style: Style, content_width: usize) -> Vec<Line<'static>> {
    let max_width = content_width.max(1);

    terminal_rows(output, base_style)
        .into_iter()
        .flat_map(|row| wrap_terminal_row(row, max_width))
        .collect()
}

/// Splits terminal text into unwrapped rows of styled spans.
///
/// Styling set by an SGR sequence carries across a newline, the way a terminal
/// keeps it until the stream itself resets it. A trailing newline ends the last
/// row instead of opening an empty one, matching [`str::lines`].
fn terminal_rows(output: &str, base_style: Style) -> Vec<Vec<Span<'static>>> {
    let mut rows = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = base_style;
    let mut column = 0_usize;
    let mut characters = output.chars();

    while let Some(character) = characters.next() {
        match character {
            '\n' => {
                rows.push(std::mem::take(&mut spans));
                column = 0;
            }
            '\u{1b}' => style = escaped_style(&mut characters, style, base_style),
            '\t' => {
                let advance = TERMINAL_TAB_WIDTH - column % TERMINAL_TAB_WIDTH;
                push_styled_text(&mut spans, &" ".repeat(advance), style);
                column = column.saturating_add(advance);
            }
            control if control.is_control() => {}
            visible => {
                let mut encoded = [0_u8; 4];
                push_styled_text(&mut spans, visible.encode_utf8(&mut encoded), style);
                column = column.saturating_add(visible.width().unwrap_or_default());
            }
        }
    }

    if !spans.is_empty() {
        rows.push(spans);
    }

    rows
}

/// Consumes one escape sequence and reports the style that follows it.
///
/// Only SGR is interpreted. A control sequence with any other final byte, and
/// an operating-system command with its own string terminator, are consumed so
/// their bytes never reach a span, and leave the current style untouched.
fn escaped_style(characters: &mut std::str::Chars<'_>, style: Style, base_style: Style) -> Style {
    match characters.next() {
        Some('[') => {
            let mut parameters = String::new();
            let mut final_byte = None;
            for character in characters.by_ref() {
                if matches!(character, '\u{40}'..='\u{7e}') {
                    final_byte = Some(character);
                    break;
                }
                parameters.push(character);
            }
            if final_byte == Some('m') {
                sgr_style(&parameters, style, base_style)
            } else {
                style
            }
        }
        Some(']') => {
            let mut escaped = false;
            for character in characters.by_ref() {
                if character == '\u{7}' || (escaped && character == '\\') {
                    break;
                }
                escaped = character == '\u{1b}';
            }
            style
        }
        _ => style,
    }
}

/// Applies one SGR parameter list to the current style.
///
/// An empty list is a reset, as the specification requires, and a parameter the
/// transcript cannot express is ignored rather than approximated.
fn sgr_style(parameters: &str, style: Style, base_style: Style) -> Style {
    if parameters.is_empty() {
        return base_style;
    }

    let codes = parameters
        .split(';')
        .map(|parameter| parameter.parse::<u16>().unwrap_or_default())
        .collect::<Vec<_>>();
    let mut style = style;
    let mut index = 0_usize;

    while let Some(&code) = codes.get(index) {
        match code {
            0 => style = base_style,
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            7 => style = style.add_modifier(Modifier::REVERSED),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            29 => style = style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style = style.fg(ansi_color(code - 30)),
            38 => {
                if let Some(color) = extended_ansi_color(&codes, &mut index) {
                    style = style.fg(color);
                }
            }
            39 => style.fg = base_style.fg,
            40..=47 => style = style.bg(ansi_color(code - 40)),
            48 => {
                if let Some(color) = extended_ansi_color(&codes, &mut index) {
                    style = style.bg(color);
                }
            }
            49 => style.bg = base_style.bg,
            90..=97 => style = style.fg(ansi_color(code - 90 + 8)),
            100..=107 => style = style.bg(ansi_color(code - 100 + 8)),
            _ => {}
        }
        index = index.saturating_add(1);
    }

    style
}

/// Reads the colour a `38` or `48` parameter introduces, advancing `index` past
/// the parameters it consumed.
fn extended_ansi_color(codes: &[u16], index: &mut usize) -> Option<Color> {
    let parameter = |offset: usize| codes.get(index.saturating_add(offset)).copied();

    match parameter(1)? {
        5 => {
            let color = ansi_color(parameter(2)?);
            *index = index.saturating_add(2);
            Some(color)
        }
        2 => {
            let color = Color::Rgb(
                ansi_byte(parameter(2)?),
                ansi_byte(parameter(3)?),
                ansi_byte(parameter(4)?),
            );
            *index = index.saturating_add(4);
            Some(color)
        }
        _ => None,
    }
}

/// Palette entry `index`, resolved by the terminal's own theme.
fn ansi_color(index: u16) -> Color {
    Color::Indexed(ansi_byte(index))
}

fn ansi_byte(value: u16) -> u8 {
    u8::try_from(value).unwrap_or_default()
}

/// Hard-wraps one terminal row at `max_width`, preserving every column.
///
/// Alignment carries meaning in tool output, so a row breaks at the column it
/// runs out of room in rather than at a word boundary, and the remainder
/// continues on the next row instead of being clipped away.
fn wrap_terminal_row(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Line<'static>> {
    let width = spans.iter().map(|span| span.content.width()).sum::<usize>();
    if width <= max_width {
        return vec![Line::from(spans)];
    }

    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0_usize;

    for span in spans {
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = grapheme.width();
            if used.saturating_add(grapheme_width) > max_width && !current.is_empty() {
                lines.push(Line::from(std::mem::take(&mut current)));
                used = 0;
            }

            push_styled_text(&mut current, grapheme, span.style);
            used = used.saturating_add(grapheme_width);
        }
    }

    if !current.is_empty() {
        lines.push(Line::from(current));
    }

    lines
}

fn subagent_card_block(
    card: &crate::SubagentCard,
    content_width: usize,
    now: Duration,
) -> RenderedBlock {
    let mut rows = Vec::new();
    let agent = bounded_single_line(&title_case(&card.agent), content_width);
    const TITLE_SEPARATOR: &str = " · ";
    let summary_width = content_width
        .saturating_sub(agent.width())
        .saturating_sub(TITLE_SEPARATOR.width());
    let summary = compact_task_title(&card.task_summary, summary_width);
    let mut title = vec![Span::styled(
        agent,
        Style::default()
            .fg(RolePalette::assistant_identity())
            .add_modifier(Modifier::BOLD),
    )];
    if !summary.is_empty() {
        title.push(Span::styled(
            format!("{TITLE_SEPARATOR}{summary}"),
            Style::default().fg(RolePalette::assistant()),
        ));
    }
    rows.push(BlockLine::with_bullet(
        Line::from(title),
        RowBullet::Identity(Glyph::AssistantBullet, subagent_status_color(card.status)),
    ));

    let status = match card.status {
        Some(agens_core::SubagentStatus::Success) => "Success",
        Some(agens_core::SubagentStatus::Failure) => "Failure",
        Some(agens_core::SubagentStatus::Cancelled) => "Cancelled",
        None if card.has_activity => "Running",
        None => "Initializing",
    };
    let presentation = if card.status.is_some() {
        "recent"
    } else {
        match card.presentation {
            crate::TuiExecutionState::ForegroundRunning => "foreground",
            crate::TuiExecutionState::BackgroundRunning => "background",
            _ => "recent",
        }
    };
    let elapsed = card
        .started_at
        .map(|started| card.terminal_at.unwrap_or(now).saturating_sub(started));
    let mut meta = format!("{status} · {presentation}{}", duration_label(elapsed));
    // What the delegation actually runs on. A subagent routinely differs from
    // its parent in model and effort, and that difference explains both its
    // speed and its answers.
    if let Some(model) = card.model.as_deref() {
        meta.push_str(&format!(" · {model}"));
    }
    if let Some(effort) = card.effort.as_deref() {
        meta.push_str(&format!(" · {effort}"));
    }
    rows.push(BlockLine::new(Line::from(Span::styled(
        bounded_single_line(&meta, content_width),
        Style::default().fg(RolePalette::muted()),
    ))));
    rows.extend(subagent_failure_row(card, content_width));

    for activity in card.activities.iter().take(3) {
        rows.push(BlockLine::new(Line::from(Span::styled(
            bounded_single_line(&format!("· {activity}"), content_width),
            Style::default().fg(RolePalette::muted()),
        ))));
    }
    let hidden = card.tool_uses.saturating_sub(card.activities.len().min(3));
    if hidden > 0 {
        let noun = if hidden == 1 {
            "activity"
        } else {
            "activities"
        };
        rows.push(BlockLine::with_bullet(
            Line::from(Span::styled(
                bounded_single_line(&format!("+{hidden} more {noun}"), content_width),
                Style::default().fg(RolePalette::muted()),
            )),
            RowBullet::Group(RowState::Muted),
        ));
    }

    RenderedBlock {
        rows,
        packs: false,
        call_id: None,
        closes_call: false,
    }
}

/// The reason a delegated run failed, as the single row a card can spend on it.
///
/// Without it a failed card reads `Failure · recent · 42s` and nothing else:
/// the outcome is named but the only part the reader can act on is withheld,
/// even though the projection has carried it since the run ended.
fn subagent_failure_row(card: &crate::SubagentCard, content_width: usize) -> Option<BlockLine> {
    if !matches!(card.status, Some(agens_core::SubagentStatus::Failure)) {
        return None;
    }

    let reason = bounded_single_line(card.final_result.as_deref()?, content_width);

    (!reason.is_empty()).then(|| {
        BlockLine::new(Line::from(Span::styled(
            reason,
            Style::default().fg(RolePalette::error()),
        )))
    })
}

fn subagent_status_color(status: Option<agens_core::SubagentStatus>) -> Color {
    match status {
        Some(agens_core::SubagentStatus::Success) => RolePalette::success(),
        Some(agens_core::SubagentStatus::Failure) => RolePalette::error(),
        Some(agens_core::SubagentStatus::Cancelled) => RolePalette::warning(),
        None => RolePalette::running(),
    }
}

fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

/// Trailing characters a cut must not leave dangling in front of the ellipsis.
const DANGLING_CUT_CHARACTERS: [char; 4] = [' ', '·', ',', ';'];

/// `value` collapsed onto one line and bounded to `max_width` display columns.
///
/// A value that does not fit is cut on a word boundary and marked with an
/// ellipsis, so a row the painter cannot wrap never breaks mid-word and never
/// spills past the columns it was given. A single word wider than the whole
/// budget is the only case that still cuts inside a word.
pub(super) fn bounded_single_line(value: &str, max_width: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.width() <= max_width {
        return normalized;
    }
    if max_width == 0 {
        return String::new();
    }

    let budget = max_width - 1;
    let kept = word_prefix(&normalized, budget);
    let kept = if kept.is_empty() {
        take_visible_width(&normalized, budget)
    } else {
        kept
    };

    elided(&kept)
}

/// Compact title for a task description that may be a whole delegated prompt.
///
/// The first sentence stands in for the prompt, because a summary the reader can
/// scan beats a prefix of the full instruction. A title is prose, so it is cut
/// only between words: when not even the first word fits, the card shows no
/// summary rather than a word fragment.
fn compact_task_title(value: &str, max_width: usize) -> String {
    let sentence = first_sentence(value).split_whitespace().collect::<Vec<_>>();
    let normalized = sentence.join(" ");
    if normalized.width() <= max_width {
        return normalized;
    }
    if max_width == 0 {
        return String::new();
    }

    let kept = word_prefix(&normalized, max_width - 1);
    if kept.is_empty() {
        return String::new();
    }
    elided(&kept)
}

/// Longest whole-word prefix of `value` fitting `max_width` display columns.
fn word_prefix(value: &str, max_width: usize) -> String {
    let mut kept = String::new();
    for word in value.split(' ') {
        let separator = usize::from(!kept.is_empty());
        if kept.width() + separator + word.width() > max_width {
            break;
        }
        if separator == 1 {
            kept.push(' ');
        }
        kept.push_str(word);
    }
    kept
}

/// `kept` marked as a cut, without a dangling separator in front of the ellipsis.
fn elided(kept: &str) -> String {
    let mut marked = kept.trim_end_matches(DANGLING_CUT_CHARACTERS).to_owned();
    marked.push('…');
    marked
}

/// Leading sentence of `value`, or all of it when it declares no sentence end.
///
/// A terminator only counts when whitespace or the end of the text follows it,
/// so a version, a decimal or a dotted path does not end a sentence.
fn first_sentence(value: &str) -> &str {
    for (index, character) in value.char_indices() {
        if !matches!(character, '.' | '!' | '?') {
            continue;
        }
        let rest = value
            .get(index + character.len_utf8()..)
            .unwrap_or_default();
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return value.get(..index).unwrap_or(value);
        }
    }
    value
}

fn is_task_tool_name(name: &str) -> bool {
    matches!(
        name,
        "task"
            | "native::task"
            | "task_control"
            | "native::task_control"
            | "task_message"
            | "native::task_message"
    )
}

fn is_task_call(conversation: &Conversation, call_id: &str) -> bool {
    conversation
        .tool_batches
        .iter()
        .flat_map(|batch| &batch.calls)
        .any(|call| call.call_id == call_id && is_task_tool_name(&call.name))
}

fn bounded_visible_tool_output(output: &str) -> String {
    if output.len() <= MAX_VISIBLE_TOOL_OUTPUT_BYTES {
        return output.to_owned();
    }

    let content_limit =
        MAX_VISIBLE_TOOL_OUTPUT_BYTES.saturating_sub(VISIBLE_TOOL_OUTPUT_MARKER.len());
    let mut end = content_limit;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}{}", &output[..end], VISIBLE_TOOL_OUTPUT_MARKER)
}

/// Describes the reasoning rows and reports the accent bar they carry.
fn thinking_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    collapsed: bool,
    streaming: bool,
    content_width: usize,
) -> Option<RowAccent> {
    let mode = ThinkingBlock::mode(streaming, collapsed);
    if mode.shows_body() {
        lines.push(ThinkingBlock::title());
        markdown_lines(
            lines,
            text,
            Style::default().fg(RolePalette::muted()),
            "",
            content_width,
        );
    } else {
        lines.push(ThinkingBlock::collapsed_title(None));
    }
    ThinkingBlock::accent(mode)
}

pub(super) fn detail_lines(
    events: &[TuiRuntimeEvent],
    conversation_is_authoritative: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let task_calls = events
        .iter()
        .filter_map(|event| match event {
            TuiRuntimeEvent::ToolStarted { call_id, name, .. } if is_task_tool_name(name) => {
                Some(call_id.as_str())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();

    for event in events {
        match event {
            TuiRuntimeEvent::ToolStarted {
                call_id,
                name,
                input,
                ..
            } if !conversation_is_authoritative && !is_task_tool_name(name) => line(
                &mut lines,
                "TOOLS",
                RolePalette::muted(),
                format!("┌ {call_id} {name}\n  input: {input}"),
            ),
            TuiRuntimeEvent::ToolEnded {
                call_id,
                duration,
                result,
            } if !conversation_is_authoritative && !task_calls.contains(call_id.as_str()) => line(
                &mut lines,
                "TOOLS",
                result_color(*result),
                format!("└ {call_id} {result:?}{}", duration_label(*duration)),
            ),
            TuiRuntimeEvent::Diff {
                call_id,
                lines: diff,
            } if !conversation_is_authoritative => {
                line(
                    &mut lines,
                    "DIFF",
                    RolePalette::muted(),
                    format!("{call_id}:"),
                );
                for change in diff {
                    diff_line(&mut lines, change.number, change.kind, &change.text);
                }
            }
            TuiRuntimeEvent::TurnStarted
            | TuiRuntimeEvent::TurnEnded { .. }
            | TuiRuntimeEvent::Usage(_)
            | TuiRuntimeEvent::ToolStarted { .. }
            | TuiRuntimeEvent::ToolEnded { .. }
            | TuiRuntimeEvent::Diff { .. }
            | TuiRuntimeEvent::TaskExecution { .. }
            | TuiRuntimeEvent::SubagentExecution(_)
            | TuiRuntimeEvent::RestoredCompletedSubagent { .. }
            | TuiRuntimeEvent::Notice { .. } => {}
        }
    }

    lines
}

fn markdown_lines(
    lines: &mut Vec<Line<'static>>,
    markdown: &str,
    base_style: Style,
    prefix: &str,
    content_width: usize,
) {
    markdown_lines_with_syntax(lines, markdown, base_style, prefix, content_width, true);
}

fn markdown_lines_with_syntax(
    lines: &mut Vec<Line<'static>>,
    markdown: &str,
    base_style: Style,
    prefix: &str,
    content_width: usize,
    highlight_syntax: bool,
) {
    if markdown.is_empty() {
        return;
    }

    let renderer = if highlight_syntax {
        MarkdownRenderer::new(base_style, prefix, content_width)
    } else {
        MarkdownRenderer::with_syntax_highlighting(base_style, prefix, content_width, false)
    };
    lines.extend(renderer.render(markdown));
}

struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    base_style: Style,
    prefix: String,
    strong: usize,
    strikethrough: usize,
    emphasis: usize,
    heading: Option<HeadingLevel>,
    code_block: bool,
    code_panel_line: bool,
    code_panel_width: usize,
    code_panel_widths: VecDeque<usize>,
    code_language: Option<String>,
    content_width: usize,
    highlight_syntax: bool,
    quote_depth: usize,
    lists: Vec<Option<u64>>,
    links: Vec<String>,
    table_layouts: VecDeque<Vec<usize>>,
    table_columns: Vec<usize>,
    table_row: bool,
    table_head: bool,
    table_column: usize,
    table_cell_start: usize,
    table_row_start: usize,
    list_item_indent: Option<usize>,
}

impl MarkdownRenderer {
    fn new(base_style: Style, prefix: &str, content_width: usize) -> Self {
        Self::with_syntax_highlighting(base_style, prefix, content_width, true)
    }

    fn with_syntax_highlighting(
        base_style: Style,
        prefix: &str,
        content_width: usize,
        highlight_syntax: bool,
    ) -> Self {
        Self {
            lines: Vec::new(),
            spans: Vec::new(),
            base_style,
            prefix: prefix.to_owned(),
            strong: 0,
            strikethrough: 0,
            emphasis: 0,
            heading: None,
            code_block: false,
            code_panel_line: false,
            code_panel_width: content_width.max(1),
            code_panel_widths: VecDeque::new(),
            code_language: None,
            content_width: content_width.max(1),
            highlight_syntax,
            quote_depth: 0,
            lists: Vec::new(),
            links: Vec::new(),
            table_layouts: VecDeque::new(),
            table_columns: Vec::new(),
            table_row: false,
            table_head: false,
            table_column: 0,
            table_cell_start: 0,
            table_row_start: 0,
            list_item_indent: None,
        }
    }

    fn render(mut self, markdown: &str) -> Vec<Line<'static>> {
        self.code_panel_widths = code_panel_widths(markdown, self.content_width);
        let table_width = self.content_width.saturating_sub(self.prefix.width());
        self.table_layouts = markdown_table_layouts(markdown, table_width);
        for event in Parser::new_ext(
            markdown,
            Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
        ) {
            self.event(event);
        }
        self.finish_line();
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) if self.code_block => self.code_text(&text),
            Event::Text(text) => self.text(&text, self.current_style()),
            Event::Html(html) | Event::InlineHtml(html) => {
                let text = strip_html_tags(&html);
                if !text.trim().is_empty() {
                    self.text(&text, self.current_style());
                }
            }
            Event::Code(code) if self.code_block => self.code_text(&code),
            Event::Code(code) => self.text(
                &code,
                self.current_style()
                    .fg(RolePalette::markdown_code())
                    .bg(code_block_background())
                    .add_modifier(Modifier::BOLD),
            ),
            // A soft break is where the author's source wrapped, not where the
            // paragraph breaks. Honouring it would keep the model's own column
            // width and leave the terminal's unused, so the paragraph reflows
            // and only an explicit hard break ends the line.
            Event::SoftBreak if !self.code_block => self.text(" ", self.current_style()),
            Event::SoftBreak | Event::HardBreak => self.finish_line(),
            Event::Rule => {
                self.finish_line();
                self.text(
                    "────────────────",
                    self.base_style.fg(RolePalette::chrome()),
                );
                self.finish_line();
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                if let Some(indent) = self.list_item_indent.as_mut() {
                    *indent = indent.saturating_add(marker.width());
                }
                self.text(
                    marker,
                    self.base_style
                        .fg(if checked {
                            RolePalette::success()
                        } else {
                            RolePalette::navigation()
                        })
                        .add_modifier(Modifier::BOLD),
                );
            }
            Event::InlineMath(text) | Event::DisplayMath(text) | Event::FootnoteReference(text) => {
                self.text(&text, self.current_style())
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.finish_line();
                self.heading = Some(level);
            }
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strikethrough += 1,
            Tag::Emphasis => self.emphasis += 1,
            Tag::BlockQuote(_) => self.quote_depth += 1,
            Tag::List(start) => self.lists.push(start),
            Tag::Item => {
                self.finish_line();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(next)) => {
                        let marker = format!("{next}. ");
                        *next += 1;
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                let marker = format!("{}{marker}", "  ".repeat(depth));
                self.list_item_indent = Some(marker.width());
                self.text(
                    &marker,
                    self.base_style
                        .fg(RolePalette::navigation())
                        .add_modifier(Modifier::BOLD),
                );
            }
            Tag::CodeBlock(kind) => {
                self.finish_line();
                // Header is written before code_block=true so it does not get the body gutter.
                let language = match kind {
                    CodeBlockKind::Fenced(language) if !language.is_empty() => {
                        language.into_string()
                    }
                    _ => "code".to_owned(),
                };
                self.code_panel_width = self
                    .code_panel_widths
                    .pop_front()
                    .unwrap_or(self.content_width);
                self.code_language = Some(normalized_code_language(&language));
                self.push_code_chrome(&format!("╭─ {language} "), '╮');
                self.code_block = true;
                self.code_panel_line = true;
            }
            Tag::Link { dest_url, .. } => self.links.push(dest_url.into_string()),
            Tag::Table(_) => {
                self.finish_line();
                self.table_columns = self.table_layouts.pop_front().unwrap_or_default();
                self.push_table_rule('╭', '┬', '╮');
            }
            Tag::TableHead => {
                self.table_head = true;
                self.start_table_row();
            }
            Tag::TableRow => self.start_table_row(),
            Tag::TableCell => self.start_table_cell(),
            Tag::Paragraph
            | Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::Image { .. }
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => self.finish_block(),
            TagEnd::Item => {
                self.finish_line();
                self.list_item_indent = None;
            }
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => {
                self.strikethrough = self.strikethrough.saturating_sub(1);
            }
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::BlockQuote(_) => {
                self.finish_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.blank_line();
            }
            TagEnd::List(_) => {
                self.finish_line();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank_line();
                }
            }
            TagEnd::CodeBlock => {
                self.finish_line();
                // Footer after clearing the body gutter flag.
                self.code_block = false;
                self.code_language = None;
                self.push_code_chrome("╰", '╯');
                self.code_panel_width = self.content_width;
                self.blank_line();
            }
            TagEnd::Link => {
                if let Some(destination) = self.links.pop()
                    && !destination.is_empty()
                {
                    self.text(
                        &format!(" ({destination})"),
                        self.base_style
                            .fg(RolePalette::muted())
                            .add_modifier(Modifier::DIM),
                    );
                }
            }
            TagEnd::TableCell => self.finish_table_cell(),
            TagEnd::TableRow => self.finish_table_row(),
            TagEnd::TableHead => {
                self.finish_table_row();
                self.table_head = false;
                self.push_table_rule('├', '┼', '┤');
            }
            TagEnd::Table => {
                self.finish_line();
                self.push_table_rule('╰', '┴', '╯');
                self.table_columns.clear();
                self.blank_line();
            }
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::Image
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
    }

    fn text(&mut self, text: &str, style: Style) {
        let mut segments = text.split('\n').peekable();
        for index in 0.. {
            let Some(segment) = segments.next() else {
                break;
            };
            if index > 0 {
                self.finish_line();
            }
            if self.code_block && segment.is_empty() && segments.peek().is_none() {
                continue;
            }
            self.start_line();
            if self.code_block {
                self.code_panel_line = true;
            }
            if !segment.is_empty() {
                self.spans.push(Span::styled(segment.to_owned(), style));
            }
        }
    }

    fn code_text(&mut self, text: &str) {
        let highlighted = self.highlight_syntax.then(|| {
            self.code_language
                .as_deref()
                .and_then(|language| syntax_tokens(language, text))
        });
        let highlighted = highlighted.flatten();
        if self.highlight_syntax && highlighted.is_none() {
            self.code_language = None;
        }

        let mut lines = text.split_inclusive('\n').peekable();
        let mut line_start = 0_usize;
        while let Some(line) = lines.next() {
            let ends_with_newline = line.ends_with('\n');
            let visible = line.trim_end_matches(['\r', '\n']);
            if visible.is_empty() && ends_with_newline && lines.peek().is_none() {
                continue;
            }

            self.start_line();
            self.code_panel_line = true;
            let line_end = line_start.saturating_add(visible.len());
            if let Some(tokens) = highlighted.as_deref() {
                self.push_highlighted_code_line(text, line_start, line_end, tokens);
            } else {
                self.push_code_span(visible, self.base_style.bg(code_block_background()));
            }
            if ends_with_newline {
                self.finish_line();
            }
            line_start = line_start.saturating_add(line.len());
        }
    }

    fn push_highlighted_code_line(
        &mut self,
        source: &str,
        line_start: usize,
        line_end: usize,
        tokens: &[SyntaxToken],
    ) {
        let mut cursor = line_start;

        for token in tokens {
            let start = token.start.max(line_start).max(cursor);
            let end = token.end.min(line_end);
            if start >= end {
                continue;
            }

            if cursor < start
                && let Some(gap) = source.get(cursor..start)
            {
                self.push_code_span(gap, syntax_base_style());
            }
            if let Some(text) = source.get(start..end) {
                self.push_code_span(text, token.style);
            }
            cursor = end;
        }

        if cursor < line_end
            && let Some(gap) = source.get(cursor..line_end)
        {
            self.push_code_span(gap, syntax_base_style());
        }
    }

    /// Adds one highlighted token to the current code-panel row, marking the
    /// row when the panel cannot hold the token whole.
    ///
    /// Code is not reflowed: a wrapped source line would read as two
    /// statements. What the panel owes the reader instead is the knowledge
    /// that something was cut, which a bare clip never gave.
    fn push_code_span(&mut self, text: &str, style: Style) {
        let used = Line::from(self.spans.clone()).width();
        let available = self.code_panel_width.saturating_sub(used.saturating_add(2));
        if available == 0 {
            self.mark_code_line_clipped(style);
            return;
        }

        let clipped = take_visible_width(text, available);
        let fits = clipped.width() == text.width();
        let kept = if fits {
            clipped
        } else {
            take_visible_width(&clipped, available.saturating_sub(1))
        };

        if !kept.is_empty() {
            self.spans.push(Span::styled(kept, style));
        }
        if !fits {
            self.mark_code_line_clipped(style);
        }
    }

    /// Closes a clipped code row with the marker that says so, once.
    fn mark_code_line_clipped(&mut self, style: Style) {
        if self
            .spans
            .last()
            .is_some_and(|span| span.content.ends_with(CODE_CLIP_MARKER))
        {
            return;
        }
        self.spans.push(Span::styled(
            CODE_CLIP_MARKER,
            style.fg(RolePalette::muted()),
        ));
    }

    fn current_style(&self) -> Style {
        let mut style = self.base_style;
        if let Some(level) = self.heading {
            style = markdown_heading_style(style, level);
        }
        if self.strong > 0 {
            style = style.add_modifier(Modifier::BOLD);
            if self.heading.is_none() {
                style = style.fg(RolePalette::markdown_strong());
            }
        }
        if self.strikethrough > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.emphasis > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.table_head {
            style = style
                .fg(RolePalette::markdown_strong())
                .add_modifier(Modifier::BOLD);
        }
        if !self.links.is_empty() {
            style = style
                .fg(RolePalette::navigation())
                .add_modifier(Modifier::UNDERLINED);
        }
        style
    }

    fn start_line(&mut self) {
        if !self.spans.is_empty() {
            return;
        }
        if !self.prefix.is_empty() {
            self.spans.push(Span::styled(
                self.prefix.clone(),
                self.base_style.fg(RolePalette::chrome()),
            ));
        }
        if self.code_block {
            // Stable two-cell gutter; joins continuously with ╭─ / ╰─ chrome.
            self.spans.push(Span::styled(
                "│ ",
                Style::default()
                    .fg(RolePalette::muted())
                    .bg(code_block_background()),
            ));
        }
        if self.quote_depth > 0 {
            self.spans.push(Span::styled(
                "│ ".repeat(self.quote_depth),
                self.base_style
                    .fg(RolePalette::markdown_quote())
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if let Some(level) = self.heading {
            self.spans.push(Span::styled(
                markdown_heading_marker(level),
                markdown_heading_style(self.base_style, level),
            ));
        }
    }

    fn start_table_row(&mut self) {
        self.finish_line();
        self.start_line();
        self.table_row_start = self.spans.len();
        self.spans.push(Span::styled(
            "│ ",
            self.base_style.fg(RolePalette::chrome()),
        ));
        self.table_row = true;
        self.table_column = 0;
        self.table_cell_start = self.spans.len();
    }

    fn start_table_cell(&mut self) {
        if self.table_column > 0 {
            self.spans.push(Span::styled(
                " │ ",
                self.base_style.fg(RolePalette::chrome()),
            ));
        }
        self.table_cell_start = self.spans.len();
    }

    fn finish_table_cell(&mut self) {
        let width = self
            .table_columns
            .get(self.table_column)
            .copied()
            .unwrap_or(1);
        let mut cell = self
            .spans
            .split_off(self.table_cell_start.min(self.spans.len()));
        truncate_spans(&mut cell, width);
        let used = Line::from(cell.clone()).width();
        if used < width {
            cell.push(Span::raw(" ".repeat(width - used)));
        }
        self.spans.extend(cell);
        self.table_column = self.table_column.saturating_add(1);
    }

    fn finish_table_row(&mut self) {
        if !self.table_row {
            return;
        }
        let row_width = self
            .content_width
            .saturating_sub(self.structural_prefix_width());
        if row_width <= 3 {
            self.spans
                .truncate(self.table_row_start.min(self.spans.len()));
            let row = match row_width {
                0 => "",
                1 => "│",
                2 => "││",
                _ => "│ │",
            };
            self.spans
                .push(Span::styled(row, self.base_style.fg(RolePalette::chrome())));
            self.finish_line();
            self.table_row = false;
            self.table_column = 0;
            self.table_cell_start = 0;
            self.table_row_start = 0;
            return;
        }
        while self.table_column < self.table_columns.len() {
            self.start_table_cell();
            self.finish_table_cell();
        }
        let closing = " │";
        let available = row_width.saturating_sub(closing.width());
        let mut row = self
            .spans
            .split_off(self.table_row_start.min(self.spans.len()));
        truncate_spans(&mut row, available);
        self.spans.extend(row);
        self.spans.push(Span::styled(
            closing,
            self.base_style.fg(RolePalette::chrome()),
        ));
        self.finish_line();
        self.table_row = false;
        self.table_column = 0;
        self.table_cell_start = 0;
        self.table_row_start = 0;
    }

    fn push_table_rule(&mut self, left: char, junction: char, right: char) {
        self.finish_line();
        self.start_line();
        let available = self
            .content_width
            .saturating_sub(self.structural_prefix_width());
        if available == 0 {
            return;
        }
        let mut rule = left.to_string();
        for (index, width) in self.table_columns.iter().enumerate() {
            if index > 0 {
                rule.push(junction);
            }
            rule.push_str(&"─".repeat(width.saturating_add(2)));
        }
        rule.push(right);
        if rule.width() > available {
            rule = match available {
                0 => String::new(),
                1 => "─".to_owned(),
                2 => format!("{left}{right}"),
                _ => format!("{left}{}{right}", "─".repeat(available - 2)),
            };
        }
        self.spans.push(Span::styled(
            rule,
            self.base_style.fg(RolePalette::chrome()),
        ));
        self.finish_line();
    }

    fn structural_prefix_width(&self) -> usize {
        self.prefix
            .width()
            .saturating_add(self.quote_depth.saturating_mul(2))
    }

    fn push_code_chrome(&mut self, label: &str, right_corner: char) {
        self.start_line();
        let rail = Style::default()
            .fg(RolePalette::muted())
            .bg(code_block_background())
            .add_modifier(Modifier::BOLD);
        self.spans.push(Span::styled(
            take_visible_width(label, self.code_panel_width.saturating_sub(1)),
            rail,
        ));
        let used = Line::from(self.spans.clone()).width();
        if used.saturating_add(1) < self.code_panel_width {
            self.spans.push(Span::styled(
                "─".repeat(self.code_panel_width - used - 1),
                Style::default()
                    .fg(RolePalette::muted())
                    .bg(code_block_background()),
            ));
        }
        if used < self.code_panel_width {
            self.spans
                .push(Span::styled(right_corner.to_string(), rail));
        }
        self.lines.push(Line::from(std::mem::take(&mut self.spans)));
        self.code_panel_line = false;
    }

    fn finish_line(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        if self.code_panel_line || self.code_block {
            self.pad_code_panel_background();
        }
        let spans = std::mem::take(&mut self.spans);
        if self.code_panel_line || self.code_block || self.table_row {
            self.lines.push(Line::from(spans));
        } else {
            let mut continuation = Vec::new();
            if !self.prefix.is_empty() {
                continuation.push(Span::styled(
                    self.prefix.clone(),
                    self.base_style.fg(RolePalette::chrome()),
                ));
            }
            if self.quote_depth > 0 {
                continuation.push(Span::styled(
                    "│ ".repeat(self.quote_depth),
                    self.base_style
                        .fg(RolePalette::markdown_quote())
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if let Some(level) = self.heading {
                continuation.push(Span::raw(
                    " ".repeat(markdown_heading_marker(level).width()),
                ));
            }
            if let Some(indent) = self.list_item_indent {
                continuation.push(Span::raw(" ".repeat(indent)));
            }
            clip_spans(&mut continuation, self.content_width.saturating_sub(1));
            self.lines
                .extend(wrap_styled_spans(spans, self.content_width, &continuation));
        }
        // Keep panel flag while body is active so blank body lines still pad.
        self.code_panel_line = self.code_block;
    }

    fn pad_code_panel_background(&mut self) {
        let used = Line::from(self.spans.clone()).width();
        if used >= self.code_panel_width {
            return;
        }
        let remaining = self.code_panel_width - used;
        if remaining > 2 {
            self.spans.push(Span::styled(
                " ".repeat(remaining - 2),
                Style::default().bg(code_block_background()),
            ));
        }
        let border = if remaining >= 2 { " │" } else { "│" };
        self.spans.push(Span::styled(
            border,
            Style::default()
                .fg(RolePalette::muted())
                .bg(code_block_background()),
        ));
    }

    fn finish_block(&mut self) {
        self.finish_line();
        self.blank_line();
        self.heading = None;
    }

    fn blank_line(&mut self) {
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }
}

const fn markdown_heading_marker(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "◆ ",
        HeadingLevel::H2 => "◇ ",
        HeadingLevel::H3 => "▪ ",
        HeadingLevel::H4 => "▫ ",
        HeadingLevel::H5 => "· ",
        HeadingLevel::H6 => "  · ",
    }
}

fn markdown_heading_style(base: Style, level: HeadingLevel) -> Style {
    let mut style = base.fg(if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
        RolePalette::markdown_heading()
    } else {
        RolePalette::navigation()
    });
    style = match level {
        HeadingLevel::H1 => style
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::UNDERLINED),
        HeadingLevel::H2 | HeadingLevel::H3 => style.add_modifier(Modifier::BOLD),
        HeadingLevel::H4 => style,
        HeadingLevel::H5 => style.add_modifier(Modifier::DIM),
        HeadingLevel::H6 => style
            .add_modifier(Modifier::DIM)
            .add_modifier(Modifier::ITALIC),
    };
    style
}

fn truncate_spans(spans: &mut Vec<Span<'static>>, max_width: usize) {
    let mut kept = Vec::new();
    let mut remaining = max_width;
    for span in spans.drain(..) {
        let width = span.content.width();
        if width <= remaining {
            remaining -= width;
            kept.push(span);
            continue;
        }
        if remaining > 0 {
            let mut content = take_visible_width(&span.content, remaining.saturating_sub(1));
            content.push('…');
            kept.push(Span::styled(content, span.style));
        }
        break;
    }
    *spans = kept;
}

fn clip_spans(spans: &mut Vec<Span<'static>>, max_width: usize) {
    let mut kept = Vec::new();
    let mut remaining = max_width;
    for span in spans.drain(..) {
        if remaining == 0 {
            break;
        }
        let width = span.content.width();
        if width <= remaining {
            remaining -= width;
            kept.push(span);
            continue;
        }
        let content = take_visible_width(&span.content, remaining);
        if !content.is_empty() {
            kept.push(Span::styled(content, span.style));
        }
        break;
    }
    *spans = kept;
}

/// Marks a rendered row as continuing into the next one.
///
/// Wrapping is a fact about the terminal, not about the text, so the copy path
/// has to be able to undo it. The marker is `U+2060 WORD JOINER`: zero width and
/// dropped by the buffer, so it reaches the selection snapshot without reaching
/// the screen. It closes the row it marks rather than opening the next one, so
/// the wrapper's "has anything been placed yet" checks keep meaning what they
/// meant.
/// Break taken between words, where the text had a space the row could not fit.
pub(super) const WRAP_JOINER_SPACE: &str = "\u{2060}";

/// Break taken inside a word too long for any row. Rejoining must not invent a
/// separator the text never had, or a wrapped path comes back off the clipboard
/// as two paths.
pub(super) const WRAP_JOINER_TIGHT: &str = "\u{2061}";

fn wrap_joiner_span(marker: &'static str) -> Span<'static> {
    Span::raw(marker)
}

fn wrap_styled_spans(
    spans: Vec<Span<'static>>,
    max_width: usize,
    continuation: &[Span<'static>],
) -> Vec<Line<'static>> {
    let max_width = max_width.max(1);
    if Line::from(spans.clone()).width() <= max_width {
        return vec![Line::from(spans)];
    }

    let continuation_width = Line::from(continuation.to_vec()).width().min(max_width);
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut used = 0usize;

    for span in spans {
        for (chunk, whitespace) in styled_chunks(&span.content) {
            let chunk_width = chunk.width();
            if whitespace {
                if used == 0 || used == continuation_width {
                    continue;
                }
                if used.saturating_add(chunk_width) <= max_width {
                    push_styled_text(&mut current, &chunk, span.style);
                    used = used.saturating_add(chunk_width);
                }
                continue;
            }

            let line_capacity = max_width.saturating_sub(if lines.is_empty() {
                0
            } else {
                continuation_width
            });
            if chunk_width <= line_capacity && used.saturating_add(chunk_width) > max_width {
                current.push(wrap_joiner_span(WRAP_JOINER_SPACE));
                lines.push(Line::from(std::mem::take(&mut current)));
                current.extend_from_slice(continuation);
                used = continuation_width;
            }

            for grapheme in chunk.graphemes(true) {
                let grapheme_width = grapheme.width();
                if used.saturating_add(grapheme_width) > max_width && !current.is_empty() {
                    current.push(wrap_joiner_span(WRAP_JOINER_TIGHT));
                    lines.push(Line::from(std::mem::take(&mut current)));
                    current.extend_from_slice(continuation);
                    used = continuation_width;
                }
                if used.saturating_add(grapheme_width) <= max_width {
                    push_styled_text(&mut current, grapheme, span.style);
                    used = used.saturating_add(grapheme_width);
                } else if used < max_width {
                    push_styled_text(&mut current, "…", span.style);
                    used = used.saturating_add(1);
                }
            }
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

fn push_styled_text(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(text);
        return;
    }
    spans.push(Span::styled(text.to_owned(), style));
}

fn styled_chunks(text: &str) -> Vec<(String, bool)> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_whitespace = None;
    for character in text.chars() {
        let whitespace = character.is_whitespace();
        if current_whitespace.is_some_and(|kind| kind != whitespace) {
            chunks.push((std::mem::take(&mut current), !whitespace));
        }
        current_whitespace = Some(whitespace);
        current.push(character);
    }
    if let Some(whitespace) = current_whitespace {
        chunks.push((current, whitespace));
    }
    chunks
}

fn markdown_table_layouts(markdown: &str, content_width: usize) -> VecDeque<Vec<usize>> {
    let mut layouts = VecDeque::new();
    let mut maxima: Option<Vec<usize>> = None;
    let mut table_width = content_width;
    let mut quote_depth = 0usize;
    let mut column = 0usize;
    let mut cell_width = 0usize;

    for event in Parser::new_ext(markdown, Options::ENABLE_TABLES) {
        match event {
            Event::Start(Tag::BlockQuote(_)) => quote_depth = quote_depth.saturating_add(1),
            Event::End(TagEnd::BlockQuote(_)) => quote_depth = quote_depth.saturating_sub(1),
            Event::Start(Tag::Table(_)) => {
                maxima = Some(Vec::new());
                table_width = content_width.saturating_sub(quote_depth.saturating_mul(2));
            }
            Event::Start(Tag::TableHead | Tag::TableRow) => column = 0,
            Event::Start(Tag::TableCell) => cell_width = 0,
            Event::Text(text) | Event::Code(text) if maxima.is_some() => {
                cell_width = cell_width.saturating_add(text.width());
            }
            Event::SoftBreak | Event::HardBreak if maxima.is_some() => {
                cell_width = cell_width.saturating_add(1);
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(widths) = maxima.as_mut() {
                    if widths.len() <= column {
                        widths.resize(column + 1, 1);
                    }
                    widths[column] = widths[column].max(cell_width.max(1));
                    column = column.saturating_add(1);
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(widths) = maxima.take() {
                    layouts.push_back(fit_table_columns(&widths, table_width));
                }
            }
            _ => {}
        }
    }

    layouts
}

fn fit_table_columns(natural: &[usize], content_width: usize) -> Vec<usize> {
    if natural.is_empty() {
        return Vec::new();
    }
    let chrome_width = natural.len().saturating_mul(3).saturating_add(1);
    let available = content_width.saturating_sub(chrome_width);
    if available == 0 {
        return vec![0; natural.len()];
    }
    let natural_total = natural.iter().copied().sum::<usize>();
    if natural_total <= available {
        return natural.to_vec();
    }

    let mut widths = natural
        .iter()
        .map(|width| width.saturating_mul(available) / natural_total.max(1))
        .collect::<Vec<_>>();
    if available >= widths.len() {
        for width in &mut widths {
            *width = (*width).max(1);
        }
    }
    let mut assigned = widths.iter().copied().sum::<usize>();
    while assigned < available {
        let Some((index, _)) = natural
            .iter()
            .enumerate()
            .filter(|(index, natural)| widths[*index] < **natural)
            .max_by_key(|(index, natural)| **natural - widths[*index])
        else {
            break;
        };
        widths[index] = widths[index].saturating_add(1);
        assigned = assigned.saturating_add(1);
    }
    while assigned > available {
        let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > 0)
            .max_by_key(|(_, width)| **width)
        else {
            break;
        };
        widths[index] -= 1;
        assigned -= 1;
    }
    widths
}

fn code_panel_widths(markdown: &str, content_width: usize) -> VecDeque<usize> {
    let mut widths = VecDeque::new();
    let mut panel_width = None;

    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let language = match kind {
                    CodeBlockKind::Fenced(language) if !language.is_empty() => {
                        language.into_string()
                    }
                    _ => "code".to_owned(),
                };
                panel_width = Some(Line::from(format!("╭─ {language} ")).width() + 1);
            }
            Event::Text(text) | Event::Code(text) if panel_width.is_some() => {
                let widest_body = text
                    .split('\n')
                    .map(|line| Line::from(line.to_owned()).width() + 4)
                    .max()
                    .unwrap_or(2);
                panel_width = panel_width.map(|width| width.max(widest_body));
            }
            Event::End(TagEnd::CodeBlock) => {
                widths.push_back(panel_width.take().unwrap_or(1).min(content_width).max(1));
            }
            _ => {}
        }
    }

    widths
}

struct SyntaxToken {
    start: usize,
    end: usize,
    style: Style,
}

enum SyntaxCacheState {
    Pending,
    Parsing,
    Tokens(Arc<[SyntaxToken]>),
    Neutral,
}

struct SyntaxCacheEntry {
    hash: u64,
    language: String,
    source: Arc<str>,
    state: SyntaxCacheState,
}

impl SyntaxCacheEntry {
    fn matches(&self, hash: u64, language: &str, source: &str) -> bool {
        self.hash == hash && self.language == language && self.source.as_ref() == source
    }

    fn retained_bytes(&self) -> usize {
        let token_bytes = match &self.state {
            SyntaxCacheState::Tokens(tokens) => {
                tokens.len().saturating_mul(size_of::<SyntaxToken>())
            }
            SyntaxCacheState::Pending | SyntaxCacheState::Parsing | SyntaxCacheState::Neutral => 0,
        };

        self.language
            .len()
            .saturating_add(self.source.len())
            .saturating_add(token_bytes)
    }
}

enum SyntaxCacheLookup {
    Ready(Option<Arc<[SyntaxToken]>>),
    Parse,
    Deferred,
}

struct SyntaxCache {
    entries: VecDeque<SyntaxCacheEntry>,
    retained_bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl SyntaxCache {
    fn with_limits(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            retained_bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    fn lookup(
        &mut self,
        hash: u64,
        language: &str,
        source: &str,
        defer_first_observation: bool,
    ) -> SyntaxCacheLookup {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.matches(hash, language, source))
        {
            let mut entry = self.remove(index);
            let result = match &entry.state {
                SyntaxCacheState::Pending => {
                    entry.state = SyntaxCacheState::Parsing;
                    SyntaxCacheLookup::Parse
                }
                SyntaxCacheState::Parsing => SyntaxCacheLookup::Deferred,
                SyntaxCacheState::Tokens(tokens) => {
                    SyntaxCacheLookup::Ready(Some(Arc::clone(tokens)))
                }
                SyntaxCacheState::Neutral => SyntaxCacheLookup::Ready(None),
            };
            self.push(entry);

            return result;
        }

        let (state, result) = if defer_first_observation {
            (SyntaxCacheState::Pending, SyntaxCacheLookup::Deferred)
        } else {
            (SyntaxCacheState::Parsing, SyntaxCacheLookup::Parse)
        };
        self.push(SyntaxCacheEntry {
            hash,
            language: language.to_owned(),
            source: Arc::from(source),
            state,
        });

        result
    }

    fn complete(
        &mut self,
        hash: u64,
        language: &str,
        source: &str,
        tokens: Option<Arc<[SyntaxToken]>>,
    ) -> Option<Arc<[SyntaxToken]>> {
        let mut entry = self
            .entries
            .iter()
            .position(|entry| entry.matches(hash, language, source))
            .map(|index| self.remove(index))
            .unwrap_or_else(|| SyntaxCacheEntry {
                hash,
                language: language.to_owned(),
                source: Arc::from(source),
                state: SyntaxCacheState::Parsing,
            });

        entry.state = match tokens {
            Some(tokens) => SyntaxCacheState::Tokens(tokens),
            None => SyntaxCacheState::Neutral,
        };
        if entry.retained_bytes() > self.max_bytes {
            entry.state = SyntaxCacheState::Neutral;
        }

        let rendered = match &entry.state {
            SyntaxCacheState::Tokens(tokens) => Some(Arc::clone(tokens)),
            SyntaxCacheState::Pending | SyntaxCacheState::Parsing | SyntaxCacheState::Neutral => {
                None
            }
        };
        self.push(entry);

        rendered
    }

    fn remove(&mut self, index: usize) -> SyntaxCacheEntry {
        let entry = self
            .entries
            .remove(index)
            .expect("syntax cache index came from the same collection");
        self.retained_bytes = self.retained_bytes.saturating_sub(entry.retained_bytes());
        entry
    }

    fn push(&mut self, entry: SyntaxCacheEntry) {
        let entry_bytes = entry.retained_bytes();
        if self.max_entries == 0 || entry_bytes > self.max_bytes {
            return;
        }

        while self.entries.len() >= self.max_entries
            || self.retained_bytes.saturating_add(entry_bytes) > self.max_bytes
        {
            let Some(evicted) = self.entries.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(evicted.retained_bytes());
        }

        self.retained_bytes = self.retained_bytes.saturating_add(entry_bytes);
        self.entries.push_back(entry);
    }
}

fn syntax_grammar_store() -> Arc<GrammarStore> {
    static STORE: OnceLock<Arc<GrammarStore>> = OnceLock::new();
    STORE.get_or_init(|| Arc::new(GrammarStore::new())).clone()
}

fn syntax_token_cache() -> &'static Mutex<SyntaxCache> {
    static CACHE: OnceLock<Mutex<SyntaxCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(SyntaxCache::with_limits(
            SYNTAX_CACHE_MAX_ENTRIES,
            SYNTAX_CACHE_MAX_BYTES,
        ))
    })
}

fn syntax_cache_hash(language: &str, source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    language.hash(&mut hasher);
    source.hash(&mut hasher);
    hasher.finish()
}

fn normalized_code_language(token: &str) -> String {
    token.trim().to_ascii_lowercase()
}

fn syntax_tokens(language: &str, source: &str) -> Option<Arc<[SyntaxToken]>> {
    if source.len() > SYNTAX_MAX_SOURCE_BYTES {
        return None;
    }

    let hash = syntax_cache_hash(language, source);
    let lookup = syntax_token_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .lookup(
            hash,
            language,
            source,
            source.len() >= SYNTAX_DEFER_SOURCE_BYTES,
        );
    match lookup {
        SyntaxCacheLookup::Ready(tokens) => return tokens,
        SyntaxCacheLookup::Deferred => return None,
        SyntaxCacheLookup::Parse => {}
    }

    #[cfg(test)]
    SYNTAX_HIGHLIGHT_CALLS.with(|calls| calls.set(calls.get() + 1));

    let mut highlighter = Highlighter::with_store(syntax_grammar_store());
    let tokens = highlighter
        .highlight_spans(language, source)
        .ok()
        .map(|spans| {
            Arc::from(
                spans_to_flat_tokens(source, spans)
                    .into_iter()
                    .filter_map(|token| {
                        let start = usize::try_from(token.start).ok()?;
                        let end = usize::try_from(token.end).ok()?;
                        source.get(start..end)?;

                        Some(SyntaxToken {
                            start,
                            end,
                            style: syntax_style(token.tag),
                        })
                    })
                    .collect::<Vec<_>>(),
            )
        });

    syntax_token_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .complete(hash, language, source, tokens)
}

fn syntax_theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(builtin::tokyo_night)
}

fn syntax_base_style() -> Style {
    let foreground = syntax_theme()
        .foreground
        .map(|color| Color::Rgb(color.r, color.g, color.b))
        .unwrap_or_else(RolePalette::assistant);
    Style::default().fg(foreground).bg(code_block_background())
}

fn syntax_style(tag: &str) -> Style {
    let Some(style) = tag_to_name(tag)
        .map(capture_to_slot)
        .and_then(slot_to_highlight_index)
        .and_then(|index| syntax_theme().style(index))
    else {
        return syntax_base_style();
    };

    let mut rendered = syntax_base_style();
    if let Some(foreground) = style.fg {
        rendered = rendered.fg(Color::Rgb(foreground.r, foreground.g, foreground.b));
    }
    if style.modifiers.bold {
        rendered = rendered.add_modifier(Modifier::BOLD);
    }
    if style.modifiers.italic {
        rendered = rendered.add_modifier(Modifier::ITALIC);
    }
    if style.modifiers.underline {
        rendered = rendered.add_modifier(Modifier::UNDERLINED);
    }
    if style.modifiers.strikethrough {
        rendered = rendered.add_modifier(Modifier::CROSSED_OUT);
    }
    rendered
}

/// The readable text of an HTML fragment: markup a terminal cannot render is
/// dropped, everything between the tags is kept.
///
/// Dropping the whole event instead would silently swallow the prose inside an
/// HTML block, which is the more common case in model output than the tags
/// themselves carrying meaning.
fn strip_html_tags(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut inside_tag = false;

    for character in html.chars() {
        match character {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag => text.push(character),
            _ => {}
        }
    }

    text
}

fn take_visible_width(text: &str, max_width: usize) -> String {
    let mut clipped = String::new();
    let mut width = 0_usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = grapheme.width();
        if width.saturating_add(grapheme_width) > max_width {
            break;
        }
        clipped.push_str(grapheme);
        width += grapheme_width;
    }
    clipped
}

/// Panel background for fenced/inline code — slightly elevated over the default terminal bg.
const fn code_block_background() -> Color {
    RolePalette::code_panel_bg()
}

const MAX_DIFF_ROWS: usize = 200;

/// Result-size metadata (lines and bytes) for a tool output.
///
/// This is a real measurement of the retained output, never a fabricated
/// per-call token count.
fn result_size_label(output: &str) -> String {
    format!("{} lines · {} B", output.lines().count(), output.len())
}

/// Paint an edit diff with a dual old/new line-number gutter, gap markers for
/// unchanged runs, insert/delete row backgrounds, and a row cap.
///
/// The projected diff carries only changed lines (context is dropped upstream),
/// so unchanged runs are inferred from line-number jumps and rendered as a
/// single `… N unchanged lines` marker. No `+`/`-` markers are drawn; the
/// gutter position and row background encode insert vs. delete.
/// The grammar to read an edit's diff with, taken from the file it edits.
///
/// The extension is the only signal available here and it is not always right,
/// so an unrecognized one yields `None` and the diff keeps its plain
/// red-and-green reading rather than being coloured by a guess.
fn diff_language(conversation: &Conversation, call_id: &str) -> Option<&'static str> {
    let path = conversation
        .tool_batches
        .iter()
        .flat_map(|batch| &batch.calls)
        .find(|call| call.call_id == call_id)
        .and_then(|call| match &call.parsed {
            agens_core::ToolInput::Edit { path } | agens_core::ToolInput::Write { path } => {
                Some(path.as_str())
            }
            _ => None,
        })?;

    let extension = path.rsplit_once('.').map(|(_, extension)| extension)?;
    language_for_extension(&extension.to_ascii_lowercase())
}

/// Extensions mapped only to grammars this build actually carries.
///
/// Naming a language the `GrammarStore` cannot load would cost a failed parse
/// on every diff row for no colour, so the table and the enabled feature list
/// are kept in step deliberately.
fn language_for_extension(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "rs" => "rust",
        "go" => "go",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" | "jsx" => "tsx",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "c-sharp",
        "java" => "java",
        "css" => "css",
        "html" | "htm" => "html",
        "json" => "json",
        "md" | "markdown" => "markdown",
        "nix" => "nix",
        "sh" | "bash" | "zsh" => "bash",
        "sql" => "sql",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        _ => return None,
    })
}

/// Styled spans for one diff row's text, coloured by the language when the
/// run it belongs to could be parsed.
///
/// Highlighting runs over a contiguous run of same-kind rows rather than over
/// the whole diff: consecutive added rows are contiguous new-file content and
/// parse as such, while an added row sitting next to the removed row it
/// replaces is not a program and would parse as neither.
/// A parsed run: the source its rows were joined into, and its tokens.
type ParsedRun = (String, Arc<[SyntaxToken]>);

/// Where one diff row sits inside the run that was parsed for it.
type RowHighlight = (Arc<ParsedRun>, usize);

fn diff_text_spans(
    text: &str,
    highlighted: Option<&ParsedRun>,
    offset: usize,
    base: Style,
) -> Vec<Span<'static>> {
    let Some((source, tokens)) = highlighted else {
        return vec![Span::styled(text.to_owned(), base)];
    };

    let line_end = offset.saturating_add(text.len());
    let mut spans = Vec::new();
    let mut cursor = offset;

    for token in tokens.iter() {
        let start = token.start.max(offset).max(cursor);
        let end = token.end.min(line_end);
        if start >= end {
            continue;
        }
        if cursor < start
            && let Some(gap) = source.get(cursor..start)
        {
            spans.push(Span::styled(gap.to_owned(), base));
        }
        if let Some(slice) = source.get(start..end) {
            spans.push(Span::styled(
                slice.to_owned(),
                base.fg(token.style.fg.unwrap_or(RolePalette::assistant())),
            ));
        }
        cursor = end;
    }

    if cursor < line_end
        && let Some(gap) = source.get(cursor..line_end)
    {
        spans.push(Span::styled(gap.to_owned(), base));
    }

    if spans.is_empty() {
        spans.push(Span::styled(text.to_owned(), base));
    }
    spans
}

/// Parses each contiguous run of same-kind rows, returning per-row the source
/// its run was parsed from and the row's byte offset into it.
fn diff_run_highlights(diff: &[DiffLine], language: Option<&str>) -> Vec<Option<RowHighlight>> {
    let mut highlights = vec![None; diff.len()];
    let Some(language) = language else {
        return highlights;
    };

    let mut start = 0;
    while start < diff.len() {
        let mut end = start + 1;
        while end < diff.len() && diff[end].kind == diff[start].kind {
            end += 1;
        }

        let mut source = String::new();
        let mut offsets = Vec::with_capacity(end - start);
        for change in &diff[start..end] {
            offsets.push(source.len());
            source.push_str(&change.text);
            source.push('\n');
        }

        if let Some(tokens) = syntax_tokens(language, &source) {
            let parsed = Arc::new((source, tokens));
            for (index, offset) in offsets.into_iter().enumerate() {
                highlights[start + index] = Some((Arc::clone(&parsed), offset));
            }
        }

        start = end;
    }

    highlights
}

fn render_diff(
    lines: &mut Vec<Line<'static>>,
    diff: &[DiffLine],
    language: Option<&str>,
    content_width: usize,
) {
    let gutter_width = diff
        .iter()
        .map(|change| digit_width(change.number))
        .max()
        .unwrap_or(1)
        .max(1);

    let highlights = diff_run_highlights(diff, language);
    let mut old_cursor: Option<u32> = None;
    let mut new_cursor: Option<u32> = None;

    for (index, change) in diff.iter().enumerate() {
        if index >= MAX_DIFF_ROWS {
            let remaining = diff.len() - index;
            lines.push(diff_note_line(
                &format!("… {remaining} more lines"),
                gutter_width,
            ));
            break;
        }

        let axis_cursor = match change.kind {
            DiffLineKind::Added => new_cursor,
            DiffLineKind::Removed | DiffLineKind::Context => old_cursor,
        };
        if let Some(expected) = axis_cursor
            && change.number > expected
        {
            let gap = change.number - expected;
            lines.push(diff_note_line(
                &format!("… {gap} unchanged lines"),
                gutter_width,
            ));
            old_cursor = old_cursor.map(|cursor| cursor + gap);
            new_cursor = new_cursor.map(|cursor| cursor + gap);
        }

        let (old_number, new_number, background) = match change.kind {
            DiffLineKind::Removed => (
                Some(change.number),
                None,
                Some(RolePalette::diff_delete_bg()),
            ),
            DiffLineKind::Added => (
                None,
                Some(change.number),
                Some(RolePalette::diff_insert_bg()),
            ),
            DiffLineKind::Context => (Some(change.number), Some(change.number), None),
        };
        let highlight = highlights
            .get(index)
            .and_then(Option::as_ref)
            .map(|(parsed, offset)| (parsed.as_ref(), *offset));
        lines.push(diff_change_line(
            old_number,
            new_number,
            &change.text,
            highlight,
            background,
            gutter_width,
            content_width,
        ));

        match change.kind {
            DiffLineKind::Removed => old_cursor = Some(change.number + 1),
            DiffLineKind::Added => new_cursor = Some(change.number + 1),
            DiffLineKind::Context => {
                old_cursor = Some(change.number + 1);
                new_cursor = Some(change.number + 1);
            }
        }
    }
}

fn digit_width(number: u32) -> usize {
    number
        .checked_ilog10()
        .map_or(1, |digits| digits as usize + 1)
}

fn diff_gutter(old: Option<u32>, new: Option<u32>, gutter_width: usize) -> String {
    let cell = |value: Option<u32>| match value {
        Some(number) => format!("{number:>gutter_width$}"),
        None => " ".repeat(gutter_width),
    };
    format!("{} {} │ ", cell(old), cell(new))
}

fn diff_change_line(
    old: Option<u32>,
    new: Option<u32>,
    text: &str,
    highlight: Option<(&ParsedRun, usize)>,
    background: Option<Color>,
    gutter_width: usize,
    content_width: usize,
) -> Line<'static> {
    let gutter = diff_gutter(old, new, gutter_width);

    let mut gutter_style = Style::default().fg(RolePalette::muted());
    let mut text_style = Style::default();
    if let Some(color) = background {
        gutter_style = gutter_style.bg(color);
        text_style = text_style.bg(color);
    }

    let mut spans = vec![Span::styled(gutter.clone(), gutter_style)];
    spans.extend(diff_text_spans(
        text,
        highlight.map(|(parsed, _)| parsed),
        highlight.map_or(0, |(_, offset)| offset),
        text_style,
    ));

    if let Some(color) = background {
        let used = gutter.width().saturating_add(text.width());
        if used < content_width {
            spans.push(Span::styled(
                " ".repeat(content_width - used),
                Style::default().bg(color),
            ));
        }
    }

    Line::from(spans)
}

fn diff_note_line(text: &str, gutter_width: usize) -> Line<'static> {
    let indent = " ".repeat(gutter_width.saturating_mul(2).saturating_add(3));
    Line::from(Span::styled(
        format!("{indent}{text}"),
        Style::default().fg(RolePalette::muted()),
    ))
}

fn diff_line(lines: &mut Vec<Line<'static>>, number: u32, kind: DiffLineKind, text: &str) {
    let (marker, color) = match kind {
        DiffLineKind::Added => ('+', RolePalette::success()),
        DiffLineKind::Removed => ('-', RolePalette::error()),
        DiffLineKind::Context => (' ', RolePalette::muted()),
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!("{:>4} {marker} ", number),
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

fn label_lines(label: &str, color: Color, text: impl Into<String>) -> Vec<Line<'static>> {
    labelled_lines(label, color, Style::default(), text)
}

/// Labelled rows whose body carries `body` instead of the transcript default.
///
/// Used when the label's colour is not enough to place the row: a failure has
/// to read as a failure across its whole width, not only in its gutter.
fn labelled_lines(
    label: &str,
    color: Color,
    body: Style,
    text: impl Into<String>,
) -> Vec<Line<'static>> {
    text.into()
        .split('\n')
        .map(|text_line| {
            Line::from(vec![
                Span::styled(
                    format!("│ {label:<9} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(text_line.to_owned(), body),
            ])
        })
        .collect()
}

fn line(lines: &mut Vec<Line<'static>>, label: &str, color: Color, text: impl Into<String>) {
    lines.extend(label_lines(label, color, text));
    lines.push(Line::default());
}

fn duration_label(duration: Option<Duration>) -> String {
    duration.map_or_else(String::new, |value| format!(" · {}", elapsed_label(value)))
}

fn result_color(result: ToolResultState) -> Color {
    match result {
        ToolResultState::Success => RolePalette::success(),
        ToolResultState::Failure => RolePalette::error(),
    }
}

#[cfg(test)]
thread_local! {
    static SYNTAX_HIGHLIGHT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TOOL_BODY_RENDERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SETTLED_CONVERSATION_RENDERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_settled_conversation_test_state() {
    SETTLED_CONVERSATION_CACHE.with_borrow_mut(VecDeque::clear);
    SETTLED_CONVERSATION_RENDERS.with(|renders| renders.set(0));
}

#[cfg(test)]
fn settled_conversation_test_renders() -> usize {
    SETTLED_CONVERSATION_RENDERS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn reset_tool_body_test_state() {
    TOOL_BODY_CACHE.with_borrow_mut(VecDeque::clear);
    TOOL_BODY_RENDERS.with(|renders| renders.set(0));
}

#[cfg(test)]
fn tool_body_test_renders() -> usize {
    TOOL_BODY_RENDERS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(super) static SYNTAX_CACHE_TESTS: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(super) fn reset_syntax_highlight_test_state() {
    *syntax_token_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        SyntaxCache::with_limits(SYNTAX_CACHE_MAX_ENTRIES, SYNTAX_CACHE_MAX_BYTES);
    SYNTAX_HIGHLIGHT_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(super) fn syntax_highlight_test_calls() -> usize {
    SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_restored_tool_call_is_terminal_instead_of_running() {
        let conversations = Conversation::from_messages(&[
            agens_core::Message {
                role: agens_core::Role::User,
                parts: vec![agens_core::MessagePart::Text("inspect".into())],
            },
            agens_core::Message {
                role: agens_core::Role::Assistant,
                parts: vec![agens_core::MessagePart::ToolCall {
                    id: "dangling".into(),
                    name: "native::read".into(),
                    input: r#"{"path":"missing"}"#.into(),
                }],
            },
        ])
        .unwrap();

        assert_eq!(
            call_row_state(&conversations[0], "dangling"),
            RowState::Failure
        );
        let rendered = joined(&conversation_lines(
            &conversations[0],
            &[],
            &BTreeMap::new(),
            80,
            conversation_state(false),
        ));
        assert!(rendered.contains("Failure"), "{rendered:?}");
        assert!(!rendered.contains("Running"), "{rendered:?}");
    }

    #[test]
    fn error_lines_splits_a_multi_line_message_into_its_own_gutter_rows() {
        let error = crate::ActionableError {
            message: "provider: ChatGPT request was rejected [ref: abcdef12]\nHTTP 400 rejected model \"x\"".to_owned(),
            action: "Retry.".to_owned(),
        };

        let lines = error_lines(&error, 80);

        assert_eq!(lines.len(), 4);
        let text = |line: &Line<'static>| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        assert_eq!(text(&lines[0]), "┌ Error");
        assert_eq!(
            text(&lines[1]),
            "│ provider: ChatGPT request was rejected [ref: abcdef12]"
        );
        assert_eq!(text(&lines[2]), "│ HTTP 400 rejected model \"x\"");
        assert_eq!(text(&lines[3]), "└ Action: Retry.");
    }

    fn conversation_state(assistant_streaming: bool) -> ConversationRenderState<'static> {
        ConversationRenderState {
            collapse_thinking: false,
            thinking_streaming: false,
            assistant_streaming,
            now: Duration::ZERO,
            focused_call: None,
            unicode: UnicodeLevel::Extended,
        }
    }

    fn reset_syntax_cache() {
        reset_syntax_highlight_test_state();
    }

    fn empty_tokens() -> Arc<[SyntaxToken]> {
        Arc::from(Vec::<SyntaxToken>::new())
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn joined(lines: &[Line<'static>]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    fn read_conversation(paths: &[&str]) -> Conversation {
        let mut conversation = Conversation::new("prompt");
        for path in paths {
            conversation
                .apply(crate::ConversationEvent::ToolCall {
                    call_id: (*path).to_owned(),
                    name: "native::read".into(),
                    input: "{}".into(),
                    parsed: agens_core::ToolInput::Read {
                        path: (*path).to_owned(),
                    },
                })
                .expect("read call should project");
        }
        conversation
    }

    fn render_at(
        conversation: &Conversation,
        modes: &BTreeMap<String, DisplayMode>,
        now: Duration,
    ) -> String {
        joined(&conversation_lines(
            conversation,
            &[],
            modes,
            80,
            ConversationRenderState {
                collapse_thinking: false,
                thinking_streaming: false,
                assistant_streaming: false,
                now,
                focused_call: None,
                unicode: UnicodeLevel::Extended,
            },
        ))
    }

    fn lines_at(
        conversation: &Conversation,
        modes: &BTreeMap<String, DisplayMode>,
    ) -> Vec<Line<'static>> {
        conversation_lines(
            conversation,
            &[],
            modes,
            80,
            ConversationRenderState {
                collapse_thinking: false,
                thinking_streaming: false,
                assistant_streaming: false,
                now: Duration::ZERO,
                focused_call: None,
                unicode: UnicodeLevel::Extended,
            },
        )
    }

    /// Style of the gutter bullet on the row containing `needle`, which follows
    /// the accent column every row opens with.
    fn bullet_style(lines: &[Line<'static>], needle: &str) -> Style {
        lines
            .iter()
            .find(|line| line_text(line).contains(needle))
            .and_then(|line| line.spans.get(1))
            .map(|span| span.style)
            .expect("row should be rendered with a gutter")
    }

    /// Style of the span whose text contains `needle`.
    fn span_style(lines: &[Line<'static>], needle: &str) -> Style {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains(needle))
            .map(|span| span.style)
            .unwrap_or_else(|| panic!("{needle:?} should be rendered"))
    }

    /// Accent-column span of the row containing `needle`.
    fn accent_span(lines: &[Line<'static>], needle: &str) -> Span<'static> {
        lines
            .iter()
            .find(|line| line_text(line).contains(needle))
            .and_then(|line| line.spans.first())
            .cloned()
            .expect("row should be rendered with an accent column")
    }

    #[test]
    fn consecutive_reads_fold_into_a_tense_aware_group_that_expanding_restores() {
        let paths = ["a.rs", "b.rs", "c.rs"];
        let mut conversation = read_conversation(&paths);
        let mut modes = BTreeMap::new();

        let running = render_at(&conversation, &modes, Duration::ZERO);
        assert!(running.contains("Reading 3 files…"), "{running:?}");
        assert!(!running.contains("Read a.rs"), "{running:?}");

        for path in paths {
            conversation
                .apply(crate::ConversationEvent::ToolResult {
                    call_id: path.to_owned(),
                    output: "ok".into(),
                    is_error: false,
                })
                .expect("read result should project");
            modes.insert(path.to_owned(), DisplayMode::Collapsed);
        }

        let finished = render_at(&conversation, &modes, Duration::ZERO);
        assert!(finished.contains("Read 3 files"), "{finished:?}");
        assert!(!finished.contains("Reading 3 files"), "{finished:?}");
        assert!(!finished.contains("Read a.rs"), "{finished:?}");

        for path in paths {
            modes.insert(path.to_owned(), DisplayMode::Truncated);
        }
        let expanded = render_at(&conversation, &modes, Duration::ZERO);
        assert!(!expanded.contains("Read 3 files"), "{expanded:?}");
        for path in paths {
            assert!(expanded.contains(&format!("Read {path}")), "{expanded:?}");
        }
    }

    #[test]
    fn destructive_and_unknown_tool_rows_never_fold_into_a_group() {
        let mut conversation = Conversation::new("prompt");
        let mut modes = BTreeMap::new();
        for (call_id, name, parsed) in [
            (
                "bash-1",
                "native::bash",
                agens_core::ToolInput::Bash {
                    command: "echo one".into(),
                },
            ),
            (
                "bash-2",
                "native::bash",
                agens_core::ToolInput::Bash {
                    command: "echo two".into(),
                },
            ),
            (
                "write-1",
                "native::write",
                agens_core::ToolInput::Write {
                    path: "x.rs".into(),
                },
            ),
            (
                "write-2",
                "native::write",
                agens_core::ToolInput::Write {
                    path: "y.rs".into(),
                },
            ),
        ] {
            conversation
                .apply(crate::ConversationEvent::ToolCall {
                    call_id: call_id.to_owned(),
                    name: name.to_owned(),
                    input: "{}".into(),
                    parsed,
                })
                .expect("call should project");
            conversation
                .apply(crate::ConversationEvent::ToolResult {
                    call_id: call_id.to_owned(),
                    output: "ok".into(),
                    is_error: false,
                })
                .expect("result should project");
            modes.insert(call_id.to_owned(), DisplayMode::Collapsed);
        }

        let rendered = render_at(&conversation, &modes, Duration::ZERO);
        for header in ["$ echo one", "$ echo two", "Write x.rs", "Write y.rs"] {
            assert!(rendered.contains(header), "{rendered:?}");
        }
        assert!(!rendered.contains("files"), "{rendered:?}");
        assert!(!rendered.contains("Wrote 2"), "{rendered:?}");
    }

    #[test]
    fn row_shape_ignores_the_tick_clock_and_state_lives_in_the_bullet_colour() {
        let running = read_conversation(&["a.rs", "b.rs"]);
        let modes = BTreeMap::new();
        assert_eq!(
            render_at(&running, &modes, Duration::ZERO),
            render_at(&running, &modes, Duration::from_millis(200)),
            "a row never changes shape as ticks advance"
        );
        assert_eq!(
            bullet_style(&lines_at(&running, &modes), "Reading 2 files…").fg,
            Some(RolePalette::running())
        );

        let mut finished = read_conversation(&["c.rs", "d.rs"]);
        let mut finished_modes = BTreeMap::new();
        for (path, is_error) in [("c.rs", false), ("d.rs", true)] {
            finished
                .apply(crate::ConversationEvent::ToolResult {
                    call_id: path.to_owned(),
                    output: "ok".into(),
                    is_error,
                })
                .expect("result should project");
            finished_modes.insert(path.to_owned(), DisplayMode::Collapsed);
        }
        let rendered = lines_at(&finished, &finished_modes);
        assert_eq!(
            bullet_style(&rendered, "Read 2 files").fg,
            Some(RolePalette::error()),
            "a group carrying a failure is coloured by that failure"
        );
        assert!(joined(&rendered).contains("Read 2 files · 1 failed"));
    }

    #[test]
    fn accent_bars_mark_user_identity_live_work_and_consequential_rows() {
        let running = read_conversation(&["a.rs", "b.rs"]);
        let live = lines_at(&running, &BTreeMap::new());
        let bar = accent_span(&live, "Reading 2 files…");
        assert_eq!(bar.content, "┃");
        assert_eq!(bar.style.fg, Some(RolePalette::running()));
        let prompt_bar = accent_span(&live, "prompt");
        assert_eq!(prompt_bar.content, "┃");
        assert_eq!(prompt_bar.style.fg, Some(RolePalette::user_identity()));

        let mut folded = read_conversation(&["a.rs", "b.rs"]);
        let mut folded_modes = BTreeMap::new();
        for path in ["a.rs", "b.rs"] {
            folded
                .apply(crate::ConversationEvent::ToolResult {
                    call_id: path.to_owned(),
                    output: "ok".into(),
                    is_error: false,
                })
                .expect("result should project");
            folded_modes.insert(path.to_owned(), DisplayMode::Collapsed);
        }
        let settled = accent_span(&lines_at(&folded, &folded_modes), "Read 2 files");
        assert_eq!(
            settled.content, "❙",
            "a collapsed groupable run gets the thin variant"
        );
        assert_ne!(
            settled.style.fg,
            Some(RolePalette::success()),
            "the thin variant is dimmed against the group's own colour"
        );

        let mut single = read_conversation(&["c.rs"]);
        single
            .apply(crate::ConversationEvent::ToolResult {
                call_id: "c.rs".into(),
                output: "ok".into(),
                is_error: false,
            })
            .expect("result should project");
        single
            .apply(crate::ConversationEvent::ReasoningDelta("thought".into()))
            .expect("reasoning should project");
        let mut single_modes = BTreeMap::new();
        single_modes.insert("c.rs".to_owned(), DisplayMode::Collapsed);
        let plain = lines_at(&single, &single_modes);

        assert_eq!(
            accent_span(&plain, "Read c.rs").content,
            " ",
            "a plain finished read carries no bar"
        );
        let thinking = accent_span(&plain, "Thinking");
        assert_eq!(thinking.content, "┃");
        assert_eq!(thinking.style.fg, Some(RolePalette::muted()));
    }

    /// The recorded mode is a reader override, so its absence has to mean "the
    /// reader asked for nothing", not a fourth state. A settled run used to
    /// render collapsed and refuse to fold at the same time, which is the one
    /// combination the group summary exists to prevent.
    #[test]
    fn a_settled_run_folds_whether_or_not_a_mode_was_ever_recorded_for_it() {
        let mut settled = read_conversation(&["a.rs", "b.rs"]);
        for path in ["a.rs", "b.rs"] {
            settled
                .apply(crate::ConversationEvent::ToolResult {
                    call_id: path.to_owned(),
                    output: "ok".into(),
                    is_error: false,
                })
                .expect("result should project");
        }

        let unrecorded = lines_at(&settled, &BTreeMap::new());
        assert!(
            unrecorded
                .iter()
                .any(|line| line_text(line).contains("Read 2 files")),
            "{unrecorded:?}"
        );

        let mut expanded = BTreeMap::new();
        expanded.insert("a.rs".to_owned(), DisplayMode::Expanded);
        let opened = lines_at(&settled, &expanded);
        assert!(
            !opened
                .iter()
                .any(|line| line_text(line).contains("Read 2 files")),
            "a call the reader opened stays out of the group: {opened:?}"
        );
    }

    #[test]
    fn collapsed_groupable_neighbours_pack_and_everything_else_keeps_one_blank_row() {
        let mut conversation = read_conversation(&["a.rs"]);
        conversation
            .apply(crate::ConversationEvent::ToolCall {
                call_id: "grep-1".into(),
                name: "native::grep".into(),
                input: "{}".into(),
                parsed: agens_core::ToolInput::Grep {
                    pattern: "needle".into(),
                    path: None,
                },
            })
            .expect("grep call should project");
        let mut modes = BTreeMap::new();
        for call_id in ["a.rs", "grep-1"] {
            conversation
                .apply(crate::ConversationEvent::ToolResult {
                    call_id: call_id.to_owned(),
                    output: "ok".into(),
                    is_error: false,
                })
                .expect("result should project");
            modes.insert(call_id.to_owned(), DisplayMode::Collapsed);
        }
        conversation
            .apply(crate::ConversationEvent::MarkdownFinal("prose".into()))
            .expect("prose should project");

        let rows = lines_at(&conversation, &modes)
            .iter()
            .map(|line| line_text(line).trim_end().to_owned())
            .collect::<Vec<_>>();
        let blank_rows = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.is_empty())
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let prose_row = rows
            .iter()
            .position(|row| row.contains("prose"))
            .expect("prose renders");

        assert_eq!(
            blank_rows,
            vec![1, prose_row - 1],
            "only the user prompt and the prose are separated: {rows:?}"
        );
        assert!(
            rows[2..prose_row - 1].iter().all(|row| !row.is_empty()),
            "collapsed tool rows pack: {rows:?}"
        );
    }

    #[test]
    fn every_rendered_row_starts_on_the_shared_gutter() {
        let mut conversation = read_conversation(&["a.rs"]);
        conversation
            .apply(crate::ConversationEvent::ReasoningDelta("thought".into()))
            .expect("reasoning should project");
        conversation
            .apply(crate::ConversationEvent::ToolResult {
                call_id: "a.rs".into(),
                output: "ok".into(),
                is_error: false,
            })
            .expect("result should project");
        conversation
            .apply(crate::ConversationEvent::Error {
                message: "failed".into(),
                action: "retry".into(),
            })
            .expect("error should project");
        conversation
            .apply(crate::ConversationEvent::MarkdownFinal("prose".into()))
            .expect("prose should project");

        for line in lines_at(&conversation, &BTreeMap::new()) {
            let text = line_text(&line);
            if text.trim().is_empty() {
                continue;
            }
            assert_eq!(
                line.spans
                    .first()
                    .map(|span| span.content.width())
                    .unwrap_or_default(),
                ACCENT_WIDTH,
                "row {text:?} does not open with the accent column"
            );
            assert_eq!(
                line.spans
                    .get(1)
                    .map(|span| span.content.width())
                    .unwrap_or_default(),
                GUTTER_WIDTH,
                "row {text:?} does not follow the accent column with the shared gutter"
            );
        }
    }

    #[test]
    fn idle_tool_bodies_are_reused_across_ticks_instead_of_recomputed() {
        reset_tool_body_test_state();
        let mut conversation = read_conversation(&["a.rs"]);
        conversation
            .apply(crate::ConversationEvent::ToolResult {
                call_id: "a.rs".into(),
                output: "line one\nline two\nline three".into(),
                is_error: false,
            })
            .expect("result should project");
        let mut modes = BTreeMap::new();
        modes.insert("a.rs".to_owned(), DisplayMode::Expanded);

        for tick in [0, 80, 160, 240] {
            let _ = render_at(&conversation, &modes, Duration::from_millis(tick));
        }

        assert_eq!(
            tool_body_test_renders(),
            1,
            "an idle tool body is described once, not once per tick"
        );
    }

    /// Freshly described rows of a tool result body, bypassing the frame cache.
    fn tool_body(call_id: &str, output: &str, content_width: usize) -> Vec<Line<'static>> {
        reset_tool_body_test_state();
        tool_result_body(call_id, output, content_width, false).to_vec()
    }

    #[test]
    fn tool_output_keeps_indentation_instead_of_becoming_a_code_panel() {
        let output = "impl Config {\n    fn new() -> Self {\n        Self\n    }\n\n    fn root(&self) {}\n}";

        let body = tool_body("read-indented", output, 40);

        assert_eq!(
            body.iter().map(line_text).collect::<Vec<_>>(),
            output.lines().collect::<Vec<_>>()
        );
    }

    #[test]
    fn tool_output_keeps_hash_and_rule_lines_as_literal_text() {
        let output = "# rebuilding\n---";

        let body = tool_body("bash-markers", output, 40);

        assert_eq!(
            body.iter().map(line_text).collect::<Vec<_>>(),
            vec!["# rebuilding", "---"]
        );
        let style = span_style(&body, "# rebuilding");
        assert_eq!(style.fg, Some(RolePalette::muted()));
        assert!(!style.add_modifier.contains(Modifier::BOLD), "{style:?}");
    }

    #[test]
    fn tool_output_wraps_a_long_line_at_the_content_width_without_losing_its_tail() {
        let output = "thread 'main' panicked at src/lib.rs:42:5: assertion failed: left == right";

        let body = tool_body("bash-wide", output, 40);

        let rows = body.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(
            rows.first().map(|row| row.width()),
            Some(40),
            "a wrapped row fills the content width instead of breaking on a word"
        );
        assert!(rows.iter().all(|row| row.width() <= 40), "{rows:?}");
        assert_eq!(rows.concat(), output, "wrapping never drops a tail");
    }

    #[test]
    fn tool_output_keeps_the_ansi_colour_the_command_emitted() {
        let output = "\u{1b}[31merror\u{1b}[0m: build failed";

        let body = tool_body("bash-ansi", output, 40);

        assert_eq!(
            body.iter().map(line_text).collect::<Vec<_>>(),
            vec!["error: build failed"]
        );
        assert_eq!(span_style(&body, "error").fg, Some(Color::Indexed(1)));
        assert_eq!(
            span_style(&body, ": build failed").fg,
            Some(RolePalette::muted())
        );
    }

    const LONG_TASK: &str = "Investiga este proyecto sin modificar archivos. Revisa la \
         estructura, tecnologias usadas, puntos de entrada, scripts disponibles, arquitectura \
         general, pruebas y documentacion.";

    fn card_conversation(task: &str) -> Conversation {
        let mut conversation = Conversation::new("delegate");
        conversation.apply_subagent_summary(
            crate::TuiSubagentEvent::started(
                7,
                "explore",
                task,
                crate::TuiExecutionState::ForegroundRunning,
            ),
            Duration::ZERO,
        );
        for (call_id, name) in [("read-1", "native::read"), ("grep-1", "native::grep")] {
            conversation.apply_subagent_summary(
                crate::TuiSubagentEvent::tool_call(7, call_id, name, "{}"),
                Duration::ZERO,
            );
        }
        conversation
    }

    /// Every colour a settled, successful transcript screen may paint.
    ///
    /// Neutral prose and chrome remain dominant while a compact Markdown palette
    /// distinguishes headings, strong text, inline code, and navigation markers.
    const SETTLED_PALETTE: [Color; 8] = [
        RolePalette::assistant(),
        RolePalette::machine(),
        RolePalette::muted(),
        RolePalette::user_identity(),
        RolePalette::success(),
        RolePalette::markdown_heading(),
        RolePalette::markdown_strong(),
        RolePalette::markdown_code(),
    ];

    fn settled_conversation(is_error: bool) -> Conversation {
        let mut conversation = Conversation::new("delegate the work");
        conversation
            .apply(crate::ConversationEvent::ReasoningDelta(
                "weighing the options".into(),
            ))
            .expect("reasoning should project");
        conversation
            .apply(crate::ConversationEvent::ToolCall {
                call_id: "bash-1".into(),
                name: "native::bash".into(),
                input: "{}".into(),
                parsed: agens_core::ToolInput::Bash {
                    command: "cargo test".into(),
                },
            })
            .expect("call should project");
        conversation
            .apply(crate::ConversationEvent::ToolResult {
                call_id: "bash-1".into(),
                output: "exit 1: command not found".into(),
                is_error,
            })
            .expect("result should project");
        conversation
            .apply(crate::ConversationEvent::MarkdownFinal(
                "# Heading\n\n**done** with _one_ note and a `token`.\n\n- item".into(),
            ))
            .expect("prose should project");
        conversation
    }

    fn settled_lines(is_error: bool) -> Vec<Line<'static>> {
        let conversation = settled_conversation(is_error);
        let mut modes = BTreeMap::new();
        modes.insert("bash-1".to_owned(), DisplayMode::Collapsed);
        conversation_lines(
            &conversation,
            &[],
            &modes,
            80,
            ConversationRenderState {
                collapse_thinking: true,
                thinking_streaming: false,
                assistant_streaming: false,
                now: Duration::ZERO,
                focused_call: None,
                unicode: UnicodeLevel::Extended,
            },
        )
    }

    fn cached_settled_lines(
        conversation: &Conversation,
        identity: SettledConversation,
    ) -> Arc<[Line<'static>]> {
        settled_conversation_lines(
            identity,
            conversation,
            &BTreeMap::new(),
            80,
            ConversationRenderState {
                collapse_thinking: true,
                thinking_streaming: false,
                assistant_streaming: false,
                now: Duration::ZERO,
                focused_call: None,
                unicode: UnicodeLevel::Extended,
            },
        )
    }

    fn settled_identity(index: usize, generation: u64) -> SettledConversation {
        SettledConversation {
            generation,
            transcript: crate::TranscriptId::Main,
            index,
        }
    }

    #[test]
    fn a_settled_turn_is_described_once_and_reused_across_frames() {
        reset_settled_conversation_test_state();
        let conversation = settled_conversation(false);

        let first = cached_settled_lines(&conversation, settled_identity(0, 0));
        let second = cached_settled_lines(&conversation, settled_identity(0, 0));

        assert_eq!(settled_conversation_test_renders(), 1);
        assert_eq!(joined(&first), joined(&second));
    }

    #[test]
    fn a_new_generation_retires_cached_rows_addressed_at_the_same_index() {
        reset_settled_conversation_test_state();
        let first = settled_conversation(false);
        let replacement = settled_conversation(true);

        let described = cached_settled_lines(&first, settled_identity(0, 0));
        let redescribed = cached_settled_lines(&replacement, settled_identity(0, 1));

        assert_eq!(settled_conversation_test_renders(), 2);
        assert_ne!(joined(&described), joined(&redescribed));
    }

    #[test]
    fn an_unfinished_turn_is_described_live_instead_of_cached() {
        reset_settled_conversation_test_state();
        let running = read_conversation(&["src/main.rs"]);

        cached_settled_lines(&running, settled_identity(0, 0));
        cached_settled_lines(&running, settled_identity(0, 0));

        assert_eq!(
            settled_conversation_test_renders(),
            0,
            "a turn with an unresolved tool call never enters the cache"
        );
    }

    #[test]
    fn a_settled_transcript_uses_only_the_semantic_palette() {
        for span in settled_lines(false).iter().flat_map(|line| &line.spans) {
            let Some(foreground) = span.style.fg else {
                continue;
            };
            assert!(
                SETTLED_PALETTE.contains(&foreground),
                "{:?} paints {foreground:?}, outside the transcript palette",
                span.content
            );
        }
    }

    #[test]
    fn failure_is_the_only_row_text_the_transcript_paints_in_a_warning_colour() {
        let lines = settled_lines(true);
        let error_text = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.style.fg == Some(RolePalette::error()))
            .map(|span| span.content.trim().to_owned())
            .filter(|content| !matches!(content.as_str(), "┃" | "❙" | "◆" | "◈" | "└"))
            .collect::<Vec<_>>();
        assert_eq!(
            error_text,
            vec!["Failure".to_owned()],
            "the collapsed row spends the warning colour only on its terminal state"
        );

        for span in lines.iter().flat_map(|line| &line.spans) {
            let Some(foreground) = span.style.fg else {
                continue;
            };
            assert!(
                SETTLED_PALETTE.contains(&foreground) || foreground == RolePalette::error(),
                "{:?} paints {foreground:?}",
                span.content
            );
        }
    }

    #[test]
    fn reasoning_and_trailing_metadata_are_muted_not_accented() {
        let mut conversation = Conversation::new("prompt");
        conversation
            .apply(crate::ConversationEvent::ReasoningDelta(
                "weighing the options".into(),
            ))
            .expect("reasoning should project");
        let lines = lines_at(&conversation, &BTreeMap::new());
        assert_eq!(
            span_style(&lines, "Thinking").fg,
            Some(RolePalette::muted())
        );
        assert_eq!(
            span_style(&lines, "weighing the options").fg,
            Some(RolePalette::muted())
        );

        let settled = settled_lines(false);
        assert_eq!(
            span_style(&settled, "Success").fg,
            Some(RolePalette::success())
        );
        assert_eq!(
            span_style(&settled, "1 lines · 25 B").fg,
            Some(RolePalette::muted())
        );
    }

    #[test]
    fn tool_row_text_and_bullet_update_to_the_same_terminal_state() {
        let succeeded = settled_lines(false);
        let failed = settled_lines(true);
        assert_eq!(
            span_style(&succeeded, "Success").fg,
            Some(RolePalette::success())
        );
        assert_eq!(
            span_style(&failed, "Failure").fg,
            Some(RolePalette::error())
        );

        assert_eq!(
            bullet_style(&succeeded, "cargo test").fg,
            Some(RolePalette::success())
        );
        assert_eq!(
            bullet_style(&failed, "cargo test").fg,
            Some(RolePalette::error())
        );
        assert_eq!(
            span_style(&succeeded, "cargo test").fg,
            Some(RolePalette::machine()),
            "tool operands stay neutral so lifecycle colours remain exclusive"
        );
        assert_eq!(
            span_style(&succeeded, "$").fg,
            Some(RolePalette::muted()),
            "a shell prompt is chrome, not a verb"
        );
    }

    #[test]
    fn narrow_markdown_tables_keep_their_structural_borders_without_wrapping() {
        let markdown =
            "| Name | State |\n| --- | --- |\n| very-long-resource-name | still-running |";
        let lines = MarkdownRenderer::new(Style::default(), "", 18).render(markdown);

        assert!(lines.iter().all(|line| line.width() <= 18), "{lines:?}");
        let body = lines
            .iter()
            .map(line_text)
            .find(|line| line.contains("very"))
            .expect("table body row");
        assert!(body.starts_with("│ "), "{body:?}");
        assert!(body.ends_with(" │"), "{body:?}");
        assert!(body.contains('…'), "{body:?}");

        for width in 1..=6 {
            let rows = MarkdownRenderer::new(Style::default(), "", width).render(markdown);
            assert!(rows.iter().all(|line| line.width() <= width), "{rows:?}");
            for row in rows.iter().map(line_text).filter(|row| row.contains('│')) {
                assert!(row.starts_with('│') && row.ends_with('│'), "{row:?}");
            }
            if width >= 2 {
                for (row, left, right) in [
                    (line_text(&rows[0]), '╭', '╮'),
                    (line_text(&rows[2]), '├', '┤'),
                    (line_text(rows.last().expect("table bottom")), '╰', '╯'),
                ] {
                    assert!(row.starts_with(left) && row.ends_with(right), "{row:?}");
                }
            }
        }

        let quoted = markdown
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = MarkdownRenderer::new(Style::default(), "", 18).render(&quoted);
        assert!(rows.iter().all(|line| line.width() <= 18), "{rows:?}");
        let rows = rows.iter().map(line_text).collect::<Vec<_>>();
        assert!(
            rows.iter().all(|row| row.width() == rows[0].width()),
            "quoted table borders do not align: {rows:?}"
        );
        assert!(rows[0].starts_with("│ ╭") && rows[0].ends_with('╮'));
        assert!(rows[2].starts_with("│ ├") && rows[2].ends_with('┤'));
        assert!(
            rows.last()
                .is_some_and(|row| row.starts_with("│ ╰") && row.ends_with('╯')),
            "{rows:?}"
        );
    }

    #[test]
    fn bounded_single_line_cuts_on_a_word_boundary_and_marks_the_cut() {
        assert_eq!(bounded_single_line("alpha bravo", 20), "alpha bravo");
        assert_eq!(bounded_single_line("alpha  \n bravo", 20), "alpha bravo");
        assert_eq!(
            bounded_single_line("alpha bravo charlie", 12),
            "alpha bravo…"
        );
        assert_eq!(
            bounded_single_line("alpha bravo, charlie", 14),
            "alpha bravo…"
        );
        assert_eq!(bounded_single_line("unbreakablesingleword", 6), "unbre…");
        assert_eq!(bounded_single_line("alpha bravo", 1), "…");
        assert_eq!(bounded_single_line("alpha bravo", 0), "");

        for max_width in 1..24usize {
            let bounded = bounded_single_line("alpha bravo charlie delta", max_width);
            assert!(
                bounded.width() <= max_width,
                "width {max_width}: {bounded:?}"
            );
        }
    }

    #[test]
    fn a_long_task_becomes_a_first_sentence_title_cut_on_a_word_boundary() {
        assert_eq!(
            compact_task_title(LONG_TASK, 80),
            "Investiga este proyecto sin modificar archivos",
            "a card prefers the first sentence over the whole prompt"
        );
        assert_eq!(
            compact_task_title(LONG_TASK, 24),
            "Investiga este proyecto…",
            "a first sentence that still overflows is cut on a word boundary"
        );
        assert_eq!(
            compact_task_title("no terminator here", 80),
            "no terminator here"
        );
        assert_eq!(
            compact_task_title(LONG_TASK, 6),
            "",
            "a title never shows a word fragment"
        );
        assert_eq!(compact_task_title("  ", 40), "");

        for max_width in 0..48usize {
            let title = compact_task_title(LONG_TASK, max_width);
            assert!(title.width() <= max_width, "width {max_width}: {title:?}");
        }
    }

    #[test]
    fn no_subagent_card_row_exceeds_the_columns_it_was_given() {
        let conversation = card_conversation(LONG_TASK);
        let card = conversation
            .subagent_cards
            .first()
            .expect("the card should project");

        for content_width in [1usize, 2, 4, 8, 12, 20, 24, 40, 60, 80] {
            for row in subagent_card_block(card, content_width, Duration::from_secs(3)).rows {
                assert!(
                    row.line.width() <= content_width,
                    "card row {:?} exceeds its {content_width}-column budget",
                    line_text(&row.line)
                );
            }
        }
    }

    #[test]
    fn the_working_indicator_never_outgrows_the_row_it_closes() {
        for content_width in [4usize, 8, 12, 20, 40, 80] {
            let line = turn_status_line(
                TurnStatus {
                    label: "Loading session…",
                    now: Duration::ZERO,
                    elapsed: Some(Duration::from_secs(41)),
                    tokens: Some(123_456),
                },
                content_width,
            );
            assert!(
                line.width() <= content_width,
                "width {content_width}: {:?}",
                line_text(&line)
            );
        }
    }

    #[test]
    fn result_size_label_reports_lines_and_bytes_not_tokens() {
        assert_eq!(result_size_label("alpha\nbravo"), "2 lines · 11 B");
        assert_eq!(result_size_label(""), "0 lines · 0 B");
        assert!(!result_size_label("x").contains("token"));
    }

    #[test]
    fn render_diff_uses_dual_gutter_and_row_backgrounds_without_plus_minus() {
        let mut lines = Vec::new();
        render_diff(
            &mut lines,
            &[
                crate::DiffLine::new(7, DiffLineKind::Removed, "old line"),
                crate::DiffLine::new(8, DiffLineKind::Added, "new line"),
            ],
            None,
            40,
        );
        assert_eq!(lines.len(), 2);

        let removed = &lines[0];
        let removed_text = line_text(removed);
        assert!(removed_text.contains('7'), "{removed_text:?}");
        assert!(removed_text.contains("old line"), "{removed_text:?}");
        assert!(
            !removed_text.contains(" - "),
            "no +/- noise: {removed_text:?}"
        );
        assert!(
            removed
                .spans
                .iter()
                .any(|span| span.style.bg == Some(RolePalette::diff_delete_bg())),
            "deleted rows carry the delete background"
        );

        let added = &lines[1];
        let added_text = line_text(added);
        assert!(added_text.contains('8'), "{added_text:?}");
        assert!(added_text.contains("new line"), "{added_text:?}");
        assert!(
            added
                .spans
                .iter()
                .any(|span| span.style.bg == Some(RolePalette::diff_insert_bg())),
            "inserted rows carry the insert background"
        );
    }

    /// A large diff painted only in red and green is a wall: the eye has no
    /// second signal to find the changed identifier by. The language supplies
    /// that signal without displacing the first — the wash still says added or
    /// removed, and the foreground says what the line is.
    #[test]
    fn a_rust_diff_is_coloured_by_the_language_on_top_of_its_wash() {
        let _guard = SYNTAX_CACHE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_syntax_cache();

        let diff = [
            crate::DiffLine::new(7, DiffLineKind::Removed, "let total = 0;"),
            crate::DiffLine::new(7, DiffLineKind::Added, "let total = compute(items);"),
        ];

        let mut plain = Vec::new();
        render_diff(&mut plain, &diff, None, 60);
        let mut highlighted = Vec::new();
        render_diff(&mut highlighted, &diff, Some("rust"), 60);

        assert_eq!(
            plain.iter().map(line_text).collect::<Vec<_>>(),
            highlighted.iter().map(line_text).collect::<Vec<_>>(),
            "highlighting repaints the diff, it does not rewrite it"
        );

        let colours = |lines: &[Line<'static>]| {
            let mut seen = Vec::new();
            for colour in lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter_map(|span| span.style.fg)
            {
                if !seen.contains(&colour) {
                    seen.push(colour);
                }
            }
            seen
        };
        assert!(
            colours(&highlighted).len() > colours(&plain).len(),
            "the language adds foreground distinctions the wash alone cannot make"
        );

        assert!(
            highlighted
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.style.bg == Some(RolePalette::diff_insert_bg())),
            "the added row keeps its wash: {highlighted:?}"
        );
        assert!(
            highlighted
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.style.bg == Some(RolePalette::diff_delete_bg())),
            "the removed row keeps its wash: {highlighted:?}"
        );
    }

    /// An extension the build carries no grammar for must leave the diff alone
    /// rather than name a language the store cannot load.
    #[test]
    fn only_extensions_backed_by_a_grammar_resolve_to_a_language() {
        assert_eq!(language_for_extension("rs"), Some("rust"));
        assert_eq!(language_for_extension("tsx"), Some("tsx"));
        assert_eq!(language_for_extension("yml"), Some("yaml"));
        assert_eq!(language_for_extension("swift"), None);
        assert_eq!(language_for_extension(""), None);
    }

    #[test]
    fn render_diff_collapses_unchanged_runs_into_a_gap_marker() {
        let mut lines = Vec::new();
        render_diff(
            &mut lines,
            &[
                crate::DiffLine::new(3, DiffLineKind::Added, "first change"),
                crate::DiffLine::new(20, DiffLineKind::Added, "second change"),
            ],
            None,
            40,
        );
        let joined: String = lines
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("… 16 unchanged lines"), "{joined:?}");
        assert!(joined.contains("first change"), "{joined:?}");
        assert!(joined.contains("second change"), "{joined:?}");
    }

    #[test]
    fn render_diff_caps_row_count_with_a_more_marker() {
        let diff: Vec<crate::DiffLine> = (0..(MAX_DIFF_ROWS as u32 + 25))
            .map(|index| {
                crate::DiffLine::new(index + 1, DiffLineKind::Added, format!("line {index}"))
            })
            .collect();
        let mut lines = Vec::new();
        render_diff(&mut lines, &diff, None, 40);
        let joined: String = lines
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("… 25 more lines"), "{joined:?}");
        assert!(
            lines.len() <= MAX_DIFF_ROWS + 1,
            "row count is capped: {}",
            lines.len()
        );
    }

    #[test]
    fn repeated_fenced_block_render_highlights_once() {
        let _guard = SYNTAX_CACHE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let markdown = "```js\nconst answer = 42;\n```\n";
        let source = "const answer = 42;\n";
        reset_syntax_cache();

        for _ in 0..3 {
            MarkdownRenderer::new(Style::default(), "", 80).render(markdown);
        }

        let highlight_calls = SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get);
        assert_eq!(highlight_calls, 1);

        let first = syntax_tokens("js", source).expect("cached JavaScript tokens");
        let second = syntax_tokens("js", source).expect("cached JavaScript tokens");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn syntax_cache_checks_content_when_hashes_collide() {
        let mut cache = SyntaxCache::with_limits(4, 1024);

        assert!(matches!(
            cache.lookup(7, "js", "first", false),
            SyntaxCacheLookup::Parse
        ));
        cache.complete(7, "js", "first", Some(empty_tokens()));

        assert!(matches!(
            cache.lookup(7, "js", "second", false),
            SyntaxCacheLookup::Parse
        ));
    }

    #[test]
    fn syntax_cache_evicts_to_strict_entry_and_byte_bounds() {
        let mut cache = SyntaxCache::with_limits(2, 32);

        for (hash, source) in [(1, "one"), (2, "two"), (3, "three")] {
            assert!(matches!(
                cache.lookup(hash, "js", source, false),
                SyntaxCacheLookup::Parse
            ));
            cache.complete(hash, "js", source, Some(empty_tokens()));
            assert!(cache.entries.len() <= 2);
            assert!(cache.retained_bytes <= 32);
        }

        assert!(matches!(
            cache.lookup(1, "js", "one", false),
            SyntaxCacheLookup::Parse
        ));
    }

    #[test]
    fn changing_large_prefixes_defer_parsing_and_remain_bounded() {
        let _guard = SYNTAX_CACHE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_syntax_cache();
        let mut latest = String::new();

        for index in 0..80 {
            latest = format!("{}-{index}", "x".repeat(SYNTAX_DEFER_SOURCE_BYTES));
            assert!(syntax_tokens("js", &latest).is_none());
        }

        assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 0);
        let cache = syntax_token_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(cache.entries.len() <= SYNTAX_CACHE_MAX_ENTRIES);
        assert!(cache.retained_bytes <= SYNTAX_CACHE_MAX_BYTES);
        drop(cache);

        assert!(syntax_tokens("js", &latest).is_some());
        assert!(syntax_tokens("js", &latest).is_some());
        assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn streaming_fenced_block_highlights_only_after_turn_completion() {
        let _guard = SYNTAX_CACHE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_syntax_cache();
        let mut conversation = Conversation::new("request");

        for delta in ["```js\n", "const answer", " = 42;\n", "```\n"] {
            conversation
                .apply(crate::ConversationEvent::MarkdownDelta(delta.into()))
                .expect("streaming markdown should project");
            let _ = conversation_lines(
                &conversation,
                &[],
                &BTreeMap::new(),
                80,
                conversation_state(true),
            );
            assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 0);
        }

        let final_markdown = conversation.live_markdown.clone();
        conversation
            .apply(crate::ConversationEvent::MarkdownFinal(final_markdown))
            .expect("completed markdown should project");
        let _ = conversation_lines(
            &conversation,
            &[],
            &BTreeMap::new(),
            80,
            conversation_state(false),
        );

        assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 1);
        let _ = conversation_lines(
            &conversation,
            &[],
            &BTreeMap::new(),
            80,
            conversation_state(false),
        );
        assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn restored_fence_is_neutral_on_first_content_frame_then_highlights_once_from_cache() {
        let _guard = SYNTAX_CACHE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_syntax_cache();
        let mut restored = Conversation::new("restored prompt");
        restored
            .apply(crate::ConversationEvent::MarkdownFinal(
                "```js\nconst restored = true;\n```\n".into(),
            ))
            .unwrap();

        let _ = conversation_lines(
            &restored,
            &[],
            &BTreeMap::new(),
            80,
            conversation_state(true),
        );
        assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 0);

        for _ in 0..2 {
            let _ = conversation_lines(
                &restored,
                &[],
                &BTreeMap::new(),
                80,
                conversation_state(false),
            );
        }
        assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn completed_fenced_block_stays_highlighted_while_next_turn_streams() {
        let _guard = SYNTAX_CACHE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_syntax_cache();
        let markdown = "```js\nconst completed = true;\n```\n";
        let mut completed = Conversation::new("first");
        completed
            .apply(crate::ConversationEvent::MarkdownFinal(markdown.into()))
            .expect("completed markdown should project");
        let completed_lines = conversation_lines(
            &completed,
            &[],
            &BTreeMap::new(),
            80,
            conversation_state(false),
        );
        let mut streaming = Conversation::new("second");
        streaming
            .apply(crate::ConversationEvent::MarkdownDelta(
                "```js\nconst streaming =".into(),
            ))
            .expect("streaming markdown should project");
        let _ = conversation_lines(
            &streaming,
            &[],
            &BTreeMap::new(),
            80,
            conversation_state(true),
        );

        assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 1);
        assert!(
            completed_lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| {
                    span.content.contains("const")
                        && span.style.fg != Some(RolePalette::assistant())
                })
        );
    }

    #[test]
    fn oversized_fenced_block_stays_neutral_without_highlighting() {
        let _guard = SYNTAX_CACHE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_syntax_cache();
        let source = "x".repeat(SYNTAX_MAX_SOURCE_BYTES + 1);
        let markdown = format!("```js\n{source}\n```\n");
        let neutral = Style::default().fg(RolePalette::assistant());

        let lines = MarkdownRenderer::new(neutral, "", 80).render(&markdown);
        let code_style = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.starts_with('x'))
            .map(|span| span.style)
            .expect("oversized code body");

        assert_eq!(code_style.fg, Some(RolePalette::assistant()));
        assert_eq!(code_style.bg, Some(code_block_background()));
        assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 0);
    }
}
