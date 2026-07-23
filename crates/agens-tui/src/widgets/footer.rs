//! Compact operational footer (Claude Code–inspired density, Agens-owned layout).

use std::time::Duration;

use agens_core::Usage;

/// Context for the single operational footer row.
pub(crate) struct FooterContext<'a> {
    pub model: &'a str,
    pub effort: Option<&'a str>,
    pub project: &'a str,
    pub turn_label: &'a str,
    pub duration: Option<Duration>,
    pub usage: Option<&'a Usage>,
    pub dangerous: bool,
}

/// Formats the metric footer line: model · effort · project · ctx · status.
pub(crate) struct MetricFooter;

impl MetricFooter {
    /// Builds a dense footer without keymap laundry lists.
    pub(crate) fn text(width: u16, ctx: FooterContext<'_>) -> String {
        let mut parts = Vec::new();

        if ctx.dangerous {
            parts.push("danger".to_owned());
        }

        let model = short_model(ctx.model);
        if !model.is_empty() {
            parts.push(model);
        }
        if let Some(effort) = ctx
            .effort
            .filter(|value| !value.is_empty() && *value != "default")
        {
            parts.push(effort.to_owned());
        }

        let project = short_project(ctx.project);
        if !project.is_empty() && width >= 70 {
            parts.push(project);
        }

        if let Some(usage) = ctx.usage.and_then(Self::format_usage) {
            parts.push(usage);
        }

        let mut status = ctx.turn_label.to_owned();
        if let Some(duration) = ctx.duration {
            status.push_str(&if duration.as_secs() > 0 {
                format!(" {}s", duration.as_secs())
            } else {
                format!(" {}ms", duration.as_millis())
            });
        }
        parts.push(status);

        let line = format!(" {}", parts.join(" · "));
        if width == 0 {
            return line;
        }
        // Soft-truncate trailing segments if the terminal is very narrow.
        if line.chars().count() as u16 <= width {
            line
        } else {
            line.chars().take(usize::from(width)).collect()
        }
    }

    /// Compact usage: `71k/1000k (7%)` or `tokens 71k` when window unknown.
    pub(crate) fn format_usage(usage: &Usage) -> Option<String> {
        match (
            usage.total_tokens.or(usage.input_tokens),
            usage.context_window,
        ) {
            (Some(used), Some(window)) if window > 0 => {
                let pct = ((used as f64 / window as f64) * 100.0).clamp(0.0, 100.0);
                let pct_label = if pct < 10.0 {
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
    }
}
