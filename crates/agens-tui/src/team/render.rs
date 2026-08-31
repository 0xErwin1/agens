//! Painting the supervision surface.
//!
//! The board reads left to right: the tree names every node the daemon holds,
//! and the panel beside it says everything known about the one the reader is
//! standing on. Nothing here reads state that the surface was not handed.

use std::io;

use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
};

use super::model::{TeamEventClass, TeamNode, TeamNodeDetail, TeamNodeKind, TeamState};
use super::{TeamRow, TeamSurface, TeamTab};
use crate::widgets::{ColorLevel, UnicodeLevel, quantize_buffer};

/// How wide the tree is against the context panel beside it.
const TREE_PERCENT: u16 = 55;
/// How many journal lines each class shows before the older ones are dropped.
const JOURNAL_LINES: usize = 6;

const DIM: Style = Style::new().fg(Color::DarkGray);

/// Draws the supervision surface onto a terminal.
///
/// Separate from the conversation's renderer on purpose: this surface has its
/// own state and its own frame, and neither one has to know about the other to
/// be painted or tested.
pub struct TeamScreen<B: Backend> {
    terminal: Terminal<B>,
    color_level: ColorLevel,
    unicode_level: UnicodeLevel,
}

impl<B: Backend> TeamScreen<B> {
    pub const fn new(
        terminal: Terminal<B>,
        color_level: ColorLevel,
        unicode_level: UnicodeLevel,
    ) -> Self {
        Self {
            terminal,
            color_level,
            unicode_level,
        }
    }

    pub const fn terminal(&self) -> &Terminal<B> {
        &self.terminal
    }

    /// Paints one frame of the board.
    pub fn draw(&mut self, surface: &TeamSurface) -> io::Result<()> {
        let color_level = self.color_level;
        let unicode_level = self.unicode_level;

        self.terminal
            .draw(|frame| {
                render_team(frame, surface, unicode_level);
                quantize_buffer(frame.buffer_mut(), color_level);
            })
            .map(|_| ())
            .map_err(|_| io::Error::other("Ratatui draw failed"))
    }
}

fn render_team(frame: &mut Frame<'_>, surface: &TeamSurface, level: UnicodeLevel) {
    let area = frame.area();
    let [header, tabs, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

    frame.render_widget(header_line(surface), header);
    frame.render_widget(tab_line(surface.tab()), tabs);
    render_body(frame, surface, level, body);
    frame.render_widget(footer_line(level), footer);
}

fn render_body(frame: &mut Frame<'_>, surface: &TeamSurface, level: UnicodeLevel, body: Rect) {
    if surface.is_expanded() {
        frame.render_widget(detail_pane(surface, level, true), body);
        return;
    }

    let [tree, context] = Layout::horizontal([
        Constraint::Percentage(TREE_PERCENT),
        Constraint::Percentage(100 - TREE_PERCENT),
    ])
    .areas(body);

    frame.render_widget(tree_pane(surface, level), tree);
    frame.render_widget(detail_pane(surface, level, false), context);
}

/// What the fleet amounts to, counted rather than described.
fn header_line(surface: &TeamSurface) -> Paragraph<'static> {
    let nodes: Vec<&TeamNode> = surface
        .snapshot()
        .repos
        .iter()
        .flat_map(|repo| &repo.nodes)
        .collect();
    let runs = nodes
        .iter()
        .filter(|node| node.kind == TeamNodeKind::Run)
        .count();
    let chats = nodes.len() - runs;
    let parked = nodes.iter().filter(|node| node.state.is_parked()).count();

    let mut spans = vec![
        Span::styled(" team ", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(format!("· {runs} runs · {chats} chats"), DIM),
    ];
    if parked > 0 {
        spans.push(Span::styled(
            format!(" · {parked} parked"),
            Style::new().fg(Color::Yellow),
        ));
    }

    Paragraph::new(Line::from(spans))
}

/// The numbered tabs, with zero always the conversation.
fn tab_line(active: TeamTab) -> Paragraph<'static> {
    let mut spans = vec![Span::raw(" ")];

    for (digit, name) in TeamTab::HEADER {
        let style = if digit == active.digit() {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            DIM
        };
        spans.push(Span::styled(format!("[{digit} {name}] "), style));
    }

    Paragraph::new(Line::from(spans))
}

fn footer_line(level: UnicodeLevel) -> Paragraph<'static> {
    let enter = match level {
        UnicodeLevel::Extended => "⏎",
        UnicodeLevel::Ascii => "enter",
    };
    let arrows = match level {
        UnicodeLevel::Extended => "↑↓",
        UnicodeLevel::Ascii => "up/down",
    };

    Paragraph::new(Line::from(Span::styled(
        format!(" {arrows} move   {enter} detail   0 chat   esc back "),
        DIM,
    )))
}

fn tree_pane<'a>(surface: &'a TeamSurface, level: UnicodeLevel) -> Paragraph<'a> {
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(" tree ");
    let rows = surface.rows();

    if rows.is_empty() {
        return Paragraph::new(Line::from(Span::styled(
            "the daemon holds no runs or chats",
            DIM,
        )))
        .block(block);
    }

    let mut lines = Vec::new();
    for row in rows {
        match row {
            TeamRow::Repo(id) => {
                if !lines.is_empty() {
                    lines.push(Line::default());
                }
                lines.push(Line::from(Span::styled(
                    id.to_owned(),
                    Style::new().add_modifier(Modifier::BOLD),
                )));
            }
            TeamRow::Node { node, depth } => {
                let selected = surface.selected() == Some(node.id);
                lines.extend(node_lines(node, depth, selected, level));
            }
        }
    }

    Paragraph::new(lines).block(block)
}

/// One node as the two lines it occupies: what it is, and how it is going.
fn node_lines(
    node: &TeamNode,
    depth: usize,
    selected: bool,
    level: UnicodeLevel,
) -> Vec<Line<'static>> {
    let indent = "  ".repeat(depth);
    let marker = if selected {
        match level {
            UnicodeLevel::Extended => "❯",
            UnicodeLevel::Ascii => ">",
        }
    } else {
        " "
    };
    let title_style = if selected {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let mut title = vec![
        Span::styled(format!("{marker}{indent} "), title_style),
        Span::styled(
            node.state.glyph(level).to_owned(),
            Style::new().fg(state_color(&node.state)),
        ),
        Span::styled(format!(" {} ", node.id), DIM),
        Span::styled(node.title.clone(), title_style),
    ];
    if node.is_self {
        title.push(Span::styled(" (this terminal)", DIM));
    }

    let mut facts = vec![
        node.parked_line()
            .unwrap_or_else(|| node.state.label().to_owned()),
    ];
    let metrics = node.metrics_line();
    if !metrics.is_empty() {
        facts.push(metrics);
    }

    vec![
        Line::from(title),
        Line::from(Span::styled(
            format!("{}   {indent}{}", " ", facts.join(" · ")),
            DIM,
        )),
    ]
}

const fn state_color(state: &TeamState) -> Color {
    match state {
        TeamState::Running | TeamState::Answering => Color::Green,
        TeamState::AwaitingInput | TeamState::AwaitingQuota => Color::Yellow,
        TeamState::Failed => Color::Red,
        TeamState::Done => Color::Cyan,
        _ => Color::DarkGray,
    }
}

/// The selected node, in as much detail as the host has fetched.
fn detail_pane<'a>(surface: &TeamSurface, level: UnicodeLevel, expanded: bool) -> Paragraph<'a> {
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(if expanded { " detail " } else { " context " });

    let Some(node) = surface.selected_node() else {
        return Paragraph::new(Line::from(Span::styled("nothing selected", DIM))).block(block);
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                node.state.glyph(level).to_owned(),
                Style::new().fg(state_color(&node.state)),
            ),
            Span::raw(format!(" {} {}", node.kind.label(), node.id)),
        ]),
        Line::from(Span::styled(node.title.clone(), DIM)),
        Line::default(),
    ];

    lines.push(field("state", node.state.label().to_owned()));
    if let Some(parked) = node.parked_line() {
        lines.push(field("waiting", parked));
    }
    if let Some(attempt) = node.attempt {
        lines.push(field("attempt", attempt.to_string()));
    }
    if let Some(model) = &node.model {
        lines.push(field("model", model.clone()));
    }
    if let Some(cost) = node.cost_micros {
        lines.push(field("cost", super::format_cost(cost)));
    }
    if let Some(duration) = node.duration {
        lines.push(field("elapsed", super::format_duration(duration)));
    }

    match surface.detail() {
        Some(detail) => lines.extend(detail_lines(detail, expanded)),
        None => {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled("reading the run…", DIM)));
        }
    }

    Paragraph::new(lines).block(block)
}

fn field(name: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{name:<9}"), DIM),
        Span::raw(value),
    ])
}

/// Everything the daemon reported about the run, with the journal split by the
/// class it came from.
fn detail_lines(detail: &TeamNodeDetail, expanded: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    if let Some(worktree) = &detail.worktree {
        lines.push(field("worktree", worktree.clone()));
    }

    if !detail.attempts.is_empty() {
        lines.push(Line::default());
        lines.push(section("attempts"));
        for attempt in &detail.attempts {
            let outcome = attempt.outcome.as_deref().unwrap_or("running");
            let mut text = format!("{} {outcome}", attempt.n);
            if let Some(cost) = attempt.cost_micros {
                text.push_str(&format!(" · {}", super::format_cost(cost)));
            }
            if let Some(duration) = attempt.duration {
                text.push_str(&format!(" · {}", super::format_duration(duration)));
            }
            lines.push(Line::from(Span::raw(text)));
        }
    }

    if !detail.questions.is_empty() {
        lines.push(Line::default());
        lines.push(section("questions"));
        for question in &detail.questions {
            lines.push(Line::from(Span::raw(format!(
                "{} {} · {}",
                question.question_id,
                question.waiting_label(),
                question.blocked_decision
            ))));
        }
    }

    if !expanded {
        return lines;
    }

    for class in [TeamEventClass::Infra, TeamEventClass::Agent] {
        let events = detail.events_of(class);
        if events.is_empty() {
            continue;
        }

        lines.push(Line::default());
        lines.push(section(class.label()));
        for event in events.iter().rev().take(JOURNAL_LINES).rev() {
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", event.kind), Style::new()),
                Span::styled(one_line(&event.payload), DIM),
            ]));
        }
    }

    lines
}

fn section(name: &str) -> Line<'static> {
    Line::from(Span::styled(
        name.to_owned(),
        Style::new().add_modifier(Modifier::BOLD),
    ))
    .alignment(Alignment::Left)
}

/// Collapses a payload to one bounded line: raw JSON must not push the rest of
/// the panel off the frame.
fn one_line(payload: &str) -> String {
    let collapsed: String = payload
        .chars()
        .map(|character| {
            if character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let mut line: String = collapsed.chars().take(80).collect();

    if collapsed.chars().count() > 80 {
        line.push('…');
    }

    line
}
