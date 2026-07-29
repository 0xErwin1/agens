//! What Moonshot accepts, as data rather than as conditionals scattered through
//! the encoder.
//!
//! Every value here was confirmed against the live API. Where the published
//! documentation and a widely-used third-party client disagreed, the live
//! response decided.

use agens_core::ReasoningEffort;

/// The one Moonshot model that accepts `reasoning_effort`, and the values it
/// accepts. Its `/v1/models` entry reports `low`, `high`, `max`, defaulting to
/// `max` — note the absence of OpenAI's `medium`.
const REASONING_EFFORT_MODEL: &str = "kimi-k3";
const REASONING_EFFORT_VALUES: [ReasoningEffort; 3] = [
    ReasoningEffort::Low,
    ReasoningEffort::High,
    ReasoningEffort::Max,
];

/// The substring Moonshot puts in an error when a request outruns the model's
/// context, which is the only way to tell that case apart from other rejections.
pub(super) const CONTEXT_OVERFLOW_MARKER: &str = "exceeded model token limit";

/// The `reasoning_effort` value to send for a model, or `None` to omit the key.
///
/// Reasoning is a per-model capability here, not a per-provider one: `kimi-k3`
/// takes an effort, while the `kimi-k2.*` models reason on their own and expose
/// no knob to turn. An effort the model does not accept is dropped rather than
/// approximated, because silently substituting a neighbouring value would bill
/// the user for depth they did not ask for.
pub(super) fn reasoning_effort(
    model: &str,
    effort: Option<ReasoningEffort>,
) -> Option<&'static str> {
    if model != REASONING_EFFORT_MODEL {
        return None;
    }

    let effort = effort?;
    REASONING_EFFORT_VALUES
        .contains(&effort)
        .then_some(effort.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_k3_accepts_only_the_efforts_the_api_advertises() {
        assert_eq!(
            reasoning_effort("kimi-k3", Some(ReasoningEffort::Low)),
            Some("low")
        );
        assert_eq!(
            reasoning_effort("kimi-k3", Some(ReasoningEffort::High)),
            Some("high")
        );
        assert_eq!(
            reasoning_effort("kimi-k3", Some(ReasoningEffort::Max)),
            Some("max")
        );
    }

    #[test]
    fn an_effort_kimi_k3_does_not_accept_is_dropped_rather_than_approximated() {
        for unsupported in [
            ReasoningEffort::None,
            ReasoningEffort::Minimal,
            ReasoningEffort::Medium,
            ReasoningEffort::XHigh,
        ] {
            assert_eq!(reasoning_effort("kimi-k3", Some(unsupported)), None);
        }
    }

    #[test]
    fn models_without_an_effort_knob_never_receive_one() {
        for model in [
            "kimi-k2.6",
            "kimi-k2.7-code",
            "kimi-k2.7-code-highspeed",
            "kimi-k4",
        ] {
            for effort in [
                None,
                Some(ReasoningEffort::Low),
                Some(ReasoningEffort::High),
                Some(ReasoningEffort::Max),
            ] {
                assert_eq!(reasoning_effort(model, effort), None, "{model}");
            }
        }
    }

    #[test]
    fn an_unset_effort_leaves_the_model_default_in_place() {
        assert_eq!(reasoning_effort("kimi-k3", None), None);
    }
}
