//! Pivot tables (TST.PivotOwnerArchive) are recomputed from their source table, as Numbers does
//! when it exports: the stored cells can be stale (written by another tool, or before a refresh).
//! Handles one row grouping, one column grouping and one aggregate (the common layout: a header row
//! of column keys, a header row naming the row field and the aggregate, a row per row key sorted
//! ascending, and grand totals on the right and at the bottom).

use crate::iwa::{IwaDocument, Proto};
use crate::numbers_table::{display, Cell, CellKind, CellPosition, NumbersTable, Strips};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

/// How a pivot combines the values of a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Aggregate {
    Count,
    Sum,
    Average,
    Minimum,
    Maximum,
}

impl Aggregate {
    fn from_stored(value: i32) -> Aggregate {
        match value {
            1 => Aggregate::Count,
            3 => Aggregate::Average,
            4 => Aggregate::Minimum,
            5 => Aggregate::Maximum,
            _ => Aggregate::Sum,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Aggregate::Count => "Count",
            Aggregate::Sum => "Sum",
            Aggregate::Average => "Average",
            Aggregate::Minimum => "Min",
            Aggregate::Maximum => "Max",
        }
    }

    fn apply(self, values: &[f64]) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        match self {
            Aggregate::Count => values.len() as f64,
            Aggregate::Sum => values.iter().sum(),
            Aggregate::Average => values.iter().sum::<f64>() / values.len() as f64,
            Aggregate::Minimum => values.iter().copied().fold(f64::INFINITY, f64::min),
            Aggregate::Maximum => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        }
    }
}

/// A group key's sort value: numbers sort numerically, everything else by its text.
#[derive(Debug, Clone, PartialEq)]
enum SortKey {
    Number(f64),
    Text(String),
}

fn compare_keys(left: &(String, SortKey), right: &(String, SortKey)) -> Ordering {
    match (&left.1, &right.1) {
        (SortKey::Number(first), SortKey::Number(second)) => first.total_cmp(second),
        _ => left.0.to_lowercase().cmp(&right.0.to_lowercase()),
    }
}

/// Distinct group keys in first-seen order, with their sort values.
#[derive(Debug, Default)]
struct Keys {
    keys: Vec<(String, SortKey)>,
}

impl Keys {
    fn add(&mut self, key: String, sort: SortKey) {
        if !self.keys.iter().any(|(existing, _)| *existing == key) {
            self.keys.push((key, sort));
        }
    }

    fn sorted(mut self) -> Vec<String> {
        self.keys.sort_by(compare_keys);
        self.keys.into_iter().map(|(key, _)| key).collect()
    }
}

/// The pivot's fields: source columns of the row and column groupings and the aggregated values.
#[derive(Debug, Clone, Copy)]
struct Fields {
    row: usize,
    column: usize,
    value: usize,
    aggregate: Aggregate,
}

/// Recomputes a pivot table's cells from its source table (unchanged when the layout is not the
/// supported one or the source is missing).
///
/// # Arguments
///
/// * `table` - The pivot table, as loaded
pub fn rebuild(table: &mut NumbersTable) {
    let doc = Rc::clone(&table.doc);
    let Some(owner) = doc
        .body(table.model.reference(85))
        .filter(|owner| !owner.boolean(7))
    else {
        return;
    };
    let (row_groups, column_groups, aggregates) = (
        owner.message(3).messages(1),
        owner.message(4).messages(1),
        owner.message(5).messages(1),
    );
    if row_groups.len() != 1 || column_groups.len() != 1 || aggregates.len() != 1 {
        return;
    }
    let Some(source) = source_table(&doc, &owner) else {
        return;
    };
    let column_of = |uuid: Proto| column_index(&source, uuid.uint(1), uuid.uint(2));
    let (Some(row), Some(column), Some(value)) = (
        column_of(row_groups[0].message(1)),
        column_of(column_groups[0].message(1)),
        column_of(aggregates[0].message(1)),
    ) else {
        return;
    };
    let fields = Fields {
        row,
        column,
        value,
        aggregate: Aggregate::from_stored(aggregates[0].int(3)),
    };
    let (row_totals, column_totals) = (!owner.boolean(11), !owner.boolean(13));
    let Some(grouped) = Grouped::collect(&source, fields) else {
        return;
    };
    let row_count = 2 + grouped.row_keys.len() + usize::from(row_totals);
    let column_count = 1 + grouped.column_keys.len() + usize::from(column_totals);
    let mut builder = Builder {
        stored: std::mem::take(&mut table.cells),
        stored_rows: table.rows.max(1),
        stored_columns: table.columns.max(1),
        rows: row_count,
        columns: column_count,
        row_totals,
        column_totals,
        cells: HashMap::new(),
        currency: source
            .cell((source.header_rows, value))
            .is_some_and(|cell| cell.kind == CellKind::Currency)
            && fields.aggregate != Aggregate::Count,
    };
    builder.fill(&source, fields, &grouped);
    let default_width = table.model.double(17) as f32;
    let default_height = table.model.double(16) as f32;
    // Added columns repeat the stored pivot's last column when it had several; added rows repeat
    // its last row.
    let resize = |sizes: &[f32], count: usize, default: f32, min_stored: usize| -> Vec<f32> {
        let repeated = (sizes.len() >= min_stored)
            .then(|| sizes.last().copied())
            .flatten();
        (0..count)
            .map(|index| sizes.get(index).copied().or(repeated).unwrap_or(default))
            .collect()
    };
    let mut column_strips = Strips::default();
    column_strips.sizes = resize(&table.column_strips.sizes, column_count, default_width, 2);
    let mut row_strips = Strips::default();
    row_strips.sizes = resize(&table.row_strips.sizes, row_count, default_height, 1);
    row_strips.explicit = (0..row_count)
        .map(|index| {
            table
                .row_strips
                .explicit
                .get(index)
                .copied()
                .unwrap_or(false)
        })
        .collect();
    for strips in [&mut column_strips, &mut row_strips] {
        let count = strips.sizes.len();
        strips.hidden = vec![false; count];
        strips.explicit.resize(count, false);
        strips.cell_style = vec![0; count];
        strips.text_style = vec![0; count];
    }
    table.column_strips = column_strips;
    table.row_strips = row_strips;
    // The stored fitted heights describe the stale layout: rows are measured afresh.
    table.fitted_row_heights = None;
    table.rows = row_count;
    table.columns = column_count;
    table.header_rows = 2;
    table.header_columns = 1;
    table.footer_rows = 0;
    table.cells = builder
        .cells
        .into_iter()
        .map(|(position, cell)| (position, RefCell::new(cell)))
        .collect();
    table.merges.clear();
}

/// The source's values grouped by (row key, column key).
struct Grouped {
    row_keys: Vec<String>,
    column_keys: Vec<String>,
    groups: HashMap<(String, String), Vec<f64>>,
}

impl Grouped {
    fn collect(source: &NumbersTable, fields: Fields) -> Option<Grouped> {
        let key = |position: CellPosition| display(source, position).unwrap_or_default();
        let sort_key = |position: CellPosition| match source.cell(position) {
            Some(cell) if cell.kind.is_numeric() => SortKey::Number(cell.number),
            _ => SortKey::Text(key(position)),
        };
        let (mut row_keys, mut column_keys) = (Keys::default(), Keys::default());
        let mut groups: HashMap<(String, String), Vec<f64>> = HashMap::new();
        for row in source.header_rows..source.rows.saturating_sub(source.footer_rows) {
            let (row_cell, column_cell) = ((row, fields.row), (row, fields.column));
            if source.cell(row_cell).is_none() && source.cell(column_cell).is_none() {
                continue;
            }
            row_keys.add(key(row_cell), sort_key(row_cell));
            column_keys.add(key(column_cell), sort_key(column_cell));
            let group = groups.entry((key(row_cell), key(column_cell))).or_default();
            match source.cell((row, fields.value)) {
                Some(value)
                    if matches!(
                        value.kind,
                        CellKind::Number | CellKind::Currency | CellKind::Duration
                    ) =>
                {
                    group.push(value.number)
                }
                Some(_) if fields.aggregate == Aggregate::Count => group.push(0.0),
                _ => {}
            }
        }
        let (row_keys, column_keys) = (row_keys.sorted(), column_keys.sorted());
        (!row_keys.is_empty() && !column_keys.is_empty()).then_some(Grouped {
            row_keys,
            column_keys,
            groups,
        })
    }

    fn values(&self, row_key: &str, column_key: &str) -> &[f64] {
        self.groups
            .get(&(row_key.to_owned(), column_key.to_owned()))
            .map_or(&[], Vec::as_slice)
    }
}

/// Writes the recomputed cells, styled like the stored pivot's cells in the same role.
struct Builder {
    stored: HashMap<CellPosition, RefCell<Cell>>,
    stored_rows: usize,
    stored_columns: usize,
    rows: usize,
    columns: usize,
    row_totals: bool,
    column_totals: bool,
    cells: HashMap<CellPosition, Cell>,
    currency: bool,
}

impl Builder {
    /// A new cell taking styles and formats from the stored cell in the same role.
    fn like(&self, (row, column): CellPosition, kind: CellKind) -> Cell {
        let mut stored_row = row.min(self.stored_rows - 1);
        let mut stored_column = column.min(self.stored_columns - 1);
        if row == self.rows - 1 && self.row_totals {
            stored_row = self.stored_rows - 1;
        }
        if column == self.columns - 1 && self.column_totals {
            stored_column = self.stored_columns - 1;
        }
        let body = (2.min(self.stored_rows - 1), 1.min(self.stored_columns - 1));
        let template = self.stored.get(&(stored_row, stored_column)).or_else(|| {
            (row >= 2 && column >= 1)
                .then(|| self.stored.get(&body))
                .flatten()
        });
        let template = template
            .map(|cell| cell.borrow().clone())
            .unwrap_or_default();
        Cell {
            kind,
            cell_style_key: template.cell_style_key,
            text_style_key: template.text_style_key,
            number_format_key: template.number_format_key,
            currency_format_key: template.currency_format_key,
            pre_bnc: template.pre_bnc,
            ..Cell::default()
        }
    }

    fn text(&mut self, position: CellPosition, text: String) {
        let cell = Cell {
            text: Some(text),
            ..self.like(position, CellKind::Text)
        };
        self.cells.insert(position, cell);
    }

    fn number(&mut self, position: CellPosition, value: f64) {
        let kind = if self.currency {
            CellKind::Currency
        } else {
            CellKind::Number
        };
        let cell = Cell {
            number: value,
            ..self.like(position, kind)
        };
        self.cells.insert(position, cell);
    }

    fn fill(&mut self, source: &NumbersTable, fields: Fields, grouped: &Grouped) {
        let header = |column: usize| display(source, (0, column)).unwrap_or_default();
        let aggregate = |values: &[f64]| fields.aggregate.apply(values);
        let last_column = self.columns - 1;
        self.text((0, 0), header(fields.column));
        for (index, key) in grouped.column_keys.iter().enumerate() {
            self.text((0, 1 + index), key.clone());
        }
        if self.column_totals {
            self.text((0, last_column), "Grand Total".into());
        }
        self.text((1, 0), header(fields.row));
        self.text(
            (1, 1),
            format!("{} ({})", header(fields.value), fields.aggregate.name()),
        );
        for (row_index, row_key) in grouped.row_keys.iter().enumerate() {
            let row = 2 + row_index;
            self.text((row, 0), row_key.clone());
            for (column_index, column_key) in grouped.column_keys.iter().enumerate() {
                let values = grouped.values(row_key, column_key);
                if !values.is_empty() {
                    self.number((row, 1 + column_index), aggregate(values));
                }
            }
            if self.column_totals {
                let all: Vec<f64> = grouped
                    .column_keys
                    .iter()
                    .flat_map(|column_key| grouped.values(row_key, column_key).iter().copied())
                    .collect();
                self.number((row, last_column), aggregate(&all));
            }
        }
        if self.row_totals {
            let last_row = self.rows - 1;
            self.text((last_row, 0), "Grand Total".into());
            for (column_index, column_key) in grouped.column_keys.iter().enumerate() {
                let all: Vec<f64> = grouped
                    .row_keys
                    .iter()
                    .flat_map(|row_key| grouped.values(row_key, column_key).iter().copied())
                    .collect();
                self.number((last_row, 1 + column_index), aggregate(&all));
            }
            if self.column_totals {
                let all: Vec<f64> = grouped.groups.values().flatten().copied().collect();
                self.number((last_row, last_column), aggregate(&all));
            }
        }
    }
}

/// The pivot's source table: by table UUID (8), else by name (12).
fn source_table(doc: &Rc<IwaDocument>, owner: &Proto) -> Option<NumbersTable> {
    let uuid = owner.message(8);
    let mut by_name = None;
    for object in doc.of_type(6000) {
        let Some(model) = doc
            .body(object.body.reference(2))
            .filter(|model| model.reference(85) == 0)
        else {
            continue;
        };
        if model
            .string(1)
            .is_some_and(|table_id| uuid_matches(&table_id, uuid.uint(1), uuid.uint(2)))
        {
            return NumbersTable::load(doc, &object.body).ok();
        }
        if by_name.is_none() && model.string(8) == owner.string(12) {
            by_name = Some(object.body.clone());
        }
    }
    by_name.and_then(|info| NumbersTable::load(doc, &info).ok())
}

/// A table id (hex with dashes) matches a two-word UUID in any byte order.
fn uuid_matches(table_id: &str, lower: u64, upper: u64) -> bool {
    let hex = table_id.replace('-', "").to_uppercase();
    let word = |value: u64, reversed: bool| {
        let bytes = if reversed {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        };
        bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>()
    };
    [
        (lower, upper, false),
        (upper, lower, true),
        (lower, upper, true),
        (upper, lower, false),
    ]
    .iter()
    .any(|&(first, second, reversed)| hex == word(first, reversed) + &word(second, reversed))
}

/// The column with a column UUID (base_column_row_uids).
fn column_index(table: &NumbersTable, lower: u64, upper: u64) -> Option<usize> {
    table.uid_indexes().0.get(&(lower, upper)).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates() {
        let values = [1.0, 4.0, 7.0];
        assert_eq!(Aggregate::Count.apply(&values), 3.0);
        assert_eq!(Aggregate::Average.apply(&values), 4.0);
        assert_eq!(Aggregate::Maximum.apply(&values), 7.0);
        assert_eq!(Aggregate::Sum.apply(&[]), 0.0);
    }

    #[test]
    fn keys_sort_numbers_then_text() {
        let mut keys = Keys::default();
        keys.add("10".into(), SortKey::Number(10.0));
        keys.add("9".into(), SortKey::Number(9.0));
        keys.add("9".into(), SortKey::Number(9.0));
        assert_eq!(keys.sorted(), vec!["9", "10"]);
        let mut words = Keys::default();
        words.add("b".into(), SortKey::Text("b".into()));
        words.add("A".into(), SortKey::Text("A".into()));
        assert_eq!(words.sorted(), vec!["A", "b"]);
    }

    #[test]
    fn uuid_byte_orders() {
        let (lower, upper) = (0x0102_0304_0506_0708u64, 0x1112_1314_1516_1718u64);
        assert!(uuid_matches(
            "08070605-0403-0201-1817-161514131211",
            lower,
            upper
        ));
        assert!(uuid_matches(
            "1112131415161718-0102030405060708",
            lower,
            upper
        ));
        assert!(!uuid_matches("00", lower, upper));
    }
}
