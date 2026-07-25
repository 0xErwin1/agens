//! Reusable presentation widgets for the terminal surface.

mod blocks;
mod expand;
mod footer;
mod glyph;
mod overlay;
mod overlay_list;
mod role_palette;

pub(crate) use blocks::{
    BlockContent, BlockLine, GUTTER_WIDTH, RowBullet, RowState, ThinkingBlock, ToolCallBlock,
    ToolResultBlock, ToolRow, VerbGroup,
};
pub use expand::DisplayMode;
pub(crate) use expand::{ExpandMode, ExpandableBody};
pub(crate) use footer::{FooterContext, MetricFooter};
pub(crate) use glyph::StatusGlyph;
pub(crate) use overlay::{
    OverlayConfig, OverlayFrame, OverlayKind, OverlayLayout, OverlayShell, OverlayShortcut,
    OverlaySizing,
};
pub(crate) use overlay_list::{OverlayList, OverlayRow};
pub(crate) use role_palette::RolePalette;
