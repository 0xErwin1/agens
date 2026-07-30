//! Row primitives painted inside the content rect produced by the overlay shell.
//!
//! Rows are inherently width-aware: the right column, the scrollbar gutter and
//! label truncation are all resolved against the inner width the shell just
//! decided, so a row cannot be described independently of its viewport.

use std::borrow::Cow;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use unicode_width::UnicodeWidthStr;

use super::{RolePalette, overlay::truncate_columns};

/// Selection marker and its blank counterpart, always the same two columns.
///
/// A solid rule rather than a chevron: it merges with the selected row's
/// background wash into one continuous band, so the eye reads the whole row as
/// selected instead of one glyph in front of it.
const SELECTED_MARKER: &str = "▌ ";
const PLAIN_MARKER: &str = "  ";
const MARKER_COLUMNS: usize = 2;
/// Gap column plus the track column reserved on the right when the list scrolls.
const SCROLLBAR_COLUMNS: u16 = 2;
const INDENT_COLUMNS: usize = 2;
const COLUMN_GAP: usize = 2;
const TRAILING_PAD: usize = 1;
const BADGE_GAP: usize = 1;
const MIN_LABEL_COLUMNS: usize = 8;
/// Below this the metadata column is dropped instead of squeezed to noise.
const MIN_RIGHT_COLUMNS: usize = 8;
/// The row is only painted while search mode is armed, so it repeats the key
/// that armed it instead of naming itself.
const SEARCH_LABEL: &str = "/ ";
const SEARCH_CURSOR: &str = "▏";
const SEARCH_HINT: &str = "type to filter · esc to exit";

/// One list row: left label, optional right-aligned metadata, optional badge.
#[derive(Default)]
pub(crate) struct OverlayRow<'a> {
    pub(crate) label: Cow<'a, str>,
    pub(crate) right_label: Option<Cow<'a, str>>,
    pub(crate) badge: Option<&'a str>,
    pub(crate) indent: u16,
    pub(crate) selected: bool,
    pub(crate) dimmed: bool,
}

impl<'a> OverlayRow<'a> {
    pub(crate) fn new(label: impl Into<Cow<'a, str>>) -> Self {
        Self {
            label: label.into(),
            ..Self::default()
        }
    }

    /// Flattens the row into exactly `width` display columns.
    ///
    /// The right column may claim at most half the row and never enough to push
    /// the label below [`MIN_LABEL_COLUMNS`]; once its own share falls below
    /// [`MIN_RIGHT_COLUMNS`] it is dropped whole rather than squeezed, because
    /// the label is the row's identity and the right column is metadata.
    fn line(&self, width: u16) -> Line<'static> {
        let width = usize::from(width);
        if width < MARKER_COLUMNS {
            return Line::from(Span::raw(" ".repeat(width)));
        }

        let mut spans = Vec::new();
        let marker = if self.selected {
            Span::styled(SELECTED_MARKER, Style::default().fg(RolePalette::brand()))
        } else {
            Span::raw(PLAIN_MARKER)
        };
        spans.push(marker);
        let mut remaining = width - MARKER_COLUMNS;

        let indent = (usize::from(self.indent) * INDENT_COLUMNS).min(remaining);
        if indent > 0 {
            spans.push(Span::raw(" ".repeat(indent)));
            remaining -= indent;
        }

        let muted = Style::default().fg(RolePalette::muted());
        // On the selection wash the muted grey loses almost all contrast, so the
        // row's metadata steps up one level while the row is the selected one.
        let meta = if self.selected {
            Style::default().fg(RolePalette::assistant())
        } else {
            muted
        };
        if let Some(badge) = self
            .badge
            .filter(|badge| badge.width() + BADGE_GAP <= remaining)
        {
            spans.push(Span::styled(format!("{badge} "), meta));
            remaining -= badge.width() + BADGE_GAP;
        }

        let right_budget = remaining
            .saturating_sub(MIN_LABEL_COLUMNS + COLUMN_GAP + TRAILING_PAD)
            .min(remaining / 2);
        let right = self
            .right_label
            .as_deref()
            .filter(|_| right_budget >= MIN_RIGHT_COLUMNS)
            .map(|right| truncate_columns(right, right_budget.min(right.width())))
            .filter(|right| !right.is_empty());
        let reserved = right
            .as_ref()
            .map_or(0, |right| right.width() + COLUMN_GAP + TRAILING_PAD);

        let label = truncate_columns(&self.label, remaining - reserved);
        let label_style = match (self.dimmed, self.selected) {
            (true, _) => muted,
            (_, true) => Style::default()
                .fg(RolePalette::selection_fg())
                .add_modifier(Modifier::BOLD),
            _ => Style::default().fg(RolePalette::assistant()),
        };
        let fill = remaining
            - label.width()
            - right
                .as_ref()
                .map_or(0, |right| right.width() + TRAILING_PAD);
        spans.push(Span::styled(label, label_style));
        spans.push(Span::raw(" ".repeat(fill)));

        if let Some(right) = right {
            spans.push(Span::styled(right, meta));
            spans.push(Span::raw(" ".repeat(TRAILING_PAD)));
        }
        Line::from(spans)
    }
}

/// Paints rows and the optional scrollbar gutter into a content rect.
pub(crate) struct OverlayList;

impl OverlayList {
    /// Paints `rows` from `offset`, reserving the scrollbar gutter only when
    /// `total` exceeds the rows the area can show.
    pub(crate) fn render(
        frame: &mut Frame<'_>,
        area: Rect,
        rows: &[OverlayRow<'_>],
        offset: usize,
        total: usize,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let visible = usize::from(area.height);
        let scrolls = total > visible && area.width > SCROLLBAR_COLUMNS + 2;
        let row_width = if scrolls {
            area.width - SCROLLBAR_COLUMNS
        } else {
            area.width
        };

        for (index, row) in rows.iter().skip(offset).take(visible).enumerate() {
            let style = if row.selected {
                Style::default().bg(RolePalette::selection_bg())
            } else {
                Style::default()
            };
            frame.render_widget(
                Paragraph::new(row.line(row_width)).style(style),
                Rect::new(area.x, area.y + index as u16, row_width, 1),
            );
        }

        if scrolls {
            render_scrollbar(
                frame,
                Rect::new(area.x + row_width, area.y, SCROLLBAR_COLUMNS, area.height),
                offset,
                total,
                visible,
            );
        }
    }

    /// Paints the search row: label, query, cursor, and an empty-state hint.
    pub(crate) fn render_search(frame: &mut Frame<'_>, area: Rect, query: &str) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let budget = usize::from(area.width).saturating_sub(SEARCH_LABEL.width() + 1);
        let mut spans = vec![
            Span::styled(
                SEARCH_LABEL,
                Style::default()
                    .fg(RolePalette::brand())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_columns(query, budget),
                Style::default().fg(RolePalette::assistant()),
            ),
            Span::styled(SEARCH_CURSOR, Style::default().fg(RolePalette::brand())),
        ];
        if query.is_empty() {
            spans.push(Span::styled(
                format!(" {SEARCH_HINT}"),
                Style::default().fg(RolePalette::muted()),
            ));
        }

        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
}

fn render_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    offset: usize,
    total: usize,
    visible: usize,
) {
    let mut state = ScrollbarState::new(total)
        .position(offset)
        .viewport_content_length(visible);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .thumb_symbol("█")
            .thumb_style(Style::default().fg(RolePalette::chrome()))
            .track_symbol(Some("│"))
            .track_style(Style::default().fg(RolePalette::muted())),
        area,
        &mut state,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
    use unicode_width::UnicodeWidthStr;

    use super::*;
    use crate::widgets::RolePalette;

    fn draw(width: u16, height: u16, paint: impl FnOnce(&mut Frame<'_>)) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(paint).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Rebuilds a row, skipping the blank continuation cell ratatui writes after
    /// a double-width symbol, so the result's column count matches the area.
    fn row_text(buffer: &Buffer, y: u16) -> String {
        let width = usize::from(buffer.area.width);
        let start = usize::from(y) * width;
        let mut text = String::new();
        let mut skip = 0usize;
        for cell in &buffer.content[start..start + width] {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            skip = cell.symbol().width().saturating_sub(1);
            text.push_str(cell.symbol());
        }
        text
    }

    fn render_rows(width: u16, rows: &[OverlayRow<'_>], total: usize) -> Buffer {
        let height = rows.len() as u16;
        draw(width, height, |frame| {
            OverlayList::render(frame, Rect::new(0, 0, width, height), rows, 0, total);
        })
    }

    #[test]
    fn rows_occupy_the_exact_inner_width_for_wide_labels() {
        let rows = [
            OverlayRow::new("日本語のラベル"),
            OverlayRow::new("emoji 🎉 label"),
            OverlayRow::new("ascii"),
        ];

        let buffer = render_rows(20, &rows, rows.len());

        let expected = ["  日本語のラベル", "  emoji 🎉 label", "  ascii"];
        for (y, prefix) in expected.iter().enumerate() {
            let text = row_text(&buffer, y as u16);
            assert!(text.starts_with(prefix), "row {y}: {text:?}");
            assert_eq!(text.width(), 20, "row {y}: {text:?}");
        }
    }

    #[test]
    fn wide_labels_do_not_shift_the_right_column() {
        let rows = [
            OverlayRow {
                right_label: Some("メタ".into()),
                ..OverlayRow::new("日本語 🎉")
            },
            OverlayRow {
                right_label: Some("meta".into()),
                ..OverlayRow::new("ascii")
            },
        ];

        let buffer = render_rows(24, &rows, rows.len());

        // Both right labels are 4 columns wide, so both start at 24 - 1 - 4.
        assert_eq!(buffer.content[19].symbol(), "メ");
        assert_eq!(buffer.content[24 + 19].symbol(), "m");
        assert_eq!(row_text(&buffer, 0).width(), 24);
        assert_eq!(row_text(&buffer, 1).width(), 24);
    }

    #[test]
    fn long_labels_truncate_with_an_ellipsis_on_a_character_boundary() {
        let rows = [OverlayRow::new("日本語のとても長いラベルです")];

        let buffer = render_rows(20, &rows, rows.len());
        let text = row_text(&buffer, 0);

        assert!(text.starts_with("  日本語"), "{text:?}");
        assert!(text.contains('…'), "{text:?}");
        assert!(!text.contains('で'), "{text:?}");
        assert_eq!(text.width(), 20, "{text:?}");
    }

    #[test]
    fn right_labels_stay_right_aligned_with_one_trailing_pad() {
        let rows = [OverlayRow {
            right_label: Some("connect to a provider".into()),
            ..OverlayRow::new("/an-extremely-long-command-name")
        }];

        let buffer = render_rows(40, &rows, rows.len());
        let text = row_text(&buffer, 0);

        assert!(text.ends_with("connect to a provi… "), "{text:?}");
        assert!(text.contains('…'), "{text:?}");
        assert_eq!(text.width(), 40, "{text:?}");
        assert_eq!(
            buffer.content[38].fg,
            RolePalette::muted(),
            "right column stays muted"
        );
    }

    #[test]
    fn the_right_column_never_starves_the_label() {
        let rows = [
            OverlayRow {
                right_label: Some("Initializing · inspect the overlay shell".into()),
                ..OverlayRow::new("explore #9")
            },
            OverlayRow {
                right_label: Some("Unavailable".into()),
                badge: Some("disabled"),
                ..OverlayRow::new("future-model")
            },
        ];

        let wide = render_rows(54, &rows, rows.len());
        let text = row_text(&wide, 0);
        assert!(text.starts_with("  explore #9  "), "{text:?}");
        assert!(text.ends_with("Initializing · inspect th… "), "{text:?}");

        // Below the minimum the metadata column is dropped, not squeezed.
        let narrow = row_text(&render_rows(22, &rows, rows.len()), 1);
        assert!(narrow.starts_with("  disabled future-mod…"), "{narrow:?}");
        assert!(!narrow.contains("Una"), "{narrow:?}");
    }

    #[test]
    fn rows_stay_exact_and_panic_free_at_every_width() {
        let rows = [
            OverlayRow {
                right_label: Some("メタデータ".into()),
                badge: Some("disabled"),
                indent: 3,
                selected: true,
                ..OverlayRow::new("日本語 🎉 label")
            },
            OverlayRow::new("plain"),
        ];

        for width in 1..60u16 {
            let buffer = render_rows(width, &rows, rows.len());
            for y in 0..2 {
                assert_eq!(
                    row_text(&buffer, y).width(),
                    usize::from(width),
                    "{width}x{y}"
                );
            }
            draw(width, 1, |frame| {
                OverlayList::render_search(frame, Rect::new(0, 0, width, 1), "query");
            });
        }
    }

    #[test]
    fn selection_marker_and_blank_marker_occupy_the_same_two_columns() {
        let rows = [
            OverlayRow {
                selected: true,
                ..OverlayRow::new("alpha")
            },
            OverlayRow::new("beta"),
        ];

        let buffer = render_rows(20, &rows, rows.len());

        assert!(row_text(&buffer, 0).starts_with("▌ alpha"), "selected row");
        assert!(row_text(&buffer, 1).starts_with("  beta"), "plain row");
        assert_eq!(buffer.content[2].fg, RolePalette::selection_fg());
        assert_eq!(buffer.content[19].bg, RolePalette::selection_bg());
        assert_ne!(buffer.content[39].bg, RolePalette::selection_bg());
    }

    #[test]
    fn the_search_row_shows_a_cursor_and_only_hints_while_the_query_is_empty() {
        let search = |query: &'static str| {
            let buffer = draw(30, 1, move |frame| {
                OverlayList::render_search(frame, Rect::new(0, 0, 30, 1), query);
            });
            row_text(&buffer, 0)
        };

        let empty = search("");
        assert!(empty.starts_with("/ ▏"), "{empty:?}");
        assert!(empty.contains("type to filter"), "{empty:?}");

        let typed = search("rev");
        assert!(typed.starts_with("/ rev▏"), "{typed:?}");
        assert!(!typed.contains("type to filter"), "{typed:?}");
    }

    #[test]
    fn the_scrollbar_gutter_is_reserved_only_when_the_list_overflows() {
        let rows = [
            OverlayRow::new("0123456789012345678901234567890"),
            OverlayRow::new("0123456789012345678901234567890"),
        ];

        let fitting = render_rows(20, &rows, rows.len());
        assert_eq!(row_text(&fitting, 0).chars().nth(19), Some('…'));

        let overflowing = render_rows(20, &rows, 40);
        let text = row_text(&overflowing, 0);
        assert_eq!(text.chars().nth(17), Some('…'), "{text:?}");
        assert_eq!(text.chars().nth(18), Some(' '), "gutter gap");
        assert!(
            matches!(text.chars().nth(19), Some('█' | '│')),
            "scrollbar track: {text:?}"
        );
    }
}
