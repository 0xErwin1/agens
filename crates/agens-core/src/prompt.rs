//! Built-in base system prompt, the single source every fallback site composes with.
//!
//! The effective system prompt for a turn is assembled in a fixed layer order: the
//! built-in base (this module), the agent's own configured or markdown prompt, the
//! AGENTS.md instruction layers, and finally the delegation discipline block. This
//! module owns only the first layer; the rest is composed by the call sites that
//! already own those layers.

/// The built-in identity every turn starts from, unless the caller supplies an
/// explicit override (such as the headless `--system` flag) that fully replaces it.
pub const BASE_SYSTEM_PROMPT: &str = "You are Agens, a helpful coding agent.";

/// Composes the built-in base with an optional configured prompt.
///
/// `configured` is trimmed and treated as absent when empty or whitespace-only, so a
/// blank `[agent].system_prompt` collapses to the base alone rather than leaving a
/// trailing separator. A present, non-empty value is appended after the base,
/// separated by a blank line.
pub fn base_system_prompt(configured: Option<&str>) -> String {
    match configured.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("{BASE_SYSTEM_PROMPT}\n\n{text}"),
        None => BASE_SYSTEM_PROMPT.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_configured_prompt_yields_base_alone() {
        assert_eq!(base_system_prompt(None), BASE_SYSTEM_PROMPT);
    }

    #[test]
    fn configured_prompt_composes_after_base() {
        assert_eq!(
            base_system_prompt(Some("You are Foo.")),
            "You are Agens, a helpful coding agent.\n\nYou are Foo."
        );
    }

    #[test]
    fn empty_configured_prompt_yields_base_alone() {
        assert_eq!(base_system_prompt(Some("")), BASE_SYSTEM_PROMPT);
    }

    #[test]
    fn whitespace_only_configured_prompt_yields_base_alone() {
        assert_eq!(base_system_prompt(Some("   \n ")), BASE_SYSTEM_PROMPT);
    }
}
