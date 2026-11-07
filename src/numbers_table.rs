//! A Numbers/IWA table (TST.TableInfoArchive + TST.TableModelArchive): geometry, header layout,
//! styles and decoded cells.

use crate::bytes::{le_u16, le_u32, le_u64};
use crate::cell_format;
use crate::error::{Error, Result};
use crate::iwa::{IwaDocument, Proto};
use crate::pivot;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// What a cell holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellKind {
    /// No value (style only).
    #[default]
    Empty,
    /// A number.
    Number,
    /// Plain text.
    Text,
    /// A date (seconds since 2001-01-01).
    Date,
    /// A boolean.
    Bool,
    /// A duration in seconds.
    Duration,
    /// A formula error (exported blank).
    Error,
    /// Rich text in a TSWP storage.
    RichText,
    /// A currency amount.
    Currency,
}

impl CellKind {
    /// The kind for a stored cell type byte.
    fn from_stored(cell_type: u8) -> CellKind {
        match cell_type {
            2 => CellKind::Number,
            3 => CellKind::Text,
            5 => CellKind::Date,
            6 => CellKind::Bool,
            7 => CellKind::Duration,
            8 => CellKind::Error,
            9 => CellKind::RichText,
            10 => CellKind::Currency,
            _ => CellKind::Empty,
        }
    }

    /// True for kinds whose value is a number (right-aligned by default).
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            CellKind::Number | CellKind::Currency | CellKind::Date | CellKind::Duration
        )
    }
}

/// One decoded cell.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cell {
    /// What the cell holds.
    pub kind: CellKind,
    /// Numeric value (numbers, dates, durations, booleans).
    pub number: f64,
    /// Text value.
    pub text: Option<String>,
    /// TSWP storage of a rich-text cell (resolved from the rich-text table).
    pub rich_text_storage: u64,
    /// Key into the string table.
    pub string_key: u32,
    /// Key of the cell style in the style table.
    pub cell_style_key: u32,
    /// Key of the text style in the style table.
    pub text_style_key: u32,
    /// Key of the conditional style set.
    pub conditional_style_key: u32,
    /// The conditional rule Numbers last applied (cached), if recorded.
    pub conditional_rule_applied: Option<u32>,
    /// Key into the formula table.
    pub formula_key: u32,
    /// Number format key.
    pub number_format_key: u32,
    /// Currency format key.
    pub currency_format_key: u32,
    /// Date format key.
    pub date_format_key: u32,
    /// Duration format key.
    pub duration_format_key: u32,
    /// Text format key.
    pub text_format_key: u32,
    /// Boolean format key.
    pub bool_format_key: u32,
    /// Stored in the pre-BNC (Numbers 2013-2019) layout.
    pub pre_bnc: bool,
}

/// Row or column UID → index (base_column_row_uids).
pub type UidIndex = HashMap<(u64, u64), usize>;

/// (row, column) of a cell.
pub type CellPosition = (usize, usize);

/// An inclusive rectangle of cells (a merged range).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRange {
    /// Top row.
    pub first_row: usize,
    /// Left column.
    pub first_column: usize,
    /// Bottom row (inclusive).
    pub last_row: usize,
    /// Right column (inclusive).
    pub last_column: usize,
}

impl CellRange {
    fn spanning(row_a: i64, column_a: i64, row_b: i64, column_b: i64) -> Option<CellRange> {
        let index = |value: i64| usize::try_from(value).ok();
        Some(CellRange {
            first_row: index(row_a.min(row_b))?,
            first_column: index(column_a.min(column_b))?,
            last_row: index(row_a.max(row_b))?,
            last_column: index(column_a.max(column_b))?,
        })
    }
}

/// Per-row or per-column layout read from the header storage buckets.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Strips {
    /// Height of each row or width of each column.
    pub sizes: Vec<f32>,
    /// Hidden by the user or by a filter.
    pub hidden: Vec<bool>,
    /// The size was set explicitly (rows only).
    pub explicit: Vec<bool>,
    /// Default cell style of the strip (0: none).
    pub cell_style: Vec<u64>,
    /// Default text style of the strip (0: none).
    pub text_style: Vec<u64>,
}

impl Strips {
    fn new(count: usize, size: f32) -> Strips {
        Strips {
            sizes: vec![size; count],
            hidden: vec![false; count],
            explicit: vec![false; count],
            cell_style: vec![0; count],
            text_style: vec![0; count],
        }
    }

    /// The strips in a new order (`None`: an inserted strip of `default_size`).
    fn reordered(&self, order: &[Option<usize>], default_size: f32) -> Strips {
        let pick = |index: &Option<usize>| index.filter(|&index| index < self.sizes.len());
        Strips {
            sizes: order
                .iter()
                .map(|index| pick(index).map_or(default_size, |index| self.sizes[index]))
                .collect(),
            hidden: order
                .iter()
                .map(|index| pick(index).is_some_and(|index| self.hidden[index]))
                .collect(),
            explicit: order
                .iter()
                .map(|index| pick(index).is_some_and(|index| self.explicit[index]))
                .collect(),
            cell_style: order
                .iter()
                .map(|index| pick(index).map_or(0, |index| self.cell_style[index]))
                .collect(),
            text_style: order
                .iter()
                .map(|index| pick(index).map_or(0, |index| self.text_style[index]))
                .collect(),
        }
    }

    /// The strips with one inserted in front.
    fn with_leading(&self, size: f32) -> Strips {
        let mut order = vec![None];
        order.extend((0..self.sizes.len()).map(Some));
        self.reordered(&order, size)
    }
}

/// Category (group-by) rows Numbers adds in front of grouped data.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Categories {
    /// Category row → group label.
    pub labels: HashMap<usize, String>,
    /// Nesting level (0 = outermost) of each category row.
    pub levels: HashMap<usize, usize>,
    /// Category rows that are summary label rows ("Kind:" … "Count (all):").
    pub summary_label_rows: HashSet<usize>,
    /// Summary values (or, in label rows, aggregate names) by position.
    pub values: HashMap<CellPosition, String>,
}

/// A decoded table.
#[derive(Debug)]
pub struct NumbersTable {
    /// The document the table belongs to.
    pub doc: Rc<IwaDocument>,
    /// TST.TableInfoArchive.
    pub info: Proto,
    /// TST.TableModelArchive.
    pub model: Proto,
    /// Number of rows shown.
    pub rows: usize,
    /// Number of columns shown.
    pub columns: usize,
    /// Header rows at the top.
    pub header_rows: usize,
    /// Header columns at the left.
    pub header_columns: usize,
    /// Footer rows at the bottom.
    pub footer_rows: usize,
    /// Row heights, hidden flags and default styles.
    pub row_strips: Strips,
    /// Column widths, hidden flags and default styles.
    pub column_strips: Strips,
    /// Numbers' cached fitted (content-only) row heights, one per row.
    pub fitted_row_heights: Option<Vec<f32>>,
    /// Cells by position; a `RefCell` because formula evaluation fills in results later.
    pub cells: HashMap<CellPosition, RefCell<Cell>>,
    /// Merged ranges.
    pub merges: Vec<CellRange>,
    /// The table's name.
    pub name: String,
    /// The name is shown above the table.
    pub name_enabled: bool,
    /// Category rows, when the table is grouped.
    pub categories: Categories,
    /// A 50pt category label column was added in front.
    pub label_column: bool,
    /// Decoded TST.TableDataList entries by list id, then key.
    lists: RefCell<HashMap<u64, Rc<HashMap<u32, Proto>>>>,
}

impl NumbersTable {
    /// Loads a table.
    ///
    /// # Arguments
    ///
    /// * `doc` - The document
    /// * `info` - The TST.TableInfoArchive
    ///
    /// # Returns
    ///
    /// The table with its cells decoded, categories applied and pivot tables recomputed.
    ///
    /// # Errors
    ///
    /// Returns `Error::Missing` when the table model is absent.
    pub fn load(doc: &Rc<IwaDocument>, info: &Proto) -> Result<NumbersTable> {
        let model = doc
            .body(info.reference(2))
            .ok_or(Error::Missing("table model"))?;
        let mut rows = usize::try_from(model.int(6)).unwrap_or(0);
        let mut columns = usize::try_from(model.int(7)).unwrap_or(0);
        // The column/row UID map (base_column_row_uids 46) lists the strips Numbers knows about; a
        // model claiming more (as written by some tools) is shown only that far (measured: a 45x20
        // model sharing a 12-row, 8-column map exports as 12x8).
        if let Some(uids) = doc.body(model.reference(46)) {
            let (uid_columns, uid_rows) = (uids.messages(1).len(), uids.messages(4).len());
            if uid_columns > 0 && uid_columns < columns {
                columns = uid_columns;
            }
            if uid_rows > 0 && uid_rows < rows {
                rows = uid_rows;
            }
        }
        let store = model.message(4);
        let mut row_strips = Strips::new(rows, model.double(16) as f32);
        let mut column_strips = Strips::new(columns, model.double(17) as f32);
        // Row headers: DataStore.rowHeaders (inline HeaderStorage) → buckets; columns: a referenced
        // HeaderStorage (or its buckets).
        apply_headers(doc, &store.message(1).references(2), &mut row_strips, true);
        let column_storage = store.reference(2);
        if let Some(object) = doc.get(column_storage) {
            let buckets = if object.typ == 6006 {
                vec![column_storage]
            } else {
                object.body.references(2)
            };
            apply_headers(doc, &buckets, &mut column_strips, false);
        }
        let mut table = NumbersTable {
            doc: Rc::clone(doc),
            info: info.clone(),
            name: model.string(8).unwrap_or_default(),
            name_enabled: model.boolean(22),
            header_rows: usize::try_from(model.int(9)).unwrap_or(0),
            header_columns: usize::try_from(model.int(10)).unwrap_or(0),
            footer_rows: usize::try_from(model.int(11)).unwrap_or(0),
            model,
            rows,
            columns,
            row_strips,
            column_strips,
            fitted_row_heights: None,
            cells: HashMap::new(),
            merges: Vec::new(),
            categories: Categories::default(),
            label_column: false,
            lists: RefCell::new(HashMap::new()),
        };
        table.apply_hidden_states();
        // Fitted (auto) row heights cached by Numbers' layout engine
        // (layout_engine.width_height_cache), one entry per row.
        let fits: Vec<f32> = info
            .message(14)
            .message(1)
            .messages(1)
            .iter()
            .map(|entry| entry.float(2))
            .collect();
        if !fits.is_empty() {
            table.fitted_row_heights = Some(fits);
        }
        table.load_cells(&store);
        table.load_merges(&store);
        table.apply_categories();
        if table.model.reference(85) != 0 {
            pivot::rebuild(&mut table);
        }
        Ok(table)
    }

    /// The cell at a position (borrowed; `None` when empty).
    pub fn cell(&self, position: CellPosition) -> Option<std::cell::Ref<'_, Cell>> {
        self.cells.get(&position).map(RefCell::borrow)
    }

    /// Merged ranges: legacy CellRange lists and modern merge-owner formulas.
    fn load_merges(&mut self, store: &Proto) {
        if let Some(legacy) = self.doc.body(store.reference(13)) {
            for range in legacy.messages(1) {
                // TST.CellRange { origin = 1 { row 1, column 2 }, size = 2 { columns 1, rows 2 } }
                let (origin, size) = (range.message(1), range.message(2));
                let (row, column) = (i64::from(origin.int(1)), i64::from(origin.int(2)));
                let last_row = row + i64::from(size.int(2)) - 1;
                let last_column = column + i64::from(size.int(1)) - 1;
                self.merges
                    .extend(CellRange::spanning(row, column, last_row, last_column));
            }
        }
        // merge_owner.formula_store holds one formula per merged range.
        for pair in self.model.message(47).message(2).messages(3) {
            let nodes = pair.message(2).message(1).messages(1);
            let types: Vec<i32> = nodes.iter().map(|node| node.int(1)).collect();
            if types.starts_with(&[36, 36, 29]) {
                // Older files: CELL_REFERENCE a, CELL_REFERENCE b, COLON (column 26, row 27, sint32).
                let coordinate = |node: &Proto, field: i32| zigzag(node.message(field).uint(1));
                let (column_a, row_a) = (coordinate(&nodes[0], 26), coordinate(&nodes[0], 27));
                let (column_b, row_b) = (coordinate(&nodes[1], 26), coordinate(&nodes[1], 27));
                self.merges
                    .extend(CellRange::spanning(row_a, column_a, row_b, column_b));
            } else if types.first() == Some(&67) {
                // COLON_TRACT: column range 3, row range 4 (begin 1, end 2).
                let tract = nodes[0].message(40);
                let span = |range: Proto| {
                    let begin = i64::from(range.int(1));
                    (
                        begin,
                        if range.has(2) {
                            i64::from(range.int(2))
                        } else {
                            begin
                        },
                    )
                };
                let ((first_column, last_column), (first_row, last_row)) =
                    (span(tract.message(3)), span(tract.message(4)));
                self.merges.extend(CellRange::spanning(
                    first_row,
                    first_column,
                    last_row,
                    last_column,
                ));
            }
        }
    }

    /// UID → index maps of base_column_row_uids (46): (columns, rows).
    pub fn uid_indexes(&self) -> (UidIndex, UidIndex) {
        let Some(map) = self.doc.body(self.model.reference(46)) else {
            return (HashMap::new(), HashMap::new());
        };
        let index = |uid_field: i32, index_field: i32| {
            let uids = map.messages(uid_field);
            let indexes = map.uints(index_field);
            uids.iter()
                .zip(indexes)
                .map(|(uid, index)| ((uid.uint(1), uid.uint(2)), index as usize))
                .collect()
        };
        (index(1, 2), index(4, 5))
    }

    /// Rows/columns hidden by the user or by a filter are listed by UID in hidden_states_owner (70).
    /// The stored "filtered" flags are what Numbers exports, even when the filter set is marked
    /// disabled (measured: demo08 hides them).
    fn apply_hidden_states(&mut self) {
        let (column_index, row_index) = self.uid_indexes();
        let hide = |extent: &Proto, index: &HashMap<(u64, u64), usize>, hidden: &mut Vec<bool>| {
            for state in extent.messages(2) {
                let uid = state.message(1);
                let Some(&position) = index.get(&(uid.uint(1), uid.uint(2))) else {
                    continue;
                };
                if position < hidden.len()
                    && (state.boolean(2) || state.boolean(3) || state.boolean(4))
                {
                    hidden[position] = true;
                }
            }
        };
        for states in self.model.message(70).messages(2) {
            hide(&states.message(3), &row_index, &mut self.row_strips.hidden);
            hide(
                &states.message(2),
                &column_index,
                &mut self.column_strips.hidden,
            );
        }
    }

    /// Decodes every cell of the tile storage.
    fn load_cells(&mut self, store: &Proto) {
        let tiles = store.message(3);
        let tile_size = usize::try_from(tiles.int_or(2, 256)).unwrap_or(256);
        // The row tile tree (DataStore.rowTileTree 9) maps each tile's first row to its id; tiles can
        // start anywhere once rows were inserted (measured: a second tile from row 31).
        let tile_start: HashMap<i32, usize> = store
            .message(9)
            .messages(1)
            .iter()
            .map(|node| (node.int(2), usize::try_from(node.int(1)).unwrap_or(0)))
            .collect();
        for entry in tiles.messages(1) {
            let tile_id = entry.int(1);
            let Some(tile) = self.doc.body(entry.reference(2)) else {
                continue;
            };
            let start = tile_start
                .get(&tile_id)
                .copied()
                .unwrap_or(usize::try_from(tile_id).unwrap_or(0) * tile_size);
            for row_info in tile.messages(5) {
                self.load_row(&row_info, start);
            }
        }
        self.resolve_strings(store);
    }

    /// Decodes one TST.TileRowInfo.
    fn load_row(&mut self, row_info: &Proto, tile_start: usize) {
        let mut row = usize::try_from(row_info.int(1)).unwrap_or(0);
        if row < tile_start {
            row += tile_start;
        }
        let bnc = row_info.has(6) && row_info.has(7) && !row_info.raw(6).is_empty();
        let (buffer, offsets) = if bnc {
            (row_info.raw(6), row_info.raw(7))
        } else {
            (row_info.raw(3), row_info.raw(4))
        };
        // Wide rows (should_use_wide_rows) store offsets in 4-byte units and may hold more than 256.
        let wide = row_info.boolean(8);
        for (column, offset) in offsets.chunks_exact(2).enumerate() {
            let Some(offset) = le_u16(offset, 0).filter(|&offset| offset != 0xFFFF) else {
                continue;
            };
            let offset = usize::from(offset) * if wide { 4 } else { 1 };
            let parsed = if bnc {
                parse_bnc(buffer, offset)
            } else {
                parse_pre_bnc(buffer, offset)
            };
            if let Some(cell) = parsed {
                self.cells.insert((row, column), RefCell::new(cell));
            }
        }
    }

    /// Entries of a TST.TableDataList (inline or segmented), by key.
    ///
    /// # Arguments
    ///
    /// * `list_id` - The list object's id
    ///
    /// # Returns
    ///
    /// Entries by key (shared, cached per list).
    pub fn list(&self, list_id: u64) -> Rc<HashMap<u32, Proto>> {
        if let Some(cached) = self.lists.borrow().get(&list_id) {
            return Rc::clone(cached);
        }
        let mut entries = HashMap::new();
        if let Some(list) = self.doc.body(list_id) {
            let segments = list
                .references(4)
                .into_iter()
                .filter_map(|segment| self.doc.body(segment));
            for holder in std::iter::once(list).chain(segments) {
                entries.extend(
                    holder
                        .messages(3)
                        .into_iter()
                        .map(|entry| (entry.uint(1) as u32, entry)),
                );
            }
        }
        let entries = Rc::new(entries);
        self.lists.borrow_mut().insert(list_id, Rc::clone(&entries));
        entries
    }

    /// Fills in text values and resolves rich-text storages.
    fn resolve_strings(&mut self, store: &Proto) {
        let strings = self.list(store.reference(4));
        let rich = self.list(store.reference(17));
        for cell in self.cells.values_mut().map(RefCell::get_mut) {
            if cell.kind == CellKind::Text {
                cell.text = strings
                    .get(&cell.string_key)
                    .and_then(|entry| entry.string(3));
            }
            if cell.kind == CellKind::RichText && cell.rich_text_storage != 0 {
                let payload = rich
                    .get(&(cell.rich_text_storage as u32))
                    .and_then(|entry| self.doc.body(entry.reference(9)));
                cell.rich_text_storage = payload.map_or(0, |payload| payload.reference(1));
            }
        }
    }

    /// The style object a style-table key refers to (0: none).
    pub fn style_reference(&self, key: u32) -> u64 {
        if key == 0 {
            return 0;
        }
        self.list(self.model.message(4).reference(5))
            .get(&key)
            .map_or(0, |entry| entry.reference(4))
    }

    /// A format by key: the format table (22), else the legacy one (11); pre-BNC cells only use the
    /// legacy table.
    pub fn format(&self, key: u32, pre_bnc: bool) -> Option<Proto> {
        if key == 0 {
            return None;
        }
        let store = self.model.message(4);
        let lookup = |field: i32| {
            self.list(store.reference(field))
                .get(&key)
                .and_then(|entry| entry.message_opt(6))
        };
        if pre_bnc {
            return lookup(11);
        }
        lookup(22).or_else(|| lookup(11))
    }

    /// Categories (TST.GroupByArchive linked by TableInfo.group_by_uuid): Numbers shows a 50pt label
    /// column in front, and before each group a summary row carrying the group value, with the
    /// group's rows gathered under it.
    fn apply_categories(&mut self) {
        let Some(plan) = crate::categories::plan(self) else {
            return;
        };
        let new_index: HashMap<usize, usize> = plan
            .order
            .iter()
            .enumerate()
            .filter_map(|(new_row, old)| old.map(|old| (old, new_row)))
            .collect();
        let default_height = self.model.double(16) as f32;
        self.row_strips = self.row_strips.reordered(&plan.order, default_height);
        if let Some(fits) = &self.fitted_row_heights {
            let reordered = plan
                .order
                .iter()
                .map(|old| old.and_then(|old| fits.get(old).copied()).unwrap_or(0.0))
                .collect();
            self.fitted_row_heights = Some(reordered);
        }
        self.cells = std::mem::take(&mut self.cells)
            .into_iter()
            .filter_map(|((row, column), cell)| {
                new_index.get(&row).map(|&row| ((row, column + 1), cell))
            })
            .collect();
        let moved = |row: usize| new_index.get(&row).copied().unwrap_or(row);
        for merge in &mut self.merges {
            *merge = CellRange {
                first_row: moved(merge.first_row),
                first_column: merge.first_column + 1,
                last_row: moved(merge.last_row),
                last_column: merge.last_column + 1,
            };
        }
        self.rows = plan.order.len();
        self.columns += 1;
        self.column_strips = self.column_strips.with_leading(50.0);
        self.header_columns += 1;
        self.label_column = true;
        self.categories = plan.categories;
    }
}

/// Reads HeaderStorageBucket entries: index 1, size 2, hidden 3, cell style 5, text style 6.
fn apply_headers(doc: &IwaDocument, buckets: &[u64], strips: &mut Strips, rows: bool) {
    for bucket in buckets.iter().filter_map(|&bucket| doc.body(bucket)) {
        for header in bucket.messages(2) {
            let Some(index) = usize::try_from(header.int(1))
                .ok()
                .filter(|&index| index < strips.sizes.len())
            else {
                continue;
            };
            let size = header.float(2);
            if size > 0.0 {
                strips.sizes[index] = size;
                strips.explicit[index] = rows;
            }
            if header.int(3) != 0 {
                strips.hidden[index] = true;
            }
            strips.cell_style[index] = header.reference(5);
            strips.text_style[index] = header.reference(6);
        }
    }
}

/// Decodes a protobuf zigzag-encoded sint32.
fn zigzag(value: u64) -> i64 {
    (value >> 1) as i64 ^ -((value & 1) as i64)
}

/// A cursor over a cell record's 4-byte fields.
struct Fields<'a> {
    buffer: &'a [u8],
    position: usize,
}

impl Fields<'_> {
    fn next_u32(&mut self) -> Option<u32> {
        let value = le_u32(self.buffer, self.position)?;
        self.position += 4;
        Some(value)
    }

    fn next_f64(&mut self) -> Option<f64> {
        let value = le_u64(self.buffer, self.position).map(f64::from_bits)?;
        self.position += 8;
        Some(value)
    }

    fn next_decimal128(&mut self) -> Option<f64> {
        let value = decimal128(self.buffer.get(self.position..self.position + 16)?)?;
        self.position += 16;
        Some(value)
    }
}

/// A BNC (Numbers 2019+, version 5) cell record.
fn parse_bnc(buffer: &[u8], offset: usize) -> Option<Cell> {
    if *buffer.get(offset)? != 5 {
        return None;
    }
    let mut cell = Cell {
        kind: CellKind::from_stored(*buffer.get(offset + 1)?),
        ..Cell::default()
    };
    let flags = le_u32(buffer, offset + 8)?;
    let mut fields = Fields {
        buffer,
        position: offset + 12,
    };
    let has = |bit: u32| flags & bit != 0;
    let decimal = if has(0x1) {
        fields.next_decimal128()
    } else {
        None
    };
    let double = if has(0x2) { fields.next_f64() } else { None };
    let seconds = if has(0x4) { fields.next_f64() } else { None };
    // The remaining flagged fields are 4-byte keys, in bit order.
    let mut key = |bit: u32| if has(bit) { fields.next_u32() } else { Some(0) };
    cell.string_key = key(0x8)?;
    cell.rich_text_storage = u64::from(key(0x10)?);
    cell.cell_style_key = key(0x20)?;
    cell.text_style_key = key(0x40)?;
    cell.conditional_style_key = key(0x80)?;
    let applied = key(0x100)?;
    cell.conditional_rule_applied = has(0x100).then_some(applied);
    cell.formula_key = key(0x200)?;
    key(0x400)?; // control cell style
    key(0x800)?; // error key
    key(0x1000)?; // comment storage
    cell.number_format_key = key(0x2000)?;
    cell.currency_format_key = key(0x4000)?;
    cell.date_format_key = key(0x8000)?;
    cell.duration_format_key = key(0x10000)?;
    cell.text_format_key = key(0x20000)?;
    cell.bool_format_key = key(0x40000)?;
    cell.number = match cell.kind {
        CellKind::Date => seconds.or(decimal).or(double),
        CellKind::Bool | CellKind::Duration => double.or(decimal).or(seconds),
        _ => decimal.or(double).or(seconds),
    }
    .unwrap_or(0.0);
    Some(cell)
}

/// A pre-BNC cell (Numbers 2013-2019): version 4 has the cell type in byte 1 and 32-bit flags at
/// byte 4; version 3 has the type in byte 2 and 16-bit flags at byte 4. Both continue after a
/// 12-byte header with the flagged fields in the same order.
fn parse_pre_bnc(buffer: &[u8], offset: usize) -> Option<Cell> {
    let version = *buffer.get(offset)?;
    let (cell_type, flags) = match version {
        4 => (*buffer.get(offset + 1)?, le_u32(buffer, offset + 4)?),
        3 => (
            *buffer.get(offset + 2)?,
            u32::from(le_u16(buffer, offset + 4)?),
        ),
        _ => return None,
    };
    let mut cell = Cell {
        kind: CellKind::from_stored(cell_type),
        pre_bnc: true,
        ..Cell::default()
    };
    let mut fields = Fields {
        buffer,
        position: offset + 12,
    };
    let has = |bit: u32| flags & bit != 0;
    let mut key = |bit: u32| if has(bit) { fields.next_u32() } else { Some(0) };
    cell.cell_style_key = key(0x2)?;
    cell.text_style_key = key(0x80)?;
    cell.conditional_style_key = key(0x400)?;
    key(0x800)?;
    cell.number_format_key = key(0x4)?;
    cell.formula_key = key(0x8)?;
    key(0x100)?; // error key
    cell.rich_text_storage = u64::from(key(0x200)?);
    key(0x1000)?;
    cell.string_key = key(0x10)?;
    for bit in [0x20, 0x40] {
        if has(bit) {
            cell.number = fields.next_f64()?;
        }
    }
    // Pre-BNC cells use one format key for every value type.
    let format = cell.number_format_key;
    (
        cell.currency_format_key,
        cell.date_format_key,
        cell.duration_format_key,
        cell.bool_format_key,
    ) = (format, format, format, format);
    Some(cell)
}

/// IEEE 754-2008 decimal128 with a binary integer significand (as Numbers stores numbers).
///
/// The decimal digits are parsed as text so that values like 0.1 become the nearest double.
fn decimal128(bytes: &[u8]) -> Option<f64> {
    let exponent = (i32::from(bytes[15] & 0x7F) << 7 | i32::from(bytes[14] >> 1)) - 0x1820;
    let significand = bytes[..14]
        .iter()
        .rev()
        .fold(u128::from(bytes[14] & 1), |acc, &byte| {
            acc << 8 | u128::from(byte)
        });
    let value: f64 = format!("{significand}E{exponent}").parse().ok()?;
    Some(if bytes[15] & 0x80 != 0 { -value } else { value })
}

/// The localised name of an aggregate. Numbers writes it in the document's language (measured: a
/// da_DK document shows "Tæl (alle)"); the English text is assumed from the same pattern.
pub fn aggregate_name(doc: &IwaDocument, aggregate: i32) -> &'static str {
    let locale = doc
        .body(1)
        .and_then(|root| root.message(8).message(1).string(4))
        .unwrap_or_default();
    match (aggregate, locale.to_lowercase().starts_with("da")) {
        (1, true) => "Tæl (alle)",
        (1, false) => "Count (all)",
        _ => "",
    }
}

/// The text a cell displays (convenience over `cell_format::display`).
pub fn display(table: &NumbersTable, position: CellPosition) -> Option<String> {
    table
        .cell(position)
        .and_then(|cell| cell_format::display(table, &cell))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decimal(significand: u128, exponent: i32, negative: bool) -> [u8; 16] {
        let mut bytes = significand.to_le_bytes();
        let biased = (exponent + 0x1820) as u16;
        bytes[14] = (bytes[14] & 1) | ((biased << 1) as u8);
        bytes[15] = ((biased >> 7) as u8 & 0x7F) | if negative { 0x80 } else { 0 };
        bytes
    }

    #[test]
    fn decimal128_values() {
        assert_eq!(decimal128(&decimal(1, -1, false)), Some(0.1));
        assert_eq!(decimal128(&decimal(12345, 2, true)), Some(-1234500.0));
    }

    #[test]
    fn zigzag_decodes_signed() {
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(1), -1);
        assert_eq!(zigzag(4), 2);
    }

    #[test]
    fn bnc_number_cell() {
        let mut record = vec![5, 2, 0, 0, 0, 0, 0, 0];
        record.extend((0x2u32 | 0x20).to_le_bytes());
        record.extend(2.5f64.to_le_bytes());
        record.extend(7u32.to_le_bytes());
        let cell = parse_bnc(&record, 0).expect("valid record");
        assert_eq!(cell.kind, CellKind::Number);
        assert_eq!(cell.number, 2.5);
        assert_eq!(cell.cell_style_key, 7);
        assert!(parse_bnc(&record[..14], 0).is_none());
    }

    #[test]
    fn pre_bnc_shares_its_format_key() {
        let mut record = vec![4, 2, 0, 0];
        record.extend((0x4u32 | 0x20).to_le_bytes());
        record.extend([0u8; 4]);
        record.extend(9u32.to_le_bytes());
        record.extend(3.0f64.to_le_bytes());
        let cell = parse_pre_bnc(&record, 0).expect("valid record");
        assert_eq!(
            (cell.number, cell.number_format_key, cell.date_format_key),
            (3.0, 9, 9)
        );
    }

    #[test]
    fn strips_reorder_and_insert() {
        let mut strips = Strips::new(2, 20.0);
        strips.sizes[1] = 30.0;
        let reordered = strips.reordered(&[Some(1), None, Some(0)], 10.0);
        assert_eq!(reordered.sizes, vec![30.0, 10.0, 20.0]);
        assert_eq!(strips.with_leading(50.0).sizes, vec![50.0, 20.0, 30.0]);
    }
}
