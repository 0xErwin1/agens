//! Soft overlay taxonomy over the existing single dialog + slash palette layer.
//!
//! Rendering is split in two phases on purpose: [`OverlayLayout::solve`] is pure
//! geometry (no `Frame`, fully unit-testable, never panics), and
//! [`OverlayFrame::render`] only paints what that geometry already decided.

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use super::RolePalette;

/// Overlay kinds for the one modal layer (palette, list picker, or confirm).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum OverlayKind {
    /// Slash command palette above the composer.
    Palette,
    /// List selection / search dialog (model, session, file, …).
    #[default]
    Picker,
    /// Permission (or similar) confirm with short-key answers.
    Confirm,
    /// Composer-anchored `@` reference picker filtered by the typed token.
    FilePicker,
}

/// Shell helpers for overlay kind classification and Confirm short keys.
pub(crate) struct OverlayShell;

impl OverlayShell {
    /// Topmost kind: open palette wins over any dialog, which wins over the
    /// composer-anchored file picker.
    pub(crate) const fn topmost(
        palette_open: bool,
        dialog_kind: Option<OverlayKind>,
        file_picker_open: bool,
    ) -> Option<OverlayKind> {
        if palette_open {
            Some(OverlayKind::Palette)
        } else if dialog_kind.is_some() {
            dialog_kind
        } else if file_picker_open {
            Some(OverlayKind::FilePicker)
        } else {
            None
        }
    }

    /// Maps Confirm short keys to permission answer tokens used in action ids.
    pub(crate) const fn confirm_answer(key: char) -> Option<&'static str> {
        match key {
            'a' => Some("allow-once"),
            'd' => Some("deny-once"),
            'A' => Some("allow-always"),
            'D' => Some("deny-always"),
            _ => None,
        }
    }

    /// Whether an action id ends with the Confirm answer suffix (`:allow-once`, …).
    pub(crate) fn action_matches_answer(action_id: &str, answer: &str) -> bool {
        action_id
            .rsplit_once(':')
            .is_some_and(|(_, suffix)| suffix == answer)
    }
}

/// Keybinding hint rendered in the overlay footer.
pub(crate) struct OverlayShortcut<'a> {
    pub(crate) key: &'a str,
    pub(crate) label: &'a str,
}

impl OverlayShortcut<'_> {
    fn columns(&self) -> usize {
        match (self.key.width(), self.label.width()) {
            (0, label) => label,
            (key, 0) => key,
            (key, label) => key + 1 + label,
        }
    }

    fn spans(&self) -> Vec<Span<'static>> {
        let key = Span::styled(
            self.key.to_owned(),
            Style::default()
                .fg(RolePalette::accent_active())
                .add_modifier(Modifier::BOLD),
        );
        let label = Span::styled(
            self.label.to_owned(),
            Style::default().fg(RolePalette::muted()),
        );
        match (self.key.is_empty(), self.label.is_empty()) {
            (true, _) => vec![label],
            (_, true) => vec![key],
            _ => vec![key, Span::raw(" "), label],
        }
    }
}

/// Optional tab strip owned by the shell because it competes for height.
pub(crate) struct OverlayTabs<'a> {
    pub(crate) labels: &'a [&'a str],
    pub(crate) active: usize,
}

/// Where the frame is placed inside the received area.
#[derive(Clone, Copy)]
pub(crate) enum OverlayAnchor {
    Center,
    /// Bottom-anchored directly above the given rect (the composer).
    Above(Rect),
}

/// Relative sizing preset. All values are resolved against the received area.
#[derive(Clone, Copy)]
pub(crate) struct OverlaySizing {
    pub(crate) width_pct: u16,
    pub(crate) min_width: u16,
    pub(crate) max_width: u16,
    pub(crate) max_height: u16,
    pub(crate) v_margin: u16,
    pub(crate) h_pad: u16,
    pub(crate) v_pad: u16,
    pub(crate) anchor: OverlayAnchor,
}

impl OverlaySizing {
    /// Default centered overlay for list and detail dialogs.
    ///
    /// Vertical margins are zero on purpose: they are absolute rows subtracted
    /// before degradation, so on a short terminal they silently cost content
    /// rows instead of yielding. `max_height` already keeps the frame from
    /// swallowing a tall terminal.
    pub(crate) const fn dialog() -> Self {
        Self {
            width_pct: 80,
            min_width: 48,
            max_width: 96,
            max_height: 18,
            v_margin: 0,
            h_pad: 1,
            v_pad: 0,
            anchor: OverlayAnchor::Center,
        }
    }

    /// Small centered overlay for confirms and short prompts.
    pub(crate) const fn compact() -> Self {
        Self {
            width_pct: 50,
            min_width: 34,
            max_width: 72,
            max_height: 10,
            v_margin: 0,
            h_pad: 1,
            v_pad: 0,
            anchor: OverlayAnchor::Center,
        }
    }

    /// The content width [`OverlayLayout::solve`] resolves for `area`, or `None` when the area
    /// cannot host a frame at all.
    ///
    /// Shared with `solve` rather than duplicated so a caller that must lay content out before
    /// solving — wrapped prose, whose row count depends on the width it will be painted at —
    /// cannot disagree with the width it eventually gets.
    pub(crate) fn inner_width(&self, area: Rect) -> Option<u16> {
        self.frame_metrics(area).map(|metrics| metrics.inner_width)
    }

    fn frame_metrics(&self, area: Rect) -> Option<FrameMetrics> {
        if area.width < 8 || area.height < 3 {
            return None;
        }

        let available_width = match self.anchor {
            OverlayAnchor::Center => area.width,
            OverlayAnchor::Above(composer) => area.width.min(composer.width),
        };
        if available_width < 4 {
            return None;
        }

        let width = (u32::from(area.width) * u32::from(self.width_pct) / 100)
            .try_into()
            .unwrap_or(u16::MAX)
            .clamp(self.min_width, self.max_width.max(self.min_width))
            .clamp(4, available_width);
        let h_pad = self.h_pad.min(width.saturating_sub(3) / 2);

        Some(FrameMetrics {
            width,
            h_pad,
            inner_width: width - 2 - 2 * h_pad,
        })
    }

    /// Full-width strip pinned directly above the composer.
    pub(crate) const fn palette(composer: Rect) -> Self {
        Self {
            width_pct: 100,
            min_width: 20,
            max_width: 120,
            max_height: 10,
            v_margin: 0,
            h_pad: 1,
            v_pad: 0,
            anchor: OverlayAnchor::Above(composer),
        }
    }
}

struct FrameMetrics {
    width: u16,
    h_pad: u16,
    inner_width: u16,
}

/// Everything the shell needs to size and paint one overlay.
pub(crate) struct OverlayConfig<'a> {
    pub(crate) title: &'a str,
    pub(crate) tabs: Option<&'a OverlayTabs<'a>>,
    pub(crate) shortcuts: &'a [OverlayShortcut<'a>],
    pub(crate) sizing: OverlaySizing,
    pub(crate) desired_content_rows: u16,
}

/// Resolved geometry. `content` is what the caller paints into.
pub(crate) struct OverlayLayout {
    pub(crate) frame: Rect,
    pub(crate) tabs: Option<Rect>,
    pub(crate) content: Rect,
    pub(crate) footer: Option<Rect>,
}

const MAX_FOOTER_ROWS: u16 = 2;
const FOOTER_SEPARATOR: &str = "  ·  ";

/// Greedy packing of footer shortcuts, capped at [`MAX_FOOTER_ROWS`].
///
/// Pure and computed before layout so the content area shrinks by exactly the
/// number of rows the footer will occupy.
fn pack_footer<'a, 'b>(
    shortcuts: &'a [OverlayShortcut<'b>],
    width: u16,
) -> Vec<Vec<&'a OverlayShortcut<'b>>> {
    if shortcuts.is_empty() || width == 0 {
        return Vec::new();
    }

    let width = usize::from(width);
    let separator = FOOTER_SEPARATOR.width();
    let mut rows: Vec<Vec<&OverlayShortcut<'b>>> = Vec::new();
    let mut current: Vec<&OverlayShortcut<'b>> = Vec::new();
    let mut used = 0usize;

    for shortcut in shortcuts {
        let columns = shortcut.columns();
        if current.is_empty() {
            current.push(shortcut);
            used = columns;
        } else if used + separator + columns <= width {
            current.push(shortcut);
            used += separator + columns;
        } else {
            rows.push(std::mem::take(&mut current));
            if rows.len() >= usize::from(MAX_FOOTER_ROWS) {
                return rows;
            }
            current.push(shortcut);
            used = columns;
        }
    }

    if !current.is_empty() {
        rows.push(current);
    }
    rows.truncate(usize::from(MAX_FOOTER_ROWS));
    rows
}

fn footer_rows(shortcuts: &[OverlayShortcut<'_>], width: u16) -> u16 {
    pack_footer(shortcuts, width).len() as u16
}

impl OverlayLayout {
    /// Resolves the frame, tab, content and footer rects, or `None` when the
    /// area cannot host even a bordered single content row.
    pub(crate) fn solve(area: Rect, config: &OverlayConfig<'_>) -> Option<Self> {
        let sizing = &config.sizing;
        let FrameMetrics {
            width,
            h_pad,
            inner_width,
        } = sizing.frame_metrics(area)?;

        let available_height = match sizing.anchor {
            OverlayAnchor::Center => area.height,
            OverlayAnchor::Above(composer) => composer.y.saturating_sub(area.y),
        };

        let budget = area
            .height
            .saturating_sub(2 * sizing.v_margin)
            .min(available_height);
        if budget < 3 {
            return None;
        }

        let mut v_pad = sizing.v_pad;
        let mut footer = footer_rows(config.shortcuts, inner_width);
        let mut tabs = if config.tabs.is_some() { 2 } else { 0 };
        let chrome = |v_pad: u16, footer: u16, tabs: u16| 2 + tabs + footer + 2 * v_pad;

        let height = config
            .desired_content_rows
            .max(1)
            .saturating_add(chrome(v_pad, footer, tabs))
            .min(budget)
            .min(sizing.max_height.max(3));

        while chrome(v_pad, footer, tabs) + 1 > height {
            if v_pad > 0 {
                v_pad = 0;
            } else if footer > 1 {
                footer = 1;
            } else if footer > 0 {
                footer = 0;
            } else if tabs > 0 {
                tabs -= 1;
            } else {
                return None;
            }
        }

        let (x, y) = match sizing.anchor {
            OverlayAnchor::Center => (
                area.x + (area.width - width) / 2,
                area.y + (area.height - height) / 2,
            ),
            OverlayAnchor::Above(composer) => (composer.x, composer.y.saturating_sub(height)),
        };
        let frame = Rect::new(x, y, width, height);

        let inner_x = x + 1 + h_pad;
        let tabs_y = y + 1 + v_pad;
        let content_y = tabs_y + tabs;
        let content_rows = height - chrome(v_pad, footer, tabs);

        Some(Self {
            frame,
            tabs: (tabs > 0).then(|| Rect::new(inner_x, tabs_y, inner_width, tabs)),
            content: Rect::new(inner_x, content_y, inner_width, content_rows),
            footer: (footer > 0)
                .then(|| Rect::new(inner_x, content_y + content_rows, inner_width, footer)),
        })
    }
}

/// Paints overlay chrome: clear, border, title, close affordance, tabs, footer.
pub(crate) struct OverlayFrame;

impl OverlayFrame {
    pub(crate) fn render(
        frame: &mut Frame<'_>,
        layout: &OverlayLayout,
        config: &OverlayConfig<'_>,
    ) {
        frame.render_widget(Clear, layout.frame);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(RolePalette::chrome())),
            layout.frame,
        );
        frame.render_widget(
            Paragraph::new(top_border(layout.frame.width, config.title)),
            Rect::new(layout.frame.x, layout.frame.y, layout.frame.width, 1),
        );

        if let (Some(area), Some(tabs)) = (layout.tabs, config.tabs) {
            render_tabs(frame, area, tabs);
        }
        if let Some(area) = layout.footer {
            render_footer(frame, area, config.shortcuts);
        }
    }
}

/// Top border row with the title spliced between dashes, degrading by width.
///
/// The title is the overlay's only identity marker, so it carries the brand hue
/// while the rule around it stays chrome. Nothing else is spliced into the
/// border: a painted close affordance reads as a control the keyboard-only
/// shell never offers.
fn top_border(width: u16, title: &str) -> Line<'static> {
    let chrome = Style::default().fg(RolePalette::chrome());
    let dashes = |count: usize| Span::styled("─".repeat(count), chrome);
    if width < 4 {
        return Line::from(dashes(usize::from(width)));
    }

    let width = usize::from(width);
    let inner = width - 2;
    let title = if width < 12 {
        String::new()
    } else {
        truncate_columns(title, inner.saturating_sub(3))
    };

    let mut spans = vec![Span::styled("╭", chrome)];
    if title.is_empty() {
        spans.push(dashes(inner));
    } else {
        spans.push(dashes(1));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            title.clone(),
            Style::default()
                .fg(RolePalette::brand())
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(dashes(inner - 3 - title.width()));
    }
    spans.push(Span::styled("╮", chrome));
    Line::from(spans)
}

/// Truncates to at most `budget` display columns, marking the cut with `…`.
pub(super) fn truncate_columns(text: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if text.width() <= budget {
        return text.to_owned();
    }

    let mut truncated = String::new();
    let mut used = 0usize;
    for character in text.chars() {
        let columns = character.to_string().width();
        if used + columns > budget.saturating_sub(1) {
            break;
        }
        truncated.push(character);
        used += columns;
    }
    truncated.push('…');
    truncated
}

fn render_tabs(frame: &mut Frame<'_>, area: Rect, tabs: &OverlayTabs<'_>) {
    let mut spans = Vec::new();
    for (index, label) in tabs.labels.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if index == tabs.active {
            Style::default()
                .fg(RolePalette::brand())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(RolePalette::muted())
        };
        spans.push(Span::styled((*label).to_owned(), style));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(area.x, area.y, area.width, 1),
    );

    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(usize::from(area.width)),
                Style::default().fg(RolePalette::chrome()),
            )),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, shortcuts: &[OverlayShortcut<'_>]) {
    let rows = pack_footer(shortcuts, area.width);
    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|row| {
            let mut spans = Vec::new();
            for (index, shortcut) in row.into_iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(
                        FOOTER_SEPARATOR,
                        Style::default().fg(RolePalette::muted()),
                    ));
                }
                spans.extend(shortcut.spans());
            }
            Line::from(spans)
        })
        .collect();

    let rendered = lines.len() as u16;
    let y = area.y + area.height.saturating_sub(rendered);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        Rect::new(area.x, y, area.width, rendered.min(area.height)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAV: [OverlayShortcut<'static>; 3] = [
        OverlayShortcut {
            key: "↑↓",
            label: "navigate",
        },
        OverlayShortcut {
            key: "⏎",
            label: "run",
        },
        OverlayShortcut {
            key: "esc",
            label: "close",
        },
    ];

    const TABS: OverlayTabs<'static> = OverlayTabs {
        labels: &["all", "recent"],
        active: 0,
    };

    fn centered(v_pad: u16, v_margin: u16, max_height: u16) -> OverlaySizing {
        OverlaySizing {
            width_pct: 100,
            min_width: 4,
            max_width: 200,
            max_height,
            v_margin,
            h_pad: 0,
            v_pad,
            anchor: OverlayAnchor::Center,
        }
    }

    fn overlay_config<'a>(
        sizing: OverlaySizing,
        tabs: Option<&'a OverlayTabs<'a>>,
        desired_content_rows: u16,
    ) -> OverlayConfig<'a> {
        OverlayConfig {
            title: "commands",
            tabs,
            shortcuts: &NAV,
            sizing,
            desired_content_rows,
        }
    }

    fn solve(width: u16, height: u16, config: &OverlayConfig<'_>) -> Option<OverlayLayout> {
        OverlayLayout::solve(Rect::new(0, 0, width, height), config)
    }

    #[test]
    fn solve_refuses_areas_too_small_to_host_a_frame() {
        let config = overlay_config(centered(0, 0, 40), None, 4);

        assert!(solve(1, 1, &config).is_none());
        assert!(solve(2, 3, &config).is_none());
        assert!(solve(7, 20, &config).is_none());
        assert!(solve(20, 2, &config).is_none());
        assert!(solve(8, 3, &config).is_some());
    }

    #[test]
    fn solve_clamps_width_between_min_and_max_around_the_percentage() {
        let mut sizing = centered(0, 0, 40);
        sizing.width_pct = 50;
        sizing.min_width = 30;
        sizing.max_width = 60;
        let config = overlay_config(sizing, None, 3);

        assert_eq!(solve(200, 20, &config).unwrap().frame.width, 60);
        assert_eq!(solve(100, 20, &config).unwrap().frame.width, 50);
        assert_eq!(solve(40, 20, &config).unwrap().frame.width, 30);
        assert_eq!(solve(20, 20, &config).unwrap().frame.width, 20);
    }

    #[test]
    fn solve_honors_max_height_and_vertical_margins() {
        let config = overlay_config(centered(0, 0, 11), None, 40);
        assert_eq!(solve(32, 40, &config).unwrap().frame.height, 11);

        let config = overlay_config(centered(0, 4, 40), None, 40);
        let layout = solve(32, 20, &config).unwrap();
        assert_eq!(layout.frame.height, 12);
        assert_eq!(layout.frame.y, 4);
    }

    #[test]
    fn solve_degrades_padding_then_footer_then_tabs_before_content() {
        let sized = |height: u16| {
            let config = overlay_config(centered(1, 0, 40), Some(&TABS), 5);
            solve(32, height, &config).unwrap()
        };

        let full = sized(20);
        assert_eq!(full.frame.height, 13);
        assert_eq!(full.content.height, 5);
        assert_eq!(full.tabs.map(|rect| rect.height), Some(2));
        assert_eq!(full.footer.map(|rect| rect.height), Some(2));
        assert_eq!(full.content.y, full.frame.y + 4);

        let no_pad = sized(8);
        assert_eq!(no_pad.content.y, no_pad.frame.y + 3);
        assert_eq!(no_pad.footer.map(|rect| rect.height), Some(2));
        assert_eq!(no_pad.content.height, 2);

        let wrapped_footer_dropped = sized(6);
        assert_eq!(
            wrapped_footer_dropped.footer.map(|rect| rect.height),
            Some(1)
        );
        assert_eq!(wrapped_footer_dropped.content.height, 1);

        let no_footer = sized(5);
        assert!(no_footer.footer.is_none());
        assert_eq!(no_footer.tabs.map(|rect| rect.height), Some(2));
        assert_eq!(no_footer.content.height, 1);

        let no_divider = sized(4);
        assert_eq!(no_divider.tabs.map(|rect| rect.height), Some(1));
        assert_eq!(no_divider.content.height, 1);

        let bare = sized(3);
        assert!(bare.tabs.is_none());
        assert!(bare.footer.is_none());
        assert_eq!(bare.content.height, 1);
    }

    #[test]
    fn solve_never_yields_an_empty_content_rect() {
        for width in 8..40 {
            for height in 3..14 {
                let config = overlay_config(centered(2, 1, 40), Some(&TABS), 0);
                if let Some(layout) = solve(width, height, &config) {
                    assert!(layout.content.height >= 1, "{width}x{height}");
                    assert!(layout.content.width >= 1, "{width}x{height}");
                    assert!(layout.frame.bottom() <= height);
                    assert!(layout.frame.right() <= width);
                }
            }
        }
    }

    #[test]
    fn above_anchor_clamps_to_the_rows_left_over_the_composer() {
        let composer = Rect::new(0, 7, 34, 3);
        let config = overlay_config(OverlaySizing::palette(composer), None, 2);
        let layout = solve(34, 10, &config).unwrap();

        assert_eq!(layout.frame, Rect::new(0, 1, 34, 6));
        assert_eq!(layout.content, Rect::new(2, 2, 30, 2));
        assert_eq!(layout.footer, Some(Rect::new(2, 4, 30, 2)));

        let flush = Rect::new(0, 0, 34, 3);
        let config = overlay_config(OverlaySizing::palette(flush), None, 2);
        assert!(solve(34, 10, &config).is_none());

        let tight = Rect::new(0, 4, 34, 3);
        let config = overlay_config(OverlaySizing::palette(tight), None, 8);
        let squeezed = solve(34, 10, &config).unwrap();
        assert_eq!(squeezed.frame, Rect::new(0, 0, 34, 4));
        assert_eq!(squeezed.content.height, 1);
        assert_eq!(squeezed.footer.map(|rect| rect.height), Some(1));
    }

    #[test]
    fn footer_rows_wrap_greedily_and_cap_at_two() {
        // Three ASCII items of 5 columns each, joined by a 5-column separator.
        let three = [
            OverlayShortcut {
                key: "a",
                label: "one",
            },
            OverlayShortcut {
                key: "b",
                label: "two",
            },
            OverlayShortcut {
                key: "c",
                label: "six",
            },
        ];

        assert_eq!(footer_rows(&[], 80), 0);
        assert_eq!(footer_rows(&three, 25), 1);
        assert_eq!(footer_rows(&three, 24), 2);
        assert_eq!(footer_rows(&three[..2], 15), 1);
        assert_eq!(footer_rows(&three[..2], 14), 2);
        assert_eq!(footer_rows(&three, 15), 2);
        assert_eq!(footer_rows(&three, 0), 0);
        assert_eq!(footer_rows(&three, 1), 2);
        assert_eq!(footer_rows(&NAV, 200), 1);

        let many = [
            OverlayShortcut {
                key: "a",
                label: "one",
            },
            OverlayShortcut {
                key: "b",
                label: "two",
            },
            OverlayShortcut {
                key: "c",
                label: "three",
            },
            OverlayShortcut {
                key: "d",
                label: "four",
            },
            OverlayShortcut {
                key: "e",
                label: "five",
            },
            OverlayShortcut {
                key: "f",
                label: "six",
            },
        ];
        assert_eq!(footer_rows(&many, 6), 2);
        assert_eq!(footer_rows(&many, 100), 1);
    }

    #[test]
    fn confirm_short_keys_map_to_permission_answers() {
        assert_eq!(OverlayShell::confirm_answer('a'), Some("allow-once"));
        assert_eq!(OverlayShell::confirm_answer('d'), Some("deny-once"));
        assert_eq!(OverlayShell::confirm_answer('A'), Some("allow-always"));
        assert_eq!(OverlayShell::confirm_answer('D'), Some("deny-always"));
        assert_eq!(OverlayShell::confirm_answer('x'), None);
        assert_eq!(OverlayShell::confirm_answer('b'), None);
    }

    #[test]
    fn action_matches_answer_uses_final_colon_suffix() {
        assert!(OverlayShell::action_matches_answer(
            "permission:7:allow-once",
            "allow-once"
        ));
        assert!(!OverlayShell::action_matches_answer(
            "permission:7:allow-always",
            "allow-once"
        ));
        assert!(!OverlayShell::action_matches_answer(
            "allow-once",
            "allow-once"
        ));
    }

    #[test]
    fn topmost_prefers_palette_then_dialog_then_the_file_picker() {
        assert_eq!(
            OverlayShell::topmost(true, Some(OverlayKind::Confirm), false),
            Some(OverlayKind::Palette)
        );
        assert_eq!(
            OverlayShell::topmost(false, Some(OverlayKind::Picker), false),
            Some(OverlayKind::Picker)
        );
        assert_eq!(OverlayShell::topmost(false, None, false), None);
        assert_eq!(
            OverlayShell::topmost(true, None, false),
            Some(OverlayKind::Palette)
        );
        assert_eq!(
            OverlayShell::topmost(false, None, true),
            Some(OverlayKind::FilePicker)
        );
        assert_eq!(
            OverlayShell::topmost(true, Some(OverlayKind::Picker), true),
            Some(OverlayKind::Palette)
        );
        assert_eq!(
            OverlayShell::topmost(false, Some(OverlayKind::Picker), true),
            Some(OverlayKind::Picker)
        );
    }
}
