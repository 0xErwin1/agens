//! Compact operational footer (Claude Code–inspired density, Agens-owned layout).

use std::borrow::Cow;
use std::time::Duration;

use agens_core::Usage;
use ratatui::{
    style::{Modifier, Style},
    text::Span,
};
use unicode_width::UnicodeWidthStr;

use super::RolePalette;
use super::overlay::truncate_columns;

/// Context for the single operational footer row.
#[derive(Clone)]
pub(crate) struct FooterContext<'a> {
    pub model: &'a str,
    pub effort: Option<&'a str>,
    pub context_window: Option<u64>,
    pub project: &'a str,
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

/// Segment sets the footer degrades through, widest first.
#[derive(Clone, Copy)]
enum FooterDetail {
    Full,
    NoProject,
    NoUsage,
    ModelAndStatus,
    ModelOnly,
}

impl FooterDetail {
    const LADDER: [Self; 5] = [
        Self::Full,
        Self::NoProject,
        Self::NoUsage,
        Self::ModelAndStatus,
        Self::ModelOnly,
    ];
}

/// Resolved footer segments, each already carrying its own explicit fallback.
struct FooterSegments {
    model: String,
    effort: String,
    usage: String,
    project: String,
    status: String,
    /// The trailing part of `status` that names *this turn's* outcome, when the
    /// status segment names one at all.
    ///
    /// A permission mode is a standing state of the session and changes on a
    /// different clock than a turn, so it never belongs to this. Bypass leaves
    /// it absent because it replaces the turn outcome outright.
    turn_status: Option<String>,
}

impl FooterSegments {
    fn new(ctx: FooterContext<'_>) -> Self {
        let model = short_model(ctx.model);
        let model = if model.is_empty() {
            "model —".to_owned()
        } else {
            model
        };
        let effort = ctx
            .effort
            .filter(|value| !value.is_empty())
            .unwrap_or("effort —")
            .to_owned();
        let usage = match ctx.usage {
            Some(usage) => MetricFooter::format_usage_with_context(usage, ctx.context_window),
            None => ctx
                .context_window
                .filter(|window| *window > 0)
                .map(|window| format!("0/{} (0%)", compact_count(window))),
        }
        .unwrap_or_else(|| "ctx —".to_owned());
        let project = short_project(ctx.project);
        let project = if project.is_empty() {
            "project —".to_owned()
        } else {
            project
        };

        let mut status = ctx.turn_label.to_string();
        if let Some(duration) = ctx.duration {
            status.push_str(&if duration.as_secs() > 0 {
                format!(" {}s", duration.as_secs())
            } else {
                format!(" {}ms", duration.as_millis())
            });
        }

        let mut turn_status = (!status.is_empty()).then(|| status.clone());
        if ctx.dangerous {
            status = format!("danger {status}");
        }
        if ctx.bypass {
            status = "bypass".to_owned();
            turn_status = None;
        }

        Self {
            model,
            effort,
            usage,
            project,
            status,
            turn_status,
        }
    }

    fn line(&self, detail: FooterDetail) -> String {
        let parts: Vec<&String> = match detail {
            FooterDetail::Full => vec![
                &self.model,
                &self.effort,
                &self.usage,
                &self.project,
                &self.status,
            ],
            FooterDetail::NoProject => {
                vec![&self.model, &self.effort, &self.usage, &self.status]
            }
            FooterDetail::NoUsage => vec![&self.model, &self.effort, &self.status],
            FooterDetail::ModelAndStatus => vec![&self.model, &self.status],
            FooterDetail::ModelOnly => vec![&self.model],
        };
        footer_line(parts)
    }

    /// Widest form fitting `width`, or `None` when not even the model alone does.
    fn fitted(&self, width: u16) -> Option<String> {
        FooterDetail::LADDER
            .into_iter()
            .map(|detail| self.line(detail))
            .find(|line| line.width() <= usize::from(width))
    }
}

/// Formats the metric footer line: model · effort · usage · project · status.
pub(crate) struct MetricFooter;

impl MetricFooter {
    /// Builds a dense footer without keymap laundry lists.
    ///
    /// Degradation drops whole segments before touching a token; only a model
    /// name wider than the whole budget is ellipsized.
    pub(crate) fn text(width: u16, ctx: FooterContext<'_>) -> String {
        let segments = FooterSegments::new(ctx);
        if width == 0 {
            return segments.line(FooterDetail::Full);
        }
        segments.fitted(width).unwrap_or_else(|| {
            truncate_columns(&segments.line(FooterDetail::ModelOnly), usize::from(width))
        })
    }

    /// The fitted footer as spans, with the turn status lifted out of the
    /// border grey when it reports a failure.
    ///
    /// The turn status closes every fitted form that still carries one, so a
    /// failed turn is emphasized by re-styling that tail rather than by
    /// rebuilding the degradation ladder in terms of spans. Emphasis stops at
    /// the turn status: a permission mode sharing the segment keeps the border
    /// grey, because a security-relevant indicator must not change appearance
    /// for a reason that has nothing to do with permissions.
    pub(crate) fn spans(width: u16, ctx: FooterContext<'_>) -> Vec<Span<'static>> {
        let turn_status = ctx
            .failed
            .then(|| FooterSegments::new(ctx.clone()).turn_status)
            .flatten();
        let text = Self::text(width, ctx);

        let chrome = Style::default().fg(RolePalette::chrome());
        let head = turn_status
            .as_ref()
            .and_then(|status| text.strip_suffix(status.as_str()));

        let (Some(head), Some(status)) = (head, turn_status) else {
            return vec![Span::styled(text, chrome)];
        };

        vec![
            Span::styled(head.to_owned(), chrome),
            Span::styled(
                status,
                Style::default()
                    .fg(RolePalette::error())
                    .add_modifier(Modifier::BOLD),
            ),
        ]
    }

    /// Border-sized footer spans, or `None` when the border cannot host them.
    pub(crate) fn border_spans(width: u16, ctx: FooterContext<'_>) -> Option<Vec<Span<'static>>> {
        (width >= MIN_BORDER_METRICS_WIDTH).then(|| Self::spans(width, ctx))
    }

    #[cfg(test)]
    fn format_usage(usage: &Usage) -> Option<String> {
        Self::format_usage_with_context(usage, None)
    }

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

fn footer_line<'a>(parts: impl IntoIterator<Item = &'a String>) -> String {
    format!(
        " {}",
        parts
            .into_iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" · ")
    )
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

fn short_project(project: &str) -> String {
    let trimmed = project.trim_end_matches('/');
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
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
    fn text_is_dense_without_keymap_laundry() {
        let line = MetricFooter::text(
            100,
            FooterContext {
                model: "openai-chatgpt / gpt-5.6-sol",
                effort: Some("high"),
                context_window: Some(200_000),
                project: "/home/iperez/dev/personal/agens",
                turn_label: Cow::Borrowed("Ready"),
                duration: Some(Duration::from_secs(2)),
                usage: Some(&Usage {
                    input_tokens: Some(1),
                    output_tokens: Some(2),
                    total_tokens: Some(15_000),
                    context_window: Some(200_000),
                }),
                dangerous: false,
                bypass: false,
                failed: false,
            },
        );
        assert!(line.contains("gpt-5.6-sol"));
        assert!(line.contains("high"));
        assert!(line.contains("agens"));
        assert!(line.contains("15k/200k"));
        assert!(line.contains('%'));
        assert!(line.contains("Ready"));
        assert!(!line.contains("Enter send"));
        assert!(!line.contains("Ctrl+C"));
        assert!(!line.contains("unavailable"));
        // Claude-like order: model · effort · usage · project · status
        let model_at = line.find("gpt-5.6-sol").expect("model");
        let effort_at = line.find("high").expect("effort");
        let usage_at = line.find("15k/200k").expect("usage");
        let project_at = line.find("agens").expect("project");
        let status_at = line.find("Ready").expect("status");
        assert!(model_at < effort_at && effort_at < usage_at);
        assert!(usage_at < project_at && project_at < status_at);
    }

    fn sample() -> FooterContext<'static> {
        FooterContext {
            model: "openai-chatgpt / gpt-5.6-sol",
            effort: Some("high"),
            context_window: Some(200_000),
            project: "/home/iperez/dev/personal/agens",
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

    #[test]
    fn text_drops_whole_segments_before_ellipsizing_a_token() {
        let at = |width: u16| MetricFooter::text(width, sample());

        assert_eq!(
            at(0),
            " gpt-5.6-sol · high · 15k/200k (7.5%) · agens · Ready"
        );
        assert_eq!(
            at(80),
            " gpt-5.6-sol · high · 15k/200k (7.5%) · agens · Ready"
        );
        assert_eq!(at(52), " gpt-5.6-sol · high · 15k/200k (7.5%) · Ready");
        assert_eq!(at(45), " gpt-5.6-sol · high · 15k/200k (7.5%) · Ready");
        assert_eq!(at(44), " gpt-5.6-sol · high · Ready");
        assert_eq!(at(27), " gpt-5.6-sol · high · Ready");
        assert_eq!(at(26), " gpt-5.6-sol · Ready");
        assert_eq!(at(20), " gpt-5.6-sol · Ready");
        assert_eq!(at(19), " gpt-5.6-sol");
        assert_eq!(at(12), " gpt-5.6-sol");
        assert_eq!(at(11), " gpt-5.6-s…");
    }

    #[test]
    fn bypass_segment_replaces_the_turn_status_without_hiding_when_dangerous() {
        let mut ctx = sample();
        ctx.bypass = true;
        let line = MetricFooter::text(100, ctx);
        assert!(line.contains("bypass"), "{line:?}");

        let ctx = sample();
        let line = MetricFooter::text(100, ctx);
        assert!(!line.contains("bypass"), "{line:?}");

        let mut ctx = sample();
        ctx.bypass = true;
        ctx.dangerous = true;
        let line = MetricFooter::text(100, ctx);
        assert!(line.contains("bypass"), "{line:?}");
        assert!(!line.contains("Ready"), "{line:?}");
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
        assert_eq!(
            border_string(80, sample()),
            Some(" gpt-5.6-sol · high · 15k/200k (7.5%) · agens · Ready".to_owned())
        );
    }

    fn failed_spans(mutate: impl FnOnce(&mut FooterContext<'static>)) -> Vec<Span<'static>> {
        let mut ctx = sample();
        ctx.turn_label = Cow::Borrowed("Failed");
        ctx.failed = true;
        mutate(&mut ctx);
        MetricFooter::spans(100, ctx)
    }

    fn spans_text(spans: &[Span<'static>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn assert_emphasized_turn_status(spans: &[Span<'static>]) {
        let (head, status) = spans.split_at(spans.len() - 1);
        assert_eq!(status[0].content, "Failed");
        assert_eq!(status[0].style.fg, Some(RolePalette::error()));
        assert!(status[0].style.add_modifier.contains(Modifier::BOLD));
        for span in head {
            assert_eq!(span.style.fg, Some(RolePalette::chrome()));
        }
    }

    #[test]
    fn a_failed_status_is_the_only_footer_segment_lifted_out_of_the_border_grey() {
        assert_emphasized_turn_status(&failed_spans(|_| {}));

        let ready = MetricFooter::spans(100, sample());
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].style.fg, Some(RolePalette::chrome()));
    }

    /// A permission mode is a standing state of the session, not the outcome of
    /// one turn. Reddening it because an unrelated turn failed would invite the
    /// inference that the mode caused the failure, on the one indicator whose
    /// job is to report what the agent is currently allowed to do.
    #[test]
    fn a_failed_turn_leaves_the_permission_mode_in_the_border_grey() {
        let dangerous = failed_spans(|ctx| ctx.dangerous = true);

        assert_emphasized_turn_status(&dangerous);
        assert_eq!(dangerous.len(), 2);
        assert!(dangerous[0].content.ends_with("danger "), "{dangerous:?}");
        assert_eq!(dangerous[0].style.fg, Some(RolePalette::chrome()));

        // Bypass replaces the turn outcome outright: "Failed" never appears, so
        // emphasizing the segment would redden "bypass" and report nothing.
        let bypass = failed_spans(|ctx| ctx.bypass = true);

        assert_eq!(bypass.len(), 1);
        assert_eq!(bypass[0].style.fg, Some(RolePalette::chrome()));
        assert!(spans_text(&bypass).ends_with("bypass"), "{bypass:?}");
        assert!(!spans_text(&bypass).contains("Failed"), "{bypass:?}");

        let both = failed_spans(|ctx| {
            ctx.dangerous = true;
            ctx.bypass = true;
        });

        assert_eq!(both.len(), 1);
        assert_eq!(both[0].style.fg, Some(RolePalette::chrome()));
    }
}
