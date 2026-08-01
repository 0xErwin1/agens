//! Reusable presentation widgets for the terminal surface.

mod blocks;
mod expand;
mod footer;
mod glyph;
mod hyperlink;
mod overlay;
mod overlay_list;
mod role_palette;

pub(crate) use blocks::{
    ACCENT_WIDTH, BlockContent, BlockLine, GUTTER_WIDTH, RowAccent, RowBullet, RowState,
    ThinkingBlock, ToolCallBlock, ToolResultBlock, VerbGroup, bounded_tool_preview,
};
pub use expand::DisplayMode;
pub(crate) use expand::{ExpandMode, ExpandableBody};
pub(crate) use footer::{FooterContext, MIN_BORDER_METRICS_WIDTH, MetricFooter};
pub(crate) use glyph::StatusGlyph;
pub(crate) use hyperlink::{apply_hyperlinks, hyperlinks_enabled};
pub(crate) use overlay::{
    OverlayConfig, OverlayFrame, OverlayKind, OverlayLayout, OverlayShell, OverlayShortcut,
    OverlaySizing,
};
pub(crate) use overlay_list::{OverlayList, OverlayRow};
pub(crate) use role_palette::RolePalette;
