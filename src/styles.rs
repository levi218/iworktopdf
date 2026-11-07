//! Style lookups. Every iWork style (TSS.StyleArchive in field 1) carries a properties message
//! (field 11, or 12 for paragraph properties) holding only the overridden values; anything missing
//! is inherited from the parent style (StyleArchive.parent = 3).

use crate::iwa::{IwaDocument, IwaObject, Proto};
use skia_safe::{Color, Paint, PaintCap, PaintStyle, PathEffect};
use std::rc::Rc;

/// Per-conversion layout state, kept per thread (like C#'s `[ThreadStatic]`) and reset by the
/// converter for each document.
pub mod ctx {
    use skia_safe::Rect;
    use std::cell::{Cell, RefCell};

    // `thread_local!` gives each thread its own copy; `Cell` allows setting a `Copy` value
    // through a shared reference, `RefCell` the same for non-`Copy` values.
    thread_local! {
        /// Pages (and Keynote) table rows grow to their content.
        pub static GROW_ROWS: Cell<bool> = const { Cell::new(false) };
        /// A zero-size text box's natural frame is centred on its position (Keynote).
        pub static CENTRE_ZERO_SIZE: Cell<bool> = const { Cell::new(false) };
        /// Pages' text engine lays some lines out differently (see text.rs).
        pub static PAGES_LAYOUT: Cell<bool> = const { Cell::new(false) };
        /// The Keynote slide width, set per slide.
        pub static SLIDE_WIDTH: Cell<f32> = const { Cell::new(0.0) };
        /// Set while painting a table cell's fill: Apple draws a cell gradient's stops opaque.
        pub static OPAQUE_STOPS: Cell<bool> = const { Cell::new(false) };
        /// Current page number for page-number fields.
        pub static PAGE_NUMBER: Cell<i32> = const { Cell::new(0) };
        /// Page count for page-count fields.
        pub static PAGE_COUNT: Cell<i32> = const { Cell::new(0) };
        /// Running footnote number for references in the body.
        pub static FOOTNOTE_COUNTER: Cell<i32> = const { Cell::new(0) };
        /// The number shown by the mark inside a footnote.
        pub static FOOTNOTE_MARK: Cell<i32> = const { Cell::new(0) };
        /// The document's locale identifier (e.g. "en_GB").
        pub static LOCALE: RefCell<Option<String>> = const { RefCell::new(None) };
        /// Floating obstacles wrapped "above and below": lines never sit beside them.
        pub static ABOVE_BELOW: RefCell<Vec<Rect>> = const { RefCell::new(Vec::new()) };
    }
}

/// Style lookups over one document.
#[derive(Debug, Clone)]
pub struct Styles {
    doc: Rc<IwaDocument>,
}

impl Styles {
    /// Wraps a document for style lookups.
    pub fn new(doc: Rc<IwaDocument>) -> Styles {
        Styles { doc }
    }

    /// The document the styles belong to.
    pub fn doc(&self) -> &Rc<IwaDocument> {
        &self.doc
    }

    /// The first properties message in the parent chain that has `field`.
    ///
    /// # Arguments
    ///
    /// * `style` - The style object id
    /// * `props_field` - 11 for character/shape properties, 12 for paragraph properties
    /// * `field` - The property's field number
    ///
    /// # Returns
    ///
    /// The properties message holding the field, or `None` when no style in the chain sets it.
    pub fn find(&self, style: u64, props_field: i32, field: i32) -> Option<Proto> {
        self.find_in(
            style,
            |object| paragraph_message(object).message_opt(props_field),
            field,
        )
    }

    /// Like `find` with an arbitrary path from a style object to its properties message.
    ///
    /// # Arguments
    ///
    /// * `style` - The style object id
    /// * `props` - Extracts the properties message from a style object in the chain
    /// * `field` - The property's field number
    ///
    /// # Returns
    ///
    /// The properties message holding the field (at most 32 parents are followed).
    pub fn find_in(
        &self,
        mut style: u64,
        props: impl Fn(&IwaObject) -> Option<Proto>,
        field: i32,
    ) -> Option<Proto> {
        for _ in 0..32 {
            let object = self.doc.get(style)?;
            if let Some(found) = props(object).filter(|found| found.has(field)) {
                return Some(found);
            }
            style = parent(object);
        }
        None
    }

    /// TSD shape properties of a TSD or TSWP shape style that set `field`.
    pub fn shape_prop(&self, style: u64, field: i32) -> Option<Proto> {
        let props = |object: &IwaObject| {
            if object.typ == 2025 {
                object.body.message(1).message_opt(11)
            } else {
                object.body.message_opt(11)
            }
        };
        self.find_in(style, props, field)
    }

    /// TSWP shape (text frame) properties of a TSWP shape style that set `field`.
    pub fn text_frame_prop(&self, style: u64, field: i32) -> Option<Proto> {
        let props = |object: &IwaObject| (object.typ == 2025).then(|| object.body.message_opt(11));
        self.find_in(style, |object| props(object).flatten(), field)
    }

    /// A float property, or `default` when the chain does not set it.
    pub fn float(&self, style: u64, props_field: i32, field: i32, default: f32) -> f32 {
        let found = self.find(style, props_field, field);
        found.map_or(default, |props| props.float_or(field, default))
    }

    /// A boolean property, or `default` when the chain does not set it.
    pub fn bool(&self, style: u64, props_field: i32, field: i32, default: bool) -> bool {
        let found = self.find(style, props_field, field);
        found.map_or(default, |props| props.boolean_or(field, default))
    }

    /// An integer property, or `default` when the chain does not set it.
    pub fn int(&self, style: u64, props_field: i32, field: i32, default: i32) -> i32 {
        let found = self.find(style, props_field, field);
        found.map_or(default, |props| props.int_or(field, default))
    }

    /// A message property, if the chain sets it.
    pub fn message(&self, style: u64, props_field: i32, field: i32) -> Option<Proto> {
        self.find(style, props_field, field)
            .map(|props| props.message(field))
    }
}

/// The paragraph style message: TSWP.TOCEntryStyleArchive (2026) wraps one as its super.
fn paragraph_message(object: &IwaObject) -> Proto {
    if object.typ == 2026 {
        object.body.message(1)
    } else {
        object.body.clone()
    }
}

/// TSS.StyleArchive.parent; TSWP shape and TOC entry styles nest their TSS super one level deeper.
pub fn parent(object: &IwaObject) -> u64 {
    if object.typ == 2025 || object.typ == 2026 {
        object.body.message(1).message(1).reference(3)
    } else {
        object.body.message(1).reference(3)
    }
}

/// A 0-1 channel value as a byte (rounded, clamped).
fn to_byte(fraction: f32) -> u8 {
    (fraction * 255.0).round().clamp(0.0, 255.0) as u8
}

/// TSP.Color → sRGB colour: RGB (Display P3 when `rgbspace` is p3), CMYK or grey.
///
/// # Arguments
///
/// * `message` - A TSP.Color message
///
/// # Returns
///
/// The colour with its alpha.
pub fn color(message: &Proto) -> Color {
    let alpha = to_byte(message.float_or(6, 1.0));
    match message.int_or(1, 1) {
        3 => {
            let white = to_byte(message.float(11));
            Color::from_argb(alpha, white, white, white)
        }
        2 => {
            let not_black = 1.0 - message.float(10);
            let channel = |field: i32| to_byte((1.0 - message.float(field)) * not_black);
            Color::from_argb(alpha, channel(7), channel(8), channel(9))
        }
        _ => {
            let (mut red, mut green, mut blue) =
                (message.float(3), message.float(4), message.float(5));
            if message.int_or(12, 1) == 2 {
                (red, green, blue) = p3_to_srgb(red, green, blue);
            }
            Color::from_argb(alpha, to_byte(red), to_byte(green), to_byte(blue))
        }
    }
}

/// Display P3 → sRGB (shared D65 white and sRGB transfer curve; out-of-gamut clamps).
fn p3_to_srgb(red: f32, green: f32, blue: f32) -> (f32, f32, f32) {
    let linear = |encoded: f32| {
        let encoded = f64::from(encoded);
        if encoded <= 0.04045 {
            encoded / 12.92
        } else {
            ((encoded + 0.055) / 1.055).powf(2.4)
        }
    };
    let encode = |linear: f64| {
        let linear = linear.clamp(0.0, 1.0);
        let encoded = if linear <= 0.0031308 {
            linear * 12.92
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        };
        encoded as f32
    };
    let (red, green, blue) = (linear(red), linear(green), linear(blue));
    (
        encode(1.2249401 * red - 0.2249404 * green),
        encode(-0.0420569 * red + 1.0420571 * green),
        encode(-0.0196376 * red - 0.0786361 * green + 1.0982735 * blue),
    )
}

/// TSD.FillArchive → solid colour (gradients approximated by their first stop).
pub fn fill_color(fill: Option<&Proto>) -> Option<Color> {
    let fill = fill?;
    if let Some(solid) = fill.message_opt(1) {
        return Some(color(&solid));
    }
    // GradientArchive: stops = 2 { color = 1, fraction = 2 }
    let gradient = fill.message_opt(2)?;
    gradient
        .messages(2)
        .first()
        .map(|stop| color(&stop.message(1)))
}

/// A resolved TSD.StrokeArchive.
#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    /// Stroke colour.
    pub color: Color,
    /// Line width in points.
    pub width: f32,
    /// Dash intervals in points, if dashed.
    pub dashes: Option<Vec<f32>>,
    /// Cap: 0 butt, 1 round, 2 square.
    pub cap: i32,
    /// False for an "empty" pattern that only takes up layout space.
    pub visible: bool,
}

/// Reads a TSD.StrokeArchive.
///
/// # Arguments
///
/// * `message` - The stroke message, if any
/// * `keep_empty` - Keep an empty-pattern stroke (tables lay out invisible rules with their width)
///
/// # Returns
///
/// `None` when absent, of zero width, or (unless `keep_empty`) empty.
pub fn stroke_of(message: Option<&Proto>, keep_empty: bool) -> Option<Stroke> {
    let message = message.filter(|message| !message.is_empty())?;
    let pattern = message.message_opt(6);
    // Pattern type: 0 dash pattern, 1 solid, 2 empty.
    let pattern_type = pattern.as_ref().map_or(1, |pattern| pattern.int_or(1, 1));
    if pattern_type == 2 && !keep_empty {
        return None;
    }
    let width = message.float_or(2, 1.0);
    if width <= 0.0 {
        return None;
    }
    let dashes = pattern.filter(|_| pattern_type == 0).and_then(|pattern| {
        let count = usize::try_from(pattern.int(3)).unwrap_or(0);
        let intervals: Vec<f32> = pattern
            .floats(4)
            .into_iter()
            .take(count)
            .map(|interval| interval * width)
            .collect();
        (intervals.len() >= 2).then_some(intervals)
    });
    let color = message
        .message_opt(1)
        .map_or(Color::BLACK, |solid| color(&solid));
    Some(Stroke {
        color,
        width,
        dashes,
        cap: message.int(3),
        visible: pattern_type != 2,
    })
}

impl Stroke {
    /// Numbers draws strokes thinner than 1pt in a colour blended toward white by the width
    /// (black 0.35pt → 65% grey), emulating hairline coverage.
    pub fn effective(&self) -> Color {
        if self.width >= 1.0 {
            return self.color;
        }
        let mix = |channel: u8| (255.0 - self.width * (255.0 - f32::from(channel))).round() as u8;
        let base = self.color;
        Color::from_argb(base.a(), mix(base.r()), mix(base.g()), mix(base.b()))
    }

    /// A stroking paint.
    ///
    /// # Arguments
    ///
    /// * `hairline_blend` - Use the blended `effective` colour for sub-point widths (tables)
    ///
    /// # Returns
    ///
    /// An anti-aliased stroke paint with this width, cap and dashes.
    pub fn paint(&self, hairline_blend: bool) -> Paint {
        let mut paint = fill_paint(if hairline_blend {
            self.effective()
        } else {
            self.color
        });
        paint.set_style(PaintStyle::Stroke);
        paint.set_stroke_width(self.width);
        paint.set_stroke_cap(match self.cap {
            1 => PaintCap::Round,
            2 => PaintCap::Square,
            _ => PaintCap::Butt,
        });
        if let Some(intervals) = &self.dashes {
            paint.set_path_effect(PathEffect::dash(intervals, 0.0));
        }
        paint
    }
}

/// An anti-aliased fill paint.
pub fn fill_paint(color: Color) -> Paint {
    let mut paint = plain_paint(color);
    paint.set_anti_alias(true);
    paint
}

/// A fill paint without anti-aliasing (SkiaSharp's default for plain rectangles).
pub fn plain_paint(color: Color) -> Paint {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Proto {
        Proto::new(Rc::new(bytes.to_vec()), 0, bytes.len())
    }

    fn float_field(number: u8, value: f32) -> Vec<u8> {
        let mut bytes = vec![number << 3 | 5];
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes
    }

    #[test]
    fn rgb_and_grey_colours() {
        let mut rgb = vec![0x08, 1];
        rgb.extend(float_field(3, 1.0));
        rgb.extend(float_field(4, 0.5));
        assert_eq!(color(&parse(&rgb)), Color::from_argb(255, 255, 128, 0));
        let mut grey = vec![0x08, 3];
        grey.extend(float_field(11, 0.2));
        grey.extend(float_field(6, 0.5));
        assert_eq!(color(&parse(&grey)), Color::from_argb(128, 51, 51, 51));
    }

    #[test]
    fn p3_white_stays_white() {
        let (red, green, blue) = p3_to_srgb(1.0, 1.0, 1.0);
        assert!([red, green, blue]
            .iter()
            .all(|channel| (channel - 1.0).abs() < 1e-3));
    }

    #[test]
    fn thin_strokes_blend_toward_white() {
        let stroke = Stroke {
            color: Color::BLACK,
            width: 0.35,
            dashes: None,
            cap: 0,
            visible: true,
        };
        assert_eq!(stroke.effective(), Color::from_argb(255, 166, 166, 166));
    }

    #[test]
    fn empty_pattern_stroke_needs_keep_empty() {
        // width 2, pattern { type = 2 }
        let mut bytes = float_field(2, 2.0);
        bytes.extend([0x32, 2, 0x08, 2]);
        assert!(stroke_of(Some(&parse(&bytes)), false).is_none());
        let kept = stroke_of(Some(&parse(&bytes)), true).expect("kept with keep_empty");
        assert!(!kept.visible);
        assert_eq!(kept.width, 2.0);
    }
}
