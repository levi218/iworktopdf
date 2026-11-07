//! TSCH.ChartDrawableArchive (chart data in extension 10000, TSCH.ChartArchive). Draws 2D column,
//! stacked column, bar, line, area, pie and donut charts, plus two-axis and interactive charts.
//! The drawable's geometry is the plot area; the legend frame is stored relative to the plot
//! area's centre; axis labels sit outside the plot at offsets measured from Numbers exports.

use crate::cell_format;
use crate::drawables::{geometry, is_zero, union};
use crate::formula::{self, ChartValue};
use crate::iwa::{IwaObject, Proto};
use crate::shaper::{self, TaggedFont};
use crate::styles::{self, ctx, Styles};
use crate::text::{self, HAlign, TextStyle};
use skia_safe::{Canvas, Color, Paint, PaintCap, PaintJoin, PaintStyle, Path, Point, Rect};

// Offsets of axis labels from the plot area, measured on 10pt labels and growing with the label
// size (fitted on 10, 12 and 32pt labels in Numbers and Keynote, to 0.4pt).

/// An offset measured on 10pt labels, grown by `per_point` for each point above 10.
fn at_size(at_10pt: f32, per_point: f32, size: f32) -> f32 {
    at_10pt + per_point * (size - 10.0)
}

/// Gap between a value label's right edge and the plot.
fn value_label_gap(size: f32) -> f32 {
    at_size(11.325, 0.605, size)
}

/// Gap between a horizontal chart's category label and the plot.
fn category_label_gap(size: f32) -> f32 {
    at_size(11.825, 0.605, size)
}

/// Baseline of the labels under the plot, below its bottom edge.
fn below_baseline(size: f32) -> f32 {
    at_size(13.91, 1.521, size)
}

/// Bottom of the labels under the plot (their baseline plus Helvetica Neue's descent).
fn below_bottom(size: f32) -> f32 {
    below_baseline(size) + 0.213 * size
}

/// Baseline offset of a label centred vertically on a point.
fn mid_baseline(size: f32) -> f32 {
    3.73 * size / 10.0
}
const LEGEND_BASELINE: f32 = 9.59;
// Measured on a two-axis chart: a value title's baseline sits 8.5pt outside its labels (left) or
// 15pt beyond their right edge (right), reading upward and centred on the plot; rotated category
// labels start 5pt below the plot, the category title's baseline 17.7pt below them. Apple's label
// boxes pad the text by 1.5pt on each side.
const LABEL_PAD: f32 = 1.5;
const TITLE_GAP_LEFT: f32 = 8.5;
const TITLE_GAP_RIGHT: f32 = 14.97;
const ROTATED_LABEL_TOP: f32 = 5.0;
const CATEGORY_TITLE_GAP: f32 = 17.7;
const RIGHT_LABEL_GAP: f32 = 12.24;
/// Stroke width of series lines and line swatches.
const LINE_WIDTH: f32 = 1.7309;

/// Chart types (ChartArchive.type). Apple's PDF export draws the 3D types (12-19) flat, like
/// their 2D counterparts, so they map to the same variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChartType {
    Column,
    Bar,
    Line,
    Area,
    Pie,
    StackedColumn,
    StackedBar,
    StackedArea,
    TwoAxis,
    /// Columns and lines sharing one value axis (drawn like `TwoAxis` without its right axis).
    Mixed,
    Interactive,
    InteractiveBar,
    Donut,
    /// Points at (X, Y) values (type 9; 23 interactive).
    Scatter,
    InteractiveScatter,
    /// Scatter points sized by a third value (type 22; 24 interactive).
    Bubble,
    InteractiveBubble,
    /// Series as closed polygons over one spoke per category (type 27).
    Radar,
    Other,
}

impl ChartType {
    fn from_stored(value: i32) -> ChartType {
        match value {
            1 | 12 => ChartType::Column,
            2 | 13 => ChartType::Bar,
            3 | 14 => ChartType::Line,
            4 | 15 => ChartType::Area,
            5 | 16 => ChartType::Pie,
            6 | 17 => ChartType::StackedColumn,
            7 | 18 => ChartType::StackedBar,
            8 | 19 => ChartType::StackedArea,
            10 => ChartType::Mixed,
            11 => ChartType::TwoAxis,
            20 => ChartType::Interactive,
            21 => ChartType::InteractiveBar,
            25 => ChartType::Donut,
            9 => ChartType::Scatter,
            22 => ChartType::Bubble,
            23 => ChartType::InteractiveScatter,
            24 => ChartType::InteractiveBubble,
            27 => ChartType::Radar,
            _ => ChartType::Other,
        }
    }

    /// Charts whose series are each drawn as columns or as a line.
    fn is_combo(self) -> bool {
        matches!(self, ChartType::TwoAxis | ChartType::Mixed)
    }

    /// Charts plotting each point at an X and a Y value.
    fn is_xy(self) -> bool {
        matches!(
            self,
            ChartType::Scatter
                | ChartType::InteractiveScatter
                | ChartType::Bubble
                | ChartType::InteractiveBubble
        )
    }

    fn is_bubble(self) -> bool {
        matches!(self, ChartType::Bubble | ChartType::InteractiveBubble)
    }

    fn is_pie(self) -> bool {
        matches!(self, ChartType::Pie | ChartType::Donut)
    }

    fn horizontal(self) -> bool {
        matches!(self, ChartType::Bar | ChartType::StackedBar)
    }

    fn stacked(self) -> bool {
        matches!(
            self,
            ChartType::StackedColumn | ChartType::StackedBar | ChartType::StackedArea
        )
    }

    /// Line and area charts put categories on the plot edges.
    fn on_edges(self) -> bool {
        matches!(
            self,
            ChartType::Line | ChartType::Area | ChartType::StackedArea
        )
    }

    /// The series style field holding the fill: bars 12, areas 11, others 13.
    fn fill_field(self) -> i32 {
        match self {
            ChartType::Bar | ChartType::StackedBar => 12,
            ChartType::Area | ChartType::StackedArea => 11,
            _ => 13,
        }
    }
}

/// A value axis's top value and step.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Scale {
    top: f64,
    step: f64,
}

impl Scale {
    fn ticks(self) -> usize {
        (self.top / self.step).round() as usize
    }

    fn values(self) -> impl Iterator<Item = f64> {
        (0..=self.ticks()).map(move |tick| tick as f64 * self.step)
    }
}

/// The plot rectangle's edges.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Plot {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl Plot {
    fn width(&self) -> f32 {
        self.right - self.left
    }
    fn height(&self) -> f32 {
        self.bottom - self.top
    }
}

/// A chart placed on a canvas.
pub struct ChartItem {
    styles: Styles,
    chart: Proto,
    frame: Rect,
    chart_type: ChartType,
    series: Vec<String>,
    categories: Vec<String>,
    /// Values by [series][category].
    values: Vec<Vec<f64>>,
    /// Each shown series's index among all the chart's series (hidden ones dropped).
    original_index: Option<Vec<usize>>,
    series_styles: Vec<u64>,
}

/// The chart label default: 10pt Helvetica Neue.
fn label_style() -> TextStyle {
    TextStyle::simple("HelveticaNeue", 10.0, Color::BLACK, HAlign::Left)
}

fn paint(color: Color) -> Paint {
    styles::fill_paint(color)
}

fn stroke_paint(color: Color, width: f32) -> Paint {
    let mut stroke = styles::fill_paint(color);
    stroke.set_style(PaintStyle::Stroke);
    stroke.set_stroke_width(width);
    stroke
}

/// Grid lines: 0.5pt grey.
fn grid_paint() -> Paint {
    stroke_paint(Color::from_rgb(184, 184, 184), 0.5)
}

/// The axis line: 1pt black, square caps.
fn axis_paint() -> Paint {
    let mut axis = stroke_paint(Color::BLACK, 1.0);
    axis.set_stroke_cap(PaintCap::Square);
    axis
}

fn max_width<'a>(texts: impl Iterator<Item = &'a String>, font: &TaggedFont) -> f32 {
    texts
        .map(|text| shaper::measure(text, font))
        .fold(0.0, f32::max)
}

/// A multiple of `small_quantum` magnitudes at or above raw (measured with 0.25: 457,485 → 125,000
/// for four steps, 131,280 → 35,000, 82 → 22.5); above five units, a multiple of 2.5 (3.22 → 1.00
/// per step). A scatter chart's X axis steps by whole magnitudes (1.0: 15 → 4 per step).
fn nice_step(raw: f64, small_quantum: f64) -> f64 {
    let magnitude = 10f64.powf(raw.log10().floor());
    let units = raw / magnitude;
    let quantum = if units > 5.0 { 2.5 } else { small_quantum };
    (units / quantum - 1e-9).ceil() * quantum * magnitude
}

impl ChartItem {
    /// Reads a chart drawable.
    ///
    /// # Arguments
    ///
    /// * `styles` - The document's styles
    /// * `drawable` - The TSCH.ChartDrawableArchive body
    ///
    /// # Returns
    ///
    /// The chart, its data re-read from linked cells when the cached grid is stale.
    pub fn new(styles: &Styles, drawable: &Proto) -> ChartItem {
        let (left, top, width, height, _) = geometry(&drawable.message(1));
        let chart = drawable.message(10000);
        let grid = chart.message(7);
        let (row_names, column_names) = (grid.strings(1), grid.strings(2));
        let rows: Vec<Vec<f64>> = grid
            .messages(3)
            .iter()
            .map(|row| {
                row.messages(1)
                    .iter()
                    .map(|value| value.double(1))
                    .collect()
            })
            .collect();
        let by_row = chart.int_or(5, 1) != 2;
        let (series, categories) = if by_row {
            (row_names, column_names)
        } else {
            (column_names, row_names)
        };
        let mut values = vec![vec![0.0; categories.len()]; series.len()];
        for (row_index, row) in rows.iter().enumerate() {
            for (column_index, &value) in row.iter().enumerate() {
                let (series_index, category_index) = if by_row {
                    (row_index, column_index)
                } else {
                    (column_index, row_index)
                };
                if let Some(slot) = values
                    .get_mut(series_index)
                    .and_then(|series| series.get_mut(category_index))
                {
                    *slot = value;
                }
            }
        }
        // Per-series private styles (sparse array, field 18) override the theme styles (17).
        let mut series_styles = chart.references(17);
        for entry in chart.message(18).messages(2) {
            let index = entry.uint(1) as usize;
            if series_styles.len() <= index {
                series_styles.resize(index + 1, 0);
            }
            if entry.reference(2) != 0 {
                series_styles[index] = entry.reference(2);
            }
        }
        let mut item = ChartItem {
            styles: styles.clone(),
            chart_type: ChartType::from_stored(chart.int(1)),
            frame: Rect::from_xywh(left, top, width, height),
            chart,
            series,
            categories,
            values,
            original_index: None,
            series_styles,
        };
        item.read_linked_data();
        item
    }

    /// Re-reads the data from the cells the chart is linked to (TN.ChartMediatorArchive formulas:
    /// one data formula per series, one label formula per series and per category), as Numbers
    /// does on open; the cached grid can be stale in files written by other tools. Keeps the grid
    /// when any formula cannot be evaluated, or when it has the same shape (Numbers' own result).
    fn read_linked_data(&mut self) {
        let doc = self.styles.doc();
        let Some(mediator) = doc.body(self.chart.reference(8)) else {
            return;
        };
        let formulas = mediator.message(3);
        let evaluate = |field: i32| {
            formulas
                .messages(field)
                .iter()
                .map(|formula| formula::chart_values(doc, formula))
                .collect::<Vec<_>>()
        };
        let data = evaluate(1);
        if data.is_empty() || data.iter().any(Option::is_none) {
            return;
        }
        let data: Vec<Vec<Option<ChartValue>>> = data.into_iter().flatten().collect();
        // Labels of hidden rows/columns come back empty: those categories and series are left out.
        let series_labels = evaluate(4);
        let category_labels = evaluate(3);
        let labels_match = series_labels.len() == data.len();
        let keep: Vec<usize> = (0..data.len())
            .filter(|&index| {
                let hidden =
                    labels_match && series_labels[index].as_ref().is_some_and(Vec::is_empty);
                !(data[index].is_empty() || hidden)
            })
            .collect();
        if keep.is_empty() {
            return;
        }
        let first_label = |labels: &Option<Vec<Option<ChartValue>>>| {
            labels
                .as_ref()
                .and_then(|labels| labels.first())
                .map(ChartValue::label)
                .unwrap_or_default()
        };
        let mut series_names: Vec<String> = if labels_match {
            keep.iter()
                .map(|&index| first_label(&series_labels[index]))
                .collect()
        } else {
            Vec::new()
        };
        let data: Vec<&Vec<Option<ChartValue>>> = keep.iter().map(|&index| &data[index]).collect();
        self.original_index = Some(keep);
        let mut category_names: Vec<String> = category_labels
            .iter()
            .filter(|labels| !labels.as_ref().is_some_and(Vec::is_empty))
            .map(first_label)
            .collect();
        let category_count = data.iter().map(|series| series.len()).max().unwrap_or(0);
        if data.len() == self.series.len() && category_count == self.categories.len() {
            return;
        }
        let values = data
            .iter()
            .map(|series| {
                (0..category_count)
                    .map(|index| {
                        if let Some(Some(ChartValue::Number(number))) = series.get(index) {
                            *number
                        } else {
                            0.0
                        }
                    })
                    .collect()
            })
            .collect();
        if series_names.len() != data.len() {
            series_names = if self.series.len() == data.len() {
                self.series.clone()
            } else {
                (1..=data.len())
                    .map(|index| format!("Series {index}"))
                    .collect()
            };
        }
        if self.categories.len() == category_count {
            category_names = self.categories.clone();
        } else if category_names.len() != category_count {
            category_names = (1..=category_count)
                .map(|index| index.to_string())
                .collect();
        }
        self.series = series_names;
        self.categories = category_names;
        self.values = values;
    }

    fn value(&self, series: usize, category: usize) -> f64 {
        self.values
            .get(series)
            .and_then(|values| values.get(category))
            .copied()
            .unwrap_or(0.0)
    }

    fn original(&self, series: usize) -> usize {
        self.original_index
            .as_ref()
            .and_then(|indexes| indexes.get(series))
            .copied()
            .unwrap_or(series)
    }

    /// A chart-style property (extension 10000 of the style), following parents.
    fn style_property(&self, style: u64, field: i32) -> Option<Proto> {
        if style == 0 {
            return None;
        }
        self.styles.find_in(
            style,
            |object: &IwaObject| object.body.message_opt(10000),
            field,
        )
    }

    fn style_int(&self, style: u64, field: i32) -> Option<i32> {
        self.style_property(style, field)
            .map(|props| props.int(field))
    }

    fn style_float(&self, style: u64, field: i32) -> Option<f32> {
        self.style_property(style, field)
            .map(|props| props.float(field))
    }

    /// Entry `index` of the chart's paragraph_styles (20), resolved over `fallback`.
    fn chart_paragraph(&self, index: Option<i32>, fallback: &TextStyle) -> TextStyle {
        let references = self.chart.references(20);
        match index
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| references.get(index))
            .filter(|&&id| id != 0)
        {
            Some(&id) => TextStyle::resolve(&self.styles, id, 0, Some(fallback)),
            None => fallback.clone(),
        }
    }

    /// Gaps between a set's bars and between sets, as fractions of a bar (chart style 16, 17;
    /// defaults 10% / 40%).
    fn bar_gap(&self) -> f32 {
        self.style_float(self.chart.reference(9), 16)
            .unwrap_or(10.0)
            / 100.0
    }

    fn set_gap(&self) -> f32 {
        self.style_float(self.chart.reference(9), 17)
            .unwrap_or(40.0)
            / 100.0
    }

    /// A value-axis label: the cached axis_type_y formatter (extension 10030) when it is more than a
    /// plain decimal (e.g. currency "$125,000"), otherwise the general format.
    fn tick_label(&self, value: f64) -> String {
        let formats = self.chart.message(10030).messages(1);
        let custom = formats
            .iter()
            .filter(|entry| entry.message(1).int(1) == 2)
            .find_map(|entry| {
                entry
                    .message_opt(2)
                    .filter(|format| !matches!(format.int(1), 0 | 256))
            });
        match custom {
            Some(format) => cell_format::number(value, Some(&format), false),
            None => cell_format::general(value),
        }
    }

    /// A value axis's non-style property (value_axis_nonstyles 14).
    fn axis_property(&self, axis: usize, field: i32) -> Option<Proto> {
        let id = self.chart.references(14).get(axis).copied().unwrap_or(0);
        self.style_property(id, field)
    }

    /// A value-axis label: the axis's own number format (42) when set, else the chart's.
    fn axis_label(&self, axis: usize, value: f64) -> String {
        if let Some(format) = self
            .axis_property(axis, 42)
            .map(|props| props.message(42))
            .filter(|format| format.int(1) != 0)
        {
            return cell_format::number(value, Some(&format), false);
        }
        if axis == 0 {
            self.tick_label(value)
        } else {
            cell_format::general(value)
        }
    }

    /// Category label rotation in degrees (category axis style 9; 270 = reading downward).
    fn category_orientation(&self) -> f32 {
        let id = self.chart.references(15).first().copied().unwrap_or(0);
        self.style_float(id, 9).unwrap_or(0.0)
    }

    fn rotated_categories(&self) -> bool {
        (45.0..315.0).contains(&self.category_orientation()) && self.category_orientation() != 45.0
    }

    fn category_property(&self, field: i32) -> Option<Proto> {
        self.style_property(
            self.chart.references(16).first().copied().unwrap_or(0),
            field,
        )
    }

    /// Category labels are hidden when the category axis non-style says so (9).
    fn show_category_labels(&self) -> bool {
        self.category_property(9)
            .is_none_or(|props| props.boolean_or(9, true))
    }

    /// An axis title when the axis shows it: value axes (non-style 14/16), category (13/15).
    fn value_title(&self, axis: usize) -> Option<String> {
        let shown = self
            .axis_property(axis, 14)
            .is_some_and(|props| props.boolean(14));
        shown
            .then(|| {
                self.axis_property(axis, 16)
                    .and_then(|props| props.string(16))
            })
            .flatten()
            .filter(|title| !title.is_empty())
    }

    fn category_title(&self) -> Option<String> {
        let shown = self
            .category_property(13)
            .is_some_and(|props| props.boolean(13));
        shown
            .then(|| {
                self.category_property(15)
                    .and_then(|props| props.string(15))
            })
            .flatten()
            .filter(|title| !title.is_empty())
    }

    /// Axis title text style: the axis style's title paragraph index (40 value, 38 category),
    /// else its default (39).
    fn title_style(&self, value_axis: bool) -> TextStyle {
        let references = if value_axis {
            self.chart.references(13)
        } else {
            self.chart.references(15)
        };
        let id = references.first().copied().unwrap_or(0);
        let index = self
            .style_int(id, if value_axis { 40 } else { 38 })
            .or_else(|| self.style_int(id, 39));
        self.chart_paragraph(index, &label_style())
    }

    /// The label style of an axis: the axis style's value (8) / category (6) label index, else its
    /// default (7).
    fn axis_style(&self, value_axis: bool, axis: usize) -> TextStyle {
        let references = if value_axis {
            self.chart.references(13)
        } else {
            self.chart.references(15)
        };
        let id = references
            .get(axis)
            .or(references.first())
            .copied()
            .unwrap_or(0);
        let index = self
            .style_int(id, if value_axis { 8 } else { 6 })
            .or_else(|| self.style_int(id, 7));
        self.chart_paragraph(index, &label_style())
    }

    fn legend_style(&self) -> TextStyle {
        self.chart_paragraph(self.style_int(self.chart.reference(11), 2), &label_style())
    }

    /// The legend frame, stored relative to the plot area's centre (hidden legends take no space:
    /// chart non-style 20; unset, Keynote hides the legend while Numbers and Pages show it).
    fn legend_frame(&self) -> Rect {
        let frame = self.chart.message(3);
        let hidden = self
            .style_property(self.chart.reference(10), 20)
            .map_or_else(|| ctx::KEYNOTE.get(), |props| !props.boolean(20));
        if frame.is_empty() || hidden {
            return Rect::default();
        }
        let (origin, size) = (frame.message(1), frame.message(2));
        Rect::from_xywh(
            self.frame.center_x() + origin.float(1),
            self.frame.center_y() + origin.float(2),
            size.float(1),
            size.float(2),
        )
    }

    fn plot(&self) -> Plot {
        Plot {
            left: self.frame.left,
            top: self.frame.top,
            right: self.frame.right,
            bottom: self.frame.bottom,
        }
    }

    // ------------------------------------------------------------- scales

    /// The value axis's top and step: the axis's number of major gridlines (5, default 4) divides
    /// the range; a user maximum (17) is kept as given.
    fn axis_scale(&self, maximum: f64, axis: usize) -> Scale {
        let steps = self
            .axis_property(axis, 5)
            .map(|props| props.int(5))
            .filter(|&count| count > 0)
            .unwrap_or(4);
        if let Some(user_maximum) = self
            .axis_property(axis, 17)
            .map(|props| props.message(17))
            .filter(|limit| limit.has(1) && limit.double(1) > 0.0)
        {
            let top = user_maximum.double(1);
            return Scale {
                top,
                step: top / f64::from(steps),
            };
        }
        let maximum = if maximum <= 0.0 { 1.0 } else { maximum };
        // Axis 1 of a scatter or bubble chart is its X axis.
        let small_quantum = if self.chart_type.is_xy() && axis == 1 {
            1.0
        } else {
            0.25
        };
        let step = nice_step(maximum / f64::from(steps), small_quantum);
        Scale {
            top: step * f64::from(steps),
            step,
        }
    }

    /// The scale over all series (stacked charts: over each category's positive total).
    fn scale(&self) -> Scale {
        let categories = 0..self.categories.len();
        let maximum = if self.chart_type.stacked() {
            categories
                .map(|category| {
                    (0..self.series.len())
                        .map(|series| self.value(series, category).max(0.0))
                        .sum::<f64>()
                })
                .fold(0.0, f64::max)
        } else {
            categories
                .flat_map(|category| (0..self.series.len()).map(move |series| (series, category)))
                .map(|(series, category)| self.value(series, category))
                .fold(0.0, f64::max)
        };
        self.axis_scale(maximum, 0)
    }

    /// The scale over some series on an axis.
    fn scale_of(&self, series: &[usize], axis: usize) -> Scale {
        let maximum = series
            .iter()
            .flat_map(|&index| (0..self.categories.len()).map(move |category| (index, category)))
            .map(|(index, category)| self.value(index, category))
            .fold(0.0, f64::max);
        self.axis_scale(maximum, axis)
    }

    /// A series non-style property (extension 10000), by shown series index.
    fn series_property(&self, series: usize, field: i32) -> Option<Proto> {
        let original = self.original(series);
        let entries = self.chart.message(19).messages(2);
        let id = entries
            .iter()
            .find(|entry| entry.uint(1) as usize == original)
            .map(|entry| entry.reference(2))
            .filter(|&id| id != 0)?;
        self.style_property(id, field)
    }

    /// Two-axis charts: a series's type (non-style 26: 2 column, 0 line) and axis (64), else by its
    /// index among all series: even ones lines on the left axis, odd ones columns on the right.
    fn series_is_column(&self, series: usize) -> bool {
        match self.series_property(series, 26).map(|props| props.int(26)) {
            Some(series_type) if series_type >= 0 => series_type == 2,
            _ => self.original(series) % 2 == 1,
        }
    }

    /// The value axis a series is plotted against: always the left (0) one on a mixed chart.
    fn series_axis(&self, series: usize) -> usize {
        if self.chart_type == ChartType::Mixed {
            return 0;
        }
        match self
            .series_property(series, 64)
            .filter(|props| props.has(64))
            .map(|props| props.int(64))
        {
            Some(axis) if axis >= 0 => axis as usize,
            _ => self.original(series) % 2,
        }
    }

    /// The style field holding a series' colour: pies fill wedges (17); two-axis charts colour
    /// their columns (16) and lines (51) separately (measured: a column and a line series sharing
    /// one colour while their theme colours differ).
    fn color_field(&self, series: usize) -> i32 {
        match self.chart_type {
            chart_type if chart_type.is_pie() => 17,
            chart_type if chart_type.is_combo() && self.series_is_column(series) => 16,
            chart_type if chart_type.is_combo() => 51,
            chart_type => chart_type.fill_field(),
        }
    }

    /// A series' style: its private style, else the theme's for its index (0 when none).
    fn series_style(&self, series: usize) -> u64 {
        if self.series_styles.is_empty() {
            return 0;
        }
        let index = self.original(series);
        match self.series_styles[index % self.series_styles.len()] {
            0 => {
                let theme = self.chart.references(17);
                theme.get(index % theme.len().max(1)).copied().unwrap_or(0)
            }
            id => id,
        }
    }

    /// A two-axis line series' stroke width (series style 51): `None` when its stroke is empty
    /// (markers only), the default width when the style sets none.
    fn line_width(&self, series: usize) -> Option<f32> {
        match self.style_property(self.series_style(series), 51) {
            Some(props) => styles::stroke_of(Some(&props.message(51)), false).map(|s| s.width),
            None => Some(LINE_WIDTH),
        }
    }

    fn series_color(&self, series: usize, field: i32) -> Color {
        let index = self.original(series);
        if !self.series_styles.is_empty() {
            let id = self.series_style(series);
            // The type's fill field, else the default fill (14).
            for fill_field in [field, 14] {
                if let Some(color) = self
                    .style_property(id, fill_field)
                    .and_then(|props| styles::fill_color(Some(&props.message(fill_field))))
                {
                    return color;
                }
            }
        }
        const FALLBACK: [(u8, u8, u8); 6] = [
            (0, 162, 255),
            (97, 216, 54),
            (146, 146, 146),
            (248, 186, 0),
            (255, 38, 0),
            (0, 84, 147),
        ];
        let (red, green, blue) = FALLBACK[index % FALLBACK.len()];
        Color::from_rgb(red, green, blue)
    }

    // ------------------------------------------------------------- bounds

    /// The chart's extent: plot, legend, axis labels and titles.
    pub fn bounds(&self) -> Rect {
        let mut bounds = self.frame;
        let legend = self.legend_frame();
        if !is_zero(&legend) {
            bounds = union(bounds, legend);
        }
        if self.chart_type == ChartType::Donut {
            // Room for the donut's outside labels (measured).
            return union(
                bounds,
                self.frame
                    .with_outset((self.frame.width() * 0.2177, self.frame.width() * 0.2177)),
            );
        }
        if self.chart_type.is_pie() {
            return bounds;
        }
        let plot = self.plot();
        let category_font = self.axis_style(false, 0).font();
        let value_font = self.axis_style(true, 0).font();
        if self.chart_type.horizontal() {
            let widest = if self.categories.is_empty() || !self.show_category_labels() {
                0.0
            } else {
                max_width(self.categories.iter(), &category_font)
                    + category_label_gap(category_font.font.size())
                    + 0.5
            };
            bounds = union(
                bounds,
                Rect::new(
                    plot.left - widest,
                    plot.top,
                    plot.left,
                    plot.bottom + below_bottom(value_font.font.size()),
                ),
            );
            // The last value label is centred on the plot's right edge and overhangs it.
            let half =
                shaper::measure(&self.tick_label(self.scale().top), &value_font) / 2.0 + LABEL_PAD;
            return union(
                bounds,
                Rect::new(plot.left, plot.top, plot.right + half, plot.bottom),
            );
        }
        let scale = self.scale();
        let widest_tick = scale
            .values()
            .map(|value| shaper::measure(&self.tick_label(value), &value_font))
            .fold(0.0, f32::max);
        bounds = union(
            bounds,
            Rect::new(
                plot.left - value_label_gap(value_font.font.size()) - widest_tick - 0.5,
                plot.top,
                plot.right + 0.5,
                plot.bottom + below_bottom(category_font.font.size()),
            ),
        );
        // Line and area charts put the categories on the plot edges: the last label is centred on
        // the right edge and overhangs it (only when every label fits: crowded axes skip labels).
        let labels_fit = self
            .categories
            .iter()
            .map(|category| shaper::measure(category, &category_font) + 4.0)
            .sum::<f32>()
            <= plot.width();
        if let (true, true, true, Some(last)) = (
            self.chart_type.on_edges(),
            self.categories.len() > 1 && self.show_category_labels(),
            labels_fit,
            self.categories.last(),
        ) {
            bounds = union(
                bounds,
                Rect::new(
                    plot.left,
                    plot.top,
                    plot.right + shaper::measure(last, &category_font) / 2.0 + LABEL_PAD,
                    plot.bottom,
                ),
            );
        }
        bounds = union(
            bounds,
            self.decoration_bounds(scale, &value_font, &category_font),
        );
        if self.chart_type == ChartType::TwoAxis && self.series.len() > 1 {
            let right_series: Vec<usize> = (0..self.series.len())
                .filter(|&series| self.series_axis(series) == 1)
                .collect();
            let right_scale = if right_series.is_empty() {
                self.scale()
            } else {
                self.scale_of(&right_series, 1)
            };
            let right_font = self.axis_style(true, 1).font();
            let widest = right_scale
                .values()
                .map(|value| shaper::measure(&self.axis_label(1, value), &right_font))
                .fold(0.0, f32::max);
            let title = if self.value_title(1).is_some() {
                TITLE_GAP_RIGHT + self.title_style(true).size * 0.213
            } else {
                0.0
            };
            bounds = union(
                bounds,
                Rect::new(
                    plot.left,
                    plot.top,
                    plot.right + RIGHT_LABEL_GAP + widest + title,
                    plot.bottom,
                ),
            );
        }
        bounds
    }

    /// Titles and rotated labels widen the frame (measured to 0.1pt on a two-axis chart).
    fn decoration_bounds(
        &self,
        scale: Scale,
        value_font: &TaggedFont,
        category_font: &TaggedFont,
    ) -> Rect {
        let plot = self.plot();
        let title_style = self.title_style(true);
        let mut bounds = self.frame;
        if self.value_title(0).is_some() {
            let widest = scale
                .values()
                .map(|value| shaper::measure(&self.axis_label(0, value), value_font))
                .fold(0.0, f32::max);
            bounds = union(
                bounds,
                Rect::new(
                    plot.left
                        - value_label_gap(value_font.font.size())
                        - widest
                        - TITLE_GAP_LEFT
                        - text::ascent(&title_style),
                    plot.top,
                    plot.left,
                    plot.bottom,
                ),
            );
        }
        let mut bottom = plot.bottom + below_bottom(category_font.font.size());
        if self.rotated_categories() && !self.categories.is_empty() {
            bottom = bottom.max(
                plot.bottom + ROTATED_LABEL_TOP + max_width(self.categories.iter(), category_font),
            );
        }
        if self.category_title().is_some() {
            bottom += CATEGORY_TITLE_GAP + title_style.size * 0.213;
        }
        union(bounds, Rect::new(plot.left, plot.top, plot.right, bottom))
    }

    // ------------------------------------------------------------- drawing

    /// Draws the chart, its title and legend.
    pub fn draw(&self, canvas: &Canvas) {
        if self.chart_type.is_pie() {
            self.draw_pie(canvas);
        } else if self.chart_type == ChartType::Radar {
            self.draw_radar(canvas);
        } else {
            self.draw_xy(canvas);
        }
        self.draw_title(canvas);
        let legend = self.legend_frame();
        if !is_zero(&legend) {
            self.draw_legend(canvas, legend);
        }
    }

    /// The chart title (chart non-style: show 21, text 23; chart style: paragraph index 20),
    /// centred over the plot with its baseline 15.25pt above it (measured, 13pt title).
    fn draw_title(&self, canvas: &Canvas) {
        let non_style = self.chart.reference(10);
        if !self
            .style_property(non_style, 21)
            .is_some_and(|props| props.boolean(21))
        {
            return;
        }
        let Some(title) = self
            .style_property(non_style, 23)
            .and_then(|props| props.string(23))
            .filter(|title| !title.is_empty())
        else {
            return;
        };
        let style =
            self.chart_paragraph(self.style_int(self.chart.reference(9), 20), &label_style());
        let font = style.font();
        let label_x =
            self.frame.left + (self.frame.width() + 1.0 - shaper::measure(&title, &font)) / 2.0;
        text::draw_line(
            canvas,
            &title,
            Point::new(label_x, self.frame.top - 15.25),
            &font,
            &style,
        );
    }

    fn legend_entries(&self) -> Vec<String> {
        if self.chart_type.is_pie() {
            self.pie_names()
        } else if self.chart_type.is_xy() {
            self.y_series()
                .iter()
                .map(|&series| self.series[series].clone())
                .collect()
        } else {
            self.series.clone()
        }
    }

    fn draw_legend(&self, canvas: &Canvas, frame: Rect) {
        let style = self.legend_style();
        let font = style.font();
        let entries = self.legend_entries();
        const SWATCH: f32 = 10.0;
        const GAP: f32 = 6.0;
        // The least space between entries before the legend wraps to one entry per line.
        const SPACING: f32 = 9.63;
        let widths: Vec<f32> = entries
            .iter()
            .map(|entry| shaper::measure(entry, &font))
            .collect();
        let total = widths.iter().sum::<f32>()
            + entries.len() as f32 * (SWATCH + GAP)
            + entries.len().saturating_sub(1) as f32 * SPACING;
        if total > frame.width() + 1.0 {
            // Too wide for its frame: one entry per line (measured on a 13pt pie legend: 12pt circles
            // at left+12, text 22.31 further, first baseline top+11.75, line pitch).
            let diameter = (style.size * 12.0 / 13.0).round();
            let pitch = text::line_height(&style);
            for (index, entry) in entries.iter().enumerate() {
                let baseline = frame.top + 11.75 + index as f32 * pitch;
                let swatch_x = frame.left + 12.0;
                let fill = paint(self.series_color(index, self.color_field(index)));
                if self.chart_type.is_pie() {
                    canvas.draw_circle(
                        (swatch_x + diameter / 2.0, baseline - 4.51),
                        diameter / 2.0,
                        &fill,
                    );
                } else {
                    canvas.draw_rect(
                        Rect::from_xywh(
                            swatch_x,
                            baseline - 4.51 - diameter / 2.0,
                            diameter,
                            diameter,
                        ),
                        &fill,
                    );
                }
                text::draw_line(
                    canvas,
                    entry,
                    Point::new(swatch_x + 22.31, baseline),
                    &font,
                    &style,
                );
            }
            return;
        }
        // One row: the space left in the frame is shared equally before, between and after the
        // entries (measured on 1-6 entry legends in Numbers and Pages, to 0.5pt).
        let count = entries.len() as f32;
        let gap = (frame.width() - (total - (count - 1.0) * SPACING)) / (count + 1.0);
        let mut pen_x = frame.left + gap;
        for (index, entry) in entries.iter().enumerate() {
            let swatch_x = pen_x.floor();
            let color = self.series_color(index, self.color_field(index));
            let top = frame.top + 1.0;
            match self.chart_type {
                chart_type if chart_type.is_combo() && !self.series_is_column(index) => {
                    draw_marker(
                        canvas,
                        (swatch_x + SWATCH / 2.0, top + SWATCH / 2.0).into(),
                        color,
                        self.line_width(index).unwrap_or(LINE_WIDTH),
                    );
                }
                ChartType::Pie | ChartType::Donut => {
                    canvas.draw_circle(
                        (swatch_x + SWATCH / 2.0, top + SWATCH / 2.0),
                        SWATCH / 2.0,
                        &paint(color),
                    );
                }
                ChartType::Line => {
                    canvas.draw_line(
                        (swatch_x, top + SWATCH / 2.0),
                        (swatch_x + SWATCH, top + SWATCH / 2.0),
                        &stroke_paint(color, LINE_WIDTH),
                    );
                }
                _ => {
                    canvas.draw_rect(
                        Rect::from_xywh(swatch_x, top, SWATCH, SWATCH),
                        &paint(color),
                    );
                }
            }
            text::draw_line(
                canvas,
                entry,
                Point::new(swatch_x + SWATCH + GAP, frame.top + LEGEND_BASELINE),
                &font,
                &style,
            );
            pen_x += SWATCH + GAP + widths[index] + gap;
        }
    }

    /// Category labels under a vertical plot (rotated when the axis says so), then the titles.
    fn draw_category_axis(
        &self,
        canvas: &Canvas,
        category_x: &dyn Fn(usize) -> f32,
        label_extents: (f32, f32),
    ) {
        let (left_labels_width, right_labels_end) = label_extents;
        let plot = self.plot();
        let style = self.axis_style(false, 0);
        let font = style.font();
        let rotated = self.rotated_categories();
        if self.show_category_labels() {
            for (index, category) in self.categories.iter().enumerate() {
                if rotated {
                    rotated_text(
                        canvas,
                        category,
                        Point::new(
                            category_x(index) - mid_baseline(style.size),
                            plot.bottom + ROTATED_LABEL_TOP,
                        ),
                        90.0,
                        &style,
                    );
                } else {
                    let label_x = category_x(index) - shaper::measure(category, &font) / 2.0;
                    text::draw_line(
                        canvas,
                        category,
                        Point::new(label_x, plot.bottom + below_baseline(style.size)),
                        &font,
                        &style,
                    );
                }
            }
        }
        let labels_bottom = if rotated {
            plot.bottom + ROTATED_LABEL_TOP + max_width(self.categories.iter(), &font)
        } else {
            plot.bottom + below_bottom(style.size)
        };
        let title_style = self.title_style(false);
        if let Some(title) = self.category_title() {
            let title_font = title_style.font();
            let label_x =
                (plot.left + plot.right) / 2.0 - shaper::measure(&title, &title_font) / 2.0;
            text::draw_line(
                canvas,
                &title,
                Point::new(label_x, labels_bottom + CATEGORY_TITLE_GAP),
                &title_font,
                &title_style,
            );
        }
        let middle = (plot.top + plot.bottom) / 2.0;
        let value_title_style = self.title_style(true);
        let value_title_font = value_title_style.font();
        if let Some(title) = self.value_title(0) {
            let origin = Point::new(
                plot.left
                    - value_label_gap(self.axis_style(true, 0).size)
                    - left_labels_width
                    - TITLE_GAP_LEFT,
                middle + shaper::measure(&title, &value_title_font) / 2.0,
            );
            rotated_text(canvas, &title, origin, -90.0, &value_title_style);
        }
        if let (ChartType::TwoAxis, Some(title)) = (self.chart_type, self.value_title(1)) {
            let origin = Point::new(
                right_labels_end + TITLE_GAP_RIGHT,
                middle + shaper::measure(&title, &value_title_font) / 2.0,
            );
            rotated_text(canvas, &title, origin, -90.0, &value_title_style);
        }
    }

    /// Value labels down the left of the plot, with grid lines across it.
    fn draw_value_axis(
        &self,
        canvas: &Canvas,
        scale: Scale,
        height_of: &dyn Fn(f64) -> f32,
        label_of: &dyn Fn(f64) -> String,
    ) {
        let plot = self.plot();
        let style = self.axis_style(true, 0);
        let font = style.font();
        for value in scale.values().skip(1) {
            let grid_y = height_of(value).floor() + 0.5;
            canvas.draw_line(
                (plot.left + 0.5, grid_y),
                (plot.right, grid_y),
                &grid_paint(),
            );
        }
        for value in scale.values() {
            let label = label_of(value);
            let label_x = plot.left - value_label_gap(style.size) - shaper::measure(&label, &font);
            text::draw_line(
                canvas,
                &label,
                Point::new(label_x, height_of(value) + mid_baseline(style.size)),
                &font,
                &style,
            );
        }
    }

    fn draw_baseline(&self, canvas: &Canvas) {
        let plot = self.plot();
        let axis_y = plot.bottom.floor() + 0.5;
        canvas.draw_line((plot.left, axis_y), (plot.right, axis_y), &axis_paint());
    }

    /// A value axis's line (its style's field 25) up the plot edge at `x`.
    fn draw_value_axis_line(&self, canvas: &Canvas, axis: usize, x: f32) {
        let shown = self
            .chart
            .references(13)
            .get(axis)
            .and_then(|&id| self.style_property(id, 25))
            .is_some_and(|props| props.boolean(25));
        if shown {
            let plot = self.plot();
            let axis_x = x.floor() + 0.5;
            canvas.draw_line((axis_x, plot.top), (axis_x, plot.bottom), &axis_paint());
        }
    }

    fn draw_xy(&self, canvas: &Canvas) {
        match self.chart_type {
            ChartType::Interactive | ChartType::InteractiveBar => {
                return self.draw_interactive(canvas)
            }
            ChartType::TwoAxis | ChartType::Mixed => return self.draw_two_axis(canvas),
            chart_type if chart_type.is_xy() => return self.draw_scatter(canvas),
            chart_type if chart_type.horizontal() => return self.draw_horizontal(canvas),
            _ => {}
        }
        let plot = self.plot();
        let scale = self.scale();
        let height_of = |value: f64| plot.bottom - (value / scale.top) as f32 * plot.height();
        self.draw_value_axis(canvas, scale, &height_of, &|value| self.tick_label(value));
        let count = self.categories.len();
        let on_edges = self.chart_type.on_edges();
        let slot = if on_edges {
            if count > 1 {
                plot.width() / (count - 1) as f32
            } else {
                0.0
            }
        } else {
            plot.width() / count.max(1) as f32
        };
        let category_x = |index: usize| {
            if on_edges {
                plot.left + index as f32 * slot
            } else {
                plot.left + (index as f32 + 0.5) * slot
            }
        };
        match self.chart_type {
            ChartType::Column | ChartType::StackedColumn => {
                self.draw_columns(canvas, slot, &category_x, &height_of)
            }
            ChartType::Area | ChartType::StackedArea => {
                // Areas cover the baseline (measured: only its end past the fill shows).
                self.draw_baseline(canvas);
                self.draw_areas(canvas, &category_x, &height_of)
            }
            ChartType::Line => {
                for series in 0..self.series.len() {
                    let points: Vec<Point> = (0..count)
                        .map(|category| {
                            Point::new(
                                category_x(category),
                                height_of(self.value(series, category)),
                            )
                        })
                        .collect();
                    canvas.draw_path(
                        &polyline(&points),
                        &line_paint(self.series_color(series, 13)),
                    );
                }
            }
            _ => {}
        }
        if !matches!(self.chart_type, ChartType::Area | ChartType::StackedArea) {
            self.draw_baseline(canvas);
        }
        self.draw_value_axis_line(canvas, 0, plot.left);
        let widest = self
            .scale()
            .values()
            .map(|value| shaper::measure(&self.tick_label(value), &self.axis_style(true, 0).font()))
            .fold(0.0, f32::max);
        self.draw_category_axis(canvas, &category_x, (widest, plot.right));
    }

    /// Bars: width = slot / (n + (n-1)·bar gap + set gap), gaps from the chart style (measured: two
    /// series in a 93.6pt slot draw 37pt bars). Stacked columns build up from the first series.
    fn draw_columns(
        &self,
        canvas: &Canvas,
        slot: f32,
        category_x: &dyn Fn(usize) -> f32,
        height_of: &dyn Fn(f64) -> f32,
    ) {
        let stacked = self.chart_type.stacked();
        let bars = if stacked { 1 } else { self.series.len().max(1) };
        let (bar_gap, set_gap) = (self.bar_gap(), self.set_gap());
        let bar_width = slot / (bars as f32 + (bars as f32 - 1.0) * bar_gap + set_gap);
        let group_width = bars as f32 * bar_width + (bars as f32 - 1.0) * bar_gap * bar_width;
        let bottom = self.plot().bottom;
        let label_style = self.value_label_style(Color::WHITE);
        let label_font = label_style.font();
        for category in 0..self.categories.len() {
            let mut base = 0.0;
            for series in 0..self.series.len() {
                let value = self.value(series, category);
                let left = if stacked {
                    category_x(category) - bar_width / 2.0
                } else {
                    category_x(category) - group_width / 2.0
                        + series as f32 * bar_width * (1.0 + bar_gap)
                };
                let lower = if stacked && series > 0 {
                    height_of(base)
                } else {
                    bottom.floor()
                };
                canvas.draw_rect(
                    Rect::new(
                        left.floor(),
                        height_of(base + value),
                        (left + bar_width).floor(),
                        lower,
                    ),
                    &paint(self.series_color(series, 13)),
                );
                if self.shows_values(series) {
                    // Inside the column's end, centred (measured: 12pt bold, baseline 14.04pt
                    // below the top).
                    let label = cell_format::general(value);
                    let label_x = left + (bar_width - shaper::measure(&label, &label_font)) / 2.0;
                    let baseline = height_of(base + value) + 14.04 * label_style.size / 12.0;
                    text::draw_line(
                        canvas,
                        &label,
                        Point::new(label_x, baseline),
                        &label_font,
                        &label_style,
                    );
                }
                if stacked {
                    base += value;
                }
            }
        }
    }

    /// Overlapping (unstacked) areas are drawn last series first, so the first is in front
    /// (measured); stacked ones build up from the first.
    fn draw_areas(
        &self,
        canvas: &Canvas,
        category_x: &dyn Fn(usize) -> f32,
        height_of: &dyn Fn(f64) -> f32,
    ) {
        let count = self.categories.len();
        if count == 0 {
            return;
        }
        let stacked = self.chart_type.stacked();
        let mut bases = vec![0.0; count];
        let order: Vec<usize> = if stacked {
            (0..self.series.len()).collect()
        } else {
            (0..self.series.len()).rev().collect()
        };
        for series in order {
            let mut path = Path::new();
            path.move_to((category_x(0), height_of(bases[0])));
            for (category, base) in bases.iter().enumerate() {
                path.line_to((
                    category_x(category),
                    height_of(base + self.value(series, category)),
                ));
            }
            for category in (0..count).rev() {
                path.line_to((category_x(category), height_of(bases[category])));
            }
            path.close();
            canvas.draw_path(&path, &paint(self.series_color(series, 11)));
            if stacked {
                for (category, base) in bases.iter_mut().enumerate() {
                    *base += self.value(series, category);
                }
            }
        }
    }

    fn draw_horizontal(&self, canvas: &Canvas) {
        let plot = self.plot();
        let scale = self.scale();
        let value_style = self.axis_style(true, 0);
        let value_font = value_style.font();
        let category_style = self.axis_style(false, 0);
        let category_font = category_style.font();
        let x_of = |value: f64| plot.left + (value / scale.top) as f32 * plot.width();
        for value in scale.values().skip(1) {
            let grid_x = x_of(value).floor() + 0.5;
            canvas.draw_line((grid_x, plot.top), (grid_x, plot.bottom), &grid_paint());
        }
        for value in scale.values() {
            let label = self.tick_label(value);
            let label_x = x_of(value) - shaper::measure(&label, &value_font) / 2.0;
            text::draw_line(
                canvas,
                &label,
                Point::new(
                    label_x,
                    plot.bottom + below_baseline(value_style.size) - 0.5,
                ),
                &value_font,
                &value_style,
            );
        }
        let slot = plot.height() / self.categories.len().max(1) as f32;
        let stacked = self.chart_type.stacked();
        let bars = if stacked { 1 } else { self.series.len().max(1) };
        let bar_gap = self.bar_gap();
        let bar_height = slot / (bars as f32 + (bars as f32 - 1.0) * bar_gap + self.set_gap());
        let group_height = bars as f32 * bar_height + (bars as f32 - 1.0) * bar_gap * bar_height;
        let label_style = self.value_label_style(Color::WHITE);
        let label_font = label_style.font();
        for (category, name) in self.categories.iter().enumerate() {
            let center_y = plot.top + (category as f32 + 0.5) * slot;
            let mut base = 0.0;
            for series in 0..self.series.len() {
                let value = self.value(series, category);
                let top = if stacked {
                    center_y - bar_height / 2.0
                } else {
                    center_y - group_height / 2.0 + series as f32 * bar_height * (1.0 + bar_gap)
                };
                let bar = Rect::new(
                    x_of(base).ceil(),
                    top.floor(),
                    x_of(base + value),
                    (top + bar_height).floor(),
                );
                canvas.draw_rect(bar, &paint(self.series_color(series, 12)));
                if self.shows_values(series) {
                    // Inside the bar's end, right-aligned 2.5pt in (measured, 12pt bold).
                    let label = cell_format::general(value);
                    let label_x = x_of(base + value) - 2.5 - shaper::measure(&label, &label_font);
                    let baseline = top + bar_height / 2.0 + 0.46 * label_style.size;
                    text::draw_line(
                        canvas,
                        &label,
                        Point::new(label_x, baseline),
                        &label_font,
                        &label_style,
                    );
                }
                if stacked {
                    base += value;
                }
            }
            if self.show_category_labels() {
                let label_x = plot.left
                    - category_label_gap(category_style.size)
                    - shaper::measure(name, &category_font);
                text::draw_line(
                    canvas,
                    name,
                    Point::new(label_x, center_y + mid_baseline(category_style.size)),
                    &category_font,
                    &category_style,
                );
            }
        }
        let axis_x = plot.left.floor() + 0.5;
        canvas.draw_line((axis_x, plot.top), (axis_x, plot.bottom), &axis_paint());
    }

    /// A two-axis chart: line series with circle markers and column series, each on its axis; both
    /// axes have their own steps and the grid lines follow the left (measured: Numbers' default
    /// rainfall/temperature chart).
    fn draw_two_axis(&self, canvas: &Canvas) {
        let plot = self.plot();
        let count = self.categories.len();
        let all: Vec<usize> = (0..self.series.len()).collect();
        let lines: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&series| !self.series_is_column(series))
            .collect();
        let columns: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&series| self.series_is_column(series))
            .collect();
        let on_left: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&series| self.series_axis(series) == 0)
            .collect();
        let on_right: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&series| self.series_axis(series) == 1)
            .collect();
        let left_scale = self.scale_of(&on_left, 0);
        let right_scale = if on_right.is_empty() {
            left_scale
        } else {
            self.scale_of(&on_right, 1)
        };
        let height_left =
            |value: f64| plot.bottom - (value / left_scale.top) as f32 * plot.height();
        let height_right =
            |value: f64| plot.bottom - (value / right_scale.top) as f32 * plot.height();
        let height_of = |series: usize, value: f64| {
            if self.series_axis(series) == 0 {
                height_left(value)
            } else {
                height_right(value)
            }
        };
        self.draw_value_axis(canvas, left_scale, &height_left, &|value| {
            self.axis_label(0, value)
        });
        // Right-axis labels are right-aligned to their widest (measured: 12.24pt from the plot).
        // A mixed chart has no right axis.
        let right_style = self.axis_style(true, 1);
        let right_font = right_style.font();
        let right_labels: Vec<String> = if self.chart_type == ChartType::TwoAxis {
            right_scale
                .values()
                .map(|value| self.axis_label(1, value))
                .collect()
        } else {
            Vec::new()
        };
        let widest_right = max_width(right_labels.iter(), &right_font);
        for (label, value) in right_labels.iter().zip(right_scale.values()) {
            let label_x =
                plot.right + RIGHT_LABEL_GAP + widest_right - shaper::measure(label, &right_font);
            text::draw_line(
                canvas,
                label,
                Point::new(
                    label_x,
                    height_right(value) + mid_baseline(right_style.size),
                ),
                &right_font,
                &right_style,
            );
        }
        let slot = plot.width() / count.max(1) as f32;
        let category_x = |index: usize| plot.left + (index as f32 + 0.5) * slot;
        let bars = columns.len().max(1) as f32;
        let bar_gap = self.bar_gap();
        let bar_width = slot / (bars + (bars - 1.0) * bar_gap + self.set_gap());
        let group_width = bars * bar_width + (bars - 1.0) * bar_gap * bar_width;
        for category in 0..count {
            for (position, &series) in columns.iter().enumerate() {
                let left = category_x(category) - group_width / 2.0
                    + position as f32 * bar_width * (1.0 + bar_gap);
                let bar = Rect::new(
                    left.floor(),
                    height_of(series, self.value(series, category)),
                    (left + bar_width).floor(),
                    plot.bottom.floor(),
                );
                canvas.draw_rect(
                    bar,
                    &paint(self.series_color(series, self.color_field(series))),
                );
            }
        }
        for &series in &lines {
            let points: Vec<Point> = (0..count)
                .map(|category| {
                    Point::new(
                        category_x(category),
                        height_of(series, self.value(series, category)),
                    )
                })
                .collect();
            let color = self.series_color(series, self.color_field(series));
            let width = self.line_width(series);
            if let Some(width) = width {
                let mut line = line_paint(color);
                line.set_stroke_width(width);
                canvas.draw_path(&polyline(&points), &line);
            }
            for point in points {
                draw_marker(canvas, point, color, width.unwrap_or(LINE_WIDTH));
            }
        }
        self.draw_baseline(canvas);
        self.draw_value_axis_line(canvas, 0, plot.left);
        self.draw_value_axis_line(canvas, 1, plot.right);
        let left_font = self.axis_style(true, 0).font();
        let widest_left = left_scale
            .values()
            .map(|value| shaper::measure(&self.axis_label(0, value), &left_font))
            .fold(0.0, f32::max);
        self.draw_category_axis(
            canvas,
            &category_x,
            (widest_left, plot.right + RIGHT_LABEL_GAP + widest_right),
        );
    }

    /// A radar chart: one spoke per category clockwise from 12 o'clock, a polygon grid line at
    /// each value step, the value labels up the first spoke (its axis line drawn last, in black),
    /// category labels past the spoke ends, and per series a 15% fill under a 6pt outline; later
    /// series are drawn first (measured: the first series' fill tints the second's outline).
    fn draw_radar(&self, canvas: &Canvas) {
        const FILL_ALPHA: f32 = 0.15;
        const OUTLINE_WIDTH: f32 = 6.0;
        const VALUE_LABEL_GAP: f32 = 7.5;
        const CATEGORY_LABEL_GAP: f32 = 45.0;
        let plot = self.plot();
        let center = Point::new(
            (plot.left + plot.right) / 2.0,
            (plot.top + plot.bottom) / 2.0,
        );
        let radius = plot.height().min(plot.width()) / 2.0;
        let scale = self.scale();
        let count = self.categories.len().max(1);
        // The unit vector of spoke `index` (sin, -cos: clockwise with y growing down).
        let direction = |index: usize| {
            let angle = std::f32::consts::TAU * index as f32 / count as f32;
            Point::new(angle.sin(), -angle.cos())
        };
        let at = |index: usize, value: f64| {
            center + direction(index) * (radius * (value / scale.top) as f32)
        };
        let polygon = |value_of: &dyn Fn(usize) -> f64| {
            let points: Vec<Point> = (0..count).map(|index| at(index, value_of(index))).collect();
            let mut path = polyline(&points);
            path.close();
            path
        };
        for value in scale.values().skip(1) {
            canvas.draw_path(&polygon(&|_| value), &grid_paint());
        }
        for index in 0..count {
            canvas.draw_line(center, at(index, scale.top), &grid_paint());
        }
        for series in (0..self.series.len()).rev() {
            let color = self.series_color(series, 13);
            let outline = polygon(&|index| self.value(series, index));
            let mut fill = paint(color);
            fill.set_alpha_f(FILL_ALPHA);
            canvas.draw_path(&outline, &fill);
            canvas.draw_path(&outline, &stroke_paint(color, OUTLINE_WIDTH));
        }
        let value_style = self.axis_style(true, 0);
        let value_font = value_style.font();
        for value in scale.values() {
            let label = self.tick_label(value);
            let label_x = center.x - VALUE_LABEL_GAP - shaper::measure(&label, &value_font);
            let label_y = center.y - radius * (value / scale.top) as f32;
            text::draw_line(
                canvas,
                &label,
                Point::new(label_x, label_y + mid_baseline(value_style.size)),
                &value_font,
                &value_style,
            );
        }
        let axis_x = center.x.floor() + 0.5;
        canvas.draw_line(
            (axis_x, center.y),
            (axis_x, center.y - radius),
            &axis_paint(),
        );
        // Each label hangs off a point 45pt past its spoke: starting there on the right, ending
        // there on the left, centred at the top and bottom, its cap height centred on it (measured
        // to 6pt on a five-spoke chart).
        let style = self.axis_style(false, 0);
        let font = style.font();
        for (index, category) in self.categories.iter().enumerate() {
            let width = shaper::measure(category, &font);
            let direction = direction(index);
            let anchor = center + direction * (radius + CATEGORY_LABEL_GAP);
            let left = match direction.x {
                x if x > 0.01 => anchor.x,
                x if x < -0.01 => anchor.x - width,
                _ => anchor.x - width / 2.0,
            };
            text::draw_line(
                canvas,
                category,
                Point::new(left, anchor.y + mid_baseline(style.size)),
                &font,
                &style,
            );
        }
    }

    /// An interactive chart's first data set name, centred under the axis labels (measured:
    /// "April").
    fn draw_set_name(&self, canvas: &Canvas) {
        let Some(name) = self.categories.first() else {
            return;
        };
        let plot = self.plot();
        let style = self.axis_style(false, 0);
        let font = style.font();
        let label_x = (plot.left + plot.right) / 2.0 - shaper::measure(name, &font) / 2.0;
        text::draw_line(
            canvas,
            name,
            Point::new(
                label_x,
                plot.bottom + below_baseline(style.size) + below_bottom(style.size),
            ),
            &font,
            &style,
        );
    }

    /// The Y series of a scatter or bubble chart: the first series holds every point's X value
    /// (shared X values), each later one a Y series, followed on a bubble chart by its sizes.
    fn y_series(&self) -> Vec<usize> {
        let stride = if self.chart_type.is_bubble() { 2 } else { 1 };
        (1..self.series.len()).step_by(stride).collect()
    }

    /// A scatter or bubble chart: value axes down the left and along the bottom, horizontal grid
    /// lines only, and per point a 14pt dot (an interactive scatter: a 3pt ring) or a bubble whose
    /// area follows its size, the largest 27% of the plot height across (measured: 205pt on a
    /// 760pt plot). An interactive chart shows its first data set (point 0) on axes spanning all
    /// sets.
    fn draw_scatter(&self, canvas: &Canvas) {
        const DOT_RADIUS: f32 = 7.0;
        const RING_RADIUS: f32 = 5.5;
        const RING_WIDTH: f32 = 3.0;
        const BUBBLE_DIAMETER: f32 = 0.27;
        let plot = self.plot();
        let count = self.categories.len();
        let y_series = self.y_series();
        let x_scale = self.scale_of(&[0], 1);
        let y_scale = self.scale_of(&y_series, 0);
        let x_of = |value: f64| plot.left + (value / x_scale.top) as f32 * plot.width();
        let y_of = |value: f64| plot.bottom - (value / y_scale.top) as f32 * plot.height();
        self.draw_value_axis(canvas, y_scale, &y_of, &|value| self.tick_label(value));
        let style = self.axis_style(false, 0);
        let font = style.font();
        for value in x_scale.values() {
            let label = self.tick_label(value);
            let label_x = x_of(value) - shaper::measure(&label, &font) / 2.0;
            text::draw_line(
                canvas,
                &label,
                Point::new(label_x, plot.bottom + below_baseline(style.size)),
                &font,
                &style,
            );
        }
        let interactive = matches!(
            self.chart_type,
            ChartType::InteractiveScatter | ChartType::InteractiveBubble
        );
        let points = if interactive { count.min(1) } else { count };
        let largest = y_series
            .iter()
            .flat_map(|&series| (0..count).map(move |point| self.value(series + 1, point)))
            .fold(0.0, f64::max);
        // Later series are drawn first, under earlier ones (measured: blue over green).
        for (ordinal, &series) in y_series.iter().enumerate().rev() {
            let color = self.series_color(ordinal, self.chart_type.fill_field());
            for point in 0..points {
                let center =
                    Point::new(x_of(self.value(0, point)), y_of(self.value(series, point)));
                if self.chart_type.is_bubble() {
                    let size = self.value(series + 1, point);
                    if size > 0.0 && largest > 0.0 {
                        let radius =
                            BUBBLE_DIAMETER * plot.height() / 2.0 * (size / largest).sqrt() as f32;
                        canvas.draw_circle(center, radius, &paint(color));
                    }
                } else if interactive {
                    canvas.draw_circle(center, RING_RADIUS, &stroke_paint(color, RING_WIDTH));
                } else {
                    canvas.draw_circle(center, DOT_RADIUS, &paint(color));
                }
            }
        }
        self.draw_baseline(canvas);
        self.draw_value_axis_line(canvas, 0, plot.left);
        // Measured: an interactive scatter names its data set, an interactive bubble does not.
        if self.chart_type == ChartType::InteractiveScatter {
            self.draw_set_name(canvas);
        }
    }

    /// An interactive chart exports its first data set: a column (type 20) or a horizontal bar
    /// (type 21) per series, labelled with the series names, and the set's name centred beneath
    /// (measured: "April"). The value axis spans every data set the slider can show.
    fn draw_interactive(&self, canvas: &Canvas) {
        let plot = self.plot();
        let value_style = self.axis_style(true, 0);
        let value_font = value_style.font();
        let category_style = self.axis_style(false, 0);
        let category_font = category_style.font();
        let count = self.series.len();
        let maximum = self.values.iter().flatten().copied().fold(0.0, f64::max);
        let scale = self.axis_scale(maximum, 0);
        let first_value = |series: usize| self.value(series, 0);
        if self.chart_type == ChartType::InteractiveBar {
            let x_of = |value: f64| plot.left + (value / scale.top) as f32 * plot.width();
            for value in scale.values().skip(1) {
                let grid_x = x_of(value).floor() + 0.5;
                canvas.draw_line((grid_x, plot.top), (grid_x, plot.bottom), &grid_paint());
            }
            for value in scale.values() {
                let label = self.axis_label(0, value);
                let label_x = x_of(value) - shaper::measure(&label, &value_font) / 2.0;
                text::draw_line(
                    canvas,
                    &label,
                    Point::new(
                        label_x,
                        plot.bottom + below_baseline(value_style.size) - 0.5,
                    ),
                    &value_font,
                    &value_style,
                );
            }
            let slot = plot.height() / count.max(1) as f32;
            let bar_height = slot / (1.0 + self.set_gap());
            for (series, name) in self.series.iter().enumerate() {
                let center_y = plot.top + (series as f32 + 0.5) * slot;
                if !self.categories.is_empty() {
                    let bar = Rect::new(
                        x_of(0.0).ceil(),
                        (center_y - bar_height / 2.0).floor(),
                        x_of(first_value(series)),
                        (center_y + bar_height / 2.0).floor(),
                    );
                    canvas.draw_rect(bar, &paint(self.series_color(series, 12)));
                }
                let label_x = plot.left
                    - category_label_gap(category_style.size)
                    - shaper::measure(name, &category_font);
                text::draw_line(
                    canvas,
                    name,
                    Point::new(label_x, center_y + mid_baseline(category_style.size)),
                    &category_font,
                    &category_style,
                );
            }
            let axis_x = plot.left.floor() + 0.5;
            canvas.draw_line((axis_x, plot.top), (axis_x, plot.bottom), &axis_paint());
            self.draw_set_name(canvas);
            return;
        }
        let height_of = |value: f64| plot.bottom - (value / scale.top) as f32 * plot.height();
        self.draw_value_axis(canvas, scale, &height_of, &|value| {
            self.axis_label(0, value)
        });
        let slot = plot.width() / count.max(1) as f32;
        let bar_width = slot / (1.0 + self.set_gap());
        for (series, name) in self.series.iter().enumerate() {
            let center_x = plot.left + (series as f32 + 0.5) * slot;
            if !self.categories.is_empty() {
                let bar = Rect::new(
                    (center_x - bar_width / 2.0).floor(),
                    height_of(first_value(series)),
                    (center_x + bar_width / 2.0).floor(),
                    plot.bottom.floor(),
                );
                canvas.draw_rect(bar, &paint(self.series_color(series, 13)));
            }
            let label_x = center_x - shaper::measure(name, &category_font) / 2.0;
            text::draw_line(
                canvas,
                name,
                Point::new(label_x, plot.bottom + below_baseline(category_style.size)),
                &category_font,
                &category_style,
            );
        }
        self.draw_baseline(canvas);
        self.draw_set_name(canvas);
    }

    // ---------------------------------------------------------------- pie

    /// A pie shows the first value of each series as a wedge; with a single series, its values.
    fn pie_by_series(&self) -> bool {
        self.series.len() > 1 || self.categories.len() <= 1
    }

    fn pie_names(&self) -> Vec<String> {
        if self.pie_by_series() {
            self.series.clone()
        } else {
            self.categories.clone()
        }
    }

    fn pie_values(&self) -> Vec<f64> {
        if self.pie_by_series() {
            (0..self.series.len())
                .map(|series| self.value(series, 0).abs())
                .collect()
        } else {
            (0..self.categories.len())
                .map(|category| self.value(0, category).abs())
                .collect()
        }
    }

    /// The value label style: the series style's label paragraph (23) over 12pt text in `color`.
    fn value_label_style(&self, color: Color) -> TextStyle {
        let style = TextStyle {
            size: 12.0,
            color,
            ..label_style()
        };
        match self
            .series_styles
            .first()
            .and_then(|&id| self.style_int(id, 23))
        {
            Some(index) => self.chart_paragraph(Some(index), &style),
            None => style,
        }
    }

    /// Whether a series shows its values on its columns or bars (series non-style 39).
    fn shows_values(&self, series: usize) -> bool {
        self.series_property(series, 39)
            .is_some_and(|props| props.boolean(39))
    }

    fn draw_pie(&self, canvas: &Canvas) {
        let donut = self.chart_type == ChartType::Donut;
        let center = Point::new(self.frame.center_x(), self.frame.center_y());
        let mut radius = self.frame.width().min(self.frame.height()) / 2.0;
        let values = self.pie_values();
        let total: f64 = values.iter().sum();
        if total <= 0.0 {
            return;
        }
        // Exploded wedges (series non-style 63, a fraction of the radius) pull out from the centre;
        // the pie shrinks so they stay inside the frame.
        let explode = |index: usize| {
            if self.pie_by_series() {
                self.series_property(index, 63)
                    .map_or(0.0, |props| props.float(63))
            } else {
                0.0
            }
        };
        let largest = (0..values.len()).map(explode).fold(0.0, f32::max);
        if !donut && largest > 0.0 {
            radius /= 1.0 + largest;
        }
        let names = self.pie_names();
        let style = self.value_label_style(if donut { Color::BLACK } else { Color::WHITE });
        let font = style.font();
        let cap_height = font.font.metrics().1.cap_height;
        let mut labels: Vec<PieLabel> = Vec::with_capacity(values.len());
        let mut start = -90.0f32;
        for (index, &value) in values.iter().enumerate() {
            let sweep = (value / total * 360.0) as f32;
            let middle = f64::from(start + sweep / 2.0).to_radians();
            let offset = if donut { 0.0 } else { explode(index) * radius };
            let wedge_center = Point::new(
                center.x + offset * middle.cos() as f32,
                center.y + offset * middle.sin() as f32,
            );
            let oval = Rect::new(
                wedge_center.x - radius,
                wedge_center.y - radius,
                wedge_center.x + radius,
                wedge_center.y + radius,
            );
            let mut path = Path::new();
            if donut {
                let inner = radius * 0.75;
                path.arc_to(oval, start, sweep, true);
                path.arc_to(
                    Rect::new(
                        wedge_center.x - inner,
                        wedge_center.y - inner,
                        wedge_center.x + inner,
                        wedge_center.y + inner,
                    ),
                    start + sweep,
                    -sweep,
                    false,
                );
            } else {
                path.move_to(wedge_center);
                path.arc_to(oval, start, sweep, false);
            }
            path.close();
            canvas.draw_path(&path, &paint(self.series_color(index, 17)));
            // The series name shows only when its pie label setting says so (non-style 31).
            let setting = if self.pie_by_series() {
                self.series_property(index, 31)
            } else {
                self.series_property(0, 31)
            };
            let name = if setting.is_some_and(|props| props.boolean(31)) {
                names.get(index).cloned().unwrap_or_default()
            } else {
                String::new()
            };
            // .NET's Math.Round: ties to even.
            let percent = format!("{}%", (value / total * 100.0).round_ties_even());
            let label_radius = if donut {
                radius
            } else {
                // A label overflowing its wedge's arc (measured across the radius) moves out
                // toward the rim (Keynote: a 25° wedge's 50pt label at 0.81, a 4° wedge's at 0.84).
                // ponytail: two-point fit; a per-wedge fit test if more pies disagree.
                let normal = radius * 0.668;
                let across = shaper::measure(&percent, &font) * middle.sin().abs() as f32
                    + cap_height * middle.cos().abs() as f32;
                if across > 1.1 * normal * sweep.to_radians() {
                    radius * 0.82
                } else {
                    normal
                }
            };
            labels.push(PieLabel {
                percent,
                name,
                at: Point::new(
                    wedge_center.x + label_radius * middle.cos() as f32,
                    wedge_center.y + label_radius * middle.sin() as f32,
                ),
                center: wedge_center,
            });
            start += sweep;
        }
        for label in labels {
            if donut {
                self.draw_donut_label(canvas, &label, radius, &style);
            } else {
                let x = label.at.x - shaper::measure(&label.percent, &font) / 2.0;
                text::draw_line(
                    canvas,
                    &label.percent,
                    Point::new(x, label.at.y + centering_baseline(&font)),
                    &font,
                    &style,
                );
            }
        }
    }

    /// Donut labels sit in columns left/right of the ring with elbow leader lines (measured), in
    /// black: the series name over the percentage.
    fn draw_donut_label(&self, canvas: &Canvas, label: &PieLabel, radius: f32, style: &TextStyle) {
        let style = TextStyle {
            color: Color::BLACK,
            ..style.clone()
        };
        let font = style.font();
        let leader = stroke_paint(Color::BLACK, 0.5);
        let direction = label.at - label.center;
        let length = direction.length();
        let unit = Point::new(direction.x / length, direction.y / length);
        let ring = label.center + Point::new(unit.x * radius, unit.y * radius);
        let elbow = label.center + Point::new(unit.x * (radius + 8.0), unit.y * (radius + 8.0));
        let right_side = direction.x >= 0.0;
        let column_x = if right_side {
            label.center.x + radius + 23.0
        } else {
            label.center.x - radius - 23.0
        };
        canvas.draw_line(ring, elbow, &leader);
        canvas.draw_line(elbow, (column_x, elbow.y), &leader);
        let (percent_width, name_width) = (
            shaper::measure(&label.percent, &font),
            shaper::measure(&label.name, &font),
        );
        let baseline = elbow.y + centering_baseline(&font);
        let text_x = |width: f32| {
            if right_side {
                column_x + 7.0
            } else {
                column_x - 7.0 - width
            }
        };
        if label.name.is_empty() {
            text::draw_line(
                canvas,
                &label.percent,
                Point::new(text_x(percent_width), baseline),
                &font,
                &style,
            );
            return;
        }
        let half_line = text::line_height(&style) / 2.0;
        text::draw_line(
            canvas,
            &label.name,
            Point::new(text_x(name_width), baseline - half_line),
            &font,
            &style,
        );
        text::draw_line(
            canvas,
            &label.percent,
            Point::new(text_x(percent_width), baseline + half_line),
            &font,
            &style,
        );
    }
}

/// A pie wedge's label.
/// The baseline offset that centres a line of `font` vertically on a point: half of
/// ascent - descent (4.3pt for 12pt Helvetica Neue, 18.5pt at 50pt).
fn centering_baseline(font: &TaggedFont) -> f32 {
    // Skia reports the ascent as a negative distance above the baseline.
    let (_, metrics) = font.font.metrics();
    (-metrics.ascent - metrics.descent) / 2.0
}

struct PieLabel {
    percent: String,
    name: String,
    at: Point,
    center: Point,
}

/// A line series' data point: a white circle ringed in the series colour at the line's width.
fn draw_marker(canvas: &Canvas, center: Point, color: Color, width: f32) {
    canvas.draw_circle(center, 3.5, &paint(Color::WHITE));
    canvas.draw_circle(center, 3.5, &stroke_paint(color, width));
}

fn polyline(points: &[Point]) -> Path {
    let mut path = Path::new();
    for (index, point) in points.iter().enumerate() {
        if index == 0 {
            path.move_to(*point);
        } else {
            path.line_to(*point);
        }
    }
    path
}

fn line_paint(color: Color) -> Paint {
    let mut line = stroke_paint(color, LINE_WIDTH);
    line.set_stroke_join(PaintJoin::Round);
    line
}

/// Text drawn rotated about its origin.
fn rotated_text(canvas: &Canvas, label: &str, origin: Point, degrees: f32, style: &TextStyle) {
    canvas.save();
    canvas.translate(origin);
    canvas.rotate(degrees, None);
    text::draw_line(canvas, label, Point::new(0.0, 0.0), &style.font(), style);
    canvas.restore();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_steps_match_numbers() {
        assert_eq!(nice_step(457_485.0 / 4.0, 0.25), 125_000.0);
        assert_eq!(nice_step(131_280.0 / 4.0, 0.25), 35_000.0);
        assert_eq!(nice_step(82.0 / 4.0, 0.25), 22.5);
        assert_eq!(nice_step(3.22 / 4.0, 0.25), 1.0);
        // Scatter X axes (measured: maxima 15, 12 and 40 over four steps).
        assert_eq!(nice_step(15.0 / 4.0, 1.0), 4.0);
        assert_eq!(nice_step(12.0 / 4.0, 1.0), 3.0);
        assert_eq!(nice_step(40.0 / 4.0, 1.0), 10.0);
    }

    #[test]
    fn scale_ticks() {
        let scale = Scale {
            top: 100.0,
            step: 25.0,
        };
        assert_eq!(
            scale.values().collect::<Vec<_>>(),
            vec![0.0, 25.0, 50.0, 75.0, 100.0]
        );
    }

    #[test]
    fn chart_types() {
        assert!(ChartType::from_stored(25).is_pie());
        assert!(ChartType::from_stored(7).horizontal() && ChartType::from_stored(7).stacked());
        assert_eq!(ChartType::from_stored(4).fill_field(), 11);
    }

    #[test]
    fn three_d_types_draw_flat_and_mixed_is_combo() {
        let flat: Vec<ChartType> = (12..=19).map(ChartType::from_stored).collect();
        let two_d: Vec<ChartType> = (1..=8).map(ChartType::from_stored).collect();
        assert_eq!(flat, two_d);
        assert!(ChartType::from_stored(10).is_combo() && ChartType::from_stored(11).is_combo());
        assert!(!ChartType::from_stored(1).is_combo());
    }

    #[test]
    fn scatter_bubble_and_radar_types() {
        let xy: Vec<bool> = [9, 22, 23, 24]
            .into_iter()
            .map(|stored| ChartType::from_stored(stored).is_xy())
            .collect();
        assert_eq!(xy, vec![true; 4]);
        assert!(ChartType::from_stored(22).is_bubble() && ChartType::from_stored(24).is_bubble());
        assert!(!ChartType::from_stored(9).is_bubble());
        assert_eq!(ChartType::from_stored(27), ChartType::Radar);
        assert!(!ChartType::Radar.is_xy());
    }
}
