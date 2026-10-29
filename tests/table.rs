//! `table`, through its public API: reading tables written by hand or by spreadsheets in any
//! locale, and writing tables that read back exactly.

use std::path::Path;

use empowerops_rs_utils::table::{Delimiter, Table, TableError, read_table, write_table};

fn read(text: &str, delimiter: Option<Delimiter>) -> Result<Table, TableError> {
    read_table(text.as_bytes(), delimiter)
}

fn names(header: &[&str]) -> Option<Vec<String>> {
    Some(header.iter().map(|name| name.to_string()).collect())
}

// ── Reading, as a model-fitting tool has read numeric matrices ──────────────────────────────

#[test]
fn when_text_has_no_header_should_read_every_line_as_a_row() {
    // setup
    let text = "1.0,2.0\n3.0,4.0";

    // act
    let table = read(text, None).unwrap();

    // assert
    assert_eq!(table.header, None);
    assert_eq!(table.rows, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
}

#[test]
fn when_first_line_is_not_numbers_should_read_it_as_the_header() {
    // setup
    let text = "x1,x2\n1.0,2.0\n3.0,4.0";

    // act
    let table = read(text, None).unwrap();

    // assert
    assert_eq!(table.header, names(&["x1", "x2"]));
    assert_eq!(table.rows, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
}

#[test]
fn when_two_lines_precede_the_numbers_should_err_naming_too_many_header_rows() {
    // setup
    let text = "section\nx1,x2\n1.0,2.0";

    // act
    let result = read(text, None);

    // assert
    assert!(
        matches!(result, Err(TableError::TooManyHeaderRows { line: 2 })),
        "{result:?}"
    );
}

#[test]
fn when_text_is_empty_should_err_empty() {
    // setup
    let text = "";

    // act
    let result = read(text, None);

    // assert
    assert!(matches!(result, Err(TableError::Empty)), "{result:?}");
}

#[test]
fn when_only_a_header_should_err_empty() {
    // setup
    let text = "x1;x2\n";

    // act
    let result = read(text, None);

    // assert
    assert!(matches!(result, Err(TableError::Empty)), "{result:?}");
}

#[test]
fn when_blank_lines_fall_between_rows_should_skip_them() {
    // setup
    let text = "1.0,2.0\n\n3.0,4.0";

    // act
    let table = read(text, None).unwrap();

    // assert
    assert_eq!(table.rows, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
}

#[test]
fn when_tab_separated_with_header_should_read_names_and_rows() {
    // setup
    let text = "a\tb\n1.0\t2.0\n3.0\t4.0";

    // act
    let table = read(text, None).unwrap();

    // assert
    assert_eq!(table.header, names(&["a", "b"]));
    assert_eq!(table.rows, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
}

#[test]
fn when_a_row_is_short_should_err_naming_its_line() {
    // setup
    let text = "1.0,2.0\n3.0";

    // act
    let result = read(text, None);

    // assert
    assert!(
        matches!(
            result,
            Err(TableError::Ragged {
                line: 2,
                expected: 2,
                found: 1
            })
        ),
        "{result:?}"
    );
}

#[test]
fn when_separated_by_spaces_should_read_columns() {
    // setup
    let text = "1.0  2.0\n3.0 4.0";

    // act
    let table = read(text, None).unwrap();

    // assert
    assert_eq!(table.rows, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
}

#[test]
fn when_text_starts_with_a_bom_should_strip_it() {
    // setup
    let text = "\u{feff}x;y\n1;2";

    // act
    let table = read(text, None).unwrap();

    // assert
    assert_eq!(table.header, names(&["x", "y"]));
}

#[test]
fn when_a_cell_is_not_a_number_should_err_naming_row_column_and_text() {
    // setup
    let text = "x;y\n1;2\n3;oops";

    // act
    let result = read(text, None);

    // assert
    let Err(TableError::NotANumber { line, column, text }) = result else {
        panic!("expected NotANumber, got {result:?}");
    };
    assert_eq!((line, column, text.as_str()), (3, 2, "oops"));
}

#[test]
fn when_header_repeats_a_name_should_err_naming_it() {
    // setup
    let text = "x;x\n1;2";

    // act
    let result = read(text, None);

    // assert
    let Err(TableError::DuplicateName { name }) = result else {
        panic!("expected DuplicateName, got {result:?}");
    };
    assert_eq!(name, "x");
}

// ── Locale: decimal commas ───────────────────────────────────────────────────────────────────

#[test]
fn when_semicolons_separate_decimal_commas_should_read_one_and_a_half() {
    // setup: a headerless sheet from a decimal-comma locale, more commas than semicolons
    let text = "1,5;2,25\n3,0;4,75";

    // act
    let table = read(text, None).unwrap();

    // assert
    assert_eq!(table.header, None);
    assert_eq!(table.rows, vec![vec![1.5, 2.25], vec![3.0, 4.75]]);
}

#[test]
fn when_tabs_separate_decimal_commas_should_read_them_as_points() {
    // setup
    let text = "x\ty\n1,5\t2,25";

    // act
    let table = read(text, None).unwrap();

    // assert
    assert_eq!(table.rows, vec![vec![1.5, 2.25]]);
}

#[test]
fn when_commas_separate_should_never_read_a_decimal_comma() {
    // setup
    let text = "1,5";

    // act
    let table = read(text, Some(Delimiter::Comma)).unwrap();

    // assert
    assert_eq!(table.rows, vec![vec![1.0, 5.0]]);
}

#[test]
fn when_a_line_holds_semicolons_and_commas_should_sniff_semicolon() {
    // act
    let delimiter = Delimiter::sniff("1,5;2,25;3,125");

    // assert
    assert_eq!(delimiter, Delimiter::Semicolon);
}

#[test]
fn when_extension_is_scsv_csv_or_tsv_should_name_its_delimiter() {
    // setup
    let cases = [
        ("plan.csv", Some(Delimiter::Comma)),
        ("plan.SCSV", Some(Delimiter::Semicolon)),
        ("plan.tsv", Some(Delimiter::Tab)),
        ("plan.txt", None),
        ("plan", None),
    ];

    for (path, expected) in cases {
        // act
        let delimiter = Delimiter::from_extension(Path::new(path));

        // assert
        assert_eq!(delimiter, expected, "{path}");
    }
}

// ── Writing, so a table reads back exactly ───────────────────────────────────────────────────

#[test]
fn when_written_then_read_should_return_every_value_bit_for_bit() {
    // setup: values whose shortest text is long, tiny, huge, signed zero, or a stepped level
    let header = ["x1", "x2", "x3", "x4", "x5"];
    let rows = vec![
        vec![0.1 + 0.2, 1e-300, -0.0, f64::MAX, 0.3],
        vec![1.0, -2.5, 1e16, 123_456.789, f64::MIN_POSITIVE],
    ];

    for delimiter in [Delimiter::Comma, Delimiter::Semicolon, Delimiter::Tab] {
        // act
        let mut written = Vec::new();
        write_table(&mut written, delimiter, &header, &rows).unwrap();
        let table = read_table(written.as_slice(), Some(delimiter)).unwrap();

        // assert
        assert_eq!(table.header, names(&header), "{delimiter:?}");
        let bits = |rows: &[Vec<f64>]| -> Vec<Vec<u64>> {
            rows.iter()
                .map(|row| row.iter().map(|value| value.to_bits()).collect())
                .collect()
        };
        assert_eq!(
            bits(&table.rows),
            bits(&rows),
            "{delimiter:?}: {}",
            String::from_utf8_lossy(&written)
        );
    }
}

#[test]
fn when_written_should_put_the_header_first_and_a_point_per_line() {
    // setup
    let header = ["r", "h"];
    let rows = vec![vec![0.5, 2.0], vec![1.25, 10.0]];

    // act
    let mut written = Vec::new();
    write_table(&mut written, Delimiter::Semicolon, &header, &rows).unwrap();

    // assert
    assert_eq!(String::from_utf8(written).unwrap(), "r;h\n0.5;2\n1.25;10\n");
}

#[test]
fn when_header_names_a_column_should_return_it_by_name() {
    // setup
    let table = read("r;h\n1;2\n3;4", None).unwrap();

    // act
    let found = table.column("h");
    let missing = table.column("z");

    // assert
    assert_eq!(found, Some(vec![2.0, 4.0]));
    assert_eq!(missing, None);
}

#[test]
fn when_a_header_name_holds_the_delimiter_should_survive_quoted() {
    // setup
    let header = ["a;b", "c"];
    let rows = vec![vec![1.0, 2.0]];
    let mut written = Vec::new();
    write_table(&mut written, Delimiter::Semicolon, &header, &rows).unwrap();

    // act
    let table = read_table(written.as_slice(), None).unwrap();

    // assert
    assert_eq!(table.header, names(&header));
    assert_eq!(table.rows, rows);
}
