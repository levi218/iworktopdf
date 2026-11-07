//! Offline converter from Apple iWork documents (.numbers, .key, .pages) to PDF.
//!
//! Documents are decoded natively (IWA archives: Snappy-framed protobuf) and drawn with Skia's PDF
//! backend. Platform-specific font handling lives in `platform`; everything else is shared.

pub mod bidi;
pub mod bytes;
pub mod categories;
pub mod cell_format;
pub mod chart;
pub mod compare;
pub mod drawables;
pub mod error;
pub mod fonts;
pub mod formula;
pub mod iwa;
pub mod numbers_table;
pub mod pdfium;
pub mod pivot;
pub mod platform;
pub mod render;
pub mod richtext;
pub mod shaper;
pub mod styles;
pub mod table;
pub mod text;
pub mod tiff;
