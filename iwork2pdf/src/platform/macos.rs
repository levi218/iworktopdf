//! macOS: the installed fonts are the very builds Apple's exporter uses (Helvetica Neue, Times,
//! Menlo, Hiragino, ...), so they are indexed by PostScript name and used as exact fonts: their own
//! advance widths and kerning, no harvested widths, no substitutes.

use skia_safe::{FontMgr, Typeface};
use std::path::PathBuf;

/// Whether installed fonts are Apple's own builds (used as exact fonts by PostScript name).
pub const NATIVE_APPLE_FONTS: bool = true;
/// The last-resort family for code points no other face covers.
pub const SYMBOL_FALLBACK: &str = "Apple Symbols";

/// Where installed fonts live.
pub fn font_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ["/System/Library/Fonts", "/Library/Fonts"]
        .iter()
        .map(PathBuf::from)
        .collect();
    // Downloadable system fonts (Canela, Founders Grotesk, Graphik, ...) once fetched by an app.
    dirs.push(PathBuf::from(
        "/System/Library/AssetsV2/com_apple_MobileAsset_Font7",
    ));
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Library/Fonts"));
    }
    dirs
}

/// Faces missing here are missing on Apple's side too (it sets them in Helvetica).
pub fn system_family(_base: &str) -> Option<&'static str> {
    None
}

/// Same-design font files opened by name for a PostScript name (none on this platform).
pub fn installed_files(_ps: &str) -> &'static [&'static str] {
    &[]
}

/// The Helvetica Neue stand-in when IWORK2PDF_HN is unset (only used if the real face is absent).
pub fn helvetica_neue_auto() -> &'static str {
    "roboto"
}

/// Skia's CoreText backend loads only face 0 of a collection whatever index is asked for, so
/// face `index` of a .ttc/.otc is rebuilt as a standalone font first.
pub fn load_face(data: &[u8], index: usize) -> Option<Typeface> {
    let mgr = FontMgr::new();
    if index == 0 && !crate::fonts::is_collection(data) {
        return mgr.new_from_data(data, None);
    }
    let standalone = crate::fonts::collection_face(data, index)?;
    mgr.new_from_data(&standalone, None)
}
