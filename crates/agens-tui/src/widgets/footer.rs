//! Compact operational footer (Claude Code–inspired density, Agens-owned layout).

use std::borrow::Cow;
use std::time::Duration;

use agens_core::Usage;

use crate::RepositoryStatus;
use ratatui::{
    style::{Modifier, Style},
    text::Span,
};
use unicode_width::UnicodeWidthStr;

use super::expand::DisplayMode;
use super::overlay::truncate_columns;
use super::{Glyph, RolePalette, UnicodeLevel};

/// Context for the single operational footer row.
#[derive(Clone)]
pub(crate) struct FooterContext<'a> {
    pub model: &'a str,
    pub effort: Option<&'a str>,
    pub context_window: Option<u64>,
    pub project: &'a str,
    /// Home directory, so the working directory can collapse its prefix.
    pub home: Option<&'a str>,
    /// Branch and working-tree size, absent until the first refresh lands.
    pub repository: Option<&'a RepositoryStatus>,
    /// Level the tool output detail cycle currently rests on.
    pub tool_detail: DisplayMode,
    /// Level of the block keyboard focus stands on, when navigation is active.
    pub focused_detail: Option<DisplayMode>,
    /// Glyph set this terminal can draw the footer's chrome with.
    pub unicode: UnicodeLevel,
    pub turn_label: Cow<'a, str>,
    pub duration: Option<Duration>,
    pub usage: Option<&'a Usage>,
    pub dangerous: bool,
    pub bypass: bool,
    /// Whether the turn the status names ended in failure. The footer is the
    /// only persistent trace of it once the transcript scrolls away, so the
    /// status segment stops being border grey when this is set.
    pub failed: bool,
}

/// Narrowest border line worth splicing metadata into.
///
/// Below it even `model · status` cannot be shown whole, so the caller owes the
/// metadata a dedicated row instead of a mangled border.
pub(crate) const MIN_BORDER_METRICS_WIDTH: u16 = 24;

/// One footer datum: its own text, its own style, and what it costs to keep.
///
/// Segments are typed rather than joined into one string so a datum can carry
/// the weight it deserves — a bypassed permission mode cannot read like a
/// directory name — and so widening the footer is adding a segment, not
/// editing a format string.
struct FooterSegment {
    spans: Vec<Span<'static>>,
    /// Order the footer sheds segments in. Lower goes first.
    ///
    /// The ladder answers, in reverse, which question the footer must keep
    /// answering as the terminal narrows: what model am I talking to, did the
    /// last turn work, what may it do without asking, how much context is
    /// left, and where am I.
    rank: u8,
}

impl FooterSegment {
    fn new(rank: u8, spans: Vec<Span<'static>>) -> Self {
        Self { spans, rank }
    }

    fn plain(rank: u8, text: impl Into<String>, style: Style) -> Self {
        Self::new(rank, vec![Span::styled(text.into(), style)])
    }

    fn width(&self) -> usize {
        self.spans.iter().map(|span| span.content.width()).sum()
    }

    fn text(&self) -> String {
        self.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }
}

const RANK_DETAIL: u8 = 0;
const RANK_CHANGES: u8 = 1;
const RANK_BRANCH: u8 = 2;
const RANK_EFFORT: u8 = 3;
const RANK_DIRECTORY: u8 = 4;
const RANK_CONTEXT: u8 = 5;
const RANK_APPROVAL: u8 = 6;
const RANK_STATUS: u8 = 7;
const RANK_MODEL: u8 = 8;

/// Share of the window above which the context segment stops being neutral.
const CONTEXT_PRESSURE: f64 = 0.75;
const CONTEXT_EXHAUSTION: f64 = 0.90;

fn chrome_style() -> Style {
    Style::default().fg(RolePalette::chrome())
}

/// Every segment the footer would show at unlimited width, left to right.
///
/// The order is fixed: a segment that survives a narrowing never moves, so the
/// eye keeps finding the same datum in the same place.
fn footer_segments(ctx: &FooterContext<'_>) -> Vec<FooterSegment> {
    let mut segments = vec![model_segment(ctx), effort_segment(ctx)];
    segments.push(context_segment(ctx));
    segments.push(detail_segment(ctx));
    segments.push(directory_segment(ctx));
    if let Some(branch) = branch_segment(ctx) {
        segments.push(branch);
    }
    if let Some(changes) = changes_segment(ctx) {
        segments.push(changes);
    }
    segments.push(approval_segment(ctx));
    if let Some(status) = status_segment(ctx) {
        segments.push(status);
    }
    segments
}

fn model_segment(ctx: &FooterContext<'_>) -> FooterSegment {
    let model = short_model(ctx.model);
    let model = if model.is_empty() {
        "model —".to_owned()
    } else {
        model
    };
    FooterSegment::plain(
        RANK_MODEL,
        model,
        Style::default()
            .fg(RolePalette::machine())
            .add_modifier(Modifier::BOLD),
    )
}

fn effort_segment(ctx: &FooterContext<'_>) -> FooterSegment {
    let effort = ctx
        .effort
        .filter(|value| !value.is_empty())
        .unwrap_or("effort —");
    FooterSegment::plain(RANK_EFFORT, effort.to_owned(), chrome_style())
}

/// Context as the exact count and the share it represents.
///
/// Both are needed and neither replaces the other: the count says how much room
/// is left in tokens, the percentage answers "am I running out" without
/// arithmetic. The used side is padded so the footer never reflows as it grows.
fn context_segment(ctx: &FooterContext<'_>) -> FooterSegment {
    let used = ctx
        .usage
        .and_then(|usage| usage.total_tokens.or(usage.input_tokens));
    let window = ctx
        .usage
        .and_then(|usage| usage.context_window)
        .or(ctx.context_window)
        .filter(|window| *window > 0);

    let Some(window) = window else {
        let text = used.map_or_else(|| "ctx —".to_owned(), compact_count);
        return FooterSegment::plain(RANK_CONTEXT, text, chrome_style());
    };

    let used = used.unwrap_or(0);
    let share = (used as f64 / window as f64).clamp(0.0, 1.0);
    let style = if share >= CONTEXT_EXHAUSTION {
        Style::default().fg(RolePalette::error())
    } else if share >= CONTEXT_PRESSURE {
        Style::default().fg(RolePalette::warning())
    } else {
        chrome_style()
    };

    FooterSegment::plain(
        RANK_CONTEXT,
        format!(
            "{:>4}/{} {:>3}%",
            compact_count(used),
            compact_count(window),
            (share * 100.0).round() as u64
        ),
        style,
    )
}

/// How much of every tool body is on screen, and the key that changes it.
///
/// Ctrl+O is a cycle rather than a toggle, so pressing it without being told
/// where the cycle now rests is guesswork. Naming the level next to its key
/// makes the three positions learnable from the footer alone; it sheds first
/// because it is the only footer datum the reader can rediscover by pressing
/// a key.
fn detail_segment(ctx: &FooterContext<'_>) -> FooterSegment {
    // While a block is focused the reader is acting on that block, so the slot
    // answers the question they actually have. One slot rather than two: a
    // footer that grows a segment per mode stops being scannable.
    let text = match ctx.focused_detail {
        Some(mode) => format!("block {} o · j/k walk", mode.label()),
        None => format!("tools {} ^O", ctx.tool_detail.label()),
    };
    FooterSegment::plain(RANK_DETAIL, text, chrome_style())
}

fn directory_segment(ctx: &FooterContext<'_>) -> FooterSegment {
    let directory = abbreviate_path(ctx.project, ctx.home);
    let directory = if directory.is_empty() {
        "dir —".to_owned()
    } else {
        directory
    };
    FooterSegment::plain(RANK_DIRECTORY, directory, chrome_style())
}

fn branch_segment(ctx: &FooterContext<'_>) -> Option<FooterSegment> {
    let branch = ctx.repository?.branch.as_deref()?;
    Some(FooterSegment::plain(
        RANK_BRANCH,
        format!("{} {branch}", Glyph::Branch.text(ctx.unicode)),
        chrome_style(),
    ))
}

/// How much the working tree has moved, not merely that it has.
fn changes_segment(ctx: &FooterContext<'_>) -> Option<FooterSegment> {
    let repository = ctx.repository?;
    if !repository.is_dirty() {
        return None;
    }

    Some(FooterSegment::new(
        RANK_CHANGES,
        vec![
            Span::styled(format!("{}±", repository.changed_files), chrome_style()),
            Span::styled(
                format!(" +{}", repository.insertions),
                Style::default().fg(RolePalette::success()),
            ),
            Span::styled(
                format!(" -{}", repository.deletions),
                Style::default().fg(RolePalette::error()),
            ),
        ],
    ))
}

/// What the agent may do without asking, and the key that changes it.
///
/// This segment is security-relevant, so it never borrows another datum's
/// colour: a bypassed session must look different from a rate-limited one.
fn approval_segment(ctx: &FooterContext<'_>) -> FooterSegment {
    let (label, style) = if ctx.bypass {
        (
            "bypass ^⇧P",
            Style::default()
                .fg(RolePalette::error())
                .add_modifier(Modifier::BOLD),
        )
    } else if ctx.dangerous {
        (
            "danger ^⇧D",
            Style::default()
                .fg(RolePalette::warning())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ("ask ^⇧P", chrome_style())
    };
    FooterSegment::plain(RANK_APPROVAL, label, style)
}

fn status_segment(ctx: &FooterContext<'_>) -> Option<FooterSegment> {
    let mut status = ctx.turn_label.to_string();
    if let Some(duration) = ctx.duration {
        status.push_str(&if duration.as_secs() > 0 {
            format!(" {}s", duration.as_secs())
        } else {
            format!(" {}ms", duration.as_millis())
        });
    }
    if status.is_empty() {
        return None;
    }

    let style = if ctx.failed {
        Style::default()
            .fg(RolePalette::error())
            .add_modifier(Modifier::BOLD)
    } else {
        chrome_style()
    };
    Some(FooterSegment::plain(RANK_STATUS, status, style))
}

/// The widest prefix of `segments` that fits, shedding by rank.
///
/// Shedding is by rank but rendering stays in position order, so a footer that
/// drops its branch does not rearrange everything else around the gap.
fn fitted_segments(mut segments: Vec<FooterSegment>, width: usize) -> Vec<FooterSegment> {
    while joined_width(&segments) > width && segments.len() > 1 {
        let Some(weakest) = segments
            .iter()
            .enumerate()
            .min_by_key(|(_, segment)| segment.rank)
            .map(|(index, _)| index)
        else {
            break;
        };
        segments.remove(weakest);
    }
    segments
}

fn joined_width(segments: &[FooterSegment]) -> usize {
    let separators = segments.len().saturating_sub(1) * SEPARATOR.width();
    let leading = 1;
    leading + separators + segments.iter().map(FooterSegment::width).sum::<usize>()
}

const SEPARATOR: &str = " · ";

/// A path a reader can place at a glance, on the widths terminals actually have.
///
/// The tail is what identifies the project, so it is what survives: the home
/// prefix collapses to `~` and intermediate components shrink to their first
/// character before anything is dropped outright. Truncating from the left
/// instead would spend the budget on the part that says the least.
fn abbreviate_path(path: &str, home: Option<&str>) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }

    let display = match home.filter(|home| !home.is_empty()) {
        Some(home) if trimmed == home => return "~".to_owned(),
        Some(home) if trimmed.starts_with(&format!("{home}/")) => {
            format!("~/{}", &trimmed[home.len() + 1..])
        }
        _ => trimmed.to_owned(),
    };

    let components: Vec<&str> = display.split('/').collect();
    if components.len() <= 3 {
        return display;
    }

    let last = components.len() - 1;
    components
        .iter()
        .enumerate()
        .map(|(index, component)| {
            if index == 0 || index >= last || component.is_empty() {
                (*component).to_owned()
            } else {
                component
                    .chars()
                    .next()
                    .map(String::from)
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Formats the metric footer: typed segments, joined only at paint time.
pub(crate) struct MetricFooter;

impl MetricFooter {
    /// The footer as text, for assertions that care about content only.
    #[cfg(test)]
    pub(crate) fn text(width: u16, ctx: FooterContext<'_>) -> String {
        Self::spans(width, ctx)
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The fitted footer as spans, each segment carrying its own style.
    pub(crate) fn spans(width: u16, ctx: FooterContext<'_>) -> Vec<Span<'static>> {
        let segments = footer_segments(&ctx);
        let budget = if width == 0 {
            usize::MAX
        } else {
            usize::from(width)
        };
        let segments = fitted_segments(segments, budget);

        let mut spans = vec![Span::styled(" ".to_owned(), chrome_style())];
        for (index, segment) in segments.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(SEPARATOR.to_owned(), chrome_style()));
            }
            spans.extend(segment.spans.iter().cloned());
        }

        // Only a lone segment wider than the whole budget reaches this: the
        // ladder has nothing left to shed, so the text itself gives way.
        if width > 0 && spans.iter().map(|span| span.content.width()).sum::<usize>() > budget {
            let text = segments
                .first()
                .map(FooterSegment::text)
                .unwrap_or_default();
            let style = segments
                .first()
                .and_then(|segment| segment.spans.first().map(|span| span.style))
                .unwrap_or_else(chrome_style);
            return vec![Span::styled(
                truncate_columns(&format!(" {text}"), budget),
                style,
            )];
        }

        spans
    }

    /// Border-sized footer spans, or `None` when the border cannot host them.
    pub(crate) fn border_spans(width: u16, ctx: FooterContext<'_>) -> Option<Vec<Span<'static>>> {
        (width >= MIN_BORDER_METRICS_WIDTH).then(|| Self::spans(width, ctx))
    }

    #[cfg(test)]
    fn format_usage(usage: &Usage) -> Option<String> {
        Self::format_usage_with_context(usage, None)
    }

    #[cfg(test)]
    fn format_usage_with_context(usage: &Usage, context_window: Option<u64>) -> Option<String> {
        match (
            usage.total_tokens.or(usage.input_tokens),
            usage.context_window.or(context_window),
        ) {
            (Some(used), Some(window)) if window > 0 => {
                let pct = ((used as f64 / window as f64) * 100.0).clamp(0.0, 100.0);
                let pct_label = if used == 0 {
                    "0%".to_owned()
                } else if pct < 10.0 {
                    format!("{pct:.1}%")
                } else {
                    format!("{pct:.0}%")
                };
                Some(format!(
                    "{}/{} ({pct_label})",
                    compact_count(used),
                    compact_count(window)
                ))
            }
            (Some(used), _) => Some(compact_count(used)),
            (None, _) => None,
        }
    }
}

fn compact_count(value: u64) -> String {
    if value < 1_000 {
        value.to_string()
    } else if value < 10_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else if value < 1_000_000 {
        format!("{}k", value / 1_000)
    } else if value < 10_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else {
        format!("{}m", value / 1_000_000)
    }
}

fn short_model(model: &str) -> String {
    // "openai-chatgpt / gpt-5.6-sol" → "gpt-5.6-sol"
    model
        .rsplit(['/', ' '])
        .find(|part| !part.is_empty() && *part != "/")
        .unwrap_or(model)
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_usage_covers_known_partial_and_empty_cases() {
        assert_eq!(
            MetricFooter::format_usage(&Usage {
                input_tokens: Some(70_000),
                output_tokens: Some(1_000),
                total_tokens: Some(71_000),
                context_window: Some(200_000),
            }),
            Some("71k/200k (36%)".into())
        );
        assert_eq!(
            MetricFooter::format_usage(&Usage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                total_tokens: Some(8),
                context_window: Some(128),
            }),
            Some("8/128 (6.2%)".into())
        );
        assert_eq!(
            MetricFooter::format_usage(&Usage {
                input_tokens: Some(1),
                output_tokens: None,
                total_tokens: Some(71_000),
                context_window: Some(1_000_000),
            }),
            Some("71k/1.0m (7.1%)".into())
        );
        assert_eq!(
            MetricFooter::format_usage(&Usage {
                input_tokens: Some(1),
                output_tokens: None,
                total_tokens: Some(10),
                context_window: None,
            }),
            Some("10".into())
        );
        assert_eq!(
            MetricFooter::format_usage(&Usage {
                input_tokens: None,
                output_tokens: None,
                total_tokens: None,
                context_window: Some(128),
            }),
            None
        );
    }

    #[test]
    fn every_datum_the_footer_owes_the_reader_is_present() {
        let repository = repository();
        let mut ctx = sample();
        ctx.repository = Some(&repository);
        ctx.home = Some("/home/iperez");
        let line = MetricFooter::text(200, ctx);

        assert!(line.contains("gpt-5.6-sol"), "model: {line:?}");
        assert!(line.contains("high"), "effort: {line:?}");
        assert!(line.contains("15k/200k"), "context count: {line:?}");
        assert!(line.contains("8%"), "context share: {line:?}");
        assert!(line.contains("~/d/p/agens"), "directory: {line:?}");
        assert!(line.contains("feat/agn-114"), "branch: {line:?}");
        assert!(
            line.contains("+120") && line.contains("-8"),
            "changes: {line:?}"
        );
        assert!(line.contains("ask ^⇧P"), "approval and its key: {line:?}");
        assert!(line.contains("Ready"), "status: {line:?}");
        assert!(!line.contains("Enter send"), "{line:?}");
    }

    /// The order the footer sheds data in, stated as the contract it is.
    #[test]
    fn narrowing_sheds_segments_in_the_declared_order() {
        let repository = repository();
        let at = |width: u16| {
            let mut ctx = sample();
            ctx.repository = Some(&repository);
            ctx.home = Some("/home/iperez");
            MetricFooter::text(width, ctx)
        };

        assert_eq!(
            at(200),
            " gpt-5.6-sol · high ·  15k/200k   8% · tools hidden ^O · ~/d/p/agens · ⎇ feat/agn-114 · 3± +120 -8 · ask ^⇧P · Ready"
        );
        assert_eq!(at(120), at(200), "a wide terminal sheds nothing");
        // the detail level, which a key press rediscovers, then changes
        assert_eq!(
            at(100),
            " gpt-5.6-sol · high ·  15k/200k   8% · ~/d/p/agens · ⎇ feat/agn-114 · 3± +120 -8 · ask ^⇧P · Ready"
        );
        // then branch
        assert_eq!(
            at(80),
            " gpt-5.6-sol · high ·  15k/200k   8% · ~/d/p/agens · ask ^⇧P · Ready"
        );
        // then effort, then the directory
        assert_eq!(
            at(64),
            " gpt-5.6-sol ·  15k/200k   8% · ~/d/p/agens · ask ^⇧P · Ready"
        );
        assert_eq!(at(50), " gpt-5.6-sol ·  15k/200k   8% · ask ^⇧P · Ready");
        // then the context reading
        assert_eq!(at(30), " gpt-5.6-sol · ask ^⇧P · Ready");
        // then the approval mode, and last of all the turn status
        assert_eq!(at(24), " gpt-5.6-sol · Ready");
        assert_eq!(at(12), " gpt-5.6-sol");
        // A lone segment wider than the budget is the only thing ever clipped.
        assert_eq!(at(11), " gpt-5.6-s…");
    }

    /// Ctrl+O cycles rather than toggles, so the footer has to answer where the
    /// cycle rests. A level the reader cannot name is a level they cannot aim
    /// for, and the key belongs next to it for the same reason the approval
    /// mode carries its own.
    #[test]
    fn the_footer_names_the_tool_detail_level_and_the_key_that_moves_it() {
        let at = |mode: DisplayMode| {
            let mut ctx = sample();
            ctx.tool_detail = mode;
            MetricFooter::text(200, ctx)
        };

        assert!(at(DisplayMode::Collapsed).contains("tools hidden ^O"));
        assert!(at(DisplayMode::Truncated).contains("tools preview ^O"));
        assert!(at(DisplayMode::Expanded).contains("tools full ^O"));
    }

    /// The slot is contextual, not additive: a reader standing on a block is
    /// asking what that block will do, not what the transcript would do.
    #[test]
    fn the_detail_slot_speaks_about_the_focused_block_while_one_is_focused() {
        let mut ctx = sample();
        ctx.focused_detail = Some(DisplayMode::Truncated);
        let line = MetricFooter::text(200, ctx);

        assert!(line.contains("block preview o · j/k walk"), "{line:?}");
        assert!(!line.contains("tools hidden"), "{line:?}");
    }

    /// A value changing must not move its neighbours, or the eye loses the
    /// place it learned to look at.
    #[test]
    fn a_growing_context_count_does_not_move_the_segments_around_it() {
        let widths = [15_000_u64, 150_000, 199_999].map(|used| {
            let usage = Usage {
                input_tokens: None,
                output_tokens: None,
                total_tokens: Some(used),
                context_window: Some(200_000),
            };
            let mut ctx = sample();
            ctx.usage = Some(&usage);
            MetricFooter::text(200, ctx).width()
        });

        assert_eq!(widths[0], widths[1]);
        assert_eq!(widths[1], widths[2]);
    }

    #[test]
    fn the_context_segment_changes_colour_only_as_the_window_fills() {
        let context_colour = |used: u64| {
            let usage = Usage {
                input_tokens: None,
                output_tokens: None,
                total_tokens: Some(used),
                context_window: Some(200_000),
            };
            let mut ctx = sample();
            ctx.usage = Some(&usage);
            MetricFooter::spans(200, ctx)
                .into_iter()
                .find(|span| span.content.contains("200k"))
                .expect("the context segment names the window")
                .style
                .fg
        };

        assert_eq!(context_colour(20_000), Some(RolePalette::chrome()));
        assert_eq!(context_colour(160_000), Some(RolePalette::warning()));
        assert_eq!(context_colour(190_000), Some(RolePalette::error()));
    }

    #[test]
    fn a_deep_path_keeps_the_component_that_names_the_project() {
        assert_eq!(
            abbreviate_path("/home/iperez/dev/personal/agens", Some("/home/iperez")),
            "~/d/p/agens"
        );
        assert_eq!(
            abbreviate_path(
                "/var/lib/builds/acme/services/api-gateway",
                Some("/home/iperez")
            ),
            "/v/l/b/a/s/api-gateway"
        );
        assert_eq!(abbreviate_path("/home/iperez", Some("/home/iperez")), "~");
        assert_eq!(abbreviate_path("/srv/agens", None), "/srv/agens");
    }

    fn repository() -> RepositoryStatus {
        RepositoryStatus {
            branch: Some("feat/agn-114".to_owned()),
            changed_files: 3,
            insertions: 120,
            deletions: 8,
        }
    }

    fn sample() -> FooterContext<'static> {
        FooterContext {
            home: None,
            repository: None,
            model: "openai-chatgpt / gpt-5.6-sol",
            effort: Some("high"),
            context_window: Some(200_000),
            project: "/home/iperez/dev/personal/agens",
            tool_detail: DisplayMode::Collapsed,
            focused_detail: None,
            unicode: UnicodeLevel::Extended,
            turn_label: Cow::Borrowed("Ready"),
            duration: None,
            usage: Some(&Usage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                total_tokens: Some(15_000),
                context_window: Some(200_000),
            }),
            dangerous: false,
            bypass: false,
            failed: false,
        }
    }

    /// The approval mode is a standing state of the session, not the outcome of
    /// one turn. Its colour therefore answers one question only — what may the
    /// agent do without asking — so a failed turn can never make it look like
    /// the mode caused the failure, and a bypassed session cannot hide behind
    /// a calm turn.
    #[test]
    fn the_approval_segment_is_coloured_by_the_mode_and_by_nothing_else() {
        let approval = |mutate: fn(&mut FooterContext<'static>)| {
            let mut ctx = sample();
            mutate(&mut ctx);
            MetricFooter::spans(200, ctx)
                .into_iter()
                .find(|span| span.content.contains('⇧'))
                .expect("the approval segment names its key")
        };

        let asking = approval(|_| {});
        assert_eq!(asking.content, "ask ^⇧P");
        assert_eq!(asking.style.fg, Some(RolePalette::chrome()));

        let dangerous = approval(|ctx| ctx.dangerous = true);
        assert_eq!(dangerous.content, "danger ^⇧D");
        assert_eq!(dangerous.style.fg, Some(RolePalette::warning()));

        let bypassed = approval(|ctx| ctx.bypass = true);
        assert_eq!(bypassed.content, "bypass ^⇧P");
        assert_eq!(bypassed.style.fg, Some(RolePalette::error()));

        let failed_but_asking = approval(|ctx| {
            ctx.failed = true;
            ctx.turn_label = Cow::Borrowed("Failed");
        });
        assert_eq!(
            failed_but_asking.style.fg,
            Some(RolePalette::chrome()),
            "a failed turn says nothing about what the agent may do"
        );
    }

    /// A bypassed session used to overwrite the turn status, so a failure and a
    /// bypass could not be reported at once. Typed segments make that trade
    /// unnecessary: both are their own datum and both stay.
    #[test]
    fn a_bypassed_session_still_reports_how_its_turn_ended() {
        let mut ctx = sample();
        ctx.bypass = true;
        ctx.turn_label = Cow::Borrowed("Failed");
        ctx.failed = true;
        let line = MetricFooter::text(200, ctx);

        assert!(line.contains("bypass"), "{line:?}");
        assert!(line.contains("Failed"), "{line:?}");
    }

    fn border_string(width: u16, ctx: FooterContext<'_>) -> Option<String> {
        MetricFooter::border_spans(width, ctx)
            .map(|spans| spans.iter().map(|span| span.content.as_ref()).collect())
    }

    #[test]
    fn border_metadata_yields_nothing_below_the_shortest_hostable_form() {
        assert_eq!(
            border_string(MIN_BORDER_METRICS_WIDTH, sample()),
            Some(" gpt-5.6-sol · Ready".to_owned())
        );
        assert_eq!(border_string(MIN_BORDER_METRICS_WIDTH - 1, sample()), None);
        assert_eq!(border_string(0, sample()), None);
    }

    #[test]
    fn a_failed_status_is_the_only_segment_a_turn_outcome_repaints() {
        let mut ctx = sample();
        ctx.turn_label = Cow::Borrowed("Failed");
        ctx.failed = true;
        let failed = MetricFooter::spans(200, ctx);

        let status = failed.last().expect("the status closes the footer");
        assert_eq!(status.content, "Failed");
        assert_eq!(status.style.fg, Some(RolePalette::error()));
        assert!(status.style.add_modifier.contains(Modifier::BOLD));

        let reddened = failed
            .iter()
            .filter(|span| span.style.fg == Some(RolePalette::error()))
            .count();
        assert_eq!(reddened, 1, "{failed:?}");

        let ready = MetricFooter::spans(200, sample());
        assert!(
            ready
                .iter()
                .all(|span| span.style.fg != Some(RolePalette::error())),
            "{ready:?}"
        );
    }
}
