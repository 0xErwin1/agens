//! Reusable presentation widgets for the terminal surface.

mod blocks;
mod expand;
mod footer;
mod glyph;
mod role_palette;

pub(crate) use blocks::{ThinkingBlock, ToolRow};
pub(crate) use expand::{ExpandMode, ExpandableBody};
pub(crate) use footer::MetricFooter;
pub(crate) use glyph::StatusGlyph;
pub(crate) use role_palette::RolePalette;
