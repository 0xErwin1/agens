#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

impl ReasoningEffort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestConfig {
    reasoning_effort: Option<ReasoningEffort>,
}

impl RequestConfig {
    pub fn with_reasoning_effort(value: &str) -> Result<Self, RequestConfigError> {
        let reasoning_effort = match value {
            "none" => ReasoningEffort::None,
            "minimal" => ReasoningEffort::Minimal,
            "low" => ReasoningEffort::Low,
            "medium" => ReasoningEffort::Medium,
            "high" => ReasoningEffort::High,
            "xhigh" => ReasoningEffort::XHigh,
            _ => {
                return Err(RequestConfigError::UnsupportedReasoningEffort(
                    value.to_owned(),
                ));
            }
        };

        Ok(Self {
            reasoning_effort: Some(reasoning_effort),
        })
    }

    pub const fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestConfigError {
    UnsupportedReasoningEffort(String),
}
