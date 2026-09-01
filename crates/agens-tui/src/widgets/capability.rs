//! What the attached terminal can actually render, and how to meet it there.
//!
//! The palette names slots and the renderer never names a colour, but a slot
//! still resolves to 24-bit RGB. A terminal that cannot show 24-bit colour does
//! not degrade gracefully on its own — it drops the sequence and paints the
//! default, so a carefully chosen palette becomes one undifferentiated colour.
//! Quantizing is what keeps the *distinctions* the palette encodes when the
//! exact hues cannot survive.

use ratatui::style::Color;

/// How much colour the terminal can be sent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub enum ColorLevel {
    /// No colour at all: every slot resolves to the terminal's own default.
    None,
    /// The sixteen ANSI colours.
    Ansi16,
    /// The 256-colour cube.
    Ansi256,
    #[default]
    TrueColor,
}

#[cfg(feature = "perf-audit")]
impl ColorLevel {
    /// Stable trace-field label for the level's discriminant.
    pub(crate) const fn trace_label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Ansi16 => "ansi16",
            Self::Ansi256 => "ansi256",
            Self::TrueColor => "true_color",
        }
    }
}

/// Whether the terminal can show the chrome glyphs the transcript prefers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UnicodeLevel {
    /// Box drawing, geometric shapes and braille are safe.
    #[default]
    Extended,
    /// ASCII only, at the same column widths.
    Ascii,
}

/// Reads the colour level from the environment's own account of itself.
///
/// `COLORTERM` is the only positive statement of 24-bit support, and `TERM`
/// carries the rest. `NO_COLOR` is honoured because it is the one convention
/// users already expect to work everywhere.
pub(crate) fn detect_color_level(
    no_color: Option<&str>,
    override_value: Option<&str>,
    colorterm: Option<&str>,
    term: Option<&str>,
) -> ColorLevel {
    match override_value.map(str::trim) {
        Some("none" | "0") => return ColorLevel::None,
        Some("16") => return ColorLevel::Ansi16,
        Some("256") => return ColorLevel::Ansi256,
        Some("truecolor" | "24bit") => return ColorLevel::TrueColor,
        _ => {}
    }
    if no_color.is_some_and(|value| !value.is_empty()) {
        return ColorLevel::None;
    }

    match term {
        None | Some("" | "dumb") => return ColorLevel::None,
        _ => {}
    }
    if matches!(colorterm, Some("truecolor" | "24bit")) {
        return ColorLevel::TrueColor;
    }
    if term.is_some_and(|term| term.contains("256color")) {
        return ColorLevel::Ansi256;
    }
    ColorLevel::Ansi16
}

/// Whether the locale claims a UTF-8 encoding.
///
/// A terminal that is not in a UTF-8 locale will not render box drawing
/// correctly, and a glyph that renders as a replacement character costs the
/// reader more than the ASCII it stood in for.
pub(crate) fn detect_unicode_level(
    override_value: Option<&str>,
    locale: Option<&str>,
) -> UnicodeLevel {
    match override_value.map(str::trim) {
        Some("ascii" | "0") => return UnicodeLevel::Ascii,
        Some("extended" | "1") => return UnicodeLevel::Extended,
        _ => {}
    }
    match locale {
        Some(locale) if locale.to_ascii_lowercase().contains("utf") => UnicodeLevel::Extended,
        _ => UnicodeLevel::Ascii,
    }
}

/// What this process's terminal claims it can show.
///
/// The environment is read here and nowhere else in a render path, so a test
/// that builds its own backend renders the same frame under every `TERM`.
pub(crate) fn detect_capabilities() -> (ColorLevel, UnicodeLevel) {
    (
        detect_color_level(
            std::env::var("NO_COLOR").ok().as_deref(),
            std::env::var("AGENS_COLOR").ok().as_deref(),
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
        ),
        detect_unicode_level(
            std::env::var("AGENS_GLYPHS").ok().as_deref(),
            std::env::var("LC_ALL")
                .or_else(|_| std::env::var("LC_CTYPE"))
                .or_else(|_| std::env::var("LANG"))
                .ok()
                .as_deref(),
        ),
    )
}

/// Chrome glyphs the transcript draws itself with.
///
/// Every variant is one column wide in both sets. That is the whole contract:
/// a fallback that changed width would move the content column with the locale,
/// which is a worse failure than the glyph a terminal could not draw.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Glyph {
    AccentBar,
    ThinAccentBar,
    ActivityBullet,
    GroupBullet,
    AssistantBullet,
    UserBullet,
    Running,
    Succeeded,
    Failed,
    Branch,
    Cancelled,
    Ellipsis,
}

impl Glyph {
    /// The glyph, or its stand-in, at the same column width.
    pub(crate) const fn text(self, level: UnicodeLevel) -> &'static str {
        match level {
            UnicodeLevel::Extended => match self {
                Self::AccentBar => "┃",
                Self::ThinAccentBar => "❙",
                Self::ActivityBullet => "◆",
                Self::GroupBullet => "◈",
                Self::AssistantBullet | Self::Running => "●",
                Self::UserBullet => "❯",
                Self::Succeeded => "✓",
                Self::Failed => "✗",
                Self::Branch => "⎇",
                Self::Cancelled => "○",
                Self::Ellipsis => "…",
            },
            UnicodeLevel::Ascii => match self {
                Self::AccentBar => "|",
                Self::ThinAccentBar => ":",
                Self::ActivityBullet => "*",
                Self::GroupBullet => "#",
                Self::AssistantBullet | Self::Running => "o",
                Self::UserBullet => ">",
                Self::Succeeded => "+",
                Self::Failed => "x",
                Self::Branch => "@",
                Self::Cancelled => "-",
                Self::Ellipsis => "~",
            },
        }
    }
}

/// `color` as the nearest thing `level` can show.
///
/// Named and indexed colours pass through: they are already within every level
/// this quantizes to, and the terminal's own palette is a better answer for
/// them than any arithmetic here.
pub(crate) fn quantize(color: Color, level: ColorLevel) -> Color {
    let Color::Rgb(red, green, blue) = color else {
        return match level {
            ColorLevel::None => Color::Reset,
            _ => color,
        };
    };

    match level {
        ColorLevel::TrueColor => color,
        ColorLevel::Ansi256 => Color::Indexed(ansi256_index(red, green, blue)),
        ColorLevel::Ansi16 => nearest_ansi16(red, green, blue),
        ColorLevel::None => Color::Reset,
    }
}

/// Brings every painted cell down to what the terminal can show.
///
/// This runs over the finished frame rather than at each call site for the same
/// reason the hyperlink pass does: the buffer is the one place every colour has
/// arrived, including the ones this crate does not choose — a syntax theme's
/// output is as much a 24-bit colour as a palette slot, and it degrades the
/// same way.
pub(crate) fn quantize_buffer(buffer: &mut ratatui::buffer::Buffer, level: ColorLevel) {
    if level == ColorLevel::TrueColor {
        return;
    }
    for cell in &mut buffer.content {
        cell.fg = quantize(cell.fg, level);
        cell.bg = quantize(cell.bg, level);
    }
}

/// The 256-colour cube index for an RGB triple.
///
/// Greys are mapped to the ramp rather than the cube: the cube's own greys are
/// coarse, and chrome is mostly grey, so the ramp is where the distinctions the
/// footer and gutters depend on survive.
fn ansi256_index(red: u8, green: u8, blue: u8) -> u8 {
    // Generous, because the palette's greys are deliberately a little cool and
    // the cube offers only six grey steps against the ramp's twenty-four. The
    // chrome, muted and machine slots sit close together; the ramp is the only
    // place they stay apart.
    const GREY_SPREAD: u8 = 32;

    let spread = red.max(green).max(blue) - red.min(green).min(blue);
    if spread < GREY_SPREAD {
        let level = u16::from(red) + u16::from(green) + u16::from(blue);
        let grey = (level / 3) as u8;
        if grey < 8 {
            return 16;
        }
        if grey > 248 {
            return 231;
        }
        return 232 + ((u16::from(grey) - 8) * 24 / 240) as u8;
    }

    let axis = |value: u8| -> u16 {
        match value {
            0..=47 => 0,
            48..=114 => 1,
            other => u16::from((other - 35) / 40),
        }
    };
    (16 + 36 * axis(red) + 6 * axis(green) + axis(blue)) as u8
}

/// The nearest of the sixteen ANSI colours, by squared distance.
///
/// Comparing against the conventional values rather than picking by hue keeps
/// the mapping stable for the desaturated tones a terminal palette is mostly
/// made of, where hue is close to meaningless.
fn nearest_ansi16(red: u8, green: u8, blue: u8) -> Color {
    const ANSI16: [(Color, (u8, u8, u8)); 16] = [
        (Color::Black, (0, 0, 0)),
        (Color::Red, (170, 0, 0)),
        (Color::Green, (0, 170, 0)),
        (Color::Yellow, (170, 85, 0)),
        (Color::Blue, (0, 0, 170)),
        (Color::Magenta, (170, 0, 170)),
        (Color::Cyan, (0, 170, 170)),
        (Color::Gray, (170, 170, 170)),
        (Color::DarkGray, (85, 85, 85)),
        (Color::LightRed, (255, 85, 85)),
        (Color::LightGreen, (85, 255, 85)),
        (Color::LightYellow, (255, 255, 85)),
        (Color::LightBlue, (85, 85, 255)),
        (Color::LightMagenta, (255, 85, 255)),
        (Color::LightCyan, (85, 255, 255)),
        (Color::White, (255, 255, 255)),
    ];

    let distance = |(candidate_red, candidate_green, candidate_blue): (u8, u8, u8)| {
        let delta = |left: u8, right: u8| {
            let difference = i32::from(left) - i32::from(right);
            difference * difference
        };
        delta(red, candidate_red) + delta(green, candidate_green) + delta(blue, candidate_blue)
    };

    ANSI16
        .into_iter()
        .min_by_key(|(_, rgb)| distance(*rgb))
        .map_or(Color::Reset, |(color, _)| color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_states_its_own_colour_level() {
        let detect = |colorterm, term| detect_color_level(None, None, colorterm, Some(term));

        assert_eq!(
            detect(Some("truecolor"), "xterm-256color"),
            ColorLevel::TrueColor
        );
        assert_eq!(detect(None, "xterm-256color"), ColorLevel::Ansi256);
        assert_eq!(detect(None, "xterm"), ColorLevel::Ansi16);
        assert_eq!(detect(None, "dumb"), ColorLevel::None);
        assert_eq!(
            detect_color_level(None, None, Some("truecolor"), None),
            ColorLevel::None,
            "no terminal is not a colourful terminal"
        );
    }

    /// `NO_COLOR` is the one convention users already expect everywhere, and an
    /// explicit override still outranks it — someone who names a level is
    /// answering the question directly.
    #[test]
    fn no_color_wins_over_detection_and_loses_to_an_explicit_level() {
        assert_eq!(
            detect_color_level(Some("1"), None, Some("truecolor"), Some("xterm-256color")),
            ColorLevel::None
        );
        assert_eq!(
            detect_color_level(Some(""), None, Some("truecolor"), Some("xterm-256color")),
            ColorLevel::TrueColor,
            "an empty NO_COLOR is not a request"
        );
        assert_eq!(
            detect_color_level(Some("1"), Some("256"), None, Some("xterm")),
            ColorLevel::Ansi256
        );
    }

    #[test]
    fn a_non_utf8_locale_falls_back_to_ascii_glyphs() {
        assert_eq!(
            detect_unicode_level(None, Some("en_US.UTF-8")),
            UnicodeLevel::Extended
        );
        assert_eq!(detect_unicode_level(None, Some("C")), UnicodeLevel::Ascii);
        assert_eq!(detect_unicode_level(None, None), UnicodeLevel::Ascii);
        assert_eq!(
            detect_unicode_level(Some("ascii"), Some("en_US.UTF-8")),
            UnicodeLevel::Ascii
        );
    }

    /// The point of quantizing is that distinctions survive. Colours the palette
    /// keeps apart must not collapse into one another on the way down.
    #[test]
    fn quantizing_keeps_the_slots_the_palette_holds_apart() {
        for level in [ColorLevel::Ansi256, ColorLevel::Ansi16] {
            let error = quantize(super::super::RolePalette::error(), level);
            let success = quantize(super::super::RolePalette::success(), level);
            let warning = quantize(super::super::RolePalette::warning(), level);

            assert_ne!(error, success, "{level:?}");
            assert_ne!(error, warning, "{level:?}");
            assert_ne!(success, warning, "{level:?}");
        }

        // The greys are the hardest case and the one chrome depends on: three
        // slots within a few points of each other, on a level with six grey
        // steps in its cube.
        let chrome = quantize(super::super::RolePalette::chrome(), ColorLevel::Ansi256);
        let muted = quantize(super::super::RolePalette::muted(), ColorLevel::Ansi256);
        let machine = quantize(super::super::RolePalette::machine(), ColorLevel::Ansi256);
        assert_ne!(chrome, muted);
        assert_ne!(muted, machine);
    }

    /// The fallback exists so a terminal that cannot draw the glyph still gets
    /// a readable transcript — not so it gets a differently-shaped one. Equal
    /// width is the contract; distinctness is what makes the substitution worth
    /// making at all.
    #[test]
    fn every_glyph_keeps_its_column_and_its_identity_in_both_sets() {
        use unicode_width::UnicodeWidthStr;

        const ALL: [Glyph; 12] = [
            Glyph::AccentBar,
            Glyph::ThinAccentBar,
            Glyph::ActivityBullet,
            Glyph::GroupBullet,
            Glyph::AssistantBullet,
            Glyph::UserBullet,
            Glyph::Running,
            Glyph::Succeeded,
            Glyph::Failed,
            Glyph::Branch,
            Glyph::Cancelled,
            Glyph::Ellipsis,
        ];

        for glyph in ALL {
            let extended = glyph.text(UnicodeLevel::Extended);
            let ascii = glyph.text(UnicodeLevel::Ascii);
            assert_eq!(extended.width(), 1, "{glyph:?}");
            assert_eq!(ascii.width(), 1, "{glyph:?}");
            assert!(ascii.is_ascii(), "{glyph:?} falls back outside ASCII");
        }

        for level in [UnicodeLevel::Extended, UnicodeLevel::Ascii] {
            let mut seen = Vec::new();
            for glyph in ALL {
                let text = glyph.text(level);
                // Running and AssistantBullet share a mark on purpose: both mean
                // "this is the thing itself", one live and one settled.
                if matches!(glyph, Glyph::Running) {
                    continue;
                }
                assert!(!seen.contains(&text), "{level:?} reuses {text:?}");
                seen.push(text);
            }
        }
    }

    #[test]
    fn no_colour_resolves_every_slot_to_the_terminal_default() {
        assert_eq!(
            quantize(super::super::RolePalette::error(), ColorLevel::None),
            Color::Reset
        );
        assert_eq!(quantize(Color::Green, ColorLevel::None), Color::Reset);
    }

    #[test]
    fn indexed_and_named_colours_survive_every_level_they_already_fit() {
        for level in [
            ColorLevel::TrueColor,
            ColorLevel::Ansi256,
            ColorLevel::Ansi16,
        ] {
            assert_eq!(quantize(Color::Indexed(42), level), Color::Indexed(42));
            assert_eq!(quantize(Color::Green, level), Color::Green);
        }
    }

    #[test]
    fn greys_take_the_ramp_and_colours_take_the_cube() {
        assert!(
            (232..=255).contains(&ansi256_index(0x8a, 0x91, 0x99)),
            "a chrome grey lands on the grey ramp"
        );
        assert!(
            (16..232).contains(&ansi256_index(0xff, 0x33, 0x33)),
            "a saturated colour lands in the cube"
        );
    }
}
