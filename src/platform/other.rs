//! Other systems (Linux): no Apple faces and no Windows families; bundled open lookalikes and the
//! font manager's best match for each family name.

use skia_safe::{FontMgr, Typeface};
use std::path::PathBuf;

/// Whether installed fonts are Apple's own builds (used as exact fonts by PostScript name).
pub const NATIVE_APPLE_FONTS: bool = false;
/// The last-resort family for code points no other face covers.
pub const SYMBOL_FALLBACK: &str = "DejaVu Sans";

/// Where installed fonts live.
pub fn font_dirs() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
    ]
}

/// The closest installed family for an Apple face that is not installed.
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

/// A typeface from font file bytes (face `index` of a collection).
pub fn load_face(data: &[u8], index: usize) -> Option<Typeface> {
    FontMgr::new().new_from_data(data, Some(index))
}
