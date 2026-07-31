//! Ayu-inspired semantic colors (dark) for conversation roles and chrome.
//! Reference: Ayu Mirage/Dark accents adapted for terminal RGB; not a multi-theme engine.
//!
//! The transcript keeps prose neutral while spending a small semantic palette on
//! Markdown hierarchy, navigation, and lifecycle state. Colour reinforces structure;
//! it does not replace weight, underline, rails, or other non-colour cues.

use ratatui::style::Color;

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// Role and status colors shared by presentation widgets.
pub(crate) struct RolePalette;

impl RolePalette {
    /// Primary transcript foreground — assistant body, prompts, row labels.
    pub(crate) const fn assistant() -> Color {
        rgb(0xbf, 0xbd, 0xb6)
    }

    /// Errors — Ayu red.
    pub(crate) const fn error() -> Color {
        rgb(0xf0, 0x71, 0x78)
    }

    /// Success / ok — Ayu green.
    pub(crate) const fn success() -> Color {
        rgb(0xaa, 0xd9, 0x4c)
    }

    /// Warnings — Ayu yellow.
    pub(crate) const fn warning() -> Color {
        rgb(0xe6, 0xb4, 0x50)
    }

    /// Muted meta / gutter.
    pub(crate) const fn muted() -> Color {
        rgb(0x5c, 0x67, 0x73)
    }

    /// Secondary chrome (borders, hints).
    pub(crate) const fn chrome() -> Color {
        rgb(0x6c, 0x73, 0x80)
    }

    /// Brand / header product accent — Ayu cyan.
    pub(crate) const fn brand() -> Color {
        rgb(0x95, 0xe6, 0xcb)
    }

    /// Row background for inserted diff lines — dim green wash.
    pub(crate) const fn diff_insert_bg() -> Color {
        rgb(0x14, 0x2a, 0x1c)
    }

    /// Row background for deleted diff lines — dim red wash.
    pub(crate) const fn diff_delete_bg() -> Color {
        rgb(0x36, 0x18, 0x1c)
    }

    /// Selected overlay row background — brand hue collapsed to a dark wash.
    pub(crate) const fn selection_bg() -> Color {
        rgb(0x1b, 0x33, 0x30)
    }

    /// Selected overlay row foreground — one step above `assistant()` for bold text.
    pub(crate) const fn selection_fg() -> Color {
        rgb(0xd6, 0xd4, 0xcd)
    }

    /// Active work and row operands.
    pub(crate) const fn accent_active() -> Color {
        rgb(0x73, 0xd0, 0xff)
    }

    /// Navigation affordances such as links and list markers.
    pub(crate) const fn navigation() -> Color {
        rgb(0x73, 0xd0, 0xff)
    }

    /// Markdown heading accent.
    pub(crate) const fn markdown_heading() -> Color {
        Self::brand()
    }

    /// Markdown quote rail accent.
    pub(crate) const fn markdown_quote() -> Color {
        Self::brand()
    }

    /// Markdown strong text: brighter than body prose without becoming a state.
    pub(crate) const fn markdown_strong() -> Color {
        Self::selection_fg()
    }

    /// Markdown inline code: warm enough to separate tokens from prose.
    pub(crate) const fn markdown_code() -> Color {
        Self::warning()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_palette_uses_ayu_rgb_accents() {
        assert_eq!(RolePalette::assistant(), rgb(0xbf, 0xbd, 0xb6));
        assert_eq!(RolePalette::accent_active(), rgb(0x73, 0xd0, 0xff));
        assert_eq!(RolePalette::success(), rgb(0xaa, 0xd9, 0x4c));
        assert_eq!(RolePalette::error(), rgb(0xf0, 0x71, 0x78));
        assert_eq!(RolePalette::brand(), rgb(0x95, 0xe6, 0xcb));
        assert_eq!(RolePalette::navigation(), RolePalette::accent_active());
        assert_eq!(RolePalette::markdown_heading(), RolePalette::brand());
        assert_eq!(RolePalette::markdown_quote(), RolePalette::brand());
        assert_eq!(RolePalette::markdown_strong(), RolePalette::selection_fg());
        assert_eq!(RolePalette::markdown_code(), RolePalette::warning());
    }

    /// The transcript's text colours must stay greys, so a row's words can never
    /// be mistaken for the accent or for a state.
    #[test]
    fn the_text_greys_carry_no_hue_while_the_accent_and_states_do() {
        for grey in [
            RolePalette::assistant(),
            RolePalette::muted(),
            RolePalette::chrome(),
        ] {
            let Color::Rgb(red, green, blue) = grey else {
                panic!("palette slots are RGB");
            };
            let spread = u16::from(red.max(green).max(blue)) - u16::from(red.min(green).min(blue));
            assert!(spread <= 0x20, "{grey:?} is not a grey");
        }

        for hue in [
            RolePalette::accent_active(),
            RolePalette::success(),
            RolePalette::error(),
        ] {
            assert_ne!(hue, RolePalette::assistant());
            assert_ne!(hue, RolePalette::muted());
        }
    }

    #[test]
    fn typed_block_colors_are_distinct_semantic_slots() {
        assert_eq!(RolePalette::diff_insert_bg(), rgb(0x14, 0x2a, 0x1c));
        assert_eq!(RolePalette::diff_delete_bg(), rgb(0x36, 0x18, 0x1c));
        assert_ne!(RolePalette::diff_insert_bg(), RolePalette::diff_delete_bg());
        assert_ne!(RolePalette::accent_active(), RolePalette::muted());
    }

    #[test]
    fn selection_slots_are_distinct_and_desaturated_against_brand() {
        assert_eq!(RolePalette::selection_bg(), rgb(0x1b, 0x33, 0x30));
        assert_eq!(RolePalette::selection_fg(), rgb(0xd6, 0xd4, 0xcd));

        for existing in [
            RolePalette::assistant(),
            RolePalette::error(),
            RolePalette::warning(),
            RolePalette::success(),
            RolePalette::muted(),
            RolePalette::chrome(),
            RolePalette::brand(),
            RolePalette::diff_insert_bg(),
            RolePalette::diff_delete_bg(),
            RolePalette::accent_active(),
        ] {
            assert_ne!(RolePalette::selection_bg(), existing);
            assert_ne!(RolePalette::selection_fg(), existing);
        }

        let (Color::Rgb(sr, sg, sb), Color::Rgb(br, bg, bb)) =
            (RolePalette::selection_bg(), RolePalette::brand())
        else {
            panic!("palette slots are RGB");
        };
        assert!(sr < br && sg < bg && sb < bb);
    }
}
