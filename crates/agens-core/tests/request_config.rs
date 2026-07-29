use agens_core::{ReasoningEffort, RequestConfig, RequestConfigError};

#[test]
fn reasoning_effort_accepts_provider_supported_values() {
    for (value, expected) in [
        ("none", ReasoningEffort::None),
        ("minimal", ReasoningEffort::Minimal),
        ("low", ReasoningEffort::Low),
        ("medium", ReasoningEffort::Medium),
        ("high", ReasoningEffort::High),
        ("xhigh", ReasoningEffort::XHigh),
        ("max", ReasoningEffort::Max),
    ] {
        let config = RequestConfig::with_reasoning_effort(value).expect("value should be valid");

        assert_eq!(config.reasoning_effort(), Some(expected));
        assert_eq!(expected.as_str(), value);
    }
}

#[test]
fn reasoning_effort_value_preserves_the_requested_effort() {
    let config = RequestConfig::with_reasoning_effort_value(ReasoningEffort::High);

    assert_eq!(config.reasoning_effort(), Some(ReasoningEffort::High));
}

#[test]
fn reasoning_effort_value_handles_a_different_supported_effort() {
    let config = RequestConfig::with_reasoning_effort_value(ReasoningEffort::None);

    assert_eq!(config.reasoning_effort(), Some(ReasoningEffort::None));
}

#[test]
fn request_config_leaves_reasoning_effort_unset_by_default() {
    assert_eq!(RequestConfig::default().reasoning_effort(), None);
}

#[test]
fn reasoning_effort_rejects_unsupported_values() {
    assert_eq!(
        RequestConfig::with_reasoning_effort("maximum"),
        Err(RequestConfigError::UnsupportedReasoningEffort(
            "maximum".to_owned()
        ))
    );
}
