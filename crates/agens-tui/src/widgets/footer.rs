//! Compact footer metrics for turn status and token usage.

use std::time::Duration;

use agens_core::Usage;

/// Formats the metric footer line: turn label, optional duration, honest usage.
pub(crate) struct MetricFooter;

impl MetricFooter {
    /// Builds the footer line for the given terminal width and turn metrics.
    ///
    /// Usage is omitted when both total tokens and context window are unknown.
    /// When only tokens are known, the window side is omitted (never "unavailable").
    pub(crate) fn text(
        width: u16,
        turn_label: &str,
        duration: Option<Duration>,
        usage: Option<&Usage>,
    ) -> String {
        let duration = duration.map_or_else(String::new, |value| {
            if value.as_secs() > 0 {
                format!(" · {}s", value.as_secs())
            } else {
                format!(" · {}ms", value.as_millis())
            }
        });
        let usage = usage
            .and_then(Self::format_usage)
            .map_or_else(String::new, |segment| format!(" · {segment}"));
        let metrics = format!(" {turn_label}{duration}{usage}");

        if width < 60 {
            format!("{metrics}  ·  Enter send  ·  Ctrl+C cancel/quit")
        } else {
            format!(
                "{metrics}  ·  Enter send  ·  Shift+Enter newline  ·  Ctrl+O output  ·  Ctrl+C cancel/quit  ·  PgUp/PgDn scroll  ·  End follow"
            )
        }
    }

    /// Formats a compact usage segment, or `None` when nothing honest can be shown.
    pub(crate) fn format_usage(usage: &Usage) -> Option<String> {
        match (usage.total_tokens, usage.context_window) {
            (Some(used), Some(window)) => Some(format!("tokens {used}/{window}")),
            (Some(used), None) => Some(format!("tokens {used}")),
            (None, _) => None,
        }
    }
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
            Some("tokens 8/128".into())
        );
        assert_eq!(
            MetricFooter::format_usage(&Usage {
                input_tokens: Some(1),
                output_tokens: None,
                total_tokens: Some(10),
                context_window: None,
            }),
            Some("tokens 10".into())
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
        assert_eq!(
            MetricFooter::format_usage(&Usage {
                input_tokens: None,
                output_tokens: None,
                total_tokens: None,
                context_window: None,
            }),
            None
        );
    }

    #[test]
    fn text_omits_unavailable_and_includes_hints() {
        let with_window = MetricFooter::text(
            80,
            "Ready",
            Some(Duration::from_millis(25)),
            Some(&Usage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                total_tokens: Some(15),
                context_window: Some(8_192),
            }),
        );
        assert!(with_window.contains("tokens 15/8192"));
        assert!(!with_window.contains("unavailable"));
        assert!(with_window.contains("Enter send"));

        let tokens_only = MetricFooter::text(
            40,
            "Ready",
            None,
            Some(&Usage {
                input_tokens: None,
                output_tokens: None,
                total_tokens: Some(10),
                context_window: None,
            }),
        );
        assert!(tokens_only.contains("tokens 10"));
        assert!(!tokens_only.contains("tokens 10/"));
        assert!(!tokens_only.contains("unavailable"));
        assert!(tokens_only.contains("Ctrl+C cancel/quit"));
    }
}
