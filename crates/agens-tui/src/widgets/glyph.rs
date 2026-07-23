//! Tick-driven status glyphs for active turn feedback.

use std::time::Duration;

/// Terminal-native spinner frames driven by the TUI tick clock.
///
/// Motion is for active states only. Idle callers receive a static glyph that
/// does not change as ticks advance.
pub(crate) struct StatusGlyph;

impl StatusGlyph {
    const FRAME_PERIOD_MS: u128 = 80;
    const FRAMES: &'static [&'static str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    const IDLE: &'static str = "·";

    /// Returns the glyph for the given activity flag and clock.
    ///
    /// When `active` is false the result is always [`Self::IDLE`] regardless of
    /// `now`. When active, the frame is deterministic for a given tick duration.
    pub(crate) fn char(active: bool, now: Duration) -> &'static str {
        if !active {
            return Self::IDLE;
        }

        Self::FRAMES[Self::frame_index(now)]
    }

    /// Zero-based frame index under a fake or real tick clock.
    pub(crate) fn frame_index(now: Duration) -> usize {
        let step = now.as_millis() / Self::FRAME_PERIOD_MS;
        (step as usize) % Self::FRAMES.len()
    }

    /// Formats a status label with a tick glyph when the surface is active.
    ///
    /// Idle labels are returned unchanged so chrome stays visually static.
    pub(crate) fn decorate_status(active: bool, label: &str, now: Duration) -> String {
        if active {
            format!("{} {label}", Self::char(true, now))
        } else {
            label.to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_glyph_advances_deterministically_with_fake_tick() {
        let frame_0 = StatusGlyph::char(true, Duration::from_millis(0));
        let frame_1 = StatusGlyph::char(true, Duration::from_millis(80));
        let frame_2 = StatusGlyph::char(true, Duration::from_millis(160));
        let frame_n = StatusGlyph::char(true, Duration::from_millis(80 * 7));

        assert_eq!(frame_0, "⠋");
        assert_eq!(frame_1, "⠙");
        assert_eq!(frame_2, "⠹");
        assert_eq!(frame_n, "⠧");
        assert_eq!(StatusGlyph::frame_index(Duration::from_millis(80 * 10)), 0);
        assert_ne!(frame_0, frame_1);
        assert_ne!(frame_1, frame_2);
    }

    #[test]
    fn idle_glyph_is_static_across_ticks() {
        let early = StatusGlyph::char(false, Duration::from_millis(0));
        let mid = StatusGlyph::char(false, Duration::from_millis(80 * 3));
        let late = StatusGlyph::char(false, Duration::from_millis(80 * 99));

        assert_eq!(early, "·");
        assert_eq!(early, mid);
        assert_eq!(mid, late);
    }

    #[test]
    fn active_frame_index_is_stable_within_period() {
        assert_eq!(
            StatusGlyph::frame_index(Duration::from_millis(0)),
            StatusGlyph::frame_index(Duration::from_millis(79))
        );
        assert_ne!(
            StatusGlyph::frame_index(Duration::from_millis(79)),
            StatusGlyph::frame_index(Duration::from_millis(80))
        );
    }

    #[test]
    fn decorate_status_spins_only_when_active() {
        let active_early = StatusGlyph::decorate_status(true, "Waiting", Duration::from_millis(0));
        let active_later = StatusGlyph::decorate_status(true, "Waiting", Duration::from_millis(80));
        let idle_early = StatusGlyph::decorate_status(false, "Ready", Duration::from_millis(0));
        let idle_later = StatusGlyph::decorate_status(false, "Ready", Duration::from_millis(800));

        assert_eq!(active_early, "⠋ Waiting");
        assert_eq!(active_later, "⠙ Waiting");
        assert_ne!(active_early, active_later);
        assert_eq!(idle_early, "Ready");
        assert_eq!(idle_later, "Ready");
    }
}
