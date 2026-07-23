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
}
