//! Shared expand modes for thinking body and tool output detail.

/// Presentation mode for expandable conversation body content.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ExpandMode {
    /// Body is visible while content is still streaming.
    Streaming,
    /// Body is visible after an explicit expand.
    Expanded,
    /// Body is hidden; title/row chrome remains.
    #[default]
    Collapsed,
}

impl ExpandMode {
    /// Whether the detail body should be rendered.
    pub(crate) const fn shows_body(self) -> bool {
        matches!(self, Self::Streaming | Self::Expanded)
    }

    /// Mode when a stream starts (thinking/assistant detail).
    pub(crate) const fn begin_stream() -> Self {
        Self::Streaming
    }

    /// Auto-collapse when streaming finishes. User-pinned Expanded is preserved.
    pub(crate) const fn finish_stream(self) -> Self {
        match self {
            Self::Streaming => Self::Collapsed,
            other => other,
        }
    }

    /// Shared Ctrl+O detail path. Streaming is left alone (still live).
    pub(crate) const fn toggle_detail(self) -> Self {
        match self {
            Self::Collapsed => Self::Expanded,
            Self::Expanded => Self::Collapsed,
            Self::Streaming => Self::Streaming,
        }
    }
}

/// Thin holder for expand mode on a presentation block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExpandableBody {
    pub mode: ExpandMode,
}

impl ExpandableBody {
    pub(crate) const fn new(mode: ExpandMode) -> Self {
        Self { mode }
    }

    pub(crate) const fn is_visible(self) -> bool {
        self.mode.shows_body()
    }

    pub(crate) const fn toggle_detail(self) -> Self {
        Self {
            mode: self.mode.toggle_detail(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_mode_shows_body_for_streaming_and_expanded_only() {
        assert!(ExpandMode::Streaming.shows_body());
        assert!(ExpandMode::Expanded.shows_body());
        assert!(!ExpandMode::Collapsed.shows_body());
    }

    #[test]
    fn finish_stream_auto_collapses_streaming_and_preserves_expanded() {
        assert_eq!(ExpandMode::Streaming.finish_stream(), ExpandMode::Collapsed);
        assert_eq!(ExpandMode::Expanded.finish_stream(), ExpandMode::Expanded);
        assert_eq!(ExpandMode::Collapsed.finish_stream(), ExpandMode::Collapsed);
    }

    #[test]
    fn toggle_detail_is_shared_path_and_ignores_live_stream() {
        assert_eq!(ExpandMode::Collapsed.toggle_detail(), ExpandMode::Expanded);
        assert_eq!(ExpandMode::Expanded.toggle_detail(), ExpandMode::Collapsed);
        assert_eq!(ExpandMode::Streaming.toggle_detail(), ExpandMode::Streaming);
        assert_eq!(ExpandMode::begin_stream(), ExpandMode::Streaming);
    }

    #[test]
    fn expandable_body_mirrors_mode_visibility_and_transitions() {
        let streaming = ExpandableBody::new(ExpandMode::Streaming);
        assert!(streaming.is_visible());

        let finished = ExpandableBody::new(streaming.mode.finish_stream());
        assert_eq!(finished.mode, ExpandMode::Collapsed);
        assert!(!finished.is_visible());

        let expanded = finished.toggle_detail();
        assert_eq!(expanded.mode, ExpandMode::Expanded);
        assert!(expanded.is_visible());
    }
}
