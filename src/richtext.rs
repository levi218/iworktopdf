//! Lays out and draws a TSWP.StorageArchive: the text is split into paragraphs, each with its
//! paragraph style (table_para_style) and character style runs (table_char_style), wrapped greedily
//! on word boundaries and positioned with Apple's line metrics.

use crate::bidi;
use crate::drawables::{self, is_zero, union, Item};
use crate::iwa::Proto;
use crate::shaper::{self, TaggedFont};
use crate::styles::{ctx, plain_paint, Styles};
use crate::text::{self, break_char, HAlign, TextStyle};
use skia_safe::{Canvas, Color, PaintCap, PictureRecorder, Point, Rect};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Where flowed text draws: a fresh page on demand, then that page's canvas.
pub trait PageSink {
    /// Starts a page; `section` is the TP.SectionArchive of the page's first line.
    fn new_page(&mut self, section: u64);
    /// The current page's canvas.
    fn canvas(&mut self) -> &Canvas;
}

/// A sink that records and discards pages (layout passes and dry runs).
pub struct Scratch {
    recorder: PictureRecorder,
    /// Pages started so far.
    pub pages: i32,
}

impl Default for Scratch {
    fn default() -> Scratch {
        let mut recorder = PictureRecorder::new();
        recorder.begin_recording(Rect::from_wh(10_000.0, 10_000.0), None);
        Scratch { recorder, pages: 0 }
    }
}

impl PageSink for Scratch {
    fn new_page(&mut self, _section: u64) {
        // The previous page's recording is dropped.
        let _ = self.recorder.finish_recording_as_picture(None);
        self.recorder
            .begin_recording(Rect::from_wh(10_000.0, 10_000.0), None);
        self.pages += 1;
    }

    fn canvas(&mut self) -> &Canvas {
        self.recorder
            .recording_canvas()
            .expect("Scratch always has a recording in progress")
    }
}

/// A run of text in one style.
#[derive(Debug, Clone)]
pub struct Run {
    /// The text (already cased).
    pub text: String,
    /// Its resolved style.
    pub style: Rc<TextStyle>,
}

/// A paragraph with its runs and layout attributes.
#[derive(Debug, Clone)]
pub struct Paragraph {
    /// The text runs.
    pub runs: Vec<Run>,
    /// The paragraph style.
    pub style: TextStyle,
    /// The list label, if any.
    pub bullet: Option<String>,
    /// Starts on a new page (page break or page_break_before).
    pub page_break_before: bool,
    /// The list label's left edge.
    pub bullet_x: f32,
    /// The list label's scale.
    pub bullet_scale: f32,
    /// Inline drawables (TSWP.DrawableAttachmentArchive at U+FFFC) in this paragraph.
    pub attachments: Vec<u64>,
    /// Footnote storages referenced in this paragraph, with their numbers.
    pub footnotes: Vec<(u64, i32)>,
    /// Attachments placed by offset from the paragraph (id, h, v) rather than inline.
    pub anchors: Vec<(u64, f32, f32)>,
    /// Anchored objects wrapped "above and below" (exterior_text_wrap type 2): no text beside them.
    pub above_below_anchors: HashSet<u64>,
    /// The TP.SectionArchive the paragraph belongs to (body storage table_section 17).
    pub section: u64,
    /// Keep with next (paragraph style field 10).
    pub keep_with_next: bool,
    /// For an empty paragraph, the character style at its position (sizes its line).
    pub empty_style: Option<TextStyle>,
    /// Widow and orphan control (paragraph style field 26).
    pub widow_control: bool,
    /// Words that overflow a line break at hyphenation points (document setting and paragraph
    /// style field 8).
    pub hyphenate: bool,
}

impl Paragraph {
    fn new(runs: Vec<Run>, style: TextStyle) -> Paragraph {
        Paragraph {
            runs,
            style,
            bullet: None,
            page_break_before: false,
            bullet_x: 0.0,
            bullet_scale: 1.0,
            attachments: Vec::new(),
            footnotes: Vec::new(),
            anchors: Vec::new(),
            above_below_anchors: HashSet::new(),
            section: 0,
            keep_with_next: false,
            empty_style: None,
            widow_control: false,
            hyphenate: false,
        }
    }

    /// Right-to-left: set on the style, or (natural) the first strong character is RTL.
    pub fn rtl(&self) -> bool {
        let natural = self.style.direction != 0
            && bidi::first_strong_rtl(self.runs.iter().map(|run| run.text.as_str()));
        self.style.direction == 1 || natural
    }
}

/// A measured piece of a word.
#[derive(Debug, Clone)]
struct Piece {
    text: String,
    style: Rc<TextStyle>,
    font: Rc<TaggedFont>,
    width: f32,
}

/// An object anchored to a paragraph: the drawable, its horizontal and vertical offsets, and
/// whether it is wrapped "above and below".
type Anchor = (Rc<Item>, f32, f32, bool);

/// One laid-out line.
#[derive(Clone)]
struct Line {
    /// Pieces with their x offsets.
    pieces: Vec<(Piece, f32)>,
    width: f32,
    height: f32,
    ascent: f32,
    leading: f32,
    /// Index of the paragraph.
    paragraph: usize,
    first: bool,
    last: bool,
    /// Inline objects stacked as this line.
    blocks: Option<Vec<Rc<Item>>>,
    anchors: Option<Vec<Anchor>>,
    /// Space kept free on the left of the line for a wrapped object.
    shift_x: f32,
    /// Width given up to wrapped objects (left plus right).
    narrowed: f32,
}

impl Line {
    fn block(height: f32, paragraph: usize, blocks: Vec<Rc<Item>>) -> Line {
        Line {
            pieces: Vec::new(),
            width: 0.0,
            height,
            ascent: 0.0,
            leading: 0.0,
            paragraph,
            first: false,
            last: false,
            blocks: Some(blocks),
            anchors: None,
            shift_x: 0.0,
            narrowed: 0.0,
        }
    }
}

/// Per (paragraph, line in paragraph): left and right insets for text wrap.
type WrapInsets = HashMap<(usize, usize), (f32, f32)>;

/// Marks a page-relative anchor offset (stored as offset − PAGE_RELATIVE).
const PAGE_RELATIVE: f32 = 1_000_000.0;

fn is_page_relative(vertical_offset: f32) -> bool {
    vertical_offset < -PAGE_RELATIVE / 2.0
}

/// An anchored object's top: from the page top, or from its line.
fn anchor_top(vertical_offset: f32, line_top: f32) -> f32 {
    if is_page_relative(vertical_offset) {
        vertical_offset + PAGE_RELATIVE
    } else {
        line_top + vertical_offset
    }
}

/// A storage's text laid out in paragraphs.
#[derive(Debug, Clone, Default)]
pub struct RichText {
    /// The paragraphs.
    pub paragraphs: Vec<Paragraph>,
    /// Drop the lines that do not fit the drawing rectangle (Keynote sized text boxes).
    pub clip_overflow: bool,
    styles: Option<Styles>,
    wrap: RefCell<Option<WrapInsets>>,
    /// The section of the first line of the page being started.
    page_section: Cell<u64>,
}

/// (character index, object) entries of a storage table, sorted by index.
type Table = Vec<(i32, u64)>;

fn read_table(table: &Proto) -> Table {
    let mut entries: Table = table
        .messages(1)
        .iter()
        .map(|entry| (entry.int(1), entry.reference(2)))
        .collect();
    entries.sort_by_key(|entry| entry.0);
    entries
}

/// The object in effect at a position.
fn at(table: &Table, position: i32) -> u64 {
    table
        .iter()
        .take_while(|entry| entry.0 <= position)
        .last()
        .map_or(0, |entry| entry.1)
}

/// Like `at`, but an entry without an object keeps the one before it.
fn at_keep(table: &Table, position: i32) -> u64 {
    table
        .iter()
        .take_while(|entry| entry.0 <= position)
        .filter(|entry| entry.1 != 0)
        .last()
        .map_or(0, |entry| entry.1)
}

fn next_boundary(table: &Table, position: i32, end: i32) -> i32 {
    table
        .iter()
        .map(|entry| entry.0)
        .find(|&index| index > position && index < end)
        .unwrap_or(end)
}

fn clean(text: &str) -> String {
    text.replace(['\u{FFFC}', '\u{FFFF}'], "")
        .replace('\u{2028}', "\n")
}

/// List number label (ListStyleArchive.NumberType: decimal/roman/alpha × "1." "(1)" "1)").
fn number_label(number: i32, number_type: i32) -> String {
    let (family, punctuation) = if number_type <= 14 {
        (number_type / 3, number_type % 3)
    } else {
        (0, 0)
    };
    let core = number_core(number, family);
    match punctuation {
        1 => format!("({core})"),
        2 => format!("{core})"),
        _ => format!("{core}."),
    }
}

/// A number in a numbering family: 1 upper and 2 lower roman, 3 upper and 4 lower letters,
/// otherwise decimal.
fn number_core(number: i32, family: i32) -> String {
    let roman = |mut value: i32| {
        let mut output = String::new();
        let numerals = [
            (1000, "M"),
            (900, "CM"),
            (500, "D"),
            (400, "CD"),
            (100, "C"),
            (90, "XC"),
            (50, "L"),
            (40, "XL"),
            (10, "X"),
            (9, "IX"),
            (5, "V"),
            (4, "IV"),
            (1, "I"),
        ];
        for (amount, numeral) in numerals {
            while value >= amount {
                output.push_str(numeral);
                value -= amount;
            }
        }
        output
    };
    let alphabetic = |mut value: i32| {
        let mut letters = Vec::new();
        while value > 0 {
            value -= 1;
            letters.push(char::from(b'A' + (value % 26) as u8));
            value /= 26;
        }
        letters.into_iter().rev().collect::<String>()
    };
    match family {
        1 => roman(number),
        2 => roman(number).to_lowercase(),
        3 => alphabetic(number),
        4 => alphabetic(number).to_lowercase(),
        _ => number.to_string(),
    }
}

/// A list level's label and indents.
#[derive(Debug, Clone, PartialEq)]
struct Bullet {
    text: String,
    indent: f32,
    /// In ems of the text.
    text_indent: f32,
    scale: f32,
}

/// TSWP.ListStyleArchive per-level arrays: label_types 11, text_indents 12, indents 13, label
/// geometries 14, number types 15, strings 16.
fn bullet_for(styles: &Styles, list_style: u64, level: usize, number: i32) -> Option<Bullet> {
    let array = |field: i32| styles.find_in(list_style, |object| Some(object.body.clone()), field);
    let types = array(11).map(|holder| holder.uints(11)).unwrap_or_default();
    let level = level.min(types.len().checked_sub(1)?);
    let pick = |values: Vec<f32>| values.get(level).or(values.last()).copied().unwrap_or(0.0);
    let label = match types[level] {
        2 => {
            let strings = array(16)
                .map(|holder| holder.strings(16))
                .unwrap_or_default();
            strings
                .get(level)
                .or(strings.last())
                .cloned()
                .unwrap_or_else(|| "•".into())
        }
        1 => "•".into(),
        3 => {
            let number_types = array(15).map(|holder| holder.uints(15)).unwrap_or_default();
            number_label(
                number,
                number_types
                    .get(level)
                    .or(number_types.last())
                    .map_or(0, |&value| value as i32),
            )
        }
        // A level without a label still indents its text (measured: a kNone list level places
        // Keynote body text 35pt in).
        _ => String::new(),
    };
    let geometries = array(14)
        .map(|holder| holder.messages(14))
        .unwrap_or_default();
    let scale = geometries
        .get(level)
        .or(geometries.last())
        .map_or(1.0, |geometry| geometry.float_or(1, 1.0));
    Some(Bullet {
        text: label,
        indent: pick(
            array(13)
                .map(|holder| holder.floats(13))
                .unwrap_or_default(),
        ),
        text_indent: pick(
            array(12)
                .map(|holder| holder.floats(12))
                .unwrap_or_default(),
        ),
        scale,
    })
}

fn trimmed_width(piece: &Piece) -> f32 {
    let trimmed = piece.text.trim_end_matches(' ');
    if trimmed.len() == piece.text.len() {
        piece.width
    } else {
        shaper::measure(trimmed, &piece.font)
    }
}

/// Reads a storage's tables and turns its text into paragraphs.
struct StorageReader<'a> {
    styles: &'a Styles,
    storage_id: u64,
    base: &'a TextStyle,
    /// The text as UTF-16 code units (the storage's indexes count those).
    units: Vec<u16>,
    /// Field values (page numbers, footnote marks) replacing characters at their index.
    fields: HashMap<i32, String>,
    attachments: Table,
    footnote_references: Vec<(i32, (u64, i32))>,
    paragraph_styles: Table,
    character_styles: Table,
    sections: Table,
    list_styles: Table,
    /// (index, list level).
    list_levels: Vec<(i32, i32)>,
    /// (index, right-to-left flag).
    directions: Vec<(i32, i32)>,
}

impl<'a> StorageReader<'a> {
    fn new(
        styles: &'a Styles,
        storage_id: u64,
        storage: &Proto,
        base: &'a TextStyle,
    ) -> StorageReader<'a> {
        // table_attachment (9): character index -> attachment object; footnote references live in
        // their own table (table_footnote, 16).
        let mut attachments = read_table(&storage.message(9));
        attachments.extend(read_table(&storage.message(16)));
        attachments.retain(|entry| entry.1 != 0);
        let mut units: Vec<u16> = storage.strings(3).concat().encode_utf16().collect();
        // A footnote reference may sit on U+000E (measured: Pages 14 footnotes); it is the
        // reference, not a column break.
        for entry in storage.message(16).messages(1) {
            if let Some(unit) = usize::try_from(entry.int(1))
                .ok()
                .and_then(|index| units.get_mut(index))
                .filter(|unit| **unit == 0x0E)
            {
                *unit = 0xFFFC;
            }
        }
        // Tracked deletions (table_deletion 22) are not exported (measured: "Heading 2" with " 2"
        // deleted prints "Heading").
        let deletions = read_table(&storage.message(22));
        for (index, &(start, object)) in deletions.iter().enumerate() {
            let end = deletions
                .get(index + 1)
                .map_or(units.len(), |next| usize::try_from(next.0).unwrap_or(0));
            if object != 0 {
                let start = usize::try_from(start).unwrap_or(0).min(units.len());
                let end = end.clamp(start, units.len());
                units[start..end].fill(0xFFFF);
            }
        }
        let mut paragraph_styles = read_table(&storage.message(5));
        // An entry without a style object continues the previous paragraph's style, variation
        // included (measured: Keynote code lines keep "Medium White Code").
        for index in 1..paragraph_styles.len() {
            if paragraph_styles[index].1 == 0 {
                paragraph_styles[index].1 = paragraph_styles[index - 1].1;
            }
        }
        let pairs = |field: i32| {
            storage
                .message(field)
                .messages(1)
                .iter()
                .map(|entry| (entry.int(1), entry.int(2)))
                .collect()
        };
        let mut reader = StorageReader {
            styles,
            storage_id,
            base,
            units,
            fields: HashMap::new(),
            attachments,
            footnote_references: Vec::new(),
            paragraph_styles,
            character_styles: read_table(&storage.message(8)),
            sections: read_table(&storage.message(17)),
            list_styles: read_table(&storage.message(7)),
            list_levels: pairs(6),
            // table_para_bidi: first = 1 marks right-to-left paragraphs.
            directions: pairs(24),
        };
        reader.read_fields();
        reader
    }

    /// Page-number fields (TSWP.NumberAttachmentArchive: kind 0 page number, 1 page count),
    /// footnote references (2008, numbered in order) and the mark inside a footnote (2004).
    fn read_fields(&mut self) {
        let doc = self.styles.doc();
        for &(index, id) in &self.attachments {
            let Some(object) = doc.get(id) else { continue };
            match object.typ {
                2043 => {
                    // TSWP.NumberAttachmentArchive: kind in its super (1.2, 1: page count) and
                    // number_format (2, the list number families: 2 is "lower-roman").
                    let count = object.body.message(1).int(2) == 1;
                    let value = if count {
                        ctx::PAGE_COUNT.get()
                    } else {
                        ctx::PAGE_NUMBER.get()
                    };
                    self.fields
                        .insert(index, number_core(value, object.body.int(2)));
                }
                2008 => {
                    let number = ctx::FOOTNOTE_COUNTER.get() + 1;
                    ctx::FOOTNOTE_COUNTER.set(number);
                    let custom = object.body.string(3).filter(|mark| !mark.is_empty());
                    self.fields
                        .insert(index, custom.unwrap_or_else(|| number.to_string()));
                    self.footnote_references
                        .push((index, (object.body.reference(2), number)));
                }
                2004 if object.body.int(2) == 2 => {
                    self.fields
                        .insert(index, ctx::FOOTNOTE_MARK.get().to_string());
                }
                _ => {}
            }
        }
    }

    /// The text of [from, to) with fields substituted.
    fn text(&self, from: usize, to: usize) -> String {
        let mut output = String::new();
        let mut position = from;
        while position < to {
            if let Some(value) = self.fields.get(&(position as i32)) {
                output.push_str(value);
                position += 1;
                continue;
            }
            let end = (position + 1..to)
                .find(|&index| self.fields.contains_key(&(index as i32)))
                .unwrap_or(to);
            output.push_str(&String::from_utf16_lossy(&self.units[position..end]));
            position = end;
        }
        output
    }

    fn runs(&self, (start, end): (usize, usize), paragraph_style: &TextStyle) -> Vec<Run> {
        let mut runs = Vec::new();
        let mut position = start;
        while position < end {
            let next = next_boundary(&self.character_styles, position as i32, end as i32) as usize;
            let character_style = at(&self.character_styles, position as i32);
            let style = if character_style != 0 {
                TextStyle::resolve(self.styles, 0, character_style, Some(paragraph_style))
            } else {
                paragraph_style.clone()
            };
            let chunk = clean(&self.text(position, next));
            if !chunk.is_empty() {
                runs.push(Run {
                    text: style.cased(&chunk),
                    style: Rc::new(style),
                });
            }
            position = next;
        }
        runs
    }
}

/// Inline and anchored objects of one paragraph.
#[derive(Default)]
struct ParagraphObjects {
    inline: Vec<u64>,
    anchors: Vec<(u64, f32, f32)>,
    above_below: HashSet<u64>,
    table_of_contents: Option<Vec<Paragraph>>,
}

impl RichText {
    /// A copy with every font size (and fixed line spacing, paragraph spacing and indents) scaled.
    ///
    /// # Arguments
    ///
    /// * `factor` - The scale (shrink-to-fit steps by 1%)
    ///
    /// # Returns
    ///
    /// The scaled text.
    pub fn scaled(&self, factor: f32) -> RichText {
        let scale = |style: &TextStyle| TextStyle {
            size: style.size * factor,
            line_spacing: if matches!(style.line_spacing_mode, 1..=4) {
                style.line_spacing * factor
            } else {
                style.line_spacing
            },
            space_before: style.space_before * factor,
            space_after: style.space_after * factor,
            // List text indents are in ems, so they shrink with the text (measured: a 0.921em indent
            // at a shrunk 32pt is 29.5pt).
            left_indent: style.left_indent * factor,
            first_indent: style.first_indent * factor,
            ..style.clone()
        };
        let paragraphs = self
            .paragraphs
            .iter()
            .map(|paragraph| Paragraph {
                runs: paragraph
                    .runs
                    .iter()
                    .map(|run| Run {
                        text: run.text.clone(),
                        style: Rc::new(scale(&run.style)),
                    })
                    .collect(),
                style: scale(&paragraph.style),
                ..paragraph.clone()
            })
            .collect();
        RichText {
            paragraphs,
            styles: self.styles.clone(),
            ..RichText::default()
        }
    }

    // ------------------------------------------------------------------ build

    /// Reads a storage.
    ///
    /// # Arguments
    ///
    /// * `styles` - The document's styles
    /// * `storage_id` - The TSWP.StorageArchive
    /// * `base` - What the styles leave unset
    /// * `default_paragraph_style` - The style of paragraphs without one (0: none)
    ///
    /// # Returns
    ///
    /// The paragraphs (empty when the storage is missing).
    pub fn from_storage(
        styles: &Styles,
        storage_id: u64,
        base: &TextStyle,
        default_paragraph_style: u64,
    ) -> RichText {
        let mut text = RichText {
            styles: Some(styles.clone()),
            ..RichText::default()
        };
        let Some(storage) = styles.doc().body(storage_id) else {
            return text;
        };
        let reader = StorageReader::new(styles, storage_id, &storage, base);
        let mut start = 0usize;
        let mut page_break = false;
        let mut counters: HashMap<usize, i32> = HashMap::new();
        let mut list_at: HashMap<usize, u64> = HashMap::new();
        loop {
            let separator = reader.units[start.min(reader.units.len())..]
                .iter()
                .position(|&unit| matches!(unit, 0x0A | 0x0D | 0x2029 | 0x0C | 0x05 | 0x04 | 0x0E));
            let end = separator.map_or(reader.units.len(), |offset| start + offset);
            let paragraph = text.read_paragraph(
                &reader,
                (start, end),
                default_paragraph_style,
                (&mut counters, &mut list_at),
            );
            text.push_paragraph(&reader, paragraph, (start, page_break));
            if separator.is_none() {
                break;
            }
            page_break = matches!(reader.units[end], 0x0C | 0x05 | 0x04); // page and section breaks
            start = end + 1;
        }
        text
    }

    /// One paragraph of a storage: its style, runs, list label and objects.
    fn read_paragraph(
        &self,
        reader: &StorageReader,
        (start, end): (usize, usize),
        default_style: u64,
        (counters, list_at): (&mut HashMap<usize, i32>, &mut HashMap<usize, u64>),
    ) -> (Paragraph, ParagraphObjects, bool) {
        let position = start as i32;
        let style_id = match at(&reader.paragraph_styles, position) {
            0 => default_style,
            id => id,
        };
        let mut style = TextStyle::resolve(reader.styles, style_id, 0, Some(reader.base));
        if reader
            .directions
            .iter()
            .rev()
            .find(|entry| entry.0 <= position)
            .is_some_and(|entry| entry.1 == 1)
        {
            style.direction = 1;
        }
        // Space between paragraphs collapses to the larger of the two (measured: 15.6pt before and
        // after give 15.6pt between paragraphs, not 31.2).
        if let Some(previous) = self.paragraphs.last() {
            style.space_before = (style.space_before - previous.style.space_after).max(0.0);
        }
        let mut runs = reader.runs((start, end), &style);
        let list_style = at(&reader.list_styles, position);
        let level = reader
            .list_levels
            .iter()
            .take_while(|entry| entry.0 <= position)
            .last()
            .map_or(0, |entry| usize::try_from(entry.1).unwrap_or(0));
        // Numbered lists count per level: a deeper level restarts under each new item, a different
        // list style at the level restarts it, and a paragraph outside any list restarts them all.
        if list_style == 0 {
            counters.clear();
            list_at.clear();
        } else {
            if list_at.get(&level) != Some(&list_style) {
                counters.remove(&level);
            }
            list_at.insert(level, list_style);
            *counters.entry(level).or_insert(0) += 1;
            counters.retain(|&other, _| other <= level);
        }
        let mut bullet_text = None;
        let (mut bullet_x, mut bullet_scale) = (0.0, 1.0);
        let first_size = runs.first().map_or(style.size, |run| run.style.size);
        let bullet = (list_style != 0)
            .then(|| {
                bullet_for(
                    reader.styles,
                    list_style,
                    level,
                    counters.get(&level).copied().unwrap_or(1),
                )
            })
            .flatten();
        if let Some(bullet) = bullet {
            let text_start = bullet.indent + bullet.text_indent * first_size;
            if bullet.text.is_empty() {
                // No label: the level's indents add to the paragraph's own.
                style.left_indent += text_start;
                style.first_indent += text_start;
            } else {
                // List indents replace the paragraph's own; the text starts after the label on every
                // line. A label wider than the text indent pushes the first line's text to its end
                // (measured: "1." at 100pt with a 0.18em indent).
                let label_style = runs
                    .first()
                    .map_or_else(|| style.clone(), |run| (*run.style).clone());
                let label_font = TextStyle {
                    size: label_style.size * bullet.scale,
                    ..label_style
                }
                .font();
                style.left_indent = text_start;
                style.first_indent = text_start
                    .max(bullet.indent + shaper::measure(bullet.text.trim_end(), &label_font));
                (bullet_x, bullet_scale) = (bullet.indent, bullet.scale);
                bullet_text = Some(bullet.text);
            }
            if text_start != 0.0 || bullet_text.is_some() {
                let left_indent = style.left_indent;
                for run in &mut runs {
                    run.style = Rc::new(TextStyle {
                        left_indent,
                        ..(*run.style).clone()
                    });
                }
            }
        }
        let objects = self.paragraph_objects(reader, (start, end));
        // Paragraph style page_break_before (14), keep_with_next (10) and widow control (26).
        let flag = |field: i32| {
            style_id != 0
                && reader
                    .styles
                    .find(style_id, 12, field)
                    .is_some_and(|props| props.boolean(field))
        };
        // An empty paragraph's line is as tall as the character style at its position.
        let empty_character_style = at(&reader.character_styles, position);
        let empty_style = (runs.is_empty() && empty_character_style != 0)
            .then(|| TextStyle::resolve(reader.styles, 0, empty_character_style, Some(&style)));
        let mut paragraph = Paragraph::new(runs, style);
        paragraph.bullet = bullet_text;
        paragraph.bullet_x = bullet_x;
        paragraph.bullet_scale = bullet_scale;
        paragraph.keep_with_next = flag(10);
        paragraph.widow_control = flag(26);
        // Hyphenation is on unless the paragraph style turns it off.
        paragraph.hyphenate = ctx::HYPHENATION.get()
            && reader
                .styles
                .find(style_id, 12, 8)
                .is_none_or(|props| props.boolean(8));
        paragraph.empty_style = empty_style;
        paragraph.section = at_keep(&reader.sections, position);
        paragraph.footnotes = reader
            .footnote_references
            .iter()
            .filter(|reference| (position..end as i32).contains(&reference.0))
            .map(|reference| reference.1)
            .collect();
        (paragraph, objects, flag(14))
    }

    /// Tables and zero-offset objects flow inline; others are anchored by their offsets
    /// (TSWP.DrawableAttachmentArchive: drawable 1, h_offset 3, v_offset 5).
    fn paragraph_objects(
        &self,
        reader: &StorageReader,
        (start, end): (usize, usize),
    ) -> ParagraphObjects {
        let doc = reader.styles.doc();
        let mut objects = ParagraphObjects::default();
        for &(_, id) in reader
            .attachments
            .iter()
            .filter(|entry| (start as i32..end as i32).contains(&entry.0))
        {
            // A table of contents (TSWP.TOCAttachmentArchive 2241 → TOCInfoArchive 2240, a text box)
            // keeps its generated entries in its own storage: they flow as body paragraphs.
            let toc_info = doc
                .get(id)
                .filter(|object| object.typ == 2241)
                .and_then(|attachment| doc.get(attachment.body.message(1).reference(1)));
            if let Some(info) = toc_info.filter(|info| info.typ == 2240) {
                let toc_storage = info.body.message(1).reference(4);
                if toc_storage != 0 && toc_storage != reader.storage_id {
                    let entries =
                        RichText::from_storage(reader.styles, toc_storage, reader.base, 0)
                            .paragraphs;
                    objects
                        .table_of_contents
                        .get_or_insert_with(Vec::new)
                        .extend(entries);
                    continue;
                }
            }
            let Some(attachment) = doc.get(id).filter(|object| object.typ == 2003) else {
                continue;
            };
            let drawable_id = attachment.body.reference(1);
            let Some(drawable) = doc.get(drawable_id) else {
                continue;
            };
            let (horizontal, vertical) = (attachment.body.float(3), attachment.body.float(5));
            let wrap_type = drawables::wrap_of(drawable).map(|wrap| wrap.int(1));
            let page_relative = attachment.body.int(4) == 1;
            // Wrap type 0 is "inline with text", whatever offsets remain stored (measured: a photo
            // with a 43pt v_offset sits at its line).
            if drawable.typ == 6000
                || (horizontal == 0.0 && vertical == 0.0)
                || (wrap_type == Some(0) && !page_relative)
            {
                objects.inline.push(drawable_id);
                continue;
            }
            // v_offset_type (4) 1: the vertical offset is from the page top, not the paragraph
            // (measured: an anchored chart 84pt down sits 84pt below the page edge).
            objects.anchors.push((
                drawable_id,
                horizontal,
                if page_relative {
                    vertical - PAGE_RELATIVE
                } else {
                    vertical
                },
            ));
            if wrap_type == Some(2) {
                objects.above_below.insert(drawable_id);
            }
        }
        objects
    }

    fn push_paragraph(
        &mut self,
        reader: &StorageReader,
        (mut paragraph, objects, style_break): (Paragraph, ParagraphObjects, bool),
        (start, page_break): (usize, bool),
    ) {
        let breaks = page_break || (style_break && !self.paragraphs.is_empty());
        let section = at_keep(&reader.sections, start as i32);
        let has_contents = objects.table_of_contents.is_some();
        if let Some(mut entries) = objects
            .table_of_contents
            .filter(|entries| !entries.is_empty())
        {
            if breaks {
                entries[0].page_break_before = true;
            }
            for mut entry in entries {
                entry.section = section;
                self.paragraphs.push(entry);
            }
        }
        if has_contents
            && paragraph.runs.is_empty()
            && objects.inline.is_empty()
            && objects.anchors.is_empty()
        {
            return;
        }
        paragraph.page_break_before = !has_contents && breaks;
        paragraph.attachments = objects.inline;
        paragraph.anchors = objects.anchors;
        paragraph.above_below_anchors = objects.above_below;
        self.paragraphs.push(paragraph);
    }

    fn create_items(&self, ids: &[u64]) -> Vec<Rc<Item>> {
        let Some(styles) = &self.styles else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|&id| {
                styles
                    .doc()
                    .get(id)
                    .and_then(|object| drawables::create(styles, object))
            })
            .map(Rc::new)
            .collect()
    }

    // ----------------------------------------------------------------- layout

    fn layout(&self, width: f32, wrap: bool) -> Vec<Line> {
        let insets = self.wrap.borrow();
        let mut lines: Vec<Line> = Vec::new();
        for (index, paragraph) in self.paragraphs.iter().enumerate() {
            // A paragraph holding only inline objects (plus whitespace): one block as tall as them.
            if !paragraph.attachments.is_empty()
                && paragraph.runs.iter().all(|run| run.text.trim().is_empty())
            {
                let items = self.create_items(&paragraph.attachments);
                if !items.is_empty() {
                    let height = items.iter().map(|item| item.flow_bounds().height()).sum();
                    lines.push(Line {
                        first: true,
                        last: true,
                        ..Line::block(height, index, items)
                    });
                    continue;
                }
            }
            let start = lines.len();
            let mut builder = LineBuilder::new(paragraph, index, (width, start), insets.as_ref());
            for word in split_words(paragraph) {
                match word {
                    Some(pieces) => builder.add_word(pieces, wrap, &mut lines),
                    None => builder.flush(&mut lines),
                }
            }
            builder.flush(&mut lines);
            if !paragraph.anchors.is_empty() && start < lines.len() {
                if let Some(styles) = &self.styles {
                    let anchored =
                        paragraph
                            .anchors
                            .iter()
                            .filter_map(|&(id, horizontal, vertical)| {
                                let item = styles
                                    .doc()
                                    .get(id)
                                    .and_then(|object| drawables::create(styles, object))?;
                                Some((
                                    Rc::new(item),
                                    horizontal,
                                    vertical,
                                    paragraph.above_below_anchors.contains(&id),
                                ))
                            });
                    lines[start].anchors = Some(anchored.collect());
                }
            }
            // Inline objects after the paragraph's text get a block line of their own.
            let items = self.create_items(&paragraph.attachments);
            if !items.is_empty() {
                let height = items.iter().map(|item| item.flow_bounds().height()).sum();
                lines.push(Line::block(height, index, items));
            }
            if let Some(last) = lines.last_mut() {
                last.last = true;
            }
            if let Some(first) = lines.get_mut(start) {
                first.first = true;
            }
        }
        lines
    }

    /// Total height of the laid-out text (last line's leading dropped).
    pub fn measure(&self, width: f32, wrap: bool) -> f32 {
        self.height(&self.layout(width, wrap))
    }

    /// Height of the laid-out text block, wrapped to `width` (last line's leading excluded).
    pub fn block_height(&self, width: f32) -> f32 {
        self.measure(width, true)
    }

    fn height(&self, lines: &[Line]) -> f32 {
        let Some(last) = lines.last() else { return 0.0 };
        let mut height = 0.0;
        for (index, line) in lines.iter().enumerate() {
            let style = &self.paragraphs[line.paragraph].style;
            // Space before the first paragraph of a text frame is not applied (measured: a centred
            // Keynote body with 42pt space-before sits where a block without it would).
            if line.first && index != 0 {
                height += style.space_before;
            }
            height += line.height;
            if line.last {
                height += style.space_after;
            }
        }
        height - last.leading - self.paragraphs[last.paragraph].style.space_after
    }

    /// Effective alignment: "natural" follows the writing direction, and so does the last line of a
    /// justified right-to-left paragraph (measured: Hebrew lines export flush right).
    fn align(&self, line: &Line, natural: HAlign) -> HAlign {
        let paragraph = &self.paragraphs[line.paragraph];
        match paragraph.style.align {
            HAlign::Natural if paragraph.rtl() => HAlign::Right,
            HAlign::Natural => natural,
            HAlign::Justify if paragraph.rtl() && line.last => HAlign::Right,
            other => other,
        }
    }

    /// Width added to each space of a justified line (not its last) so it fills the column.
    fn justify_extra(&self, line: &Line, width: f32, natural: HAlign) -> f32 {
        let paragraph = &self.paragraphs[line.paragraph];
        if self.align(line, natural) != HAlign::Justify
            || line.last
            || line.pieces.len() < 2
            || paragraph.rtl()
        {
            return 0.0;
        }
        let style = &paragraph.style;
        let indent = if line.first {
            style.first_indent
        } else {
            style.left_indent
        };
        let available = width - indent - style.right_indent - line.narrowed;
        let spaces: usize = line.pieces[..line.pieces.len() - 1]
            .iter()
            .map(|(piece, _)| piece.text.matches(' ').count())
            .sum();
        if spaces == 0 {
            0.0
        } else {
            (available - line.width) / spaces as f32
        }
    }

    /// Where a line starts: indent plus alignment within the available width.
    fn line_start(&self, line: &Line, (left, width): (f32, f32), natural: HAlign) -> f32 {
        let style = &self.paragraphs[line.paragraph].style;
        let indent = if line.first {
            style.first_indent
        } else {
            style.left_indent
        };
        let available = width - indent - style.right_indent - line.narrowed;
        left + indent
            + line.shift_x
            + match self.align(line, natural) {
                HAlign::Right => available - line.width,
                HAlign::Center => (available - line.width) / 2.0,
                _ => 0.0,
            }
    }

    /// Paragraph fill across the column and character backgrounds behind their text, each over the
    /// line's height (measured: Numbers cell text highlights).
    fn draw_backgrounds(&self, canvas: &Canvas, line: &Line, origin: Point, column: (f32, f32)) {
        if let Some(fill) = self.paragraphs[line.paragraph].style.para_fill {
            canvas.draw_rect(
                Rect::new(column.0, origin.y, column.1, origin.y + line.height),
                &plain_paint(fill),
            );
        }
        for (index, (piece, offset)) in line.pieces.iter().enumerate() {
            let Some(background) = piece.style.background else {
                continue;
            };
            // Spaces between words are highlighted too; a wrapped line's trailing space is not, but
            // a paragraph's own trailing spaces are (measured: padded code lines in Keynote).
            let width = if index == line.pieces.len() - 1 && !line.last {
                trimmed_width(piece)
            } else {
                piece.width
            };
            let left = origin.x + offset;
            canvas.draw_rect(
                Rect::new(left, origin.y, left + width, origin.y + line.height),
                &plain_paint(background),
            );
        }
    }

    /// The line's pieces in visual order (bidi-reordered when it holds right-to-left text).
    fn visual(&self, line: &Line) -> Vec<(Piece, f32)> {
        let rtl = self.paragraphs[line.paragraph].rtl();
        if !rtl
            && !line
                .pieces
                .iter()
                .any(|(piece, _)| bidi::has_rtl(&piece.text))
        {
            return line.pieces.clone();
        }
        let mut logical: Vec<(String, usize)> = line
            .pieces
            .iter()
            .enumerate()
            .filter(|(_, (piece, _))| piece.text != "\t")
            .map(|(index, (piece, _))| (piece.text.clone(), index))
            .collect();
        if let Some(last) = logical.last_mut() {
            last.0 = last.0.trim_end().to_owned();
        }
        let mut pen = 0.0;
        bidi::reorder(&logical, rtl)
            .into_iter()
            .map(|(text, index)| {
                let source = &line.pieces[index].0;
                let width = shaper::measure(&text, &source.font);
                let placed = (
                    Piece {
                        text,
                        width,
                        ..source.clone()
                    },
                    pen,
                );
                pen += width;
                placed
            })
            .collect()
    }

    /// The list label on a paragraph's first line. A scaled label sits lower so it stays centred
    /// on the text (measured: +0.35 of the growth).
    fn draw_bullet(&self, canvas: &Canvas, line: &Line, left: f32, baseline: f32) {
        let paragraph = &self.paragraphs[line.paragraph];
        let (true, Some(label), Some((first, _))) =
            (line.first, &paragraph.bullet, line.pieces.first())
        else {
            return;
        };
        let text_style = &first.style;
        let label_style = TextStyle {
            size: text_style.size * paragraph.bullet_scale,
            ..(**text_style).clone()
        };
        let origin = Point::new(
            left + paragraph.bullet_x,
            baseline + 0.35 * (label_style.size - text_style.size),
        );
        text::draw_line(canvas, label, origin, &label_style.font(), &label_style);
    }

    /// Draws into a rectangle.
    ///
    /// # Arguments
    ///
    /// * `canvas` - The canvas
    /// * `rect` - The text column; lines wrap to its width
    /// * `vertical_align` - 0 top, 1 middle, 2 bottom
    /// * `wrap` - False keeps each paragraph on one line
    /// * `natural` - The alignment "natural" paragraphs take
    pub fn draw(
        &self,
        canvas: &Canvas,
        rect: Rect,
        vertical_align: i32,
        wrap: bool,
        natural: HAlign,
    ) {
        let mut lines = self.layout(rect.width(), wrap);
        // Keynote does not export lines that overflow a sized text box: a line is kept while its
        // baseline is inside the box (measured: a code box keeps a last "}" whose descent overflows;
        // a body box drops a line whose baseline is 44pt out).
        while self.clip_overflow && lines.len() > 1 {
            let last = &lines[lines.len() - 1];
            let below_baseline = last.height - last.ascent;
            if self.height(&lines) - below_baseline <= rect.height() + 0.5 {
                break;
            }
            lines.pop();
        }
        let total = self.height(&lines);
        let mut line_top = match vertical_align {
            1 => rect.top + (rect.height() - total) / 2.0,
            2 => rect.bottom - total,
            _ => rect.top,
        };
        for (index, line) in lines.iter().enumerate() {
            let paragraph = &self.paragraphs[line.paragraph];
            let style = &paragraph.style;
            if line.first && index != 0 {
                line_top += style.space_before;
            }
            let left = self.line_start(line, (rect.left, rect.width()), natural);
            let baseline = line_top + line.ascent;
            self.draw_bullet(canvas, line, rect.left, baseline);
            let extra = self.justify_extra(line, rect.width(), natural);
            self.draw_backgrounds(
                canvas,
                line,
                Point::new(left, line_top),
                (rect.left, rect.right),
            );
            let mut shift = 0.0;
            for (piece, offset) in self.visual(line) {
                text::draw_line(
                    canvas,
                    piece.text.trim_end(),
                    Point::new(left + offset + shift, baseline),
                    &piece.font,
                    &piece.style,
                );
                shift += extra * piece.text.matches(' ').count() as f32;
            }
            line_top += line.height;
            if line.last {
                line_top += style.space_after;
            }
        }
    }

    /// Union of the laid-out line boxes when drawn into a rectangle (zero when no line has text).
    pub fn text_bounds(
        &self,
        rect: Rect,
        vertical_align: i32,
        wrap: bool,
        natural: HAlign,
    ) -> Rect {
        let lines = self.layout(rect.width(), wrap);
        let total = self.height(&lines);
        let mut line_top = match vertical_align {
            1 => rect.top + (rect.height() - total) / 2.0,
            2 => rect.bottom - total,
            _ => rect.top,
        };
        let mut bounds = Rect::default();
        for (index, line) in lines.iter().enumerate() {
            let style = &self.paragraphs[line.paragraph].style;
            if line.first && index != 0 {
                line_top += style.space_before;
            }
            if line.width > 0.0 {
                let left = self.line_start(line, (rect.left, rect.width()), natural);
                let line_box = Rect::new(left, line_top, left + line.width, line_top + line.height);
                bounds = if is_zero(&bounds) {
                    line_box
                } else {
                    union(bounds, line_box)
                };
            }
            line_top += line.height;
            if line.last {
                line_top += style.space_after;
            }
        }
        bounds
    }

    /// Flows the text through successive page areas: a line that would cross the bottom of the area
    /// (its leading excluded) starts the next page. Text wraps beside objects that leave room: the
    /// lines meeting them are measured, the text laid out again with those lines narrowed, and so on
    /// until the placement settles.
    ///
    /// # Arguments
    ///
    /// * `sink` - Where pages go
    /// * `area` - The text column on every page
    /// * `natural` - The alignment "natural" paragraphs take
    /// * `obstacles` - Per page (0-based), the wrap rectangles of floating objects
    ///
    /// # Returns
    ///
    /// The number of pages used.
    pub fn draw_flow(
        &self,
        sink: &mut dyn PageSink,
        area: Rect,
        natural: HAlign,
        obstacles: Option<&dyn Fn(i32) -> Rc<Vec<Rect>>>,
    ) -> i32 {
        if obstacles.is_some()
            || self
                .paragraphs
                .iter()
                .any(|paragraph| !paragraph.anchors.is_empty())
        {
            *self.wrap.borrow_mut() = None;
            for _ in 0..4 {
                let mut found = WrapInsets::new();
                Flow::new(self, &mut Scratch::default(), (area, natural), obstacles)
                    .run(Some(&mut found));
                let mut current = self.wrap.borrow_mut();
                let close = |insets: &(f32, f32), other: &(f32, f32)| {
                    (insets.0 - other.0).abs() < 0.5 && (insets.1 - other.1).abs() < 0.5
                };
                let settled = current.as_ref().is_some_and(|current| {
                    found.len() == current.len()
                        && found.iter().all(|(key, insets)| {
                            current.get(key).is_some_and(|other| close(insets, other))
                        })
                });
                if found.is_empty() || settled {
                    break;
                }
                current.get_or_insert_with(WrapInsets::new).extend(found);
            }
        }
        Flow::new(self, sink, (area, natural), obstacles).run(None)
    }

    /// The section of the first line of the page being started.
    pub fn page_section(&self) -> u64 {
        self.page_section.get()
    }
}

/// Byte offsets where `word` may be hyphenated: inside its letters (US English patterns), never
/// in a word holding digits or other symbols between its letters.
fn hyphenation_points(word: &str) -> Vec<usize> {
    use hyphenation::{Hyphenator, Language, Load, Standard};
    thread_local! {
        static DICTIONARY: Option<Standard> = Standard::from_embedded(Language::EnglishUS).ok();
    }
    let start = word.find(char::is_alphabetic).unwrap_or(word.len());
    let end = word.rfind(char::is_alphabetic).map_or(start, |index| {
        index + word[index..].chars().next().map_or(0, char::len_utf8)
    });
    let letters = &word[start..end];
    if letters.is_empty() || !letters.chars().all(char::is_alphabetic) {
        return Vec::new();
    }
    DICTIONARY.with(|dictionary| {
        dictionary.as_ref().map_or_else(Vec::new, |dictionary| {
            dictionary
                .hyphenate(letters)
                .breaks
                .into_iter()
                .map(|index| start + index)
                .collect()
        })
    })
}

/// Splits a paragraph into words that keep their trailing spaces; tabs are words of their own and
/// explicit line breaks are `None`.
fn split_words(paragraph: &Paragraph) -> Vec<Option<Vec<Piece>>> {
    let mut words: Vec<Option<Vec<Piece>>> = Vec::new();
    let mut current: Vec<Piece> = Vec::new();
    for run in &paragraph.runs {
        let font = Rc::new(run.style.font());
        let piece = |text: String, width: f32| Piece {
            text,
            style: Rc::clone(&run.style),
            font: Rc::clone(&font),
            width,
        };
        let characters: Vec<char> = run.text.chars().collect();
        let mut start = 0;
        while start < characters.len() {
            match characters[start] {
                '\n' | '\t' => {
                    // A line break's own font sizes the line it ends (measured: a Courier line
                    // ended by a Helvetica break is 0.3pt taller).
                    if characters[start] == '\n' && !current.is_empty() {
                        current.push(piece(String::new(), 0.0));
                    }
                    if !current.is_empty() {
                        words.push(Some(std::mem::take(&mut current)));
                    }
                    words.push(if characters[start] == '\n' {
                        None
                    } else {
                        Some(vec![piece("\t".into(), 0.0)])
                    });
                    start += 1;
                    continue;
                }
                _ => {}
            }
            let mut end = start;
            while end < characters.len()
                && characters[end] != '\n'
                && characters[end] != '\t'
                && !break_char(&characters, end)
            {
                end += 1;
            }
            // A dash or slash break ends its word with that character; spaces after a word stay with
            // it (a following dash starts the next word: "sætte" | "— afsnits").
            if end < characters.len() && characters[end] != ' ' && break_char(&characters, end) {
                end += 1;
            }
            while end < characters.len() && characters[end] == ' ' {
                end += 1;
            }
            let end = end.max(start + 1);
            let segment: String = characters[start..end].iter().collect();
            // A word's trailing space kerns with the next word's first letter (Helvetica Bold
            // " A": the next word starts 1.5pt earlier at 41pt), so that pair joins its width.
            let width = match characters.get(end) {
                Some(&next) if characters[end - 1] == ' ' && !matches!(next, '\n' | '\t') => {
                    let next = next.to_string();
                    shaper::measure(&format!("{segment}{next}"), &font)
                        - shaper::measure(&next, &font)
                }
                _ => shaper::measure(&segment, &font),
            };
            current.push(piece(segment, width));
            if break_char(&characters, end - 1) {
                words.push(Some(std::mem::take(&mut current)));
            }
            start = end;
        }
    }
    if !current.is_empty() {
        words.push(Some(current));
    }
    words
}

/// A pending tab: alignment, stop (from the line's indent), first piece after it, pen where it
/// started, leader.
type PendingTab = (i32, f32, usize, f32, Option<String>);

/// The line being filled while a paragraph is laid out.
struct LineBuilder<'a> {
    paragraph: &'a Paragraph,
    index: usize,
    width: f32,
    paragraph_start: usize,
    insets: Option<&'a WrapInsets>,
    pieces: Vec<(Piece, f32)>,
    pen: f32,
    first: bool,
    tab: Option<PendingTab>,
}

impl<'a> LineBuilder<'a> {
    fn new(
        paragraph: &'a Paragraph,
        index: usize,
        (width, paragraph_start): (f32, usize),
        insets: Option<&'a WrapInsets>,
    ) -> LineBuilder<'a> {
        LineBuilder {
            paragraph,
            index,
            width,
            paragraph_start,
            insets,
            pieces: Vec::new(),
            pen: 0.0,
            first: true,
            tab: None,
        }
    }

    fn style(&self) -> &TextStyle {
        &self.paragraph.style
    }

    /// The first-line indent is absolute, as in Cocoa, not added to the left indent (measured: left
    /// 36 / first 9 starts the first line at 9).
    fn indent(&self) -> f32 {
        if self.first {
            self.style().first_indent
        } else {
            self.style().left_indent
        }
    }

    fn inset(&self, lines: &[Line]) -> (f32, f32) {
        let key = (self.index, lines.len() - self.paragraph_start);
        self.insets
            .and_then(|insets| insets.get(&key).copied())
            .unwrap_or((0.0, 0.0))
    }

    fn available(&self, lines: &[Line]) -> f32 {
        let (left, right) = self.inset(lines);
        self.width - self.indent() - self.style().right_indent - left - right
    }

    fn add_word(&mut self, word: Vec<Piece>, wrap: bool, lines: &mut Vec<Line>) {
        let Some(last) = word.last() else { return };
        let word_width: f32 =
            word.iter().map(|piece| piece.width).sum::<f32>() - last.width + trimmed_width(last);
        let mut word = word;
        if wrap && !self.pieces.is_empty() && self.pen + word_width > self.available(lines) + 0.01 {
            if let Some(rest) = self.hyphenate(&word, lines) {
                word = vec![rest];
            }
            self.flush(lines);
        }
        for piece in word {
            if piece.text == "\t" {
                self.tab();
                continue;
            }
            if wrap
                && self.pieces.is_empty()
                && trimmed_width(&piece) > self.available(lines) + 0.01
                && piece.text.chars().count() > 1
            {
                self.break_long_word(piece, lines);
                continue;
            }
            let width = piece.width;
            self.pieces.push((piece, self.pen));
            self.pen += width;
        }
    }

    /// Ends the line with the longest hyphenated start of a one-piece word that fits after the pen.
    ///
    /// # Returns
    ///
    /// The rest of the word, or `None` when the paragraph does not hyphenate or nothing fits.
    fn hyphenate(&mut self, word: &[Piece], lines: &[Line]) -> Option<Piece> {
        // ponytail: words split across style runs are not hyphenated.
        let [piece] = word else { return None };
        if !self.paragraph.hyphenate {
            return None;
        }
        let room = self.available(lines) - self.pen + 0.01;
        let split = hyphenation_points(&piece.text)
            .into_iter()
            .rev()
            .find(|&split| {
                shaper::measure(&format!("{}-", &piece.text[..split]), &piece.font) <= room
            })?;
        let head = format!("{}-", &piece.text[..split]);
        let tail = piece.text[split..].to_string();
        let (head_width, tail_width) = (
            shaper::measure(&head, &piece.font),
            shaper::measure(&tail, &piece.font),
        );
        self.pieces.push((
            Piece {
                text: head,
                width: head_width,
                ..piece.clone()
            },
            self.pen,
        ));
        self.pen += head_width;
        Some(Piece {
            text: tail,
            width: tail_width,
            ..piece.clone()
        })
    }

    /// Breaks a word wider than the line by characters.
    fn break_long_word(&mut self, piece: Piece, lines: &mut Vec<Line>) {
        let mut rest: Vec<char> = piece.text.chars().collect();
        while !rest.is_empty() {
            let room = self.available(lines) - self.pen + 0.01;
            let mut count = rest.len();
            while count > 1
                && shaper::measure(
                    rest[..count].iter().collect::<String>().trim_end(),
                    &piece.font,
                ) > room
            {
                count -= 1;
            }
            let part: String = rest.drain(..count).collect();
            let width = shaper::measure(&part, &piece.font);
            self.pieces.push((
                Piece {
                    text: part,
                    width,
                    ..piece.clone()
                },
                self.pen,
            ));
            self.pen += width;
            if !rest.is_empty() {
                self.flush(lines);
            }
        }
    }

    /// Width of the text after a decimal tab up to its first decimal point (or all of it).
    fn decimal_offset(&self, from: usize) -> f32 {
        let mut width = 0.0;
        for (piece, _) in &self.pieces[from..] {
            if let Some(dot) = piece.text.find('.') {
                return width + shaper::measure(&piece.text[..dot], &piece.font);
            }
            width += piece.width;
        }
        width
    }

    /// Places the text after a tab against its stop once its width is known, with the leader.
    fn finish_tab(&mut self) {
        let Some((align, stop, first_piece, tab_start, leader)) = self.tab.take() else {
            return;
        };
        let width = self.pen - tab_start;
        let start = match align {
            1 => stop - width / 2.0,
            2 => stop - width,
            3 => stop - self.decimal_offset(first_piece),
            _ => stop,
        };
        let shift = (start - tab_start).max(0.0);
        for (_, offset) in &mut self.pieces[first_piece..] {
            *offset += shift;
        }
        self.pen += shift;
        let Some(leader) =
            leader.filter(|leader| !leader.is_empty() && leader != " " && shift > 0.0)
        else {
            return;
        };
        let leader_style = self.pieces.get(first_piece).map_or_else(
            || Rc::new(self.style().clone()),
            |(piece, _)| Rc::clone(&piece.style),
        );
        let leader_font = Rc::new(leader_style.font());
        let leader_width = shaper::measure(&leader, &leader_font);
        let count = if leader_width > 0.0 {
            (shift / leader_width) as usize
        } else {
            0
        };
        if count > 0 {
            let width = leader_width * count as f32;
            let piece = Piece {
                text: leader.repeat(count),
                style: leader_style,
                font: leader_font,
                width,
            };
            self.pieces
                .insert(first_piece, (piece, tab_start + shift - width));
        }
    }

    fn tab(&mut self) {
        self.finish_tab();
        let line_indent = self.indent();
        let position = line_indent + self.pen;
        let style = self.style();
        let mut stop = style
            .tabs
            .iter()
            .find(|tab| tab.position > position + 0.01)
            .map(|tab| (tab.position, tab.align, tab.leader.clone()));
        // A hanging indent is a tab stop: on the first line, short of the left indent and before any
        // explicit stop, a tab goes to the left indent.
        let (left_indent, first_indent) = (style.left_indent, style.first_indent);
        if self.first
            && left_indent > first_indent
            && position < left_indent - 0.01
            && stop.as_ref().is_none_or(|stop| stop.0 > left_indent)
        {
            stop = Some((left_indent, 0, None));
        }
        let default_interval = style.default_tab;
        let (stop_position, align, leader) = stop.unwrap_or_else(|| {
            (
                ((position / default_interval + 0.001).floor() + 1.0) * default_interval,
                0,
                None,
            )
        });
        self.tab = Some((
            align,
            stop_position - line_indent,
            self.pieces.len(),
            self.pen,
            leader,
        ));
        if align == 0 {
            self.finish_tab();
        }
    }

    fn flush(&mut self, lines: &mut Vec<Line>) {
        self.finish_tab();
        let styles: Vec<TextStyle> = if self.pieces.is_empty() {
            vec![self
                .paragraph
                .empty_style
                .clone()
                .unwrap_or_else(|| self.style().clone())]
        } else {
            self.pieces
                .iter()
                .map(|(piece, _)| (*piece.style).clone())
                .collect()
        };
        let largest = |measure: fn(&TextStyle) -> f32, styles: &[TextStyle]| {
            styles.iter().map(measure).fold(f32::MIN, f32::max)
        };
        let mut height = largest(text::line_height, &styles);
        let mut ascent = largest(text::ascent, &styles);
        let leading = largest(text::trailing_leading, &styles);
        let single_spaced = styles
            .iter()
            .all(|style| style.line_spacing_mode == 0 && style.line_spacing == 1.0);
        let mut faces: Vec<String> = styles.iter().map(text::face).collect();
        faces.sort();
        faces.dedup();
        // A line mixing faces: the largest ascent over the largest descent and leading.
        if faces.len() > 1 && single_spaced {
            height = height.max(ascent + text::mixed_below(styles.iter()));
        }
        // Relative line spacing (below or above 1) sets only the pitch between lines: the text
        // block's first line keeps its natural height (measured: 0.9-spaced Keynote titles; in
        // Pages, where spacing above 1 lies below the baseline, the first line keeps it).
        let spacing_below = text::spacing_below_active()
            && styles
                .iter()
                .all(|style| style.line_spacing_mode == 0 && style.line_spacing > 1.0);
        let pitch_only = styles.iter().all(|style| {
            (style.line_spacing_mode == 0 && style.line_spacing != 1.0)
                || matches!(style.line_spacing_mode, 1 | 4)
        });
        if lines.is_empty() && !spacing_below && pitch_only {
            let natural: Vec<TextStyle> = styles
                .iter()
                .map(|style| TextStyle {
                    line_spacing: 1.0,
                    line_spacing_mode: 0,
                    ..style.clone()
                })
                .collect();
            height = largest(text::line_height, &natural);
            ascent = largest(text::ascent, &natural);
        }
        // Baseline-shifted text grows the line unless the spacing is exact (measured: a +3pt shift
        // moves the line and the next one down by 3pt, -1pt adds 1pt below).
        if styles.iter().all(|style| style.line_spacing_mode != 2) {
            let up = styles
                .iter()
                .map(|style| style.baseline)
                .fold(f32::MIN, f32::max)
                .max(0.0);
            let down = (-styles
                .iter()
                .map(|style| style.baseline)
                .fold(f32::MAX, f32::min))
            .max(0.0);
            ascent += up;
            height += up + down;
        }
        let width = self
            .pieces
            .last()
            .map_or(0.0, |(piece, offset)| offset + trimmed_width(piece));
        let (inset_left, inset_right) = self.inset(lines);
        lines.push(Line {
            pieces: std::mem::take(&mut self.pieces),
            width,
            height,
            ascent,
            leading,
            paragraph: self.index,
            first: self.first,
            last: false,
            blocks: None,
            anchors: None,
            shift_x: inset_left,
            narrowed: inset_left + inset_right,
        });
        self.pen = 0.0;
        self.first = false;
    }
}

/// Records the insets of a line that sits beside objects (the widest seen per line).
fn record_insets(found: Option<&mut WrapInsets>, key: (usize, usize), beside: Option<(f32, f32)>) {
    if let (Some(found), Some((left, right))) = (found, beside) {
        let entry = found.entry(key).or_insert((0.0, 0.0));
        *entry = (entry.0.max(left), entry.1.max(right));
    }
}

/// A contour-wrapped anchored picture: its obstacle rectangle, the picture, its drawn frame and
/// the wrap margin.
struct Contour {
    obstacle: Rect,
    item: Rc<Item>,
    frame: Rect,
    margin: f32,
}

/// Space between consecutive footnotes (measured: Pages notes 23pt apart, lines inside a note
/// 13pt).
const NOTE_GAP: f32 = 10.0;

/// The state of one flow of text through pages.
struct Flow<'a> {
    text: &'a RichText,
    sink: &'a mut dyn PageSink,
    area: Rect,
    natural: HAlign,
    obstacles: Option<&'a dyn Fn(i32) -> Rc<Vec<Rect>>>,
    pages: i32,
    cursor_y: f32,
    paragraph_top: f32,
    anchor_obstacles: Vec<Rect>,
    blocking: Vec<Rect>,
    contours: Vec<Contour>,
    /// Footnotes of the current page: laid-out text and its height.
    page_notes: Vec<(RichText, f32)>,
}

/// A line's place on its page: where its anchors hang from and whether they moved to the page top.
#[derive(Debug, Clone, Copy)]
struct AnchorPlacement {
    line_top: f32,
    pin_top: bool,
}

impl<'a> Flow<'a> {
    fn new(
        text: &'a RichText,
        sink: &'a mut dyn PageSink,
        (area, natural): (Rect, HAlign),
        obstacles: Option<&'a dyn Fn(i32) -> Rc<Vec<Rect>>>,
    ) -> Flow<'a> {
        Flow {
            text,
            sink,
            area,
            natural,
            obstacles,
            pages: 0,
            cursor_y: area.top,
            paragraph_top: area.top,
            anchor_obstacles: Vec::new(),
            blocking: Vec::new(),
            contours: Vec::new(),
            page_notes: Vec::new(),
        }
    }

    fn new_page(&mut self, section: u64) {
        self.text.page_section.set(section);
        self.sink.new_page(section);
        self.pages += 1;
        self.cursor_y = self.area.top;
    }

    /// Footnotes take a zone 15pt plus their text tall at the page bottom, with `NOTE_GAP` between
    /// consecutive notes.
    fn notes_height(notes: &[(RichText, f32)]) -> f32 {
        if notes.is_empty() {
            0.0
        } else {
            15.0 + notes.iter().map(|note| note.1 + NOTE_GAP).sum::<f32>() - NOTE_GAP
        }
    }

    /// Draws the page's footnotes under a 100pt x 0.5pt grey rule 8.1pt below the zone's top
    /// (measured).
    fn flush_notes(&mut self) {
        if self.page_notes.is_empty() {
            return;
        }
        let area = self.area;
        let top = area.bottom - Self::notes_height(&self.page_notes);
        let notes = std::mem::take(&mut self.page_notes);
        let canvas = self.sink.canvas();
        let mut rule = plain_paint(Color::from_rgb(96, 96, 96));
        rule.set_anti_alias(true);
        canvas.draw_rect(
            Rect::new(area.left, top + 8.1, area.left + 100.0, top + 8.6),
            &rule,
        );
        let mut note_top = top + 15.0;
        for (note, height) in notes {
            note.draw(
                canvas,
                Rect::new(area.left, note_top, area.right, note_top + height + 1.0),
                0,
                true,
                HAlign::Left,
            );
            note_top += height + NOTE_GAP;
        }
    }

    /// A footnote laid out at the column width.
    fn footnote(&self, (storage, number): (u64, i32)) -> (RichText, f32) {
        ctx::FOOTNOTE_MARK.set(number);
        let note = match &self.text.styles {
            Some(styles) => RichText::from_storage(styles, storage, &TextStyle::default(), 0),
            None => RichText::default(),
        };
        let height = note.measure(self.area.width(), true);
        (note, height)
    }

    fn anchor_top(&self, vertical_offset: f32, placement: AnchorPlacement) -> f32 {
        if placement.pin_top && !is_page_relative(vertical_offset) {
            self.area.top
        } else {
            anchor_top(vertical_offset, placement.line_top)
        }
    }

    /// Objects anchored to a line wrap the text too (measured: an anchored chart pushes its own
    /// paragraph's text below it), with a 12pt margin.
    fn add_anchor_obstacles(&mut self, line: &Line, placement: AnchorPlacement) {
        for (item, horizontal, vertical, above_below) in line.anchors.iter().flatten() {
            let flow_bounds = item.flow_bounds();
            let frame = Rect::from_xywh(
                self.area.left + horizontal,
                self.anchor_top(*vertical, placement),
                flow_bounds.width(),
                flow_bounds.height(),
            );
            let obstacle = frame.with_outset((12.0, 12.0));
            self.anchor_obstacles.push(obstacle);
            if *above_below {
                self.blocking.push(obstacle);
            }
            if matches!(&**item, Item::Image(image) if image.contour_wrap()) {
                self.contours.push(Contour {
                    obstacle,
                    item: Rc::clone(item),
                    frame,
                    margin: 12.0,
                });
            }
        }
    }

    /// A line that cannot fit beside a floating object moves below it (measured: a table under a
    /// centred text box starts below the box plus its margin). Returns the insets of a line that
    /// sits beside objects.
    fn clear(&mut self, line: &Line) -> Option<(f32, f32)> {
        // Empty lines are not pushed (measured: an empty body under full-width wrapped boxes stays
        // one page).
        let has_obstacles = self.obstacles.is_some() || !self.anchor_obstacles.is_empty();
        if !has_obstacles || (line.pieces.is_empty() && line.blocks.is_none()) {
            return None;
        }
        let area = self.area;
        let needed = match &line.blocks {
            Some(blocks) if !blocks.is_empty() => blocks
                .iter()
                .map(|item| item.flow_bounds().width())
                .fold(f32::MIN, f32::max),
            _ => line.width,
        };
        let narrowest = needed.min(0.3 * area.width());
        let page_obstacles = self
            .obstacles
            .map(|obstacles| obstacles(self.pages - 1))
            .unwrap_or_default();
        loop {
            let mut beside: Vec<Rect> = Vec::new();
            let (mut inset_left, mut inset_right) = (0.0f32, 0.0f32);
            let mut moved = false;
            for original in page_obstacles.iter().chain(self.anchor_obstacles.iter()) {
                let Some(obstacle) = self.obstacle_for_line(*original, line) else {
                    continue;
                };
                let overlaps = obstacle.top < self.cursor_y + line.height
                    && obstacle.bottom > self.cursor_y
                    && obstacle.left < area.right
                    && obstacle.right > area.left;
                if !overlaps {
                    continue;
                }
                let (free_left, free_right) =
                    (obstacle.left - area.left, area.right - obstacle.right);
                if self.blocking.contains(original)
                    // Read live: page obstacles register themselves while the flow runs.
                    || ctx::ABOVE_BELOW.with(|rects| rects.borrow().contains(original))
                    || free_left.max(free_right) < narrowest
                {
                    self.cursor_y = obstacle.bottom;
                    moved = true;
                    break;
                }
                // Room beside it: the line keeps the wider side.
                beside.push(obstacle);
                if free_left >= free_right {
                    inset_right = inset_right.max(area.right - obstacle.left);
                } else {
                    inset_left = inset_left.max(obstacle.right - area.left);
                }
            }
            if moved {
                continue;
            }
            // Objects side by side can leave too little between them together (measured: two
            // pictures across the page put the next paragraph below both).
            if beside.len() > 1 && area.width() - inset_left - inset_right < narrowest {
                self.cursor_y = beside
                    .iter()
                    .map(|rect| rect.bottom)
                    .fold(f32::MAX, f32::min);
                continue;
            }
            return (!beside.is_empty()).then_some((inset_left, inset_right));
        }
    }

    /// The obstacle a line meets: a contour-wrapped picture's opaque pixels across the line's band
    /// (widened by the margin), or `None` when that band is clear.
    fn obstacle_for_line(&self, obstacle: Rect, line: &Line) -> Option<Rect> {
        let Some(contour) = self
            .contours
            .iter()
            .find(|contour| contour.obstacle == obstacle)
        else {
            return Some(obstacle);
        };
        let Item::Image(image) = &*contour.item else {
            return Some(obstacle);
        };
        let band = (
            self.cursor_y - contour.margin - contour.frame.top,
            self.cursor_y + line.height + contour.margin - contour.frame.top,
        );
        let (left, right) =
            image.opaque_span(band, (contour.frame.width(), contour.frame.height()))?;
        Some(Rect::new(
            contour.frame.left + left - contour.margin,
            obstacle.top,
            contour.frame.left + right + contour.margin,
            obstacle.bottom,
        ))
    }

    /// Height that must fit with this line: keep with next (the paragraph's lines, those of the
    /// kept paragraphs after it, then the next paragraph's first line, two with widow control) or
    /// widow control (no lone first or last line at a page edge, three-line paragraphs whole).
    fn keep_height(&self, lines: &[Line], index: usize) -> f32 {
        let line = &lines[index];
        let paragraph = &self.text.paragraphs[line.paragraph];
        if line.first && paragraph.keep_with_next {
            // Paragraphs kept with their next one chain (measured: a heading over a kept
            // sub-heading moves to the next page with it and its text's first lines).
            let mut height = 0.0;
            let mut current = index;
            loop {
                let kept = &self.text.paragraphs[lines[current].paragraph];
                let last = (current..lines.len())
                    .find(|&other| lines[other].last)
                    .unwrap_or(lines.len() - 1);
                height += lines[current..=last]
                    .iter()
                    .map(|other| other.height)
                    .sum::<f32>()
                    + kept.style.space_after;
                let Some(next) = lines.get(last + 1).filter(|next| next.blocks.is_none()) else {
                    return 0.0;
                };
                let next_paragraph = &self.text.paragraphs[next.paragraph];
                height += next_paragraph.style.space_before;
                if next_paragraph.keep_with_next {
                    current = last + 1;
                    continue;
                }
                height += next.height - next.leading;
                if next_paragraph.widow_control && !next.last {
                    height += lines.get(last + 2).map_or(0.0, |line| line.height);
                }
                return height;
            }
        }
        if !paragraph.widow_control || line.blocks.is_some() {
            return 0.0;
        }
        let Some(next) = lines
            .get(index + 1)
            .filter(|next| next.paragraph == line.paragraph)
        else {
            return 0.0;
        };
        // Orphan: the first line alone at the bottom; widow: the last line alone on top. A
        // paragraph of three lines cannot leave two on each side, so it moves whole (measured:
        // "acos(x)", its description and an empty line start the next page together).
        if line.first && !line.last {
            let count = lines[index..]
                .iter()
                .position(|other| other.last)
                .map_or(lines.len() - index, |last| last + 1);
            let kept = &lines[index..index + if count == 3 { 3 } else { 2 }];
            return kept.iter().map(|other| other.height).sum::<f32>()
                - kept.last().map_or(0.0, |other| other.leading);
        }
        let widow = next.last && !(index > 0 && lines[index - 1].first);
        if widow {
            line.height + next.height - next.leading
        } else {
            0.0
        }
    }

    fn run(mut self, mut found: Option<&mut WrapInsets>) -> i32 {
        let lines = self.text.layout(self.area.width(), true);
        let first_section = lines
            .first()
            .map_or(0, |line| self.text.paragraphs[line.paragraph].section);
        self.new_page(first_section);
        let mut line_in_paragraph = 0usize;
        for (index, line) in lines.iter().enumerate() {
            let paragraph = &self.text.paragraphs[line.paragraph];
            let style = &paragraph.style;
            line_in_paragraph = if line.first { 0 } else { line_in_paragraph + 1 };
            if line.first && self.cursor_y > self.area.top {
                self.cursor_y += style.space_before;
            }
            let line_notes: Vec<(RichText, f32)> = if line.first {
                paragraph
                    .footnotes
                    .iter()
                    .map(|note| self.footnote(*note))
                    .collect()
            } else {
                Vec::new()
            };
            // An anchored object that does not fit below its anchor on this page moves to the next
            // one with its line; there it keeps its offset when that fits, else it moves to the top
            // and the anchor's text continues below it (measured in Pages: a 468pt screenshot
            // 166pt under its paragraph starts the next page; a 297pt diagram 338pt under its
            // caption keeps that offset).
            let move_anchor = self.cursor_y > self.area.top && self.anchors_overflow(line);
            let mut placement = AnchorPlacement {
                line_top: self.cursor_y,
                pin_top: false,
            };
            if !move_anchor {
                self.add_anchor_obstacles(line, placement);
            }
            let key = (line.paragraph, line_in_paragraph);
            record_insets(found.as_deref_mut(), key, self.clear(line));
            let notes: Vec<(RichText, f32)> = self
                .page_notes
                .iter()
                .cloned()
                .chain(line_notes.iter().cloned())
                .collect();
            let bottom = self.area.bottom - Self::notes_height(&notes);
            let single_table =
                matches!(line.blocks.as_deref(), Some([item]) if matches!(**item, Item::Table(_)));
            // A table taller than the space left starts here and continues on the next pages, broken
            // at row boundaries (measured: Pages does not move it whole or repeat headers).
            let split_table = single_table
                && self.cursor_y + line.height > bottom + 0.01
                && !(line.first && paragraph.page_break_before);
            let keep = self.keep_height(&lines, index);
            let overflows = self.cursor_y + line.height - line.leading > bottom + 0.01;
            let kept_together =
                keep > 0.0 && keep <= self.area.height() && self.cursor_y + keep > bottom + 0.01;
            if !split_table
                && (move_anchor
                    || overflows
                    || (line.first && paragraph.page_break_before)
                    || kept_together)
                && self.cursor_y > self.area.top
            {
                self.flush_notes();
                self.new_page(paragraph.section);
                self.anchor_obstacles.clear();
                self.contours.clear();
                placement = AnchorPlacement {
                    line_top: self.cursor_y,
                    pin_top: move_anchor && self.anchors_overflow(line),
                };
                self.add_anchor_obstacles(line, placement);
                record_insets(found.as_deref_mut(), key, self.clear(line));
            }
            self.page_notes.extend(line_notes);
            self.draw_anchors(line, placement);
            if split_table {
                self.draw_split_table(line);
                continue;
            }
            self.draw_line(line);
        }
        self.flush_notes();
        self.pages
    }

    /// Whether one of the line's anchored objects, no taller than the page, reaches past the page
    /// bottom when it hangs from the line at the cursor.
    fn anchors_overflow(&self, line: &Line) -> bool {
        line.anchors.iter().flatten().any(|(item, _, vertical, _)| {
            let height = item.flow_bounds().height();
            !is_page_relative(*vertical)
                && anchor_top(*vertical, self.cursor_y) + height > self.area.bottom + 0.01
                && height <= self.area.height()
        })
    }

    fn draw_anchors(&mut self, line: &Line, placement: AnchorPlacement) {
        for (item, horizontal, vertical, _) in line.anchors.iter().flatten() {
            let bounds = item.bounds();
            let offset = (
                self.area.left + horizontal - bounds.left,
                self.anchor_top(*vertical, placement) - bounds.top,
            );
            let canvas = self.sink.canvas();
            canvas.save();
            canvas.translate(offset);
            item.draw(canvas);
            canvas.restore();
        }
    }

    /// A table broken at row boundaries across pages.
    fn draw_split_table(&mut self, line: &Line) {
        let Some(Item::Table(table)) = line
            .blocks
            .as_ref()
            .and_then(|blocks| blocks.first())
            .map(|item| &**item)
        else {
            return;
        };
        let paragraph = &self.text.paragraphs[line.paragraph];
        let style = &paragraph.style;
        let bounds = table.bounds();
        let breaks: Vec<f32> = table
            .break_points()
            .into_iter()
            .filter(|&point| point > 0.01)
            .collect();
        let (mut done, total) = (0.0f32, bounds.height());
        let indent = if line.first {
            style.first_indent
        } else {
            style.left_indent
        };
        let left = self.area.left + indent - bounds.left;
        loop {
            let room = self.area.bottom - self.cursor_y;
            let fitting = breaks
                .iter()
                .copied()
                .filter(|&point| point > done + 0.01 && point - done <= room + 0.01)
                .reduce(f32::max);
            let upto = if total - done <= room + 0.01 {
                total
            } else {
                fitting.unwrap_or(done + room)
            };
            let area = self.area;
            let cursor_y = self.cursor_y;
            let canvas = self.sink.canvas();
            canvas.save();
            canvas.clip_rect(
                Rect::new(
                    area.left - 1000.0,
                    cursor_y,
                    area.right + 1000.0,
                    cursor_y + (upto - done),
                ),
                None,
                None,
            );
            canvas.translate((left, cursor_y - bounds.top - done));
            table.draw(canvas);
            canvas.restore();
            self.cursor_y += upto - done;
            done = upto;
            if done >= total - 0.01 {
                break;
            }
            self.flush_notes();
            self.new_page(paragraph.section);
        }
        if line.last {
            self.cursor_y += style.space_after;
        }
    }

    fn draw_line(&mut self, line: &Line) {
        let paragraph = &self.text.paragraphs[line.paragraph];
        let style = &paragraph.style;
        let area = self.area;
        let indent = if line.first {
            style.first_indent
        } else {
            style.left_indent
        };
        let left = self
            .text
            .line_start(line, (area.left, area.width()), self.natural);
        let cursor_y = self.cursor_y;
        let canvas = self.sink.canvas();
        let mut block_top = cursor_y;
        for item in line.blocks.iter().flatten() {
            let bounds = item.flow_bounds();
            canvas.save();
            canvas.translate((area.left + indent - bounds.left, block_top - bounds.top));
            item.draw(canvas);
            canvas.restore();
            block_top += bounds.height();
        }
        let baseline = cursor_y + line.ascent;
        self.text
            .draw_bullet(canvas, line, area.left + line.shift_x, baseline);
        self.text.draw_backgrounds(
            canvas,
            line,
            Point::new(left, cursor_y),
            (area.left, area.right),
        );
        let extra = self.text.justify_extra(line, area.width(), self.natural);
        let mut shift = 0.0;
        for (piece, offset) in self.text.visual(line) {
            text::draw_line(
                canvas,
                piece.text.trim_end(),
                Point::new(left + offset + shift, baseline),
                &piece.font,
                &piece.style,
            );
            shift += extra * piece.text.matches(' ').count() as f32;
        }
        if line.first {
            self.paragraph_top = cursor_y;
        }
        self.cursor_y += line.height;
        if line.last {
            if let Some(border) = &style.border {
                // Paragraph borders sit 7.5pt outside the text column and span its line boxes
                // (measured); leading and trailing swap sides in right-to-left paragraphs. A rule (top
                // or bottom border without sides) sits 3.5pt outside the text and the paragraph takes
                // that space below it; boxes with sides do not.
                let mut paint = border.stroke.paint(false);
                paint.set_stroke_cap(PaintCap::Round);
                let outside = if border.sides & 12 == 0 { 3.5 } else { 0.0 };
                let (left_x, right_x) = (area.left - 7.5, area.right + 7.5);
                let (top, bottom) = (
                    self.paragraph_top - outside,
                    self.cursor_y - line.leading + outside,
                );
                let (mut leading, mut trailing) = (border.sides & 4 != 0, border.sides & 8 != 0);
                if border.right_to_left || paragraph.rtl() {
                    std::mem::swap(&mut leading, &mut trailing);
                }
                let canvas = self.sink.canvas();
                let sides = [
                    (leading, (left_x, top), (left_x, bottom)),
                    (trailing, (right_x, top), (right_x, bottom)),
                    (border.sides & 1 != 0, (left_x, top), (right_x, top)),
                    (border.sides & 2 != 0, (left_x, bottom), (right_x, bottom)),
                ];
                for (_, from, to) in sides.into_iter().filter(|side| side.0) {
                    canvas.draw_line(from, to, &paint);
                }
                self.cursor_y += outside;
            }
            self.cursor_y += style.space_after;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_labels() {
        assert_eq!(number_label(3, 0), "3.");
        assert_eq!(number_label(4, 3), "IV.");
        assert_eq!(number_label(9, 7), "(ix)");
        assert_eq!(number_label(28, 11), "AB)");
        assert_eq!(number_core(4, 2), "iv");
        assert_eq!(number_core(12, 0), "12");
    }

    #[test]
    fn style_tables() {
        let table: Table = vec![(0, 5), (10, 0), (20, 7)];
        assert_eq!(at(&table, 12), 0);
        assert_eq!(at_keep(&table, 12), 5);
        assert_eq!(at(&table, 25), 7);
        assert_eq!(next_boundary(&table, 0, 15), 10);
        assert_eq!(next_boundary(&table, 10, 15), 15);
    }

    #[test]
    fn page_relative_anchors() {
        assert_eq!(anchor_top(30.0, 100.0), 130.0);
        assert_eq!(anchor_top(84.0 - PAGE_RELATIVE, 100.0), 84.0);
    }

    #[test]
    fn plain_paragraph_layout() {
        let style = TextStyle::default();
        let run = Run {
            text: "one two three".into(),
            style: Rc::new(style.clone()),
        };
        let text = RichText {
            paragraphs: vec![Paragraph::new(vec![run], style)],
            ..RichText::default()
        };
        // One 10pt Helvetica Neue line: 12pt minus the leading (11.72 in Numbers).
        let one_line = text.measure(1000.0, true);
        assert!((one_line - 11.72).abs() < 0.1);
        assert!(text.measure(20.0, true) > one_line);
        assert!(!is_zero(&text.text_bounds(
            Rect::from_wh(1000.0, 100.0),
            0,
            true,
            HAlign::Left
        )));
    }

    /// A paragraph of one plain run.
    fn plain(text: &str) -> Paragraph {
        let style = TextStyle::default();
        let run = Run {
            text: text.into(),
            style: Rc::new(style.clone()),
        };
        Paragraph::new(vec![run], style)
    }

    #[test]
    fn hyphenation_points_stay_inside_letters() {
        let points = hyphenation_points("(classifier),");
        assert!(!points.is_empty());
        assert!(points
            .iter()
            .all(|&point| point > 1 && point < "(classifier".len()));
        assert!(hyphenation_points("2015103042").is_empty());
        assert!(hyphenation_points("scikit-learn").is_empty());
    }

    #[test]
    fn overflowing_words_hyphenate_only_when_enabled() {
        let mut paragraph = plain("aaaa hyphenation");
        let font = paragraph.runs[0].style.font();
        let width = shaper::measure("aaaa hyphen-", &font) + 1.0;
        let first_line_end = |paragraph: &Paragraph| {
            let text = RichText {
                paragraphs: vec![paragraph.clone()],
                ..RichText::default()
            };
            let lines = text.layout(width, true);
            lines[0].pieces.last().map(|(piece, _)| piece.text.clone())
        };
        assert_eq!(first_line_end(&paragraph).as_deref(), Some("aaaa "));
        paragraph.hyphenate = true;
        assert!(first_line_end(&paragraph).is_some_and(|end| end.ends_with('-')));
    }

    #[test]
    fn kept_paragraphs_chain_and_short_paragraphs_stay_whole() {
        let mut heading = plain("Heading");
        heading.keep_with_next = true;
        let mut subheading = plain("Subheading");
        subheading.keep_with_next = true;
        let mut three = plain("one\ntwo\nthree");
        three.widow_control = true;
        let mut four = plain("one\ntwo\nthree\nfour");
        four.widow_control = true;
        let text = RichText {
            paragraphs: vec![heading, subheading, plain("Body"), three, four],
            ..RichText::default()
        };
        let lines = text.layout(500.0, true);
        let mut sink = Scratch::default();
        let flow = Flow::new(
            &text,
            &mut sink,
            (Rect::from_wh(500.0, 700.0), HAlign::Left),
            None,
        );
        let span = |from: usize, to: usize| {
            lines[from..to].iter().map(|line| line.height).sum::<f32>() - lines[to - 1].leading
        };
        // Heading, sub-heading and the body's line move together.
        assert!((flow.keep_height(&lines, 0) - span(0, 3)).abs() < 1e-3);
        // Three lines cannot split two and two; four can.
        assert!((flow.keep_height(&lines, 3) - span(3, 6)).abs() < 1e-3);
        assert!((flow.keep_height(&lines, 6) - span(6, 8)).abs() < 1e-3);
    }

    #[test]
    fn footnotes_are_spaced_apart() {
        let notes = [(RichText::default(), 13.0), (RichText::default(), 26.0)];
        assert_eq!(Flow::notes_height(&notes), 15.0 + 13.0 + NOTE_GAP + 26.0);
        assert_eq!(Flow::notes_height(&[]), 0.0);
    }

    #[test]
    fn words_keep_spaces_and_break_on_tabs() {
        let style = TextStyle::default();
        let run = Run {
            text: "ab cd\tef\ngh".into(),
            style: Rc::new(style.clone()),
        };
        let paragraph = Paragraph::new(vec![run], style);
        let texts: Vec<Option<Vec<String>>> = split_words(&paragraph)
            .into_iter()
            .map(|word| word.map(|pieces| pieces.into_iter().map(|piece| piece.text).collect()))
            .collect();
        let word = |parts: &[&str]| {
            Some(
                parts
                    .iter()
                    .map(|part| part.to_string())
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            texts,
            vec![
                word(&["ab "]),
                word(&["cd"]),
                word(&["\t"]),
                word(&["ef", ""]),
                None,
                word(&["gh"])
            ]
        );
    }
}
