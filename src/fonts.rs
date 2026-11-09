//! Maps the PostScript font names stored in iWork documents to typefaces. Order: supplied fonts
//! (and, on macOS, the installed Apple faces), same-design faces installed under another name,
//! open-licence lookalikes bundled in fonts/open (labelled substitutes), then the closest
//! installed family. What differs per platform lives in platform/.

use crate::bytes::{be_u16, be_u32};
use crate::platform;
use rayon::prelude::*;
use skia_safe::{font_style, FontMgr, FontStyle, Typeface};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Licensed font files supplied by the user (--fonts), else IWORK2PDF_FONTS.
static USER_FONT_DIR: OnceLock<String> = OnceLock::new();

/// Sets the directory of supplied font files; only the first call before any lookup counts.
///
/// # Arguments
///
/// * `dir` - A directory searched recursively for .ttf/.otf/.ttc/.otc files
pub fn set_user_font_dir(dir: &str) {
    // `set` fails if a value is already present; the first directory wins, as documented.
    let _ = USER_FONT_DIR.set(dir.to_owned());
}

/// Where fonts/apple-metrics.json and fonts/open live: IWORK2PDF_RESOURCES, next to the
/// executable, else the C# project's copy in this repository.
pub fn resource_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("IWORK2PDF_RESOURCES") {
        return PathBuf::from(dir);
    }
    let executable = std::env::current_exe().ok();
    let beside_executable = executable.and_then(|exe| exe.parent().map(|dir| dir.join("fonts")));
    match beside_executable {
        Some(dir) if dir.join("apple-metrics.json").exists() => dir,
        _ => Path::new(env!("CARGO_MANIFEST_DIR")).join("../IWorkToPdf/fonts"),
    }
}

/// The Helvetica Neue stand-in (IWORK2PDF_HN): "linotype" (the licensed Helvetica Neue LT Pro
/// cut), or an open lookalike bundled in fonts/open: "roboto", "nimbus" (URW Nimbus Sans),
/// "inter", "public" (Public Sans); "auto" (default) is the platform's choice.
pub fn helvetica_neue_face() -> &'static str {
    static CHOICE: OnceLock<String> = OnceLock::new();
    CHOICE.get_or_init(|| {
        let requested = std::env::var("IWORK2PDF_HN")
            .unwrap_or_default()
            .to_lowercase();
        if requested.is_empty() || requested == "auto" {
            platform::helvetica_neue_auto().to_owned()
        } else {
            requested
        }
    })
}

// ------------------------------------------------------------------ sfnt helpers

/// True for a .ttc/.otc font collection.
pub fn is_collection(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && bytes.starts_with(b"ttcf")
}

/// Face `index` of a .ttc/.otc collection rebuilt as a standalone font.
///
/// # Arguments
///
/// * `bytes` - The collection file's bytes
/// * `index` - The face's index in the collection
///
/// # Returns
///
/// The standalone font file, or `None` when `bytes` is not a collection or is truncated.
pub fn collection_face(bytes: &[u8], index: usize) -> Option<Vec<u8>> {
    if !is_collection(bytes) || index >= be_u32(bytes, 8)? as usize {
        return None;
    }
    let directory = be_u32(bytes, 12 + 4 * index)? as usize;
    let table_count = usize::from(be_u16(bytes, directory + 4)?);
    let mut output = bytes
        .get(directory..directory + 12 + 16 * table_count)?
        .to_vec();
    for table in 0..table_count {
        let record = directory + 12 + 16 * table;
        let offset = be_u32(bytes, record + 8)? as usize;
        let length = be_u32(bytes, record + 12)? as usize;
        // Tables start on 4-byte boundaries.
        output.resize(output.len().next_multiple_of(4), 0);
        let new_offset = u32::try_from(output.len()).ok()?;
        output.extend_from_slice(bytes.get(offset..offset + length)?);
        let offset_field = 12 + 16 * table + 8;
        output[offset_field..offset_field + 4].copy_from_slice(&new_offset.to_be_bytes());
    }
    Some(output)
}

/// Reads `length` bytes at `offset` of an open file.
fn read_at(file: &mut std::fs::File, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = vec![0; length];
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

/// PostScript names (name ID 6) of every face in a font file with their collection indexes, read
/// with a few small reads instead of loading the whole file (unreadable files yield none).
fn postscript_names(path: &Path) -> Vec<(String, usize)> {
    let read = || -> Option<Vec<(String, usize)>> {
        let mut file = std::fs::File::open(path).ok()?;
        let header = read_at(&mut file, 0, 12).ok()?;
        let face_offsets: Vec<u64> = if is_collection(&header) {
            let face_count = be_u32(&header, 8)?.min(256) as usize;
            let offsets = read_at(&mut file, 12, 4 * face_count).ok()?;
            (0..face_count)
                .filter_map(|face| be_u32(&offsets, 4 * face).map(u64::from))
                .collect()
        } else {
            vec![0]
        };
        let mut names = Vec::with_capacity(face_offsets.len());
        for (face, offset) in face_offsets.into_iter().enumerate() {
            let directory = read_at(&mut file, offset, 12).ok()?;
            let table_count = usize::from(be_u16(&directory, 4)?);
            let records = read_at(&mut file, offset + 12, 16 * table_count).ok()?;
            let Some(record) = records
                .chunks_exact(16)
                .find(|record| record.starts_with(b"name"))
            else {
                continue;
            };
            let table_offset = u64::from(be_u32(record, 8)?);
            let table = read_at(&mut file, table_offset, be_u32(record, 12)? as usize).ok()?;
            if let Some(name) = postscript_name_record(&table) {
                names.push((name, face));
            }
        }
        Some(names)
    };
    read().unwrap_or_default()
}

/// The first name-ID-6 record of a 'name' table (UTF-16BE for Unicode/Windows platforms,
/// otherwise Mac Roman read as Latin-1).
fn postscript_name_record(table: &[u8]) -> Option<String> {
    let count = usize::from(be_u16(table, 2)?);
    let strings = usize::from(be_u16(table, 4)?);
    (0..count).find_map(|record| {
        let at = 6 + record * 12;
        let (platform_id, name_id) = (be_u16(table, at)?, be_u16(table, at + 6)?);
        let length = usize::from(be_u16(table, at + 8)?);
        let offset = usize::from(be_u16(table, at + 10)?);
        let start = strings + offset;
        let raw = table.get(start..start + length).filter(|_| name_id == 6)?;
        Some(if platform_id == 0 || platform_id == 3 {
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            raw.iter().map(|&byte| char::from(byte)).collect()
        })
    })
}

// ------------------------------------------------------------------ indexes

/// A font file and the index of a face within it.
type FaceLocation = (PathBuf, usize);

/// Supplied fonts (and, where the platform's fonts are Apple's own, the installed ones) by
/// lower-cased PostScript name. Supplied fonts come first.
fn exact_index() -> &'static HashMap<String, FaceLocation> {
    static INDEX: OnceLock<HashMap<String, FaceLocation>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut index = HashMap::new();
        let user = USER_FONT_DIR
            .get()
            .cloned()
            .or_else(|| std::env::var("IWORK2PDF_FONTS").ok());
        let mut dirs: Vec<PathBuf> = user
            .into_iter()
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .collect();
        if platform::NATIVE_APPLE_FONTS {
            dirs.extend(platform::font_dirs());
        }
        for dir in dirs {
            let mut files = Vec::new();
            collect_font_files(&dir, &mut files);
            files.sort();
            // Name tables are read in parallel (hundreds of system fonts on macOS); insertion stays
            // in sorted file order so the first file claiming a name wins deterministically.
            let named: Vec<(PathBuf, Vec<(String, usize)>)> = files
                .into_par_iter()
                .map(|file| {
                    let names = postscript_names(&file);
                    (file, names)
                })
                .collect();
            for (file, names) in named {
                for (name, face) in names {
                    add_with_aliases(&mut index, &name, (file.clone(), face));
                }
            }
        }
        index
    })
}

/// Indexes a face under its name and aliases: trial/demo builds ("GraphikTrial-Regular") and the
/// regular face's name without its style suffix ("HelveticaNeue-Roman" is Apple's "HelveticaNeue").
fn add_with_aliases(index: &mut HashMap<String, FaceLocation>, name: &str, location: FaceLocation) {
    let alias = name.replace("Trial", "").replace("Demo", "");
    let regular = ["-Roman", "-Regular", "-Book"]
        .iter()
        .find_map(|suffix| alias.strip_suffix(suffix));
    for key in [Some(name), Some(alias.as_str()), regular]
        .into_iter()
        .flatten()
    {
        index
            .entry(key.to_lowercase())
            .or_insert_with(|| location.clone());
    }
}

/// Font files (.ttf, .otf, .ttc, .otc) under `dir`, recursively.
fn collect_font_files(dir: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_font_files(&path, output);
            continue;
        }
        let extension = path
            .extension()
            .map(|ext| ext.to_string_lossy().to_lowercase());
        if matches!(extension.as_deref(), Some("ttf" | "otf" | "ttc" | "otc")) {
            output.push(path);
        }
    }
}

thread_local! {
    static FILE_FACES: RefCell<HashMap<FaceLocation, Option<Typeface>>> =
        RefCell::new(HashMap::new());
    static RESOLVED: RefCell<HashMap<String, Typeface>> = RefCell::new(HashMap::new());
}

/// A typeface from a font file (cached per file and face).
fn load_file(path: &Path, index: usize) -> Option<Typeface> {
    FILE_FACES.with(|cache| {
        let mut cache = cache.borrow_mut();
        let entry = cache.entry((path.to_path_buf(), index)).or_insert_with(|| {
            let bytes = std::fs::read(path).ok()?;
            platform::load_face(&bytes, index)
        });
        entry.clone()
    })
}

/// Every exact (supplied or native Apple) PostScript name found, sorted.
pub fn user_font_names() -> Vec<String> {
    let mut names: Vec<String> = exact_index()
        .values()
        .filter_map(|(path, face)| {
            postscript_names(path)
                .into_iter()
                .find(|entry| entry.1 == *face)
        })
        .map(|(name, _)| name)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// True when the document's own face is available (no substitution happening).
pub fn is_exact(postscript_name: &str) -> bool {
    exact_index().contains_key(&postscript_name.to_lowercase())
}

/// A supplied (or native Apple) font by exact PostScript name, if any.
pub fn user(postscript_name: &str) -> Option<Typeface> {
    let (path, face) = exact_index().get(&postscript_name.to_lowercase())?;
    load_file(path, *face)
}

/// The typeface a document font name resolves to (cached per name and style).
///
/// # Arguments
///
/// * `postscript_name` - The name stored in the document (`None`: Helvetica Neue)
/// * `bold` - The style asks for bold
/// * `italic` - The style asks for italic
///
/// # Returns
///
/// The exact face when available, else a same-design face, an open lookalike or the closest
/// installed family.
pub fn get(postscript_name: Option<&str>, bold: bool, italic: bool) -> Typeface {
    let name = postscript_name.unwrap_or("HelveticaNeue");
    let key = format!("{}|{bold}|{italic}", name.to_lowercase());
    if let Some(cached) = RESOLVED.with(|cache| cache.borrow().get(&key).cloned()) {
        return cached;
    }
    let typeface = resolve(name, bold, italic);
    RESOLVED.with(|cache| cache.borrow_mut().insert(key, typeface.clone()));
    typeface
}

/// The family part of a PostScript name ("Helvetica-Bold" → "Helvetica").
pub fn base_name(postscript_name: &str) -> &str {
    match postscript_name.find('-') {
        Some(dash) if dash > 0 => &postscript_name[..dash],
        _ => postscript_name,
    }
}

fn contains_ignore_case(text: &str, part: &str) -> bool {
    text.to_lowercase().contains(&part.to_lowercase())
}

/// Whether a face name itself says bold, or italic.
fn named_weight_and_slant(postscript_name: &str) -> (bool, bool) {
    let bold_words = ["Bold", "Semibold", "Heavy", "Black"];
    let bold = bold_words
        .iter()
        .any(|word| contains_ignore_case(postscript_name, word))
        || ["-Bd", "-W6", "-W7", "-W8"]
            .iter()
            .any(|suffix| postscript_name.ends_with(suffix));
    let italic = contains_ignore_case(postscript_name, "Italic")
        || contains_ignore_case(postscript_name, "Oblique");
    (bold, italic)
}

fn resolve(postscript_name: &str, bold: bool, italic: bool) -> Typeface {
    // A bold/italic flag on a regular face name selects the family's bold/italic face.
    let has_style = postscript_name.contains('-')
        && !postscript_name.ends_with("-Roman")
        && !postscript_name.ends_with("-Regular");
    if !has_style && (bold || italic) {
        let suffix = match (bold, italic) {
            (true, true) => "BoldItalic",
            (true, false) => "Bold",
            _ => "Italic",
        };
        if let Some(styled) = user(&format!("{}-{suffix}", base_name(postscript_name))) {
            return styled;
        }
    }
    if let Some(exact) = user(postscript_name) {
        return exact;
    }
    let (named_bold, named_italic) = named_weight_and_slant(postscript_name);
    let (bold, italic) = (bold || named_bold, italic || named_italic);
    let bold_name = format!("{}-Bold", base_name(postscript_name));
    let use_bold_file = bold && !named_bold && !platform::installed_files(&bold_name).is_empty();
    let file_name = if use_bold_file {
        bold_name.as_str()
    } else {
        postscript_name
    };
    if !italic {
        let candidates = platform::installed_files(file_name)
            .iter()
            .flat_map(|file| {
                platform::font_dirs()
                    .into_iter()
                    .map(move |dir| dir.join(file))
            });
        if let Some(installed) = candidates
            .filter(|path| path.exists())
            .find_map(|path| load_file(&path, 0))
        {
            return installed;
        }
    }
    if let Some(lookalike) = open_lookalike(postscript_name, bold, italic) {
        return lookalike;
    }
    let family = platform::system_family(base_name(postscript_name));
    // A family not known here. Apple sets fonts missing from the Mac in Helvetica (SkolarSans,
    // Lato, Karla, Ubuntu documents export so); Arial has Helvetica's widths. The real Helvetica,
    // when available, is what Apple itself falls back to.
    if family.is_none() {
        let helvetica = match (bold, italic) {
            (true, true) => "Helvetica-BoldOblique",
            (true, false) => "Helvetica-Bold",
            (false, true) => "Helvetica-Oblique",
            (false, false) => "Helvetica",
        };
        if let Some(found) = user(helvetica) {
            return found;
        }
    }
    let style = font_style_for(postscript_name, bold, italic);
    let manager = FontMgr::new();
    manager
        .match_family_style(family.unwrap_or("Arial"), style)
        .or_else(|| manager.match_family_style("Arial", style))
        .or_else(|| manager.legacy_make_typeface(None::<&str>, style))
        .expect("the platform font manager has at least one typeface")
}

/// The weight, width and slant to ask the font manager for.
fn font_style_for(postscript_name: &str, bold: bool, italic: bool) -> FontStyle {
    // Hiragino's W-numbers: W3 regular, W6 semibold (Yu Gothic Medium is the closer stroke
    // weight).
    let weight = if postscript_name.to_lowercase().starts_with("hira") {
        if postscript_name.ends_with("-W6") || postscript_name.ends_with("-W5") {
            font_style::Weight::MEDIUM
        } else if postscript_name.ends_with("-W7") || postscript_name.ends_with("-W8") {
            font_style::Weight::BOLD
        } else {
            font_style::Weight::NORMAL
        }
    } else if bold {
        font_style::Weight::BOLD
    } else {
        font_style::Weight::NORMAL
    };
    let condensed = base_name(postscript_name) == "DINCondensed";
    let width = if condensed {
        font_style::Width::CONDENSED
    } else {
        font_style::Width::NORMAL
    };
    let slant = if italic {
        font_style::Slant::Italic
    } else {
        font_style::Slant::Upright
    };
    FontStyle::new(weight, width, slant)
}

/// True for a face with no supplied, installed or same-design copy here and no known family: one
/// Apple would also be missing and set in Helvetica.
pub fn is_unknown_family(postscript_name: &str) -> bool {
    let base = base_name(postscript_name);
    !is_exact(postscript_name)
        && platform::installed_files(postscript_name).is_empty()
        && lookalike_files(postscript_name).is_none()
        && lookalike_files(base).is_none()
        && platform::system_family(base).is_none()
}

/// The four faces of a bundled lookalike, as paths under fonts/open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FaceFiles {
    regular: &'static str,
    bold: &'static str,
    italic: &'static str,
    bold_italic: &'static str,
}

impl FaceFiles {
    const fn new(
        regular: &'static str,
        bold: &'static str,
        italic: &'static str,
        bold_italic: &'static str,
    ) -> FaceFiles {
        FaceFiles {
            regular,
            bold,
            italic,
            bold_italic,
        }
    }

    const fn single(file: &'static str) -> FaceFiles {
        FaceFiles::new(file, file, file, file)
    }

    fn pick(&self, bold: bool, italic: bool) -> &'static str {
        match (bold, italic) {
            (false, false) => self.regular,
            (true, false) => self.bold,
            (false, true) => self.italic,
            (true, true) => self.bold_italic,
        }
    }
}

/// A lookalike family's four faces in one directory of fonts/open, joined at compile time
/// (`concat!` only accepts literals, hence a macro rather than a function).
macro_rules! family {
    ($dir:literal, $regular:literal, $bold:literal, $italic:literal, $bold_italic:literal) => {
        FaceFiles::new(
            concat!($dir, "/", $regular),
            concat!($dir, "/", $bold),
            concat!($dir, "/", $italic),
            concat!($dir, "/", $bold_italic),
        )
    };
}

const NIMBUS_SANS: FaceFiles = FaceFiles::new(
    "nimbus-sans/NimbusSans-Regular.otf",
    "nimbus-sans/NimbusSans-Bold.otf",
    "nimbus-sans/NimbusSans-Italic.otf",
    "nimbus-sans/NimbusSans-BoldItalic.otf",
);
const PUBLIC_SANS: FaceFiles = FaceFiles::new(
    "public-sans/PublicSans-Regular.ttf",
    "public-sans/PublicSans-Bold.ttf",
    "public-sans/PublicSans-Regular.ttf",
    "public-sans/PublicSans-Bold.ttf",
);
const PUBLIC_SANS_MEDIUM: FaceFiles = FaceFiles::new(
    "public-sans/PublicSans-Medium.ttf",
    "public-sans/PublicSans-Bold.ttf",
    "public-sans/PublicSans-Medium.ttf",
    "public-sans/PublicSans-Bold.ttf",
);

/// Open-licence faces bundled in fonts/open (licence files next to them), used as labelled
/// substitutes for Apple-only families when no exact copy exists. Keyed by PostScript name or
/// family base name.
fn lookalike_files(key: &str) -> Option<FaceFiles> {
    let lower = key.to_lowercase();
    Some(match lower.as_str() {
        // URW Nimbus Sans (AGPL with a font-embedding exception): URW's licensed Helvetica clone,
        // identical advance widths.
        "helvetica" => NIMBUS_SANS,
        "helvetica-narrow" => family!(
            "nimbus-sans",
            "NimbusSansNarrow-Regular.otf",
            "NimbusSansNarrow-Bold.otf",
            "NimbusSansNarrow-Oblique.otf",
            "NimbusSansNarrow-BoldOblique.otf"
        ),
        // TeX Gyre Termes (GUST Font License): URW's Nimbus Roman clone of Times, same widths.
        "times" => FaceFiles::new(
            "tex-gyre/texgyretermes-regular.otf",
            "tex-gyre/texgyretermes-bold.otf",
            "tex-gyre/texgyretermes-italic.otf",
            "tex-gyre/texgyretermes-bolditalic.otf",
        ),
        // Carlito (OFL): tyPoland's metric-compatible Calibri clone, identical advance widths.
        "calibri" => family!(
            "carlito",
            "Carlito-Regular.ttf",
            "Carlito-Bold.ttf",
            "Carlito-Italic.ttf",
            "Carlito-BoldItalic.ttf"
        ),
        // Commercial Type's Graphik: Public Sans (OFL), the open grotesque nearest in proportions.
        "graphik" => PUBLIC_SANS,
        "graphik-medium" => PUBLIC_SANS_MEDIUM,
        "graphik-semibold" => FaceFiles::single("public-sans/PublicSans-SemiBold.ttf"),
        // Produkt (Graphik's slab companion): Roboto Slab (Apache 2.0) ExtraLight, total width
        // within 0.5%.
        "produkt-extralight" => FaceFiles::single("roboto-slab/RobotoSlab-ExtraLight.ttf"),
        _ => return helvetica_neue_lookalike(&lower),
    })
}

/// Helvetica Neue lookalikes: (IWORK2PDF_HN choice, lower-cased face name) → files.
const HELVETICA_NEUE_LOOKALIKES: [(&str, &str, FaceFiles); 14] = [
    ("nimbus", "helveticaneue", NIMBUS_SANS),
    (
        "nimbus",
        "helveticaneue-condensedbold",
        family!(
            "nimbus-sans",
            "NimbusSansNarrow-Bold.otf",
            "NimbusSansNarrow-Bold.otf",
            "NimbusSansNarrow-BoldOblique.otf",
            "NimbusSansNarrow-BoldOblique.otf"
        ),
    ),
    (
        "inter",
        "helveticaneue",
        family!(
            "inter",
            "Inter-Regular.ttf",
            "Inter-Bold.ttf",
            "Inter-Italic.ttf",
            "Inter-BoldItalic.ttf"
        ),
    ),
    (
        "inter",
        "helveticaneue-light",
        family!(
            "inter",
            "Inter-Light.ttf",
            "Inter-Bold.ttf",
            "Inter-Italic.ttf",
            "Inter-BoldItalic.ttf"
        ),
    ),
    (
        "inter",
        "helveticaneue-medium",
        family!(
            "inter",
            "Inter-Medium.ttf",
            "Inter-Bold.ttf",
            "Inter-Italic.ttf",
            "Inter-BoldItalic.ttf"
        ),
    ),
    (
        "inter",
        "helveticaneue-thin",
        family!(
            "inter",
            "Inter-Thin.ttf",
            "Inter-Bold.ttf",
            "Inter-Italic.ttf",
            "Inter-BoldItalic.ttf"
        ),
    ),
    (
        "inter",
        "helveticaneue-ultralight",
        family!(
            "inter",
            "Inter-Thin.ttf",
            "Inter-Bold.ttf",
            "Inter-Italic.ttf",
            "Inter-BoldItalic.ttf"
        ),
    ),
    (
        "roboto",
        "helveticaneue",
        family!(
            "roboto",
            "Roboto-Regular.ttf",
            "Roboto-Bold.ttf",
            "Roboto-Italic.ttf",
            "Roboto-BoldItalic.ttf"
        ),
    ),
    (
        "roboto",
        "helveticaneue-light",
        family!(
            "roboto",
            "Roboto-Light.ttf",
            "Roboto-Bold.ttf",
            "Roboto-Italic.ttf",
            "Roboto-BoldItalic.ttf"
        ),
    ),
    (
        "roboto",
        "helveticaneue-medium",
        family!(
            "roboto",
            "Roboto-Medium.ttf",
            "Roboto-Bold.ttf",
            "Roboto-Italic.ttf",
            "Roboto-BoldItalic.ttf"
        ),
    ),
    (
        "roboto",
        "helveticaneue-thin",
        family!(
            "roboto",
            "Roboto-Thin.ttf",
            "Roboto-Bold.ttf",
            "Roboto-Italic.ttf",
            "Roboto-BoldItalic.ttf"
        ),
    ),
    (
        "roboto",
        "helveticaneue-ultralight",
        family!(
            "roboto",
            "Roboto-Thin.ttf",
            "Roboto-Bold.ttf",
            "Roboto-Italic.ttf",
            "Roboto-BoldItalic.ttf"
        ),
    ),
    ("public", "helveticaneue", PUBLIC_SANS),
    ("public", "helveticaneue-medium", PUBLIC_SANS_MEDIUM),
];

/// Open lookalike files for Helvetica Neue by weight, per IWORK2PDF_HN.
fn helvetica_neue_lookalike(lower_name: &str) -> Option<FaceFiles> {
    let choice = helvetica_neue_face();
    HELVETICA_NEUE_LOOKALIKES
        .iter()
        .find(|(stand_in, name, _)| *stand_in == choice && *name == lower_name)
        .map(|(_, _, files)| *files)
}

/// The bundled lookalike face for a name, if any and if enabled.
fn open_lookalike(postscript_name: &str, bold: bool, italic: bool) -> Option<Typeface> {
    let files =
        lookalike_files(postscript_name).or_else(|| lookalike_files(base_name(postscript_name)))?;
    // IWORK2PDF_LOOKALIKES: unset or "all" = all, "none" = none, else a comma list of family base
    // names (for paired before/after measurements with one build).
    if let Ok(enabled) = std::env::var("IWORK2PDF_LOOKALIKES") {
        let listed = enabled.split(',').any(|family| {
            family
                .trim()
                .eq_ignore_ascii_case(base_name(postscript_name))
        });
        if !enabled.is_empty() && !enabled.eq_ignore_ascii_case("all") && !listed {
            return None;
        }
    }
    let path = resource_dir().join("open").join(files.pick(bold, italic));
    path.exists().then(|| load_file(&path, 0)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal sfnt with one 'name' table holding one PostScript name record.
    fn sfnt(name: &str, unicode: bool) -> Vec<u8> {
        let text: Vec<u8> = if unicode {
            name.encode_utf16().flat_map(u16::to_be_bytes).collect()
        } else {
            name.bytes().collect()
        };
        let mut table = vec![0, 0, 0, 1, 0, 18];
        let platform_id: u16 = if unicode { 3 } else { 1 };
        for value in [platform_id, 1, 0x409, 6, text.len() as u16, 0] {
            table.extend(value.to_be_bytes());
        }
        table.extend(&text);
        let mut file = vec![0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        file.extend(b"name");
        file.extend([0u8; 4]);
        file.extend(28u32.to_be_bytes());
        file.extend((table.len() as u32).to_be_bytes());
        file.extend(table);
        file
    }

    #[test]
    fn reads_postscript_names() {
        let dir = std::env::temp_dir().join(format!("iwork2pdf-fonts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir is writable");
        let path = dir.join("a.ttf");
        std::fs::write(&path, sfnt("Foo-Bold", true)).expect("temp file is writable");
        assert_eq!(postscript_names(&path), vec![("Foo-Bold".to_string(), 0)]);
        std::fs::write(&path, sfnt("Mac-Roman", false)).expect("temp file is writable");
        assert_eq!(postscript_names(&path), vec![("Mac-Roman".to_string(), 0)]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn collection_face_rebuilds_offsets() {
        let face = sfnt("Inner", true);
        let mut collection = b"ttcf".to_vec();
        collection.extend([0, 1, 0, 0]);
        collection.extend(1u32.to_be_bytes());
        collection.extend(16u32.to_be_bytes());
        // The face starts 16 bytes in, so its table offset moves by 16.
        let mut shifted = face.clone();
        let table_offset = be_u32(&face, 20).expect("offset field") + 16;
        shifted[20..24].copy_from_slice(&table_offset.to_be_bytes());
        collection.extend(shifted);
        assert!(is_collection(&collection));
        let standalone = collection_face(&collection, 0).expect("face 0 exists");
        assert_eq!(
            postscript_name_record(&standalone[28..]).as_deref(),
            Some("Inner")
        );
        assert!(collection_face(&collection, 1).is_none());
        assert!(collection_face(&face, 0).is_none());
    }

    #[test]
    fn aliases_cover_regular_suffixes() {
        let mut index = HashMap::new();
        add_with_aliases(
            &mut index,
            "GraphikTrial-Regular",
            (PathBuf::from("g.otf"), 0),
        );
        assert!(index.contains_key("graphiktrial-regular"));
        assert!(index.contains_key("graphik-regular"));
        assert!(index.contains_key("graphik"));
    }

    #[test]
    fn base_names_and_lookalikes() {
        assert_eq!(base_name("Helvetica-Bold"), "Helvetica");
        assert_eq!(base_name("-Odd"), "-Odd");
        assert_eq!(
            lookalike_files("Times").map(|files| files.pick(true, false)),
            Some("tex-gyre/texgyretermes-bold.otf")
        );
        assert!(lookalike_files("NoSuchFamily").is_none());
        assert_eq!(named_weight_and_slant("HiraginoSans-W6"), (true, false));
        assert_eq!(named_weight_and_slant("Helvetica-Oblique"), (false, true));
    }
}
