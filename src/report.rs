//! Semicolon-separated CSV tables, sized for reading by eye and by Excel.
//!
//! The format is a pragmatic compromise: `sep=;` and a UTF-8 BOM so Excel opens it without an
//! import dialog, fixed-width padded columns so a human can scan a file in a terminal, and one
//! row per record so it stays greppable and diffable.
//!
//! **This module resolves no paths and reads no environment.** Every function takes the file it
//! should act on. Column definitions and row construction belong to the caller — nothing here
//! knows what the rows mean.
//!
//! The interesting logic (reconciling a new row against a file's existing content) is separated
//! from the IO that surrounds it, so it can be tested on a `Vec<String>` without touching a disk.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

/// One record: column name to already-formatted cell text.
///
/// Values are `String` rather than a numeric union on purpose — formatting is the caller's
/// decision (see [`format_float`]), and by the time a row reaches this module every cell is
/// simply text to be padded and written.
pub type RowMap = HashMap<&'static str, String>;

/// A column: its name, and the width to pad it to.
pub type NameWidthPair = (&'static str, u8);

/// Serialises writes so two tests appending to the same file cannot interleave mid-row.
///
/// NOTE for anyone linking this crate twice (see the `[patch]` note at artemis's dependency
/// declaration): this is crate-level state, so two copies of the crate means two mutexes and no
/// mutual exclusion at all.
static CSV_WRITE_LOCK: Mutex<()> = Mutex::new(());

const BOM: &[u8] = b"\xEF\xBB\xBF";
const SEP_PREAMBLE: &str = "sep=;";

// --------------------------------------------------------------------------- formatting

/// Format a float for a fixed-width column: significant figures over decimal places.
///
/// A fixed `{:.3}` is fine for typical values and unreadable for pathological ones — an r² of
/// -5.5e50 renders as a fifty-character run of zeros. Bucketing by magnitude keeps every cell to
/// a handful of significant figures whatever the scale.
///
/// Moved verbatim from both consumers, where it had independently grown identical.
pub fn format_float(val: f64) -> String {
    if !val.is_finite() { return val.to_string(); }
    if val == 0.0 { return "0".to_string(); } // handles +0 and -0
    let neg = val.is_sign_negative();
    let core = match val.abs() {
        x if x >= 1.0e10  => format!("{x:.4e}"), // 1.1234e308 kind of scale
        x if x >= 1.0e3   => format!("{x:.6e}"), // 1.123456e9
        x if x >= 1.0     => format!("{x:.6}"),  // 987.123456 .. 1.000000
        x if x >= 1.0e-3  => format!("{x:.8}"),  // 0.00345678 .. 0.12345678
        x if x >  1.0e-10 => format!("{x:.5e}"), // 1.12345e-4 .. 1.00000e-6
        x                 => format!("{x:.3e}"), // super tiny: 1.2345e-12, etc.
    };
    if neg { format!("-{core}") } else { core }
}

// --------------------------------------------------------------------------- pure rendering

/// Write one line of padded, `;`-terminated cells.
pub fn write_line<W: Write>(writer: &mut W, fields: &[&str], widths: &[u8]) -> io::Result<()> {
    debug_assert_eq!(fields.len(), widths.len());
    for (&field_str, &width) in fields.iter().zip(widths) {
        let pad = (width as usize).saturating_sub(field_str.chars().count());
        writer.write_all(field_str.as_bytes())?;
        for _ in 0..pad { writer.write_all(b" ")?; }
        writer.write_all(b";")?;
    }
    writer.write_all(b"\n")
}

/// Write a header row and/or data rows to any sink.
///
/// Cells are looked up by column name, so a row missing a column yields an empty cell rather
/// than shifting every subsequent column — the failure mode of parallel name/value arrays.
pub fn print_table<W: Write>(
    w: &mut W,
    columns: &[NameWidthPair],
    rows: &[RowMap],
    include_header: bool,
) -> io::Result<()> {
    let widths: Vec<u8> = columns.iter().map(|&(_, w)| w).collect();
    let heads: Vec<&str> = columns.iter().map(|&(name, _)| name).collect();

    if include_header {
        write_line(w, &heads, &widths)?;
    }
    for row in rows {
        let storage: Vec<String> = columns.iter()
            .map(|&(name, _)| row.get(name).cloned().unwrap_or_default())
            .collect();
        let cells: Vec<&str> = storage.iter().map(|s| s.as_str()).collect();
        write_line(w, &cells, &widths)?;
    }
    w.flush()
}

/// Render one row to a single line, without a trailing newline.
pub fn render_row(columns: &[NameWidthPair], row: &RowMap) -> String {
    let mut buf: Vec<u8> = Vec::new();
    // Writing to a Vec cannot fail.
    let _ = print_table(&mut buf, columns, std::slice::from_ref(row), false);
    String::from_utf8_lossy(&buf).trim_end().to_string()
}

/// True if `line` carries data rather than being a preamble, a header, or blank.
fn is_data_line(line: &str, key_column: &str) -> bool {
    let t = line.trim_start_matches('\u{feff}').trim();
    !t.is_empty() && !t.starts_with(SEP_PREAMBLE) && !t.starts_with(key_column)
}

/// Split a data line into a [`RowMap`] using `columns` positionally.
pub fn parse_row(line: &str, columns: &[NameWidthPair]) -> RowMap {
    let line = line.trim_start_matches('\u{feff}').trim();
    let parts: Vec<&str> = line.split(';').map(str::trim).collect();
    let mut row = RowMap::with_capacity(columns.len());
    for (i, &(key, _)) in columns.iter().enumerate() {
        if let Some(&part) = parts.get(i) {
            if !part.is_empty() {
                row.insert(key, part.to_string());
            }
        }
    }
    row
}

/// Reconcile new rows against a file's existing lines. **Pure** -- no IO, no clock, no globals.
///
/// Replaces the **trailing block** of data lines whose `key_column` equals `key`, and appends
/// otherwise. A block rather than a single line because records are not always one-per-key: a
/// summary file holds one row per version, but a per-run sidecar holds ten, and re-running that
/// version must replace all ten rather than leave nine stale rows followed by one fresh one.
///
/// Only a *trailing* block is considered, so earlier history is never touched. That preserves
/// the "never rewrite history" property for every key except the one being rewritten right now.
pub fn upsert_lines(
    existing: &[String],
    columns: &[NameWidthPair],
    key_column: &str,
    key: &str,
    rows: &[RowMap],
) -> Vec<String> {
    let new_lines: Vec<String> = rows.iter().map(|r| render_row(columns, r)).collect();

    // Walk back over trailing data lines carrying `key`; stop at the first that does not.
    let mut keep = existing.len();
    for (idx, line) in existing.iter().enumerate().rev() {
        if !is_data_line(line, key_column) { break; } // header/preamble: keep everything before
        if key_of(line) != key { break; }
        keep = idx;
    }

    let mut out: Vec<String> = existing[..keep].to_vec();
    out.extend(new_lines);
    out
}

/// Drop every data line whose `key_column` is one of `keys`, wherever it sits. **Pure.**
///
/// The complement of [`upsert_lines`] for the case it cannot handle: several keys written in
/// turn (one per concurrency level, say). Only the *last* of those is a trailing block, so a
/// re-run would replace it and append duplicates of the others. Clearing all of them first,
/// then upserting each, keeps the file at one block per key.
pub fn remove_lines(existing: &[String], key_column: &str, keys: &[String]) -> Vec<String> {
    existing.iter()
        .filter(|line| !is_data_line(line, key_column) || !keys.iter().any(|k| k == key_of(line)))
        .cloned()
        .collect()
}

/// The first (key) field of a data line.
fn key_of(line: &str) -> &str {
    line.trim_start_matches('\u{feff}').trim().split(';').next().unwrap_or("").trim()
}

// --------------------------------------------------------------------------- filesystem

/// True if `path` is empty or already ends in a newline — ie it is safe to append a row.
fn ends_with_newline(path: &Path) -> io::Result<bool> {
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len == 0 { return Ok(true); }
    f.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    f.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

fn read_lines(path: &Path) -> io::Result<Vec<String>> {
    let f = fs::File::open(path)?;
    Ok(BufReader::new(f).lines().map_while(Result::ok).collect())
}

/// `File::create` that outlives a reader holding the file mapped. On Windows, truncating a
/// file that another process has memory-mapped fails with `ERROR_USER_MAPPED_FILE` (1224);
/// git maps files it diffs and the desktop app runs `git status` on a timer, so a ledger
/// CSV is mapped for a few milliseconds every so often, and a suite writing hundreds of rows
/// an hour hits that window (2026-09-21: two benchmarks of fifteen, their rows lost). The
/// reader is gone within milliseconds; wait for it. Sharing violations (32) get the same
/// treatment. Anything else, or two seconds of failure, is a real error.
fn create_retrying(path: &Path) -> io::Result<fs::File> {
    const TRANSIENT: [i32; 2] = [1224, 32];
    let mut waited = std::time::Duration::ZERO;
    loop {
        match fs::File::create(path) {
            Err(e) if e.raw_os_error().is_some_and(|c| TRANSIENT.contains(&c)) && waited < std::time::Duration::from_secs(2) => {
                let step = std::time::Duration::from_millis(50);
                std::thread::sleep(step);
                waited += step;
            }
            other => return other,
        }
    }
}

fn write_new_file(path: &Path, columns: &[NameWidthPair], rows: &[RowMap]) -> io::Result<()> {
    if let Some(dir) = path.parent() { fs::create_dir_all(dir)?; }
    let mut w = BufWriter::new(create_retrying(path)?);
    w.write_all(BOM)?;
    writeln!(w, "{SEP_PREAMBLE}")?;
    print_table(&mut w, columns, rows, true)
}

/// Append `row` to `path`, creating it with a BOM, `sep=;` and a header if it does not exist.
///
/// Never rewrites an existing line: use this for audit logs whose value is that they are a
/// complete history.
pub fn append_row(path: &Path, columns: &[NameWidthPair], row: &RowMap) -> io::Result<()> {
    let _lock = CSV_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    if !path.exists() {
        return write_new_file(path, columns, std::slice::from_ref(row));
    }

    let file = OpenOptions::new().append(true).open(path)?;
    let mut w = BufWriter::new(file);

    // Several hand-curated ledger files were saved without a trailing newline. Appending blind
    // welds the new row onto the tail of the last one, silently corrupting the row that was
    // already there -- and the damage only surfaces much later, when something greps for a key
    // at line start and comes up short.
    if !ends_with_newline(path)? {
        w.write_all(b"\n")?;
    }

    let widths: Vec<u8> = columns.iter().map(|&(_, w)| w).collect();
    let storage: Vec<String> = columns.iter()
        .map(|&(name, _)| row.get(name).cloned().unwrap_or_default())
        .collect();
    let cells: Vec<&str> = storage.iter().map(|s| s.as_str()).collect();
    write_line(&mut w, &cells, &widths)?;
    w.flush()
}

/// Upsert `rows` into `path`, keyed on `key_column` == `key`.
///
/// Replaces the trailing block of rows carrying the same key, otherwise appends. Use this for a
/// curated record — re-running the same key updates in place instead of accumulating duplicates.
///
/// `key` is a parameter rather than being read from the environment on purpose: a
/// `env!("CARGO_PKG_VERSION")` inside this crate would resolve to *this crate's* version, not the
/// caller's, and silently key every row identically.
pub fn upsert_rows(
    path: &Path,
    columns: &[NameWidthPair],
    key_column: &str,
    key: &str,
    rows: &[RowMap],
) -> io::Result<()> {
    let _lock = CSV_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    if !path.exists() {
        return write_new_file(path, columns, rows);
    }

    let existing = read_lines(path)?;
    let out = upsert_lines(&existing, columns, key_column, key, rows);

    let mut w = BufWriter::new(create_retrying(path)?);
    for line in &out {
        writeln!(w, "{line}")?;
    }
    w.flush()
}

/// Remove every row of `path` whose `key_column` is one of `keys`. A missing file is fine.
///
/// See [`remove_lines`]; call this before a sequence of [`upsert_rows`] with different keys.
pub fn remove_rows(path: &Path, key_column: &str, keys: &[String]) -> io::Result<()> {
    let _lock = CSV_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    if !path.exists() {
        return Ok(());
    }

    let existing = read_lines(path)?;
    let out = remove_lines(&existing, key_column, keys);
    if out.len() == existing.len() {
        return Ok(());
    }

    let mut w = BufWriter::new(create_retrying(path)?);
    for line in &out {
        writeln!(w, "{line}")?;
    }
    w.flush()
}

/// Read the last data row from `path`. `Ok(None)` if the file is absent or holds no data rows —
/// which is not an error, it is a new file.
pub fn read_last_row(
    path: &Path,
    columns: &[NameWidthPair],
) -> io::Result<Option<RowMap>> {
    if !path.exists() { return Ok(None); }
    let key_column = columns.first().map(|&(n, _)| n).unwrap_or("");
    let last = read_lines(path)?.into_iter()
        .filter(|l| is_data_line(l, key_column))
        .next_back();
    Ok(last.map(|l| parse_row(&l, columns)))
}

/// Print the previous row and the current row together, so a regression is visible immediately
/// rather than at the end of a long run.
pub fn print_comparison<W: Write>(
    w: &mut W,
    path: &Path,
    columns: &[NameWidthPair],
    current: &RowMap,
) -> io::Result<()> {
    let mut rows = Vec::with_capacity(2);
    if let Some(prev) = read_last_row(path, columns)? { rows.push(prev); }
    rows.push(current.clone());
    print_table(w, columns, &rows, true)
}

// --------------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: &[NameWidthPair] = &[("version", 8), ("value", 8)];

    fn row(v: &str, val: &str) -> RowMap {
        let mut r = RowMap::new();
        r.insert("version", v.to_string());
        r.insert("value", val.to_string());
        r
    }

    #[test]
    fn format_float_buckets_by_magnitude() {
        assert_eq!(format_float(0.0), "0");
        assert_eq!(format_float(-0.0), "0");
        assert_eq!(format_float(-210.0), "-210.000000");
        assert_eq!(format_float(1.0e11), "1.0000e11");
        assert_eq!(format_float(f64::NAN), "NaN");
    }

    #[test]
    fn upsert_replaces_only_the_last_row_with_a_matching_key() {
        let existing = vec![
            "sep=;".to_string(),
            "version ;value   ;".to_string(),
            "0.1.0   ;aaa     ;".to_string(),
            "0.2.0   ;bbb     ;".to_string(),
        ];
        let out = upsert_lines(&existing, COLS, "version", "0.2.0", &[row("0.2.0", "ccc")]);
        assert_eq!(out.len(), existing.len(), "same key must replace, not grow");
        assert!(out[2].starts_with("0.1.0"), "earlier rows must be untouched");
        assert!(out[3].contains("ccc"));
        assert!(!out[3].contains("bbb"));
    }

    #[test]
    fn upsert_appends_when_the_key_is_new() {
        let existing = vec![
            "sep=;".to_string(),
            "version ;value   ;".to_string(),
            "0.1.0   ;aaa     ;".to_string(),
        ];
        let out = upsert_lines(&existing, COLS, "version", "0.2.0", &[row("0.2.0", "bbb")]);
        assert_eq!(out.len(), existing.len() + 1);
        assert!(out[2].contains("aaa"), "history must be preserved");
        assert!(out[3].contains("bbb"));
    }

    /// The header line starts with the key column's name, so a naive "does the last line start
    /// with the key" check could mistake it for data on an otherwise empty file.
    #[test]
    fn a_header_only_file_is_not_mistaken_for_data() {
        let existing = vec!["sep=;".to_string(), "version ;value   ;".to_string()];
        let out = upsert_lines(&existing, COLS, "version", "0.1.0", &[row("0.1.0", "aaa")]);
        assert_eq!(out.len(), 3, "must append; the header is not a data row");
        assert!(out[1].starts_with("version"), "header must survive");
    }

    /// The per-run sidecar holds ten rows per version; re-running must replace all ten, not
    /// leave nine stale rows behind followed by one fresh one.
    #[test]
    fn upsert_replaces_a_whole_trailing_block() {
        let mut existing = vec!["sep=;".to_string(), "version ;value   ;".to_string()];
        existing.push(render_row(COLS, &row("0.1.0", "old")));
        for i in 0..3 { existing.push(render_row(COLS, &row("0.2.0", &format!("a{i}")))); }

        let fresh: Vec<RowMap> = (0..3).map(|i| row("0.2.0", &format!("b{i}"))).collect();
        let out = upsert_lines(&existing, COLS, "version", "0.2.0", &fresh);

        assert_eq!(out.len(), existing.len(), "three replaced by three");
        assert!(out[2].contains("old"), "the earlier version must survive untouched");
        assert!(out.iter().all(|l| !l.contains("a0") && !l.contains("a1") && !l.contains("a2")),
                "every stale row of the replaced block must be gone");
        assert!(out[5].contains("b2"));
    }

    /// Several keys written in turn: only the last is a trailing block, so a re-run through
    /// `upsert_lines` alone would duplicate the others. Clearing them first is what keeps the
    /// file at one block per key.
    #[test]
    fn remove_then_upsert_keeps_one_block_per_key() {
        let mut existing = vec!["sep=;".to_string(), "version ;value   ;".to_string()];
        existing.push(render_row(COLS, &row("0.1.0", "old")));
        existing.push(render_row(COLS, &row("0.2-c=1", "a1")));
        existing.push(render_row(COLS, &row("0.2-c=2", "a2")));

        let keys = vec!["0.2-c=1".to_string(), "0.2-c=2".to_string()];
        let out = remove_lines(&existing, "version", &keys);
        assert_eq!(out.len(), 3, "both labelled rows gone, header and history kept");
        assert!(out[2].contains("old"));

        let out = upsert_lines(&out, COLS, "version", "0.2-c=1", &[row("0.2-c=1", "b1")]);
        let out = upsert_lines(&out, COLS, "version", "0.2-c=2", &[row("0.2-c=2", "b2")]);
        assert_eq!(out.len(), 5);
        assert!(out.iter().filter(|l| l.starts_with("0.2-c=1")).count() == 1);
        assert!(out[3].contains("b1") && out[4].contains("b2"));
    }

    #[test]
    fn remove_lines_ignores_the_header_even_when_a_key_matches_it() {
        let existing = vec!["sep=;".to_string(), "version ;value   ;".to_string()];
        let out = remove_lines(&existing, "version", &["version".to_string()]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn round_trips_through_parse() {
        let r = row("0.9.4", "-210.000000");
        let parsed = parse_row(&render_row(COLS, &r), COLS);
        assert_eq!(parsed.get("version").map(String::as_str), Some("0.9.4"));
        assert_eq!(parsed.get("value").map(String::as_str), Some("-210.000000"));
    }

    #[test]
    fn a_row_missing_a_column_yields_an_empty_cell_not_a_shift() {
        let mut r = RowMap::new();
        r.insert("value", "x".to_string());
        let parsed = parse_row(&render_row(COLS, &r), COLS);
        assert_eq!(parsed.get("value").map(String::as_str), Some("x"),
                   "the present column must stay in its own position");
    }
}
