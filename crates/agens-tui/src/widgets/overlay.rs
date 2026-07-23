//! Soft overlay taxonomy over the existing single dialog + slash palette layer.

/// Overlay kinds for the one modal layer (palette, list picker, or confirm).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum OverlayKind {
    /// Slash command palette above the composer.
    Palette,
    /// List selection / search dialog (model, session, file, …).
    #[default]
    Picker,
    /// Permission (or similar) confirm with short-key answers.
    Confirm,
}

/// Shell helpers for overlay kind classification and Confirm short keys.
pub(crate) struct OverlayShell;

impl OverlayShell {
    /// Topmost kind: open palette wins over any dialog.
    pub(crate) const fn topmost(
        palette_open: bool,
        dialog_kind: Option<OverlayKind>,
    ) -> Option<OverlayKind> {
        if palette_open {
            Some(OverlayKind::Palette)
        } else {
            dialog_kind
        }
    }

    /// Maps Confirm short keys to permission answer tokens used in action ids.
    pub(crate) const fn confirm_answer(key: char) -> Option<&'static str> {
        match key {
            'a' => Some("allow-once"),
            'd' => Some("deny-once"),
            'A' => Some("allow-always"),
            'D' => Some("deny-always"),
            _ => None,
        }
    }

    /// Whether an action id ends with the Confirm answer suffix (`:allow-once`, …).
    pub(crate) fn action_matches_answer(action_id: &str, answer: &str) -> bool {
        action_id
            .rsplit_once(':')
            .is_some_and(|(_, suffix)| suffix == answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_short_keys_map_to_permission_answers() {
        assert_eq!(OverlayShell::confirm_answer('a'), Some("allow-once"));
        assert_eq!(OverlayShell::confirm_answer('d'), Some("deny-once"));
        assert_eq!(OverlayShell::confirm_answer('A'), Some("allow-always"));
        assert_eq!(OverlayShell::confirm_answer('D'), Some("deny-always"));
        assert_eq!(OverlayShell::confirm_answer('x'), None);
        assert_eq!(OverlayShell::confirm_answer('b'), None);
    }

    #[test]
    fn action_matches_answer_uses_final_colon_suffix() {
        assert!(OverlayShell::action_matches_answer(
            "permission:7:allow-once",
            "allow-once"
        ));
        assert!(!OverlayShell::action_matches_answer(
            "permission:7:allow-always",
            "allow-once"
        ));
        assert!(!OverlayShell::action_matches_answer(
            "allow-once",
            "allow-once"
        ));
    }

    #[test]
    fn topmost_prefers_palette_over_dialog() {
        assert_eq!(
            OverlayShell::topmost(true, Some(OverlayKind::Confirm)),
            Some(OverlayKind::Palette)
        );
        assert_eq!(
            OverlayShell::topmost(false, Some(OverlayKind::Picker)),
            Some(OverlayKind::Picker)
        );
        assert_eq!(OverlayShell::topmost(false, None), None);
        assert_eq!(
            OverlayShell::topmost(true, None),
            Some(OverlayKind::Palette)
        );
    }
}
