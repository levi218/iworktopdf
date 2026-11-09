//! Things placed on a canvas (shapes, text boxes, images, groups, tables, charts): each knows its
//! bounds (for page sizing) and how to draw itself. Also path geometry, fills and line ends.

use crate::chart::ChartItem;
use crate::iwa::{IwaDocument, IwaObject, Proto};
use crate::numbers_table::NumbersTable;
use crate::richtext::RichText;
use crate::styles::{self, ctx, Stroke, Styles};
use crate::table::TableItem;
use crate::text::{HAlign, TextStyle};
use skia_safe::{
    images, path_utils, AlphaType, BlendMode, Canvas, Color, ColorType, Data, FilterMode, Image,
    ImageInfo, Matrix, MipmapMode, Paint, PaintCap, PaintStyle, Path, PathFillType, PathOp, Point,
    Rect, SamplingOptions, Shader, TileMode,
};
use std::cell::OnceCell;
use std::rc::Rc;

/// A drawable object.
pub enum Item {
    /// A shape or text box.
    Shape(ShapeItem),
    /// A picture or movie poster.
    Image(ImageItem),
    /// A group of drawables.
    Group(GroupItem),
    /// A table (boxed: it is much larger than the other variants).
    Table(Box<TableItem>),
    /// A chart.
    Chart(ChartItem),
}

impl Item {
    /// The painted extent (what sizes a Numbers page).
    pub fn bounds(&self) -> Rect {
        match self {
            Item::Shape(shape) => shape.bounds(),
            Item::Image(image) => image.bounds(),
            Item::Group(group) => group.bounds(),
            Item::Table(table) => table.bounds(),
            Item::Chart(chart) => chart.bounds(),
        }
    }

    /// The frame text flows around or stacks inline: the geometry, without stroke or clip padding.
    pub fn flow_bounds(&self) -> Rect {
        match self {
            Item::Shape(shape) => shape.flow_bounds(),
            Item::Image(image) => image.flow_bounds(),
            other => other.bounds(),
        }
    }

    /// Draws the object in page coordinates.
    pub fn draw(&self, canvas: &Canvas) {
        match self {
            Item::Shape(shape) => shape.draw(canvas),
            Item::Image(image) => image.draw(canvas),
            Item::Group(group) => group.draw(canvas),
            Item::Table(table) => table.draw(canvas),
            Item::Chart(chart) => chart.draw(canvas),
        }
    }
}

/// The drawable for an archive object.
///
/// # Arguments
///
/// * `styles` - The document's styles
/// * `object` - A shape (2011, 3004), connection line (3009), image (3005), movie (3007), group
///   (3008), chart (5021) or table (6000)
///
/// # Returns
///
/// The drawable, or `None` for types not drawn (logged at debug level).
pub fn create(styles: &Styles, object: &IwaObject) -> Option<Item> {
    let doc = styles.doc();
    Some(match object.typ {
        6000 => {
            let table = Rc::new(NumbersTable::load(doc, &object.body).ok()?);
            Item::Table(Box::new(TableItem::new(table, styles.clone())))
        }
        2011 | 3004 => Item::Shape(ShapeItem::new(styles, object)),
        3009 => {
            // A connection line, re-routed between the shapes it joins.
            let mut line = ShapeItem::new(styles, &object.with(3004, object.body.message(1)));
            line.reroute(doc, &object.body);
            Item::Shape(line)
        }
        3005 => Item::Image(ImageItem::new(styles, object.body.clone(), false)),
        // TSD.MovieArchive: its poster frame; a live-video (camera) feed exports as nothing.
        3007 if object.body.boolean(30) => return None,
        3007 => Item::Image(ImageItem::new(styles, object.body.clone(), true)),
        3008 => Item::Group(GroupItem::new(styles, &object.body)),
        5021 => Item::Chart(ChartItem::new(styles, &object.body)),
        other => {
            log::debug!("skip drawable {} type {other}", object.id);
            return None;
        }
    })
}

/// A drawable's position, size and rotation (TSD.DrawableArchive.geometry): x, y, width, height,
/// angle.
pub fn geometry(drawable: &Proto) -> (f32, f32, f32, f32, f32) {
    let geometry = drawable.message(1);
    let (position, size) = (geometry.message(1), geometry.message(2));
    (
        position.float(1),
        position.float(2),
        size.float(1),
        size.float(2),
        geometry.float(4),
    )
}

/// The frame (position and size) of a drawable object by reference, if it has one.
pub fn frame_of(doc: &IwaDocument, id: u64) -> Option<Rect> {
    let object = doc.get(id)?;
    // The DrawableArchive is the first or second nested super (TSWP.ShapeInfo wraps TSD.Shape).
    [object.body.message(1), object.body.message(1).message(1)]
        .iter()
        .find_map(|drawable| {
            let geometry = drawable.message_opt(1)?;
            let (position, size) = (geometry.message_opt(1)?, geometry.message_opt(2)?);
            // A TSP.Point/Size holds floats (wire type 5); a reference or geometry does not.
            let floats = |message: &Proto| message.fields().iter().all(|field| field.wire == 5);
            if position.is_empty() || !floats(&position) || !floats(&size) {
                return None;
            }
            let (width, height) = (size.float(1), size.float(2));
            let rotated = rotated_bounds(
                position.float(1),
                position.float(2),
                (width, height),
                geometry.float(4),
            );
            (width > 0.0 || height > 0.0).then_some(rotated)
        })
}

/// TSD.DrawableArchive.exterior_text_wrap (field 3, a message) of a DrawableArchive (the
/// geometry's own field 3 is a varint).
pub fn wrap_in(drawable: &Proto) -> Option<Proto> {
    let has_message = |number: i32| {
        drawable
            .fields()
            .iter()
            .any(|field| field.number == number && field.wire == 2)
    };
    (has_message(3) && has_message(1) && drawable.message(1).has(2)).then(|| drawable.message(3))
}

/// The exterior text wrap of a drawable whose DrawableArchive is the first or second nested super.
pub fn wrap_of(object: &IwaObject) -> Option<Proto> {
    wrap_in(&object.body.message(1).message(1)).or_else(|| wrap_in(&object.body.message(1)))
}

/// The box around a frame rotated (counter-clockwise, degrees) about its centre.
pub fn rotated_bounds(left: f32, top: f32, (width, height): (f32, f32), angle: f32) -> Rect {
    let frame = Rect::from_xywh(left, top, width, height);
    if angle == 0.0 {
        return frame;
    }
    Matrix::rotate_deg_pivot(-angle, (left + width / 2.0, top + height / 2.0))
        .map_rect(frame)
        .0
}

/// SkiaSharp's SKRect.Empty test: all four coordinates zero (not Skia's zero-area test).
pub fn is_zero(rect: &Rect) -> bool {
    *rect == Rect::default()
}

/// SKRect.Union: no special case for empty rectangles (callers check `is_zero` where they must).
pub fn union(first: Rect, second: Rect) -> Rect {
    Rect::new(
        first.left.min(second.left),
        first.top.min(second.top),
        first.right.max(second.right),
        first.bottom.max(second.bottom),
    )
}

/// Rotates the canvas about the centre of a `width` x `height` frame at the origin.
fn rotate_about_centre(canvas: &Canvas, (width, height): (f32, f32), angle: f32) {
    if angle != 0.0 {
        canvas.translate((width / 2.0, height / 2.0));
        canvas.rotate(-angle, None);
        canvas.translate((-width / 2.0, -height / 2.0));
    }
}

/// Starts a layer drawn at `opacity` (restore with `canvas.restore()`).
fn layer_with_opacity(canvas: &Canvas, opacity: f32) {
    let mut paint = Paint::default();
    paint.set_color(Color::WHITE.with_a((opacity * 255.0) as u8));
    canvas.save_layer(&skia_safe::canvas::SaveLayerRec::default().paint(&paint));
}

fn linear_sampling() -> SamplingOptions {
    SamplingOptions::new(FilterMode::Linear, MipmapMode::Linear)
}

// ------------------------------------------------------------------ images

/// Decodes a picture: TIFF (built-in decoder), PDF (rasterised by PDFium at up to 3x on a
/// transparent background, the long side capped at 3000px), or anything Skia reads.
///
/// # Arguments
///
/// * `bytes` - The file's bytes
///
/// # Returns
///
/// A raster image, or `None` when it does not decode.
pub fn decode(bytes: &[u8]) -> Option<Image> {
    if crate::tiff::is_tiff(bytes) {
        let decoded = crate::tiff::decode(bytes)?;
        let alpha = if decoded.premultiplied {
            AlphaType::Premul
        } else {
            AlphaType::Unpremul
        };
        return raster(decoded.width, decoded.height, &decoded.rgba, alpha);
    }
    if bytes.starts_with(b"%PDF") {
        let pdfium = crate::pdfium::get()?;
        let document = pdfium.load_pdf_from_byte_slice(bytes, None).ok()?;
        let page = document.pages().get(0).ok()?;
        let longest = page.width().value.max(page.height().value);
        let dpi = (3000.0 / longest.max(1.0) * 72.0)
            .clamp(24.0, 216.0)
            .trunc();
        let (width, height, rgba) = crate::pdfium::render(&page, dpi, None).ok()?;
        return raster(width, height, &rgba, AlphaType::Unpremul);
    }
    Image::from_encoded(Data::new_copy(bytes))?.make_raster_image(None, None)
}

/// A raster image from RGBA pixels.
pub fn raster(width: usize, height: usize, rgba: &[u8], alpha: AlphaType) -> Option<Image> {
    let dimensions = (i32::try_from(width).ok()?, i32::try_from(height).ok()?);
    let info = ImageInfo::new(dimensions, ColorType::RGBA8888, alpha, None);
    images::raster_from_data(&info, Data::new_copy(rgba), width * 4)
}

/// Unpremultiplied RGBA pixels of an image: (width, height, pixels).
pub fn rgba_pixels(image: &Image) -> Option<(usize, usize, Vec<u8>)> {
    let (width, height) = (
        usize::try_from(image.width()).ok()?,
        usize::try_from(image.height()).ok()?,
    );
    let info = ImageInfo::new(
        (image.width(), image.height()),
        ColorType::RGBA8888,
        AlphaType::Unpremul,
        None,
    );
    let mut pixels = vec![0u8; width * height * 4];
    let read = image.read_pixels(
        &info,
        &mut pixels,
        width * 4,
        (0, 0),
        skia_safe::image::CachingHint::Allow,
    );
    read.then_some((width, height, pixels))
}

/// A row's opaque extent as fractions of the width (NaN for a clear row).
type RowSpan = (f32, f32);

/// Image data fields, best first: adjusted, full, original, thumbnail.
const IMAGE_SOURCES: [i32; 5] = [15, 17, 11, 13, 12];
/// A movie's poster frame (15), else its audio-only image (16).
const MOVIE_SOURCES: [i32; 2] = [15, 16];

/// TSD.ImageArchive: a placed bitmap (optionally masked), or a movie's poster frame.
pub struct ImageItem {
    styles: Styles,
    image: Proto,
    frame: Rect,
    angle: f32,
    movie: bool,
    /// Opaque extent per pixel row, computed on first use (`None`: the picture does not decode).
    rows: OnceCell<Option<Vec<RowSpan>>>,
}

impl ImageItem {
    fn new(styles: &Styles, image: Proto, movie: bool) -> ImageItem {
        let (left, top, width, height, angle) = geometry(&image.message(1));
        let frame = Rect::from_xywh(left, top, width, height);
        ImageItem {
            styles: styles.clone(),
            image,
            frame,
            angle,
            movie,
            rows: OnceCell::new(),
        }
    }

    fn size(&self) -> (f32, f32) {
        (self.frame.width(), self.frame.height())
    }

    fn bounds(&self) -> Rect {
        rotated_bounds(self.frame.left, self.frame.top, self.size(), self.angle)
    }

    /// Exterior wrap fitted to the picture's outline (exterior_text_wrap fit_type 1).
    pub fn contour_wrap(&self) -> bool {
        self.image.message(1).message(3).int(3) == 1 && !self.movie
    }

    /// The first data source that decodes (a TIFF this build cannot read falls back to the next).
    fn decode_first(&self, fields: &[i32]) -> Option<Image> {
        fields.iter().find_map(|&field| {
            let id = self.image.message(field).uint(1);
            let bytes = (id != 0)
                .then(|| self.styles.doc().read_data(id))
                .flatten()?;
            decode(&bytes)
        })
    }

    /// The horizontal extent of the picture's opaque pixels (alpha above its threshold) between two
    /// heights, in a frame of the given size.
    ///
    /// # Arguments
    ///
    /// * `band` - Top and bottom of the band, from the frame's top
    /// * `size` - The frame's width and height
    ///
    /// # Returns
    ///
    /// Left and right of the opaque pixels, or `None` when that band is transparent (the full width
    /// when the picture does not decode).
    pub fn opaque_span(&self, band: (f32, f32), (width, height): (f32, f32)) -> Option<(f32, f32)> {
        let rows = self.rows.get_or_init(|| {
            let image = self.decode_first(&IMAGE_SOURCES)?;
            let (pixel_width, pixel_height, pixels) = rgba_pixels(&image)?;
            let threshold = self.image.message(1).message(3).float_or(5, 0.5) * 255.0;
            let row_span = |row: &[u8]| -> Option<RowSpan> {
                let mut opaque = row
                    .chunks_exact(4)
                    .enumerate()
                    .filter(|(_, pixel)| f32::from(pixel[3]) > threshold);
                let (left, _) = opaque.next()?;
                let right = opaque.next_back().map_or(left, |(right, _)| right);
                Some((
                    left as f32 / pixel_width as f32,
                    (right + 1) as f32 / pixel_width as f32,
                ))
            };
            let rows = pixels.chunks_exact(pixel_width * 4).take(pixel_height);
            Some(
                rows.map(|row| row_span(row).unwrap_or((f32::NAN, f32::NAN)))
                    .collect(),
            )
        });
        let Some(rows) = rows else {
            return Some((0.0, width));
        };
        let count = rows.len() as f32;
        let first = (band.0 / height * count).floor().max(0.0) as usize;
        let last = ((band.1 / height * count).ceil() as usize).min(rows.len().saturating_sub(1));
        let spans = rows
            .get(first..=last)
            .unwrap_or_default()
            .iter()
            .filter(|span| !span.0.is_nan());
        let (left, right) = spans.fold((f32::MAX, f32::MIN), |(left, right), span| {
            (left.min(span.0), right.max(span.1))
        });
        (left <= right).then_some((left * width, right * width))
    }

    /// The mask (TSD.MaskArchive) of a picture, if any.
    fn mask(&self) -> Option<Proto> {
        if self.movie {
            None
        } else {
            self.styles.doc().body(self.image.reference(5))
        }
    }

    /// A masked image flows as its mask's frame (measured: an inline masked picture sits with the
    /// mask, not the full image, at the margin).
    fn flow_bounds(&self) -> Rect {
        let Some(mask) = self.mask() else {
            return self.bounds();
        };
        let (mask_x, mask_y, mask_width, mask_height, _) = geometry(&mask.message(1));
        if mask_width > 0.0 && mask_height > 0.0 {
            rotated_bounds(
                self.frame.left + mask_x,
                self.frame.top + mask_y,
                (mask_width, mask_height),
                self.angle,
            )
        } else {
            self.bounds()
        }
    }

    /// The mask's outline in the image's coordinates.
    fn mask_path(mask: &Proto) -> Path {
        let (mask_x, mask_y, mask_width, mask_height, _) = geometry(&mask.message(1));
        build_path(&mask.message(2), mask_width, mask_height).with_offset((mask_x, mask_y))
    }

    fn draw(&self, canvas: &Canvas) {
        let Some(image) = self.decode_first(if self.movie {
            &MOVIE_SOURCES
        } else {
            &IMAGE_SOURCES
        }) else {
            return;
        };
        let size = self.size();
        // TSD.MediaStyleArchive media_properties.opacity (image style 3, movie style 19).
        let media_style = self.image.reference(if self.movie { 19 } else { 3 });
        let opacity = self
            .styles
            .shape_prop(media_style, 2)
            .map_or(1.0, |props| props.float_or(2, 1.0));
        if opacity < 1.0 {
            layer_with_opacity(canvas, opacity);
        }
        canvas.save();
        canvas.translate((self.frame.left, self.frame.top));
        rotate_about_centre(canvas, size, self.angle);
        // TSD.GeometryArchive flags: 4 mirrors horizontally, 8 vertically.
        let flags = self.image.message(1).message(1).uint(3);
        let (mirror_x, mirror_y) = (flags & 4 != 0, flags & 8 != 0);
        if mirror_x || mirror_y {
            canvas.translate((
                if mirror_x { size.0 } else { 0.0 },
                if mirror_y { size.1 } else { 0.0 },
            ));
            canvas.scale((
                if mirror_x { -1.0 } else { 1.0 },
                if mirror_y { -1.0 } else { 1.0 },
            ));
        }
        if let Some(mask) = self.mask() {
            canvas.clip_path(&Self::mask_path(&mask), None, Some(true));
        }
        // Instant Alpha (instantAlphaPath 10): only the region inside the traced outline is kept, in
        // the image's natural-size coordinates (measured: a phone mock-up's white screen removed).
        let instant_alpha = self
            .image
            .message_opt(10)
            .filter(|path| !self.movie && !path.messages(1).is_empty());
        let natural = self.image.message(9);
        if let (Some(outline), true) = (
            instant_alpha,
            natural.float(1) > 0.0 && natural.float(2) > 0.0,
        ) {
            let mut clip = from_tsp_path(
                &outline,
                size.0 / natural.float(1),
                size.1 / natural.float(2),
            );
            clip.set_fill_type(PathFillType::EvenOdd);
            canvas.clip_path(&clip, None, Some(true));
        }
        let destination = Rect::from_wh(size.0, size.1);
        canvas.draw_image_rect_with_sampling_options(
            &image,
            None,
            destination,
            linear_sampling(),
            &Paint::default(),
        );
        canvas.restore();
        // The picture's stroke (media style 1) frames the visible part: the mask, else the frame.
        let stroke = (!self.movie)
            .then(|| self.styles.shape_prop(self.image.reference(3), 1))
            .flatten();
        if let Some(stroke) =
            stroke.and_then(|props| styles::stroke_of(Some(&props.message(1)), false))
        {
            canvas.save();
            canvas.translate((self.frame.left, self.frame.top));
            rotate_about_centre(canvas, size, self.angle);
            match self.mask() {
                Some(mask) => canvas.draw_path(&Self::mask_path(&mask), &stroke.paint(true)),
                None => canvas.draw_rect(destination, &stroke.paint(true)),
            };
            canvas.restore();
        }
        if opacity < 1.0 {
            canvas.restore();
        }
    }
}

/// TSD.GroupArchive: children are in the group's coordinate space.
pub struct GroupItem {
    children: Vec<Item>,
    frame: Rect,
    angle: f32,
}

impl GroupItem {
    fn new(styles: &Styles, group: &Proto) -> GroupItem {
        let (left, top, width, height, angle) = geometry(&group.message(1));
        let objects = group
            .references(2)
            .into_iter()
            .filter_map(|id| styles.doc().get(id));
        let children = objects
            .filter_map(|object| create(styles, object))
            .collect();
        GroupItem {
            children,
            frame: Rect::from_xywh(left, top, width, height),
            angle,
        }
    }

    fn bounds(&self) -> Rect {
        rotated_bounds(
            self.frame.left,
            self.frame.top,
            (self.frame.width(), self.frame.height()),
            self.angle,
        )
    }

    fn draw(&self, canvas: &Canvas) {
        canvas.save();
        canvas.translate((self.frame.left, self.frame.top));
        rotate_about_centre(
            canvas,
            (self.frame.width(), self.frame.height()),
            self.angle,
        );
        for child in &self.children {
            child.draw(canvas);
        }
        canvas.restore();
    }
}

// ------------------------------------------------------------------ shapes

/// An inset of TSWP.ShapeStylePropertiesArchive.padding: 4pt without a padding message; an absent
/// field of a present one is 0 (measured: `padding {}` puts PowerPoint-born text at the box edge).
pub fn padding_inset(padding: Option<&Proto>, field: i32) -> f32 {
    padding.map_or(4.0, |padding| padding.float(field))
}

/// Text frame insets.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Insets {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

/// Laid-out text of a shape: the text, the rectangle it is drawn in and its vertical alignment.
struct TextLayout {
    text: RichText,
    rect: Rect,
    vertical_align: i32,
}

/// A shape or text box (TSWP.ShapeInfoArchive / TSD.ShapeArchive) with optional text.
pub struct ShapeItem {
    styles: Styles,
    /// TSD.ShapeArchive.
    shape: Proto,
    style: u64,
    storage: u64,
    origin: Point,
    width: f32,
    height: f32,
    angle: f32,
    flip_x: bool,
    flip_y: bool,
    path: Path,
    is_text_box: bool,
    auto_width: bool,
}

impl ShapeItem {
    /// Reads a shape, text box or bare TSD.ShapeArchive (3004).
    pub fn new(styles: &Styles, object: &IwaObject) -> ShapeItem {
        let body = &object.body;
        // TSWP.ShapeInfoArchive wraps TSD.ShapeArchive in field 1; a bare TSD.ShapeArchive is 3004.
        let bare = object.typ == 3004;
        let shape = if bare { body.clone() } else { body.message(1) };
        let storage = if bare {
            0
        } else {
            text_storage(styles, object)
        };
        let style = shape.reference(2);
        let (left, mut top, width, mut height, angle) = geometry(&shape.message(1));
        // Geometry flags 0 (width not valid): the box sizes to its text, anchored at x by the
        // paragraph alignment (measured: a centred title is centred on x).
        let flags = shape.message(1).message(1);
        let auto_width = !bare && storage != 0 && flags.has(3) && flags.int(3) == 0;
        let path_source = shape.message(3);
        // A text box with a width but zero height takes its path's natural height: centred on y
        // when middle-aligned, else hanging from y.
        let natural_height = path_source
            .message_opt(5)
            .map(|bezier| bezier.message(2).float(2))
            .filter(|&natural| natural > 0.0);
        if let (true, Some(natural_height)) = (
            storage != 0 && width > 0.01 && height <= 0.01,
            natural_height,
        ) {
            height = natural_height;
            // The box grows to its text when that is taller (measured: a 262.8pt natural frame
            // holding 4 lines of 54pt text draws 354pt tall, still centred on y).
            let shrinks = styles
                .text_frame_prop(style, 1)
                .is_some_and(|props| props.boolean(1));
            if ctx::KEYNOTE.get() && !shrinks {
                let insets = text_insets(styles, style);
                let text = storage_text(styles, style, storage);
                let needed = text.block_height(width - insets.left - insets.right)
                    + insets.top
                    + insets.bottom;
                if needed > height + 0.5 {
                    height = needed;
                }
            }
            if styles
                .text_frame_prop(style, 2)
                .map_or(1, |props| props.int(2))
                == 1
            {
                top -= height / 2.0;
            }
        } else if storage != 0
            && flags.has(3)
            && (flags.int(3) & 2) == 0
            && flags.message(2).float(2) > 0.0
        {
            // A middle-aligned box whose height is not valid (geometry flags without bit 1) is
            // centred on the stored y, fill and text alike (measured: a 10-line Keynote code box
            // 383pt down spans 170 to 596; a 148pt caption box at y 669.5 fills 595.5 to 743.5).
            let default_align = if !bare && body.boolean(6) { 0 } else { 1 };
            let vertical_align = styles
                .text_frame_prop(style, 2)
                .map_or(default_align, |props| props.int(2));
            if vertical_align == 1 {
                top -= height / 2.0;
            }
        }
        ShapeItem {
            styles: styles.clone(),
            path: build_path(&path_source, width, height),
            flip_x: path_source.boolean(1),
            flip_y: path_source.boolean(2),
            shape,
            style,
            storage,
            origin: Point::new(left, top),
            width,
            height,
            angle,
            is_text_box: !bare && body.boolean(6),
            auto_width,
        }
    }

    /// Moves the shape up so that it ends at or above `bottom` (Keynote keeps a slide-number
    /// placeholder on the slide: measured, a 57.5pt box stored 26.5pt past the bottom edge draws
    /// flush with it).
    ///
    /// # Arguments
    ///
    /// * `bottom` - The lowest y the shape may reach
    pub fn keep_above(&mut self, bottom: f32) {
        // Text that grows upward from the stored y already ends there.
        if !self.text_grows_upward() {
            self.origin.y = self.origin.y.min(bottom - self.height);
        }
    }

    /// Whether the text sits above the stored position instead of in the box: a bottom-aligned
    /// box whose geometry flags lack bit 1 (height not valid) grows upward from y (measured: a
    /// Keynote slide header sits above y; a slide number stored 19pt past the slide bottom ends
    /// 10pt above it).
    fn text_grows_upward(&self) -> bool {
        let default_align = if self.is_text_box { 0 } else { 1 };
        let vertical_align = self
            .styles
            .text_frame_prop(self.style, 2)
            .map_or(default_align, |props| props.int(2));
        let flags = self.shape.message(1).message(1);
        vertical_align == 2 && flags.has(3) && (flags.int(3) & 2) == 0
    }

    /// Re-routes a connection line between the current frames of the shapes it joins
    /// (TSD.ConnectionLineArchive connected_from 2 / connected_to 3). Apple ignores the stored
    /// path's endpoints: the curve runs centre to centre, its mid-curve point keeps the stored
    /// offset from the chord (fraction along it, points across it), and it is cut at the frames
    /// (measured: endpoints within 0.4pt of Apple's on a moved diagram).
    ///
    /// # Arguments
    ///
    /// * `doc` - The document (for the joined shapes' frames)
    /// * `connection` - The TSD.ConnectionLineArchive
    pub fn reroute(&mut self, doc: &IwaDocument, connection: &Proto) {
        let Some(source) = self
            .shape
            .message(3)
            .message_opt(7)
            .filter(|_| self.angle == 0.0)
        else {
            return;
        };
        let frames = (
            frame_of(doc, connection.reference(2)),
            frame_of(doc, connection.reference(3)),
        );
        let (Some(mut from_frame), Some(mut to_frame)) = frames else {
            return;
        };
        let elements = source.message(1).message(3).messages(1);
        let stored: Vec<Point> = elements
            .iter()
            .flat_map(|element| element.messages(2))
            .map(|point| Point::new(point.float(1), point.float(2)))
            .collect();
        let [start, middle, .., end] = stored[..] else {
            return;
        };
        if stored.len() < 3 {
            return;
        }
        // The stored endpoint nearer each shape's centre (stored path in frame-local coordinates).
        let centre = |frame: Rect| Point::new(frame.center_x(), frame.center_y());
        let (mut from, mut to) = (centre(from_frame), centre(to_frame));
        if Point::distance(self.origin + start, to) < Point::distance(self.origin + start, from) {
            std::mem::swap(&mut from, &mut to);
            std::mem::swap(&mut from_frame, &mut to_frame);
        }
        let origin = self.origin;
        let route = Route {
            ends: (from, to),
            frames: (from_frame, to_frame),
            stored: (start, middle, end),
            origin,
        };
        if source.int(2) == 1 {
            self.path = route.orthogonal();
        } else if let Some(path) = route.curved() {
            self.path = path;
        }
    }

    /// Moves the shape's origin (captions are positioned relative to their object).
    pub fn move_to(&mut self, left: f32, top: f32) {
        self.origin = Point::new(left, top);
    }

    /// Height of the laid-out text plus the top and bottom padding (0 with no text).
    pub fn text_height(&self) -> f32 {
        let Some(layout) = self.text_layout() else {
            return 0.0;
        };
        let insets = text_insets(&self.styles, self.style);
        insets.top + layout.text.block_height(layout.rect.width()) + insets.bottom
    }

    /// The geometry frame, rotated.
    fn flow_bounds(&self) -> Rect {
        rotated_bounds(
            self.origin.x,
            self.origin.y,
            (self.width, self.height),
            self.angle,
        )
    }

    /// The path source's natural size, when set.
    fn natural_size(&self) -> Option<(f32, f32)> {
        let natural = self.shape.message(3).message_opt(5)?.message(2);
        (natural.float(1) > 0.0 && natural.float(2) > 0.0)
            .then(|| (natural.float(1), natural.float(2)))
    }

    /// A zero-size, auto-sizing text box.
    fn zero_size(&self) -> bool {
        self.auto_width && self.width <= 0.01 && self.height <= 0.01 && self.storage != 0
    }

    fn stroke(&self) -> Option<Stroke> {
        styles::stroke_of(
            self.styles
                .shape_prop(self.style, 2)
                .map(|props| props.message(2))
                .as_ref(),
            false,
        )
    }

    fn fill(&self) -> Option<Proto> {
        self.styles
            .shape_prop(self.style, 1)
            .map(|props| props.message(1))
    }

    /// The page reaches each drawable's clip: its painted extent plus 1pt (measured: 100pt squares
    /// clip to 102pt; a butt-capped line does not reach past its ends; a 5pt round-capped stroke
    /// reaches 2.5·√2 + 1 past the path, a mitred one its outline + 1).
    fn bounds(&self) -> Rect {
        let (left, top) = (self.origin.x, self.origin.y);
        // A zero-size text box in Numbers spans its path's natural frame from the position.
        if let (false, true, Some((natural_width, natural_height))) =
            (ctx::KEYNOTE.get(), self.zero_size(), self.natural_size())
        {
            return Rect::new(
                left - 1.0,
                top - 1.0,
                left + natural_width + 1.0,
                top + natural_height + 1.0,
            );
        }
        // A shape with neither fill nor stroke only shows its text; Numbers sizes the page by that
        // visible extent, not by the (often much wider) text box frame.
        let has_fill = self.fill().is_some_and(|fill| !fill.is_empty());
        let stroke = self.stroke();
        if !has_fill && stroke.is_none() && self.angle == 0.0 {
            if let Some(layout) = self.text_layout() {
                let text_bounds =
                    layout
                        .text
                        .text_bounds(layout.rect, layout.vertical_align, true, HAlign::Left);
                if !is_zero(&text_bounds) {
                    return text_bounds.with_offset((left, top));
                }
            }
        }
        let mut transform = Matrix::translate((left, top));
        if self.angle != 0.0 {
            transform.pre_concat(&Matrix::rotate_deg_pivot(
                -self.angle,
                (self.width / 2.0, self.height / 2.0),
            ));
        }
        if self.flip_x || self.flip_y {
            let scale = (
                if self.flip_x { -1.0 } else { 1.0 },
                if self.flip_y { -1.0 } else { 1.0 },
            );
            let mut flip = Matrix::new_identity();
            flip.set_scale(scale, Point::new(self.width / 2.0, self.height / 2.0));
            transform.pre_concat(&flip);
        }
        let (local, rotated_outline) = self.local_extent(stroke.as_ref());
        // Rotated, the stroked outline itself is measured, not its rotated box (measured: a 45°
        // line's clip reaches 1.1pt less far than the box would).
        let mut extent = match rotated_outline {
            Some(outline) => outline.with_transform(&transform).compute_tight_bounds(),
            None => transform.map_rect(local).0,
        };
        if let Some(shadow) = self.shadow_image_bounds() {
            extent = union(extent, shadow);
        }
        extent.with_outset((1.0, 1.0))
    }

    /// The painted extent in the shape's own coordinates, and for rotated stroked paths the outline
    /// to measure after rotation (path, frame and line ends included).
    fn local_extent(&self, stroke: Option<&Stroke>) -> (Rect, Option<Path>) {
        // A connection line's stored path predates any move of the shapes it joins (Apple re-routes
        // it), so its frame, not the path, is what the page measures.
        let connection = self.shape.message(3).message_opt(7).is_some();
        let frame = Rect::from_wh(self.width, self.height);
        let mut local = if self.path.is_empty() || connection {
            frame
        } else {
            self.path.compute_tight_bounds()
        };
        let Some(stroke) = stroke else {
            return (local, None);
        };
        let mut outline_paint = Paint::default();
        outline_paint.set_style(PaintStyle::Stroke);
        outline_paint.set_stroke_width(stroke.width);
        let mut outline = Path::new();
        let mut rotated = None;
        let strokable = !self.path.is_empty() && !connection;
        if stroke.cap == 1 && self.angle != 0.0 && strokable {
            outline_paint.set_stroke_cap(PaintCap::Round);
            if path_utils::fill_path_with_paint(
                &self.path,
                &outline_paint,
                &mut outline,
                None,
                None,
            ) {
                local = union(local, outline.compute_tight_bounds());
                rotated = Some(outline);
            }
        } else if stroke.cap == 1 {
            let reach = stroke.width / 2.0 * 2f32.sqrt();
            local = local.with_outset((reach, reach));
        } else {
            outline_paint.set_stroke_miter(4.0);
            if strokable
                && path_utils::fill_path_with_paint(
                    &self.path,
                    &outline_paint,
                    &mut outline,
                    None,
                    None,
                )
            {
                local = union(local, outline.compute_tight_bounds());
                if self.angle != 0.0 {
                    rotated = Some(outline);
                }
            } else {
                local = local.with_outset((stroke.width / 2.0, stroke.width / 2.0));
            }
        }
        // Arrowheads and other line ends can reach past the frame (measured: line-end documents
        // size the page by them).
        let ends = self.line_ends_bounds(stroke);
        if !is_zero(&ends) {
            local = union(local, ends);
        }
        let rotated = rotated.map(|mut outline| {
            // Everything else in `local` as a polygon, rotated with it.
            outline.add_path(&self.path, (0, 0), None);
            outline.add_rect(if connection { local } else { frame }, None);
            outline.add_path(&self.line_ends_path(stroke), (0, 0), None);
            outline
        });
        (local, rotated)
    }

    /// Contact and curved shadows paint nothing visible but export as images, and the page reaches
    /// them (measured, Numbers: a 100pt square's 28pt contact shadow is a 340x150 image centred on
    /// its bottom edge plus the offset; a 15pt curved shadow pads the frame 50pt across and 80pt
    /// along it).
    fn shadow_image_bounds(&self) -> Option<Rect> {
        if self.angle != 0.0 {
            return None;
        }
        let shadow = self.styles.shape_prop(self.style, 4)?.message(4);
        if shadow.is_empty() || !shadow.boolean_or(6, true) || !matches!(shadow.int(7), 1 | 2) {
            return None;
        }
        let (radius, offset) = (shadow.int_or(4, 1) as f32, shadow.float_or(3, 0.0));
        let (left, top) = (self.origin.x, self.origin.y);
        let (right, bottom) = (left + self.width, top + self.height);
        Some(if shadow.int(7) == 1 {
            let (across, along) = (radius * 30.0 / 7.0, radius * 75.0 / 28.0);
            Rect::new(
                left - across,
                bottom + offset - along,
                right + across,
                bottom + offset + along,
            )
        } else {
            let (across, along) = (radius * 10.0 / 3.0, radius * 16.0 / 3.0);
            Rect::new(
                left - across,
                top + offset - along,
                right + across,
                bottom + offset + along,
            )
        })
    }

    fn draw(&self, canvas: &Canvas) {
        canvas.save();
        canvas.translate(self.origin);
        rotate_about_centre(canvas, (self.width, self.height), self.angle);
        let opacity = self
            .styles
            .shape_prop(self.style, 3)
            .map_or(1.0, |props| props.float_or(3, 1.0));
        if opacity < 1.0 {
            layer_with_opacity(canvas, opacity);
        }
        self.draw_content(canvas);
        self.draw_reflection(canvas);
        if opacity < 1.0 {
            canvas.restore();
        }
        canvas.restore();
    }

    /// The shape itself (shadow, fill, stroke, text) in its own unrotated frame.
    fn draw_content(&self, canvas: &Canvas) {
        canvas.save();
        if self.flip_x || self.flip_y {
            canvas.translate((
                if self.flip_x { self.width } else { 0.0 },
                if self.flip_y { self.height } else { 0.0 },
            ));
            canvas.scale((
                if self.flip_x { -1.0 } else { 1.0 },
                if self.flip_y { -1.0 } else { 1.0 },
            ));
        }
        self.draw_shadow(canvas);
        let fill = self.fill();
        let natural_frame = self.zero_size_frame(fill.as_ref());
        let (path, frame) = match &natural_frame {
            Some((path, frame)) => (path, *frame),
            None => (&self.path, Rect::from_wh(self.width, self.height)),
        };
        paint_fill(canvas, self.styles.doc(), fill.as_ref(), path, frame);
        if let Some(stroke) = self.stroke() {
            canvas.draw_path(path, &stroke.paint(false));
            if natural_frame.is_none() {
                self.draw_line_ends(canvas, &stroke);
            }
        }
        canvas.restore();
        if let Some(layout) = self.text_layout() {
            layout.text.draw(
                canvas,
                layout.rect,
                layout.vertical_align,
                true,
                HAlign::Left,
            );
        }
    }

    /// The shape's reflection (shape style 5, TSD.ReflectionArchive opacity 1): the shape mirrored
    /// below its bottom edge, fading from that opacity to nothing over 0.4 of its height as
    /// (1 - t)^1.75 (measured: a 200pt shape at 0.5 fades out 81pt below it).
    fn draw_reflection(&self, canvas: &Canvas) {
        // An empty archive means no reflection.
        let Some(reflection) = self
            .styles
            .shape_prop(self.style, 5)
            .and_then(|props| props.message_opt(5))
            .filter(|reflection| !reflection.is_empty())
        else {
            return;
        };
        let strength = reflection.float_or(1, 0.5);
        if strength <= 0.0 || self.height <= 0.0 {
            return;
        }
        // Rendered to an image as Apple does: PDF has no destination-in blending.
        const SCALE: f32 = 2.0;
        let fade = Rect::from_xywh(0.0, self.height, self.width, self.height * 0.4);
        let size = (
            (fade.width() * SCALE).ceil() as i32,
            (fade.height() * SCALE).ceil() as i32,
        );
        let Some(mut surface) = skia_safe::surfaces::raster_n32_premul(size) else {
            return;
        };
        let raster = surface.canvas();
        raster.scale((SCALE, SCALE));
        raster.save();
        raster.translate((0.0, self.height));
        raster.scale((1.0, -1.0));
        self.draw_content(raster);
        raster.restore();
        const STEPS: usize = 8;
        let (colors, positions): (Vec<Color>, Vec<f32>) = (0..=STEPS)
            .map(|step| {
                let t = step as f32 / STEPS as f32;
                let alpha = strength * (1.0 - t).powf(1.75);
                (Color::BLACK.with_a((alpha * 255.0).round() as u8), t)
            })
            .unzip();
        let mut mask = Paint::default();
        mask.set_blend_mode(BlendMode::DstIn);
        mask.set_shader(Shader::linear_gradient(
            (Point::new(0.0, 0.0), Point::new(0.0, fade.height())),
            colors.as_slice(),
            positions.as_slice(),
            TileMode::Clamp,
            None,
            None,
        ));
        raster.draw_rect(Rect::from_wh(fade.width(), fade.height()), &mask);
        let image = surface.image_snapshot();
        canvas.draw_image_rect_with_sampling_options(
            &image,
            None,
            fade,
            linear_sampling(),
            &Paint::default(),
        );
    }

    /// A zero-size Keynote text box paints its fill and stroke where its text is laid out: hugging
    /// the text plus padding when filled (measured: white highlight boxes behind centred slogans
    /// span each text line), else in its natural frame (measured: outlined code boxes stored 0 x 0).
    fn zero_size_frame(&self, fill: Option<&Proto>) -> Option<(Path, Rect)> {
        if !ctx::KEYNOTE.get() || !self.zero_size() {
            return None;
        }
        let layout = self.text_layout()?;
        let insets = text_insets(&self.styles, self.style);
        if fill.is_some_and(|fill| !fill.is_empty()) {
            let rect = layout.rect;
            let frame = Rect::new(
                rect.left - insets.left,
                rect.top - insets.top,
                rect.right + insets.right,
                rect.bottom + insets.bottom,
            );
            let mut path = Path::new();
            path.add_rect(frame, None);
            return Some((path, frame));
        }
        let (natural_width, natural_height) = self.natural_size()?;
        let left = match layout
            .text
            .paragraphs
            .first()
            .map(|paragraph| paragraph.style.align)
        {
            Some(HAlign::Center) => -natural_width / 2.0,
            Some(HAlign::Right) => -natural_width,
            _ => 0.0,
        };
        let frame = Rect::from_xywh(
            left,
            layout.rect.top - insets.top,
            natural_width,
            natural_height,
        );
        let path = build_path(&self.shape.message(3), natural_width, natural_height)
            .with_offset((frame.left, frame.top));
        Some((path, frame))
    }

    /// Only drop shadows are exported (measured: contact and curved shadows leave no mark). The
    /// angle is measured clockwise on the page (90° casts the shadow straight down).
    fn draw_shadow(&self, canvas: &Canvas) {
        let Some(shadow) = self
            .styles
            .shape_prop(self.style, 4)
            .map(|props| props.message(4))
        else {
            return;
        };
        if shadow.is_empty() || !shadow.boolean_or(6, true) || !shadow.has(1) || shadow.int(7) != 0
        {
            return;
        }
        let color = styles::color(&shadow.message(1));
        let angle = f64::from(shadow.float_or(2, 315.0)).to_radians();
        let offset = f64::from(shadow.float_or(3, 5.0));
        let radius = shadow.int_or(4, 1) as f32;
        let mut paint = styles::fill_paint(
            color.with_a((f32::from(color.a()) * shadow.float_or(5, 1.0)) as u8),
        );
        if radius > 0.0 {
            paint.set_mask_filter(skia_safe::MaskFilter::blur(
                skia_safe::BlurStyle::Normal,
                radius / 2.0,
                None,
            ));
        }
        // The shadow follows what is painted: with no visible fill only the stroke casts one, and a
        // translucent colour fill casts a proportionally fainter one.
        let fill = self.fill();
        let fill_color = fill
            .as_ref()
            .and_then(|fill| fill.message_opt(1))
            .map(|solid| styles::color(&solid));
        let filled = fill.as_ref().is_some_and(|fill| !fill.is_empty())
            && fill_color.is_none_or(|color| color.a() != 0);
        if let Some(alpha) = fill_color
            .map(|color| color.a())
            .filter(|&alpha| alpha < 255)
        {
            let current = paint.color();
            paint
                .set_color(current.with_a((u32::from(current.a()) * u32::from(alpha) / 255) as u8));
        }
        if !filled {
            let Some(stroke) = self.stroke() else { return };
            paint.set_style(PaintStyle::Stroke);
            paint.set_stroke_width(stroke.width);
        }
        canvas.save();
        canvas.translate(((offset * angle.cos()) as f32, (offset * angle.sin()) as f32));
        canvas.draw_path(&self.path, &paint);
        canvas.restore();
    }

    /// Where the shape's text goes, or `None` when it has none.
    fn text_layout(&self) -> Option<TextLayout> {
        if self.storage == 0 {
            return None;
        }
        let styles = &self.styles;
        let insets = text_insets(styles, self.style);
        let default_align = if self.is_text_box { 0 } else { 1 };
        let vertical_align = styles
            .text_frame_prop(self.style, 2)
            .map_or(default_align, |props| props.int(2));
        let mut text = storage_text(styles, self.style, self.storage);
        if text
            .paragraphs
            .iter()
            .all(|paragraph| paragraph.runs.is_empty())
        {
            return None;
        }
        let align = text.paragraphs[0].style.align;
        let (width, height) = (self.width, self.height);
        if self.auto_width && width <= 0.01 && height <= 0.01 {
            return Some(self.zero_size_layout(text, insets, vertical_align));
        }
        let grows_upward = self.text_grows_upward();
        if self.auto_width {
            let shift = match align {
                HAlign::Center => width / 2.0,
                HAlign::Right => width,
                _ => 0.0,
            };
            let (top, bottom) = if grows_upward {
                (insets.top - height, -insets.bottom)
            } else {
                (insets.top, height - insets.bottom)
            };
            let rect = Rect::new(
                insets.left - shift,
                top,
                width - insets.right - shift,
                bottom,
            );
            return Some(TextLayout {
                text,
                rect,
                vertical_align,
            });
        }
        let mut rect = Rect::new(
            insets.left,
            insets.top,
            width - insets.right,
            height - insets.bottom,
        );
        let flags = self.shape.message(1).message(1);
        if grows_upward {
            rect = Rect::new(
                insets.left,
                insets.top - height,
                width - insets.right,
                -insets.bottom,
            );
        }
        // Shrink text to fit (shape style field 1): font sizes step down by 1% until the text fits
        // the box (measured: a 62pt Keynote title drawn at 61.38pt).
        let shrink = styles
            .text_frame_prop(self.style, 1)
            .is_some_and(|props| props.boolean(1));
        if shrink && rect.height() > 0.0 && text.block_height(rect.width()) > rect.height() {
            let fits = |percent: i32| {
                text.scaled(percent as f32 / 100.0)
                    .block_height(rect.width())
                    <= rect.height()
            };
            let percent = (11..=99).rev().find(|&percent| fits(percent)).unwrap_or(10);
            text = text.scaled(percent as f32 / 100.0);
        }
        // Keynote (and Pages) do not export lines that overflow a sized text box.
        let height_valid = flags.has(3) && (flags.int(3) & 2) != 0;
        if (ctx::KEYNOTE.get() || ctx::GROW_ROWS.get()) && height_valid && rect.height() > 0.0 {
            text.clip_overflow = true;
        }
        Some(TextLayout {
            text,
            rect,
            vertical_align,
        })
    }

    /// A zero-size auto-sizing text box: the text is not wrapped and sits around the anchor.
    fn zero_size_layout(&self, text: RichText, insets: Insets, vertical_align: i32) -> TextLayout {
        let align = text.paragraphs[0].style.align;
        let unwrapped =
            text.text_bounds(Rect::from_wh(100_000.0, 100_000.0), 0, false, HAlign::Left);
        let (text_width, text_height) = (unwrapped.width() + 0.5, unwrapped.bottom);
        // With a path natural size, the text sits in that frame: aligned within its width, at its top
        // plus padding, unwrapped (measured: a 596.8pt-wide heading and 56pt menu labels in a web
        // mock-up, both within 0.4pt). The anchor is the frame's left edge, centre or right edge by
        // alignment; Keynote places the frame around y by its vertical alignment, Numbers hangs it
        // from y.
        if let Some((natural_width, natural_height)) = self.natural_size() {
            let left = match align {
                HAlign::Center => (natural_width - text_width) / 2.0 - natural_width / 2.0,
                HAlign::Right => natural_width - insets.right - text_width - natural_width,
                _ => insets.left,
            };
            let mut top = match (ctx::KEYNOTE.get(), vertical_align) {
                (false, _) | (true, 0) => insets.top,
                (true, 2) => -natural_height + insets.top,
                _ => -natural_height / 2.0 + insets.top,
            };
            // Middle-aligned text taller than the frame centres on y.
            if vertical_align == 1 && text_height > natural_height {
                top = -text_height / 2.0;
            }
            return TextLayout {
                text,
                rect: Rect::from_xywh(left, top, text_width, text_height),
                vertical_align: 0,
            };
        }
        // Wider than the slide, it wraps at the slide width less padding (measured: a 108pt slogan
        // 1570pt long breaks after 1143pt on a 1280pt slide).
        let slide_width = ctx::SLIDE_WIDTH.get();
        if slide_width > 0.0 && text_width > slide_width - insets.left - insets.right {
            let column = slide_width - insets.left - insets.right;
            let wrapped_height = text
                .text_bounds(Rect::from_wh(column, 100_000.0), 0, true, HAlign::Left)
                .bottom;
            let left = match align {
                HAlign::Center => -column / 2.0,
                HAlign::Right => -column - insets.right,
                _ => insets.left,
            };
            let top = match vertical_align {
                0 => insets.top,
                2 => -wrapped_height - insets.bottom,
                _ => -wrapped_height / 2.0,
            };
            return TextLayout {
                text,
                rect: Rect::from_xywh(left, top, column, wrapped_height),
                vertical_align: 0,
            };
        }
        // Without a natural frame the anchor x is the text's left edge (plus padding), centre or
        // right edge by alignment; vertically the text centres on y.
        let left = match align {
            HAlign::Center => -text_width / 2.0,
            HAlign::Right => -text_width - insets.right,
            _ => insets.left,
        };
        TextLayout {
            text,
            rect: Rect::from_xywh(left, -text_height / 2.0, text_width, text_height),
            vertical_align: 0,
        }
    }
}

/// The storage a text box shows: its own (4, else 2), or the storage of the text flow (3) it is
/// the first box of (measured: a linked label box). Overflow into later boxes is not laid out.
fn text_storage(styles: &Styles, object: &IwaObject) -> u64 {
    let body = &object.body;
    let own = [body.reference(4), body.reference(2)]
        .into_iter()
        .find(|&id| id != 0)
        .unwrap_or(0);
    let first_box =
        |flow: &Proto| flow.reference(1) != 0 && flow.references(2).first() == Some(&object.id);
    let flow = styles.doc().body(body.reference(3)).filter(first_box);
    flow.map_or(own, |flow| flow.reference(1))
}

/// A text frame's padding.
fn text_insets(styles: &Styles, style: u64) -> Insets {
    let padding = styles
        .text_frame_prop(style, 6)
        .map(|props| props.message(6));
    let inset = |field: i32| padding_inset(padding.as_ref(), field);
    Insets {
        left: inset(1),
        top: inset(2),
        right: inset(3),
        bottom: inset(4),
    }
}

/// A shape's text, over its default paragraph style (text frame property 10).
fn storage_text(styles: &Styles, style: u64, storage: u64) -> RichText {
    let paragraph_style = styles
        .text_frame_prop(style, 10)
        .map_or(0, |props| props.reference(10));
    let base = if paragraph_style != 0 {
        TextStyle::resolve(styles, paragraph_style, 0, None)
    } else {
        TextStyle::default()
    };
    RichText::from_storage(styles, storage, &base, paragraph_style)
}

/// A connection line being re-routed: the joined shapes' centres and frames, the stored path's
/// start, middle and end points (frame-local), and the line's origin.
struct Route {
    ends: (Point, Point),
    frames: (Rect, Rect),
    stored: (Point, Point, Point),
    origin: Point,
}

impl Route {
    /// An orthogonal connection: centre to centre in three legs, the middle leg where the stored
    /// corner point puts it (as a fraction between the endpoints), cut at both frames.
    fn orthogonal(&self) -> Path {
        let ((from, to), (from_frame, to_frame), (start, corner, end)) =
            (self.ends, self.frames, self.stored);
        let fraction = |begin: f32, middle: f32, finish: f32| {
            if (finish - begin).abs() > 0.01 {
                (middle - begin) / (finish - begin)
            } else {
                0.5
            }
        };
        let local = |point_x: f32, point_y: f32| Point::new(point_x, point_y) - self.origin;
        let mut path = Path::new();
        if (to.y - from.y).abs() >= (to.x - from.x).abs() {
            let middle_y = from.y + fraction(start.y, corner.y, end.y) * (to.y - from.y);
            let (leave_y, enter_y) = if to.y > from.y {
                (from_frame.bottom, to_frame.top)
            } else {
                (from_frame.top, to_frame.bottom)
            };
            path.move_to(local(from.x, leave_y))
                .line_to(local(from.x, middle_y))
                .line_to(local(to.x, middle_y))
                .line_to(local(to.x, enter_y));
        } else {
            let middle_x = from.x + fraction(start.x, corner.x, end.x) * (to.x - from.x);
            let (leave_x, enter_x) = if to.x > from.x {
                (from_frame.right, to_frame.left)
            } else {
                (from_frame.left, to_frame.right)
            };
            path.move_to(local(leave_x, from.y))
                .line_to(local(middle_x, from.y))
                .line_to(local(middle_x, to.y))
                .line_to(local(enter_x, to.y));
        }
        path
    }

    /// A quadratic connection: centre to centre, its mid-curve point keeping the stored offset from
    /// the chord, cut where it leaves each frame.
    fn curved(&self) -> Option<Path> {
        let ((from, to), (from_frame, to_frame), (start, through, end)) =
            (self.ends, self.frames, self.stored);
        let stored_chord = end - start;
        let stored_length = stored_chord.length();
        let chord = to - from;
        let length = chord.length();
        if stored_length < 0.01 || length < 0.01 {
            return None;
        }
        let offset = through - Point::new((start.x + end.x) / 2.0, (start.y + end.y) / 2.0);
        let along = (offset.x * stored_chord.x + offset.y * stored_chord.y)
            / (stored_length * stored_length);
        let across = (stored_chord.x * offset.y - stored_chord.y * offset.x) / stored_length;
        let perpendicular = Point::new(-chord.y / length, chord.x / length);
        let middle = Point::new((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
        let on_curve = middle
            + Point::new(
                along * chord.x + across * perpendicular.x,
                along * chord.y + across * perpendicular.y,
            );
        let control = Point::new(2.0 * on_curve.x - middle.x, 2.0 * on_curve.y - middle.y);
        let bezier = |progress: f32| {
            let (start_weight, control_weight, end_weight) = (
                (1.0 - progress) * (1.0 - progress),
                2.0 * progress * (1.0 - progress),
                progress * progress,
            );
            Point::new(
                start_weight * from.x + control_weight * control.x + end_weight * to.x,
                start_weight * from.y + control_weight * control.y + end_weight * to.y,
            )
        };
        // First parameter (from the start, or from the end) where the curve leaves a frame.
        let exit = |frame: Rect, from_start: bool| {
            const STEPS: u16 = 2000;
            let inside = |point: Point| {
                point.x >= frame.left
                    && point.x < frame.right
                    && point.y >= frame.top
                    && point.y < frame.bottom
            };
            let parameter = |step: u16| {
                if from_start {
                    f32::from(step) / f32::from(STEPS)
                } else {
                    1.0 - f32::from(step) / f32::from(STEPS)
                }
            };
            (0..=STEPS)
                .map(parameter)
                .find(|&progress| !inside(bezier(progress)))
                .unwrap_or(if from_start { 0.0 } else { 1.0 })
        };
        let (first, last) = (exit(from_frame, true), exit(to_frame, false));
        if last <= first {
            return None;
        }
        // The sub-curve between the two parameters (its control point by blossoming).
        let (start_weight, control_weight, end_weight) = (
            (1.0 - first) * (1.0 - last),
            first * (1.0 - last) + last * (1.0 - first),
            first * last,
        );
        let sub_control = Point::new(
            start_weight * from.x + control_weight * control.x + end_weight * to.x,
            start_weight * from.y + control_weight * control.y + end_weight * to.y,
        );
        let mut path = Path::new();
        path.move_to(bezier(first) - self.origin)
            .quad_to(sub_control - self.origin, bezier(last) - self.origin);
        Some(path)
    }
}

// ------------------------------------------------------------------ paths

/// The outline of a TSD path source, scaled to the drawable's size.
///
/// # Arguments
///
/// * `source` - The TSD.PathSourceArchive
/// * `width` - The drawable's width
/// * `height` - The drawable's height
///
/// # Returns
///
/// The path in the drawable's coordinates (a rectangle for unknown sources).
pub fn build_path(source: &Proto, width: f32, height: f32) -> Path {
    if let Some(bezier) = source.message_opt(5) {
        return bezier_path(&bezier, width, height);
    }
    if let Some(connection) = source.message_opt(7) {
        return bezier_path(&connection.message(1), width, height);
    }
    if let Some(scalar) = source.message_opt(4) {
        return scalar_path(&scalar, width, height);
    }
    if let Some(point) = source.message_opt(3) {
        return point_path(&point, width, height);
    }
    if let Some(editable) = source.message_opt(8) {
        return editable_path(&editable, width, height);
    }
    if let Some(callout) = source.message_opt(6) {
        return callout_path(&callout, width, height);
    }
    let mut path = Path::new();
    path.add_rect(Rect::from_wh(width, height), None);
    path
}

/// A callout: its box (corner radius 4; past half the short side an ellipse) joined to a tail
/// (tip 2, in natural units of 1, its base 2 x field 3 wide). The base is centred on the nearest
/// edge level with the tip, or with field 5 (tail at centre) lies between two lines parallel to
/// the centre-to-tip direction (measured: Keynote's rectangle and oval callouts).
fn callout_path(callout: &Proto, width: f32, height: f32) -> Path {
    let frame = Rect::from_wh(width, height);
    let radius = callout.float(4);
    let oval = radius > width.min(height) / 2.0;
    let mut body = Path::new();
    if oval {
        body.add_oval(frame, None);
    } else {
        body.add_round_rect(frame, (radius, radius), None);
    }
    let (scale_x, scale_y) = natural_scale(&callout.message(1), width, height);
    let tip_point = callout.message(2);
    let tip = Point::new(tip_point.float(1) * scale_x, tip_point.float(2) * scale_y);
    let half_base = callout.float(3);
    let base = if callout.boolean(5) {
        centered_tail_base(frame, oval, tip, half_base)
    } else {
        box_tail_base(frame, tip, half_base)
    };
    let Some((first, second)) = base else {
        return body;
    };
    body.op(&polygon([tip, first, second]), PathOp::Union)
        .unwrap_or(body)
}

/// The tail base on a box's edge nearest a tip outside it, level with the tip.
fn box_tail_base(frame: Rect, tip: Point, half: f32) -> Option<(Point, Point)> {
    let along_y = |x: f32| {
        let y = tip.y.clamp(frame.top + half, frame.bottom - half);
        (Point::new(x, y - half), Point::new(x, y + half))
    };
    let along_x = |y: f32| {
        let x = tip.x.clamp(frame.left + half, frame.right - half);
        (Point::new(x - half, y), Point::new(x + half, y))
    };
    if tip.x < frame.left {
        Some(along_y(frame.left))
    } else if tip.x > frame.right {
        Some(along_y(frame.right))
    } else if tip.y < frame.top {
        Some(along_x(frame.top))
    } else if tip.y > frame.bottom {
        Some(along_x(frame.bottom))
    } else {
        None
    }
}

/// The tail base where the two lines `half` either side of the centre-to-tip line, running
/// towards the tip, leave the body (an ellipse, or the box ignoring its rounded corners).
fn centered_tail_base(frame: Rect, oval: bool, tip: Point, half: f32) -> Option<(Point, Point)> {
    let center = frame.center();
    let (radius_x, radius_y) = (frame.width() / 2.0, frame.height() / 2.0);
    let to_tip = tip - center;
    let length = to_tip.length();
    if length <= 0.0 || radius_x <= 0.0 || radius_y <= 0.0 {
        return None;
    }
    let direction = Point::new(to_tip.x / length, to_tip.y / length);
    let normal = Point::new(-direction.y, direction.x);
    // The distance along `direction` from `start` (relative to the centre) to the body's edge.
    let exit_distance = |start: Point| -> Option<f32> {
        if oval {
            // Solve |(start + t * direction) / radii| = 1 for the larger t.
            let (dx, dy) = (direction.x / radius_x, direction.y / radius_y);
            let (sx, sy) = (start.x / radius_x, start.y / radius_y);
            let a = dx * dx + dy * dy;
            let b = 2.0 * (dx * sx + dy * sy);
            let discriminant = b * b - 4.0 * a * (sx * sx + sy * sy - 1.0);
            (discriminant >= 0.0).then(|| (-b + discriminant.sqrt()) / (2.0 * a))
        } else {
            // The nearest of the box sides ahead of the line.
            let side = |start: f32, step: f32, radius: f32| {
                (step.abs() > f32::EPSILON).then(|| (radius.copysign(step) - start) / step)
            };
            [
                side(start.x, direction.x, radius_x),
                side(start.y, direction.y, radius_y),
            ]
            .into_iter()
            .flatten()
            .reduce(f32::min)
        }
    };
    let exit = |offset: f32| {
        let start = Point::new(normal.x * offset, normal.y * offset);
        exit_distance(start).map(|t| center + start + Point::new(direction.x * t, direction.y * t))
    };
    Some((exit(half)?, exit(-half)?))
}

fn natural_scale(natural: &Proto, width: f32, height: f32) -> (f32, f32) {
    let scale = |size: f32, natural: f32| if natural > 0.0 { size / natural } else { 1.0 };
    (
        scale(width, natural.float(1)),
        scale(height, natural.float(2)),
    )
}

/// A TSP.Path (elements: 1 move, 2 line, 3 quad, 4 cubic, 5 close), scaled.
pub fn from_tsp_path(path: &Proto, scale_x: f32, scale_y: f32) -> Path {
    let mut output = Path::new();
    for element in path.messages(1) {
        let points: Vec<Point> = element
            .messages(2)
            .iter()
            .map(|point| Point::new(point.float(1) * scale_x, point.float(2) * scale_y))
            .collect();
        match (element.int(1), points.as_slice()) {
            (1, [to, ..]) => {
                output.move_to(*to);
            }
            (2, [to, ..]) => {
                output.line_to(*to);
            }
            (3, [control, to, ..]) => {
                output.quad_to(*control, *to);
            }
            (4, [first, second, to, ..]) => {
                output.cubic_to(*first, *second, *to);
            }
            (5, _) => {
                output.close();
            }
            _ => {}
        }
    }
    output
}

/// A bezier path source. A path whose own extent (the curve's, not its control points') differs
/// from its natural size is scaled from that extent (measured: a 141.42pt line path with a 39.92pt
/// natural size draws 39.92pt long; an ellipse whose control points reach 82pt outside its frame
/// draws at its size).
fn bezier_path(bezier: &Proto, width: f32, height: f32) -> Path {
    let natural = bezier.message(2);
    let (mut scale_x, mut scale_y) = natural_scale(&natural, width, height);
    let extent = from_tsp_path(&bezier.message(3), 1.0, 1.0).compute_tight_bounds();
    let differs = |extent: f32, natural: f32| {
        extent > 0.01 && (extent - natural).abs() > 0.5f32.max(0.01 * natural)
    };
    if differs(extent.width(), natural.float(1)) {
        scale_x = width / extent.width();
    }
    if differs(extent.height(), natural.float(2)) {
        scale_y = height / extent.height();
    }
    from_tsp_path(&bezier.message(3), scale_x, scale_y)
}

/// A closed polygon through points.
fn polygon(points: impl IntoIterator<Item = Point>) -> Path {
    let mut path = Path::new();
    for (index, point) in points.into_iter().enumerate() {
        if index == 0 {
            path.move_to(point);
        } else {
            path.line_to(point);
        }
    }
    path.close();
    path
}

/// A point on the ellipse inscribed in a `width` x `height` box, `reach` of the way out.
fn on_ellipse(angle: f64, (width, height): (f32, f32), reach: f32) -> Point {
    Point::new(
        width / 2.0 + angle.cos() as f32 * width / 2.0 * reach,
        height / 2.0 + angle.sin() as f32 * height / 2.0 * reach,
    )
}

/// Scalar path sources: 0 rounded rectangle (corner radius), 1 regular polygon (sides), others a
/// chevron (notch depth).
fn scalar_path(scalar: &Proto, width: f32, height: f32) -> Path {
    let value = scalar.float(2);
    let frame = Rect::from_wh(width, height);
    match scalar.int(1) {
        0 => {
            let radius = value.min(width.min(height) / 2.0);
            let mut path = Path::new();
            if radius <= 0.0 {
                path.add_rect(frame, None);
            } else {
                path.add_round_rect(frame, (radius, radius), None);
            }
            path
        }
        1 => {
            let sides = (value.round() as i32).max(3);
            let angle = |side: i32| {
                -std::f64::consts::FRAC_PI_2
                    + 2.0 * std::f64::consts::PI * f64::from(side) / f64::from(sides)
            };
            polygon((0..sides).map(|side| on_ellipse(angle(side), (width, height), 1.0)))
        }
        _ => polygon(
            [
                (0.0, 0.0),
                (width - value, 0.0),
                (width, height / 2.0),
                (width - value, height),
                (0.0, height),
                (value, height / 2.0),
            ]
            .map(Point::from),
        ),
    }
}

/// Point path sources: 0/1 left/right arrow, 10 double arrow, 100 star, others a plus.
fn point_path(source: &Proto, width: f32, height: f32) -> Path {
    let point = source.message(2);
    let (point_x, point_y) = (point.float(1), point.float(2));
    let (scale_x, scale_y) = natural_scale(&source.message(3), width, height);
    // point.y: the shaft's top edge as a fraction of the height (else in natural units), the
    // shaft symmetric about the middle; point.x: the head's length from its tip (measured:
    // arrows with point (64, 0.34) and (68.88, 0.25)).
    let shaft_top = if point_y <= 1.0 {
        point_y * height
    } else {
        point_y * scale_y
    };
    let shaft_bottom = height - shaft_top;
    let head = point_x * scale_x;
    let mid = height / 2.0;
    let points: Vec<(f32, f32)> = match source.int(1) {
        1 => {
            let head_x = width - head;
            vec![
                (0.0, shaft_top),
                (head_x, shaft_top),
                (head_x, 0.0),
                (width, mid),
                (head_x, height),
                (head_x, shaft_bottom),
                (0.0, shaft_bottom),
            ]
        }
        0 => vec![
            (width, shaft_top),
            (head, shaft_top),
            (head, 0.0),
            (0.0, mid),
            (head, height),
            (head, shaft_bottom),
            (width, shaft_bottom),
        ],
        10 => {
            let far = width - head;
            vec![
                (0.0, mid),
                (head, 0.0),
                (head, shaft_top),
                (far, shaft_top),
                (far, 0.0),
                (width, mid),
                (far, height),
                (far, shaft_bottom),
                (head, shaft_bottom),
                (head, height),
            ]
        }
        100 => {
            // point.x: number of points, point.y: inner radius ratio.
            let tips = (point_x.round() as i32).max(3);
            let angle = |corner: i32| {
                -std::f64::consts::FRAC_PI_2
                    + std::f64::consts::PI * f64::from(corner) / f64::from(tips)
            };
            return polygon((0..2 * tips).map(|corner| {
                on_ellipse(
                    angle(corner),
                    (width, height),
                    if corner % 2 == 0 { 1.0 } else { point_y },
                )
            }));
        }
        _ => {
            // plus: point = arm thickness
            let (arm_x, arm_y) = (point_x * scale_x, point_y * scale_y);
            let (right, bottom) = (width - arm_x, height - arm_y);
            vec![
                (arm_x, 0.0),
                (right, 0.0),
                (right, arm_y),
                (width, arm_y),
                (width, bottom),
                (right, bottom),
                (right, height),
                (arm_x, height),
                (arm_x, bottom),
                (0.0, bottom),
                (0.0, arm_y),
                (arm_x, arm_y),
            ]
        }
    };
    polygon(points.into_iter().map(Point::from))
}

/// Editable bezier sources: sub-paths of nodes (in-control 1, point 2, out-control 3), closed when
/// flagged.
fn editable_path(editable: &Proto, width: f32, height: f32) -> Path {
    let (scale_x, scale_y) = natural_scale(&editable.message(2), width, height);
    let scaled = |point: Proto| Point::new(point.float(1) * scale_x, point.float(2) * scale_y);
    let mut path = Path::new();
    for sub_path in editable.messages(1) {
        let nodes = sub_path.messages(1);
        let (Some(first), Some(last)) = (nodes.first(), nodes.last()) else {
            continue;
        };
        path.move_to(scaled(first.message(2)));
        for pair in nodes.windows(2) {
            path.cubic_to(
                scaled(pair[0].message(3)),
                scaled(pair[1].message(1)),
                scaled(pair[1].message(2)),
            );
        }
        if sub_path.boolean(2) {
            path.cubic_to(
                scaled(last.message(3)),
                scaled(first.message(1)),
                scaled(first.message(2)),
            );
            path.close();
        }
    }
    path
}

// ------------------------------------------------------------------ line ends

/// The line end's transform: its own coordinate system points along -x with end_point at the tip.
fn end_matrix(end: &Proto, (tip, from): (Point, Point), width: f32) -> Matrix {
    let end_point = end.message(3);
    let angle = (tip.y - from.y).atan2(tip.x - from.x).to_degrees();
    let mut matrix = Matrix::translate((tip.x, tip.y));
    matrix.pre_concat(&Matrix::rotate_deg(angle));
    matrix.pre_concat(&Matrix::scale((width, width)));
    matrix.pre_concat(&Matrix::scale((-1.0, 1.0)));
    matrix.pre_concat(&Matrix::translate((
        -end_point.float(1),
        -end_point.float(2),
    )));
    matrix
}

/// Arrowheads and other line ends of open paths.
impl ShapeItem {
    /// Calls `visit(end, (tip, from))` for the tail end, then the head end, of the shape's path.
    fn each_end(&self, mut visit: impl FnMut(&Proto, (Point, Point))) {
        let (styles, style) = (&self.styles, self.style);
        let head = self
            .shape
            .message_opt(4)
            .or_else(|| styles.shape_prop(style, 6).map(|props| props.message(6)));
        let tail = self
            .shape
            .message_opt(5)
            .or_else(|| styles.shape_prop(style, 7).map(|props| props.message(7)));
        let mut points = vec![Point::default(); self.path.count_points()];
        self.path.get_points(&mut points);
        let ([first, second, ..], [.., before_last, last]) = (&points[..], &points[..]) else {
            return;
        };
        if let Some(tail) = tail.filter(|tail| tail.has(1)) {
            visit(&tail, (*last, *before_last));
        }
        if let Some(head) = head.filter(|head| head.has(1)) {
            visit(&head, (*first, *second));
        }
    }

    fn draw_line_ends(&self, canvas: &Canvas, stroke: &Stroke) {
        self.each_end(|end, segment| {
            let shape = from_tsp_path(&end.message(1), 1.0, 1.0);
            canvas.save();
            canvas.concat(&end_matrix(end, segment, stroke.width));
            let mut paint = styles::fill_paint(stroke.color);
            // A filled end (4), else stroked one unit (one stroke width) wide.
            paint.set_style(if end.boolean(4) {
                PaintStyle::Fill
            } else {
                PaintStyle::Stroke
            });
            paint.set_stroke_width(1.0);
            canvas.draw_path(&shape, &paint);
            canvas.restore();
        });
    }

    /// Extent of the line ends in the shape's own coordinates (zero when none).
    fn line_ends_bounds(&self, stroke: &Stroke) -> Rect {
        let mut extent = Rect::default();
        self.each_end(|end, segment| {
            let shape = from_tsp_path(&end.message(1), 1.0, 1.0);
            if shape.is_empty() {
                return;
            }
            let mut bounds = end_matrix(end, segment, stroke.width)
                .map_rect(*shape.bounds())
                .0;
            if !end.boolean(4) {
                bounds = bounds.with_outset((stroke.width / 2.0, stroke.width / 2.0));
            }
            extent = if is_zero(&extent) {
                bounds
            } else {
                union(extent, bounds)
            };
        });
        extent
    }

    /// The line ends' painted outlines in the shape's own coordinates (for rotated bounds).
    fn line_ends_path(&self, stroke: &Stroke) -> Path {
        let mut outlines = Path::new();
        self.each_end(|end, segment| {
            let shape = from_tsp_path(&end.message(1), 1.0, 1.0);
            if shape.is_empty() {
                return;
            }
            let mapped = shape.with_transform(&end_matrix(end, segment, stroke.width));
            if end.boolean(4) {
                outlines.add_path(&mapped, (0, 0), None);
                return;
            }
            let mut paint = Paint::default();
            paint.set_style(PaintStyle::Stroke);
            paint.set_stroke_width(stroke.width);
            let mut stroked = Path::new();
            let outlined =
                path_utils::fill_path_with_paint(&mapped, &paint, &mut stroked, None, None);
            outlines.add_path(if outlined { &stroked } else { &mapped }, (0, 0), None);
        });
        outlines
    }
}

// ------------------------------------------------------------------ fills

/// The data identifier of an ImageFillArchive's picture (6, else 7).
fn fill_image_id(image: &Proto) -> u64 {
    [image.message(6).uint(1), image.message(7).uint(1)]
        .into_iter()
        .find(|&id| id != 0)
        .unwrap_or(0)
}

/// A theme picture's colour ramp: `(position, [red, green, blue])` stops from top to bottom.
type ThemeRamp = &'static [(f32, [u8; 3])];

/// Keynote theme pictures a package references only by locator (the image lives inside
/// Keynote.app), keyed by locator without its size suffix ("gradient/gradient_10x7" ->
/// "gradient/gradient"). Each is a top-to-bottom ramp measured from Apple's exports.
const THEME_PICTURES: &[(&str, ThemeRamp)] = &[
    // A 10x7 picture stretched over the slide: near black down to slate.
    (
        "gradient/gradient",
        &[
            (0.0, [3, 3, 3]),
            (0.1667, [4, 3, 6]),
            (0.3333, [6, 5, 33]),
            (0.4167, [18, 18, 49]),
            (0.5833, [54, 54, 82]),
            (0.75, [85, 85, 109]),
            (1.0, [115, 113, 134]),
        ],
    ),
    // Chipboard texture: its grain stays within the comparison tolerance of the average.
    (
        "new_template5/chipboard",
        &[(0.0, [184, 172, 158]), (1.0, [175, 163, 148])],
    ),
];

/// Looks up a theme picture locator ("gradient/gradient_10x7") in `THEME_PICTURES`.
fn ramp_for_locator(locator: &str) -> Option<ThemeRamp> {
    let stem = locator.rsplit_once('_').map_or(locator, |(stem, _)| stem);
    THEME_PICTURES
        .iter()
        .find(|(name, _)| *name == stem)
        .map(|&(_, ramp)| ramp)
}

/// The built-in ramp for an ImageFillArchive whose picture is a known Keynote theme resource.
fn theme_ramp(doc: &IwaDocument, image: &Proto) -> Option<ThemeRamp> {
    ramp_for_locator(&doc.data_locator(fill_image_id(image))?)
}

/// Whether `paint_fill` can paint an ImageFillArchive: its picture is in the package and
/// decodes, or it is a known Keynote theme picture.
pub fn image_available(doc: &IwaDocument, image: &Proto) -> bool {
    doc.read_data(fill_image_id(image))
        .and_then(|bytes| decode(&bytes))
        .is_some()
        || theme_ramp(doc, image).is_some()
}

/// Paints a theme ramp top to bottom over `area`.
fn paint_ramp(canvas: &Canvas, ramp: ThemeRamp, area: Rect) {
    let colors: Vec<Color> = ramp
        .iter()
        .map(|&(_, [red, green, blue])| Color::from_rgb(red, green, blue))
        .collect();
    let positions: Vec<f32> = ramp.iter().map(|&(position, _)| position).collect();
    let mut paint = Paint::default();
    paint.set_shader(Shader::linear_gradient(
        (
            Point::new(area.left, area.top),
            Point::new(area.left, area.bottom),
        ),
        colors.as_slice(),
        Some(positions.as_slice()),
        TileMode::Clamp,
        None,
        None,
    ));
    canvas.draw_rect(area, &paint);
}

/// Paints a TSD.FillArchive: solid colour, gradient or image.
///
/// # Arguments
///
/// * `canvas` - The canvas
/// * `doc` - The document (for image data)
/// * `fill` - The fill (nothing is painted for `None` or an empty fill)
/// * `path` - The area to fill
/// * `bounds` - The box gradients and images are laid out in
pub fn paint_fill(
    canvas: &Canvas,
    doc: &IwaDocument,
    fill: Option<&Proto>,
    path: &Path,
    bounds: Rect,
) {
    let Some(fill) = fill.filter(|fill| !fill.is_empty()) else {
        return;
    };
    if let Some(color) = fill.message_opt(1) {
        canvas.draw_path(path, &styles::fill_paint(styles::color(&color)));
    } else if let Some(gradient) = fill.message_opt(2) {
        paint_gradient(canvas, &gradient, path, bounds);
    } else if let Some(image) = fill.message_opt(3) {
        // ImageFillArchive: technique 2, size 4, tint 3. A theme picture the package lacks is
        // painted from its built-in ramp.
        let picture = doc
            .read_data(fill_image_id(&image))
            .and_then(|bytes| decode(&bytes));
        let ramp = match picture {
            Some(_) => None,
            None => theme_ramp(doc, &image),
        };
        if picture.is_none() && ramp.is_none() {
            return;
        }
        canvas.save();
        canvas.clip_path(path, None, Some(true));
        if let Some(picture) = picture {
            let size = image
                .message_opt(4)
                .filter(|size| size.float(1) > 0.0 && size.float(2) > 0.0)
                .map(|size| (size.float(1), size.float(2)));
            draw_image_fill(canvas, &picture, bounds, image.int(2), size);
        } else if let Some(ramp) = ramp {
            paint_ramp(canvas, ramp, bounds);
        }
        // The tint colour is laid over the image at its alpha (measured: a 50% white tint washes a
        // photo background out, across 17 Keynote decks).
        if let Some(tint) = image
            .message_opt(3)
            .map(|tint| styles::color(&tint))
            .filter(|tint| tint.a() > 0)
        {
            canvas.draw_rect(bounds, &styles::plain_paint(tint));
        }
        canvas.restore();
    }
}

/// A GradientArchive: linear or radial (type 1), stops (2), angle in radians (5.2: 4.712389 =
/// 270°, light-to-dark top to bottom), opacity (3), or a transform gradient (6).
fn paint_gradient(canvas: &Canvas, gradient: &Proto, path: &Path, bounds: Rect) {
    let stops = gradient.messages(2);
    if stops.is_empty() {
        return;
    }
    let opaque = ctx::OPAQUE_STOPS.get();
    let stop_color = |stop: &Proto| {
        let color = styles::color(&stop.message(1));
        if opaque {
            color.with_a(255)
        } else {
            color
        }
    };
    let colors: Vec<Color> = stops.iter().map(stop_color).collect();
    let positions: Vec<f32> = stops.iter().map(|stop| stop.float(2)).collect();
    let (colors, positions) = (colors.as_slice(), Some(positions.as_slice()));
    let radial = gradient.int(1) == 1;
    let center = Point::new(bounds.center_x(), bounds.center_y());
    let transform = gradient.message_opt(6).filter(|transform| {
        transform.message(3).float(1) > 0.0 && transform.message(3).float(2) > 0.0
    });
    let shader = match transform {
        // A transform gradient places the start and end points in a base frame (baseNaturalSize)
        // stretched to the shape: a radial one is an ellipse centred on start with end on its rim
        // (measured: a vignette over a slide-sized shape).
        Some(transform) => {
            let base = transform.message(3);
            let mut matrix = Matrix::scale((
                bounds.width() / base.float(1),
                bounds.height() / base.float(2),
            ));
            matrix.post_concat(&Matrix::translate((bounds.left, bounds.top)));
            let start = Point::new(transform.message(1).float(1), transform.message(1).float(2));
            let end = Point::new(transform.message(2).float(1), transform.message(2).float(2));
            if radial {
                let radius = Point::distance(start, end).max(0.01);
                Shader::radial_gradient(
                    start,
                    radius,
                    colors,
                    positions,
                    TileMode::Clamp,
                    None,
                    Some(&matrix),
                )
            } else {
                Shader::linear_gradient(
                    (matrix.map_point(start), matrix.map_point(end)),
                    colors,
                    positions,
                    TileMode::Clamp,
                    None,
                    None,
                )
            }
        }
        None if radial => {
            let radius = bounds.width().max(bounds.height()) / 2.0;
            Shader::radial_gradient(
                center,
                radius,
                colors,
                positions,
                TileMode::Clamp,
                None,
                None,
            )
        }
        None => {
            let angle = f64::from(gradient.message(5).float(2));
            let reach = (f64::from(bounds.width()) * angle.cos()).abs()
                + (f64::from(bounds.height()) * angle.sin()).abs();
            let half = reach as f32 / 2.0;
            let direction = Point::new(angle.cos() as f32 * half, -angle.sin() as f32 * half);
            Shader::linear_gradient(
                (center - direction, center + direction),
                colors,
                positions,
                TileMode::Clamp,
                None,
                None,
            )
        }
    };
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_shader(shader);
    let opacity = gradient.float_or(3, 1.0);
    if opacity < 1.0 {
        paint.set_color(Color::BLACK.with_a((opacity * 255.0) as u8));
    }
    canvas.draw_path(path, &paint);
}

/// Draws an image fill.
///
/// # Arguments
///
/// * `canvas` - The canvas (clipped to the filled area)
/// * `image` - The picture
/// * `area` - The box to fill
/// * `technique` - 0 natural size (centred), 1 stretch, 2 tile (from the top-left), 3 scale to
///   fill, 4 scale to fit
/// * `natural` - The fill size (`fillsize`), else the picture's pixel size
pub fn draw_image_fill(
    canvas: &Canvas,
    image: &Image,
    area: Rect,
    technique: i32,
    natural: Option<(f32, f32)>,
) {
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    let (image_width, image_height) = (image.width() as f32, image.height() as f32);
    let (natural_width, natural_height) = natural.unwrap_or((image_width, image_height));
    let centred = |scale: f32| {
        let (width, height) = (image_width * scale, image_height * scale);
        Rect::from_xywh(
            area.center_x() - width / 2.0,
            area.center_y() - height / 2.0,
            width,
            height,
        )
    };
    let destination = match technique {
        0 => Rect::from_xywh(
            area.center_x() - natural_width / 2.0,
            area.center_y() - natural_height / 2.0,
            natural_width,
            natural_height,
        ),
        1 => area,
        2 if natural_width > 0.0 && natural_height > 0.0 => {
            let mut top = area.top;
            while top < area.bottom {
                let mut left = area.left;
                while left < area.right {
                    let tile = Rect::from_xywh(left, top, natural_width, natural_height);
                    canvas.draw_image_rect_with_sampling_options(
                        image,
                        None,
                        tile,
                        linear_sampling(),
                        &paint,
                    );
                    left += natural_width;
                }
                top += natural_height;
            }
            return;
        }
        4 => centred((area.width() / image_width).min(area.height() / image_height)),
        _ => centred((area.width() / image_width).max(area.height() / image_height)),
    };
    canvas.draw_image_rect_with_sampling_options(
        image,
        None,
        destination,
        linear_sampling(),
        &paint,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_locators_ignore_the_size_suffix() {
        assert!(ramp_for_locator("gradient/gradient_10x7").is_some());
        assert!(ramp_for_locator("new_template5/chipboard_hd").is_some());
        assert!(ramp_for_locator("unknown/unknown_hd").is_none());
    }

    #[test]
    fn rect_helpers_match_skiasharp() {
        assert!(is_zero(&Rect::default()));
        assert!(!is_zero(&Rect::new(0.0, 0.0, 5.0, 0.0)));
        assert_eq!(
            union(Rect::new(0.0, 0.0, 1.0, 1.0), Rect::default()),
            Rect::new(0.0, 0.0, 1.0, 1.0)
        );
    }

    #[test]
    fn rotation_grows_the_box() {
        let rotated = rotated_bounds(0.0, 0.0, (10.0, 10.0), 45.0);
        assert!((rotated.width() - 200f32.sqrt()).abs() < 1e-3);
        assert_eq!(
            rotated_bounds(1.0, 2.0, (3.0, 4.0), 0.0),
            Rect::from_xywh(1.0, 2.0, 3.0, 4.0)
        );
    }

    #[test]
    fn callout_tail_bases() {
        // Keynote's rectangle callout: tip left of the box, base on the left edge level with it.
        let frame = Rect::from_wh(320.0, 220.0);
        let (first, second) = box_tail_base(frame, Point::new(-20.0, 50.0), 10.0).expect("base");
        assert_eq!(
            (first, second),
            (Point::new(0.0, 40.0), Point::new(0.0, 60.0))
        );
        assert!(box_tail_base(frame, Point::new(10.0, 10.0), 10.0).is_none());
        // Its oval callout: both base points on the ellipse, 16pt apart across the tip line.
        let tip = Point::new(0.8, 232.8);
        let (first, second) = centered_tail_base(frame, true, tip, 8.0).expect("base");
        let on_ellipse = |point: Point| {
            ((point.x - 160.0) / 160.0).powi(2) + ((point.y - 110.0) / 110.0).powi(2)
        };
        assert!((on_ellipse(first) - 1.0).abs() < 1e-3 && (on_ellipse(second) - 1.0).abs() < 1e-3);
        assert!(((first - second).length() - 16.0).abs() < 1.5);
    }

    #[test]
    fn scalar_shapes() {
        let source = |shape_type: u8, value: f32| {
            let mut bytes = vec![0x08, shape_type, 0x15];
            bytes.extend(value.to_le_bytes());
            Proto::new(Rc::new(bytes.clone()), 0, bytes.len())
        };
        assert_eq!(
            scalar_path(&source(0, 100.0), 20.0, 10.0).bounds(),
            &Rect::from_wh(20.0, 10.0)
        );
        assert_eq!(scalar_path(&source(1, 5.0), 10.0, 10.0).count_points(), 5);
    }

    #[test]
    fn padding_defaults() {
        assert_eq!(padding_inset(None, 1), 4.0);
        assert_eq!(padding_inset(Some(&Proto::empty()), 1), 0.0);
    }
}
