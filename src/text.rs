//! Resolved text styles, Apple's line metrics, and single-line drawing.

use crate::fonts;
use crate::iwa::Proto;
use crate::shaper::{self, apple_metrics, TaggedFont};
use crate::styles::{self, ctx, Stroke, Styles};
use skia_safe::{
    font::Edging as FontEdging, BlurStyle, Canvas, Color, Font, FontHinting, MaskFilter,
    PaintStyle, Point,
};
use std::borrow::Cow;
use std::rc::Rc;

/// Paragraph alignment (TSWP ParagraphStylePropertiesArchive.alignment).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HAlign {
    /// Flush left.
    Left = 0,
    /// Flush right.
    Right = 1,
    /// Centred.
    Center = 2,
    /// Justified.
    Justify = 3,
    /// By writing direction (left for left-to-right text).
    Natural = 4,
}

impl HAlign {
    /// The alignment for a stored value (unknown values are left).
    pub fn from_stored(value: i32) -> HAlign {
        match value {
            1 => HAlign::Right,
            2 => HAlign::Center,
            3 => HAlign::Justify,
            4 => HAlign::Natural,
            _ => HAlign::Left,
        }
    }
}

/// A tab stop.
#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    /// Position from the text's left edge.
    pub position: f32,
    /// 0 left, 1 centre, 2 right, 3 decimal.
    pub align: i32,
    /// Leader characters repeated up to the stop.
    pub leader: Option<String>,
}

/// A paragraph border (paragraph style fields 32, 45, 38).
#[derive(Debug, Clone, PartialEq)]
pub struct ParagraphBorder {
    /// The rule's stroke.
    pub stroke: Stroke,
    /// Sides drawn: 1 top, 2 bottom, 4 leading, 8 trailing.
    pub sides: i32,
    /// Leading and trailing swap (right-to-left paragraph).
    pub right_to_left: bool,
}

/// A text drop shadow (character style field 21).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextShadow {
    /// Shadow colour, its alpha including the opacity.
    pub color: Color,
    /// Horizontal offset.
    pub offset_x: f32,
    /// Vertical offset (down).
    pub offset_y: f32,
    /// Blur radius.
    pub blur: f32,
}

/// Resolved character + paragraph attributes for a run of text.
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    /// PostScript name of the face.
    pub font_name: String,
    /// Point size.
    pub size: f32,
    /// Bold requested.
    pub bold: bool,
    /// Italic requested.
    pub italic: bool,
    /// Text colour.
    pub color: Color,
    /// Paragraph alignment.
    pub align: HAlign,
    /// Underlined.
    pub underline: bool,
    /// Struck through.
    pub strike: bool,
    /// Line spacing amount (factor or points, by mode).
    pub line_spacing: f32,
    /// 0 relative, 1 at least, 2 exact, 3 at most, 4 between.
    pub line_spacing_mode: i32,
    /// Space above the paragraph.
    pub space_before: f32,
    /// Space below the paragraph.
    pub space_after: f32,
    /// Indent of every line but the first.
    pub left_indent: f32,
    /// Right indent.
    pub right_indent: f32,
    /// Indent of the first line (absolute, as in Cocoa).
    pub first_indent: f32,
    /// Capitalization: 1 all caps, 3 title case.
    pub caps: i32,
    /// Baseline shift in points (up).
    pub baseline: f32,
    /// 1 superscript, 2 subscript.
    pub superscript: i32,
    /// Tracking as an em fraction.
    pub tracking: f32,
    /// Explicit tab stops, sorted by position.
    pub tabs: Rc<Vec<Tab>>,
    /// Paragraph border.
    pub border: Option<ParagraphBorder>,
    /// Paragraph background (paragraph fill 6) across the text column.
    pub para_fill: Option<Color>,
    /// Character background (26).
    pub background: Option<Color>,
    /// Glyph outline (character tsd_stroke 44), stroked over the fill.
    pub outline: Option<Stroke>,
    /// Paragraph writing direction: -1 natural (from the text), 0 left-to-right, 1 right-to-left.
    pub direction: i32,
    /// Text drop shadow.
    pub shadow: Option<TextShadow>,
    /// Interval of the default tab stops past the last explicit one.
    pub default_tab: f32,
}

// CharacterStylePropertiesArchive field numbers.
const CHAR_BOLD: i32 = 1;
const CHAR_ITALIC: i32 = 2;
const CHAR_SIZE: i32 = 3;
const CHAR_FONT_NAME: i32 = 5;
const CHAR_COLOR: i32 = 7;
const CHAR_SUPERSCRIPT: i32 = 10;
const CHAR_UNDERLINE: i32 = 11;
const CHAR_STRIKE: i32 = 12;
const CHAR_CAPS: i32 = 13;
const CHAR_BASELINE: i32 = 14;
const CHAR_TRACKING: i32 = 27;

/// Case-insensitive (ASCII) prefix test.
pub fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn ends_with_ignore_case(text: &str, suffix: &str) -> bool {
    let start = text.len().wrapping_sub(suffix.len());
    text.len() >= suffix.len() && text.as_bytes()[start..].eq_ignore_ascii_case(suffix.as_bytes())
}

fn contains_ignore_case(text: &str, part: &str) -> bool {
    text.to_lowercase().contains(&part.to_lowercase())
}

impl Default for TextStyle {
    fn default() -> TextStyle {
        TextStyle {
            font_name: "HelveticaNeue".into(),
            size: 10.0,
            bold: false,
            italic: false,
            color: Color::BLACK,
            align: HAlign::Natural,
            underline: false,
            strike: false,
            line_spacing: 1.0,
            line_spacing_mode: 0,
            space_before: 0.0,
            space_after: 0.0,
            left_indent: 0.0,
            right_indent: 0.0,
            first_indent: 0.0,
            caps: 0,
            baseline: 0.0,
            superscript: 0,
            tracking: 0.0,
            tabs: Rc::new(Vec::new()),
            border: None,
            para_fill: None,
            background: None,
            outline: None,
            direction: -1,
            shadow: None,
            default_tab: 36.0,
        }
    }
}

impl TextStyle {
    /// A style with only font, size, colour and alignment set.
    pub fn simple(font: &str, size: f32, color: Color, align: HAlign) -> TextStyle {
        TextStyle {
            font_name: font.into(),
            size,
            color,
            align,
            ..Default::default()
        }
    }

    /// The Skia font for the style (variant face, superscript size), tagged for shaping.
    pub fn font(&self) -> TaggedFont {
        // A bold/italic request selects the variant face when one is known (measured: bold
        // Helvetica-Light text exports in Helvetica-Bold).
        let variant_name = variant(&self.font_name, self.bold, self.italic);
        let typeface = fonts::get(Some(&variant_name), self.bold, self.italic);
        let size = if self.superscript != 0 {
            self.size * 0.66
        } else {
            self.size
        };
        let mut font = Font::from_typeface(&typeface, size);
        font.set_subpixel(true);
        font.set_edging(FontEdging::AntiAlias);
        font.set_linear_metrics(true);
        font.set_hinting(FontHinting::None);
        // A face neither here nor on Apple's Mac is set in Helvetica by both: tag the face drawn
        // so it is shaped with Helvetica's own kerning (measured: BitstreamVeraSansMono code
        // text exports with Helvetica's "Te" pair).
        let name = match typeface.post_script_name() {
            Some(drawn) if fonts::is_exact(&drawn) && fonts::is_unknown_family(&variant_name) => {
                drawn
            }
            _ => variant_name,
        };
        TaggedFont {
            font,
            name,
            tracking: self.tracking,
        }
    }

    /// The text as displayed under the style's capitalization (1 = all caps, 3 = title case).
    pub fn cased(&self, text: &str) -> String {
        match self.caps {
            1 => text.to_uppercase(),
            3 => title_case(text),
            _ => text.to_owned(),
        }
    }

    /// Resolves a paragraph style (char props in 11, para props in 12) and optionally a character
    /// style (char props in 11) layered over it.
    ///
    /// # Arguments
    ///
    /// * `styles` - The document's styles
    /// * `para_style` - Paragraph style id (0: none)
    /// * `char_style` - Character style id (0: none), which wins over the paragraph style
    /// * `fallback` - What the styles leave unset (default: 10pt Helvetica Neue)
    ///
    /// # Returns
    ///
    /// The resolved style.
    pub fn resolve(
        styles: &Styles,
        para_style: u64,
        char_style: u64,
        fallback: Option<&TextStyle>,
    ) -> TextStyle {
        let default = TextStyle::default();
        let base = fallback.unwrap_or(&default);
        // The character properties holding a field: the character style's, else the paragraph's.
        let char_props = |field: i32| -> Option<Proto> {
            let from_char = (char_style != 0)
                .then(|| styles.find(char_style, 11, field))
                .flatten();
            from_char.or_else(|| {
                (para_style != 0)
                    .then(|| styles.find(para_style, 11, field))
                    .flatten()
            })
        };
        let mut style = TextStyle {
            size: char_props(CHAR_SIZE).map_or(base.size, |props| props.float(CHAR_SIZE)),
            bold: char_props(CHAR_BOLD).map_or(base.bold, |props| props.boolean(CHAR_BOLD)),
            italic: char_props(CHAR_ITALIC).map_or(base.italic, |props| props.boolean(CHAR_ITALIC)),
            color: char_props(CHAR_COLOR).map_or(base.color, |props| {
                styles::color(&props.message(CHAR_COLOR))
            }),
            underline: char_props(CHAR_UNDERLINE)
                .map_or(base.underline, |props| props.int(CHAR_UNDERLINE) != 0),
            strike: char_props(CHAR_STRIKE)
                .map_or(base.strike, |props| props.int(CHAR_STRIKE) != 0),
            caps: char_props(CHAR_CAPS).map_or(base.caps, |props| props.int(CHAR_CAPS)),
            baseline: char_props(CHAR_BASELINE)
                .map_or(base.baseline, |props| props.float(CHAR_BASELINE)),
            superscript: char_props(CHAR_SUPERSCRIPT)
                .map_or(base.superscript, |props| props.int(CHAR_SUPERSCRIPT)),
            tracking: char_props(CHAR_TRACKING)
                .map_or(base.tracking, |props| props.float(CHAR_TRACKING)),
            ..base.clone()
        };
        if para_style != 0 {
            apply_paragraph_properties(styles, para_style, &mut style);
        }
        // An explicit bold=false decides the weight: a "-Bold" face then renders regular
        // (measured: "Cochin-Bold" body text without the flag exports as Cochin).
        let font_name = char_props(CHAR_FONT_NAME).and_then(|props| props.string(CHAR_FONT_NAME));
        style.font_name = font_name.unwrap_or_else(|| base.font_name.clone());
        if !style.bold
            && contains_ignore_case(&style.font_name, "-Bold")
            && char_props(CHAR_BOLD).is_some()
        {
            style.font_name = style
                .font_name
                .replace("-BoldItalic", "-Italic")
                .replace("-Bold", "");
        }
        // Character outline (44; tsd_stroke_null 43 switches an inherited one off), and character
        // background (26; null flag 25): a style's own value wins over an inherited null flag.
        let unless_null = |field: i32, null_field: i32| {
            char_props(field).filter(|props| !(props.has(null_field) && props.boolean(null_field)))
        };
        let nulled =
            |null_field: i32| char_props(null_field).is_some_and(|props| props.boolean(null_field));
        match unless_null(44, 43)
            .and_then(|props| styles::stroke_of(Some(&props.message(44)), false))
        {
            Some(outline) => style.outline = Some(outline),
            None if nulled(43) => style.outline = None,
            None => {}
        }
        let background = unless_null(26, 25).map(|props| styles::color(&props.message(26)));
        match background.filter(|color| color.a() > 0) {
            Some(color) => style.background = Some(color),
            None if nulled(25) => style.background = None,
            None => {}
        }
        // Character shadow (21; shadow_null 20 switches an inherited one off).
        let shadow = char_props(21)
            .map(|props| props.message(21))
            .filter(|shadow| shadow.has(1) && shadow.boolean_or(6, true));
        if nulled(20) {
            style.shadow = None;
        } else if let Some(shadow) = shadow {
            style.shadow = Some(text_shadow(&shadow));
        }
        if style.default_tab <= 0.0 {
            style.default_tab = 36.0;
        }
        style
    }
}

/// Paragraph properties (field 12) of a paragraph style applied over `style`.
fn apply_paragraph_properties(styles: &Styles, para_style: u64, style: &mut TextStyle) {
    let props = |field: i32| styles.find(para_style, 12, field);
    if let Some(alignment) = props(1) {
        style.align = HAlign::from_stored(alignment.int(1));
    }
    if let Some(holder) = props(13) {
        // line_spacing: LineSpacingArchive { mode = 1, amount = 2 }; an empty one is an explicit
        // single spacing, not "inherit".
        let spacing = holder.message(13);
        let explicit = spacing.has(2);
        style.line_spacing = if explicit {
            spacing.float_or(2, 1.0)
        } else {
            1.0
        };
        style.line_spacing_mode = if explicit { spacing.int(1) } else { 0 };
    }
    style.space_before = styles.float(para_style, 12, 21, style.space_before);
    style.space_after = styles.float(para_style, 12, 20, style.space_after);
    style.left_indent = styles.float(para_style, 12, 11, style.left_indent);
    style.right_indent = styles.float(para_style, 12, 19, style.right_indent);
    style.first_indent = styles.float(para_style, 12, 7, style.first_indent);
    let right_to_left = props(38).map(|direction| direction.int(38));
    match props(45).map(|sides| sides.int(45)) {
        Some(0) => style.border = None,
        Some(sides) => {
            let stroke =
                props(32).and_then(|holder| styles::stroke_of(Some(&holder.message(32)), false));
            if let Some(stroke) = stroke {
                style.border = Some(ParagraphBorder {
                    stroke,
                    sides,
                    right_to_left: right_to_left == Some(1),
                });
            }
        }
        None => {}
    }
    if let Some(holder) = props(25) {
        let stops = holder.message(25).messages(1);
        let mut tabs: Vec<Tab> = stops
            .iter()
            .map(|stop| Tab {
                position: stop.float(1),
                align: stop.int(2),
                leader: if stop.has(3) { stop.string(3) } else { None },
            })
            .collect();
        tabs.sort_by(|first, second| first.position.total_cmp(&second.position));
        style.tabs = Rc::new(tabs);
    }
    style.default_tab = styles.float(para_style, 12, 4, style.default_tab);
    let mut para_fill = None;
    if let Some(fill) = props(6) {
        let color = styles::color(&fill.message(6));
        if !(fill.has(5) && fill.boolean(5)) && color.a() > 0 {
            para_fill = Some(color);
        }
    }
    style.para_fill = para_fill;
    if let Some(direction) = right_to_left {
        style.direction = direction;
    }
}

/// A TSD.ShadowArchive as a text shadow: angle (degrees, clockwise), offset, radius, opacity.
fn text_shadow(shadow: &Proto) -> TextShadow {
    let color = styles::color(&shadow.message(1));
    let angle = f64::from(shadow.float_or(2, 315.0)).to_radians();
    let offset = f64::from(shadow.float_or(3, 5.0));
    TextShadow {
        color: color.with_a((f32::from(color.a()) * shadow.float_or(5, 1.0)) as u8),
        offset_x: (offset * angle.cos()) as f32,
        offset_y: (offset * angle.sin()) as f32,
        blur: shadow.int_or(4, 1) as f32,
    }
}

/// The PostScript name of a face's bold/italic variant when the style asks for one the name lacks
/// ("Palatino-Roman" + bold → "Palatino-Bold"); the name itself when none is known.
///
/// # Arguments
///
/// * `postscript_name` - The stored face name
/// * `bold` - Bold requested
/// * `italic` - Italic requested
///
/// # Returns
///
/// The variant's name when apple-metrics.json or an exact font knows it, else `postscript_name`.
pub fn variant(postscript_name: &str, bold: bool, italic: bool) -> String {
    let bold = bold && !contains_ignore_case(postscript_name, "Bold");
    let italic = italic
        && !contains_ignore_case(postscript_name, "Italic")
        && !contains_ignore_case(postscript_name, "Oblique");
    if !bold && !italic {
        return postscript_name.to_owned();
    }
    let known = |name: &str| shaper::knows(name) || fonts::is_exact(name);
    let weights = [
        "-Roman",
        "-Regular",
        "-Plain",
        "-Book",
        "-Light",
        "-Thin",
        "-UltraLight",
        "-Medium",
    ];
    let stem = weights
        .iter()
        .find(|suffix| ends_with_ignore_case(postscript_name, suffix))
        .map_or(postscript_name, |suffix| {
            &postscript_name[..postscript_name.len() - suffix.len()]
        });
    let tails: &[&str] = match (bold, italic) {
        (true, true) => &["-BoldItalic", "-BoldOblique"],
        (true, false) => &["-Bold"],
        _ => &["-Italic", "-Oblique"],
    };
    // A styled weight joins the style to it ("Helvetica-Light" → "Helvetica-LightOblique"); a
    // family name takes it after a dash ("Helvetica" → "Helvetica-Oblique").
    let joined = |base: &str, tail: &str| {
        if base.contains('-') {
            format!("{base}{}", &tail[1..])
        } else {
            format!("{base}{tail}")
        }
    };
    [postscript_name, stem]
        .iter()
        .flat_map(|base| tails.iter().map(move |tail| joined(base, tail)))
        .find(|candidate| known(candidate))
        .unwrap_or_else(|| postscript_name.to_owned())
}

/// .NET's invariant TextInfo.ToTitleCase: each word's first letter upper case, the rest lower
/// case, words entirely in upper case kept (acronyms).
fn title_case(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let characters: Vec<char> = text.chars().collect();
    let mut start = 0;
    while start < characters.len() {
        if !characters[start].is_alphanumeric() {
            output.push(characters[start]);
            start += 1;
            continue;
        }
        let in_word = |character: &char| character.is_alphanumeric() || *character == '\'';
        let end = start
            + characters[start..]
                .iter()
                .take_while(|character| in_word(character))
                .count();
        let word = &characters[start..end];
        let acronym = word.iter().all(|character| !character.is_lowercase())
            && word.iter().any(|character| character.is_uppercase());
        if acronym {
            output.extend(word);
        } else {
            output.extend(word[0].to_uppercase());
            output.extend(
                word[1..]
                    .iter()
                    .flat_map(|character| character.to_lowercase()),
            );
        }
        start = end;
    }
    output
}

// ------------------------------------------------------------- Apple line metrics
//
// Line layout as Apple's text engine does it. With the font's ascent, descent and leading at the
// given size, a line is round(ascent + descent + leading) tall; the rounding slack goes above the
// first baseline (baseline = line - descent - leading), and a block of n lines is n * line -
// leading tall (the last line's leading is dropped). Verified against Numbers exports: Helvetica
// Neue 10pt gives line 12, first baseline 9.59, one-line block 11.72. Apple faces use their
// FontDescriptor metrics (apple-metrics.json).

/// Ascent, descent and leading of a face at a size, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FaceMetrics {
    ascent: f32,
    descent: f32,
    leading: f32,
}

/// Rounds half away from zero, as .NET's MidpointRounding.AwayFromZero.
fn round_half_away(value: f32) -> f32 {
    value.round()
}

fn is_times(name: &str) -> bool {
    name == "Times" || starts_with_ignore_case(name, "Times-")
}

/// Ascents (1/1000 em) Apple lays out where the FontDescriptor understates the line:
/// Cochin (a 14pt title takes a 17pt line while 13pt legend rows keep a 15pt pitch; midpoint of
/// [0.929, 0.942)), Palatino (22pt lines 29pt apart), Lucida Grande (13pt lines 16pt apart), Times
/// (8-25pt natural lines 10-30pt), Skia (26-144pt lines 1.1991-1.2014em), Courier (11-48pt lines
/// 1.198-1.202em).
fn measured_ascent(name: &str) -> Option<f32> {
    let by_prefix = [
        ("Cochin", 935.0),
        ("Palatino", 1041.0),
        ("LucidaGrande", 1000.0),
        ("Skia", 977.0),
    ];
    if let Some((_, ascent)) = by_prefix
        .iter()
        .find(|(prefix, _)| starts_with_ignore_case(name, prefix))
    {
        return Some(*ascent);
    }
    if is_times(name) {
        return Some(949.0);
    }
    (name == "Courier" || starts_with_ignore_case(name, "Courier-")).then_some(954.0)
}

fn face_metrics(style: &TextStyle) -> FaceMetrics {
    // The styled face's own metrics: Helvetica Neue Italic's 0.957em ascent rounds an 11pt line to
    // 13 where the roman's 0.952em gives 12.
    let mut name = variant(&style.font_name, style.bold, style.italic);
    if apple_metrics(&name).is_none() {
        name = variant(&style.font_name, style.bold, false);
    }
    if apple_metrics(&name).is_none() {
        name = style.font_name.clone();
    }
    let helvetica = starts_with_ignore_case(&name, "Helvetica")
        && !starts_with_ignore_case(&name, "HelveticaNeue");
    // Helvetica's bold/oblique share the roman metrics; a font Apple's Mac lacks as well exports
    // in Helvetica, so its lines take Helvetica's metrics.
    let unknown = !helvetica && fonts::is_unknown_family(&style.font_name);
    if apple_metrics(&name).is_none() && (helvetica || unknown) {
        name = "Helvetica".into();
    }
    let Some(descriptor) = apple_metrics(&name) else {
        let (_, metrics) = style.font().font.metrics();
        return FaceMetrics {
            ascent: -metrics.ascent,
            descent: metrics.descent,
            leading: metrics.leading,
        };
    };
    let (mut ascent, descent, leading) = (descriptor[0], descriptor[1], descriptor[2]);
    // Helvetica's descriptor (770/-230) understates the line Apple lays out: 10pt rows use 12pt
    // lines with the first baseline 9.7pt down, i.e. a 0.97em ascent.
    if leading == 0.0
        && starts_with_ignore_case(&name, "Helvetica")
        && !starts_with_ignore_case(&name, "HelveticaNeue")
    {
        ascent = 970.0;
    }
    if let Some(measured) = measured_ascent(&name) {
        ascent = measured;
    }
    let scale = style.size / 1000.0;
    FaceMetrics {
        ascent: ascent * scale,
        descent: -descent * scale,
        leading: leading * scale,
    }
}

/// round(ascent + descent + leading) (Helvetica Neue 10pt → 12, Gill Sans 40pt → 46).
fn natural_line(style: &TextStyle) -> f32 {
    let metrics = face_metrics(style);
    let name = &style.font_name;
    // Helvetica Neue, Menlo, Times and Georgia sum their separately rounded ascent, descent and
    // leading (measured: 11pt Helvetica Neue lines 12pt apart in Pages; 15, 18 and 22pt Menlo lines
    // 18, 21 and 25pt; 16, 20 and 34pt Georgia lines 19, 22 and 38pt).
    let separately = ["HelveticaNeue", "Menlo", "Georgia"]
        .iter()
        .any(|prefix| starts_with_ignore_case(name, prefix))
        || is_times(name);
    if separately {
        return round_half_away(metrics.ascent)
            + round_half_away(metrics.descent)
            + round_half_away(metrics.leading);
    }
    round_half_away(metrics.ascent + metrics.descent + metrics.leading)
}

/// Distance from the top of the text block to the first baseline.
pub fn ascent(style: &TextStyle) -> f32 {
    let metrics = face_metrics(style);
    let below = metrics.descent + metrics.leading;
    if style.line_spacing_mode == 2 {
        // Exact spacing: the space beyond the natural line is split above and below the baseline in
        // the font's ascent:descent proportion (Helvetica 10pt at 15.5pt: baseline
        // 9.7 + 3.5 × 0.77).
        let natural = natural_line(style);
        let ratio = match apple_metrics(&style.font_name) {
            Some(descriptor) if descriptor[0] - descriptor[1] > 0.0 => {
                descriptor[0] / (descriptor[0] - descriptor[1])
            }
            _ => metrics.ascent / (metrics.ascent + metrics.descent),
        };
        return natural - below + (style.line_spacing - natural) * ratio;
    }
    // In Pages, relative spacing above 1 adds its space below the baseline (Keynote: above).
    if spacing_below_active() && style.line_spacing_mode == 0 && style.line_spacing > 1.0 {
        return natural_line(style) - below;
    }
    line_height(style) - below
}

/// True while laying out Pages, where relative spacing above 1 lies below the baseline.
pub fn spacing_below_active() -> bool {
    ctx::PAGES_LAYOUT.get()
}

/// The font's leading at the style's size (dropped after a block's last line).
pub fn trailing_leading(style: &TextStyle) -> f32 {
    face_metrics(style).leading
}

/// The font's descent at the style's size.
pub fn descent(style: &TextStyle) -> f32 {
    face_metrics(style).descent
}

/// Single-line title metrics.
///
/// # Returns
///
/// (line, baseline): line = round(ascent + descent), baseline = line - descent - leading.
pub fn title_line(style: &TextStyle) -> (f32, f32) {
    let metrics = face_metrics(style);
    let line = round_half_away(metrics.ascent + metrics.descent);
    // A CJK face's 0.5em leading (Hiragino) is not applied to the single title line.
    let leading = if metrics.leading > 0.2 * style.size {
        0.0
    } else {
        metrics.leading
    };
    (line, line - metrics.descent - leading)
}

/// The face whose metrics a style's line takes (for detecting lines that mix faces).
pub fn face(style: &TextStyle) -> String {
    variant(&style.font_name, style.bold, style.italic)
}

/// Below the baseline of a line mixing faces: the largest descent plus the largest leading
/// (measured in Pages: 11pt Helvetica Neue lines with inline Courier 13.31pt apart, 12.37pt to
/// the next plain line).
pub fn mixed_below<'a>(styles: impl Iterator<Item = &'a TextStyle>) -> f32 {
    let (descent, leading) = styles
        .map(face_metrics)
        .fold((0f32, 0f32), |(descent, leading), metrics| {
            (descent.max(metrics.descent), leading.max(metrics.leading))
        });
    descent + leading
}

/// Baseline-to-baseline distance, honouring the paragraph's line spacing mode.
pub fn line_height(style: &TextStyle) -> f32 {
    let natural = natural_line(style);
    match style.line_spacing_mode {
        1 => natural.max(style.line_spacing),
        2 => style.line_spacing,
        3 => natural.min(style.line_spacing),
        4 => natural + style.line_spacing,
        // Relative spacing scales the line without its leading (measured: Helvetica Neue Light 10pt
        // at 1.3 is 15.51pt, (12 - 0.29) x 1.3 + 0.29).
        _ => {
            let leading = face_metrics(style).leading;
            (natural - leading) * style.line_spacing + leading
        }
    }
}

// ------------------------------------------------------------------ drawing

/// Greedy word wrap of one paragraph.
///
/// # Arguments
///
/// * `text` - The paragraph (no line breaks)
/// * `font` - The font it is set in
/// * `width` - The line width
/// * `wrap` - False keeps the text on one line
///
/// # Returns
///
/// The lines, trailing spaces trimmed; a word wider than the line is broken by characters.
pub fn wrap(text: &str, font: &TaggedFont, width: f32, wrap: bool) -> Vec<String> {
    if !wrap || width <= 0.0 {
        return vec![text.to_owned()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in split_keep_spaces(text) {
        let trial = format!("{current}{word}");
        if !current.is_empty() && shaper::measure(trial.trim_end(), font) > width + 0.01 {
            lines.push(current.trim_end().to_owned());
            current = word.trim_start().to_owned();
        } else {
            current = trial;
        }
        // A single word wider than the line (also a paragraph's first) is broken by characters.
        while current.trim_end().chars().count() > 1
            && shaper::measure(current.trim_end(), font) > width + 0.01
        {
            let characters: Vec<char> = current.chars().collect();
            let mut fit = characters.len() - 1;
            while fit > 1
                && shaper::measure(&characters[..fit].iter().collect::<String>(), font) > width
            {
                fit -= 1;
            }
            lines.push(characters[..fit].iter().collect());
            current = characters[fit..].iter().collect();
        }
    }
    lines.push(current.trim_end().to_owned());
    lines
}

/// Words that keep their trailing spaces (and their breaking dash or solidus).
fn split_keep_spaces(text: &str) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    let mut words = Vec::new();
    let mut start = 0;
    while start < characters.len() {
        let mut end = start;
        while end < characters.len() && !break_char(&characters, end) {
            end += 1;
        }
        if end < characters.len() && characters[end] != ' ' {
            end += 1;
        }
        while end < characters.len() && characters[end] == ' ' {
            end += 1;
        }
        let end = end.max(start + 1);
        words.push(characters[start..end].iter().collect());
        start = end;
    }
    words
}

/// True when a line may break after `characters[index]`: after a space, after a hyphen not
/// followed by a digit (UAX #14 HY × NU: "2026-10-21" stays together), after a solidus before a
/// letter ("(docs/" | "BLOCKERS"), or before a digit when a letter precedes it
/// ("blog.appsignal.com/" | "2020/03/10/", while "2010/01/" stays together).
pub fn break_char(characters: &[char], index: usize) -> bool {
    let next = characters.get(index + 1);
    let previous = index
        .checked_sub(1)
        .and_then(|before| characters.get(before));
    match characters[index] {
        ' ' => true,
        '-' | '—' => !next.is_some_and(|next| next.is_numeric()),
        '/' => match (previous, next) {
            (Some(previous), Some(next)) => {
                next.is_alphabetic() || (next.is_numeric() && previous.is_alphabetic())
            }
            _ => false,
        },
        _ => false,
    }
}

/// Draws one line of text in the style's colour, with its baseline shift, superscript, shadow,
/// outline, underline and strikethrough.
///
/// # Arguments
///
/// * `canvas` - The canvas
/// * `text` - The line's text (upper-cased here for all-caps styles)
/// * `origin` - The left end of the baseline
/// * `font` - The font (from `style.font()`, possibly scaled)
/// * `style` - The style
pub fn draw_line(canvas: &Canvas, text: &str, origin: Point, font: &TaggedFont, style: &TextStyle) {
    if text.is_empty() {
        return;
    }
    // `Cow` borrows the text unless all caps needs an upper-cased copy.
    let text: Cow<'_, str> = if style.caps == 1 {
        text.to_uppercase().into()
    } else {
        text.into()
    };
    let superscript_shift = match style.superscript {
        1 => style.size * 0.33,
        2 => -style.size * 0.15,
        _ => 0.0,
    };
    let at = Point::new(origin.x, origin.y - style.baseline - superscript_shift);
    if let Some(shadow) = style.shadow {
        let mut shadow_paint = styles::fill_paint(shadow.color);
        if shadow.blur > 0.0 {
            shadow_paint.set_mask_filter(MaskFilter::blur(
                BlurStyle::Normal,
                shadow.blur / 2.0,
                None,
            ));
        }
        shaper::draw(
            canvas,
            &text,
            at + Point::new(shadow.offset_x, shadow.offset_y),
            font,
            &shadow_paint,
        );
    }
    shaper::draw(canvas, &text, at, font, &styles::fill_paint(style.color));
    if let Some(outline) = &style.outline {
        let mut outline_paint = outline.paint(false);
        outline_paint.set_style(PaintStyle::Stroke);
        shaper::draw(canvas, &text, at, font, &outline_paint);
    }
    if !style.underline && !style.strike {
        return;
    }
    let width = shaper::measure(&text, font);
    let mut rule = styles::fill_paint(style.color);
    rule.set_stroke_width((style.size / 14.0).max(0.5));
    for (enabled, offset) in [
        (style.underline, style.size * 0.15),
        (style.strike, -style.size * 0.3),
    ] {
        if enabled {
            canvas.draw_line((at.x, at.y + offset), (at.x + width, at.y + offset), &rule);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_case_keeps_acronyms() {
        assert_eq!(title_case("hello wORLD NASA"), "Hello World NASA");
    }

    #[test]
    fn break_rules() {
        let text = "2026-10-21 a-b docs/X 2010/01 com/2020";
        let characters: Vec<char> = text.chars().collect();
        let position = |part: &str| text.find(part).expect("part of the text");
        assert!(break_char(&characters, position("a-b") + 1));
        assert!(break_char(&characters, position("docs/") + 4));
        assert!(break_char(&characters, position("com/") + 3));
        assert!(!break_char(&characters, 4));
        assert!(!break_char(&characters, position("2010/") + 4));
    }

    #[test]
    fn variant_picks_known_faces() {
        assert_eq!(variant("Helvetica-Bold", true, false), "Helvetica-Bold");
        assert_eq!(variant("Helvetica", false, true), "Helvetica-Oblique");
        if fonts::is_exact("Helvetica-LightOblique") {
            assert_eq!(
                variant("Helvetica-Light", false, true),
                "Helvetica-LightOblique"
            );
        }
        assert_eq!(variant("NoSuchFace", true, false), "NoSuchFace");
    }

    #[test]
    fn helvetica_neue_line_metrics() {
        // Measured on Numbers exports: 10pt Helvetica Neue gives a 12pt line, baseline 9.59 down.
        let style = TextStyle::default();
        assert_eq!(line_height(&style), 12.0);
        assert!((ascent(&style) - 9.59).abs() < 0.05);
    }

    #[test]
    fn wrap_breaks_long_text() {
        let font = TextStyle::default().font();
        assert!(wrap("one two three four five six", &font, 40.0, true).len() > 1);
        assert_eq!(
            wrap("one two", &font, 40.0, false),
            vec!["one two".to_string()]
        );
    }
}
