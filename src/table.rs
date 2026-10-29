//! Delimited tables of numbers: written so they read back exactly, read so a decimal comma
//! doesn't split a value in two.
//!
//! A table is an optional header row of names, then rows of numbers, every row as long as the
//! first. [`write_table`] writes one; [`read_table`] reads one back, or one written by hand or by
//! a spreadsheet: comma, semicolon, tab or whitespace separated, with a header or without.
//!
//! **Locale.** A spreadsheet in a locale that writes `1,5` for one and a half separates its
//! cells with semicolons. So a semicolon or tab separated cell may use a decimal comma, and a
//! comma separated one may not: there the comma always separates. When the delimiter is guessed
//! ([`Delimiter::sniff`]), a semicolon wins over a comma for the same reason. Numbers are always
//! *written* with a decimal point, whatever the locale, so a written table means the same thing
//! everywhere.
//!
//! **Exactness.** Every value is written as the shortest text that parses back to the same
//! `f64`, so writing a table and reading it back returns the same bits.
//!
//! The splitting and quoting are the `csv` crate's; this module is the policy over it. Like the
//! rest of the crate, it resolves no paths: it reads from any `Read` and writes to any `Write`.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;

/// What separates one cell from the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delimiter {
    Comma,
    Semicolon,
    Tab,
    /// Any run of spaces or tabs. Read only: [`write_table`] refuses it, since a reader can't
    /// tell a written empty cell from none.
    Whitespace,
}

impl Delimiter {
    /// The delimiter a file's extension names: `.csv`, `.scsv` or `.tsv`, in any case. `None`
    /// for any other extension, `.txt` included, and for none: those are for [`sniff`](Self::sniff).
    pub fn from_extension(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "csv" => Some(Self::Comma),
            "scsv" => Some(Self::Semicolon),
            "tsv" => Some(Self::Tab),
            _ => None,
        }
    }

    /// Guesses a delimiter from a table's first line: a semicolon if there is one, else a tab,
    /// else a comma, else whitespace.
    ///
    /// Semicolon first, because a line of semicolon-separated decimal commas (`1,5;2,25`) holds
    /// more commas than semicolons, and counting would split it on the commas.
    pub fn sniff(first_line: &str) -> Self {
        if first_line.contains(';') {
            Self::Semicolon
        } else if first_line.contains('\t') {
            Self::Tab
        } else if first_line.contains(',') {
            Self::Comma
        } else {
            Self::Whitespace
        }
    }

    /// The byte `csv` splits on; `None` for whitespace, which `csv` can't split on.
    fn byte(self) -> Option<u8> {
        match self {
            Self::Comma => Some(b','),
            Self::Semicolon => Some(b';'),
            Self::Tab => Some(b'\t'),
            Self::Whitespace => None,
        }
    }
}

/// A table read by [`read_table`].
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    /// The names in the first row, when the first row was not numbers.
    pub header: Option<Vec<String>>,
    /// Every row of numbers, in order, each as long as the first.
    pub rows: Vec<Vec<f64>>,
}

impl Table {
    /// The column `name` heads, top to bottom. `None` if there is no header or no such name.
    pub fn column(&self, name: &str) -> Option<Vec<f64>> {
        let index = self.header.as_ref()?.iter().position(|header| header == name)?;
        Some(self.rows.iter().map(|row| row[index]).collect())
    }
}

/// Why a table could not be read. Lines and columns count from one, as an editor shows them.
#[derive(Debug)]
pub enum TableError {
    /// No row of numbers: nothing at all, only blank lines, or only a header.
    Empty,
    /// A second line of names before the first row of numbers.
    TooManyHeaderRows {
        line: usize,
    },
    /// A row longer or shorter than the first.
    Ragged {
        line: usize,
        expected: usize,
        found: usize,
    },
    /// A cell that is not a number, where numbers were wanted.
    NotANumber {
        line: usize,
        column: usize,
        text: String,
    },
    /// A header naming the same column twice.
    DuplicateName {
        name: String,
    },
    /// Text the `csv` reader could not split, such as a quote left open.
    Malformed {
        line: usize,
        message: String,
    },
    Io(io::Error),
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the table has no rows of numbers"),
            Self::TooManyHeaderRows { line } => {
                write!(
                    f,
                    "line {line}: a second line of names; a table has at most one header row"
                )
            }
            Self::Ragged { line, expected, found } => {
                write!(f, "line {line}: {found} cells where the table has {expected}")
            }
            Self::NotANumber { line, column, text } => {
                write!(f, "line {line}, column {column}: {text:?} is not a number")
            }
            Self::DuplicateName { name } => write!(f, "the header names {name:?} twice"),
            Self::Malformed { line, message } => write!(f, "line {line}: {message}"),
            Self::Io(error) => write!(f, "reading the table: {error}"),
        }
    }
}

impl std::error::Error for TableError {}

/// Writes `header`, then each row, one line apiece, cells separated by `delimiter`.
///
/// A name holding the delimiter or a quote is quoted, as `csv` quotes. Each value is the
/// shortest text that reads back as the same `f64`, with a decimal point.
///
/// # Panics
/// If `delimiter` is [`Delimiter::Whitespace`], or a row is not as long as `header`.
pub fn write_table(writer: impl Write, delimiter: Delimiter, header: &[&str], rows: &[Vec<f64>]) -> io::Result<()> {
    let byte = delimiter
        .byte()
        .unwrap_or_else(|| panic!("a table is not written separated by whitespace"));
    let mut csv = csv::WriterBuilder::new()
        .delimiter(byte)
        .terminator(csv::Terminator::Any(b'\n'))
        .from_writer(writer);
    csv.write_record(header).map_err(io::Error::from)?;
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.len(), header.len(), "row {index} is not as long as the header");
        csv.write_record(row.iter().map(|value| exact_text(*value)))
            .map_err(io::Error::from)?;
    }
    csv.flush()
}

/// The shortest text that parses back to `value`, with a decimal point: plain where that is
/// short, in exponent form where plain would be a long run of zeros (`1e-300`, not three hundred
/// digits). Both of Rust's forms are shortest round-trip.
fn exact_text(value: f64) -> String {
    let magnitude = value.abs();
    if value == 0.0 || !value.is_finite() || (1e-5..1e16).contains(&magnitude) {
        format!("{value}")
    } else {
        format!("{value:e}")
    }
}

/// Reads a table, guessing the delimiter from its first line when `delimiter` is `None`.
///
/// A UTF-8 byte order mark is dropped and blank lines are skipped. The first line is the header
/// if none of its cells is a number. Cells are trimmed, and a semicolon or tab separated cell may
/// use a decimal comma (see the module docs).
pub fn read_table(mut reader: impl Read, delimiter: Option<Delimiter>) -> Result<Table, TableError> {
    let mut text = String::new();
    reader.read_to_string(&mut text).map_err(TableError::Io)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);

    let Some(first_line) = text.lines().find(|line| !line.trim().is_empty()) else {
        return Err(TableError::Empty);
    };
    let delimiter = delimiter.unwrap_or_else(|| Delimiter::sniff(first_line));

    let mut header: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<f64>> = Vec::new();
    for (line, cells) in split_lines(text, delimiter)? {
        let numbers: Vec<Option<f64>> = cells.iter().map(|cell| number(cell, delimiter)).collect();
        // A line of names before any numbers: the header, if it is the first.
        if rows.is_empty() && numbers.iter().all(Option::is_none) {
            if header.is_some() {
                return Err(TableError::TooManyHeaderRows { line });
            }
            for (index, name) in cells.iter().enumerate() {
                if cells[..index].contains(name) {
                    return Err(TableError::DuplicateName { name: name.clone() });
                }
            }
            header = Some(cells);
            continue;
        }
        let expected = match (&header, rows.first()) {
            (Some(names), _) => names.len(),
            (None, Some(first)) => first.len(),
            (None, None) => cells.len(),
        };
        if cells.len() != expected {
            return Err(TableError::Ragged {
                line,
                expected,
                found: cells.len(),
            });
        }
        let row = numbers
            .into_iter()
            .zip(&cells)
            .enumerate()
            .map(|(index, (parsed, text))| {
                parsed.ok_or_else(|| TableError::NotANumber {
                    line,
                    column: index + 1,
                    text: text.clone(),
                })
            })
            .collect::<Result<Vec<f64>, TableError>>()?;
        rows.push(row);
    }

    if rows.is_empty() {
        return Err(TableError::Empty);
    }
    Ok(Table { header, rows })
}

/// Each line that has cells, with its line number, its cells trimmed. A line with nothing in any
/// cell is skipped: a blank line, or a spreadsheet's trailing row of bare delimiters.
fn split_lines(text: &str, delimiter: Delimiter) -> Result<Vec<(usize, Vec<String>)>, TableError> {
    let lines: Vec<(usize, Vec<String>)> = match delimiter.byte() {
        None => text
            .lines()
            .enumerate()
            .map(|(index, line)| (index + 1, line.split_whitespace().map(str::to_string).collect()))
            .collect(),
        Some(byte) => {
            let mut csv = csv::ReaderBuilder::new()
                .delimiter(byte)
                .has_headers(false)
                .flexible(true)
                .trim(csv::Trim::All)
                .from_reader(text.as_bytes());
            let mut lines = Vec::new();
            for record in csv.records() {
                let record = record.map_err(|error| TableError::Malformed {
                    line: error.position().map_or(0, |position| position.line() as usize),
                    message: error.to_string(),
                })?;
                let line = record.position().map_or(0, |position| position.line() as usize);
                lines.push((line, record.iter().map(str::to_string).collect()));
            }
            lines
        }
    };
    Ok(lines
        .into_iter()
        .filter(|(_, cells)| cells.iter().any(|cell| !cell.is_empty()))
        .collect())
}

/// A cell as a number, or `None`. Where the comma doesn't separate cells it may be a decimal
/// comma.
fn number(cell: &str, delimiter: Delimiter) -> Option<f64> {
    match delimiter {
        Delimiter::Comma => cell.parse::<f64>().ok(),
        Delimiter::Semicolon | Delimiter::Tab | Delimiter::Whitespace => cell.replace(',', ".").parse::<f64>().ok(),
    }
}
