//! Platform-specific font handling. Everything else in the converter is shared; each platform
//! module exports the same items:
//! - `font_dirs()`: where installed fonts live;
//! - `NATIVE_APPLE_FONTS`: whether those installed fonts are Apple's own builds (macOS), so a face
//!   found there by PostScript name is used as the document's exact font, like a supplied one;
//! - `system_family(base)`: the closest installed family for an Apple face that is not installed;
//! - `installed_files(ps)`: same-design faces opened by file name (families the font manager
//!   files under a parent family);
//! - `helvetica_neue_auto()`: the Helvetica Neue stand-in when `IWORK2PDF_HN` is unset;
//! - `load_face(data, index)`: a typeface from font file bytes (face `index` of a collection);
//! - `SYMBOL_FALLBACK`: the last-resort family for code points no other face covers.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(any(target_os = "macos", windows)))]
mod other;
#[cfg(not(any(target_os = "macos", windows)))]
pub use other::*;
