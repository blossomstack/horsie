//! Canonical thinking-effort vocabulary and the per-model wire dialects that
//! encode it.
//!
//! The *value* is portable across vendors; the *encoding* is not. Two models on
//! the same provider kind can need different request shapes (Anthropic Opus 4.8
//! takes `output_config.effort`, Haiku 4.5 rejects `effort` entirely), and one
//! model can need a different encoding than its provider kind implies (Kimi k3
//! on the Anthropic wire silently ignores `reasoning_effort`). So the dialect is
//! stored per model rather than inferred from the provider.

/// A canonical effort value. Absence of a value means "send no thinking
/// control at all"; [`ThinkingEffort::None`] means "explicitly disable".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ThinkingEffort {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Self::None,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::XHigh,
            "max" => Self::Max,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// True for the explicit "disable thinking" value.
    pub fn is_none_effort(self) -> bool {
        matches!(self, Self::None)
    }
}

/// How a canonical effort is encoded on the wire for a given model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingDialect {
    /// `output_config.effort` + adaptive thinking; `none` becomes
    /// `thinking:{type:"disabled"}`.
    AnthropicEffort,
    /// `output_config.effort` only, `thinking` omitted. Thinking cannot be
    /// disabled (Fable 5 rejects `{type:"disabled"}` with a 400).
    AnthropicAlwaysOn,
    /// `thinking:{type:"enabled",budget_tokens:N}` / `{type:"disabled"}`;
    /// `output_config.effort` only when the model offers effort values.
    AnthropicBudget,
    /// Top-level `reasoning_effort`.
    OpenAiEffort,
    /// `thinking:{type:"enabled"|"disabled"}` — toggle only, no effort.
    ZaiThinking,
    /// `thinking:{type:"enabled",keep:"all"}` — the only legal value.
    KimiThinking,
    /// No thinking control at all.
    NoControl,
}

impl ThinkingDialect {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "anthropic_effort" => Self::AnthropicEffort,
            "anthropic_always_on" => Self::AnthropicAlwaysOn,
            "anthropic_budget" => Self::AnthropicBudget,
            "openai_effort" => Self::OpenAiEffort,
            "zai_thinking" => Self::ZaiThinking,
            "kimi_thinking" => Self::KimiThinking,
            "none" => Self::NoControl,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicEffort => "anthropic_effort",
            Self::AnthropicAlwaysOn => "anthropic_always_on",
            Self::AnthropicBudget => "anthropic_budget",
            Self::OpenAiEffort => "openai_effort",
            Self::ZaiThinking => "zai_thinking",
            Self::KimiThinking => "kimi_thinking",
            Self::NoControl => "none",
        }
    }

    /// Whether this dialect can express the given effort at all. Config-time
    /// validation uses this so an impossible combination is rejected before it
    /// reaches a provider.
    pub fn supports(self, effort: ThinkingEffort) -> bool {
        match self {
            Self::AnthropicEffort | Self::OpenAiEffort | Self::AnthropicBudget => true,
            Self::AnthropicAlwaysOn => !effort.is_none_effort(),
            Self::ZaiThinking | Self::KimiThinking => effort.is_none_effort(),
            Self::NoControl => false,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    fn effort(v: &str) -> ThinkingEffort {
        ThinkingEffort::parse(v).expect("canonical effort")
    }

    #[test]
    fn parses_every_canonical_value() {
        for v in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            assert_eq!(effort(v).as_str(), v);
        }
    }

    #[test]
    fn rejects_unknown_effort() {
        assert!(ThinkingEffort::parse("ultra").is_none());
        assert!(ThinkingEffort::parse("").is_none());
    }

    #[test]
    fn none_is_distinguishable() {
        assert!(effort("none").is_none_effort());
        assert!(!effort("low").is_none_effort());
    }

    #[test]
    fn parses_every_dialect() {
        for d in [
            "anthropic_effort",
            "anthropic_always_on",
            "anthropic_budget",
            "openai_effort",
            "zai_thinking",
            "kimi_thinking",
            "none",
        ] {
            assert_eq!(
                ThinkingDialect::parse(d).expect("known dialect").as_str(),
                d
            );
        }
        assert!(ThinkingDialect::parse("bogus").is_none());
    }

    #[test]
    fn always_on_dialect_rejects_none_effort() {
        let d = ThinkingDialect::AnthropicAlwaysOn;
        assert!(
            !d.supports(effort("none")),
            "Fable 5 cannot disable thinking"
        );
        assert!(d.supports(effort("high")));
    }

    #[test]
    fn none_dialect_supports_nothing() {
        for v in ["none", "low", "high", "max"] {
            assert!(!ThinkingDialect::NoControl.supports(effort(v)));
        }
    }

    #[test]
    fn effort_dialects_support_all_values() {
        for d in [
            ThinkingDialect::AnthropicEffort,
            ThinkingDialect::OpenAiEffort,
        ] {
            for v in ["none", "low", "medium", "high", "xhigh", "max"] {
                assert!(d.supports(effort(v)));
            }
        }
    }

    #[test]
    fn toggle_dialects_only_express_none() {
        for d in [ThinkingDialect::ZaiThinking, ThinkingDialect::KimiThinking] {
            assert!(d.supports(effort("none")));
            assert!(!d.supports(effort("high")));
        }
    }
}
