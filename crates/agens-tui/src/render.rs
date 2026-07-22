//! Rich presentation of typed runtime details without mutating their source data.

use std::time::Duration;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};
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
    collapse_thinking: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    for item in &conversation.items {
        match item {
            ConversationItem::Info(text) => line(&mut lines, "INFO", Color::Yellow, text),
            ConversationItem::User(text) => user_lines(&mut lines, text),
            ConversationItem::Assistant(text) => {
                markdown_lines(&mut lines, text, Style::default(), "");
            }
            ConversationItem::Reasoning(text) => {
                thinking_lines(&mut lines, text, collapse_thinking);
            }
            ConversationItem::ToolCall {
                call_id,
                name,
                input,
                batch,
            } => {
                if let Some(batch) = batch {
                    lines.push(Line::from(Span::styled(
                        format!("Tools · batch {batch}"),
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    )));
                }
                lines.push(Line::from(vec![
                    Span::styled("┌ ", Style::default().fg(Color::Magenta)),
                    Span::styled(
                        name.to_owned(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" · {call_id}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("│ input ", Style::default().fg(Color::DarkGray)),
                    Span::raw(input.to_owned()),
                ]));
            }
            ConversationItem::ToolResult {
                call_id,
                output,
                is_error,
            } => {
                let (result_state, duration) = tool_state(events, call_id, *is_error);
                let color = result_color(result_state);
                lines.push(Line::from(vec![
                    Span::styled("└ ", Style::default().fg(color)),
                    Span::styled(
                        call_id.to_owned(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" · {result_state:?}{}", duration_label(duration)),
                        Style::default().fg(color),
                    ),
                ]));
                if collapsed_tool_outputs.contains(call_id) {
                    lines.push(Line::from(Span::styled(
                        "output collapsed; expand to recover",
                        Style::default().fg(Color::Gray),
                    )));
                    lines.push(Line::default());
                } else {
                    markdown_lines(&mut lines, output, Style::default().fg(Color::Gray), "");
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
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(Color::Red)),
                    Span::raw(error.message.clone()),
                ]));
                lines.push(Line::from(Span::styled(
                    format!("└ Action: {}", error.action),
                    Style::default().fg(Color::Yellow),
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
                let status = match card.presentation {
                    crate::TuiExecutionState::ForegroundRunning => "foreground running",
                    crate::TuiExecutionState::BackgroundRunning => "background running",
                    _ => "running",
                };
                lines.push(Line::from(format!(
                    "Subagent {} · {status} · {}",
                    card.agent, card.task_summary
                )));
                let marker = format!("subagent:{}", card.id);
                if collapsed_tool_outputs.contains(&marker) {
                    lines.push(Line::from(format!("+{} tool uses", card.tool_calls.len())));
                } else {
                    for call in &card.tool_calls {
                        lines.push(Line::from(format!("┌ {} · {}", call.name, call.call_id)));
                        if let Some(result) = &call.result {
                            lines.push(Line::from(format!("└ {}", result.output)));
                        }
                    }
                }
                lines.push(Line::default());
            }
        }
    }
    lines
}

fn user_lines(lines: &mut Vec<Line<'static>>, text: &str) {
    lines.push(Line::from(Span::styled(
        "You",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    for source_line in text.split('\n') {
        lines.push(Line::from(Span::raw(source_line.to_owned())));
    }
    lines.push(Line::default());
}

fn thinking_lines(lines: &mut Vec<Line<'static>>, text: &str, collapsed: bool) {
    let title = if collapsed {
        "Thinking · collapsed"
    } else {
        "Thinking"
    };
    lines.push(Line::from(Span::styled(
        title,
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    )));
    if collapsed {
        lines.push(Line::default());
    } else {
        markdown_lines(lines, text, Style::default().fg(Color::Gray), "");
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
            | TuiRuntimeEvent::SubagentExecution(_) => {}
        }
    }

    lines
}

fn markdown_lines(lines: &mut Vec<Line<'static>>, markdown: &str, base_style: Style, prefix: &str) {
    if markdown.is_empty() {
        return;
    }

    lines.extend(MarkdownRenderer::new(base_style, prefix).render(markdown));
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
    quote_depth: usize,
    lists: Vec<Option<u64>>,
    links: Vec<String>,
}

impl MarkdownRenderer {
    fn new(base_style: Style, prefix: &str) -> Self {
        Self {
            lines: Vec::new(),
            spans: Vec::new(),
            base_style,
            prefix: prefix.to_owned(),
            strong: 0,
            emphasis: 0,
            heading: None,
            code_block: false,
            quote_depth: 0,
            lists: Vec::new(),
            links: Vec::new(),
        }
    }

    fn render(mut self, markdown: &str) -> Vec<Line<'static>> {
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
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.text(&text, self.current_style())
            }
            Event::Code(code) => self.text(
                &code,
                self.current_style()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::DIM),
            ),
            Event::SoftBreak | Event::HardBreak => self.finish_line(),
            Event::Rule => {
                self.finish_line();
                self.text("────────────────", self.base_style.fg(Color::DarkGray));
                self.finish_line();
            }
            Event::TaskListMarker(checked) => {
                self.text(if checked { "[x] " } else { "[ ] " }, self.base_style)
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
                self.text(&format!("{}{marker}", "  ".repeat(depth)), self.base_style);
            }
            Tag::CodeBlock(kind) => {
                self.finish_line();
                self.code_block = true;
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.is_empty()
                {
                    self.text(&language, self.base_style.fg(Color::DarkGray));
                    self.finish_line();
                }
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
                self.code_block = false;
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
        for (index, segment) in text.split('\n').enumerate() {
            if index > 0 {
                self.finish_line();
            }
            self.start_line();
            if !segment.is_empty() {
                self.spans.push(Span::styled(segment.to_owned(), style));
            }
        }
    }

    fn current_style(&self) -> Style {
        let mut style = self.base_style;
        if self.strong > 0 || self.heading.is_some() {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.emphasis > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if !self.links.is_empty() {
            style = style.fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
        }
        if self.code_block {
            style = style.fg(Color::Gray);
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
        if self.quote_depth > 0 {
            self.spans.push(Span::styled(
                "│ ".repeat(self.quote_depth),
                self.base_style.fg(Color::DarkGray),
            ));
        }
    }

    fn finish_line(&mut self) {
        if !self.spans.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.spans)));
        }
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
        ToolResultState::Success => Color::Green,
        ToolResultState::Failure => Color::Red,
    }
}
