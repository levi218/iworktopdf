//! Turns a cell value into the string Numbers displays (TSK.FormatStructArchive).
//!
//! Number formatting reproduces .NET's: values are first taken to 15 significant digits, then
//! rounded half away from zero to the format's decimals (the C# converter's output, which matched
//! Apple's exports, depends on it).

use crate::iwa::{IwaDocument, Proto};
use crate::numbers_table::{Cell, CellKind, NumbersTable};
use crate::styles::ctx;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};

// FormatStructArchive.format_type values.
/// Decimal number.
pub const DECIMAL: i32 = 256;
/// Currency.
pub const CURRENCY: i32 = 257;
/// Percentage.
pub const PERCENT: i32 = 258;
/// Scientific notation.
pub const SCIENTIFIC: i32 = 259;
/// Text.
pub const TEXT: i32 = 260;
/// Date and time.
pub const DATE_TIME: i32 = 261;
/// Fraction.
pub const FRACTION: i32 = 262;
/// Checkbox control.
pub const CHECKBOX: i32 = 263;
/// Stepper control.
pub const STEPPER: i32 = 264;
/// Slider control.
pub const SLIDER: i32 = 265;
/// Pop-up menu control.
pub const POPUP: i32 = 266;
/// Star rating control.
pub const RATING: i32 = 267;
/// Duration.
pub const DURATION: i32 = 268;
/// Numeral system (base).
pub const BASE: i32 = 269;
/// Custom number format.
pub const CUSTOM_NUMBER: i32 = 270;
/// Custom text format.
pub const CUSTOM_TEXT: i32 = 271;
/// Custom date format.
pub const CUSTOM_DATE: i32 = 272;
/// Decimal places "auto".
const AUTO_DECIMALS: i32 = 253;
/// Fraction accuracy "up to three digits" (0xFFFFFFFD as a signed int).
const FRACTION_THREE_DIGITS: i32 = -3;

/// The format that applies to a cell, custom formats resolved.
///
/// # Arguments
///
/// * `table` - The cell's table
/// * `cell` - The cell
///
/// # Returns
///
/// The TSK.FormatStructArchive, if the cell has one.
pub fn format_of(table: &NumbersTable, cell: &Cell) -> Option<Proto> {
    let fallback = |key: u32, other: u32| if key != 0 { key } else { other };
    let key = match cell.kind {
        CellKind::Currency => fallback(cell.currency_format_key, cell.number_format_key),
        CellKind::Date => cell.date_format_key,
        CellKind::Duration => cell.duration_format_key,
        CellKind::Text => cell.text_format_key,
        CellKind::Bool => fallback(cell.bool_format_key, cell.number_format_key),
        _ => cell.number_format_key,
    };
    let mut format = table.format(key, cell.pre_bnc);
    if format.is_none() && cell.kind == CellKind::Number && !cell.pre_bnc {
        format = table
            .format(cell.currency_format_key, false)
            .or_else(|| table.format(cell.duration_format_key, false));
    }
    format.map(|format| resolve_custom(&table.doc, format))
}

/// Custom formats are referenced by UUID into the document's TSK.CustomFormatListArchive.
fn resolve_custom(doc: &IwaDocument, format: Proto) -> Proto {
    if !matches!(format.int(1), CUSTOM_NUMBER | CUSTOM_TEXT | CUSTOM_DATE) {
        return format;
    }
    if let Some(embedded) = format.message_opt(42) {
        return embedded.message(3);
    }
    let uuid = format.message(41);
    if uuid.is_empty() {
        return format;
    }
    let (low, high) = (uuid.uint(1), uuid.uint(2));
    doc.of_type(222)
        .into_iter()
        .find_map(|list| {
            let uuids = list.body.messages(1);
            let formats = list.body.messages(2);
            uuids
                .iter()
                .zip(formats)
                .find(|(id, _)| id.uint(1) == low && id.uint(2) == high)
                .map(|(_, custom)| custom.message(3))
        })
        .unwrap_or(format)
}

/// The text a cell displays, or `None` when it shows none (errors, controls, empty cells).
///
/// # Arguments
///
/// * `table` - The cell's table
/// * `cell` - The cell
///
/// # Returns
///
/// The displayed text.
pub fn display(table: &NumbersTable, cell: &Cell) -> Option<String> {
    let format = format_of(table, cell);
    let format_type = format.as_ref().map_or(0, |format| format.int(1));
    match cell.kind {
        CellKind::Text => cell.text.clone(),
        // Apple's PDF export leaves error cells blank (measured: #DIV/0, deleted references,
        // unknown functions and circular references all export empty).
        CellKind::Error => None,
        CellKind::Bool => {
            if format_type == CHECKBOX {
                return None;
            }
            let checked = cell.number != 0.0;
            match format.filter(|format| format.has(44)) {
                Some(format) => format.string(if checked { 44 } else { 45 }),
                None => Some(if checked { "TRUE" } else { "FALSE" }.into()),
            }
        }
        CellKind::Date => Some(date(cell.number, format.as_ref())),
        CellKind::Duration => Some(duration_text(cell.number, format.as_ref())),
        CellKind::Number | CellKind::Currency => {
            if matches!(format_type, CHECKBOX | RATING | SLIDER) {
                return None;
            }
            if format_type == POPUP {
                if let Some(item) = format
                    .as_ref()
                    .and_then(|format| popup_item(table, format, cell.number))
                {
                    return Some(item);
                }
            }
            Some(number(
                cell.number,
                format.as_ref(),
                cell.kind == CellKind::Currency,
            ))
        }
        CellKind::Empty | CellKind::RichText => None,
    }
}

/// The pop-up menu item of a cell whose value is the item index: multiple_choice_list_id (39)
/// keys the data store's multipleChoiceListFormatTable (16), whose TST.PopUpMenuModel lists the
/// items as TSCE.CellValueArchive (item 0 is NIL).
fn popup_value(table: &NumbersTable, format: &Proto, index: f64) -> Option<Proto> {
    if !format.has(39) || index < 0.0 || index.fract() != 0.0 {
        return None;
    }
    let lists = table.list(table.model.message(4).reference(16));
    let entry = lists.get(&(format.uint(39) as u32))?;
    table
        .doc
        .body(entry.reference(4))?
        .messages(2)
        .into_iter()
        .nth(index as usize)
}

/// True when a pop-up cell shows a text item (left-aligned like text).
pub fn is_text_popup(table: &NumbersTable, cell: &Cell) -> bool {
    let format = format_of(table, cell).filter(|format| format.int(1) == POPUP);
    format
        .and_then(|format| popup_value(table, &format, cell.number))
        .is_some_and(|value| value.int(1) == 5)
}

fn popup_item(table: &NumbersTable, format: &Proto, index: f64) -> Option<String> {
    let value = popup_value(table, format, index)?;
    match value.int(1) {
        5 => value.message(5).string(1),
        4 => Some(number(
            value.message(4).double(1),
            value.message(4).message_opt(3).as_ref(),
            false,
        )),
        1 => Some(String::new()),
        _ => None,
    }
}

// ---------------------------------------------------------------- decimal digits

/// A positive value's decimal digits (no leading zeros) and the exponent of the first digit:
/// value ≈ 0.d1d2d3… × 10^(exponent + 1).
#[derive(Debug, Clone, PartialEq)]
struct Digits {
    digits: Vec<u8>,
    exponent: i32,
}

impl Digits {
    /// The value at 15 significant digits (.NET's precision for double formatting).
    fn of(value: f64) -> Digits {
        let text = format!("{:.14e}", value.abs());
        let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
        let digits: Vec<u8> = mantissa
            .bytes()
            .filter(u8::is_ascii_digit)
            .map(|byte| byte - b'0')
            .collect();
        Digits {
            digits,
            exponent: exponent.parse().unwrap_or(0),
        }
        .trimmed()
    }

    fn trimmed(mut self) -> Digits {
        while self.digits.last() == Some(&0) {
            self.digits.pop();
        }
        if self.digits.is_empty() {
            self.exponent = 0;
        }
        self
    }

    fn is_zero(&self) -> bool {
        self.digits.is_empty()
    }

    /// Rounded half away from zero to keep `count` digits in total.
    fn rounded_to(&self, count: i32) -> Digits {
        // Rounding more than one place left of the first digit leaves nothing.
        let Ok(keep) = usize::try_from(count) else {
            return Digits {
                digits: vec![],
                exponent: 0,
            };
        };
        if keep >= self.digits.len() {
            return self.clone();
        }
        let mut digits = self.digits[..keep].to_vec();
        let mut exponent = self.exponent;
        if self.digits[keep] >= 5 {
            // Propagate the carry; a carry out of the first digit adds a leading 1.
            let mut index = keep;
            loop {
                if index == 0 {
                    digits.insert(0, 1);
                    exponent += 1;
                    break;
                }
                index -= 1;
                if digits[index] == 9 {
                    digits[index] = 0;
                } else {
                    digits[index] += 1;
                    break;
                }
            }
        }
        Digits { digits, exponent }.trimmed()
    }

    /// Rounded to `decimals` places after the point.
    fn rounded_to_decimals(&self, decimals: usize) -> Digits {
        self.rounded_to(self.exponent + 1 + decimals as i32)
    }

    /// (integer part, fractional part) as digit strings ("0" for a zero integer part).
    fn split(&self) -> (String, String) {
        let digit = |index: i32| {
            usize::try_from(index)
                .ok()
                .and_then(|index| self.digits.get(index))
                .map_or('0', |digit| char::from(b'0' + digit))
        };
        let integer: String = if self.exponent < 0 {
            "0".into()
        } else {
            (0..=self.exponent).map(digit).collect()
        };
        let fraction_len = self.digits.len() as i32 - self.exponent - 1;
        let fraction: String = (0..fraction_len.max(0))
            .map(|index| digit(self.exponent + 1 + index))
            .collect();
        (integer, fraction)
    }
}

/// Inserts thousands separators into an integer digit string.
fn group_thousands(integer: &str) -> String {
    let mut grouped = String::with_capacity(integer.len() + integer.len() / 3);
    for (index, digit) in integer.chars().enumerate() {
        if index > 0 && (integer.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// A numeric pattern like .NET's "#,##0.00##": decimals between `min_decimals` and
/// `max_decimals`, at least `min_integer` integer digits (0: a zero integer part is left out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pattern {
    min_integer: usize,
    group: bool,
    min_decimals: usize,
    max_decimals: usize,
}

impl Pattern {
    fn fixed(decimals: usize, group: bool) -> Pattern {
        Pattern {
            min_integer: 1,
            group,
            min_decimals: decimals,
            max_decimals: decimals,
        }
    }

    /// Formats the absolute value of `value`.
    fn apply(&self, value: f64) -> String {
        let rounded = Digits::of(value).rounded_to_decimals(self.max_decimals);
        let (integer, mut fraction) = rounded.split();
        let integer = integer.trim_start_matches('0');
        let integer = format!("{integer:0>width$}", width = self.min_integer);
        let integer = if self.group {
            group_thousands(&integer)
        } else {
            integer
        };
        while fraction.len() > self.min_decimals && fraction.ends_with('0') {
            fraction.pop();
        }
        while fraction.len() < self.min_decimals {
            fraction.push('0');
        }
        if fraction.is_empty() {
            integer
        } else {
            format!("{integer}.{fraction}")
        }
    }
}

/// True when a formatted body shows a non-zero digit (a negative value rounding to zero shows no
/// minus sign).
fn shows_nonzero(body: &str) -> bool {
    body.bytes().any(|byte| (b'1'..=b'9').contains(&byte))
}

// ---------------------------------------------------------------- numbers

/// Formats a number with a cell format.
///
/// # Arguments
///
/// * `value` - The number
/// * `format` - The format (`None`: automatic)
/// * `currency` - The cell is a currency cell (automatic format shows US dollars)
///
/// # Returns
///
/// The displayed text.
pub fn number(value: f64, format: Option<&Proto>, currency: bool) -> String {
    let format_type = format.map_or(0, |format| format.int(1));
    let Some(format) = format.filter(|_| !matches!(format_type, 0 | 1 | TEXT | POPUP | STEPPER))
    else {
        return if currency {
            currency_text(value, &CurrencyFormat::automatic())
        } else {
            general(value)
        };
    };
    let decimals = format.int_or(2, AUTO_DECIMALS);
    let explicit = |default: usize| {
        if decimals == AUTO_DECIMALS {
            default
        } else {
            usize::try_from(decimals).unwrap_or(default)
        }
    };
    let separator = format.boolean(5);
    let negative_style = format.int(4);
    match format_type {
        DECIMAL => fixed(value, decimals, separator, negative_style),
        CURRENCY => {
            let currency = CurrencyFormat {
                decimals: explicit(2),
                code: format.string(3).unwrap_or_else(|| "USD".into()),
                separator,
                negative_style,
                accounting: format.boolean(6),
            };
            currency_text(value, &currency)
        }
        PERCENT => fixed(value * 100.0, decimals, separator, negative_style) + "%",
        SCIENTIFIC => scientific(value, explicit(2)),
        FRACTION => fraction_text(value, format.int_or(11, FRACTION_THREE_DIGITS)),
        DURATION => duration_text(value, Some(format)),
        BASE => base_text(value, format.int_or(8, 10), format.int(9)),
        CUSTOM_NUMBER => custom_number_text(value, format),
        DATE_TIME | CUSTOM_DATE => date(value, Some(format)),
        _ => general(value),
    }
}

/// Numbers' "Automatic" format: 15 significant digits, no grouping; scientific outside
/// [1e-9, 1e15).
pub fn general(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let magnitude = value.abs();
    let digits = Digits::of(magnitude);
    if !(1e-9..1e15).contains(&magnitude) {
        // .NET "0.##############E+0": one integer digit, up to 14 decimals.
        let mantissa: String = digits
            .digits
            .iter()
            .map(|digit| char::from(b'0' + digit))
            .collect();
        let (first, rest) = mantissa.split_at(1);
        let point = if rest.is_empty() { "" } else { "." };
        let exponent_sign = if digits.exponent < 0 { "-" } else { "+" };
        return format!(
            "{sign}{first}{point}{rest}E{exponent_sign}{}",
            digits.exponent.abs()
        );
    }
    let (integer, fraction) = digits.split();
    if fraction.is_empty() {
        format!("{sign}{integer}")
    } else {
        format!("{sign}{integer}.{fraction}")
    }
}

fn fixed(value: f64, decimals: i32, separator: bool, negative_style: i32) -> String {
    let body = if decimals == AUTO_DECIMALS {
        let body = general(value.abs());
        match (separator, body.split_once('.')) {
            _ if !separator || body.contains('E') => body,
            (_, Some((integer, fraction))) => format!("{}.{fraction}", group_thousands(integer)),
            (_, None) => group_thousands(&body),
        }
    } else {
        Pattern::fixed(usize::try_from(decimals).unwrap_or(0), separator).apply(value)
    };
    negate(body, value < 0.0, negative_style)
}

/// negative_style: 0 "-1", 1 red "1" (minus kept), 2 "(1)", 3 red "(1)".
fn negate(body: String, negative: bool, style: i32) -> String {
    if !negative || !shows_nonzero(&body) {
        return body;
    }
    match style {
        2 | 3 => format!("({body})"),
        _ => format!("-{body}"),
    }
}

/// A currency format's settings.
#[derive(Debug, Clone, PartialEq)]
struct CurrencyFormat {
    decimals: usize,
    code: String,
    separator: bool,
    negative_style: i32,
    accounting: bool,
}

impl CurrencyFormat {
    /// Numbers' automatic currency: US dollars, two decimals, grouped.
    fn automatic() -> CurrencyFormat {
        CurrencyFormat {
            decimals: 2,
            code: "USD".into(),
            separator: true,
            negative_style: 0,
            accounting: false,
        }
    }
}

fn currency_symbol(code: &str) -> String {
    let symbol = match code {
        "USD" => "$",
        "GBP" => "£",
        "EUR" => "€",
        "JPY" => "¥",
        "CNY" => "CN¥",
        "INR" => "₹",
        "KRW" => "₩",
        "CAD" => "CA$",
        "AUD" => "A$",
        "CHF" | "RUB" | "SEK" | "NOK" | "DKK" => code,
        other => return format!("{other} "),
    };
    // Outside US-English locales the dollar is written "US$" (measured: an en_GB document).
    let foreign = ctx::LOCALE.with(|locale| {
        locale
            .borrow()
            .as_deref()
            .is_some_and(|locale| !locale.is_empty() && !locale.starts_with("en_US"))
    });
    if code == "USD" && foreign {
        "US$".into()
    } else {
        symbol.into()
    }
}

fn currency_text(value: f64, currency: &CurrencyFormat) -> String {
    let symbol = currency_symbol(&currency.code);
    let body = Pattern::fixed(currency.decimals, currency.separator).apply(value);
    let negative = value < 0.0 && shows_nonzero(&body);
    if currency.accounting {
        // Accounting style: the symbol at the cell's left edge, the amount right-aligned.
        return if negative {
            format!("{symbol}\t({body})")
        } else {
            format!("{symbol}\t{body}")
        };
    }
    match (negative, currency.negative_style) {
        (false, _) => format!("{symbol}{body}"),
        (true, 2 | 3) => format!("({symbol}{body})"),
        (true, _) => format!("-{symbol}{body}"),
    }
}

/// .NET "0.00E+00": the mantissa at `decimals` places, a signed exponent of at least two digits.
fn scientific(value: f64, decimals: usize) -> String {
    let sign = if value < 0.0 { "-" } else { "" };
    let digits = Digits::of(value);
    if digits.is_zero() {
        let zeros = "0".repeat(decimals);
        let point = if decimals > 0 { "." } else { "" };
        return format!("0{point}{zeros}E+00");
    }
    let rounded = digits.rounded_to(decimals as i32 + 1);
    let mut mantissa: Vec<char> = rounded
        .digits
        .iter()
        .map(|digit| char::from(b'0' + digit))
        .collect();
    mantissa.resize(decimals + 1, '0');
    let (first, rest): (String, String) = (
        mantissa[..1].iter().collect(),
        mantissa[1..].iter().collect(),
    );
    let point = if decimals > 0 { "." } else { "" };
    let exponent_sign = if rounded.exponent < 0 { "-" } else { "+" };
    format!(
        "{sign}{first}{point}{rest}E{exponent_sign}{:02}",
        rounded.exponent.abs()
    )
}

/// accuracy: up to 1, 2 or 3 digits (-1, -2, -3) or a fixed denominator.
fn fraction_text(value: f64, accuracy: i32) -> String {
    let magnitude = value.abs();
    let mut whole = magnitude.floor() as i64;
    let fraction = magnitude - whole as f64;
    let (mut numerator, denominator) = if (-3..=-1).contains(&accuracy) {
        let max_denominator = [9, 99, 999][(-accuracy - 1) as usize];
        let mut best = (0i64, 1i64, f64::MAX);
        for denominator in 1..=max_denominator {
            let numerator = (fraction * denominator as f64).round();
            let error = (fraction - numerator / denominator as f64).abs();
            if error < best.2 - 1e-12 {
                best = (numerator as i64, denominator, error);
            }
        }
        (best.0, best.1)
    } else {
        let denominator = i64::from(accuracy.max(1));
        ((fraction * denominator as f64).round() as i64, denominator)
    };
    if numerator == denominator {
        whole += 1;
        numerator = 0;
    }
    let sign = if value < 0.0 { "-" } else { "" };
    match (numerator, whole) {
        (0, _) => format!("{sign}{whole}"),
        (_, 0) => format!("{sign}{numerator}/{denominator}"),
        _ => format!("{sign}{whole} {numerator}/{denominator}"),
    }
}

/// Numeral system: base 2, 8, 10 or 16 (others shown in base 10), zero-padded to `places`.
fn base_text(value: f64, base: i32, places: i32) -> String {
    let rounded = value.round() as i64;
    let magnitude = rounded.unsigned_abs();
    let digits = match base.clamp(2, 16) {
        2 => format!("{magnitude:b}"),
        8 => format!("{magnitude:o}"),
        16 => format!("{magnitude:X}"),
        _ => magnitude.to_string(),
    };
    let width = usize::try_from(places).unwrap_or(0);
    format!("{}{digits:0>width$}", if rounded < 0 { "-" } else { "" })
}

// ----------------------------------------------------------------- dates

/// The 2001-01-01 epoch Numbers counts dates from.
fn epoch() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2001, 1, 1)
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .unwrap_or_default()
}

/// Formats a date (seconds since 2001-01-01) with the format's ICU pattern (default "d MMM yyyy").
///
/// # Arguments
///
/// * `seconds` - Seconds since 2001-01-01 00:00
/// * `format` - The date format (pattern in field 14, or 18 for custom formats)
///
/// # Returns
///
/// The formatted date.
pub fn date(seconds: f64, format: Option<&Proto>) -> String {
    // Like .NET's DateTime.AddSeconds, to the millisecond.
    let moment = epoch() + Duration::milliseconds((seconds * 1000.0).round() as i64);
    let pattern = [14, 18].iter().find_map(|&field| {
        format
            .and_then(|format| format.string(field))
            .filter(|pattern| !pattern.is_empty())
    });
    icu(moment, pattern.as_deref().unwrap_or("d MMM yyyy"))
}

/// Formats a date with an ICU/Unicode date pattern (what Numbers stores).
///
/// # Arguments
///
/// * `moment` - The date and time
/// * `pattern` - The pattern ('quoted' literals, runs of letters as fields)
///
/// # Returns
///
/// The formatted text.
pub fn icu(moment: NaiveDateTime, pattern: &str) -> String {
    let characters: Vec<char> = pattern.chars().collect();
    let mut output = String::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if character == '\'' {
            // '' is a literal quote; otherwise everything up to the closing quote is literal.
            if characters.get(index + 1) == Some(&'\'') {
                output.push('\'');
                index += 2;
                continue;
            }
            index += 1;
            while index < characters.len() {
                if characters[index] == '\'' {
                    if characters.get(index + 1) == Some(&'\'') {
                        output.push('\'');
                        index += 2;
                        continue;
                    }
                    break;
                }
                output.push(characters[index]);
                index += 1;
            }
            index += 1;
            continue;
        }
        if !character.is_ascii_alphabetic() {
            output.push(character);
            index += 1;
            continue;
        }
        let run = characters[index..]
            .iter()
            .take_while(|&&next| next == character)
            .count();
        output.push_str(&date_field(moment, character, run));
        index += run;
    }
    output
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn date_field(moment: NaiveDateTime, letter: char, width: usize) -> String {
    let pad = |value: u32| format!("{value:0>width$}");
    let month_name = MONTHS[moment.month0() as usize];
    let day_name = DAYS[moment.weekday().num_days_from_sunday() as usize];
    match letter {
        'y' | 'Y' | 'u' if width == 2 => format!("{:02}", moment.year().rem_euclid(100)),
        'y' | 'Y' | 'u' => format!("{:0>width$}", moment.year()),
        'M' | 'L' => match width {
            1 => moment.month().to_string(),
            2 => format!("{:02}", moment.month()),
            3 => month_name[..3].to_string(),
            4 => month_name.to_string(),
            _ => month_name[..1].to_string(),
        },
        'd' => pad(moment.day()),
        'D' => pad(moment.ordinal()),
        'e' | 'c' if width <= 2 => pad(moment.weekday().num_days_from_sunday() + 1),
        'E' | 'e' | 'c' => match width {
            0..=3 => day_name[..3].to_string(),
            4 => day_name.to_string(),
            5 => day_name[..1].to_string(),
            _ => day_name[..2].to_string(),
        },
        'H' => pad(moment.hour()),
        'k' => pad(if moment.hour() == 0 {
            24
        } else {
            moment.hour()
        }),
        'h' => pad(moment.hour12().1),
        'K' => pad(moment.hour() % 12),
        'm' => pad(moment.minute()),
        's' => pad(moment.second()),
        'S' => {
            // Fractional seconds in 100ns ticks, cut or padded to the width.
            let ticks = format!("{:07}", moment.nanosecond() / 100);
            format!("{:0<width$}", &ticks[..width.min(7)])
        }
        'a' => {
            // British-style English locales write the day period in lower case (measured: an en_GB
            // document shows "1:01:01 am").
            let lower = ctx::LOCALE.with(|locale| {
                matches!(
                    locale.borrow().as_deref(),
                    Some("en_GB" | "en_AU" | "en_IE" | "en_NZ")
                )
            });
            let period = if moment.hour() < 12 { "AM" } else { "PM" };
            if lower {
                period.to_lowercase()
            } else {
                period.into()
            }
        }
        'w' => pad(moment.iso_week().week()),
        'W' => pad(week_of_month(moment)),
        'F' => pad((moment.day() - 1) / 7 + 1),
        'Q' | 'q' if width <= 2 => pad(moment.month0() / 3 + 1),
        'Q' | 'q' => format!("Q{}", moment.month0() / 3 + 1),
        'G' => (if moment.year() > 0 { "AD" } else { "BC" }).into(),
        'z' | 'Z' | 'v' | 'V' | 'O' | 'X' | 'x' => "GMT".into(),
        other => other.to_string().repeat(width),
    }
}

/// Week of month with ISO rules: a first partial week of fewer than 4 days is week 0.
fn week_of_month(moment: NaiveDateTime) -> u32 {
    let first = moment.date().with_day(1).unwrap_or(moment.date());
    let first_weekday = first.weekday().num_days_from_monday();
    let days_in_first_week = 7 - first_weekday;
    let first_week = u32::from(days_in_first_week >= 4);
    if moment.day() <= days_in_first_week {
        first_week
    } else {
        first_week + (moment.day() - days_in_first_week - 1) / 7 + 1
    }
}

// -------------------------------------------------------------- durations

/// Formats a duration: duration_style (7) 0 compact "1d 2h", 2 full words; units from largest (15)
/// to smallest (16) as flags 1 week, 2 day, 4 hour, 8 minute, 16 second, 32 millisecond.
///
/// # Arguments
///
/// * `seconds` - The duration
/// * `format` - The duration format (`None` or automatic: the units the value needs)
///
/// # Returns
///
/// The formatted duration.
pub fn duration_text(seconds: f64, format: Option<&Proto>) -> String {
    const UNIT_FLAGS: [i32; 6] = [1, 2, 4, 8, 16, 32];
    const UNIT_SECONDS: [f64; 6] = [604800.0, 86400.0, 3600.0, 60.0, 1.0, 0.001];
    const SHORT_NAMES: [&str; 6] = ["w", "d", "h", "m", "s", "ms"];
    const LONG_NAMES: [&str; 6] = ["week", "day", "hour", "minute", "second", "millisecond"];
    let style = format.map_or(0, |format| format.int(7));
    let (largest, smallest) = format.map_or((0, 0), |format| (format.int(15), format.int(16)));
    let automatic = format.is_none_or(|format| format.boolean(40)) || largest == 0 || smallest == 0;
    let (first, last) = if automatic {
        (0, 4)
    } else {
        let position = |flag: i32, default: usize| {
            UNIT_FLAGS
                .iter()
                .position(|&unit| unit == flag)
                .unwrap_or(default)
        };
        (position(largest, 0), position(smallest, 4))
    };
    let mut remaining = seconds.abs();
    let mut parts = Vec::new();
    for unit in first..=last {
        let amount = if unit == last {
            remaining / UNIT_SECONDS[unit]
        } else {
            (remaining / UNIT_SECONDS[unit] + 1e-9).floor()
        };
        remaining -= (amount + 1e-9).floor() * UNIT_SECONDS[unit];
        if automatic && amount == 0.0 && unit != last {
            continue;
        }
        let text = if unit == last {
            general((amount * 1e6).round() / 1e6)
        } else {
            (amount as i64).to_string()
        };
        parts.push(match style {
            2 => format!(
                "{text} {}{}",
                LONG_NAMES[unit],
                if text == "1" { "" } else { "s" }
            ),
            _ => format!("{text}{}", SHORT_NAMES[unit]),
        });
    }
    format!(
        "{}{}",
        if seconds < 0.0 { "-" } else { "" },
        parts.join(" ")
    )
}

// ---------------------------------------------------------- custom numbers

/// Custom number formats: a pattern of '#', '0', ',', '.' digit tokens with literal text, applied
/// to the value times scale_factor (19).
fn custom_number_text(value: f64, format: &Proto) -> String {
    let pattern = format.string(18).unwrap_or_default();
    let scaled = value * format.double_or(19, 1.0);
    if format.boolean(20) || pattern.contains('\u{E0A0}') {
        return fraction_text(scaled, format.int_or(11, FRACTION_THREE_DIGITS));
    }
    // Split the pattern into prefix literal, numeric token, suffix literal.
    let characters: Vec<char> = pattern.chars().collect();
    let Some(mut start) = characters
        .iter()
        .position(|&character| character == '#' || character == '0')
    else {
        return pattern;
    };
    if start > 0 && characters[start - 1] == '.' {
        start -= 1; // ".####": the point belongs to the number
    }
    let is_token = |character: char| "#0,.".contains(character);
    let end = characters
        .iter()
        .rposition(|&character| is_token(character))
        .filter(|&end| end >= start)
        .unwrap_or(start);
    let literal = |range: &[char]| {
        range
            .iter()
            .filter(|&&character| character != '\'')
            .collect::<String>()
    };
    let (prefix, suffix) = (
        literal(&characters[..start]),
        literal(&characters[end + 1..]),
    );
    let token: String = characters[start..=end].iter().collect();
    let (integer_part, decimal_part) = token.split_once('.').unwrap_or((&token, ""));
    let placeholders = |part: &str, digits: &[char]| {
        part.chars()
            .filter(|character| digits.contains(character))
            .count()
    };
    // With no integer placeholders the integer part still shows (measured: ".########" prints 0.23
    // as "0.23" and 2.34 as "2.34").
    let has_integer = placeholders(integer_part, &['0', '#']) > 0;
    let pattern = Pattern {
        min_integer: if has_integer {
            placeholders(integer_part, &['0'])
        } else {
            1
        },
        group: integer_part.contains(',') || format.boolean(5),
        min_decimals: placeholders(decimal_part, &['0']),
        max_decimals: placeholders(decimal_part, &['0', '#']),
    };
    let sign = if scaled < 0.0 { "-" } else { "" };
    format!("{sign}{prefix}{}{suffix}", pattern.apply(scaled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_round_half_away() {
        assert_eq!(Pattern::fixed(2, false).apply(2.675), "2.68");
        assert_eq!(Pattern::fixed(0, true).apply(1234567.5), "1,234,568");
        assert_eq!(Pattern::fixed(2, false).apply(0.004), "0.00");
        assert_eq!(Pattern::fixed(1, false).apply(9.96), "10.0");
        assert_eq!(Pattern::fixed(2, false).apply(0.006), "0.01");
        assert_eq!(Pattern::fixed(2, false).apply(0.0006), "0.00");
    }

    #[test]
    fn general_format() {
        assert_eq!(general(0.0), "0");
        assert_eq!(general(0.1 + 0.2), "0.3");
        assert_eq!(general(-1234.5), "-1234.5");
        assert_eq!(general(1e-7), "0.0000001");
        assert_eq!(general(1.5e15), "1.5E+15");
        assert_eq!(general(2e-10), "2E-10");
    }

    #[test]
    fn negative_styles() {
        assert_eq!(fixed(-1.5, 1, false, 0), "-1.5");
        assert_eq!(fixed(-1.5, 1, false, 2), "(1.5)");
        assert_eq!(fixed(-0.001, 2, false, 0), "0.00");
        assert_eq!(fixed(1234.5, AUTO_DECIMALS, true, 0), "1,234.5");
    }

    #[test]
    fn currency_and_scientific() {
        assert_eq!(
            currency_text(-1234.5, &CurrencyFormat::automatic()),
            "-$1,234.50"
        );
        let accounting = CurrencyFormat {
            accounting: true,
            code: "EUR".into(),
            ..CurrencyFormat::automatic()
        };
        assert_eq!(currency_text(-775.0, &accounting), "€\t(775.00)");
        assert_eq!(scientific(12345.0, 2), "1.23E+04");
        assert_eq!(scientific(0.000999, 1), "1.0E-03");
    }

    #[test]
    fn fractions_and_bases() {
        assert_eq!(fraction_text(1.5, -1), "1 1/2");
        assert_eq!(fraction_text(0.333, -1), "1/3");
        assert_eq!(fraction_text(2.99, -1), "3");
        assert_eq!(base_text(-10.0, 2, 8), "-00001010");
        assert_eq!(base_text(255.0, 16, 0), "FF");
    }

    #[test]
    fn icu_dates() {
        let moment = epoch() + Duration::days(1) + Duration::hours(13) + Duration::minutes(1);
        assert_eq!(
            icu(moment, "EEEE, MMMM d, yyyy 'at' h:mm a"),
            "Tuesday, January 2, 2001 at 1:01 PM"
        );
        assert_eq!(icu(moment, "yy-MM-dd QQQ ''x''"), "01-01-02 Q1 'GMT'");
        assert_eq!(date(0.0, None), "1 Jan 2001");
    }

    #[test]
    fn durations() {
        assert_eq!(duration_text(90061.0, None), "1d 1h 1m 1s");
        assert_eq!(duration_text(-60.0, None), "-1m 0s");
    }

    #[test]
    fn week_numbers() {
        let moment = |day| {
            NaiveDate::from_ymd_opt(2026, 11, day)
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .expect("valid date")
        };
        // November 2026 starts on a Sunday: a one-day first week is week 0.
        assert_eq!(week_of_month(moment(1)), 0);
        assert_eq!(week_of_month(moment(2)), 1);
    }
}
