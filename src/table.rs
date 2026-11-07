//! Draws a Numbers table the way Numbers' PDF export lays it out:
//! - the drawable frame is the geometry rect; the grid starts 2pt inside it, below a title band
//!   when the table name is shown;
//! - columns are exactly their widths apart (strokes straddle the edge);
//! - rows are their fitted content height apart plus the width of each horizontal rule, so a rule
//!   never overlaps cell content vertically;
//! - cell content is inset by half the adjacent rule widths, then by the cell padding.

use crate::cell_format;
use crate::drawables::{geometry, paint_fill};
use crate::formula;
use crate::iwa::Proto;
use crate::numbers_table::{Cell, CellKind, CellPosition, CellRange, NumbersTable};
use crate::richtext::RichText;
use crate::shaper;
use crate::styles::{self, ctx, plain_paint, Stroke, Styles};
use crate::text::{self, HAlign, TextStyle};
use skia_safe::{Canvas, Color, PaintCap, PaintJoin, PaintStyle, Path, Point, Rect};
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// The drawn frame's outset from the grid.
const OUTSET: f32 = 2.0;
/// A cell style that does not inherit from the table's region style falls back to Numbers'
/// built-in defaults, which differ from the stylesheet's (measured: 2pt padding, middle).
const DEFAULT_PADDING: f32 = 2.0;
const DEFAULT_VERTICAL_ALIGN: i32 = 1;

// TableStylePropertiesArchive field numbers.
const BANDED_ROWS: i32 = 1;
const BANDED_FILL: i32 = 2;
const VERTICAL_VISIBLE: i32 = 33;
const HORIZONTAL_VISIBLE: i32 = 34;
const HEADER_ROW_SEPARATOR_VISIBLE: i32 = 35;
const HEADER_COLUMN_SEPARATOR_VISIBLE: i32 = 36;
const FOOTER_SEPARATOR_VISIBLE: i32 = 37;
const BORDER_VISIBLE: i32 = 38;
const HEADER_ROW_SEPARATOR: i32 = 46;
const HEADER_ROW_BORDER: i32 = 47;
const HEADER_ROW_HORIZONTAL: i32 = 48;
const HEADER_ROW_VERTICAL: i32 = 49;
const HEADER_COLUMN_BORDER: i32 = 50;
const HEADER_COLUMN_SEPARATOR: i32 = 51;
const HEADER_COLUMN_HORIZONTAL: i32 = 52;
const HEADER_COLUMN_VERTICAL: i32 = 53;
const FOOTER_SEPARATOR: i32 = 54;
const FOOTER_BORDER: i32 = 55;
const FOOTER_HORIZONTAL: i32 = 56;
const FOOTER_VERTICAL: i32 = 57;
const BODY_HORIZONTAL_BORDER: i32 = 58;
const BODY_VERTICAL_BORDER: i32 = 59;
const BODY_HORIZONTAL: i32 = 60;
const BODY_VERTICAL: i32 = 61;

/// Which part of the table a cell is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    Body,
    HeaderRow,
    HeaderColumn,
    Footer,
}

/// Cell padding (left, top, right, bottom).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Padding {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

/// Per-edge stroke overrides from the stroke sidecar: (line, cell along it) → (stroke, order).
type StrokeOverrides = HashMap<(usize, usize), (Option<Stroke>, i32)>;

/// The table's title band.
#[derive(Debug, Clone)]
struct Title {
    style: TextStyle,
    band: f32,
    pad_top: f32,
    pad_left: f32,
    pad_right: f32,
    stored_height: f32,
}

/// A table placed on a canvas.
pub struct TableItem {
    table: Rc<NumbersTable>,
    styles: Styles,
    /// Frame origin (geometry position minus the outset).
    frame: Point,
    /// Grid origin (top-left rule centre).
    grid: Point,
    /// Vertical rule centres relative to the grid origin.
    column_x: Vec<f32>,
    /// Horizontal rule centres.
    line_y: Vec<f32>,
    /// Horizontal rule widths (widest along each rule).
    line_width: Vec<f32>,
    /// Content height of each row.
    row_height: Vec<f32>,
    title: Option<Title>,
    table_style: u64,
    merge_of: HashMap<CellPosition, CellRange>,
    horizontal_overrides: OnceCell<StrokeOverrides>,
    vertical_overrides: OnceCell<StrokeOverrides>,
    /// Table-style lookups are asked once per cell edge; decoding them each time dominated large
    /// tables.
    style_strokes: RefCell<HashMap<i32, Option<Stroke>>>,
    style_flags: RefCell<HashMap<(i32, bool), bool>>,
    conditional_cache: RefCell<HashMap<CellPosition, Option<(u64, u64)>>>,
}

impl TableItem {
    /// Lays a table out.
    ///
    /// # Arguments
    ///
    /// * `table` - The decoded table
    /// * `styles` - The document's styles
    ///
    /// # Returns
    ///
    /// The table with its row and column positions computed (error cells recalculated first).
    pub fn new(table: Rc<NumbersTable>, styles: Styles) -> TableItem {
        // Formula cells stored without a result (written by other tools) are recalculated, as
        // Numbers does on open.
        let mut errors: Vec<CellPosition> = table
            .cells
            .iter()
            .filter(|(_, cell)| cell.borrow().kind == CellKind::Error)
            .map(|(position, _)| *position)
            .collect();
        errors.sort_unstable();
        for position in errors {
            formula::try_resolve(&table, position);
        }
        let (left, top, _, _, _) = geometry(&table.info.message(1));
        let title = table.name_enabled.then(|| title_band(&table, &styles));
        let merge_of = merge_lookup(&table);
        let mut item = TableItem {
            frame: Point::new(left - OUTSET, top - OUTSET),
            grid: Point::new(left, top + title.as_ref().map_or(0.0, |title| title.band)),
            column_x: Vec::new(),
            line_y: Vec::new(),
            line_width: Vec::new(),
            row_height: Vec::new(),
            title,
            table_style: table.model.reference(3),
            merge_of,
            horizontal_overrides: OnceCell::new(),
            vertical_overrides: OnceCell::new(),
            style_strokes: RefCell::new(HashMap::new()),
            style_flags: RefCell::new(HashMap::new()),
            conditional_cache: RefCell::new(HashMap::new()),
            table,
            styles,
        };
        item.layout();
        item
    }

    fn layout(&mut self) {
        let table = Rc::clone(&self.table);
        let columns = &table.column_strips;
        self.column_x = std::iter::once(0.0)
            .chain((0..table.columns).scan(0.0, |offset, column| {
                *offset += if columns.hidden[column] {
                    0.0
                } else {
                    columns.sizes[column]
                };
                Some(*offset)
            }))
            .collect();
        self.line_width = (0..=table.rows)
            .map(|row| {
                (0..table.columns)
                    .map(|column| {
                        self.horizontal_stroke(row, column)
                            .map_or(0.0, |stroke| stroke.width)
                    })
                    .fold(0.0, f32::max)
            })
            .collect();
        self.row_height = self.row_heights();
        // Hidden rows take no space; the rules of the boundaries they collapse together count as the
        // widest of them (measured: a filtered group's 0.75pt bottom rule and the next group's 2pt
        // top rule leave a 2pt gap). Positions accumulate in f64: summing 100,000 rows in f32 drifts
        // by over 1,600pt.
        let mut line_y = vec![0f64; table.rows + 1];
        let hidden = &table.row_strips.hidden;
        for row in 0..table.rows {
            if hidden[row] {
                continue;
            }
            let next = (row + 1..table.rows)
                .find(|&next| !hidden[next])
                .unwrap_or(table.rows);
            for collapsed in row + 1..next {
                self.line_width[next] = self.line_width[next].max(self.line_width[collapsed]);
            }
            let position = line_y[row]
                + f64::from(self.line_width[row]) / 2.0
                + f64::from(self.row_height[row])
                + f64::from(self.line_width[next]) / 2.0;
            line_y[row + 1..=next].fill(position);
        }
        self.line_y = line_y.into_iter().map(|position| position as f32).collect();
    }

    /// The table's frame, floored to whole points as Numbers does (stroked extent plus outset).
    pub fn bounds(&self) -> Rect {
        let last = |values: &[f32]| values.last().copied().unwrap_or(0.0);
        let right_half = (0..self.table.rows)
            .map(|row| {
                self.vertical_stroke(row, self.table.columns)
                    .map_or(0.0, |stroke| stroke.width / 2.0)
            })
            .fold(0.0, f32::max);
        let raw_width = self.grid.x + last(&self.column_x) + OUTSET - self.frame.x;
        let width = (raw_width + right_half + 0.001).floor();
        let height = (self.grid.y + last(&self.line_y) + last(&self.line_width) / 2.0 + OUTSET
            - self.frame.y
            + 0.001)
            .floor();
        Rect::from_xywh(self.frame.x, self.frame.y, width, height)
    }

    /// Row boundaries measured from the top of `bounds` (where a page may break the table).
    pub fn break_points(&self) -> Vec<f32> {
        let top = self.bounds().top;
        self.line_y.iter().map(|y| self.grid.y + y - top).collect()
    }

    // ------------------------------------------------------------ row heights

    fn row_heights(&self) -> Vec<f32> {
        let table = &self.table;
        let grow = ctx::GROW_ROWS.get();
        (0..table.rows)
            .map(|row| {
                let fitted = table
                    .fitted_row_heights
                    .as_ref()
                    .and_then(|fits| fits.get(row).copied());
                // An explicit row size is the rule-to-rule pitch; fitted heights are content only.
                if table.row_strips.explicit[row] {
                    let height = table.row_strips.sizes[row]
                        - (self.line_width[row] + self.line_width[row + 1]) / 2.0;
                    // In Pages an explicit height is a minimum: Apple's cached fitted height wins
                    // when the content is taller (measured: 22.75pt rows holding bullet lists draw
                    // 51.7pt tall); without a cached fit the row grows to its content. In Numbers the
                    // explicit height stands.
                    return match (grow, fitted) {
                        (true, Some(fit)) if fit > height => fit,
                        (true, None) => height.max(self.natural_row_height(row)),
                        _ => height,
                    };
                }
                if table.categories.labels.contains_key(&row) {
                    let style = self.category_text_style(self.category_level(row));
                    return 8.0 + text::line_height(&style) - text::trailing_leading(&style);
                }
                match fitted.filter(|&fit| fit > 0.0) {
                    // In Pages rows grow beyond the stored fitted height to their content.
                    Some(fit) if grow => fit.max(self.natural_row_height(row)),
                    Some(fit) => fit,
                    None => self.natural_row_height(row),
                }
            })
            .collect()
    }

    /// Auto height of a row without a cached fit: padding plus the row's text, wrapped.
    fn natural_row_height(&self, row: usize) -> f32 {
        let table = &self.table;
        let tallest = (0..table.columns)
            .filter(|&column| {
                // A cell merged across several rows does not size any one of them (measured: an
                // 8-line merged cell over three rows leaves them at their own heights).
                self.merge(row, column)
                    .is_none_or(|merge| merge.last_row <= merge.first_row)
            })
            .map(|column| {
                let cell = table.cell((row, column)).map(|cell| cell.clone());
                let cell_style = self.base_cell_style(row, column, cell.as_ref());
                let padding = self.padding(cell_style);
                let style = self.cell_text_style(row, column, cell.as_ref());
                let mut height = text::line_height(&style) - text::trailing_leading(&style);
                let width = table.column_strips.sizes[column] - padding.left - padding.right;
                if let (Some(cell), true) = (&cell, width > 0.0) {
                    // Wrapped or multi-paragraph content grows the row (measured: Pages tables with
                    // bullet lists in cells and no stored fitted heights).
                    if cell.kind == CellKind::RichText && cell.rich_text_storage != 0 {
                        let rich =
                            RichText::from_storage(&self.styles, cell.rich_text_storage, &style, 0);
                        let bounds =
                            rich.text_bounds(Rect::from_wh(width, 1e6), 0, true, HAlign::Left);
                        if bounds != Rect::default() {
                            height = height.max(bounds.bottom);
                        }
                    } else if let Some(display) =
                        cell_format::display(table, cell).filter(|text| !text.is_empty())
                    {
                        if self.styles.bool(cell_style, 11, 3, true) {
                            let font = style.font();
                            let lines: usize = style
                                .cased(&display)
                                .split('\n')
                                .map(|paragraph| text::wrap(paragraph, &font, width, true).len())
                                .sum();
                            if lines > 1 {
                                height = height.max(
                                    lines as f32 * text::line_height(&style)
                                        - text::trailing_leading(&style),
                                );
                            }
                        }
                    }
                }
                padding.top + padding.bottom + height
            })
            .fold(0.0, f32::max);
        if tallest > 0.0 {
            tallest
        } else {
            table.row_strips.sizes[row]
        }
    }

    // ---------------------------------------------------------------- styles

    fn region_of(&self, row: usize, column: usize) -> Region {
        let table = &self.table;
        if row < table.header_rows {
            Region::HeaderRow
        } else if row + table.footer_rows >= table.rows {
            Region::Footer
        } else if column < table.header_columns {
            Region::HeaderColumn
        } else {
            Region::Body
        }
    }

    fn default_cell_style(&self, region: Region) -> u64 {
        let field = match region {
            Region::HeaderRow => 19,
            Region::HeaderColumn => 20,
            Region::Footer => 21,
            Region::Body => 18,
        };
        self.table.model.reference(field)
    }

    fn default_text_style(&self, region: Region) -> u64 {
        let field = match region {
            Region::HeaderRow => 25,
            Region::HeaderColumn => 26,
            Region::Footer => 27,
            Region::Body => 24,
        };
        self.table.model.reference(field)
    }

    fn style_flag(&self, field: i32, default: bool) -> bool {
        *self
            .style_flags
            .borrow_mut()
            .entry((field, default))
            .or_insert_with(|| self.styles.bool(self.table_style, 11, field, default))
    }

    fn style_stroke(&self, field: i32) -> Option<Stroke> {
        self.style_strokes
            .borrow_mut()
            .entry(field)
            .or_insert_with(|| {
                styles::stroke_of(
                    self.styles.message(self.table_style, 11, field).as_ref(),
                    true,
                )
            })
            .clone()
    }

    /// A cell's own style, else its column's, then its row's default (measured: a column given a
    /// left-aligned text style left-aligns the numbers in it), else the region's.
    fn base_cell_style(&self, row: usize, column: usize, cell: Option<&Cell>) -> u64 {
        let own = cell.map_or(0, |cell| self.table.style_reference(cell.cell_style_key));
        let strip = || {
            strip_style(
                &self.table.column_strips.cell_style,
                &self.table.row_strips.cell_style,
                (row, column),
            )
        };
        [own, strip()]
            .into_iter()
            .find(|&style| style != 0)
            .unwrap_or_else(|| self.default_cell_style(self.region_of(row, column)))
    }

    /// The cell style including conditional highlighting. A highlight style without a fill of its
    /// own leaves the cell's fill (measured: an empty rule style on blank rows keeps the banding).
    fn cell_style(&self, row: usize, column: usize, cell: Option<&Cell>) -> u64 {
        match self.conditional((row, column), cell) {
            Some((conditional, _))
                if conditional != 0 && self.styles.message(conditional, 11, 1).is_some() =>
            {
                conditional
            }
            _ => self.base_cell_style(row, column, cell),
        }
    }

    /// Conditional highlighting: the rule Numbers last applied (cached in the cell), else the first
    /// rule of the cell's TST.ConditionalStyleSetArchive whose predicate holds, supplies the cell
    /// and text style.
    fn conditional(&self, position: CellPosition, cell: Option<&Cell>) -> Option<(u64, u64)> {
        let cell = cell.filter(|cell| cell.conditional_style_key != 0)?;
        if let Some(cached) = self.conditional_cache.borrow().get(&position) {
            return *cached;
        }
        let sets = self.table.list(self.table.model.message(4).reference(18));
        let set = sets
            .get(&cell.conditional_style_key)
            .and_then(|entry| self.table.doc.body(entry.reference(4)));
        let result = set.and_then(|set| {
            let mut rules = set.message(3).messages(1);
            if rules.is_empty() {
                rules = set.messages(2);
            }
            let styles_of = |rule: &Proto| (rule.reference(2), rule.reference(3));
            match cell.conditional_rule_applied {
                Some(applied) => rules.get(applied as usize).map(styles_of),
                None => rules
                    .iter()
                    .find(|rule| predicate(&rule.message(1).message(7), cell) == Some(true))
                    .map(styles_of),
            }
        });
        self.conditional_cache.borrow_mut().insert(position, result);
        result
    }

    fn cell_text_style(&self, row: usize, column: usize, cell: Option<&Cell>) -> TextStyle {
        let region_style = TextStyle::resolve(
            &self.styles,
            self.default_text_style(self.region_of(row, column)),
            0,
            None,
        );
        if let Some((_, conditional_text)) = self
            .conditional((row, column), cell)
            .filter(|styles| styles.1 != 0)
        {
            return TextStyle::resolve(&self.styles, conditional_text, 0, Some(&region_style));
        }
        let own = cell.map_or(0, |cell| self.table.style_reference(cell.text_style_key));
        let style = if own != 0 {
            own
        } else {
            strip_style(
                &self.table.column_strips.text_style,
                &self.table.row_strips.text_style,
                (row, column),
            )
        };
        if style != 0 {
            TextStyle::resolve(&self.styles, style, 0, Some(&region_style))
        } else {
            region_style
        }
    }

    /// Text style of summary label rows (label_level_N_text_style), else the body text style.
    fn label_text_style(&self, level: usize) -> TextStyle {
        let id = self.table.model.reference(76 + level.min(4) as i32);
        TextStyle::resolve(
            &self.styles,
            if id != 0 {
                id
            } else {
                self.table.model.reference(24)
            },
            0,
            None,
        )
    }

    /// Category rows' bold text style (category_level_N_text_style).
    fn category_text_style(&self, level: usize) -> TextStyle {
        let model = &self.table.model;
        let id = [
            model.reference(65 + level.min(4) as i32),
            model.reference(65),
        ]
        .into_iter()
        .find(|&id| id != 0)
        .unwrap_or_else(|| model.reference(24));
        TextStyle {
            bold: true,
            ..TextStyle::resolve(&self.styles, id, 0, None)
        }
    }

    /// Cell padding; side padding below 2pt is drawn at 2pt (measured: a 1pt-padded cell's text
    /// sits 2pt inside the stroke).
    fn padding(&self, cell_style: u64) -> Padding {
        let padding = self.styles.message(cell_style, 11, 9);
        let side = |field: i32| {
            padding.as_ref().map_or(DEFAULT_PADDING, |padding| {
                padding.float_or(field, DEFAULT_PADDING)
            })
        };
        Padding {
            left: side(1).max(2.0),
            top: side(2),
            right: side(3).max(2.0),
            bottom: side(4),
        }
    }

    fn merge(&self, row: usize, column: usize) -> Option<CellRange> {
        self.merge_of.get(&(row, column)).copied()
    }

    fn category_level(&self, row: usize) -> usize {
        self.table.categories.levels.get(&row).copied().unwrap_or(0)
    }

    // ---------------------------------------------------------------- strokes

    /// The stroke sidecar's per-edge overrides (horizontal, vertical), loaded on first use.
    fn overrides(&self, horizontal: bool) -> &StrokeOverrides {
        let load = |layer_fields: [(i32, usize); 2]| {
            let mut overrides = StrokeOverrides::new();
            let Some(sidecar) = self.table.doc.body(self.table.model.reference(49)) else {
                return overrides;
            };
            for (field, delta) in layer_fields {
                for layer in sidecar
                    .references(field)
                    .into_iter()
                    .filter_map(|id| self.table.doc.body(id))
                {
                    let Ok(line) = usize::try_from(layer.int(1)) else {
                        continue;
                    };
                    for run in layer.messages(2) {
                        let (origin, length, order) = (
                            usize::try_from(run.int(1)).unwrap_or(0),
                            usize::try_from(run.int(2)).unwrap_or(0),
                            run.int(4),
                        );
                        let stroke = styles::stroke_of(Some(&run.message(3)), true);
                        for along in origin..origin + length {
                            let entry = overrides
                                .entry((line + delta, along))
                                .or_insert((None, i32::MIN));
                            if order >= entry.1 {
                                *entry = (stroke.clone(), order);
                            }
                        }
                    }
                }
            }
            overrides
        };
        if horizontal {
            // top_row_stroke_layers (6), bottom_row_stroke_layers (7).
            self.horizontal_overrides
                .get_or_init(|| load([(6, 0), (7, 1)]))
        } else {
            // left_column_stroke_layers (4), right_column_stroke_layers (5).
            self.vertical_overrides
                .get_or_init(|| load([(4, 0), (5, 1)]))
        }
    }

    /// Stroke of the horizontal rule above `row` in `column`.
    fn horizontal_stroke(&self, row: usize, column: usize) -> Option<Stroke> {
        let table = &self.table;
        let categories = &table.categories;
        if row > 0 && categories.summary_label_rows.contains(&(row - 1)) {
            return None; // the label row runs into its first group
        }
        if row > 0 && categories.labels.contains_key(&row) {
            // category_levelN_top_stroke
            let level_stroke = self.style_stroke(62 + self.category_level(row).min(4) as i32);
            return level_stroke
                .or_else(|| self.style_stroke(62))
                .or_else(|| self.style_stroke(BODY_HORIZONTAL));
        }
        // Below a category row: category_levelN_bottom_stroke.
        if row > 0 && row < table.rows && categories.labels.contains_key(&(row - 1)) {
            if let Some(below) = self.style_stroke(67 + self.category_level(row - 1).min(4) as i32)
            {
                return Some(below);
            }
        }
        if let Some((stroke, _)) = self.overrides(true).get(&(row, column)) {
            return stroke.clone();
        }
        let (header_rows, footer_rows, rows) = (table.header_rows, table.footer_rows, table.rows);
        let in_header_column = column < table.header_columns;
        if row == 0 || row == rows {
            if !self.style_flag(BORDER_VISIBLE, true) {
                return None;
            }
            let outer = if row == 0 {
                (header_rows > 0).then_some(HEADER_ROW_BORDER)
            } else {
                (footer_rows > 0).then_some(FOOTER_BORDER)
            };
            let fallback = if in_header_column {
                HEADER_COLUMN_BORDER
            } else {
                BODY_HORIZONTAL_BORDER
            };
            return self.style_stroke(outer.unwrap_or(fallback));
        }
        if header_rows > 0 && row == header_rows {
            return self
                .style_flag(HEADER_ROW_SEPARATOR_VISIBLE, true)
                .then(|| self.style_stroke(HEADER_ROW_SEPARATOR))
                .flatten();
        }
        if footer_rows > 0 && row == rows.saturating_sub(footer_rows) {
            return self
                .style_flag(FOOTER_SEPARATOR_VISIBLE, true)
                .then(|| self.style_stroke(FOOTER_SEPARATOR))
                .flatten();
        }
        if !self.style_flag(HORIZONTAL_VISIBLE, true) {
            return None;
        }
        let field = if row < header_rows {
            HEADER_ROW_HORIZONTAL
        } else if row > rows.saturating_sub(footer_rows) {
            FOOTER_HORIZONTAL
        } else if in_header_column {
            HEADER_COLUMN_HORIZONTAL
        } else {
            BODY_HORIZONTAL
        };
        self.style_stroke(field)
    }

    /// Stroke of the vertical rule left of `column` in `row`.
    fn vertical_stroke(&self, row: usize, column: usize) -> Option<Stroke> {
        let table = &self.table;
        if table.categories.labels.contains_key(&row) && column == 1 {
            return None; // the group label overflows into the first data column
        }
        if let Some((stroke, _)) = self.overrides(false).get(&(column, row)) {
            return stroke.clone();
        }
        let header_columns = table.header_columns;
        let in_header_row = row < table.header_rows;
        let in_footer = row + table.footer_rows >= table.rows;
        if column == 0 || column == table.columns {
            if !self.style_flag(BORDER_VISIBLE, true) {
                return None;
            }
            let field = if in_header_row {
                HEADER_ROW_BORDER
            } else if in_footer {
                FOOTER_BORDER
            } else if column == 0 && header_columns > 0 {
                HEADER_COLUMN_BORDER
            } else {
                BODY_VERTICAL_BORDER
            };
            return self.style_stroke(field);
        }
        // Where the header row meets the header-column separator Apple draws neither stroke
        // (measured: a black header_row_vertical_stroke is absent there; the separator starts below).
        if in_header_row && header_columns > 0 && column == header_columns && !table.label_column {
            return None;
        }
        if in_header_row {
            return self
                .style_flag(VERTICAL_VISIBLE, true)
                .then(|| self.style_stroke(HEADER_ROW_VERTICAL))
                .flatten();
        }
        if header_columns > 0 && column == header_columns {
            return self
                .style_flag(HEADER_COLUMN_SEPARATOR_VISIBLE, true)
                .then(|| self.style_stroke(HEADER_COLUMN_SEPARATOR))
                .flatten();
        }
        if !self.style_flag(VERTICAL_VISIBLE, true) {
            return None;
        }
        let field = if in_footer {
            FOOTER_VERTICAL
        } else if column < header_columns {
            HEADER_COLUMN_VERTICAL
        } else {
            BODY_VERTICAL
        };
        self.style_stroke(field)
    }

    // ------------------------------------------------------------------ draw

    /// Draws the title, fills, cell text and rules.
    pub fn draw(&self, canvas: &Canvas) {
        canvas.save();
        canvas.translate(self.grid);
        if let Some(title) = &self.title {
            self.draw_title(canvas, title);
        }
        self.draw_fills(canvas);
        self.draw_cell_text(canvas);
        self.draw_strokes(canvas);
        canvas.restore();
    }

    /// The title wraps to the table width (measured: a 642.4pt line fits a 643.5pt table, a 649.8pt
    /// one does not). With no stored title height the band holds one line and only the first
    /// wrapped line shows; a stored table_name_height makes the band that tall with the wrapped
    /// lines centred in it.
    fn draw_title(&self, canvas: &Canvas, title: &Title) {
        let style = &title.style;
        let font = style.font();
        let width = self.column_x.last().copied().unwrap_or(0.0);
        let mut lines = text::wrap(&style.cased(&self.table.name), &font, width, true);
        let band_top = self.frame.y.floor() + OUTSET;
        let (pitch, first_baseline) = text::title_line(style);
        let mut baseline = if title.stored_height > 0.0 {
            band_top + (title.band - lines.len() as f32 * pitch) / 2.0 + first_baseline
                - self.grid.y
        } else {
            lines.truncate(1);
            band_top + title.pad_top + style.space_after / 2.0 + first_baseline - self.grid.y
        };
        for line in lines {
            let line_width = shaper::measure(&line, &font);
            // Measured inset: 4pt with the usual 4pt padding, 0 with 2pt padding (2 * pad - 4).
            let line_x = match style.align {
                HAlign::Left => 2.0 * title.pad_left - 4.0,
                HAlign::Right => width - (2.0 * title.pad_right - 4.0) - line_width,
                _ => (width - line_width) / 2.0,
            };
            text::draw_line(canvas, &line, Point::new(line_x, baseline), &font, style);
            baseline += pitch;
        }
    }

    /// The rectangle a cell (or the merge it starts) covers, from rule centre to rule centre.
    fn cell_rect(&self, row: usize, column: usize, last: CellPosition) -> Rect {
        let table = &self.table;
        let right = self.column_x[(last.1 + 1).min(table.columns)];
        let bottom = self.line_y[(last.0 + 1).min(table.rows)];
        Rect::new(self.column_x[column], self.line_y[row], right, bottom)
    }

    fn draw_fills(&self, canvas: &Canvas) {
        let table = &self.table;
        let band_color = self
            .style_flag(BANDED_ROWS, false)
            .then(|| {
                styles::fill_color(
                    self.styles
                        .message(self.table_style, 11, BANDED_FILL)
                        .as_ref(),
                )
            })
            .flatten();
        for row in (0..table.rows).filter(|&row| !table.row_strips.hidden[row]) {
            for column in (0..table.columns).filter(|&column| !table.column_strips.hidden[column]) {
                let merge = self.merge(row, column);
                if merge.is_some_and(|merge| merge.first_row != row || merge.first_column != column)
                {
                    continue;
                }
                let last = merge.map_or((row, column), |merge| (merge.last_row, merge.last_column));
                let rect = self.cell_rect(row, column, last);
                let cell = table.cell((row, column)).map(|cell| cell.clone());
                if table.categories.labels.contains_key(&row) {
                    self.fill_category_row(canvas, row, rect);
                    continue;
                }
                let style = self.cell_style(row, column, cell.as_ref());
                let own = self.styles.message(style, 11, 1);
                if own.as_ref().is_some_and(|fill| fill.has(3) || fill.has(2)) {
                    // Image (cell pictures) and gradient fills.
                    let mut cell_path = Path::new();
                    cell_path.add_rect(rect, None);
                    ctx::OPAQUE_STOPS.set(true);
                    paint_fill(canvas, &table.doc, own.as_ref(), &cell_path, rect);
                    ctx::OPAQUE_STOPS.set(false);
                    continue;
                }
                // The region's background (header/footer fill, banding) is a layer under the cell's
                // own fill. An explicit empty cell_fill clears the region fill but not banding
                // (measured: a header-row cell with "cell_fill {}" exports white).
                let region = self.region_of(row, column);
                let cleared = own.as_ref().is_some_and(Proto::is_empty);
                let region_fill = || {
                    if cleared {
                        None
                    } else {
                        styles::fill_color(
                            self.styles
                                .message(self.default_cell_style(region), 11, 1)
                                .as_ref(),
                        )
                    }
                };
                let banded = region == Region::Body
                    && (row as i64 - table.header_rows as i64).rem_euclid(2) == 1;
                let fill = styles::fill_color(own.as_ref())
                    .or_else(region_fill)
                    .or(if banded { band_color } else { None });
                if let Some(color) = fill.filter(|color| color.a() > 0) {
                    canvas.draw_rect(rect, &styles::fill_paint(color));
                }
            }
        }
    }

    /// A category row's fill: label_level_N_style on summary label rows when it has a fill, else
    /// category_level_N_style.
    fn fill_category_row(&self, canvas: &Canvas, row: usize, rect: Rect) {
        let model = &self.table.model;
        let level = self.category_level(row).min(4) as i32;
        let has_fill =
            |style: u64| styles::fill_color(self.styles.message(style, 11, 1).as_ref()).is_some();
        let label_style = model.reference(71 + level);
        let mut style = model.reference(60 + level);
        if self.table.categories.summary_label_rows.contains(&row)
            && label_style != 0
            && has_fill(label_style)
        {
            style = label_style;
        }
        if style == 0 {
            style = model.reference(60);
        }
        if let Some(color) = styles::fill_color(self.styles.message(style, 11, 1).as_ref()) {
            canvas.draw_rect(rect, &styles::fill_paint(color));
        }
    }

    fn draw_strokes(&self, canvas: &Canvas) {
        let table = &self.table;
        let (rows, columns) = (table.rows, table.columns);
        // Outer ends of rules overshoot by half a point, as in Numbers' output.
        const OVERSHOOT: f32 = 0.5;
        let mut segments: Vec<(Point, Point, Stroke, i32)> = Vec::new();
        let priority =
            |stroke: &Stroke, border: bool| (stroke.width * 100.0) as i32 * 2 + i32::from(border);
        for row in 0..=rows {
            if row > 0
                && row < rows
                && table.row_strips.hidden[row]
                && table.row_strips.hidden[row - 1]
            {
                continue;
            }
            for column in (0..columns).filter(|&column| !table.column_strips.hidden[column]) {
                if row > 0
                    && row < rows
                    && self
                        .merge(row, column)
                        .is_some_and(|merge| merge.first_row < row)
                {
                    continue;
                }
                let Some(stroke) = self
                    .horizontal_stroke(row, column)
                    .filter(|stroke| stroke.visible)
                else {
                    continue;
                };
                let left = self.column_x[column] - if column == 0 { OVERSHOOT } else { 0.0 };
                let right = self.column_x[column + 1]
                    + if column == columns - 1 {
                        OVERSHOOT
                    } else {
                        0.0
                    };
                let order = priority(&stroke, row == 0 || row == rows);
                segments.push((
                    Point::new(left, self.line_y[row]),
                    Point::new(right, self.line_y[row]),
                    stroke,
                    order,
                ));
            }
        }
        for column in 0..=columns {
            if column > 0
                && column < columns
                && table.column_strips.hidden[column]
                && table.column_strips.hidden[column - 1]
            {
                continue;
            }
            for row in (0..rows).filter(|&row| !table.row_strips.hidden[row]) {
                if column > 0
                    && column < columns
                    && self
                        .merge(row, column)
                        .is_some_and(|merge| merge.first_column < column)
                {
                    continue;
                }
                let Some(stroke) = self
                    .vertical_stroke(row, column)
                    .filter(|stroke| stroke.visible)
                else {
                    continue;
                };
                let top = self.line_y[row] - if row == 0 { OVERSHOOT } else { 0.0 };
                let bottom = self.line_y[row + 1] + if row == rows - 1 { OVERSHOOT } else { 0.0 };
                let order = priority(&stroke, column == 0 || column == columns);
                segments.push((
                    Point::new(self.column_x[column], top),
                    Point::new(self.column_x[column], bottom),
                    stroke,
                    order,
                ));
            }
        }
        // Wider rules (and borders among equals) draw last, over narrower ones.
        segments.sort_by_key(|segment| segment.3);
        for (from, to, stroke, _) in segments {
            canvas.draw_line(from, to, &stroke.paint(true));
        }
    }

    // ------------------------------------------------------------------ text

    /// A cell's content area: inside half the adjacent rule widths. Content starts below the cell's
    /// own top stroke (measured: a 3pt top border pushes its cell's text 1.5pt down while a
    /// neighbour without one keeps its position).
    fn inner(&self, row: usize, column: usize) -> Rect {
        let width = |stroke: Option<Stroke>| stroke.map_or(0.0, |stroke| stroke.width);
        let (left, right) = (
            width(self.vertical_stroke(row, column)),
            width(self.vertical_stroke(row, column + 1)),
        );
        let top = width(self.horizontal_stroke(row, column));
        Rect::new(
            self.column_x[column] + left / 2.0,
            self.line_y[row] + top / 2.0,
            self.column_x[column + 1] - right / 2.0,
            self.line_y[row] + self.line_width[row] / 2.0 + self.row_height[row],
        )
    }

    fn draw_category_text(&self, canvas: &Canvas) {
        let table = &self.table;
        let categories = &table.categories;
        let visible = |row: usize| row < table.rows && !table.row_strips.hidden[row];
        for (&(row, column), value) in &categories.values {
            if !visible(row) || column >= table.columns {
                continue;
            }
            let level = self.category_level(row);
            let style = if categories.summary_label_rows.contains(&row) {
                self.label_text_style(level)
            } else {
                self.category_text_style(level)
            };
            let font = style.font();
            // Right-aligned 4pt inside the cell, on the category label's baseline (measured).
            let value_x = self.inner(row, column).right - 4.0 - shaper::measure(value, &font);
            let baseline = self.inner(row, 0).top + 4.0 + text::ascent(&style);
            text::draw_line(canvas, value, Point::new(value_x, baseline), &font, &style);
        }
        for (&row, label) in &categories.labels {
            if !visible(row) {
                continue;
            }
            let top = self.inner(row, 0).top;
            let level = self.category_level(row);
            // Each nesting level indents 8pt (measured).
            let indent = 8.0 * level as f32;
            if categories.summary_label_rows.contains(&row) {
                // Summary label row: the grouping column's name, regular, 7pt in (measured).
                let style = self.label_text_style(level);
                let origin = Point::new(
                    self.column_x[0] + 0.175 + 7.0 + indent,
                    top + 4.0 + text::ascent(&style),
                );
                text::draw_line(canvas, label, origin, &style.font(), &style);
                continue;
            }
            // Disclosure triangle (7 x 6pt, measured) and the bold group value 21pt into the row.
            let left = self.column_x[0] + indent;
            let mut triangle = Path::new();
            triangle
                .move_to((left + 7.0, top + 7.36))
                .line_to((left + 14.0, top + 7.36))
                .line_to((left + 10.5, top + 13.36))
                .close();
            canvas.draw_path(&triangle, &styles::fill_paint(Color::BLACK));
            let style = self.category_text_style(level);
            let origin = Point::new(left + 0.175 + 21.0, top + 4.0 + text::ascent(&style));
            text::draw_line(canvas, label, origin, &style.font(), &style);
        }
    }

    fn draw_cell_text(&self, canvas: &Canvas) {
        self.draw_category_text(canvas);
        let table = &self.table;
        let mut positions: Vec<CellPosition> = table.cells.keys().copied().collect();
        positions.sort_unstable();
        for (row, column) in positions {
            if row >= table.rows
                || column >= table.columns
                || table.row_strips.hidden[row]
                || table.column_strips.hidden[column]
            {
                continue;
            }
            let Some(cell) = table.cell((row, column)).map(|cell| cell.clone()) else {
                continue;
            };
            let merge = self.merge(row, column);
            if merge.is_some_and(|merge| merge.first_row != row || merge.first_column != column) {
                continue;
            }
            let format_type =
                cell_format::format_of(table, &cell).map_or(0, |format| format.int(1));
            if matches!(format_type, cell_format::CHECKBOX | cell_format::RATING)
                && matches!(cell.kind, CellKind::Bool | CellKind::Number)
            {
                draw_control(canvas, format_type, cell.number, self.inner(row, column));
                continue;
            }
            let display = cell_format::display(table, &cell);
            if display.as_deref().is_none_or(str::is_empty) && cell.kind != CellKind::RichText {
                continue;
            }
            let mut inner = self.inner(row, column);
            if let Some(merge) = merge {
                let last = self.inner(
                    merge.last_row.min(table.rows - 1),
                    merge.last_column.min(table.columns - 1),
                );
                inner = Rect::new(inner.left, inner.top, last.right, last.bottom);
            }
            canvas.save();
            canvas.clip_rect(inner, None, None);
            self.draw_cell_content(
                canvas,
                (row, column),
                &cell,
                display.unwrap_or_default(),
                inner,
            );
            canvas.restore();
        }
    }

    /// One cell's text inside its padded content area.
    fn draw_cell_content(
        &self,
        canvas: &Canvas,
        (row, column): CellPosition,
        cell: &Cell,
        display: String,
        inner: Rect,
    ) {
        let cell_style = self.base_cell_style(row, column, Some(cell));
        let style = self.cell_text_style(row, column, Some(cell));
        let padding = self.padding(cell_style);
        let vertical_align = self.styles.int(cell_style, 11, 8, DEFAULT_VERTICAL_ALIGN);
        let rect = Rect::new(
            inner.left + padding.left,
            inner.top + padding.top,
            inner.right - padding.right,
            inner.bottom - padding.bottom,
        );
        if cell.kind == CellKind::RichText && cell.rich_text_storage != 0 {
            RichText::from_storage(&self.styles, cell.rich_text_storage, &style, 0).draw(
                canvas,
                rect,
                vertical_align,
                true,
                HAlign::Left,
            );
            return;
        }
        let align = match style.align {
            HAlign::Natural if cell_format::is_text_popup(&self.table, cell) => HAlign::Left, // a text pop-up item
            HAlign::Natural if cell.kind.is_numeric() => HAlign::Right,
            HAlign::Natural if matches!(cell.kind, CellKind::Bool | CellKind::Error) => {
                HAlign::Center
            }
            HAlign::Natural => HAlign::Left,
            other => other,
        };
        let font = style.font();
        let mut line_height = text::line_height(&style);
        let mut ascent = text::ascent(&style);
        if style.line_spacing_mode != 2 && style.baseline != 0.0 {
            // A baseline shift grows each line by the shift (see the rich-text layout).
            line_height += style.baseline.abs();
            ascent += style.baseline.max(0.0);
        }
        let cased = style.cased(&display);
        let mut wrap = self.styles.bool(cell_style, 11, 3, true);
        let (mut align, mut overflow_shift) = (align, 0.0);
        if cell.kind == CellKind::Date {
            // Dates never wrap; one too wide for its cell is drawn from the left and clipped, 1pt
            // inside the padded edge (measured: "Wednesday, October 21, 2026" in a narrow column).
            wrap = false;
            if shaper::measure(&cased, &font) > rect.width() {
                (align, overflow_shift) = (HAlign::Left, 1.0);
            }
        }
        let lines: Vec<String> = cased
            .split('\n')
            .flat_map(|paragraph| text::wrap(paragraph, &font, rect.width(), wrap))
            .collect();
        // Bottom alignment measures the block without the last line's leading (measured:
        // Excel-imported bottom-aligned cells).
        let trailing = if vertical_align == 2 {
            text::trailing_leading(&style)
        } else {
            0.0
        };
        let total = lines.len() as f32 * line_height - trailing;
        let top = match vertical_align {
            1 => rect.top + (rect.height() - total) / 2.0,
            2 => rect.bottom - total,
            _ => rect.top,
        };
        let line_box = LineBox {
            rect,
            line_height,
            ascent,
            align,
            overflow_shift,
        };
        for (index, line) in lines.iter().enumerate() {
            let baseline = top + ascent + index as f32 * line_height;
            let amount = matches!(cell.kind, CellKind::Number | CellKind::Currency);
            draw_cell_line(canvas, line, baseline, &line_box, (&font, &style), amount);
        }
    }
}

/// Where a cell's lines go.
struct LineBox {
    rect: Rect,
    line_height: f32,
    ascent: f32,
    align: HAlign,
    overflow_shift: f32,
}

/// One line of a cell: accounting layout, tabbed text, or aligned text with its highlights.
fn draw_cell_line(
    canvas: &Canvas,
    line: &str,
    baseline: f32,
    line_box: &LineBox,
    (font, style): (&shaper::TaggedFont, &TextStyle),
    amount: bool,
) {
    let rect = line_box.rect;
    // Accounting style ("€\t(775.00)"): the symbol at the left edge, the amount right-aligned.
    if let (Some((symbol, figure)), true) = (line.split_once('\t'), amount) {
        if !symbol.is_empty() {
            text::draw_line(canvas, symbol, Point::new(rect.left, baseline), font, style);
            text::draw_line(
                canvas,
                figure,
                Point::new(rect.right - shaper::measure(figure, font), baseline),
                font,
                style,
            );
            return;
        }
    }
    if line.contains('\t') {
        // Text with tabs: segments go to the paragraph's tab stops (from the text's left edge), else
        // to multiples of the default interval.
        let mut pen = 0.0f32;
        for (index, segment) in line.split('\t').enumerate() {
            let segment_width = shaper::measure(segment, font);
            if index > 0 {
                let stop = style
                    .tabs
                    .iter()
                    .find(|tab| tab.position > pen + 0.01)
                    .map(|tab| (tab.position, tab.align));
                let (position, tab_align) = stop.unwrap_or((
                    ((pen / style.default_tab + 0.001).floor() + 1.0) * style.default_tab,
                    0,
                ));
                let start = match tab_align {
                    1 => position - segment_width / 2.0,
                    2 => position - segment_width,
                    _ => position,
                };
                pen = pen.max(start);
            }
            text::draw_line(
                canvas,
                segment,
                Point::new(rect.left + pen, baseline),
                font,
                style,
            );
            pen += segment_width;
        }
        return;
    }
    let width = shaper::measure(line, font);
    let line_x = match line_box.align {
        HAlign::Right => rect.right - width,
        HAlign::Center => rect.left + (rect.width() - width) / 2.0,
        _ => rect.left + line_box.overflow_shift,
    };
    // Paragraph fill across the cell and character background behind the text, each over the line
    // box (measured: cell text highlights).
    let line_top = baseline - line_box.ascent;
    if let Some(fill) = style.para_fill {
        canvas.draw_rect(
            Rect::new(
                rect.left,
                line_top,
                rect.right,
                line_top + line_box.line_height,
            ),
            &plain_paint(fill),
        );
    }
    if let Some(background) = style.background {
        let trimmed = shaper::measure(line.trim_end(), font);
        canvas.draw_rect(
            Rect::new(
                line_x,
                line_top,
                line_x + trimmed,
                line_top + line_box.line_height,
            ),
            &plain_paint(background),
        );
    }
    text::draw_line(canvas, line, Point::new(line_x, baseline), font, style);
}

/// Checkbox (16pt box centred in the cell) and star rating (five slots across the cell, a star per
/// point and a dot per empty slot; star outer radius 7.89pt, inner 0.382), as measured.
fn draw_control(canvas: &Canvas, control: i32, value: f64, inner: Rect) {
    let ink = styles::fill_paint(Color::BLACK);
    if control == cell_format::CHECKBOX {
        // Numbers' 16pt checkbox image: a light grey rounded square, and when checked a heavy black
        // tick overshooting its top-right corner (measured from the export).
        let (left, top) = (inner.center_x() - 8.0, inner.center_y() - 8.0);
        let square = Rect::new(left + 2.0, top + 2.0, left + 14.0, top + 13.8);
        canvas.draw_round_rect(
            square,
            2.5,
            2.5,
            &styles::fill_paint(Color::from_rgb(212, 212, 212)),
        );
        let mut border = styles::fill_paint(Color::from_rgb(160, 160, 160));
        border.set_style(PaintStyle::Stroke);
        border.set_stroke_width(0.6);
        canvas.draw_round_rect(square, 2.5, 2.5, &border);
        if value != 0.0 {
            let mut tick = styles::fill_paint(Color::BLACK);
            tick.set_style(PaintStyle::Stroke);
            tick.set_stroke_width(1.9);
            tick.set_stroke_cap(PaintCap::Round);
            tick.set_stroke_join(PaintJoin::Round);
            let mut path = Path::new();
            path.move_to((left + 4.2, top + 6.3))
                .line_to((left + 8.1, top + 10.5))
                .line_to((left + 15.8, top));
            canvas.draw_path(&path, &tick);
        }
        return;
    }
    let stars = value.clamp(0.0, 5.0).round() as usize;
    let slot = inner.width() / 5.0;
    let center_y = inner.center_y() + 0.986;
    for index in 0..5 {
        let center_x = inner.left + slot * (index as f32 + 0.5);
        if index >= stars {
            canvas.draw_circle((center_x, center_y), 2.465, &ink);
            continue;
        }
        let mut star = Path::new();
        for corner in 0..10 {
            let angle =
                -std::f64::consts::FRAC_PI_2 + f64::from(corner) * std::f64::consts::PI / 5.0;
            let radius = if corner % 2 == 0 {
                7.888
            } else {
                7.888 * 0.382
            };
            let point = Point::new(
                center_x + radius * angle.cos() as f32,
                center_y + radius * angle.sin() as f32,
            );
            if corner == 0 {
                star.move_to(point);
            } else {
                star.line_to(point);
            }
        }
        star.close();
        canvas.draw_path(&star, &ink);
    }
}

/// A cell with no style of its own takes its column's, then its row's default style.
fn strip_style(column_styles: &[u64], row_styles: &[u64], (row, column): CellPosition) -> u64 {
    match column_styles.get(column) {
        Some(&style) if style != 0 => style,
        _ => row_styles.get(row).copied().unwrap_or(0),
    }
}

/// Every cell covered by a merge → the merge.
fn merge_lookup(table: &NumbersTable) -> HashMap<CellPosition, CellRange> {
    let mut lookup = HashMap::new();
    for merge in &table.merges {
        for row in merge.first_row..=merge.last_row.min(table.rows.saturating_sub(1)) {
            for column in
                merge.first_column..=merge.last_column.min(table.columns.saturating_sub(1))
            {
                lookup.insert((row, column), *merge);
            }
        }
    }
    lookup
}

/// The title band: shape padding + one line + the paragraph's space-after (measured: Helvetica
/// Neue 12pt with 4pt padding and 6pt space-after gives the usual 28pt). The single title line
/// counts without its leading (matters for CJK faces whose leading is 0.5em). A stored
/// table_name_height (33) wins.
fn title_band(table: &NumbersTable, styles: &Styles) -> Title {
    let style = TextStyle::resolve(styles, table.model.reference(30), 0, None);
    let padding = styles
        .text_frame_prop(table.model.reference(36), 6)
        .map(|holder| holder.message(6));
    let side = |field: i32| padding.as_ref().map_or(4.0, |padding| padding.float(field));
    let (pad_left, pad_top, pad_right, pad_bottom) = (side(1), side(2), side(3), side(4));
    let stored_height = table.model.double(33) as f32;
    let computed = pad_top
        + text::title_line(&style).0
        + style.space_after
        + pad_bottom
        + cjk_fallback_extra(&table.name, &style);
    Title {
        band: if stored_height > 0.0 {
            stored_height
        } else {
            computed
        },
        pad_top,
        pad_left,
        pad_right,
        stored_height,
        style,
    }
}

/// A CJK character in a Latin-font title falls back to a CJK face whose 0.5em leading grows the
/// line by a quarter em (measured: "表 1" in Helvetica Neue 12pt gives a 31pt band, not 28).
fn cjk_fallback_extra(title: &str, style: &TextStyle) -> f32 {
    let cjk_face = text::starts_with_ignore_case(&style.font_name, "Hira")
        || text::starts_with_ignore_case(&style.font_name, "PingFang");
    let has_cjk = title.chars().any(|character| matches!(u32::from(character), 0x3000..=0x9FFF | 0xAC00..=0xD7AF | 0xFF00..=0xFFEF));
    if has_cjk && !cjk_face {
        (0.25 * style.size).round()
    } else {
        0.0
    }
}

/// A conditional-format predicate: a postfix formula over the cell's own value. Comparisons
/// (7 > ... 12 ≠) of numbers or text are evaluated; anything else is undecided (`None`).
fn predicate(formula: &Proto, cell: &Cell) -> Option<bool> {
    #[derive(Debug, Clone, PartialEq)]
    enum Operand {
        Empty,
        Number(f64),
        Text(String),
    }
    let mut stack: Vec<Operand> = Vec::new();
    for node in formula.message(1).messages(1) {
        let operand = match node.int(1) {
            // The cell itself.
            63 | 27 | 36 => match cell.kind {
                CellKind::Text => Operand::Text(cell.text.clone().unwrap_or_default()),
                CellKind::Empty => Operand::Empty,
                _ => Operand::Number(cell.number),
            },
            17 => Operand::Number(node.double(4)),
            18 => Operand::Number(f64::from(u8::from(node.boolean(5)))),
            19 => Operand::Text(node.string(6).unwrap_or_default()),
            13 => match stack.pop()? {
                Operand::Number(number) => Operand::Number(-number),
                _ => return None,
            },
            comparison @ 7..=12 => {
                let (right, left) = (stack.pop()?, stack.pop()?);
                let ordering = match (left, right) {
                    (Operand::Number(left), Operand::Number(right)) => left.partial_cmp(&right)?,
                    (Operand::Text(left), Operand::Text(right)) => {
                        left.to_uppercase().cmp(&right.to_uppercase())
                    }
                    // Mismatched kinds: only "not equal" holds.
                    _ => return Some(comparison == 12),
                };
                use std::cmp::Ordering::{Equal, Greater, Less};
                let holds = match comparison {
                    7 => ordering == Greater,
                    8 => ordering != Less,
                    9 => ordering == Less,
                    10 => ordering != Greater,
                    11 => ordering == Equal,
                    _ => ordering != Equal,
                };
                Operand::Number(f64::from(u8::from(holds)))
            }
            // Functions and other nodes are not evaluated.
            _ => return None,
        };
        stack.push(operand);
    }
    match stack.as_slice() {
        [Operand::Number(result)] => Some(*result != 0.0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_styles_prefer_columns() {
        assert_eq!(strip_style(&[0, 7], &[3, 4], (1, 1)), 7);
        assert_eq!(strip_style(&[0, 7], &[3, 4], (1, 0)), 4);
        assert_eq!(strip_style(&[], &[], (5, 5)), 0);
    }

    #[test]
    fn cjk_titles_grow() {
        let style = TextStyle {
            size: 12.0,
            ..TextStyle::default()
        };
        assert_eq!(cjk_fallback_extra("表 1", &style), 3.0);
        assert_eq!(cjk_fallback_extra("Table 1", &style), 0.0);
    }

    #[test]
    fn predicate_compares_the_cell() {
        // cell (63) > 5 (17, double 5) → node list in a TSCE formula { 1: { 1: node... } }
        let node = |bytes: Vec<u8>| {
            let mut wrapped = vec![0x0a, bytes.len() as u8];
            wrapped.extend(bytes);
            wrapped
        };
        let mut number = vec![0x08, 17, 0x21];
        number.extend(5.0f64.to_le_bytes());
        let mut nodes = node(vec![0x08, 63]);
        nodes.extend(node(number));
        nodes.extend(node(vec![0x08, 7]));
        let mut formula = vec![0x0a, nodes.len() as u8];
        formula.extend(nodes);
        let formula = Proto::new(Rc::new(formula.clone()), 0, formula.len());
        let cell = |value: f64| Cell {
            kind: CellKind::Number,
            number: value,
            ..Cell::default()
        };
        assert_eq!(predicate(&formula, &cell(6.0)), Some(true));
        assert_eq!(predicate(&formula, &cell(4.0)), Some(false));
    }
}
