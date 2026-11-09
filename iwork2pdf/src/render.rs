//! Document conversion: renders a decoded document's pages as a PDF in memory (`render`), or
//! file to file (`convert`).

use crate::drawables::{
    self, geometry, is_zero, paint_fill, rotated_bounds, union, wrap_in, wrap_of, Item, ShapeItem,
};
use crate::error::{Error, Result};
use crate::formula;
use crate::iwa::{IwaDocument, IwaObject, Proto};
use crate::richtext::{PageSink, RichText, Scratch};
use crate::styles::{self, ctx, Styles};
use crate::text::{HAlign, TextStyle};
use skia_safe::{Canvas, Color, Path, Picture, PictureRecorder, Rect};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path as FilePath;
use std::rc::Rc;

/// Pages recorded as pictures and written to a PDF at the end.
pub struct PdfPages {
    recorder: PictureRecorder,
    current: Option<(f32, f32)>,
    pages: Vec<((f32, f32), Picture)>,
}

impl Default for PdfPages {
    fn default() -> PdfPages {
        PdfPages {
            recorder: PictureRecorder::new(),
            current: None,
            pages: Vec::new(),
        }
    }
}

impl PdfPages {
    /// Starts a page of the given size (points).
    ///
    /// # Returns
    ///
    /// The page's canvas.
    pub fn begin(&mut self, width: f32, height: f32) -> &Canvas {
        self.end();
        self.current = Some((width, height));
        self.recorder
            .begin_recording(Rect::from_wh(width, height), None)
    }

    /// The current page's canvas, if a page is open.
    pub fn canvas(&mut self) -> Option<&Canvas> {
        self.recorder.recording_canvas()
    }

    /// Ends the current page, if any.
    pub fn end(&mut self) {
        if let Some(size) = self.current.take() {
            if let Some(picture) = self.recorder.finish_recording_as_picture(None) {
                self.pages.push((size, picture));
            }
        }
    }

    /// Ends the current page and encodes every page as a PDF.
    ///
    /// # Returns
    ///
    /// The PDF's bytes and its page count.
    pub fn finish(mut self) -> (Vec<u8>, usize) {
        self.end();
        let mut bytes: Vec<u8> = Vec::new();
        // An encoding quality of 101 keeps images lossless (SkiaSharp's default; 0 would write
        // opaque images as quality-0 JPEG).
        let metadata = skia_safe::pdf::Metadata {
            producer: "iwork2pdf".into(),
            raster_dpi: Some(72.0),
            encoding_quality: Some(101),
            ..Default::default()
        };
        let mut document = skia_safe::pdf::new_document(&mut bytes, Some(&metadata));
        for ((width, height), picture) in &self.pages {
            let mut page = document.begin_page((*width, *height), None);
            picture.playback(page.canvas());
            document = page.end_page();
        }
        document.close();
        (bytes, self.pages.len())
    }
}

/// The iWork application a document belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    /// A .numbers spreadsheet.
    Numbers,
    /// A .key presentation.
    Keynote,
    /// A .pages document.
    Pages,
}

impl DocumentKind {
    /// The kind for a file extension (without the dot, any case).
    ///
    /// # Returns
    ///
    /// `None` for anything but numbers, key and pages.
    pub fn from_extension(extension: &str) -> Option<DocumentKind> {
        match extension.to_ascii_lowercase().as_str() {
            "numbers" => Some(DocumentKind::Numbers),
            "key" => Some(DocumentKind::Keynote),
            "pages" => Some(DocumentKind::Pages),
            _ => None,
        }
    }
}

/// Converts an iWork document file or package to a PDF file.
///
/// # Arguments
///
/// * `document` - A .numbers, .key or .pages file or package
/// * `output` - Where to write the PDF
///
/// # Returns
///
/// The number of pages written.
///
/// # Errors
///
/// Returns `Error::Unsupported` for other extensions, the open errors of the document and
/// `Error::Io` when the PDF cannot be written.
pub fn convert(document: &FilePath, output: &FilePath) -> Result<usize> {
    let extension = document
        .extension()
        .map(|extension| extension.to_string_lossy())
        .unwrap_or_default();
    let kind = DocumentKind::from_extension(&extension)
        .ok_or_else(|| Error::Unsupported(format!(".{extension}")))?;
    let (bytes, pages) = render(IwaDocument::open(document)?, kind)?;
    std::fs::write(output, bytes).map_err(Error::io(output))?;
    Ok(pages)
}

/// Renders a decoded document as a PDF in memory (no file system access besides font lookup).
///
/// # Arguments
///
/// * `document` - The decoded document, e.g. from `IwaDocument::from_bytes`
/// * `kind` - Which application's layout rules apply
///
/// # Returns
///
/// The PDF's bytes and its page count.
///
/// # Errors
///
/// Returns `Error::Missing` when a required archive object is absent.
pub fn render(document: IwaDocument, kind: DocumentKind) -> Result<(Vec<u8>, usize)> {
    let doc = Rc::new(document);
    // TSK.DocumentArchive.locale_identifier (4), under TSA.DocumentArchive (field 1) in the app's
    // document archive (Numbers field 8, Keynote 3, Pages 1).
    let root = doc.body(1);
    let locale = [8, 3, 1].iter().find_map(|&field| {
        root.as_ref()
            .and_then(|root| root.message(field).message(1).string(4))
            .filter(|locale| !locale.is_empty())
    });
    // Per-document layout state (thread-local, so batch conversions reset it).
    ctx::LOCALE.with(|cell| *cell.borrow_mut() = locale);
    ctx::GROW_ROWS.set(kind != DocumentKind::Numbers);
    ctx::KEYNOTE.set(kind == DocumentKind::Keynote);
    ctx::PAGES_LAYOUT.set(kind == DocumentKind::Pages);
    ctx::HYPHENATION.set(false);
    ctx::LIGATURES.set(false);
    ctx::SLIDE_WIDTH.set(0.0);
    ctx::ABOVE_BELOW.with(|rects| rects.borrow_mut().clear());
    formula::reset();
    let styles = Styles::new(Rc::clone(&doc));
    let mut pdf = PdfPages::default();
    match kind {
        DocumentKind::Numbers => render_numbers(&styles, &mut pdf)?,
        DocumentKind::Keynote => render_keynote(&styles, &mut pdf)?,
        DocumentKind::Pages => render_pages(&styles, &mut pdf)?,
    }
    Ok(pdf.finish())
}

// ----------------------------------------------------------------- Numbers

/// Numbers export: one page per sheet, the sheet's canvas drawn at 1:1 offset by 72pt, the page
/// extending 72pt past the drawables' right/bottom edge; pages are at least US Letter (612 x 792).
/// Form-based sheets are not exported.
fn render_numbers(styles: &Styles, pdf: &mut PdfPages) -> Result<()> {
    const MARGIN: f32 = 72.0;
    const MINIMUM: (f32, f32) = (612.0, 792.0);
    let doc = styles.doc();
    let root = doc.body(1).ok_or(Error::Missing("TN.DocumentArchive"))?;
    for sheet_id in root.references(1) {
        let Some(sheet) = doc
            .get(sheet_id)
            .filter(|object| object.typ == 2)
            .map(|object| object.body.clone())
        else {
            continue;
        };
        let mut items = Vec::new();
        for id in sheet.references(2) {
            collect_numbers_items(styles, id, &mut items);
        }
        // A masked image extends the page only as far as its mask (measured).
        let extent = items
            .iter()
            .map(|item| {
                if matches!(item, Item::Image(_)) {
                    item.flow_bounds()
                } else {
                    item.bounds()
                }
            })
            .fold(Rect::default(), |extent, bounds| {
                if is_zero(&extent) {
                    bounds
                } else {
                    union(extent, bounds)
                }
            });
        let width = MINIMUM.0.max(extent.right + 2.0 * MARGIN);
        let height = MINIMUM.1.max(extent.bottom + 2.0 * MARGIN);
        log::debug!(
            "sheet '{}': extent {extent:?} -> page {width}x{height}",
            sheet.string(1).unwrap_or_default()
        );
        let canvas = pdf.begin(width, height);
        // The sheet's background fill (TN.SheetStyleArchive sheet_properties.fill) covers the page
        // unless print_backgrounds (23) is off.
        let fill = doc
            .body(sheet.reference(22))
            .and_then(|style| style.message(3).message_opt(1))
            .filter(|fill| !fill.is_empty());
        if let (true, Some(fill)) = (sheet.boolean_or(23, true), fill) {
            let mut page = Path::new();
            page.add_rect(Rect::from_wh(width, height), None);
            paint_fill(
                canvas,
                doc,
                Some(&fill),
                &page,
                Rect::from_wh(width, height),
            );
        }
        // The canvas is always offset by exactly the margin (content at negative coordinates bleeds
        // into it).
        canvas.translate((MARGIN, MARGIN));
        for item in &items {
            item.draw(canvas);
        }
    }
    Ok(())
}

/// A sheet drawable plus its title and caption. A drawable's title (TSD.DrawableArchive title 10,
/// title_hidden 12) and caption (11, 13) are TSA.CaptionInfoArchive text boxes above or below the
/// frame, 6.2pt away and as wide as the object (an empty TSD.StandinCaptionArchive means none).
fn collect_numbers_items(styles: &Styles, id: u64, items: &mut Vec<Item>) {
    let Some(object) = styles.doc().get(id) else {
        return;
    };
    let Some(item) = drawables::create(styles, object) else {
        return;
    };
    // Shapes carry the drawable one level deeper.
    let drawable = if object.typ == 2011 {
        object.body.message(1).message(1)
    } else {
        object.body.message(1)
    };
    let frame = if object.typ == 6000 {
        item.bounds()
    } else {
        item.flow_bounds()
    };
    items.push(item);
    let caption_box = |field: i32| {
        let hidden = drawable.boolean(field + 2);
        let caption = styles
            .doc()
            .get(drawable.reference(field))
            .filter(|caption| !hidden && caption.typ == 633)?;
        Some(ShapeItem::new(
            styles,
            &caption.with(2011, caption.body.message(1)),
        ))
    };
    if let Some(mut caption) = caption_box(11) {
        // Measured: the first caption baseline sits 22.3pt below a table frame (gap + padding +
        // ascent).
        caption.move_to(frame.left, frame.bottom + 6.2);
        items.push(Item::Shape(caption));
    }
    if let Some(mut title) = caption_box(10) {
        let height = title.text_height();
        title.move_to(frame.left, frame.top - 6.2 - height);
        items.push(Item::Shape(title));
    }
}

// ----------------------------------------------------------------- Keynote

/// Keynote export: one page per non-skipped slide, page size = show size. Each slide draws its
/// background fill, then its master's non-placeholder drawables, then its own drawables in z-order;
/// placeholders are drawn when the slide style marks them visible.
fn render_keynote(styles: &Styles, pdf: &mut PdfPages) -> Result<()> {
    let doc = styles.doc();
    let root = doc.body(1).ok_or(Error::Missing("KN.DocumentArchive"))?;
    let show = doc
        .body(root.reference(2))
        .ok_or(Error::Missing("KN.ShowArchive"))?;
    let size = show.message(4);
    let (width, height) = (size.float_or(1, 1024.0), size.float_or(2, 768.0));
    let mut preview = PreviewBackground::default();
    let slides = slide_order(doc, &show);
    ctx::PAGE_COUNT.set(slides.len() as i32);
    for (index, slide_id) in slides.into_iter().enumerate() {
        let Some(slide) = doc.body(slide_id) else {
            continue;
        };
        let canvas = pdf.begin(width, height);
        // Keynote's export starts every page with a white fill and a 0.25pt black hairline on the
        // page edge, both under the slide (a renderer that widens hairlines to a pixel shows the
        // outline only where the slide does not cover it).
        canvas.draw_rect(
            Rect::from_wh(width, height),
            &styles::plain_paint(Color::WHITE),
        );
        let mut edge = styles::plain_paint(Color::BLACK);
        edge.set_style(skia_safe::PaintStyle::Stroke);
        edge.set_stroke_width(0.25);
        edge.set_anti_alias(true);
        canvas.draw_rect(
            Rect::from_xywh(-0.125, -0.125, width + 0.25, height + 0.25),
            &edge,
        );
        canvas.save();
        canvas.clip_rect(Rect::from_wh(width, height), None, None);
        ctx::SLIDE_WIDTH.set(width);
        let slide_number = index + 1;
        // Slide-number placeholders hold a page-number field (TSWP.NumberAttachmentArchive).
        ctx::PAGE_NUMBER.set(slide_number as i32);
        draw_slide(
            styles,
            canvas,
            &slide,
            (width, height),
            &mut |canvas: &Canvas| {
                log::debug!("missing-theme-image slide {slide_number}");
                preview.paint(doc, canvas, (width, height))
            },
        );
        canvas.restore();
    }
    Ok(())
}

/// Slides in presentation order, skipped slides left out.
fn slide_order(doc: &IwaDocument, show: &Proto) -> Vec<u64> {
    let tree = show.message(3);
    let mut nodes = tree.references(2);
    if nodes.is_empty() {
        nodes = doc
            .body(tree.reference(1))
            .map(|root| root.references(1))
            .unwrap_or_default();
    }
    fn walk(doc: &IwaDocument, node_id: u64, slides: &mut Vec<u64>) {
        let Some(node) = doc.body(node_id) else {
            return;
        };
        if !node.boolean(4) && node.reference(2) != 0 {
            slides.push(node.reference(2));
        }
        for child in node.references(1) {
            walk(doc, child, slides);
        }
    }
    let mut slides = Vec::new();
    for node in nodes {
        walk(doc, node, &mut slides);
    }
    slides
}

/// Paints a slide whose theme background image the package lacks; returns false when it cannot.
type MissingBackground<'a> = dyn FnMut(&Canvas) -> bool + 'a;

fn draw_slide(
    styles: &Styles,
    canvas: &Canvas,
    slide: &Proto,
    (width, height): (f32, f32),
    missing_background: &mut MissingBackground,
) {
    const PLACEHOLDER: u32 = 7;
    let doc = styles.doc();
    let style = slide.reference(1);
    let master = doc.body(slide.reference(17));
    // Background: the slide style's fill, else the master's. An empty fill set on the slide's own
    // style is "no fill": the page stays white (measured: a variation with `fill {}` over a blue
    // master exports white).
    let own_fill = styles.find(style, 11, 1).map(|props| props.message(1));
    let fill = match (&own_fill, &master) {
        (Some(fill), _) if !fill.is_empty() => Some(fill.clone()),
        (Some(_), _) => None,
        (None, Some(master)) => styles
            .find(master.reference(1), 11, 1)
            .map(|props| props.message(1)),
        (None, None) => None,
    };
    let page = Rect::from_wh(width, height);
    let mut background = Path::new();
    background.add_rect(page, None);
    canvas.draw_rect(page, &styles::plain_paint(Color::WHITE));
    // A theme background image the package does not carry (a Keynote theme resource stored only by
    // locator): paint the fill's reference colour (its stored average), else the colour the
    // package's preview shows around slide 1, instead of leaving the slide white.
    let missing_image = fill
        .as_ref()
        .and_then(|fill| fill.message_opt(3))
        .filter(|image| !drawables::image_available(doc, image));
    match missing_image {
        Some(image) => {
            let painted = match image.message_opt(9) {
                Some(reference) => {
                    canvas.draw_rect(page, &styles::plain_paint(styles::color(&reference)));
                    true
                }
                None => missing_background(canvas),
            };
            if !painted {
                paint_fill(canvas, doc, fill.as_ref(), &background, page);
            }
        }
        None => paint_fill(canvas, doc, fill.as_ref(), &background, page),
    }
    if let Some(master) = &master {
        // Tagged master objects (sage_tag_to_info_map, e.g. "Subtitle") are templates that slides
        // instantiate with their own copy; they are not drawn from the master.
        let tagged: HashSet<u64> = master
            .messages(28)
            .iter()
            .map(|entry| entry.reference(2))
            .collect();
        for id in z_order(master)
            .into_iter()
            .filter(|id| !tagged.contains(id))
        {
            if let Some(object) = doc.get(id).filter(|object| object.typ != PLACEHOLDER) {
                draw_object(styles, canvas, object);
            }
        }
    }
    let visible = |field: i32, default: bool| {
        styles
            .find(style, 11, field)
            .map_or(default, |props| props.boolean_or(field, default))
    };
    // Title (5), body (6), object (30) and slide-number (20) placeholders.
    let shown: HashMap<u64, bool> = HashMap::from([
        (slide.reference(5), visible(4, true)),
        (slide.reference(6), visible(5, true)),
        (slide.reference(30), visible(7, false)),
        (slide.reference(20), visible(6, true)),
    ]);
    for id in z_order(slide) {
        let Some(object) = doc.get(id) else { continue };
        if object.typ == PLACEHOLDER && shown.get(&id) == Some(&false) {
            continue;
        }
        if object.typ == PLACEHOLDER {
            // KN.PlaceholderArchive wraps a TSWP.ShapeInfoArchive.
            let shape_info = object.with(2011, object.body.message(1));
            if let Some(mut item) = drawables::create(styles, &shape_info) {
                if let (Item::Shape(shape), true) = (&mut item, id == slide.reference(20)) {
                    shape.keep_above(height);
                }
                item.draw(canvas);
            }
        } else {
            draw_object(styles, canvas, object);
        }
    }
}

/// A slide's drawables in z-order (42), else in stored order (7).
fn z_order(slide: &Proto) -> Vec<u64> {
    let ordered = slide.references(42);
    if ordered.is_empty() {
        slide.references(7)
    } else {
        ordered
    }
}

fn draw_object(styles: &Styles, canvas: &Canvas, object: &IwaObject) {
    if let Some(item) = drawables::create(styles, object) {
        item.draw(canvas);
    }
}

/// The median colour along the border of the package's preview.jpg (slide 1), computed once.
#[derive(Default)]
struct PreviewBackground {
    computed: bool,
    color: Option<Color>,
}

impl PreviewBackground {
    fn paint(&mut self, doc: &IwaDocument, canvas: &Canvas, (width, height): (f32, f32)) -> bool {
        if !self.computed {
            self.computed = true;
            self.color = doc
                .read_file("preview.jpg")
                .and_then(|bytes| drawables::decode(&bytes))
                .and_then(|image| drawables::rgba_pixels(&image))
                .map(border_median);
        }
        let Some(color) = self.color else {
            return false;
        };
        canvas.draw_rect(Rect::from_wh(width, height), &styles::plain_paint(color));
        true
    }
}

/// The per-channel median of pixels sampled along an image's border (one pixel in from the edge).
fn border_median((width, height, pixels): (usize, usize, Vec<u8>)) -> Color {
    let step = (width.min(height) / 64).max(1);
    let pixel = |column: usize, row: usize| {
        let at = (row.min(height - 1) * width + column.min(width - 1)) * 4;
        [pixels[at], pixels[at + 1], pixels[at + 2]]
    };
    let mut samples = Vec::new();
    for column in (0..width).step_by(step) {
        samples.push(pixel(column, 1));
        samples.push(pixel(column, height.saturating_sub(2)));
    }
    for row in (0..height).step_by(step) {
        samples.push(pixel(1, row));
        samples.push(pixel(width.saturating_sub(2), row));
    }
    let median = |channel: usize| {
        let mut values: Vec<u8> = samples.iter().map(|sample| sample[channel]).collect();
        values.sort_unstable();
        values[values.len() / 2]
    };
    Color::from_rgb(median(0), median(1), median(2))
}

// ------------------------------------------------------------------- Pages

/// Header and footer storages of a page template (TP.SectionTemplateArchive: headers 1, footers
/// 2; three each: left, centre, right).
type HeadersFooters = (Vec<u64>, Vec<u64>);

/// TP.DocumentArchive page geometry (field numbers observed): page width 30, height 31, margins
/// left 32, right 33, top 34, bottom 35, header 36, footer 37.
#[derive(Debug, Clone, Copy)]
struct PageGeometry {
    width: f32,
    height: f32,
    left: f32,
    right: f32,
    header: f32,
    footer: f32,
}

/// Pages export: the body storage flows through pages of the document's page size inside its
/// margins, with section headers/footers, master drawables and page-anchored floating objects.
fn render_pages(styles: &Styles, pdf: &mut PdfPages) -> Result<()> {
    let doc = styles.doc();
    let root = doc.body(1).ok_or(Error::Missing("TP.DocumentArchive"))?;
    let geometry = PageGeometry {
        width: root.float_or(30, 612.0),
        height: root.float_or(31, 792.0),
        left: root.float_or(32, 72.0),
        right: root.float_or(33, 72.0),
        header: root.float_or(36, 36.0),
        footer: root.float_or(37, 36.0),
    };
    let (top, bottom) = (root.float_or(34, 72.0), root.float_or(35, 72.0));
    ctx::FOOTNOTE_COUNTER.set(0);
    // TP.SettingsArchive (root field 7): hyphenation (9) and ligatures (10) switches.
    let settings = doc.body(root.reference(7));
    ctx::HYPHENATION.set(
        settings
            .as_ref()
            .is_some_and(|settings| settings.boolean(9)),
    );
    ctx::LIGATURES.set(
        settings
            .as_ref()
            .is_some_and(|settings| settings.boolean(10)),
    );
    let body = RichText::from_storage(styles, root.reference(4), &TextStyle::default(), 0);
    let floating = floating_drawables(doc);
    let area = Rect::new(
        geometry.left,
        top,
        geometry.width - geometry.right,
        geometry.height - bottom,
    );
    let obstacle_cache: RefCell<HashMap<i32, Rc<Vec<Rect>>>> = RefCell::new(HashMap::new());
    let obstacles = |page: i32| -> Rc<Vec<Rect>> {
        if let Some(cached) = obstacle_cache.borrow().get(&page) {
            return Rc::clone(cached);
        }
        let rects = Rc::new(page_obstacles(
            doc,
            floating.get(&page).map(Vec::as_slice).unwrap_or_default(),
        ));
        obstacle_cache.borrow_mut().insert(page, Rc::clone(&rects));
        rects
    };
    let last_floating_page = floating.keys().max().copied();
    // A dry run counts the pages (page-count fields in headers and footers).
    let mut dry = Scratch::default();
    let flowed = body.draw_flow(&mut dry, area, HAlign::Left, Some(&obstacles));
    let total = flowed.max(last_floating_page.map_or(0, |last| last + 1));
    let mut sink = PagesSink {
        styles,
        pdf,
        geometry,
        total,
        index: 0,
        previous_section: None,
        template_section: 0,
        page_number: 0,
        masters: section_master_drawables(doc),
        floating: &floating,
    };
    body.draw_flow(&mut sink, area, HAlign::Left, Some(&obstacles));
    // Pages holding only floating objects past the end of the text.
    while last_floating_page.is_some_and(|last| sink.index <= last) {
        sink.new_page(body.page_section());
    }
    sink.pdf.end();
    Ok(())
}

/// Floating objects with text wrap (TSD.DrawableArchive exterior_text_wrap): types 1 (around), 2
/// (above and below) and 4 with direction 2 push text; their frame (a masked picture's mask frame)
/// plus the wrap margin is the obstacle. Above-and-below obstacles are also registered so lines
/// never sit beside them.
fn page_obstacles(doc: &IwaDocument, ids: &[u64]) -> Vec<Rect> {
    let mut rects = Vec::new();
    for object in ids.iter().filter_map(|&id| doc.get(id)) {
        // An unset direction does not push (measured: a full-width image).
        let Some(wrap) =
            wrap_of(object).filter(|wrap| matches!(wrap.int(1), 1 | 2 | 4) && wrap.int(2) == 2)
        else {
            continue;
        };
        let drawable = if wrap_in(&object.body.message(1).message(1)).is_some() {
            object.body.message(1).message(1)
        } else {
            object.body.message(1)
        };
        let (left, top, width, height, angle) = geometry(&drawable);
        let mut frame = rotated_bounds(left, top, (width, height), angle);
        // A masked picture wraps text by its mask (measured: a cover photo cropped 27pt short of
        // its frame lets the title sit 26pt higher).
        if let Some(mask) = (object.typ == 3005 && object.body.reference(5) != 0)
            .then(|| doc.body(object.body.reference(5)))
            .flatten()
        {
            let (mask_x, mask_y, mask_width, mask_height, _) = geometry(&mask.message(1));
            if mask_width > 0.0 && mask_height > 0.0 {
                frame = rotated_bounds(
                    left + mask_x,
                    top + mask_y,
                    (mask_width, mask_height),
                    angle,
                );
            }
        }
        let obstacle = frame.with_outset((wrap.float(4), wrap.float(4)));
        if wrap.int(1) == 2 {
            ctx::ABOVE_BELOW.with(|above_below| above_below.borrow_mut().push(obstacle));
        }
        rects.push(obstacle);
    }
    rects
}

/// Page-anchored objects. TP.FloatingDrawablesArchive (observed): entries of { 1: page index, 4:
/// { 1: drawable reference } }, drawn in TP.DrawablesZOrderArchive order where one exists.
fn floating_drawables(doc: &IwaDocument) -> HashMap<i32, Vec<u64>> {
    let z_order = doc
        .of_type(10015)
        .first()
        .map(|object| object.body.references(1))
        .unwrap_or_default();
    let mut pages: HashMap<i32, Vec<u64>> = HashMap::new();
    for entry in doc
        .of_type(10010)
        .iter()
        .flat_map(|object| object.body.messages(1))
    {
        let ids = entry
            .messages(4)
            .into_iter()
            .map(|drawable| drawable.reference(1))
            .filter(|&id| id != 0);
        pages.entry(entry.int(1)).or_default().extend(ids);
    }
    let rank = |id: &u64| {
        z_order
            .iter()
            .position(|ordered| ordered == id)
            .unwrap_or(usize::MAX)
    };
    for ids in pages.values_mut() {
        ids.sort_by_key(rank);
    }
    pages
}

/// Section-master objects, repeated on every page: the distinct drawables (3) of every section's
/// page templates (23-25).
fn section_master_drawables(doc: &IwaDocument) -> Vec<u64> {
    let mut ids = Vec::new();
    for section in doc.of_type(10011) {
        for field in [23, 24, 25] {
            for id in doc
                .body(section.body.reference(field))
                .map(|template| template.references(3))
                .unwrap_or_default()
            {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

/// Header and footer storages of a section's page template.
fn template_storages(doc: &IwaDocument, section: &Proto, template_field: i32) -> HeadersFooters {
    doc.body(section.reference(template_field))
        .map_or_else(Default::default, |template| {
            (template.references(1), template.references(2))
        })
}

/// Starts Pages pages: masters, headers and footers of the page's section, floating objects.
struct PagesSink<'a> {
    styles: &'a Styles,
    pdf: &'a mut PdfPages,
    geometry: PageGeometry,
    total: i32,
    index: i32,
    previous_section: Option<u64>,
    /// The section whose header and footer templates are in effect (sections may inherit them).
    template_section: u64,
    /// The number page-number fields show.
    page_number: i32,
    masters: Vec<u64>,
    floating: &'a HashMap<i32, Vec<u64>>,
}

impl PagesSink<'_> {
    /// The page's headers and footers, from the templates of the section in effect: first
    /// (TP.SectionArchive field 23) on a section's first page when its first page differs (18),
    /// even (24) on even pages when even and odd pages differ (19), else odd (25).
    fn headers_footers(&self, first: bool) -> HeadersFooters {
        let doc = self.styles.doc();
        let Some(body) = doc.body(self.template_section) else {
            return HeadersFooters::default();
        };
        let even = (self.index + 1) % 2 == 0;
        let field = if first && body.boolean(18) {
            23
        } else if body.boolean(19) && even {
            24
        } else {
            25
        };
        template_storages(doc, &body, field)
    }

    /// Enters a page of `section` (TP.SectionArchive): a new section keeps the previous one's
    /// header and footer templates when it inherits them (17), and restarts page numbers at
    /// field 22 when its numbering kind (21) is 1, else continues them.
    ///
    /// # Returns
    ///
    /// Whether the page is the section's first, or `None` when the section hides its headers
    /// and footers there (28).
    fn enter_section(&mut self, section: u64) -> Option<bool> {
        if self.previous_section == Some(section) {
            self.page_number += 1;
            return Some(false);
        }
        self.previous_section = Some(section);
        let body = self.styles.doc().body(section).unwrap_or_else(Proto::empty);
        if !body.boolean(17) || self.template_section == 0 {
            self.template_section = section;
        }
        self.page_number = if body.int(21) == 1 {
            body.int_or(22, 1)
        } else {
            self.page_number + 1
        };
        (!body.boolean(28)).then_some(true)
    }

    /// Headers hang from the header margin, footers sit on the footer margin; left, centre and
    /// right parts take those alignments.
    fn draw_headers_footers(&mut self, (headers, footers): HeadersFooters) {
        ctx::PAGE_NUMBER.set(self.page_number);
        ctx::PAGE_COUNT.set(self.total);
        let geometry = self.geometry;
        let aligns = [HAlign::Left, HAlign::Center, HAlign::Right];
        let Some(canvas) = self.pdf.canvas() else {
            return;
        };
        for (ids, is_header) in [(headers, true), (footers, false)] {
            for (storage, align) in ids.into_iter().zip(aligns) {
                let text = RichText::from_storage(self.styles, storage, &TextStyle::default(), 0);
                if text
                    .paragraphs
                    .iter()
                    .all(|paragraph| paragraph.runs.is_empty())
                {
                    continue;
                }
                let (left, right) = (geometry.left, geometry.width - geometry.right);
                let rect = if is_header {
                    Rect::new(left, geometry.header, right, geometry.header + 400.0)
                } else {
                    Rect::new(
                        left,
                        geometry.height - geometry.footer - 400.0,
                        right,
                        geometry.height - geometry.footer,
                    )
                };
                text.draw(canvas, rect, if is_header { 0 } else { 2 }, true, align);
            }
        }
    }
}

impl PageSink for PagesSink<'_> {
    fn new_page(&mut self, section: u64) {
        let doc = self.styles.doc();
        let canvas = self.pdf.begin(self.geometry.width, self.geometry.height);
        for object in self.masters.iter().filter_map(|&id| doc.get(id)) {
            draw_object(self.styles, canvas, object);
        }
        let section = if section == 0 {
            doc.of_type(10011).first().map_or(0, |object| object.id)
        } else {
            section
        };
        if let Some(first) = self.enter_section(section) {
            let headers_footers = self.headers_footers(first);
            self.draw_headers_footers(headers_footers);
        }
        if let (Some(ids), Some(canvas)) = (self.floating.get(&self.index), self.pdf.canvas()) {
            for object in ids.iter().filter_map(|&id| doc.get(id)) {
                draw_object(self.styles, canvas, object);
            }
        }
        self.index += 1;
    }

    fn canvas(&mut self) -> &Canvas {
        self.pdf
            .canvas()
            .expect("new_page opens a page before the flow draws")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn border_median_ignores_outliers() {
        let (width, height) = (8usize, 8usize);
        let mut pixels = vec![200u8; width * height * 4];
        pixels[..4].copy_from_slice(&[0, 0, 0, 255]);
        assert_eq!(
            border_median((width, height, pixels)),
            Color::from_rgb(200, 200, 200)
        );
    }

    #[test]
    fn unsupported_extension() {
        let result = convert(FilePath::new("notes.txt"), FilePath::new("notes.pdf"));
        assert!(matches!(result, Err(Error::Unsupported(_))));
    }

    #[test]
    fn document_kind_from_extension() {
        assert_eq!(
            DocumentKind::from_extension("KEY"),
            Some(DocumentKind::Keynote)
        );
        assert_eq!(DocumentKind::from_extension("docx"), None);
    }

    #[test]
    fn from_bytes_rejects_non_zip() {
        assert!(matches!(
            IwaDocument::from_bytes(b"not a zip"),
            Err(Error::Zip(_))
        ));
    }
}
