//! Windows: Apple's faces are not installed, so each is drawn with the same design from another
//! vendor where Windows or Office ship one, else the closest Windows face (positioned with Apple's
//! harvested advance widths, see shaper.rs).

use skia_safe::{FontMgr, Typeface};
use std::path::PathBuf;

/// Whether installed fonts are Apple's own builds (used as exact fonts by PostScript name).
pub const NATIVE_APPLE_FONTS: bool = false;
/// The last-resort family for code points no other face covers.
pub const SYMBOL_FALLBACK: &str = "Segoe UI Symbol";

/// Where installed fonts live.
pub fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(windows_dir) = std::env::var_os("WINDIR") {
        dirs.push(PathBuf::from(windows_dir).join("Fonts"));
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(
            PathBuf::from(local_app_data)
                .join("Microsoft")
                .join("Windows")
                .join("Fonts"),
        );
    }
    dirs
}

/// The closest installed family for an Apple face that is not installed.
pub fn system_family(base: &str) -> Option<&'static str> {
    Some(match base {
        "HelveticaNeue" | "Helvetica" | "ArialMT" | "Arial" | "ArialUnicodeMS" | "SFPro"
        | ".SFNS" => "Arial",
        "Symbol" => "Symbol",
        // Same designs from the same foundries, shipped with Windows:
        "Palatino" => "Palatino Linotype",
        "LucidaGrande" => "Lucida Sans Unicode",
        "TimesNewRomanPSMT"
        | "TimesNewRomanPS"
        | "Times"
        | "TimesNewRoman"
        | "Cochin"
        | "Baskerville"
        | "AcademyEngravedLetPlain" => "Times New Roman",
        "Menlo" | "Courier" | "CourierNewPSMT" | "CourierNewPS" | "Monaco" => "Courier New",
        "Verdana" => "Verdana",
        "Tahoma" => "Tahoma",
        "TrebuchetMS" => "Trebuchet MS",
        "Calibri" => "Calibri",
        "Cambria" => "Cambria",
        "Georgia" => "Georgia",
        "Impact" => "Impact",
        "ComicSansMS" => "Comic Sans MS",
        "MicrosoftSansSerif" => "Microsoft Sans Serif",
        "Rockwell" => "Rockwell",
        "BrushScriptMT" => "Brush Script MT",
        "Papyrus" => "Papyrus",
        "BradleyHandITCTT" => "Bradley Hand ITC",
        // Styles not in installed_files:
        "ArialNarrow" | "ArialRoundedMTBold" | "Copperplate" => "Arial",
        "GillSans" => "Gill Sans MT",
        // Formal scripts shipped with Office, nearest in design (substitutes):
        "SnellRoundhand" => "Edwardian Script ITC",
        "Apple-Chancery" | "AppleChancery" => "Monotype Corsiva",
        // Substitute: Bahnschrift is a DIN 1451 design (condensed widths for DIN Condensed).
        "DINCondensed" | "DINAlternate" => "Bahnschrift",
        "AvenirNext" | "Avenir" => "Avenir Next LT Com",
        "HiraKakuProN" | "HiraKakuPro" | "HiraginoSans" | "HiraMinProN" | "PingFangSC"
        | "PingFangTC" | "PingFangHK" => "Yu Gothic",
        "Graphik" | "SFProText" | "SFProDisplay" | ".SFNSText" | ".SFNSDisplay" => "Arial",
        _ => return None,
    })
}

/// Same designs shipped with Windows/Office that DirectWrite files under a parent family
/// ("Arial", "Copperplate Gothic"), plus licensed installed cuts, opened by file name.
pub fn installed_files(ps: &str) -> &'static [&'static str] {
    let linotype = crate::fonts::helvetica_neue_face() == "linotype";
    match ps.to_ascii_lowercase().as_str() {
        "avenirnext-regular" => &["AvenirNextLTCom-Regular.otf"],
        "avenirnext-bold" => &["AvenirNextLTCom-Bold.otf"],
        "arial-black" => &["ariblk.ttf"],
        "arialnarrow" => &["ARIALN.TTF"],
        "arialnarrow-bold" => &["ARIALNB.TTF"],
        "arialroundedmtbold" => &["ARLRDBD.TTF"],
        "copperplate" | "copperplate-light" => &["COPRGTL.TTF"],
        "copperplate-bold" => &["COPRGTB.TTF"],
        // Menlo is Apple's derivative of Bitstream Vera / DejaVu Sans Mono (same 0.602em advance).
        "menlo" | "menlo-regular" => &["DejaVuSansMono.ttf"],
        "menlo-bold" => &["DejaVuSansMono-Bold.ttf"],
        // Linotype's Helvetica Neue LT Pro: Apple's design and advance widths (licensed copy).
        "helveticaneue" if linotype => &["HelveticaNeueLTPro-Roman.otf"],
        "helveticaneue-bold" if linotype => &["HelveticaNeueLTPro-Bd.otf"],
        "helveticaneue-light" | "helvetica-light" if linotype => &["HelveticaNeueLTPro-Lt.otf"],
        "helveticaneue-ultralight" | "helveticaneue-thin" if linotype => {
            &["HelveticaNeueLTPro-UltLt.otf"]
        }
        "helveticaneue-condensedbold" if linotype => &["HelveticaNeueLTPro-BdCn.otf"],
        "helveticaneue-medium" if linotype => {
            &["HelveticaNeueLTPro-Md.otf", "HelveticaNeueLTPro-Roman.otf"]
        }
        _ => &[],
    }
}

/// Measured on the 607-file quick set: Linotype 408 passes, Roboto 404, Public Sans 396, Nimbus
/// Sans 395, Inter 394. The licensed Linotype cut where installed, else Roboto.
pub fn helvetica_neue_auto() -> &'static str {
    if font_dirs()
        .iter()
        .any(|dirs| dirs.join("HelveticaNeueLTPro-Roman.otf").exists())
    {
        "linotype"
    } else {
        "roboto"
    }
}

/// A typeface from font file bytes (face `index` of a collection).
pub fn load_face(data: &[u8], index: usize) -> Option<Typeface> {
    FontMgr::new().new_from_data(data, Some(index))
}
