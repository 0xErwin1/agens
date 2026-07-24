//! Ayu-inspired semantic colors (dark) for conversation roles and chrome.
//! Reference: Ayu Mirage/Dark accents adapted for terminal RGB; not a multi-theme engine.

use ratatui::style::Color;

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// Role and status colors shared by presentation widgets.
pub(crate) struct RolePalette;

impl RolePalette {
    /// User prompts — Ayu orange.
    pub(crate) const fn user() -> Color {
        rgb(0xff, 0x8f, 0x40)
    }

    /// Assistant body — soft primary foreground.
    pub(crate) const fn assistant() -> Color {
        rgb(0xbf, 0xbd, 0xb6)
    }

    /// Thinking / reasoning — Ayu purple.
    pub(crate) const fn thinking() -> Color {
        rgb(0xd2, 0xa6, 0xff)
    }

    /// Tool headers — Ayu blue.
    pub(crate) const fn tool() -> Color {
        rgb(0x59, 0xc2, 0xff)
    }

    /// Errors — Ayu red.
    pub(crate) const fn error() -> Color {
        rgb(0xf0, 0x71, 0x78)
    }

    /// Info labels — Ayu yellow.
    pub(crate) const fn info() -> Color {
        rgb(0xe6, 0xb4, 0x50)
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

    /// Accent bar for user turns (Grok-like quote affordance).
    pub(crate) const fn user_bar() -> Color {
        rgb(0xff, 0xb4, 0x54)
    }

    /// Brand / header product accent — Ayu cyan.
    pub(crate) const fn brand() -> Color {
        rgb(0x95, 0xe6, 0xcb)
    }

    /// File-path operand in typed tool headers — distinct from the tool accent.
    pub(crate) const fn path() -> Color {
        rgb(0x5c, 0xcf, 0xe5)
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

    /// Accent for a running/active block's pulsing gutter.
    ///
    /// Consumed by the S3 running-block animation pass; defined here with the
    /// rest of the typed-block palette.
    #[allow(dead_code)]
    pub(crate) const fn accent_active() -> Color {
        rgb(0x73, 0xd0, 0xff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_palette_uses_ayu_rgb_accents() {
        assert_eq!(RolePalette::user(), rgb(0xff, 0x8f, 0x40));
        assert_eq!(RolePalette::thinking(), rgb(0xd2, 0xa6, 0xff));
        assert_eq!(RolePalette::tool(), rgb(0x59, 0xc2, 0xff));
        assert_eq!(RolePalette::success(), rgb(0xaa, 0xd9, 0x4c));
        assert_eq!(RolePalette::error(), rgb(0xf0, 0x71, 0x78));
        assert_eq!(RolePalette::brand(), rgb(0x95, 0xe6, 0xcb));
        assert_ne!(RolePalette::user(), RolePalette::assistant());
        assert_ne!(RolePalette::tool(), RolePalette::thinking());
    }

    #[test]
    fn typed_block_colors_are_distinct_semantic_slots() {
        assert_eq!(RolePalette::path(), rgb(0x5c, 0xcf, 0xe5));
        assert_eq!(RolePalette::diff_insert_bg(), rgb(0x14, 0x2a, 0x1c));
        assert_eq!(RolePalette::diff_delete_bg(), rgb(0x36, 0x18, 0x1c));
        assert_eq!(RolePalette::accent_active(), rgb(0x73, 0xd0, 0xff));

        assert_ne!(RolePalette::path(), RolePalette::tool());
        assert_ne!(RolePalette::path(), RolePalette::muted());
        assert_ne!(RolePalette::diff_insert_bg(), RolePalette::diff_delete_bg());
        assert_ne!(RolePalette::accent_active(), RolePalette::tool());
    }

    #[test]
    fn selection_slots_are_distinct_and_desaturated_against_brand() {
        assert_eq!(RolePalette::selection_bg(), rgb(0x1b, 0x33, 0x30));
        assert_eq!(RolePalette::selection_fg(), rgb(0xd6, 0xd4, 0xcd));

        for existing in [
            RolePalette::user(),
            RolePalette::assistant(),
            RolePalette::thinking(),
            RolePalette::tool(),
            RolePalette::error(),
            RolePalette::info(),
            RolePalette::success(),
            RolePalette::muted(),
            RolePalette::chrome(),
            RolePalette::user_bar(),
            RolePalette::brand(),
            RolePalette::path(),
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
