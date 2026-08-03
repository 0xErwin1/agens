//! OSC 8 hyperlinks spliced into an already-painted buffer.
//!
//! Escape sequences cannot travel inside a `Span`: the buffer measures a span
//! by its display width and drops zero-width control characters, so a sequence
//! written there never reaches the screen. Ratatui's answer is
//! [`CellDiffOption::ForcedWidth`], which lets one cell carry more bytes than
//! it occupies columns. This module is the pass that uses it — it runs after
//! the widgets have painted, reads back what they wrote, and rewrites only the
//! two cells at each end of a link.

use std::num::NonZeroU16;
use std::sync::OnceLock;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthStr;

/// Longest link the pass will splice.
///
/// A bound keeps a pathological row from turning into one enormous cell, and
/// nothing a terminal will usefully open is longer than this.
const MAX_TARGET_LEN: usize = 2_048;

/// Whether this terminal should be sent OSC 8 sequences at all.
///
/// Well-behaved terminals ignore an OSC they do not implement, but the Linux
/// console and `dumb` terminals print the payload, which would be worse than
/// having no links. `AGENS_HYPERLINKS` overrides the guess in both directions
/// for terminals this cannot know about.
pub(crate) fn hyperlinks_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        supports_hyperlinks(
            std::env::var("AGENS_HYPERLINKS").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
        )
    })
}

/// The capability decision itself, separated from where the values come from so
/// it can be stated as a table rather than inferred from a process.
pub(crate) fn supports_hyperlinks(override_value: Option<&str>, term: Option<&str>) -> bool {
    match override_value {
        Some("0" | "never" | "off") => return false,
        Some("1" | "always" | "on") => return true,
        _ => {}
    }
    !matches!(term, None | Some("" | "dumb" | "linux" | "vt100" | "vt220"))
}

/// A run of cells that should become one hyperlink.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LinkRun {
    /// First cell of the run, as an offset into the row.
    pub(crate) start: usize,
    /// How many cells the run spans.
    pub(crate) len: usize,
    pub(crate) target: String,
}

/// Turns every link-shaped run of a painted row into a hyperlink.
///
/// Rows are read back rather than tracked from the render side because the
/// widgets that produce them are many and the buffer is the one place all of
/// their output has already been laid out.
pub(crate) fn apply_hyperlinks(buffer: &mut Buffer, area: Rect, project: &str, enabled: bool) {
    if !enabled || area.width == 0 || area.height == 0 {
        return;
    }

    for y in area.top()..area.bottom() {
        let mut row = String::new();
        let mut cell_of_byte = Vec::new();
        for x in area.left()..area.right() {
            let symbol = buffer[(x, y)].symbol();
            for _ in 0..symbol.len() {
                cell_of_byte.push(x);
            }
            row.push_str(symbol);
        }

        for run in link_runs(&row, project) {
            let Some(&start_x) = cell_of_byte.get(run.start) else {
                continue;
            };
            let Some(&last_x) = cell_of_byte.get(run.start + run.len - 1) else {
                continue;
            };
            splice_link(buffer, start_x, last_x, y, &run.target);
        }
    }
}

/// Wraps the cells from `start_x` to `last_x` in one OSC 8 hyperlink.
///
/// Only the two boundary cells are touched. Everything between them keeps the
/// style and symbol the widget gave it, so a link never changes how its text
/// looks — the terminal decides how to mark it.
fn splice_link(buffer: &mut Buffer, start_x: u16, last_x: u16, y: u16, target: &str) {
    let opener = format!("\x1b]8;;{target}\x1b\\");
    let closer = "\x1b]8;;\x1b\\";

    if start_x == last_x {
        let cell = &mut buffer[(start_x, y)];
        let width = symbol_width(cell.symbol());
        let symbol = format!("{opener}{}{closer}", cell.symbol());
        cell.set_symbol(&symbol);
        cell.set_diff_option(CellDiffOption::ForcedWidth(width));
        return;
    }

    let head = {
        let cell = &mut buffer[(start_x, y)];
        let width = symbol_width(cell.symbol());
        let symbol = format!("{opener}{}", cell.symbol());
        cell.set_symbol(&symbol);
        cell.set_diff_option(CellDiffOption::ForcedWidth(width));
        width
    };
    debug_assert!(head.get() >= 1);

    let cell = &mut buffer[(last_x, y)];
    let width = symbol_width(cell.symbol());
    let symbol = format!("{}{closer}", cell.symbol());
    cell.set_symbol(&symbol);
    cell.set_diff_option(CellDiffOption::ForcedWidth(width));
}

/// The columns a symbol occupies, never zero: a forced width of zero would
/// leave the diff walking the same cell forever.
fn symbol_width(symbol: &str) -> NonZeroU16 {
    let width = u16::try_from(symbol.width()).unwrap_or(1).max(1);
    NonZeroU16::new(width).unwrap_or(NonZeroU16::MIN)
}

/// Every link-shaped run in one row of text.
///
/// Detection is syntactic and never touches the filesystem: this runs on every
/// painted row of every frame, and a `stat` per candidate would put disk I/O in
/// the render loop.
pub(crate) fn link_runs(row: &str, project: &str) -> Vec<LinkRun> {
    let mut runs = Vec::new();
    let mut start = 0;

    for (index, byte) in row
        .bytes()
        .enumerate()
        .chain(std::iter::once((row.len(), b' ')))
    {
        if !byte.is_ascii_whitespace() {
            continue;
        }
        if index > start
            && let Some(word) = row.get(start..index)
        {
            let trimmed = word.trim_end_matches([',', '.', ';', ':', ')', ']', '}', '"', '\'']);
            if !trimmed.is_empty()
                && trimmed.len() <= MAX_TARGET_LEN
                && let Some(target) = link_target(trimmed, project)
            {
                runs.push(LinkRun {
                    start,
                    len: trimmed.len(),
                    target,
                });
            }
        }
        start = index + 1;
    }

    runs
}

/// What a word points at, or `None` when it points at nothing.
///
/// A URL is its own target. A path is resolved against the project so a tool
/// row showing `crates/agens-tui/src/lib.rs` opens the file it names rather
/// than a path relative to wherever the terminal happens to be.
fn link_target(word: &str, project: &str) -> Option<String> {
    if word.starts_with("https://") || word.starts_with("http://") {
        return Some(word.to_owned());
    }
    if !looks_like_path(word) {
        return None;
    }
    if word.starts_with('/') {
        return Some(format!("file://{word}"));
    }
    if project.is_empty() {
        return None;
    }
    Some(format!("file://{}/{word}", project.trim_end_matches('/')))
}

/// Whether a word reads as a file path rather than as prose.
///
/// The rule is deliberately narrow: at least one separator, a dotted final
/// segment, and nothing that would make it a sentence. A false positive is an
/// inert link on text nobody clicks; a false negative just leaves plain text.
fn looks_like_path(word: &str) -> bool {
    if !word.contains('/') || word.contains("://") {
        return false;
    }
    if word
        .chars()
        .any(|character| character.is_whitespace() || character == '\\')
    {
        return false;
    }
    word.rsplit('/')
        .next()
        .is_some_and(|segment| segment.contains('.') && !segment.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    /// The fallback where links are unsupported is plain text, so the decision
    /// has to be conservative about terminals that would print the payload.
    #[test]
    fn capability_falls_back_for_terminals_that_would_print_the_sequence() {
        assert!(supports_hyperlinks(None, Some("xterm-256color")));
        assert!(supports_hyperlinks(None, Some("alacritty")));
        assert!(!supports_hyperlinks(None, Some("dumb")));
        assert!(!supports_hyperlinks(None, Some("linux")));
        assert!(!supports_hyperlinks(None, None));

        assert!(supports_hyperlinks(Some("1"), Some("dumb")));
        assert!(!supports_hyperlinks(Some("never"), Some("xterm-256color")));
    }

    #[test]
    fn urls_and_project_relative_paths_are_the_only_runs_detected() {
        let runs = link_runs(
            "see https://example.com/x and crates/agens-tui/src/lib.rs now",
            "/home/user/agens",
        );

        assert_eq!(
            runs,
            vec![
                LinkRun {
                    start: 4,
                    len: "https://example.com/x".len(),
                    target: "https://example.com/x".to_owned(),
                },
                LinkRun {
                    start: 30,
                    len: "crates/agens-tui/src/lib.rs".len(),
                    target: "file:///home/user/agens/crates/agens-tui/src/lib.rs".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn an_absolute_path_keeps_itself_and_prose_is_left_alone() {
        assert_eq!(
            link_runs("/etc/hosts.conf", "/home/user/agens"),
            vec![LinkRun {
                start: 0,
                len: "/etc/hosts.conf".len(),
                target: "file:///etc/hosts.conf".to_owned(),
            }]
        );
        assert!(link_runs("Tools · batch 1 · Success", "/p").is_empty());
        assert!(link_runs("and/or", "/p").is_empty());
        assert!(link_runs("read the docs", "/p").is_empty());
    }

    /// Trailing punctuation belongs to the sentence, not to the link.
    #[test]
    fn a_trailing_sentence_mark_stays_out_of_the_target() {
        let runs = link_runs("open https://example.com/a.html.", "/p");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].target, "https://example.com/a.html");
        assert_eq!(runs[0].len, "https://example.com/a.html".len());
    }

    #[test]
    fn splicing_wraps_the_run_without_changing_what_it_says() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        buffer.set_string(0, 0, "ab/c.rs", Style::default());

        splice_link(&mut buffer, 0, 6, 0, "file:///ab/c.rs");

        assert!(
            buffer[(0, 0)]
                .symbol()
                .starts_with("\x1b]8;;file:///ab/c.rs\x1b\\")
        );
        assert!(buffer[(0, 0)].symbol().ends_with('a'));
        assert_eq!(
            buffer[(0, 0)].diff_option,
            CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap())
        );
        assert_eq!(buffer[(3, 0)].symbol(), "c", "the middle is untouched");
        assert_eq!(buffer[(6, 0)].symbol(), "s\x1b]8;;\x1b\\");
    }

    #[test]
    fn a_single_cell_run_carries_both_ends_of_the_sequence() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        buffer.set_string(0, 0, "x", Style::default());

        splice_link(&mut buffer, 0, 0, 0, "file:///x");

        assert_eq!(
            buffer[(0, 0)].symbol(),
            "\x1b]8;;file:///x\x1b\\x\x1b]8;;\x1b\\"
        );
    }
}
