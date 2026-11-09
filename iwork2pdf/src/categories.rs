//! Table categories (TST.GroupByArchive linked by TableInfo.group_by_uuid): Numbers shows a 50pt
//! label column in front, and before each group a summary row carrying the group value, with the
//! group's rows gathered under it.

use crate::cell_format;
use crate::iwa::Proto;
use crate::numbers_table::{aggregate_name, display, Categories, NumbersTable};
use std::collections::{HashMap, HashSet};

/// The rows of a grouped table in display order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// For each displayed row: the original row, or `None` for an inserted category row.
    pub order: Vec<Option<usize>>,
    /// The inserted category rows, by displayed row.
    pub categories: Categories,
}

/// One inserted category row.
#[derive(Debug, Clone, PartialEq)]
struct CategoryRow {
    label: String,
    level: usize,
    count: usize,
    summary_label: bool,
}

/// A "count (all)" summary: the column it appears in and its group level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Summary {
    column: usize,
    level: usize,
    aggregate: i32,
}

/// Builds the grouped row order, or `None` when the table is not grouped.
///
/// # Arguments
///
/// * `table` - The table as loaded (before the label column is added)
///
/// # Returns
///
/// The display order and the category rows.
pub fn plan(table: &NumbersTable) -> Option<Plan> {
    let wanted = table.info.message(7);
    if wanted.is_empty() {
        return None;
    }
    let doc = &table.doc;
    let same_uuid = |body: &Proto| {
        body.message(1).uint(1) == wanted.uint(1) && body.message(1).uint(2) == wanted.uint(2)
    };
    let group_by = doc
        .of_type(6373)
        .into_iter()
        .map(|object| object.body.clone())
        .find(same_uuid)?;
    if !group_by.boolean(6) || !group_by.has(2) {
        return None;
    }
    let root = group_by
        .message_opt(3)
        .or_else(|| doc.body(group_by.reference(18)))?;
    if root.messages(3).is_empty() && !root.has(10) {
        return None;
    }
    let mut walker = Walker::new(table, &group_by);
    walker.order.extend((0..table.header_rows).map(Some));
    walker.walk(&root, 0);
    let unused: Vec<usize> = (table.header_rows..table.rows)
        .filter(|row| !walker.used.contains(row))
        .collect();
    walker.order.extend(unused.into_iter().map(Some));
    Some(walker.finish())
}

/// Walks the group tree, emitting category rows and the grouped rows in order.
struct Walker<'a> {
    table: &'a NumbersTable,
    /// Group-lookup index → row UID.
    lookup: Vec<(u64, u64)>,
    row_of: HashMap<(u64, u64), usize>,
    column_of: HashMap<(u64, u64), usize>,
    group_columns: Vec<Proto>,
    summaries: Vec<Summary>,
    order: Vec<Option<usize>>,
    rows: Vec<CategoryRow>,
    /// Row index in `order` → index in `rows`, for category rows.
    category_at: HashMap<usize, usize>,
    used: HashSet<usize>,
    label_rows_done: HashSet<usize>,
}

impl<'a> Walker<'a> {
    fn new(table: &'a NumbersTable, group_by: &Proto) -> Walker<'a> {
        let (column_of, row_of) = table.uid_indexes();
        // Summaries (column_agg_type: column, level 1-based, aggregate). Only "count (all)" (1) is
        // implemented: the group's row count, hidden rows included, shown in its category row,
        // with one label row ("Kind:" … "Count (all):") above the first group of that level.
        let summaries = group_by
            .messages(5)
            .iter()
            .filter_map(|summary| {
                let uid = summary.message(1);
                let column = *column_of.get(&(uid.uint(1), uid.uint(2)))?;
                let level = usize::try_from(summary.int(2) - 1).ok()?;
                Some(Summary {
                    column,
                    level,
                    aggregate: summary.int(3),
                })
                .filter(|summary| summary.aggregate == 1)
            })
            .collect();
        Walker {
            table,
            lookup: group_by
                .message(15)
                .messages(1)
                .iter()
                .map(|uid| (uid.uint(1), uid.uint(2)))
                .collect(),
            row_of,
            column_of,
            group_columns: group_by.messages(2),
            summaries,
            order: Vec::new(),
            rows: Vec::new(),
            category_at: HashMap::new(),
            used: HashSet::new(),
            label_rows_done: HashSet::new(),
        }
    }

    fn row_for(&self, lookup_index: usize) -> usize {
        self.lookup
            .get(lookup_index)
            .and_then(|uid| self.row_of.get(uid).copied())
            .unwrap_or(lookup_index)
    }

    fn children(&self, node: &Proto) -> Vec<Proto> {
        let inline = node.messages(3);
        if !inline.is_empty() {
            return inline;
        }
        node.references(10)
            .into_iter()
            .filter_map(|id| self.table.doc.body(id))
            .collect()
    }

    /// The original rows a group node holds (ranges of lookup indexes).
    fn node_rows(&self, node: &Proto) -> Vec<usize> {
        let ranges = node.message(9).messages(1);
        let lookup_indexes = ranges.iter().flat_map(|range| {
            let begin = range.int(1);
            let end = if range.has(2) { range.int(2) } else { begin };
            (begin..=end).filter_map(|index| usize::try_from(index).ok())
        });
        lookup_indexes.map(|index| self.row_for(index)).collect()
    }

    /// The header text of the column grouping a level.
    fn group_header(&self, level: usize) -> String {
        let column = self.group_columns.get(level).and_then(|group| {
            let uid = group.message(1);
            self.column_of.get(&(uid.uint(1), uid.uint(2))).copied()
        });
        column
            .and_then(|column| display(self.table, (0, column)))
            .unwrap_or_default()
    }

    /// A group's label: its value formatted as Numbers shows it.
    fn label(&self, node: &Proto, level: usize) -> String {
        let value = node.message(7);
        match value.int(1) {
            4 => cell_format::number(
                value.message(4).double(1),
                value.message(4).message_opt(3).as_ref(),
                false,
            ),
            3 => {
                // Date buckets carry their own pattern ("yyyy-QQQ", "MMMM yyyy", ...).
                let format = node
                    .message(6)
                    .messages(2)
                    .into_iter()
                    .next()
                    .or_else(|| value.message(3).message_opt(2));
                cell_format::date(value.message(3).double(1), format.as_ref())
            }
            // Boolean groups read Not “Header” / Is “Header” (measured).
            2 => {
                let verdict = if value.message(2).boolean(1) {
                    "Is"
                } else {
                    "Not"
                };
                format!("{verdict} “{}”", self.group_header(level))
            }
            _ => value.message(5).string(1).unwrap_or_default(),
        }
    }

    fn push_category(&mut self, row: CategoryRow) {
        self.category_at.insert(self.order.len(), self.rows.len());
        self.order.push(None);
        self.rows.push(row);
    }

    /// Emits a level's groups in the order of their first row in the table.
    fn walk(&mut self, node: &Proto, level: usize) {
        let mut children: Vec<(usize, Proto)> = self
            .children(node)
            .into_iter()
            .map(|child| {
                (
                    self.node_rows(&child)
                        .into_iter()
                        .min()
                        .unwrap_or(usize::MAX),
                    child,
                )
            })
            .collect();
        children.sort_by_key(|(first_row, _)| *first_row);
        for (_, child) in children {
            if self.summaries.iter().any(|summary| summary.level == level)
                && self.label_rows_done.insert(level)
            {
                let label = format!("{}:", self.group_header(level));
                self.push_category(CategoryRow {
                    label,
                    level,
                    count: 0,
                    summary_label: true,
                });
            }
            let rows = self.node_rows(&child);
            let label = self.label(&child, level);
            self.push_category(CategoryRow {
                label,
                level,
                count: rows.len(),
                summary_label: false,
            });
            if !self.children(&child).is_empty() {
                self.walk(&child, level + 1);
                continue;
            }
            for row in rows {
                if row >= self.table.header_rows && row < self.table.rows && self.used.insert(row) {
                    self.order.push(Some(row));
                }
            }
        }
    }

    fn finish(self) -> Plan {
        let mut categories = Categories::default();
        for (&display_row, &index) in &self.category_at {
            let category = &self.rows[index];
            categories
                .labels
                .insert(display_row, category.label.clone());
            categories.levels.insert(display_row, category.level);
            if category.summary_label {
                categories.summary_label_rows.insert(display_row);
            }
            for summary in self
                .summaries
                .iter()
                .filter(|summary| summary.level == category.level)
            {
                let value = if category.summary_label {
                    format!("{}:", aggregate_name(&self.table.doc, summary.aggregate))
                } else {
                    category.count.to_string()
                };
                // Values sit in the summarised column, shifted by the inserted label column.
                categories
                    .values
                    .insert((display_row, summary.column + 1), value);
            }
        }
        Plan {
            order: self.order,
            categories,
        }
    }
}
