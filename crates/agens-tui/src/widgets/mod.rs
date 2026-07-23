//! Reusable presentation widgets for the terminal surface.

mod blocks;
mod expand;
mod footer;
mod glyph;
mod overlay;
mod role_palette;

pub(crate) use blocks::{ThinkingBlock, ToolRow};
pub(crate) use expand::{ExpandMode, ExpandableBody};
pub(crate) use footer::{FooterContext, MetricFooter};
pub(crate) use glyph::StatusGlyph;
pub(crate) use overlay::{OverlayKind, OverlayShell};
pub(crate) use role_palette::RolePalette;
