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

    /// Machine chrome the reader scans rather than reads — tool headers.
    ///
    /// One step below [`Self::assistant`] and one above [`Self::muted`], which
    /// is what puts the transcript's three greys in order: the answer reads
    /// loudest, what the agent ran reads next, and what it printed reads last.
    pub(crate) const fn machine() -> Color {
        rgb(0x8a, 0x91, 0x99)
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

    /// Code panel background — one step above the terminal's own, so a fenced
    /// block reads as a panel without becoming a second surface.
    pub(crate) const fn code_panel_bg() -> Color {
        rgb(0x1a, 0x1f, 0x29)
    }

    /// Selected overlay row background — brand hue collapsed to a dark wash.
    pub(crate) const fn selection_bg() -> Color {
        rgb(0x1b, 0x33, 0x30)
    }

    /// Selected overlay row foreground — one step above `assistant()` for bold text.
    pub(crate) const fn selection_fg() -> Color {
        rgb(0xd6, 0xd4, 0xcd)
    }

    /// Active lifecycle state. This blue is reserved for work still running.
    pub(crate) const fn running() -> Color {
        rgb(0x73, 0xd0, 0xff)
    }

    /// Band behind the user's own prompt — `user_identity` collapsed to a wash.
    ///
    /// It is the transcript's only full-width background outside diffs, which
    /// is what makes a turn findable at a glance. It never carries meaning on
    /// its own: the rail and the bullet say the same thing without colour.
    pub(crate) const fn user_band() -> Color {
        rgb(0x24, 0x1f, 0x33)
    }

    /// User identity marker and rail; deliberately outside the lifecycle palette.
    pub(crate) const fn user_identity() -> Color {
        rgb(0xd2, 0xa6, 0xff)
    }

    /// Navigation affordances such as links and list markers.
    pub(crate) const fn navigation() -> Color {
        Self::brand()
    }

    /// Assistant message identity marker and rail.
    pub(crate) const fn assistant_identity() -> Color {
        Self::brand()
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

    /// Markdown inline code; orange remains distinct from warning yellow.
    pub(crate) const fn markdown_code() -> Color {
        rgb(0xff, 0x8f, 0x40)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_palette_uses_ayu_rgb_accents() {
        assert_eq!(RolePalette::assistant(), rgb(0xbf, 0xbd, 0xb6));
        assert_eq!(RolePalette::running(), rgb(0x73, 0xd0, 0xff));
        assert_eq!(RolePalette::user_identity(), rgb(0xd2, 0xa6, 0xff));
        assert_eq!(RolePalette::success(), rgb(0xaa, 0xd9, 0x4c));
        assert_eq!(RolePalette::error(), rgb(0xf0, 0x71, 0x78));
        assert_eq!(RolePalette::brand(), rgb(0x95, 0xe6, 0xcb));
        assert_eq!(RolePalette::navigation(), RolePalette::brand());
        assert_eq!(RolePalette::assistant_identity(), RolePalette::brand());
        assert_eq!(RolePalette::markdown_heading(), RolePalette::brand());
        assert_eq!(RolePalette::markdown_quote(), RolePalette::brand());
        assert_eq!(RolePalette::markdown_strong(), RolePalette::selection_fg());
        assert_eq!(RolePalette::markdown_code(), rgb(0xff, 0x8f, 0x40));
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
            RolePalette::running(),
            RolePalette::success(),
            RolePalette::error(),
        ] {
            assert_ne!(hue, RolePalette::assistant());
            assert_ne!(hue, RolePalette::muted());
        }
    }

    #[test]
    fn lifecycle_colors_are_reserved_from_identity_navigation_and_code() {
        let lifecycle = [
            RolePalette::running(),
            RolePalette::success(),
            RolePalette::error(),
            RolePalette::warning(),
        ];
        let non_lifecycle = [
            RolePalette::user_identity(),
            RolePalette::assistant_identity(),
            RolePalette::navigation(),
            RolePalette::markdown_code(),
            RolePalette::assistant(),
            RolePalette::muted(),
        ];

        for state in lifecycle {
            for identity in non_lifecycle {
                assert_ne!(state, identity);
            }
        }
    }

    #[test]
    fn typed_block_colors_are_distinct_semantic_slots() {
        assert_eq!(RolePalette::diff_insert_bg(), rgb(0x14, 0x2a, 0x1c));
        assert_eq!(RolePalette::diff_delete_bg(), rgb(0x36, 0x18, 0x1c));
        assert_ne!(RolePalette::diff_insert_bg(), RolePalette::diff_delete_bg());
        assert_ne!(RolePalette::running(), RolePalette::muted());
        assert_ne!(RolePalette::user_identity(), RolePalette::running());
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
            RolePalette::running(),
            RolePalette::user_identity(),
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

    /// Relative luminance per WCAG 2.1, used only to keep the selection pair
    /// legible. Two slots can both be valid palette colours and still be
    /// unreadable stacked on each other, which no equality assertion catches.
    fn relative_luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("palette slots are RGB");
        };

        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };

        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    fn contrast_ratio(a: Color, b: Color) -> f64 {
        let (high, low) = {
            let (x, y) = (relative_luminance(a), relative_luminance(b));
            if x >= y { (x, y) } else { (y, x) }
        };

        (high + 0.05) / (low + 0.05)
    }

    #[test]
    fn selected_text_stays_readable_against_its_own_background() {
        let ratio = contrast_ratio(RolePalette::selection_fg(), RolePalette::selection_bg());
        assert!(
            ratio >= 7.0,
            "selection foreground and background contrast at {ratio:.2}:1"
        );

        // The brand mint is the trap: it is close enough to the selection
        // foreground that pairing them washes the selection out entirely,
        // and it reads as a plausible highlight colour at the call site.
        let against_brand = contrast_ratio(RolePalette::selection_fg(), RolePalette::brand());
        assert!(
            against_brand < 3.0,
            "brand was expected to be an unusable selection background, but contrasts at {against_brand:.2}:1"
        );
    }
}
