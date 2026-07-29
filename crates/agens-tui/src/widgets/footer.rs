//! Compact operational footer (Claude Code–inspired density, Agens-owned layout).

use std::time::Duration;

use agens_core::Usage;
use unicode_width::UnicodeWidthStr;

use super::overlay::truncate_columns;

/// Context for the single operational footer row.
pub(crate) struct FooterContext<'a> {
    pub model: &'a str,
    pub effort: Option<&'a str>,
    pub context_window: Option<u64>,
    pub project: &'a str,
    pub turn_label: &'a str,
    pub duration: Option<Duration>,
    pub usage: Option<&'a Usage>,
    pub dangerous: bool,
    pub bypass: bool,
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

        let mut status = ctx.turn_label.to_owned();
        if let Some(duration) = ctx.duration {
            status.push_str(&if duration.as_secs() > 0 {
                format!(" {}s", duration.as_secs())
            } else {
                format!(" {}ms", duration.as_millis())
            });
        }
        if ctx.dangerous {
            status = format!("danger {status}");
        }
        if ctx.bypass {
            status = format!("BYPASS {status}");
        }

        Self {
            model,
            effort,
            usage,
            project,
            status,
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

    /// Same metadata sized for a border line, or `None` when the border is too
    /// narrow to host the shortest form whole.
    pub(crate) fn border_text(width: u16, ctx: FooterContext<'_>) -> Option<String> {
        (width >= MIN_BORDER_METRICS_WIDTH).then(|| Self::text(width, ctx))
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
                turn_label: "Ready",
                duration: Some(Duration::from_secs(2)),
                usage: Some(&Usage {
                    input_tokens: Some(1),
                    output_tokens: Some(2),
                    total_tokens: Some(15_000),
                    context_window: Some(200_000),
                }),
                dangerous: false,
                bypass: false,
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
            turn_label: "Ready",
            duration: None,
            usage: Some(&Usage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                total_tokens: Some(15_000),
                context_window: Some(200_000),
            }),
            dangerous: false,
            bypass: false,
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
    fn bypass_segment_is_shown_hidden_and_coexists_with_dangerous_mode() {
        let mut ctx = sample();
        ctx.bypass = true;
        let line = MetricFooter::text(100, ctx);
        assert!(line.contains("BYPASS"), "{line:?}");

        let ctx = sample();
        let line = MetricFooter::text(100, ctx);
        assert!(!line.contains("BYPASS"), "{line:?}");

        let mut ctx = sample();
        ctx.bypass = true;
        ctx.dangerous = true;
        let line = MetricFooter::text(100, ctx);
        assert!(line.contains("BYPASS"), "{line:?}");
        assert!(line.contains("danger"), "{line:?}");
    }

    #[test]
    fn border_text_yields_nothing_below_the_shortest_hostable_form() {
        assert_eq!(
            MetricFooter::border_text(MIN_BORDER_METRICS_WIDTH, sample()),
            Some(" gpt-5.6-sol · Ready".to_owned())
        );
        assert_eq!(
            MetricFooter::border_text(MIN_BORDER_METRICS_WIDTH - 1, sample()),
            None
        );
        assert_eq!(MetricFooter::border_text(0, sample()), None);
        assert_eq!(
            MetricFooter::border_text(80, sample()),
            Some(" gpt-5.6-sol · high · 15k/200k (7.5%) · agens · Ready".to_owned())
        );
    }
}
