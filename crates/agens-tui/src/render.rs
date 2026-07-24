//! Rich presentation of typed runtime details without mutating their source data.

use std::{
    collections::{BTreeSet, VecDeque, hash_map::DefaultHasher},
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
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::conversation::ConversationItem;
use crate::widgets::{RolePalette, ThinkingBlock, ToolRow};
use crate::{Conversation, DiffLineKind, ToolResultState, TuiRuntimeEvent};

const MAX_VISIBLE_TOOL_OUTPUT_BYTES: usize = 4 * 1024;
const VISIBLE_TOOL_OUTPUT_MARKER: &str = "\n… visible output truncated";
const SYNTAX_CACHE_MAX_ENTRIES: usize = 64;
const SYNTAX_CACHE_MAX_BYTES: usize = 4 * 1024 * 1024;
const SYNTAX_DEFER_SOURCE_BYTES: usize = 4 * 1024;
const SYNTAX_MAX_SOURCE_BYTES: usize = 32 * 1024;

pub(super) fn conversation_lines(
    conversation: &Conversation,
    events: &[TuiRuntimeEvent],
    collapsed_tool_outputs: &BTreeSet<String>,
    collapse_thinking: bool,
    thinking_streaming: bool,
    assistant_streaming: bool,
    content_width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let content_width = usize::from(content_width.max(1));

    for item in &conversation.items {
        match item {
            ConversationItem::Info(text) => line(&mut lines, "INFO", RolePalette::info(), text),
            ConversationItem::User(text) => user_lines(&mut lines, text),
            ConversationItem::Assistant(text) => {
                markdown_lines_with_syntax(
                    &mut lines,
                    text,
                    Style::default().fg(RolePalette::assistant()),
                    "",
                    content_width,
                    !assistant_streaming,
                );
                lines.push(Line::default());
            }
            ConversationItem::Reasoning(text) => {
                thinking_lines(
                    &mut lines,
                    text,
                    collapse_thinking,
                    thinking_streaming,
                    content_width,
                );
            }
            ConversationItem::ToolCall {
                call_id: _,
                name,
                input,
                batch,
            } => {
                if let Some(batch) = batch {
                    lines.push(Line::from(Span::styled(
                        format!("Tools · batch {batch}"),
                        Style::default()
                            .fg(RolePalette::tool())
                            .add_modifier(Modifier::BOLD),
                    )));
                }
                lines.push(ToolRow::header(name));
                lines.push(ToolRow::args(input));
            }
            ConversationItem::ToolResult {
                call_id,
                output,
                is_error,
            } => {
                let (result_state, duration) = tool_state(events, call_id, *is_error);
                let color = result_color(result_state);
                let tool_name = tool_name_for_call(conversation, call_id);
                let status = format!("{result_state:?}{}", duration_label(duration));
                lines.push(ToolRow::result_footer(&tool_name, &status, color));
                if collapsed_tool_outputs.contains(call_id) {
                    if *is_error {
                        lines.push(ToolRow::collapsed_failure(output));
                    } else {
                        lines.push(ToolRow::collapsed_output());
                    }
                    lines.push(Line::default());
                } else {
                    markdown_lines(
                        &mut lines,
                        &bounded_visible_tool_output(output),
                        Style::default().fg(RolePalette::chrome()),
                        "",
                        content_width,
                    );
                }
            }
            ConversationItem::Diff(diff) => {
                for change in diff {
                    diff_line(&mut lines, change.number, change.kind, &change.text);
                }
            }
            ConversationItem::Error(error) => {
                lines.push(Line::from(Span::styled(
                    "┌ Error",
                    Style::default()
                        .fg(RolePalette::error())
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(RolePalette::error())),
                    Span::raw(error.message.clone()),
                ]));
                lines.push(Line::from(Span::styled(
                    format!("└ Action: {}", error.action),
                    Style::default().fg(RolePalette::warning()),
                )));
                lines.push(Line::default());
            }
            ConversationItem::SubagentCard(id) => {
                let Some(card) = conversation
                    .subagent_cards
                    .iter()
                    .find(|card| card.id == *id)
                else {
                    continue;
                };
                let status = match card.status {
                    Some(crate::TuiSubagentStatus::Success) => "success",
                    Some(crate::TuiSubagentStatus::Failure) => "failure",
                    Some(crate::TuiSubagentStatus::Cancelled) => "cancelled",
                    None => match card.presentation {
                        crate::TuiExecutionState::ForegroundRunning => "foreground running",
                        crate::TuiExecutionState::BackgroundRunning => "background running",
                        _ => "running",
                    },
                };
                lines.push(Line::from(format!(
                    "Subagent {} · {} · {status} · {} · {} tool uses",
                    card.id, card.agent, card.task_summary, card.tool_uses
                )));
                lines.push(Line::default());
            }
        }
    }
    lines
}

fn tool_name_for_call(conversation: &Conversation, call_id: &str) -> String {
    conversation
        .tool_batches
        .iter()
        .flat_map(|batch| &batch.calls)
        .find(|call| call.call_id == call_id)
        .map(|call| call.name.clone())
        .unwrap_or_else(|| "tool".into())
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

fn user_lines(lines: &mut Vec<Line<'static>>, text: &str) {
    let mut first = true;
    for source_line in text.split('\n') {
        let prefix = if first { "❯ " } else { "  " };
        first = false;
        lines.push(Line::from(vec![
            Span::styled(
                prefix,
                Style::default()
                    .fg(RolePalette::user_bar())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                source_line.to_owned(),
                Style::default()
                    .fg(RolePalette::user())
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::default());
}

fn thinking_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    collapsed: bool,
    streaming: bool,
    content_width: usize,
) {
    let mode = ThinkingBlock::mode(streaming, collapsed);
    lines.push(ThinkingBlock::title(mode));
    if mode.shows_body() {
        markdown_lines(
            lines,
            text,
            Style::default().fg(RolePalette::chrome()),
            "",
            content_width,
        );
    } else {
        lines.push(Line::default());
    }
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
            | TuiRuntimeEvent::Diff { .. }
            | TuiRuntimeEvent::TaskExecution { .. }
            | TuiRuntimeEvent::SubagentExecution(_)
            | TuiRuntimeEvent::RestoredCompletedSubagent { .. } => {}
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
    lines.push(Line::default());
}

struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    base_style: Style,
    prefix: String,
    strong: usize,
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
        }
    }

    fn render(mut self, markdown: &str) -> Vec<Line<'static>> {
        self.code_panel_widths = code_panel_widths(markdown, self.content_width);
        for event in Parser::new(markdown) {
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
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.text(&text, self.current_style())
            }
            Event::Code(code) if self.code_block => self.code_text(&code),
            Event::Code(code) => self.text(
                &code,
                self.current_style()
                    .fg(RolePalette::warning())
                    .bg(code_block_background())
                    .add_modifier(Modifier::BOLD),
            ),
            Event::SoftBreak | Event::HardBreak => self.finish_line(),
            Event::Rule => {
                self.finish_line();
                self.text("────────────────", self.base_style.fg(Color::DarkGray));
                self.finish_line();
            }
            Event::TaskListMarker(checked) => self.text(
                if checked { "[x] " } else { "[ ] " },
                self.base_style
                    .fg(RolePalette::tool())
                    .add_modifier(Modifier::BOLD),
            ),
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
                self.text(
                    &format!("{}{marker}", "  ".repeat(depth)),
                    self.base_style
                        .fg(RolePalette::tool())
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
            Tag::Paragraph
            | Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::Strikethrough
            | Tag::Image { .. }
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
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
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item => self.finish_block(),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::BlockQuote(_) => {
                self.finish_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.finish_line();
                self.lists.pop();
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
                        self.base_style.fg(Color::Blue).add_modifier(Modifier::DIM),
                    );
                }
            }
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::Strikethrough
            | TagEnd::Image
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
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

    fn push_code_span(&mut self, text: &str, style: Style) {
        let used = Line::from(self.spans.clone()).width();
        let available = self.code_panel_width.saturating_sub(used.saturating_add(2));
        let text = take_visible_width(text, available);
        if !text.is_empty() {
            self.spans.push(Span::styled(text, style));
        }
    }

    fn current_style(&self) -> Style {
        let mut style = self.base_style;
        if let Some(level) = self.heading {
            style = style
                .fg(match level {
                    HeadingLevel::H1 => RolePalette::user_bar(),
                    HeadingLevel::H2 => RolePalette::brand(),
                    _ => RolePalette::tool(),
                })
                .add_modifier(Modifier::BOLD);
            if matches!(level, HeadingLevel::H1) {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
        }
        if self.strong > 0 {
            style = style
                .fg(RolePalette::user_bar())
                .add_modifier(Modifier::BOLD);
        }
        if self.emphasis > 0 {
            style = style
                .fg(RolePalette::thinking())
                .add_modifier(Modifier::ITALIC);
        }
        if !self.links.is_empty() {
            style = style
                .fg(RolePalette::tool())
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
                self.base_style.fg(Color::DarkGray),
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
                self.base_style.fg(RolePalette::chrome()),
            ));
        }
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
        self.lines.push(Line::from(std::mem::take(&mut self.spans)));
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

fn take_visible_width(text: &str, max_width: usize) -> String {
    let mut clipped = String::new();
    let mut width = 0_usize;
    for character in text.chars() {
        let character_width = Line::from(character.to_string()).width();
        if width.saturating_add(character_width) > max_width {
            break;
        }
        clipped.push(character);
        width += character_width;
    }
    clipped
}

/// Panel background for fenced/inline code — slightly elevated over the default terminal bg.
const fn code_block_background() -> Color {
    Color::Rgb(0x1a, 0x1f, 0x29)
}

fn diff_line(lines: &mut Vec<Line<'static>>, number: u32, kind: DiffLineKind, text: &str) {
    let (marker, color) = match kind {
        DiffLineKind::Added => ('+', Color::Green),
        DiffLineKind::Removed => ('-', Color::Red),
        DiffLineKind::Context => (' ', Color::Gray),
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

fn line(lines: &mut Vec<Line<'static>>, label: &str, color: Color, text: impl Into<String>) {
    for text_line in text.into().split('\n') {
        lines.push(Line::from(vec![
            Span::styled(
                format!("│ {label:<9} "),
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
        ToolResultState::Success => RolePalette::success(),
        ToolResultState::Failure => RolePalette::error(),
    }
}

#[cfg(test)]
thread_local! {
    static SYNTAX_HIGHLIGHT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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

    fn reset_syntax_cache() {
        reset_syntax_highlight_test_state();
    }

    fn empty_tokens() -> Arc<[SyntaxToken]> {
        Arc::from(Vec::<SyntaxToken>::new())
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
            let _ =
                conversation_lines(&conversation, &[], &BTreeSet::new(), false, false, true, 80);
            assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 0);
        }

        let final_markdown = conversation.live_markdown.clone();
        conversation
            .apply(crate::ConversationEvent::MarkdownFinal(final_markdown))
            .expect("completed markdown should project");
        let _ = conversation_lines(
            &conversation,
            &[],
            &BTreeSet::new(),
            false,
            false,
            false,
            80,
        );

        assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 1);
        let _ = conversation_lines(
            &conversation,
            &[],
            &BTreeSet::new(),
            false,
            false,
            false,
            80,
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

        let _ = conversation_lines(&restored, &[], &BTreeSet::new(), false, false, true, 80);
        assert_eq!(SYNTAX_HIGHLIGHT_CALLS.with(std::cell::Cell::get), 0);

        for _ in 0..2 {
            let _ = conversation_lines(&restored, &[], &BTreeSet::new(), false, false, false, 80);
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
        let completed_lines =
            conversation_lines(&completed, &[], &BTreeSet::new(), false, false, false, 80);
        let mut streaming = Conversation::new("second");
        streaming
            .apply(crate::ConversationEvent::MarkdownDelta(
                "```js\nconst streaming =".into(),
            ))
            .expect("streaming markdown should project");
        let _ = conversation_lines(&streaming, &[], &BTreeSet::new(), false, false, true, 80);

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
