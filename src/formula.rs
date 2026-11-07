//! Evaluates formulas whose cells carry no cached result. Numbers recalculates on open, so a
//! document written by another tool (pybers, numbers-parser) can store only the formula and an
//! error value, while Apple's export shows the computed value. The AST (TSCE.ASTNodeArrayArchive)
//! is postfix: operands precede their operator, functions carry their argument count. Anything
//! unsupported leaves the cell's error in place, which exports blank.

use crate::cell_format;
use crate::iwa::{IwaDocument, Proto};
use crate::numbers_table::{CellKind, CellPosition, NumbersTable};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, RangeInclusive};
use std::rc::Rc;

/// Why a formula was not evaluated (logged at debug level).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct Unsupported(String);

type Evaluation<T> = std::result::Result<T, Unsupported>;

fn unsupported<T>(what: impl Into<String>) -> Evaluation<T> {
    Err(Unsupported(what.into()))
}

/// A table a formula refers to: the formula's own (borrowed) or another one (loaded and cached).
#[derive(Debug, Clone)]
enum TableRef<'t> {
    Host(&'t NumbersTable),
    Other(Rc<NumbersTable>),
}

impl Deref for TableRef<'_> {
    type Target = NumbersTable;
    fn deref(&self) -> &NumbersTable {
        match self {
            TableRef::Host(table) => table,
            TableRef::Other(table) => table,
        }
    }
}

impl PartialEq for TableRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq::<NumbersTable>(&**self, &**other)
    }
}

/// A rectangle of cells in one table.
#[derive(Debug, Clone, PartialEq)]
struct Span<'t> {
    table: TableRef<'t>,
    rows: RangeInclusive<usize>,
    columns: RangeInclusive<usize>,
}

impl Span<'_> {
    fn cell_count(&self) -> usize {
        self.rows.clone().count() * self.columns.clone().count()
    }
}

/// A formula value.
#[derive(Debug, Clone, PartialEq)]
enum Value<'t> {
    Empty,
    Number(f64),
    Bool(bool),
    Text(String),
    /// NOW(): only its type is observable (ISDATE).
    Date,
    Error,
    Range(Span<'t>),
    /// Cells picked by row/column UIDs (COLON_NODE_WITH_UIDS), in order.
    Picked(TableRef<'t>, Vec<CellPosition>),
}

/// A chart data value read through a formula.
#[derive(Debug, Clone, PartialEq)]
pub enum ChartValue {
    /// A number (numbers, dates and durations as their stored value).
    Number(f64),
    /// A boolean.
    Bool(bool),
    /// Text.
    Text(String),
}

impl ChartValue {
    /// The value as a label (numbers in the automatic format).
    pub fn label(value: &Option<ChartValue>) -> String {
        match value {
            Some(ChartValue::Number(number)) => cell_format::general(*number),
            Some(ChartValue::Bool(true)) => "TRUE".into(),
            Some(ChartValue::Bool(false)) => "FALSE".into(),
            Some(ChartValue::Text(text)) => text.clone(),
            None => String::new(),
        }
    }
}

thread_local! {
    /// Cells being evaluated (a reference back to one of them is circular).
    static ACTIVE: RefCell<HashSet<(usize, CellPosition)>> = RefCell::new(HashSet::new());
    /// Tables by normalised table id, loaded for cross-table references (cleared per document).
    static TABLES: RefCell<HashMap<String, Rc<NumbersTable>>> = RefCell::new(HashMap::new());
    /// The table chart formulas are evaluated in (the document's first).
    static CHART_HOST: RefCell<Option<Rc<NumbersTable>>> = const { RefCell::new(None) };
}

/// Forgets tables loaded for the previous document.
pub fn reset() {
    TABLES.with(|tables| tables.borrow_mut().clear());
    CHART_HOST.with(|host| host.borrow_mut().take());
}

/// Replaces an error cell's value with its formula's result.
///
/// # Arguments
///
/// * `table` - The cell's table
/// * `position` - The cell
///
/// # Returns
///
/// True when the formula was evaluated and the cell now holds a number, boolean or text.
pub fn try_resolve(table: &NumbersTable, position: CellPosition) -> bool {
    let Some(cell) = table.cells.get(&position) else {
        return false;
    };
    let formula_key = cell.borrow().formula_key;
    let formulas = table.list(table.model.message(4).reference(6));
    let Some(formula) = formulas
        .get(&formula_key)
        .and_then(|entry| entry.message_opt(5))
        .filter(|_| formula_key != 0)
    else {
        return false;
    };
    let guard = (std::ptr::from_ref(table) as usize, position);
    if !ACTIVE.with(|active| active.borrow_mut().insert(guard)) {
        return false;
    }
    let evaluator = Evaluator {
        host: table,
        row: position.0,
        column: position.1,
    };
    let result = evaluator
        .run(&formula.message(1).messages(1))
        .and_then(scalar);
    ACTIVE.with(|active| active.borrow_mut().remove(&guard));
    let mut cell = cell.borrow_mut();
    match result {
        Ok(Value::Number(number)) if number.is_finite() => {
            cell.kind = CellKind::Number;
            cell.number = number;
        }
        Ok(Value::Bool(flag)) => {
            cell.kind = CellKind::Bool;
            cell.number = f64::from(u8::from(flag));
        }
        Ok(Value::Text(text)) => {
            cell.kind = CellKind::Text;
            cell.text = Some(text);
        }
        Ok(_) => return false,
        Err(reason) => {
            log::debug!("formula at {position:?} not evaluated: {reason}");
            return false;
        }
    }
    true
}

/// A chart series or label formula (TN.ChartMediatorFormulaStorage) evaluated to its values.
///
/// # Arguments
///
/// * `doc` - The document
/// * `formula` - The TSCE.FormulaArchive
///
/// # Returns
///
/// The values of the visible cells it covers (`None` per error cell), or `None` when the formula
/// cannot be evaluated. Charts leave out hidden rows and columns.
pub fn chart_values(doc: &Rc<IwaDocument>, formula: &Proto) -> Option<Vec<Option<ChartValue>>> {
    let host = CHART_HOST.with(|host| {
        let mut host = host.borrow_mut();
        if host.is_none() {
            let info = doc
                .of_type(6000)
                .first()
                .map(|object| object.body.clone())?;
            *host = NumbersTable::load(doc, &info).ok().map(Rc::new);
        }
        host.clone()
    })?;
    let evaluator = Evaluator {
        host: &host,
        row: 0,
        column: 0,
    };
    let value = evaluator.run(&formula.message(1).messages(1)).ok()?;
    let shown = |table: &NumbersTable, (row, column): CellPosition| {
        !table.row_strips.hidden.get(row).copied().unwrap_or(false)
            && !table
                .column_strips
                .hidden
                .get(column)
                .copied()
                .unwrap_or(false)
    };
    let values = match value {
        Value::Picked(table, cells) => cells
            .into_iter()
            .filter(|&cell| shown(&table, cell))
            .map(|cell| cell_value(&table, cell))
            .collect(),
        Value::Range(span) => {
            let cells = span
                .rows
                .clone()
                .flat_map(|row| span.columns.clone().map(move |column| (row, column)));
            cells
                .filter(|&cell| shown(&span.table, cell))
                .map(|cell| cell_value(&span.table, cell))
                .collect()
        }
        other => values(other).ok()?,
    };
    let convert = |value: Value| match value {
        Value::Number(number) => Some(Some(ChartValue::Number(number))),
        Value::Bool(flag) => Some(Some(ChartValue::Bool(flag))),
        Value::Text(text) => Some(Some(ChartValue::Text(text))),
        Value::Empty | Value::Error => Some(None),
        _ => None,
    };
    values.into_iter().map(convert).collect()
}

/// Decodes a protobuf zigzag sint32.
fn zigzag(value: u64) -> i64 {
    (value >> 1) as i64 ^ -((value & 1) as i64)
}

/// The value of a cell (evaluating an unresolved formula first).
fn cell_value<'t>(table: &TableRef<'t>, position: CellPosition) -> Value<'t> {
    let Some(stored) = table.cells.get(&position) else {
        return Value::Empty;
    };
    if stored.borrow().kind == CellKind::Error && !try_resolve(table, position) {
        return Value::Error;
    }
    let cell = stored.borrow();
    match cell.kind {
        CellKind::Number | CellKind::Currency | CellKind::Date | CellKind::Duration => {
            Value::Number(cell.number)
        }
        CellKind::Bool => Value::Bool(cell.number != 0.0),
        CellKind::Text => Value::Text(cell.text.clone().unwrap_or_default()),
        CellKind::Error => Value::Error,
        CellKind::Empty => Value::Empty,
        // Rich text has no plain value here; it reads as text-free.
        CellKind::RichText => Value::Error,
    }
}

/// A single value: a one-cell range reads its cell.
fn scalar(value: Value<'_>) -> Evaluation<Value<'_>> {
    match value {
        Value::Range(span) if span.cell_count() == 1 => Ok(cell_value(
            &span.table,
            (*span.rows.start(), *span.columns.start()),
        )),
        Value::Range(_) | Value::Picked(..) => unsupported("range as scalar"),
        other => Ok(other),
    }
}

/// Every value a range (or picked list) holds; a scalar is a list of one.
fn values(value: Value<'_>) -> Evaluation<Vec<Value<'_>>> {
    Ok(match value {
        Value::Picked(table, cells) => cells
            .into_iter()
            .map(|cell| cell_value(&table, cell))
            .collect(),
        Value::Range(span) if span.cell_count() > 10_000 => {
            // Whole-axis ranges over a large table: only stored cells hold values.
            let mut stored: Vec<CellPosition> = span
                .table
                .cells
                .keys()
                .filter(|(row, column)| span.rows.contains(row) && span.columns.contains(column))
                .copied()
                .collect();
            stored.sort_unstable();
            stored
                .into_iter()
                .map(|cell| cell_value(&span.table, cell))
                .collect()
        }
        Value::Range(span) => {
            let cells = span
                .rows
                .clone()
                .flat_map(|row| span.columns.clone().map(move |column| (row, column)));
            cells.map(|cell| cell_value(&span.table, cell)).collect()
        }
        other => vec![other],
    })
}

fn number(value: &Value) -> Evaluation<f64> {
    match value {
        Value::Empty => Ok(0.0),
        Value::Number(number) => Ok(*number),
        Value::Bool(flag) => Ok(f64::from(u8::from(*flag))),
        Value::Text(text) => text
            .trim()
            .parse()
            .or_else(|_| unsupported("text is not a number")),
        _ => unsupported("not a number"),
    }
}

fn text(value: &Value) -> Evaluation<String> {
    match value {
        Value::Empty => Ok(String::new()),
        Value::Text(text) => Ok(text.clone()),
        Value::Bool(flag) => Ok(if *flag { "TRUE" } else { "FALSE" }.into()),
        Value::Number(number) => Ok(cell_format::number(*number, None, false)),
        _ => unsupported("not text"),
    }
}

fn truth(value: &Value) -> Evaluation<bool> {
    match value {
        Value::Bool(flag) => Ok(*flag),
        other => Ok(number(other)? != 0.0),
    }
}

/// Case-insensitive ordering of two strings (as .NET's OrdinalIgnoreCase).
fn compare_text(left: &str, right: &str) -> std::cmp::Ordering {
    left.to_uppercase().cmp(&right.to_uppercase())
}

/// Binary operators: 1 + 2 - 3 × 4 ÷ 5 ^, 7 > 8 ≥ 9 < 10 ≤ 11 = 12 ≠.
fn binary<'t>(operator: i32, left: Value<'t>, right: Value<'t>) -> Evaluation<Value<'t>> {
    if left == Value::Error || right == Value::Error {
        return Ok(Value::Error);
    }
    if operator >= 7 {
        let ordering = if matches!(left, Value::Text(_)) || matches!(right, Value::Text(_)) {
            compare_text(&text(&left)?, &text(&right)?)
        } else {
            number(&left)?.total_cmp(&number(&right)?)
        };
        use std::cmp::Ordering::{Equal, Greater, Less};
        let holds = match operator {
            7 => ordering == Greater,
            8 => ordering != Less,
            9 => ordering == Less,
            10 => ordering != Greater,
            11 => ordering == Equal,
            _ => ordering != Equal,
        };
        return Ok(Value::Bool(holds));
    }
    let (left_number, right_number) = (number(&left)?, number(&right)?);
    Ok(match operator {
        1 => Value::Number(left_number + right_number),
        2 => Value::Number(left_number - right_number),
        3 => Value::Number(left_number * right_number),
        4 if right_number == 0.0 => Value::Error,
        4 => Value::Number(left_number / right_number),
        _ => Value::Number(left_number.powf(right_number)),
    })
}

/// A decimal128 number node (BID: sign, 14-bit exponent biased by 6176, 113-bit coefficient), or
/// the double in field 4.
fn number_node(node: &Proto) -> f64 {
    if !node.has(42) {
        return node.double(4);
    }
    let (low, high) = (node.uint(42), node.uint(43));
    let exponent = ((high >> 49) & 0x3FFF) as i64 - 6176;
    let coefficient = u128::from(high & 0x1_FFFF_FFFF_FFFF) << 64 | u128::from(low);
    let value: f64 = format!("{coefficient}E{exponent}").parse().unwrap_or(0.0);
    if high >> 63 != 0 {
        -value
    } else {
        value
    }
}

/// Evaluates one formula in the context of its cell.
struct Evaluator<'t> {
    host: &'t NumbersTable,
    row: usize,
    column: usize,
}

impl<'t> Evaluator<'t> {
    fn run(&self, nodes: &[Proto]) -> Evaluation<Value<'t>> {
        let mut stack: Vec<Value<'t>> = Vec::new();
        let pop = |stack: &mut Vec<Value<'t>>| {
            stack
                .pop()
                .ok_or_else(|| Unsupported("stack underflow".into()))
        };
        for node in nodes {
            let node_type = node.int(1);
            let value = match node_type {
                1..=5 | 7..=12 => {
                    let right = scalar(pop(&mut stack)?)?;
                    let left = scalar(pop(&mut stack)?)?;
                    binary(node_type, left, right)?
                }
                6 => {
                    // Concatenation (&).
                    let right = scalar(pop(&mut stack)?)?;
                    let left = scalar(pop(&mut stack)?)?;
                    if left == Value::Error || right == Value::Error {
                        Value::Error
                    } else {
                        Value::Text(text(&left)? + &text(&right)?)
                    }
                }
                13 => match scalar(pop(&mut stack)?)? {
                    Value::Error => Value::Error,
                    operand => Value::Number(-number(&operand)?),
                },
                15 => match scalar(pop(&mut stack)?)? {
                    Value::Error => Value::Error,
                    operand => Value::Number(number(&operand)? / 100.0),
                },
                14 | 32 | 33 => continue, // unary plus, whitespace
                16 => {
                    let count = usize::try_from(node.int(3)).unwrap_or(0);
                    let split = stack
                        .len()
                        .checked_sub(count)
                        .ok_or_else(|| Unsupported("argument count".into()))?;
                    let arguments = stack.split_off(split);
                    call(node.int(2), arguments)?
                }
                17 => Value::Number(number_node(node)),
                18 => Value::Bool(node.boolean(5)),
                19 => Value::Text(node.string(6).unwrap_or_default()),
                22 => Value::Empty,
                36 => Value::Range(self.cell_reference(node)?),
                29 => {
                    let (Value::Range(right), Value::Range(left)) =
                        (pop(&mut stack)?, pop(&mut stack)?)
                    else {
                        return unsupported("colon");
                    };
                    Value::Range(bounding(left, right)?)
                }
                67 => Value::Range(self.tract(node)?),
                45 => {
                    // COLON_NODE_WITH_UIDS: the endpoints' range, its rows/columns named by UID.
                    let (Value::Range(right), Value::Range(left)) =
                        (pop(&mut stack)?, pop(&mut stack)?)
                    else {
                        return unsupported("uid colon");
                    };
                    let span = bounding(left, right)?;
                    uid_cells(span, &node.message(38))?
                }
                other => return unsupported(format!("node {other}")),
            };
            stack.push(value);
        }
        match (stack.pop(), stack.is_empty()) {
            (Some(value), true) => Ok(value),
            _ => unsupported("stack"),
        }
    }

    /// The table a reference node names (cross-table extra info 28), else the host.
    fn table_of(&self, node: &Proto) -> Evaluation<TableRef<'t>> {
        let Some(extra) = node.message_opt(28) else {
            return Ok(TableRef::Host(self.host));
        };
        let id = extra.message(1);
        let doc = &self.host.doc;
        let host_model = self.host.info.reference(2);
        for key in uuid_keys(&id) {
            for object in doc.of_type(6000) {
                let model_id = object.body.reference(2);
                let matches = doc
                    .body(model_id)
                    .and_then(|model| model.string(1))
                    .is_some_and(|table_id| normalise_id(&table_id) == key);
                if !matches {
                    continue;
                }
                if model_id == host_model {
                    return Ok(TableRef::Host(self.host));
                }
                let cached = TABLES.with(|tables| tables.borrow().get(&key).cloned());
                let table = match cached {
                    Some(table) => table,
                    None => {
                        let loaded = Rc::new(
                            NumbersTable::load(doc, &object.body)
                                .or_else(|error| unsupported(error.to_string()))?,
                        );
                        TABLES.with(|tables| {
                            tables.borrow_mut().insert(key.clone(), Rc::clone(&loaded))
                        });
                        loaded
                    }
                };
                return Ok(TableRef::Other(table));
            }
        }
        unsupported("table id")
    }

    /// A cell reference: column 26 and row 27, each absolute (flag 2) or relative to the cell. A
    /// reference without a row is the whole column, without a column the whole row: the body cells,
    /// headers and footers excluded (measured: chart series "Avg. Rain Fall (in)").
    fn cell_reference(&self, node: &Proto) -> Evaluation<Span<'t>> {
        let table = self.table_of(node)?;
        let coordinate = |field: i32, own: usize| {
            let axis = node.message(field);
            let offset = zigzag(axis.uint(1));
            let absolute = if axis.boolean(2) {
                offset
            } else {
                own as i64 + offset
            };
            usize::try_from(absolute).or_else(|_| unsupported("negative reference"))
        };
        let (has_column, has_row) = (node.has(26), node.has(27));
        let column = if has_column {
            coordinate(26, self.column)?
        } else {
            0
        };
        let row = if has_row {
            coordinate(27, self.row)?
        } else {
            0
        };
        let (rows, columns) = match (has_row, has_column) {
            (false, true) => {
                let last = table
                    .rows
                    .saturating_sub(1 + table.footer_rows)
                    .max(table.header_rows);
                (table.header_rows..=last, column..=column)
            }
            (true, false) => (
                row..=row,
                table.header_columns..=table.columns.saturating_sub(1).max(table.header_columns),
            ),
            _ => (row..=row, column..=column),
        };
        Ok(Span {
            table,
            rows,
            columns,
        })
    }

    /// A COLON_TRACT range: absolute (columns 3, rows 4) or relative (1, 2) begin/end pairs.
    fn tract(&self, node: &Proto) -> Evaluation<Span<'t>> {
        let table = self.table_of(node)?;
        let tract = node.message(40);
        let span = |relative_field: i32,
                    absolute_field: i32,
                    own: usize,
                    count: usize|
         -> RangeInclusive<usize> {
            let whole = 0..=count.saturating_sub(1);
            if let Some(absolute) = tract.message_opt(absolute_field) {
                let begin = absolute.int(1);
                // i32::MAX (rows) and 32767 (columns) mean the whole axis ("A" = A1:A∞).
                if begin == i32::MAX || (absolute_field == 3 && begin == 32767) {
                    return whole;
                }
                let end = if absolute.has(2) {
                    absolute.int(2)
                } else {
                    begin
                };
                return to_index(i64::from(begin))..=to_index(i64::from(end));
            }
            if let Some(relative) = tract.message_opt(relative_field) {
                let begin = own as i64 + i64::from(relative.int(1));
                let end = own as i64
                    + i64::from(if relative.has(2) {
                        relative.int(2)
                    } else {
                        relative.int(1)
                    });
                return to_index(begin)..=to_index(end);
            }
            whole
        };
        let columns = span(1, 3, self.column, table.columns);
        let rows = span(2, 4, self.row, table.rows);
        Ok(Span {
            table,
            rows,
            columns,
        })
    }
}

fn to_index(value: i64) -> usize {
    usize::try_from(value).unwrap_or(0)
}

/// The smallest range covering two ranges of one table.
fn bounding<'t>(left: Span<'t>, right: Span<'t>) -> Evaluation<Span<'t>> {
    if left.table != right.table {
        return unsupported("ranges in different tables");
    }
    let rows = *left.rows.start().min(right.rows.start())..=*left.rows.end().max(right.rows.end());
    let columns = *left.columns.start().min(right.columns.start())
        ..=*left.columns.end().max(right.columns.end());
    Ok(Span {
        table: left.table,
        rows,
        columns,
    })
}

/// Cells of the UID tracts (38): per tract, the listed column (1) and row (2) UIDs, or the whole
/// span's when none are listed.
fn uid_cells<'t>(span: Span<'t>, tracts: &Proto) -> Evaluation<Value<'t>> {
    let (column_index, row_index) = span.table.uid_indexes();
    let pick = |list: Proto,
                index: &HashMap<(u64, u64), usize>,
                whole: &RangeInclusive<usize>|
     -> Evaluation<Vec<usize>> {
        let uids = list.messages(1);
        if uids.is_empty() {
            return Ok(whole.clone().collect());
        }
        uids.iter()
            .map(|uid| {
                index
                    .get(&(uid.uint(1), uid.uint(2)))
                    .copied()
                    .ok_or_else(|| Unsupported("uid".into()))
            })
            .collect()
    };
    let mut cells = Vec::new();
    for tract in tracts.messages(1) {
        let columns = pick(tract.message(1), &column_index, &span.columns)?;
        let rows = pick(tract.message(2), &row_index, &span.rows)?;
        cells.extend(
            rows.iter()
                .flat_map(|&row| columns.iter().map(move |&column| (row, column))),
        );
    }
    Ok(Value::Picked(span.table, cells))
}

/// A table id without dashes, upper case.
fn normalise_id(id: &str) -> String {
    id.replace('-', "").to_uppercase()
}

/// The forms a TSP.UUID may take in a model's table id: raw bytes (1), or four 32-bit words
/// (2-5) in either byte order.
fn uuid_keys(id: &Proto) -> Vec<String> {
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>()
    };
    let mut keys = Vec::new();
    if id.has(1) {
        keys.push(hex(id.raw(1)));
    }
    let words = [2, 3, 4, 5].map(|field| id.uint(field) as u32);
    keys.push(words.iter().map(|word| format!("{word:08X}")).collect());
    keys.push(words.iter().map(|word| hex(&word.to_le_bytes())).collect());
    keys
}

// ------------------------------------------------------------ functions

/// The numbers among the arguments: ranges contribute their numeric cells, scalars are converted.
fn numbers(arguments: &[Value]) -> Evaluation<Vec<f64>> {
    let mut found = Vec::new();
    for argument in arguments {
        if matches!(argument, Value::Range(_) | Value::Picked(..)) {
            for value in values(argument.clone())? {
                match value {
                    Value::Number(number) => found.push(number),
                    Value::Error => return unsupported("error in range"),
                    _ => {}
                }
            }
        } else {
            found.push(number(&scalar(argument.clone())?)?);
        }
    }
    Ok(found)
}

/// Every value of every argument.
fn flattened<'t>(arguments: &[Value<'t>]) -> Evaluation<Vec<Value<'t>>> {
    let mut all = Vec::new();
    for argument in arguments {
        all.extend(values(argument.clone())?);
    }
    Ok(all)
}

/// The cells of a conditional range (SUMIF, COUNTIF...).
fn condition_cells<'t>(range: &Value<'t>) -> Evaluation<Vec<Value<'t>>> {
    match range {
        Value::Range(span) if span.cell_count() > 10_000 => unsupported("large conditional range"),
        other => values(other.clone()),
    }
}

/// A test on a cell value.
type Test = Box<dyn Fn(&Value) -> bool>;

/// A criterion (">=5", "apples", 3) as a test on values.
fn criterion(criterion: &Value) -> Evaluation<Test> {
    let criterion = scalar(criterion.clone())?;
    let Value::Text(text_criterion) = criterion else {
        let target = number(&criterion)?;
        return Ok(Box::new(move |value: &Value| {
            *value == Value::Number(target)
        }));
    };
    for operator in [">=", "<=", "<>", ">", "<", "="] {
        let Some(right) = text_criterion.strip_prefix(operator) else {
            continue;
        };
        let right = right.to_owned();
        let numeric: Option<f64> = right.trim().parse().ok();
        return Ok(Box::new(move |value: &Value| {
            use std::cmp::Ordering::{Equal, Greater, Less};
            let ordering = match (numeric, value) {
                (Some(target), Value::Number(number)) => number.total_cmp(&target),
                // A numeric criterion never matches text; only "<>" holds.
                (Some(_), _) => return operator == "<>",
                (None, other) => compare_text(&text(other).unwrap_or_default(), &right),
            };
            match operator {
                ">=" => ordering != Less,
                "<=" => ordering != Greater,
                "<>" => ordering != Equal,
                ">" => ordering == Greater,
                "<" => ordering == Less,
                _ => ordering == Equal,
            }
        }));
    }
    Ok(Box::new(move |value: &Value| {
        text(value).is_ok_and(|value| compare_text(&value, &text_criterion).is_eq())
    }))
}

/// SUMIFS: the sum of `sum_range` cells whose conditions all hold.
fn sum_ifs<'t>(
    sum_range: &Value<'t>,
    conditions: &[(Value<'t>, Value<'t>)],
) -> Evaluation<Value<'t>> {
    let sums = condition_cells(sum_range)?;
    let tests = conditions
        .iter()
        .map(|(range, test)| Ok((condition_cells(range)?, criterion(test)?)))
        .collect::<Evaluation<Vec<_>>>()?;
    let total = sums
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            tests
                .iter()
                .all(|(cells, test)| cells.get(*index).is_some_and(test))
        })
        .filter_map(|(_, value)| {
            if let Value::Number(number) = value {
                Some(*number)
            } else {
                None
            }
        })
        .sum();
    Ok(Value::Number(total))
}

fn factorial(value: f64) -> f64 {
    (2..=value as i64).map(|factor| factor as f64).product()
}

fn combinations(total: f64, chosen: f64) -> f64 {
    factorial(total) / (factorial(chosen) * factorial(total - chosen))
}

/// The gamma function (Lanczos approximation, g = 7, n = 9).
fn gamma(value: f64) -> f64 {
    use std::f64::consts::PI;
    if value < 0.5 {
        return PI / ((PI * value).sin() * gamma(1.0 - value));
    }
    const COEFFICIENTS: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    let shifted = value - 1.0;
    let sum = COEFFICIENTS[1..]
        .iter()
        .enumerate()
        .fold(COEFFICIENTS[0], |sum, (index, coefficient)| {
            sum + coefficient / (shifted + index as f64 + 1.0)
        });
    let base = shifted + 7.5;
    (2.0 * PI).sqrt() * base.powf(shifted + 0.5) * (-base).exp() * sum
}

/// Rounds to `places` decimals (clamped to 0-15): half away from zero, or toward zero.
fn round_to(value: f64, places: i32, toward_zero: bool) -> f64 {
    let scale = 10f64.powi(places.clamp(0, 15));
    let scaled = value * scale;
    (if toward_zero {
        scaled.trunc()
    } else {
        scaled.round()
    }) / scale
}

/// Pearson correlation of two equal-length lists (None when undefined).
fn correlation(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() != ys.len() || xs.len() < 2 {
        return None;
    }
    let mean = |list: &[f64]| list.iter().sum::<f64>() / list.len() as f64;
    let (mean_x, mean_y) = (mean(xs), mean(ys));
    let products: f64 = xs
        .iter()
        .zip(ys)
        .map(|(first, second)| (first - mean_x) * (second - mean_y))
        .sum();
    let squares_x: f64 = xs.iter().map(|first| (first - mean_x).powi(2)).sum();
    let squares_y: f64 = ys.iter().map(|second| (second - mean_y).powi(2)).sum();
    Some(products / (squares_x * squares_y).sqrt())
}

/// Calls function `index` (TSCE function numbering) on its arguments.
fn call<'t>(index: i32, arguments: Vec<Value<'t>>) -> Evaluation<Value<'t>> {
    // IFERROR (235) and ISERROR-like (70) see errors; every other function propagates them.
    if !matches!(index, 70 | 235) && arguments.contains(&Value::Error) {
        return Ok(Value::Error);
    }
    let argument = |position: usize| {
        arguments
            .get(position)
            .cloned()
            .ok_or_else(|| Unsupported("missing argument".into()))
    };
    let numeric = |position: usize| -> Evaluation<f64> { number(&scalar(argument(position)?)?) };
    let textual = |position: usize| -> Evaluation<String> { text(&scalar(argument(position)?)?) };
    let optional = |position: usize, default: f64| {
        if arguments.len() > position {
            numeric(position)
        } else {
            Ok(default)
        }
    };
    let num = |value: f64| Ok(Value::Number(value));
    match index {
        1 => num(numeric(0)?.abs()),
        4 => num(numeric(0)?.acos()),
        5 => num(numeric(0)?.acosh()),
        7 => Ok(Value::Bool(
            flattened(&arguments)?
                .iter()
                .map(truth)
                .collect::<Evaluation<Vec<_>>>()?
                .into_iter()
                .all(|flag| flag),
        )),
        9 => num(numeric(0)?.asin()),
        10 => num(numeric(0)?.asinh()),
        11 => num(numeric(0)?.atan()),
        12 => num(numeric(1)?.atan2(numeric(0)?)),
        13 => num(numeric(0)?.atanh()),
        15 => {
            let list = numbers(&arguments)?;
            if list.is_empty() {
                Ok(Value::Error)
            } else {
                num(list.iter().sum::<f64>() / list.len() as f64)
            }
        }
        24 => num(combinations(numeric(0)?.floor(), numeric(1)?.floor())),
        25 | 329 => Ok(Value::Text(
            flattened(&arguments)?
                .iter()
                .map(text)
                .collect::<Evaluation<String>>()?,
        )),
        28 => num(numeric(0)?.cos()),
        29 => num(numeric(0)?.cosh()),
        30 => num(flattened(&arguments)?
            .iter()
            .filter(|value| matches!(value, Value::Number(_)))
            .count() as f64),
        31 => num(flattened(&arguments)?
            .iter()
            .filter(|value| !matches!(value, Value::Empty) && **value != Value::Text(String::new()))
            .count() as f64),
        33 => {
            let test = criterion(&argument(1)?)?;
            num(condition_cells(&argument(0)?)?
                .iter()
                .filter(|value| test(value))
                .count() as f64)
        }
        44 => num(numeric(0)?.to_degrees()),
        50 => num(numeric(0)?.exp()),
        51 => num(factorial(numeric(0)?)),
        52 => Ok(Value::Bool(false)),
        53 | 131 => {
            // FIND (case-sensitive) and SEARCH, 1-based, optional start.
            let (needle, haystack) = (textual(0)?, textual(1)?);
            let start = optional(2, 1.0)? as i64 - 1;
            let characters: Vec<char> = haystack.chars().collect();
            let Some(start) = usize::try_from(start)
                .ok()
                .filter(|&start| start <= characters.len())
            else {
                return Ok(Value::Error);
            };
            let fold = |text: &str| {
                if index == 53 {
                    text.to_owned()
                } else {
                    text.to_uppercase()
                }
            };
            let rest: String = characters[start..].iter().collect();
            match fold(&rest).find(&fold(&needle)) {
                Some(byte) => num((start + rest[..byte].chars().count() + 1) as f64),
                None => Ok(Value::Error),
            }
        }
        62 => {
            // IF
            let chosen = if truth(&scalar(argument(0)?)?)? {
                argument(1)
            } else if arguments.len() > 2 {
                argument(2)
            } else {
                Ok(Value::Bool(false))
            };
            scalar(chosen?)
        }
        65 => num(numeric(0)?.floor()),
        76 => {
            let source = textual(0)?;
            let count = optional(1, 1.0)?.max(0.0) as usize;
            Ok(Value::Text(source.chars().take(count).collect()))
        }
        77 => num(textual(0)?.chars().count() as f64),
        78 => num(numeric(0)?.ln()),
        79 => num(if arguments.len() > 1 {
            numeric(0)?.log(numeric(1)?)
        } else {
            numeric(0)?.log10()
        }),
        80 => num(numeric(0)?.log10()),
        82 => Ok(Value::Text(textual(0)?.to_lowercase())),
        84 => num(numbers(&arguments)?
            .into_iter()
            .reduce(f64::max)
            .unwrap_or(0.0)),
        87 => {
            // MID
            let source: Vec<char> = textual(0)?.chars().collect();
            let (start, length) = (numeric(1)? as i64 - 1, numeric(2)? as i64);
            let (Ok(start), Ok(length)) = (usize::try_from(start), usize::try_from(length)) else {
                return Ok(Value::Error);
            };
            Ok(Value::Text(
                source.iter().skip(start).take(length).collect(),
            ))
        }
        88 => num(numbers(&arguments)?
            .into_iter()
            .reduce(f64::min)
            .unwrap_or(0.0)),
        92 => {
            let (dividend, divisor) = (numeric(0)?, numeric(1)?);
            if divisor == 0.0 {
                Ok(Value::Error)
            } else {
                num(dividend - divisor * (dividend / divisor).floor())
            }
        }
        96 => Ok(Value::Bool(!truth(&scalar(argument(0)?)?)?)),
        97 => Ok(Value::Date),
        102 => Ok(Value::Bool(
            flattened(&arguments)?
                .iter()
                .map(truth)
                .collect::<Evaluation<Vec<_>>>()?
                .into_iter()
                .any(|flag| flag),
        )),
        104 => num(std::f64::consts::PI),
        107 => num(numeric(0)?.powf(numeric(1)?)),
        113 => num(numbers(&arguments)?.iter().product()),
        117 => num(numeric(0)?.to_radians()),
        126 => num(round_to(numeric(0)?, optional(1, 0.0)? as i32, false)),
        127 => num(round_to(numeric(0)?, optional(1, 0.0)? as i32, true)),
        128 => {
            // ROUNDUP: away from zero.
            let (value, places) = (numeric(0)?, optional(1, 0.0)? as i32);
            let rounded = round_to(value, places, false);
            num(if rounded.abs() < value.abs() {
                rounded + value.signum() * 10f64.powi(-places)
            } else {
                rounded
            })
        }
        133 => num(if numeric(0)? == 0.0 {
            0.0
        } else {
            numeric(0)?.signum()
        }),
        134 => num(numeric(0)?.sin()),
        135 => num(numeric(0)?.sinh()),
        139 => {
            let value = numeric(0)?;
            if value < 0.0 {
                Ok(Value::Error)
            } else {
                num(value.sqrt())
            }
        }
        145 => {
            let sum_range = if arguments.len() > 2 {
                argument(2)?
            } else {
                argument(0)?
            };
            sum_ifs(&sum_range, &[(argument(0)?, argument(1)?)])
        }
        147 => num(numbers(&arguments)?.iter().map(|value| value * value).sum()),
        150 => num(numeric(0)?.tan()),
        151 => num(numeric(0)?.tanh()),
        156 => Ok(Value::Bool(true)),
        157 => num(numeric(0)?.trunc()),
        158 => Ok(Value::Text(textual(0)?.to_uppercase())),
        168 => num(numbers(&arguments)?.iter().sum()),
        175 => argument(0), // the chart mediator's series wrapper
        219 => num((numeric(0)? * std::f64::consts::PI).sqrt()),
        231 => {
            let pairs: Vec<(Value, Value)> = arguments[1..]
                .chunks_exact(2)
                .map(|pair| (pair[0].clone(), pair[1].clone()))
                .collect();
            sum_ifs(&argument(0)?, &pairs)
        }
        233 => {
            // COUNTIFS
            let tests = arguments
                .chunks_exact(2)
                .map(|pair| Ok((condition_cells(&pair[0])?, criterion(&pair[1])?)))
                .collect::<Evaluation<Vec<_>>>()?;
            let count = tests.first().map_or(0, |(cells, _)| cells.len());
            num((0..count)
                .filter(|&position| {
                    tests
                        .iter()
                        .all(|(cells, test)| cells.get(position).is_some_and(test))
                })
                .count() as f64)
        }
        235 => match scalar(argument(0)?)? {
            Value::Error => scalar(argument(1)?),
            value => Ok(value),
        },
        245 => {
            num(factorial(numeric(0)?.floor())
                / factorial(numeric(0)?.floor() - numeric(1)?.floor()))
        }
        306 => Ok(Value::Bool(scalar(argument(0)?)? == Value::Date)),
        374 => num(std::f64::consts::FRAC_PI_2 - numeric(0)?.atan()),
        375 => num(0.5 * ((numeric(0)? + 1.0) / (numeric(0)? - 1.0)).ln()),
        376 => num(1.0 / numeric(0)?.tan()),
        377 => num(1.0 / numeric(0)?.tanh()),
        378 => num(1.0 / numeric(0)?.sin()),
        379 => num(1.0 / numeric(0)?.sinh()),
        380 => num(1.0 / numeric(0)?.cos()),
        381 => num(1.0 / numeric(0)?.cosh()),
        382 => {
            let flags = flattened(&arguments)?
                .iter()
                .map(truth)
                .collect::<Evaluation<Vec<_>>>()?;
            Ok(Value::Bool(
                flags.into_iter().filter(|flag| *flag).count() % 2 == 1,
            ))
        }
        383 => num(combinations(
            numeric(0)?.floor() + numeric(1)?.floor() - 1.0,
            numeric(1)?.floor(),
        )),
        384 => num(gamma(numeric(0)?)),
        386 => num(0.5 * ((1.0 + numeric(0)?) / (1.0 - numeric(0)?)).ln()),
        387 => num(numeric(0)?.tanh()),
        388 => num(numeric(0)?.floor().powf(numeric(1)?.floor())),
        389 => num(numeric(0)?.floor() - numeric(1)?.floor()),
        390 | 391 => {
            let (xs, ys) = (numbers(&[argument(0)?])?, numbers(&[argument(1)?])?);
            match correlation(&xs, &ys) {
                Some(rho) => num(if index == 390 { rho } else { rho * rho }),
                None => Ok(Value::Error),
            }
        }
        other => unsupported(format!("function {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(index: i32, arguments: Vec<Value<'static>>) -> Value<'static> {
        call(index, arguments).expect("supported")
    }

    #[test]
    fn arithmetic_and_comparison() {
        assert_eq!(
            binary(3, Value::Number(2.0), Value::Number(4.0)),
            Ok(Value::Number(8.0))
        );
        assert_eq!(
            binary(4, Value::Number(1.0), Value::Number(0.0)),
            Ok(Value::Error)
        );
        assert_eq!(
            binary(11, Value::Text("A".into()), Value::Text("a".into())),
            Ok(Value::Bool(true))
        );
        assert_eq!(
            binary(9, Value::Empty, Value::Number(1.0)),
            Ok(Value::Bool(true))
        );
    }

    #[test]
    fn common_functions() {
        let numbers = vec![Value::Number(1.0), Value::Number(2.0), Value::Number(6.0)];
        assert_eq!(run(168, numbers.clone()), Value::Number(9.0));
        assert_eq!(run(15, numbers.clone()), Value::Number(3.0));
        assert_eq!(run(84, numbers), Value::Number(6.0));
        assert_eq!(
            run(126, vec![Value::Number(2.5), Value::Number(0.0)]),
            Value::Number(3.0)
        );
        assert_eq!(
            run(128, vec![Value::Number(1.21), Value::Number(1.0)]),
            Value::Number(1.3)
        );
        assert_eq!(
            run(
                87,
                vec![
                    Value::Text("abcdef".into()),
                    Value::Number(2.0),
                    Value::Number(3.0)
                ]
            ),
            Value::Text("bcd".into())
        );
        assert_eq!(
            run(
                131,
                vec![Value::Text("C".into()), Value::Text("abc".into())]
            ),
            Value::Number(3.0)
        );
        assert_eq!(
            run(62, vec![Value::Bool(false), Value::Number(1.0)]),
            Value::Bool(false)
        );
        assert_eq!(
            run(235, vec![Value::Error, Value::Number(7.0)]),
            Value::Number(7.0)
        );
        assert_eq!(run(168, vec![Value::Error]), Value::Error);
        assert!(call(9999, vec![]).is_err());
    }

    #[test]
    fn criteria() {
        let at_least_five = criterion(&Value::Text(">=5".into())).expect("criterion");
        assert!(at_least_five(&Value::Number(5.0)));
        assert!(!at_least_five(&Value::Text("x".into())));
        let apples = criterion(&Value::Text("Apples".into())).expect("criterion");
        assert!(apples(&Value::Text("apples".into())));
    }

    #[test]
    fn numeric_helpers() {
        assert!((gamma(5.0) - 24.0).abs() < 1e-9);
        assert_eq!(combinations(5.0, 2.0), 10.0);
        assert_eq!(correlation(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]), Some(1.0));
        assert_eq!(zigzag(3), -2);
    }

    #[test]
    fn uuid_forms() {
        let id = Proto::new(Rc::new(vec![0x10, 1]), 0, 2);
        let keys = uuid_keys(&id);
        assert_eq!(keys[0], "00000001000000000000000000000000");
        assert_eq!(normalise_id("ab-cd"), "ABCD");
    }
}
