//! Reusable presentation widgets for the terminal surface.

mod blocks;
mod capability;
mod expand;
mod footer;
mod glyph;
mod hyperlink;
mod overlay;
mod overlay_list;
mod role_palette;

pub(crate) use blocks::{
    ACCENT_WIDTH, BlockContent, BlockLine, GUTTER_WIDTH, RowAccent, RowBullet, RowState,
    TOOL_DETAIL_CONTENT_ROWS, ThinkingBlock, ToolCallBlock, ToolResultBlock, VerbGroup,
    argument_line, bounded_argument_preview, bounded_tool_preview, format_ask_user_result_lines,
    is_ask_user_tool_name, tool_argument_detail_text, tool_header,
};
pub use capability::{ColorLevel, UnicodeLevel};
pub(crate) use capability::{Glyph, detect_color_level, detect_unicode_level, quantize_buffer};
pub use expand::DisplayMode;
pub(crate) use expand::{ExpandMode, ExpandableBody};
pub(crate) use footer::{FooterContext, MIN_BORDER_METRICS_WIDTH, MetricFooter};
pub(crate) use glyph::StatusGlyph;
pub(crate) use hyperlink::{apply_hyperlinks, hyperlinks_enabled};
pub(crate) use overlay::{
    OverlayConfig, OverlayFrame, OverlayKind, OverlayLayout, OverlayShell, OverlayShortcut,
    OverlaySizing, truncate_columns,
};
pub(crate) use overlay_list::{OverlayList, OverlayRow, ROW_LABEL_RESERVE};
pub(crate) use role_palette::RolePalette;
