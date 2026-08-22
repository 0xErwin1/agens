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
    /// Whether no turn is in flight, so the status slot can stay empty.
    pub idle: bool,
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

const RANK_LOCATION: u8 = 0;
const RANK_CONTEXT: u8 = 1;
const RANK_APPROVAL: u8 = 2;
const RANK_STATUS: u8 = 3;
const RANK_MODEL: u8 = 4;

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
    let mut segments = vec![model_segment(ctx), context_segment(ctx)];
    if let Some(location) = location_segment(ctx) {
        segments.push(location);
    }
    segments.push(approval_segment(ctx));
    if let Some(status) = status_segment(ctx) {
        segments.push(status);
    }
    segments
}

/// Which model is answering, and how hard it is being asked to think.
///
/// One segment rather than two: the effort qualifies the model and is
/// meaningless without it, so a separator between them spent a column to
/// suggest they were independent data.
fn model_segment(ctx: &FooterContext<'_>) -> FooterSegment {
    let model = short_model(ctx.model);
    let model = if model.is_empty() {
        "model —".to_owned()
    } else {
        model
    };

    let mut spans = vec![Span::styled(
        model,
        Style::default()
            .fg(RolePalette::machine())
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(effort) = ctx.effort.filter(|value| !value.is_empty()) {
        spans.push(Span::styled(format!(" ({effort})"), chrome_style()));
    }

    FooterSegment::new(RANK_MODEL, spans)
}

/// Context as the share of the window in use, and the counts once that matters.
///
/// The percentage answers "am I running out", which is the only context
/// question a reader has while things are going well. The exact counts answer
/// "how much room is left", which is a question worth a permanent slot only
/// once the answer is close to none — so they join the segment under pressure
/// and stay out of the way before it.
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

    let percentage = format!("{:>3}%", (share * 100.0).round() as u64);
    let text = if share >= CONTEXT_PRESSURE {
        // Padded, so the counts do not reflow their neighbours as they grow.
        format!(
            "{percentage} ({:>4}/{})",
            compact_count(used),
            compact_count(window)
        )
    } else {
        percentage
    };

    FooterSegment::plain(RANK_CONTEXT, text, style)
}

/// Where the agent is working: the directory, the branch, and how far the tree
/// has moved.
///
/// One segment rather than three, because they answer a single question and a
/// reader checks them together. The insertion and deletion counts join only
/// when they are not zero: a clean-but-tracked file count already says the tree
/// moved, and `+0 -0` spent six columns to say nothing.
fn location_segment(ctx: &FooterContext<'_>) -> Option<FooterSegment> {
    let directory = abbreviate_path(ctx.project, ctx.home);
    if directory.is_empty() && ctx.repository.is_none() {
        return None;
    }

    let mut spans = Vec::new();
    if !directory.is_empty() {
        spans.push(Span::styled(directory, chrome_style()));
    }

    if let Some(repository) = ctx.repository {
        if let Some(branch) = repository.branch.as_deref() {
            let separator = if spans.is_empty() { "" } else { " " };
            spans.push(Span::styled(
                format!("{separator}{} {branch}", Glyph::Branch.text(ctx.unicode)),
                chrome_style(),
            ));
        }
        if repository.is_dirty() {
            spans.push(Span::styled(
                format!(" {}±", repository.changed_files),
                chrome_style(),
            ));
            if repository.insertions > 0 {
                spans.push(Span::styled(
                    format!(" +{}", repository.insertions),
                    Style::default().fg(RolePalette::success()),
                ));
            }
            if repository.deletions > 0 {
                spans.push(Span::styled(
                    format!(" -{}", repository.deletions),
                    Style::default().fg(RolePalette::error()),
                ));
            }
        }
    }

    (!spans.is_empty()).then(|| FooterSegment::new(RANK_LOCATION, spans))
}

/// What the agent may do without asking.
///
/// This segment is security-relevant, so it never borrows another datum's
/// colour: a bypassed session must look different from a rate-limited one. The
/// key that changes it lives in the shortcuts overlay — a permanent instruction
/// here competed for attention with the state it was instructing about.
fn approval_segment(ctx: &FooterContext<'_>) -> FooterSegment {
    let (label, style) = if ctx.bypass {
        (
            "bypass",
            Style::default()
                .fg(RolePalette::error())
                .add_modifier(Modifier::BOLD),
        )
    } else if ctx.dangerous {
        (
            "danger",
            Style::default()
                .fg(RolePalette::warning())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ("ask", chrome_style())
    };
    FooterSegment::plain(RANK_APPROVAL, label, style)
}

/// What the current turn is doing, when it is doing anything.
///
/// The transcript carries its own live activity row, so the footer repeating it
/// is only worth a slot for the states that outlive the turn. Idle is not one
/// of them: a permanent "Ready" is the absence of news taking up room. Failure
/// is, because once the error card scrolls away the footer is the only trace
/// left that the last turn did not work.
fn status_segment(ctx: &FooterContext<'_>) -> Option<FooterSegment> {
    if ctx.idle && !ctx.failed {
        return None;
    }

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
///
/// Public because a caller that pins a whole frame has to be able to predict
/// this rather than restate it: the project root a test runs under is chosen by
/// the machine, so the footer text it produces can only be derived.
pub fn abbreviate_path(path: &str, home: Option<&str>) -> String {
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
        assert!(line.contains("8%"), "context share: {line:?}");
        assert!(line.contains("~/d/p/agens"), "directory: {line:?}");
        assert!(line.contains("feat/agn-114"), "branch: {line:?}");
        assert!(
            line.contains("+120") && line.contains("-8"),
            "changes: {line:?}"
        );
        assert!(line.contains("ask"), "approval: {line:?}");
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
            " gpt-5.6-sol (high) ·   8% · ~/d/p/agens ⎇ feat/agn-114 3± +120 -8 · ask"
        );
        assert_eq!(at(80), at(200), "a wide terminal sheds nothing");
        // where it is working goes first: the reader chose the directory and
        // the branch, so they are the data they already know
        assert_eq!(at(40), " gpt-5.6-sol (high) ·   8% · ask");
        // then the context reading
        assert_eq!(at(30), " gpt-5.6-sol (high) · ask");
        // and last of all the approval mode
        assert_eq!(at(24), " gpt-5.6-sol (high)");
        // A lone segment wider than the budget is the only thing ever clipped.
        assert_eq!(at(12), " gpt-5.6-so…");
    }

    /// The footer reports state. Every key it used to name drifted out of date
    /// the moment the keymap moved, and the shortcuts overlay is the one place
    /// that cannot.
    #[test]
    fn the_footer_carries_no_keybindings() {
        let mut ctx = sample();
        ctx.bypass = true;
        let line = MetricFooter::text(200, ctx);

        assert!(line.contains("bypass"), "{line:?}");
        for key in ["^O", "^⇧P", "^⇧D", "j/k", " o ·"] {
            assert!(!line.contains(key), "{key} still in the footer: {line:?}");
        }
    }

    /// Idle is the absence of news, and the transcript already carries the live
    /// activity while there is any.
    #[test]
    fn the_status_slot_is_empty_when_idle_and_kept_when_the_turn_failed() {
        let idle = MetricFooter::text(200, sample());
        assert!(!idle.contains("Ready"), "{idle:?}");

        let mut running = sample();
        running.idle = false;
        running.turn_label = Cow::Borrowed("Reasoning");
        assert!(MetricFooter::text(200, running).contains("Reasoning"));

        let mut failed = sample();
        failed.failed = true;
        failed.turn_label = Cow::Borrowed("Failed");
        assert!(MetricFooter::text(200, failed).contains("Failed"));
    }

    /// The exact counts are worth a permanent slot only once they are running
    /// out; before that the share answers the whole question.
    #[test]
    fn context_counts_join_the_share_only_under_pressure() {
        let at = |used: u64| {
            let usage = Usage {
                input_tokens: None,
                output_tokens: None,
                total_tokens: Some(used),
                context_window: Some(200_000),
            };
            let mut ctx = sample();
            ctx.usage = Some(Box::leak(Box::new(usage)));
            MetricFooter::text(200, ctx)
        };

        let relaxed = at(15_000);
        assert!(relaxed.contains("8%"), "{relaxed:?}");
        assert!(!relaxed.contains("200k"), "{relaxed:?}");

        let pressed = at(180_000);
        assert!(pressed.contains("90%"), "{pressed:?}");
        assert!(pressed.contains("180k/200k"), "{pressed:?}");
    }

    /// A value changing must not move its neighbours, or the eye loses the
    /// place it learned to look at. Crossing into pressure widens the segment
    /// once, on purpose; growing inside either regime must not move anything.
    #[test]
    fn a_growing_context_count_does_not_move_the_segments_around_it() {
        let width_at = |used: u64| {
            let usage = Usage {
                input_tokens: None,
                output_tokens: None,
                total_tokens: Some(used),
                context_window: Some(200_000),
            };
            let mut ctx = sample();
            ctx.usage = Some(&usage);
            MetricFooter::text(200, ctx).width()
        };

        assert_eq!(width_at(1_000), width_at(15_000));
        assert_eq!(width_at(15_000), width_at(140_000));
        assert_eq!(width_at(150_000), width_at(199_999));
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
                .find(|span| span.content.contains('%'))
                .expect("the context segment names the share")
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
            idle: true,
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
                .find(|span| matches!(span.content.as_ref(), "ask" | "danger" | "bypass"))
                .expect("the approval segment names the mode")
        };

        let asking = approval(|_| {});
        assert_eq!(asking.content, "ask");
        assert_eq!(asking.style.fg, Some(RolePalette::chrome()));

        let dangerous = approval(|ctx| ctx.dangerous = true);
        assert_eq!(dangerous.content, "danger");
        assert_eq!(dangerous.style.fg, Some(RolePalette::warning()));

        let bypassed = approval(|ctx| ctx.bypass = true);
        assert_eq!(bypassed.content, "bypass");
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
            Some(" gpt-5.6-sol (high)".to_owned())
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
