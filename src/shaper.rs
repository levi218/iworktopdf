//! Positions glyphs the way Apple's text engine does for Latin text: font advances plus pair
//! kerning. Substitutes (e.g. Nimbus Sans for Helvetica) carry the same advances as Apple's faces
//! or take Apple's harvested ones, and pair kerning comes from fonts/apple-metrics.json. The
//! document's own face (supplied, or installed on macOS) is shaped with its own kerning.

use crate::fonts;
use crate::platform;
use serde_json::Value;
use skia_safe::{
    font_style, Canvas, Font, FontMgr, FontStyle, Paint, Point, TextBlobBuilder, Typeface,
};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

/// Measurements harvested from Apple's exported PDFs, keyed by lower-cased PostScript name.
#[derive(Debug, Default)]
struct AppleTables {
    /// Pair kerning: "AV" → adjustment in 1/1000 em.
    kerning: HashMap<String, HashMap<String, i32>>,
    /// FontDescriptor ascent, descent, leading (and cap height) in 1/1000 em.
    metrics: HashMap<String, Vec<f32>>,
    /// Advance widths: character → 1/1000 em.
    widths: HashMap<String, HashMap<String, f32>>,
}

/// An object of objects from the JSON, with each inner value converted.
fn nested<T>(
    root: &Value,
    key: &str,
    convert: impl Fn(&Value) -> T,
) -> HashMap<String, HashMap<String, T>> {
    let Some(faces) = root[key].as_object() else {
        return HashMap::new();
    };
    faces
        .iter()
        .map(|(face, entries)| {
            let entries = entries.as_object().into_iter().flatten();
            let converted = entries
                .map(|(key, value)| (key.clone(), convert(value)))
                .collect();
            (face.to_lowercase(), converted)
        })
        .collect()
}

fn tables() -> &'static AppleTables {
    static TABLES: OnceLock<AppleTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let path = fonts::resource_dir().join("apple-metrics.json");
        let Some(root) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        else {
            log::warn!(
                "{} not found: no Apple widths, kerning or metrics",
                path.display()
            );
            return AppleTables::default();
        };
        let number = |value: &Value| value.as_f64().unwrap_or(0.0) as f32;
        let metrics = root["metrics"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(face, values)| {
                let values = values
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(number)
                    .collect();
                (face.to_lowercase(), values)
            });
        AppleTables {
            kerning: nested(&root, "kern", |value| value.as_i64().unwrap_or(0) as i32),
            metrics: metrics.collect(),
            widths: nested(&root, "widths", number),
        }
    })
}

/// True when apple-metrics.json has metrics, kerning or widths for a PostScript name.
pub fn knows(postscript_name: &str) -> bool {
    let key = postscript_name.to_lowercase();
    let tables = tables();
    tables.metrics.contains_key(&key)
        || tables.kerning.contains_key(&key)
        || tables.widths.contains_key(&key)
}

/// Apple FontDescriptor metrics (ascent, descent, leading in 1/1000 em) when known.
pub fn apple_metrics(postscript_name: &str) -> Option<&'static [f32]> {
    tables()
        .metrics
        .get(&postscript_name.to_lowercase())
        .map(Vec::as_slice)
}

/// A font tagged with the Apple PostScript name (for its kerning pairs and widths) and the
/// character tracking (em fraction added after every glyph).
#[derive(Debug, Clone)]
pub struct TaggedFont {
    /// The Skia font (typeface and size).
    pub font: Font,
    /// The document's PostScript name for the face (after bold/italic variant selection).
    pub name: String,
    /// Tracking as an em fraction added after every glyph.
    pub tracking: f32,
}

/// Faces drawn with a same-design clone (Helvetica → Nimbus Sans): the harvested pairs were
/// compensating another substitute's widths and only push the clone's glyphs apart.
fn own_kerning(postscript_name: &str) -> bool {
    matches!(
        postscript_name,
        "Helvetica" | "Helvetica-Bold" | "Helvetica-Oblique" | "Helvetica-BoldOblique"
    )
}

/// Harvested kerning pairs for a substitute. A real Apple font carries its own kerning, which pair
/// tables would double.
fn kerning_for(font: &TaggedFont) -> Option<&'static HashMap<String, i32>> {
    if fonts::is_exact(&font.name) || own_kerning(&font.name) {
        return None;
    }
    tables().kerning.get(&font.name.to_lowercase())
}

/// A bundled clone whose own widths, kerning and ligatures are the original's: Carlito for
/// Calibri ("Postscript" and the "ti" ligature in "adoption" match Apple's export to 0.1pt, where
/// harvested pairs without ligatures are 2pt off).
fn own_tables(font: &TaggedFont) -> bool {
    font.font.typeface().family_name() == "Carlito"
}

/// Faces whose substitute already has the same advance widths.
fn metric_compatible(postscript_name: &str) -> bool {
    let lower = postscript_name.to_lowercase();
    let linotype_neue = fonts::helvetica_neue_face() == "linotype"
        && lower.starts_with("helveticaneue")
        && lower != "helveticaneue-thin"
        && lower != "helveticaneue-medium";
    let same_widths = [
        "arial",
        "timesnewroman",
        "times-",
        "courier",
        "calibri",
        "verdana",
        "tahoma",
        "georgia",
    ];
    linotype_neue
        || own_kerning(postscript_name)
        || same_widths.iter().any(|prefix| lower.starts_with(prefix))
}

/// Apple's advance widths for fonts substituted by a face with different metrics.
fn apple_widths_for(font: &TaggedFont) -> Option<&'static HashMap<String, f32>> {
    // A same-width substitute only counts when one resolved: a face with none here falls back to
    // Helvetica and takes Apple's widths (Calibri on a Mac: titles 10% too wide otherwise).
    if fonts::is_exact(&font.name)
        || (metric_compatible(&font.name) && !fonts::is_unknown_family(&font.name))
    {
        return None;
    }
    tables()
        .widths
        .get(&font.name.to_lowercase())
        .filter(|widths| widths.len() >= 10)
}

/// Font file bytes and collection index of a typeface, for HarfBuzz.
type ShapeData = Option<Rc<(Vec<u8>, u32)>>;

thread_local! {
    static FALLBACK: RefCell<HashMap<(String, u32), Option<Typeface>>> =
        RefCell::new(HashMap::new());
    static SHAPE_DATA: RefCell<HashMap<u32, ShapeData>> = RefCell::new(HashMap::new());
}

/// A system typeface for a code point the primary face lacks (cached per family and code point).
fn fallback(primary: &Typeface, code_point: char) -> Option<Typeface> {
    let key = (primary.family_name(), u32::from(code_point));
    if let Some(cached) = FALLBACK.with(|cache| cache.borrow().get(&key).cloned()) {
        return cached;
    }
    let manager = FontMgr::new();
    let unichar = code_point as i32;
    let style = primary.font_style();
    let found = cascade_font(primary, code_point)
        .or_else(|| manager.match_family_style_character(&key.0, style, &[], unichar))
        .or_else(|| {
            manager.match_family_style_character(
                platform::SYMBOL_FALLBACK,
                FontStyle::normal(),
                &[],
                unichar,
            )
        });
    FALLBACK.with(|cache| cache.borrow_mut().insert(key, found.clone()));
    found
}

/// Apple's cascade for a character the styled face lacks (measured on Helvetica Neue and Gill
/// Sans text): Han ideographs in PingFang SC ("日本語" without kana), kana, CJK punctuation,
/// fullwidth forms and the Miscellaneous Symbols and Arrows block ("⬆") in Hiragino Sans. Used
/// when those faces are available.
fn cascade_font(primary: &Typeface, code_point: char) -> Option<Typeface> {
    let heavy = *primary.font_style().weight() >= 600;
    let typeface = match u32::from(code_point) {
        0x3400..=0x9FFF | 0xF900..=0xFAFF => {
            let weight = if heavy {
                font_style::Weight::SEMI_BOLD
            } else {
                font_style::Weight::NORMAL
            };
            let style = FontStyle::new(
                weight,
                font_style::Width::NORMAL,
                font_style::Slant::Upright,
            );
            FontMgr::new().match_family_style("PingFang SC", style)?
        }
        0x2B00..=0x2BFF | 0x3000..=0x30FF | 0xFF00..=0xFFEF => {
            let weight_face = if heavy {
                "HiraginoSans-W6"
            } else {
                "HiraginoSans-W3"
            };
            fonts::user(weight_face).or_else(|| fonts::user("HiraginoSans-W3"))?
        }
        _ => return None,
    };
    (typeface.unichar_to_glyph(code_point as i32) != 0).then_some(typeface)
}

/// PostScript names of the fallback faces a string's characters take (those its font lacks).
///
/// # Arguments
///
/// * `text` - The string
/// * `font` - The tagged font it is set in
///
/// # Returns
///
/// Each fallback face's PostScript name once, in first-use order.
pub fn fallback_faces(text: &str, font: &TaggedFont) -> Vec<String> {
    let primary = font.font.typeface();
    let mut names: Vec<String> = Vec::new();
    for code_point in visible(text).chars() {
        if code_point.is_whitespace() || font.font.unichar_to_glyph(code_point as i32) != 0 {
            continue;
        }
        let name = fallback(&primary, code_point).and_then(|face| face.post_script_name());
        if let Some(name) = name.filter(|name| !names.contains(name)) {
            names.push(name);
        }
    }
    names
}

fn is_variation_selector(character: char) -> bool {
    ('\u{FE00}'..='\u{FE0F}').contains(&character)
}

/// Variation selectors (U+FE00–FE0F, e.g. the emoji selector after "↩") are invisible.
fn visible(text: &str) -> Cow<'_, str> {
    if text.chars().any(is_variation_selector) {
        text.chars()
            .filter(|&character| !is_variation_selector(character))
            .collect::<String>()
            .into()
    } else {
        text.into()
    }
}

/// Positioned glyphs of one string.
#[derive(Debug, Clone)]
pub struct Shaped {
    /// Glyph ids, one per code point.
    pub glyphs: Vec<u16>,
    /// Each glyph's x offset from the string's origin.
    pub positions: Vec<f32>,
    /// Total advance.
    pub width: f32,
    /// Horizontal scale that fits the face's outlines to Apple's widths (substitutes only).
    pub scale_x: f32,
    /// Fallback face per glyph (`None`: the font's own face).
    pub faces: Vec<Option<Typeface>>,
}

/// Lays a string out: glyphs, positions and total advance.
///
/// # Arguments
///
/// * `text` - The string (variation selectors are dropped)
/// * `font` - The tagged font
///
/// # Returns
///
/// The positioned glyphs, with fallback faces for code points the font lacks.
pub fn layout(text: &str, font: &TaggedFont) -> Shaped {
    let text = visible(text);
    let code_points: Vec<char> = text.chars().collect();
    let mut glyphs: Vec<u16> = code_points
        .iter()
        .map(|&code_point| font.font.unichar_to_glyph(code_point as i32))
        .collect();
    let mut widths = vec![0f32; glyphs.len()];
    font.font.get_widths(&glyphs, &mut widths);
    let mut faces: Vec<Option<Typeface>> = vec![None; glyphs.len()];
    // Glyph fallback for code points the face does not cover.
    let primary = font.font.typeface();
    for (index, &code_point) in code_points.iter().enumerate() {
        if glyphs[index] != 0 || code_point.is_whitespace() {
            continue;
        }
        let Some(typeface) = fallback(&primary, code_point) else {
            continue;
        };
        let fallback_font = Font::from_typeface(&typeface, font.font.size());
        let glyph = fallback_font.unichar_to_glyph(code_point as i32);
        if glyph == 0 {
            continue;
        }
        glyphs[index] = glyph;
        fallback_font.get_widths(&[glyph], &mut widths[index..=index]);
        faces[index] = Some(typeface);
    }
    let size = font.font.size();
    let tracking = font.tracking * size;
    if (fonts::is_exact(&font.name) || own_tables(font)) && faces.iter().all(Option::is_none) {
        // The document's own font (or a clone with its tables): shape with HarfBuzz so its
        // kerning and ligatures apply. Tracked text is set without ligatures.
        let one_per_character = |shaped: &Shaped| shaped.glyphs.len() == glyphs.len();
        let ligatures = font.tracking == 0.0;
        if let Some(shaped) = shape_exact(&text, &font.font, ligatures)
            .filter(one_per_character)
            .or_else(|| shape_exact(&text, &font.font, false).filter(one_per_character))
        {
            let count = shaped.positions.len() as f32;
            let positions = shaped
                .positions
                .iter()
                .enumerate()
                .map(|(index, position)| position + tracking * index as f32)
                .collect();
            return Shaped {
                positions,
                width: shaped.width + tracking * count,
                faces,
                ..shaped
            };
        }
    }
    let mut scale_x = 1.0;
    if let Some(apple) = apple_widths_for(font) {
        let (mut ours, mut theirs) = (0f32, 0f32);
        let mut utf8 = [0u8; 4];
        for (index, code_point) in code_points
            .iter()
            .enumerate()
            .filter(|(index, _)| faces[*index].is_none())
        {
            if let Some(apple_width) = apple.get(&*code_point.encode_utf8(&mut utf8)) {
                ours += widths[index];
                widths[index] = apple_width * size / 1000.0;
                theirs += widths[index];
            }
        }
        if ours > 0.0 {
            scale_x = theirs / ours;
        }
    }
    let kerning = kerning_for(font);
    let mut positions = Vec::with_capacity(glyphs.len());
    let mut pen = 0f32;
    let mut pair = String::with_capacity(8);
    for (index, width) in widths.iter().enumerate() {
        if let (Some(previous), Some(pairs)) = (index.checked_sub(1), kerning) {
            pair.clear();
            pair.push(code_points[previous]);
            pair.push(code_points[index]);
            if let Some(adjustment) = pairs.get(&pair) {
                pen += *adjustment as f32 * size / 1000.0;
            }
        }
        positions.push(pen);
        pen += width + tracking;
    }
    Shaped {
        glyphs,
        positions,
        width: pen,
        scale_x,
        faces,
    }
}

/// HarfBuzz shaping with the face's own GPOS/kern/kerx tables (no fallback faces, no tracking).
///
/// # Arguments
///
/// * `text` - The run
/// * `font` - The document's own face at its size
/// * `ligatures` - Whether the face's ligatures may merge glyphs
///
/// # Returns
///
/// The shaped glyphs, or `None` when the face's data cannot be read.
fn shape_exact(text: &str, font: &Font, ligatures: bool) -> Option<Shaped> {
    let typeface = font.typeface();
    let data = SHAPE_DATA.with(|cache| {
        let mut cache = cache.borrow_mut();
        let entry = cache.entry(typeface.unique_id()).or_insert_with(|| {
            typeface
                .to_font_data()
                .map(|(bytes, index)| Rc::new((bytes, index as u32)))
        });
        entry.clone()
    })?;
    let face = rustybuzz::Face::from_slice(&data.0, data.1)?;
    let scale = font.size() / face.units_per_em() as f32;
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    let features: &[rustybuzz::Feature] = if ligatures {
        &[]
    } else {
        &[rustybuzz::Feature::new(
            rustybuzz::ttf_parser::Tag::from_bytes(b"liga"),
            0,
            ..,
        )]
    };
    let shaped = rustybuzz::shape(&face, features, buffer);
    // rustybuzz enables pair kerning only through a GPOS 'kern' feature, so an Apple face whose
    // pairs live in an AAT kerx table (Helvetica Light: "Te" -111, "Yo" -92) is kerned here.
    let kerx = face
        .tables()
        .gpos
        .is_none()
        .then(|| {
            face.raw_face()
                .table(rustybuzz::ttf_parser::Tag::from_bytes(b"kerx"))
        })
        .flatten();
    let pair_kerning =
        |left: u16, right: u16| kerx.map_or(0.0, |table| kerx_kerning(table, left, right) as f32);
    let infos = shaped.glyph_infos();
    // Callers place one glyph per character: a ligature takes its first character's slot and
    // the characters it absorbed get blank (space) glyphs at its end, as Apple's export does.
    let character_starts: Vec<usize> = text.char_indices().map(|(start, _)| start).collect();
    let blank = font.unichar_to_glyph(' ' as i32);
    let mut absorbed = Vec::with_capacity(infos.len());
    for (index, info) in infos.iter().enumerate() {
        let start = info.cluster as usize;
        let end = infos
            .get(index + 1)
            .map_or(text.len(), |next| next.cluster as usize);
        let characters = character_starts
            .iter()
            .filter(|&&offset| offset >= start && offset < end)
            .count();
        absorbed.push(characters.saturating_sub(1));
    }
    let (mut glyphs, mut positions, mut pen) = (
        Vec::with_capacity(character_starts.len()),
        Vec::with_capacity(character_starts.len()),
        0f32,
    );
    let mut previous = None;
    for ((info, position), &extra) in infos.iter().zip(shaped.glyph_positions()).zip(&absorbed) {
        let glyph = info.glyph_id as u16;
        if let Some(previous) = previous {
            pen += pair_kerning(previous, glyph) * scale;
        }
        previous = Some(glyph);
        glyphs.push(glyph);
        positions.push(pen + position.x_offset as f32 * scale);
        pen += position.x_advance as f32 * scale;
        if extra > 0 && blank == 0 {
            return None;
        }
        glyphs.extend(std::iter::repeat_n(blank, extra));
        positions.extend(std::iter::repeat_n(pen, extra));
    }
    Some(Shaped {
        glyphs,
        positions,
        width: pen,
        scale_x: 1.0,
        faces: Vec::new(),
    })
}

// ------------------------------------------------------------- AAT kerx pair kerning
//
// Apple's kerx table (https://developer.apple.com/fonts/TrueType-Reference-Manual/RM06/Chap6kerx.html):
// a header (version, padding, subtable count) and subtables of (length, coverage, tuple count,
// format data). Simple subtables are a sorted pair list (format 0) or a class array (format 2)
// whose class lookups give byte offsets into the array, as HarfBuzz reads them. ttf-parser reads
// format 2 class values as absolute offsets instead, which loses every class pair.

fn read_u16(data: &[u8], at: usize) -> Option<u16> {
    data.get(at..at + 2)
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Binary search: the index of the first of `count` sorted `size`-byte records whose key (read
/// by `key` from the record) is not below `target`.
fn first_not_below<K: Ord>(
    records: &[u8],
    size: usize,
    count: usize,
    target: K,
    key: impl Fn(&[u8]) -> Option<K>,
) -> usize {
    let (mut low, mut high) = (0, count);
    while low < high {
        let middle = (low + high) / 2;
        let below = records
            .get(middle * size..)
            .and_then(&key)
            .is_some_and(|found| found < target);
        if below {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

/// A glyph's 16-bit value in an AAT lookup table (formats 0, 2, 6 and 8).
fn aat_lookup(table: &[u8], glyph: u16) -> Option<u16> {
    let glyph_index = usize::from(glyph);
    match read_u16(table, 0)? {
        0 => read_u16(table, 2 + 2 * glyph_index),
        2 | 6 => {
            // Binary-search header: unit size and count, then the sorted units.
            let format = read_u16(table, 0)?;
            let (size, count) = (
                usize::from(read_u16(table, 2)?),
                usize::from(read_u16(table, 4)?),
            );
            let units = table.get(12..)?;
            // Segments (last, first, value) sort by their last glyph; singles (glyph, value) by
            // their glyph.
            let index = first_not_below(units, size, count, glyph, |unit| read_u16(unit, 0));
            let unit = units.get(index * size..(index + 1) * size)?;
            let (first, value_at) = if format == 2 {
                (read_u16(unit, 2)?, 4)
            } else {
                (read_u16(unit, 0)?, 2)
            };
            (first <= glyph && read_u16(unit, 0)? >= glyph)
                .then(|| read_u16(unit, value_at))
                .flatten()
        }
        8 => {
            let (first, count) = (read_u16(table, 2)?, read_u16(table, 4)?);
            let offset = glyph.checked_sub(first).filter(|&offset| offset < count)?;
            read_u16(table, 6 + 2 * usize::from(offset))
        }
        _ => None,
    }
}

/// A format 0 subtable's pair value: (left, right, value) records sorted by the glyph pair.
fn kerx_pair_list(subtable: &[u8], left: u16, right: u16) -> Option<i16> {
    let count = read_u32(subtable, 12)? as usize;
    let pairs = subtable.get(28..)?;
    let pair = (left, right);
    let pair_of = |record: &[u8]| Some((read_u16(record, 0)?, read_u16(record, 2)?));
    let index = first_not_below(pairs, 6, count.min(pairs.len() / 6), pair, pair_of);
    let record = pairs.get(index * 6..)?;
    (pair_of(record)? == pair).then(|| read_u16(record, 4).map(|value| value as i16))?
}

/// A format 2 subtable's class array value.
fn kerx_class_array(subtable: &[u8], left: u16, right: u16) -> Option<i16> {
    let class = |table_at: usize, glyph: u16| {
        let table = subtable.get(read_u32(subtable, table_at)? as usize..)?;
        aat_lookup(table, glyph)
    };
    let (left_class, right_class) = (class(16, left).unwrap_or(0), class(20, right).unwrap_or(0));
    let at = read_u32(subtable, 24)? as usize + usize::from(left_class) + usize::from(right_class);
    read_u16(subtable, at).map(|value| value as i16)
}

/// Pair kerning in font units from a kerx table's horizontal format 0 and 2 subtables.
///
/// # Arguments
///
/// * `kerx` - The raw kerx table
/// * `left` - The first glyph of the pair
/// * `right` - The second glyph
///
/// # Returns
///
/// The summed adjustment (0 when no subtable kerns the pair).
fn kerx_kerning(kerx: &[u8], left: u16, right: u16) -> i32 {
    // Vertical (bit 31), cross-stream (30) and variation (29) subtables leave the advance alone.
    const NOT_HORIZONTAL_ADVANCE: u32 = 0xE000_0000;
    let count = read_u32(kerx, 4).unwrap_or(0);
    let mut offset = 8;
    let mut total = 0;
    for _ in 0..count {
        let (Some(length), Some(coverage)) = (read_u32(kerx, offset), read_u32(kerx, offset + 4))
        else {
            break;
        };
        let Some(subtable) = kerx.get(offset..offset + length as usize) else {
            break;
        };
        if coverage & NOT_HORIZONTAL_ADVANCE == 0 {
            let value = match coverage & 0xFF {
                0 => kerx_pair_list(subtable, left, right),
                2 => kerx_class_array(subtable, left, right),
                _ => None,
            };
            total += i32::from(value.unwrap_or(0));
        }
        offset += length as usize;
    }
    total
}

/// The advance width of a string.
pub fn measure(text: &str, font: &TaggedFont) -> f32 {
    if text.is_empty() {
        0.0
    } else {
        layout(text, font).width
    }
}

/// Draws a string as one text blob (a positioned run per stretch of glyphs from one face).
///
/// # Arguments
///
/// * `canvas` - The canvas
/// * `text` - The string
/// * `origin` - The baseline origin
/// * `font` - The tagged font
/// * `paint` - Fill or stroke paint
pub fn draw(canvas: &Canvas, text: &str, origin: Point, font: &TaggedFont, paint: &Paint) {
    let text = visible(text);
    let shaped = layout(&text, font);
    if shaped.glyphs.is_empty() {
        return;
    }
    let face_id = |index: usize| shaped.faces[index].as_ref().map(Typeface::unique_id);
    let mut builder = TextBlobBuilder::new();
    // One positioned run per contiguous stretch of glyphs from the same face.
    let mut start = 0;
    while start < shaped.glyphs.len() {
        let end = (start + 1..shaped.glyphs.len())
            .find(|&index| face_id(index) != face_id(start))
            .unwrap_or(shaped.glyphs.len());
        let mut run_font = font.font.clone();
        let substituted_scale = (shaped.scale_x - 1.0).abs() > 0.01;
        match &shaped.faces[start] {
            Some(typeface) => {
                run_font.set_typeface(typeface);
                run_font.set_scale_x(1.0);
            }
            None => {
                run_font.set_scale_x(if substituted_scale {
                    shaped.scale_x
                } else {
                    1.0
                });
            }
        }
        let (run_glyphs, run_positions) =
            builder.alloc_run_pos_h(&run_font, end - start, 0.0, None);
        run_glyphs.copy_from_slice(&shaped.glyphs[start..end]);
        run_positions.copy_from_slice(&shaped.positions[start..end]);
        start = end;
    }
    if let Some(blob) = builder.make() {
        canvas.draw_text_blob(&blob, origin, paint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variation_selectors_are_invisible() {
        assert_eq!(visible("a\u{FE0F}b"), "ab");
        assert!(matches!(visible("plain"), Cow::Borrowed(_)));
    }

    #[test]
    fn helvetica_clones_keep_their_own_kerning() {
        assert!(own_kerning("Helvetica-Bold"));
        assert!(!own_kerning("HelveticaNeue"));
        assert!(metric_compatible("ArialMT"));
        assert!(metric_compatible("Times-Roman"));
        assert!(!metric_compatible("Cochin"));
    }

    #[test]
    fn layout_advances_add_up() {
        let typeface = fonts::get(Some("Helvetica"), false, false);
        let font = TaggedFont {
            font: Font::from_typeface(&typeface, 10.0),
            name: "Helvetica".into(),
            tracking: 0.1,
        };
        let shaped = layout("ab", &font);
        assert_eq!(shaped.glyphs.len(), 2);
        assert_eq!(shaped.positions[0], 0.0);
        assert!(shaped.width > shaped.positions[1]);
        assert!((measure("ab", &font) - shaped.width).abs() < 1e-4);
        assert_eq!(measure("", &font), 0.0);
    }

    #[test]
    fn cascade_matches_apples_cjk_and_arrow_faces() {
        // Needs the installed Hiragino Sans (macOS).
        if fonts::user("HiraginoSans-W3").is_none() {
            return;
        }
        let font = TaggedFont {
            font: Font::from_typeface(fonts::get(Some("GillSans"), false, false), 64.0),
            name: "GillSans".into(),
            tracking: 0.0,
        };
        assert_eq!(fallback_faces("Data Size ⬆", &font), ["HiraginoSans-W3"]);
        assert!(fallback_faces("Data Size", &font).is_empty());
        // Han ideographs take PingFang SC where the system has it; kana stay in Hiragino Sans.
        let han = fallback_faces("日本語", &font);
        assert!(han == ["PingFangSC-Regular"] || han == ["HiraginoSans-W3"]);
        assert_eq!(fallback_faces("かな", &font), ["HiraginoSans-W3"]);
    }

    #[test]
    fn calibri_takes_carlitos_ligatures_one_glyph_per_character() {
        let typeface = fonts::get(Some("Calibri"), false, false);
        // Needs the bundled Carlito (fonts/open); elsewhere Helvetica stands in.
        if typeface.family_name() != "Carlito" {
            return;
        }
        let font = TaggedFont {
            font: Font::from_typeface(&typeface, 84.0),
            name: "Calibri".into(),
            tracking: 0.0,
        };
        // Apple's export sets "adoption" with the "ti" ligature, 307.6pt wide at 84pt.
        let shaped = layout("adoption", &font);
        assert_eq!(shaped.glyphs.len(), 8);
        assert!((shaped.width - 307.66).abs() < 0.05);
        // The ligature's second character is a blank at the ligature's end.
        assert_eq!(shaped.glyphs[5], font.font.unichar_to_glyph(' ' as i32));
        assert_eq!(shaped.positions[5], shaped.positions[6]);
    }

    #[test]
    fn kerx_pairs_and_ligature_runs_are_kerned() {
        // Needs the installed Apple face (macOS); elsewhere a substitute is used instead.
        if !fonts::is_exact("Helvetica-Light") {
            return;
        }
        let typeface = fonts::get(Some("Helvetica-Light"), false, false);
        let font = TaggedFont {
            font: Font::from_typeface(&typeface, 46.0),
            name: "Helvetica-Light".into(),
            tracking: 0.0,
        };
        let unkerned = measure("T", &font) + measure("e", &font);
        // Apple's export kerns "Te" (pair list) by 111/1000 em and "Yo" (class array) by 92.
        assert!((unkerned - measure("Te", &font) - 0.111 * 46.0).abs() < 0.01);
        let unkerned = measure("Y", &font) + measure("o", &font);
        assert!((unkerned - measure("Yo", &font) - 0.092 * 46.0).abs() < 0.01);
        // The "fi" ligature still leaves one glyph per character, kerned.
        let shaped = layout("fiTe", &font);
        assert_eq!(shaped.glyphs.len(), 4);
        assert!((shaped.width - measure("fi", &font) - measure("Te", &font)).abs() < 0.01);
    }

    /// Big-endian bytes of 16-bit words.
    fn words(values: &[u16]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_be_bytes())
            .collect()
    }

    #[test]
    fn aat_lookups_find_glyph_values() {
        // Format 2: segments (last, first, value) for glyphs 5-9 -> 7, then the 0xFFFF end unit.
        let segments = words(&[2, 6, 2, 0, 0, 0, 9, 5, 7, 0xFFFF, 0xFFFF, 0]);
        assert_eq!(aat_lookup(&segments, 7), Some(7));
        assert_eq!(aat_lookup(&segments, 4), None);
        // Format 6: single glyphs (glyph, value).
        let singles = words(&[6, 4, 2, 0, 0, 0, 3, 30, 8, 80]);
        assert_eq!(aat_lookup(&singles, 8), Some(80));
        assert_eq!(aat_lookup(&singles, 5), None);
        // Format 8: a trimmed array from glyph 10.
        let trimmed = words(&[8, 10, 2, 100, 110]);
        assert_eq!(aat_lookup(&trimmed, 11), Some(110));
        assert_eq!(aat_lookup(&trimmed, 12), None);
    }

    #[test]
    fn kerx_reads_pair_lists_and_class_arrays() {
        // Format 0: two sorted pairs, (1, 2) -> -50 and (3, 4) -> 20.
        let mut pairs = 2u32.to_be_bytes().to_vec();
        pairs.extend([0; 12]); // binary-search header
        pairs.extend(words(&[1, 2, (-50i16) as u16, 3, 4, 20]));
        // Format 2: left class 4 (row offset) for glyph 1, right class 2 for glyph 9, array at 44.
        let mut classes = words(&[0, 20, 0, 28, 0, 36, 0, 44]);
        classes.extend(words(&[8, 1, 1, 4])); // left lookup at 28
        classes.extend(words(&[8, 9, 1, 2])); // right lookup at 36
        classes.extend(words(&[0, 0, 0, (-92i16) as u16])); // array: [row 0, row 1]
        let subtable = |format: u32, body: &[u8]| {
            let mut bytes = Vec::new();
            bytes.extend(((12 + body.len()) as u32).to_be_bytes());
            bytes.extend(format.to_be_bytes());
            bytes.extend(0u32.to_be_bytes());
            bytes.extend(body);
            bytes
        };
        let mut kerx = words(&[2, 0, 0, 2]);
        kerx.extend(subtable(0, &pairs));
        kerx.extend(subtable(2, &classes));
        assert_eq!(kerx_kerning(&kerx, 1, 2), -50);
        assert_eq!(kerx_kerning(&kerx, 3, 4), 20);
        assert_eq!(kerx_kerning(&kerx, 1, 9), -92);
        assert_eq!(kerx_kerning(&kerx, 2, 1), 0);
    }
}
