//! Fixed semantic colors for conversation roles and chrome.

use ratatui::style::Color;

/// Role and status colors shared by presentation widgets.
pub(crate) struct RolePalette;

impl RolePalette {
    pub(crate) const fn user() -> Color {
        Color::Cyan
    }

    pub(crate) const fn assistant() -> Color {
        Color::White
    }

    pub(crate) const fn thinking() -> Color {
        Color::Magenta
    }

    pub(crate) const fn tool() -> Color {
        Color::Magenta
    }

    pub(crate) const fn error() -> Color {
        Color::Red
    }

    pub(crate) const fn info() -> Color {
        Color::Yellow
    }

    pub(crate) const fn success() -> Color {
        Color::Green
    }

    pub(crate) const fn warning() -> Color {
        Color::Yellow
    }

    pub(crate) const fn muted() -> Color {
        Color::DarkGray
    }

    pub(crate) const fn chrome() -> Color {
        Color::Gray
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_palette_exposes_distinct_semantic_colors() {
        assert_eq!(RolePalette::user(), Color::Cyan);
        assert_eq!(RolePalette::assistant(), Color::White);
        assert_eq!(RolePalette::thinking(), Color::Magenta);
        assert_eq!(RolePalette::tool(), Color::Magenta);
        assert_eq!(RolePalette::error(), Color::Red);
        assert_eq!(RolePalette::info(), Color::Yellow);
        assert_eq!(RolePalette::success(), Color::Green);
        assert_eq!(RolePalette::warning(), Color::Yellow);
        assert_eq!(RolePalette::muted(), Color::DarkGray);
        assert_eq!(RolePalette::chrome(), Color::Gray);
        assert_ne!(RolePalette::user(), RolePalette::error());
        assert_ne!(RolePalette::thinking(), RolePalette::muted());
        assert_ne!(RolePalette::assistant(), RolePalette::tool());
    }
}
