//! Column preview for datatable sources: the header row, up to five sample rows and the row count of an
//! uploaded CSV or XLSX. Pure functions over bytes; nothing is stored.
//!
//! XLSX is read directly (zip + quick-xml, no calamine): `xl/workbook.xml` names the sheets,
//! `xl/_rels/workbook.xml.rels` maps a sheet to its part, `xl/sharedStrings.xml` holds the shared
//! strings, and the sheet part is streamed row by row. Every byte decompressed counts against
//! [`MAX_XLSX_EXPANDED_BYTES`] and the archive may hold at most [`MAX_XLSX_ENTRIES`] parts, so a zip
//! bomb ends in [`PreviewError::TooLarge`], not in memory exhaustion.
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, Cursor, Read};

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use serde::Serialize;

pub const MAX_SAMPLE_ROWS: usize = 5;
/// Columns a previewed table may have (CSV header width, XLSX cell column).
pub const MAX_COLUMNS: usize = 1_000;
/// Bytes of one previewed cell (header or sample) kept; longer text is cut at a character boundary. With
/// [`MAX_COLUMNS`] and [`MAX_SAMPLE_ROWS`] this bounds a preview to about 6 MiB of text.
pub const MAX_CELL_BYTES: usize = 1024;
/// Shared-string entries (`<si>`) an XLSX may hold.
pub const MAX_SHARED_STRINGS: usize = 1_000_000;
/// Bytes of shared-string text an XLSX may hold.
pub const MAX_SHARED_STRINGS_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TableFormat {
    Csv,
    Xlsx,
}

impl TableFormat {
    /// From the uploaded file name's extension (case-insensitive).
    pub fn from_file_name(name: &str) -> Option<Self> {
        let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
        match ext.as_str() {
            "csv" | "txt" => Some(Self::Csv),
            "xlsx" => Some(Self::Xlsx),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ColumnPreview {
    pub format: TableFormat,
    pub columns: Vec<String>,
    pub sample_rows: Vec<Vec<String>>,
    pub row_count: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PreviewError {
    #[error("the file has no header row")]
    Empty,
    #[error("the file is not a readable {0}")]
    Unreadable(&'static str),
    #[error("the workbook has no sheet named \"{0}\"")]
    SheetNotFound(String),
    #[error("the workbook is too large to preview: {0}")]
    TooLarge(&'static str),
    #[error("the table has more than 1,000 columns")]
    TooManyColumns,
}

/// `s` cut to at most [`MAX_CELL_BYTES`], at a character boundary.
fn cut(s: &str) -> &str {
    if s.len() <= MAX_CELL_BYTES {
        return s;
    }
    let mut end = MAX_CELL_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// [`cut`] in place.
fn cut_owned(s: &mut String) {
    let n = cut(s).len();
    s.truncate(n);
}

/// Blank header cells become `column_<n>` (1-based); a repeated name gets `_2`, `_3`, ... (the first suffix
/// not already taken). Linear: a set of the names taken and, per name, the next suffix to try.
fn header(cells: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(cells.len());
    let mut taken: HashSet<String> = HashSet::with_capacity(cells.len());
    let mut next: HashMap<String, usize> = HashMap::new();
    for (i, c) in cells.into_iter().enumerate() {
        let base = match c.trim() {
            "" => format!("column_{}", i + 1),
            t => t.to_string(),
        };
        let name = if taken.contains(&base) {
            let n = next.entry(base.clone()).or_insert(2);
            let mut name = format!("{base}_{n}");
            while taken.contains(&name) {
                *n += 1;
                name = format!("{base}_{n}");
            }
            *n += 1;
            name
        } else {
            base
        };
        taken.insert(name.clone());
        out.push(name);
    }
    out
}

/// Streams the rows once and skips blank rows. With `header_row` the first non-blank row is the header;
/// without it every row is data and the columns are `column_1..N`, N the widest row in the whole file (strata_ingest
/// names every row's cells `column_N`, so a wider row past the sample still has its column). Every data
/// row is counted and at most `MAX_SAMPLE_ROWS` are kept. Memory stays bounded whatever the input size.
fn finish(
    format: TableFormat,
    mut rows: impl Iterator<Item = Result<Vec<String>, PreviewError>>,
    header_row: bool,
) -> Result<ColumnPreview, PreviewError> {
    let blank = |r: &Vec<String>| r.iter().all(|c| c.trim().is_empty());
    if !header_row {
        let (sample_rows, row_count, width) = data_rows(rows, None)?;
        if row_count == 0 {
            return Err(PreviewError::Empty);
        }
        if width > MAX_COLUMNS {
            return Err(PreviewError::TooManyColumns);
        }
        let columns: Vec<String> = (1..=width).map(|i| format!("column_{i}")).collect();
        let sample_rows = sample_rows
            .into_iter()
            .map(|mut r| {
                r.resize(width, String::new());
                r
            })
            .collect();
        return Ok(ColumnPreview {
            format,
            columns,
            sample_rows,
            row_count,
        });
    }
    let mut head = None;
    for r in rows.by_ref() {
        let r = r?;
        if !blank(&r) {
            head = Some(r);
            break;
        }
    }
    let mut head = head.ok_or(PreviewError::Empty)?;
    if head.len() > MAX_COLUMNS {
        return Err(PreviewError::TooManyColumns);
    }
    head.iter_mut().for_each(cut_owned);
    let columns = header(head);
    let (sample_rows, row_count, _) = data_rows(rows, Some(columns.len()))?;
    Ok(ColumnPreview {
        format,
        columns,
        sample_rows,
        row_count,
    })
}

/// The non-blank rows: all counted, the first `MAX_SAMPLE_ROWS` kept with their cells cut, and padded or
/// truncated to `width` when the header fixes it.
fn data_rows(
    rows: impl Iterator<Item = Result<Vec<String>, PreviewError>>,
    width: Option<usize>,
) -> Result<(Vec<Vec<String>>, u64, usize), PreviewError> {
    let blank = |r: &Vec<String>| r.iter().all(|c| c.trim().is_empty());
    let mut sample_rows = Vec::new();
    let mut row_count = 0u64;
    let mut widest = 0usize;
    for r in rows {
        let mut r = r?;
        if blank(&r) {
            continue;
        }
        row_count += 1;
        widest = widest.max(r.len());
        if sample_rows.len() < MAX_SAMPLE_ROWS {
            if let Some(w) = width {
                r.resize(w, String::new());
            }
            r.iter_mut().for_each(cut_owned);
            sample_rows.push(r);
        }
    }
    Ok((sample_rows, row_count, widest))
}

/// The delimiter (`,` `;` or tab) that splits the first line into the most fields; `,` when none occurs.
fn sniff_delimiter(bytes: &[u8]) -> u8 {
    let first = bytes.split(|b| *b == b'\n').next().unwrap_or_default();
    let count = |d: u8| first.iter().filter(|b| **b == d).count();
    let mut best = (b',', count(b','));
    for &d in b";\t" {
        if count(d) > best.1 {
            best = (d, count(d));
        }
    }
    best.0
}

/// Column preview of a CSV; `header_row` false reads the first row as data (see [`finish`]).
pub fn preview_csv(bytes: &[u8], header_row: bool) -> Result<ColumnPreview, PreviewError> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(sniff_delimiter(bytes))
        .from_reader(bytes);
    let rows = rdr.byte_records().map(|rec| {
        let rec = rec.map_err(|_| PreviewError::Unreadable("CSV file"))?;
        // refused on the raw field count, before any field becomes a String or a column name
        if rec.len() > MAX_COLUMNS {
            return Err(PreviewError::TooManyColumns);
        }
        Ok(rec
            .iter()
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect::<Vec<_>>())
    });
    finish(TableFormat::Csv, rows, header_row)
}

// ---------------------------------------------------------------------------------------------
// XLSX

/// Total bytes an XLSX preview may decompress, over all parts it reads.
pub const MAX_XLSX_EXPANDED_BYTES: u64 = 50 * 1024 * 1024;
/// Parts (zip entries) an XLSX may hold.
pub const MAX_XLSX_ENTRIES: usize = 10_000;
const XLSX: PreviewError = PreviewError::Unreadable("XLSX file");

type Archive<'d> = zip::ZipArchive<Cursor<&'d [u8]>>;

/// The decompression allowance shared by every part read for one preview.
struct Budget {
    left: Cell<u64>,
    blown: Cell<bool>,
}

impl Budget {
    fn new(bytes: u64) -> Self {
        Self {
            left: Cell::new(bytes),
            blown: Cell::new(false),
        }
    }

    /// A failure while reading: the cap when it was hit, else an unreadable file.
    fn error(&self) -> PreviewError {
        if self.blown.get() {
            PreviewError::TooLarge("it expands beyond 50 MiB")
        } else {
            XLSX
        }
    }
}

/// A reader that charges every byte it yields to the [`Budget`] and fails once it is spent.
struct Capped<'b, R> {
    inner: R,
    budget: &'b Budget,
}

impl<R: Read> Read for Capped<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        let left = self.budget.left.get();
        if n as u64 > left {
            self.budget.blown.set(true);
            return Err(io::Error::other("XLSX expansion cap reached"));
        }
        self.budget.left.set(left - n as u64);
        Ok(n)
    }
}

type PartReader<'a, 'd> = Reader<BufReader<Capped<'a, zip::read::ZipFile<'a, Cursor<&'d [u8]>>>>>;

/// An XML reader over part `name` (exact, then ASCII case-insensitive); `None` when absent.
fn open_part<'a, 'd>(
    zip: &'a mut Archive<'d>,
    budget: &'a Budget,
    name: &str,
) -> Result<Option<PartReader<'a, 'd>>, PreviewError> {
    let index = zip.index_for_name(name).or_else(|| {
        zip.file_names()
            .position(|n| n.is_ok_and(|n| n.eq_ignore_ascii_case(name)))
    });
    let Some(index) = index else {
        return Ok(None);
    };
    let file = zip.by_index(index).map_err(|_| XLSX)?;
    Ok(Some(Reader::from_reader(BufReader::new(Capped {
        inner: file,
        budget,
    }))))
}

/// The value of the attribute whose local name is `local` (prefixed when `prefixed`, e.g. `r:id`).
fn attr(e: &BytesStart<'_>, local: &str, prefixed: bool) -> Result<Option<String>, PreviewError> {
    for a in e.attributes() {
        let a = a.map_err(|_| XLSX)?;
        if a.key.local_name().as_ref() == local && a.key.prefix().is_some() == prefixed {
            let v = a
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|_| XLSX)?;
            return Ok(Some(v.into_owned()));
        }
    }
    Ok(None)
}

/// Appends a text-bearing event (text, CDATA, entity or character reference) to `out`.
/// Returns whether the event carried text.
fn push_text(ev: &Event<'_>, out: &mut String) -> Result<bool, PreviewError> {
    match ev {
        Event::Text(t) => out.push_str(&t.xml10_content()),
        Event::CData(c) => out.push_str(&c.xml10_content()),
        Event::GeneralRef(r) => {
            if r.is_char_ref() {
                out.push(r.resolve_char_ref().ok().flatten().ok_or(XLSX)?);
            } else {
                let name = r.xml10_content();
                out.push_str(quick_xml::escape::resolve_predefined_entity(&name).ok_or(XLSX)?);
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// `(sheet name, relationship id)` in workbook order.
fn workbook_sheets(
    r: &mut PartReader<'_, '_>,
    budget: &Budget,
) -> Result<Vec<(String, String)>, PreviewError> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf).map_err(|_| budget.error())? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "sheet" => {
                let name = attr(&e, "name", false)?.ok_or(XLSX)?;
                let rid = attr(&e, "id", true)?.ok_or(XLSX)?;
                out.push((name, rid));
            }
            Event::Eof => return Ok(out),
            _ => {}
        }
        buf.clear();
    }
}

/// The part path (inside the zip) of relationship `rid` in `xl/_rels/workbook.xml.rels`.
fn relationship_target(
    r: &mut PartReader<'_, '_>,
    budget: &Budget,
    rid: &str,
) -> Result<Option<String>, PreviewError> {
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf).map_err(|_| budget.error())? {
            Event::Start(e) | Event::Empty(e)
                if e.local_name().as_ref() == "Relationship"
                    && attr(&e, "Id", false)?.as_deref() == Some(rid) =>
            {
                return Ok(attr(&e, "Target", false)?.map(|t| part_path(&t)));
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
        buf.clear();
    }
}

/// A relationship target resolved against `xl/` (absolute targets start at the package root).
fn part_path(target: &str) -> String {
    let joined = match target.strip_prefix('/') {
        Some(abs) => abs.to_string(),
        None => format!("xl/{target}"),
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// The shared-string table, packed into one buffer: string `i` is `text[ends[i-1]..ends[i]]`.
#[derive(Default)]
struct SharedStrings {
    text: String,
    ends: Vec<usize>,
}

impl SharedStrings {
    fn push_end(&mut self) -> Result<(), PreviewError> {
        if self.ends.len() >= MAX_SHARED_STRINGS {
            return Err(PreviewError::TooLarge(
                "it holds more than 1,000,000 shared strings",
            ));
        }
        self.ends.push(self.text.len());
        Ok(())
    }

    fn get(&self, i: usize) -> Option<&str> {
        let end = *self.ends.get(i)?;
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        Some(&self.text[start..end])
    }

    /// `<si>` items: plain `<t>` or rich-text runs `<r><t>`; phonetic hints (`<rPh>`) are skipped.
    fn read(r: &mut PartReader<'_, '_>, budget: &Budget) -> Result<Self, PreviewError> {
        let mut s = Self::default();
        let mut buf = Vec::new();
        let (mut in_t, mut in_rph) = (false, false);
        loop {
            let ev = r.read_event_into(&mut buf).map_err(|_| budget.error())?;
            match &ev {
                Event::Start(e) => match e.local_name().as_ref() {
                    "t" => in_t = true,
                    "rPh" => in_rph = true,
                    _ => {}
                },
                Event::End(e) => match e.local_name().as_ref() {
                    "t" => in_t = false,
                    "rPh" => in_rph = false,
                    "si" => s.push_end()?,
                    _ => {}
                },
                Event::Empty(e) if e.local_name().as_ref() == "si" => s.push_end()?,
                Event::Eof => return Ok(s),
                _ if in_t && !in_rph => {
                    push_text(&ev, &mut s.text)?;
                    if s.text.len() > MAX_SHARED_STRINGS_BYTES {
                        return Err(PreviewError::TooLarge("its shared strings exceed 16 MiB"));
                    }
                }
                _ => {}
            }
            buf.clear();
        }
    }
}

/// 0-based column of a cell reference's letters (`A1` → 0, `AA7` → 26); `None` without letters.
fn column_index(cell_ref: &str) -> Option<usize> {
    let letters = cell_ref.bytes().take_while(u8::is_ascii_alphabetic);
    let mut col = 0usize;
    let mut any = false;
    for b in letters {
        any = true;
        col = col
            .checked_mul(26)?
            .checked_add(usize::from(b.to_ascii_uppercase() - b'A') + 1)?;
    }
    any.then(|| col - 1)
}

/// The cell being read: its column, type attribute (`t`) and raw value text.
struct XlsxCell {
    col: usize,
    kind: Option<String>,
    raw: String,
}

/// The rows of one worksheet part, streamed. Cells land at their referenced column, so sparse rows
/// keep their shape; a row is as wide as its last non-empty cell.
struct SheetRows<'a, 'd> {
    reader: PartReader<'a, 'd>,
    budget: &'a Budget,
    strings: &'a SharedStrings,
    buf: Vec<u8>,
    done: bool,
}

/// A finished cell's display text: shared strings resolved, booleans as `TRUE` / `FALSE`. A shared string is
/// copied only up to [`MAX_CELL_BYTES`]: one long string referenced by every cell must not be copied whole each time.
fn cell_value(strings: &SharedStrings, c: XlsxCell) -> Result<String, PreviewError> {
    Ok(match c.kind.as_deref() {
        Some("s") => {
            let i: usize = c.raw.trim().parse().map_err(|_| XLSX)?;
            cut(strings.get(i).ok_or(XLSX)?).to_string()
        }
        Some("b") => match c.raw.trim() {
            "1" => "TRUE".into(),
            "0" => "FALSE".into(),
            _ => c.raw,
        },
        // inlineStr, str (formula result), e (error), n / absent (number): the text as stored
        _ => c.raw,
    })
}

impl SheetRows<'_, '_> {
    fn next_row(&mut self) -> Result<Option<Vec<String>>, PreviewError> {
        let mut row: Option<Vec<String>> = None;
        let mut cell: Option<XlsxCell> = None;
        let mut next_col = 0usize;
        let (mut in_v, mut in_is, mut in_t, mut in_rph) = (false, false, false, false);
        loop {
            self.buf.clear();
            let ev = self
                .reader
                .read_event_into(&mut self.buf)
                .map_err(|_| self.budget.error())?;
            match &ev {
                Event::Start(e) | Event::Empty(e) => {
                    let empty = matches!(ev, Event::Empty(_));
                    match e.local_name().as_ref() {
                        "row" if empty => return Ok(Some(Vec::new())),
                        "row" => {
                            row = Some(Vec::new());
                            next_col = 0;
                        }
                        "c" if row.is_some() => {
                            let col = match attr(e, "r", false)? {
                                Some(r) => column_index(&r).unwrap_or(next_col),
                                None => next_col,
                            };
                            if col >= MAX_COLUMNS {
                                return Err(PreviewError::TooManyColumns);
                            }
                            next_col = col + 1;
                            if !empty {
                                cell = Some(XlsxCell {
                                    col,
                                    kind: attr(e, "t", false)?,
                                    raw: String::new(),
                                });
                            }
                        }
                        "v" if !empty => in_v = true,
                        "is" if !empty => in_is = true,
                        "t" if !empty => in_t = true,
                        "rPh" if !empty => in_rph = true,
                        _ => {}
                    }
                }
                Event::End(e) => match e.local_name().as_ref() {
                    "v" => in_v = false,
                    "is" => in_is = false,
                    "t" => in_t = false,
                    "rPh" => in_rph = false,
                    "c" => {
                        if let (Some(c), Some(r)) = (cell.take(), row.as_mut()) {
                            let col = c.col;
                            let v = cell_value(self.strings, c)?;
                            if !v.is_empty() {
                                if r.len() <= col {
                                    r.resize(col + 1, String::new());
                                }
                                r[col] = v;
                            }
                        }
                    }
                    "row" => return Ok(Some(row.take().unwrap_or_default())),
                    _ => {}
                },
                Event::Eof => return Ok(None),
                _ => {
                    if let Some(c) = cell.as_mut()
                        && (in_v || (in_is && in_t && !in_rph))
                    {
                        push_text(&ev, &mut c.raw)?;
                        // an inline value longer than a preview cell is cut as it streams in
                        if c.raw.len() > 4 * MAX_CELL_BYTES {
                            cut_owned(&mut c.raw);
                        }
                    }
                }
            }
        }
    }
}

impl Iterator for SheetRows<'_, '_> {
    type Item = Result<Vec<String>, PreviewError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.next_row() {
            Ok(Some(r)) => Some(Ok(r)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// Column preview of one worksheet of an XLSX workbook: `sheet` by name (ASCII case-insensitive),
/// else the first sheet. The first non-blank row is the header unless `header_row` is false, as for CSV.
pub fn preview_xlsx(
    bytes: &[u8],
    sheet: Option<&str>,
    header_row: bool,
) -> Result<ColumnPreview, PreviewError> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| XLSX)?;
    if zip.len() > MAX_XLSX_ENTRIES {
        return Err(PreviewError::TooLarge("it holds more than 10,000 parts"));
    }
    let budget = Budget::new(MAX_XLSX_EXPANDED_BYTES);

    let sheets = {
        let mut r = open_part(&mut zip, &budget, "xl/workbook.xml")?.ok_or(XLSX)?;
        workbook_sheets(&mut r, &budget)?
    };
    let rid = match sheet.map(str::trim).filter(|s| !s.is_empty()) {
        Some(want) => sheets
            .iter()
            .find(|(n, _)| n == want)
            .or_else(|| sheets.iter().find(|(n, _)| n.eq_ignore_ascii_case(want)))
            .ok_or_else(|| PreviewError::SheetNotFound(want.to_string()))?,
        None => sheets.first().ok_or(XLSX)?,
    }
    .1
    .clone();
    let target = {
        let mut r = open_part(&mut zip, &budget, "xl/_rels/workbook.xml.rels")?.ok_or(XLSX)?;
        relationship_target(&mut r, &budget, &rid)?.ok_or(XLSX)?
    };
    let strings = match open_part(&mut zip, &budget, "xl/sharedStrings.xml")? {
        Some(mut r) => SharedStrings::read(&mut r, &budget)?,
        None => SharedStrings::default(),
    };
    let reader = open_part(&mut zip, &budget, &target)?.ok_or(XLSX)?;
    let rows = SheetRows {
        reader,
        budget: &budget,
        strings: &strings,
        buf: Vec::new(),
        done: false,
    };
    finish(TableFormat::Xlsx, rows, header_row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_header_samples_and_count() {
        let p = preview_csv(
            b"\xEF\xBB\xBFklant_id;naam;email\nC-1;Anna;a@x.nl\n\nC-2;Bram;b@x.nl\nC-3;Cas\n",
            true,
        )
        .unwrap();
        assert_eq!(p.format, TableFormat::Csv);
        assert_eq!(p.columns, ["klant_id", "naam", "email"]);
        assert_eq!(p.row_count, 3, "blank lines are not rows");
        assert_eq!(
            p.sample_rows[2],
            ["C-3", "Cas", ""],
            "short rows are padded to the header"
        );
    }

    #[test]
    fn without_a_header_row_the_first_row_is_data_and_columns_are_numbered() {
        let p = preview_csv(b"C-1;Anna;a@x.nl\n\nC-2;Bram\nC-3;Cas;c@x.nl;VIP\n", false).unwrap();
        assert_eq!(
            p.columns,
            ["column_1", "column_2", "column_3", "column_4"],
            "N is the widest sampled row"
        );
        assert_eq!(p.row_count, 3, "row 1 is counted as data");
        assert_eq!(p.sample_rows[0], ["C-1", "Anna", "a@x.nl", ""]);
        assert_eq!(p.sample_rows[1], ["C-2", "Bram", "", ""]);
        assert_eq!(p.sample_rows[2], ["C-3", "Cas", "c@x.nl", "VIP"]);
        // the default reading of the same file loses that row to the header
        let h = preview_csv(b"C-1;Anna;a@x.nl\nC-2;Bram\n", true).unwrap();
        assert_eq!((h.columns[0].as_str(), h.row_count), ("C-1", 1));
        assert_eq!(
            preview_csv(b"\n\n", false).unwrap_err(),
            PreviewError::Empty
        );
    }

    #[test]
    fn without_a_header_row_the_width_is_the_widest_row_beyond_the_sample() {
        let p = preview_csv(b"a,b\nc,d\ne,f\ng,h\ni,j\nk,l\nm,n,o\n", false).unwrap();
        assert_eq!(p.columns, ["column_1", "column_2", "column_3"]);
        assert_eq!((p.row_count, p.sample_rows.len()), (7, MAX_SAMPLE_ROWS));
        assert_eq!(
            p.sample_rows[0],
            ["a", "b", ""],
            "sampled rows padded to the full width"
        );
    }

    #[test]
    fn without_a_header_row_every_row_is_counted_but_five_are_sampled() {
        let csv: String = (0..9).map(|i| format!("{i}\n")).collect();
        let p = preview_csv(csv.as_bytes(), false).unwrap();
        assert_eq!(p.columns, ["column_1"]);
        assert_eq!((p.row_count, p.sample_rows.len()), (9, MAX_SAMPLE_ROWS));
        assert_eq!(p.sample_rows[0], ["0"]);
    }

    #[test]
    fn blank_and_duplicate_headers_get_stable_names() {
        let p = preview_csv(b"id,,id,name\n1,2,3,4\n", true).unwrap();
        assert_eq!(p.columns, ["id", "column_2", "id_2", "name"]);
    }

    #[test]
    fn twenty_thousand_duplicate_headers_are_named_in_linear_time() {
        let cells = vec!["a".to_string(); 20_000];
        let t = std::time::Instant::now();
        let names = header(cells);
        let took = t.elapsed();
        assert!(took < std::time::Duration::from_secs(2), "took {took:?}");
        assert_eq!(names.len(), 20_000);
        assert_eq!(names[1], "a_2");
        assert_eq!(names[19_999], "a_20000");
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), 20_000);
        // a suffix that is itself a header is skipped, as before
        assert_eq!(
            header(vec!["a".into(), "a_2".into(), "a".into()]),
            ["a", "a_2", "a_3"]
        );
    }

    #[test]
    fn a_header_wider_than_the_column_cap_is_refused() {
        // an 8 KB line of `a,a,a…` (4,096 columns)
        let wide = vec!["a"; 4_096].join(",");
        assert_eq!(
            preview_csv(format!("{wide}\n1\n").as_bytes(), true).unwrap_err(),
            PreviewError::TooManyColumns
        );
        // a narrow header and one data record past the cap: refused on the record's field count
        let far = vec!["1"; MAX_COLUMNS + 1].join(",");
        assert_eq!(
            preview_csv(format!("h\n{far}\n").as_bytes(), true).unwrap_err(),
            PreviewError::TooManyColumns
        );
        let at_cap = vec!["a"; MAX_COLUMNS].join(",");
        assert_eq!(
            preview_csv(format!("{at_cap}\n").as_bytes(), true)
                .unwrap()
                .columns
                .len(),
            MAX_COLUMNS
        );
    }

    #[test]
    fn preview_cells_are_cut_to_the_cell_cap() {
        let long = "é".repeat(MAX_CELL_BYTES); // 2 bytes each: twice the cap
        let p = preview_csv(format!("h\n{long}\n").as_bytes(), true).unwrap();
        let cell = &p.sample_rows[0][0];
        assert!(cell.len() <= MAX_CELL_BYTES, "{} bytes", cell.len());
        assert!(cell.len() >= MAX_CELL_BYTES - 4);
    }

    #[test]
    fn at_most_five_samples_but_every_row_counted() {
        let mut csv = String::from("n\n");
        for i in 0..12 {
            csv.push_str(&format!("{i}\n"));
        }
        let p = preview_csv(csv.as_bytes(), true).unwrap();
        assert_eq!(p.sample_rows.len(), 5);
        assert_eq!(p.row_count, 12);
    }

    #[test]
    fn tabs_quotes_and_non_utf8_bytes() {
        let p = preview_csv(b"a\tb\n\"x\ty\"\t\xff\n", true).unwrap();
        assert_eq!(p.columns, ["a", "b"]);
        assert_eq!(p.sample_rows[0], ["x\ty", "\u{fffd}"]);
    }

    #[test]
    fn a_large_input_of_tiny_rows_is_counted_and_sampled_in_one_pass() {
        let mut csv = String::from("a,b\n");
        for _ in 0..200_000 {
            csv.push_str(",\n"); // blank rows
        }
        for i in 0..200_000 {
            csv.push_str(&format!("{i},x\n"));
        }
        let p = preview_csv(csv.as_bytes(), true).unwrap();
        assert_eq!(p.row_count, 200_000);
        assert_eq!(p.sample_rows.len(), 5);
        assert_eq!(p.sample_rows[0], ["0", "x"]);
    }

    #[test]
    fn single_column_input_defaults_to_comma() {
        assert_eq!(sniff_delimiter(b"name\nAnna\n"), b',');
    }

    #[test]
    fn empty_input_is_an_error() {
        assert_eq!(preview_csv(b"\n\n", true).unwrap_err(), PreviewError::Empty);
    }

    #[test]
    fn format_from_extension() {
        assert_eq!(TableFormat::from_file_name("a.CSV"), Some(TableFormat::Csv));
        assert_eq!(
            TableFormat::from_file_name("b.xlsx"),
            Some(TableFormat::Xlsx)
        );
        assert_eq!(TableFormat::from_file_name("c.xls"), None);
        assert_eq!(TableFormat::from_file_name("noext"), None);
    }
}

#[cfg(test)]
pub(crate) mod xlsx_tests {
    use std::io::{Cursor, Write};

    use super::*;

    /// A zip of `(part name, contents)`, deflated, built in memory.
    pub(crate) fn zip_of(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in parts {
            w.start_file(*name, opts).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    const NS: &str = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

    /// A minimal workbook: `sheets` are `(name, <sheetData> inner xml)`, `shared` the `<si>` texts.
    pub(crate) fn workbook(sheets: &[(&str, &str)], shared: Option<&[&str]>) -> Vec<u8> {
        let mut wb = format!("<?xml version=\"1.0\"?><workbook {NS}><sheets>");
        let mut rels = String::from(
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
        );
        let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
        for (i, (name, data)) in sheets.iter().enumerate() {
            let n = i + 1;
            wb.push_str(&format!(
                r#"<sheet name="{name}" sheetId="{n}" r:id="rId{n}"/>"#
            ));
            // the second sheet uses an absolute target, as some writers do
            let target = if n == 2 {
                format!("/xl/worksheets/sheet{n}.xml")
            } else {
                format!("worksheets/sheet{n}.xml")
            };
            rels.push_str(&format!(
                r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="{target}"/>"#
            ));
            parts.push((
                format!("xl/worksheets/sheet{n}.xml"),
                format!("<?xml version=\"1.0\"?><worksheet {NS}><sheetData>{data}</sheetData></worksheet>")
                    .into_bytes(),
            ));
        }
        wb.push_str("</sheets></workbook>");
        rels.push_str("</Relationships>");
        parts.push(("xl/workbook.xml".into(), wb.into_bytes()));
        parts.push(("xl/_rels/workbook.xml.rels".into(), rels.into_bytes()));
        if let Some(shared) = shared {
            let mut sst = format!("<?xml version=\"1.0\"?><sst {NS}>");
            for s in shared {
                sst.push_str(&format!("<si>{s}</si>"));
            }
            sst.push_str("</sst>");
            parts.push(("xl/sharedStrings.xml".into(), sst.into_bytes()));
        }
        let refs: Vec<(&str, &[u8])> = parts
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        zip_of(&refs)
    }

    #[test]
    fn shared_and_inline_strings_numbers_and_booleans() {
        let data = concat!(
            r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c>"#,
            r#"<c r="C1" t="inlineStr"><is><t>active</t></is></c><c r="D1" t="s"><v>2</v></c></row>"#,
            r#"<row r="2"><c r="A2" t="s"><v>3</v></c><c r="B2"><v>42.5</v></c>"#,
            r#"<c r="C2" t="b"><v>1</v></c><c r="D2" t="inlineStr"><is><r><t>B &amp; </t></r><r><t xml:space="preserve">co&#233;</t></r></is></c></row>"#,
            r#"<row r="3"><c r="A3" t="str"><f>A2&amp;"x"</f><v>C-1x</v></c><c r="B3"><v>7</v></c>"#,
            r#"<c r="C3" t="b"><v>0</v></c><c r="D3" t="e"><v>#N/A</v></c></row>"#,
        );
        let shared: &[&str] = &[
            "<t>klant_id</t>",
            "<r><t>na</t></r><r><t>am</t></r><rPh><t>ignored</t></rPh>",
            "<t>notes</t>",
            "<t>C-1</t>",
        ];
        let p = preview_xlsx(&workbook(&[("Customers", data)], Some(shared)), None, true).unwrap();
        assert_eq!(p.format, TableFormat::Xlsx);
        assert_eq!(p.columns, ["klant_id", "naam", "active", "notes"]);
        assert_eq!(p.row_count, 2);
        assert_eq!(p.sample_rows[0], ["C-1", "42.5", "TRUE", "B & coé"]);
        assert_eq!(p.sample_rows[1], ["C-1x", "7", "FALSE", "#N/A"]);
    }

    #[test]
    fn sparse_cells_land_in_their_lettered_column() {
        let mut header = String::from(r#"<row r="1">"#);
        for i in 0..28 {
            // columns A..AB, the last two without an r attribute (they follow the previous cell)
            let r = if i < 26 {
                format!(r#" r="{}1""#, (b'A' + i as u8) as char)
            } else {
                String::new()
            };
            header.push_str(&format!(r#"<c{r} t="inlineStr"><is><t>h{i}</t></is></c>"#));
        }
        header.push_str("</row>");
        let data = format!(
            r#"{header}<row r="2"/><row r="4"><c r="B4"><v>2</v></c><c r="D4"/><c r="AA4"><v>27</v></c></row>"#
        );
        let p = preview_xlsx(&workbook(&[("S", &data)], None), None, true).unwrap();
        assert_eq!(p.columns.len(), 28);
        assert_eq!(p.columns[26], "h26");
        assert_eq!(p.row_count, 1, "the empty row 2 is not a row");
        let row = &p.sample_rows[0];
        assert_eq!(row.len(), 28, "padded to the header");
        assert_eq!((row[0].as_str(), row[1].as_str()), ("", "2"));
        assert_eq!(row[3], "", "an empty cell element stays empty");
        assert_eq!(row[26], "27", "AA is the 27th column");
    }

    #[test]
    fn a_named_sheet_is_chosen_and_the_first_is_the_default() {
        let one = r#"<row><c t="inlineStr"><is><t>first</t></is></c></row>"#;
        let two = concat!(
            r#"<row><c t="inlineStr"><is><t>second</t></is></c></row>"#,
            r#"<row><c><v>1</v></c></row><row><c><v>2</v></c></row>"#,
        );
        let wb = workbook(&[("Intro", one), ("Klanten 2026", two)], None);
        assert_eq!(preview_xlsx(&wb, None, true).unwrap().columns, ["first"]);
        assert_eq!(
            preview_xlsx(&wb, Some("  "), true).unwrap().columns,
            ["first"]
        );
        let p = preview_xlsx(&wb, Some("Klanten 2026"), true).unwrap();
        assert_eq!(
            (p.columns.clone(), p.row_count),
            (vec!["second".to_string()], 2)
        );
        assert_eq!(
            preview_xlsx(&wb, Some("klanten 2026"), true)
                .unwrap()
                .columns,
            ["second"]
        );
    }

    #[test]
    fn an_xlsx_without_a_header_row_keeps_its_first_row_as_data() {
        let data = concat!(
            r#"<row><c t="inlineStr"><is><t>10482</t></is></c><c t="inlineStr"><is><t>Anna</t></is></c></row>"#,
            r#"<row><c><v>10483</v></c></row>"#,
        );
        let wb = workbook(&[("Klanten", data)], None);
        let p = preview_xlsx(&wb, None, false).unwrap();
        assert_eq!(p.columns, ["column_1", "column_2"]);
        assert_eq!(p.row_count, 2);
        assert_eq!(p.sample_rows, [["10482", "Anna"], ["10483", ""]]);
    }

    #[test]
    fn a_missing_sheet_is_named_in_the_error() {
        let wb = workbook(&[("Intro", "<row><c><v>1</v></c></row>")], None);
        let e = preview_xlsx(&wb, Some("Orders"), true).unwrap_err();
        assert_eq!(e, PreviewError::SheetNotFound("Orders".into()));
        assert_eq!(e.to_string(), "the workbook has no sheet named \"Orders\"");
    }

    #[test]
    fn samples_are_capped_and_every_row_counted() {
        let mut data = String::from(r#"<row><c t="inlineStr"><is><t>n</t></is></c></row>"#);
        for i in 0..12 {
            data.push_str(&format!("<row><c><v>{i}</v></c></row>"));
        }
        let p = preview_xlsx(&workbook(&[("S", &data)], None), None, true).unwrap();
        assert_eq!((p.sample_rows.len(), p.row_count), (MAX_SAMPLE_ROWS, 12));
    }

    #[test]
    fn a_shared_string_is_cut_per_cell_not_copied_whole() {
        let big = "x".repeat(64 * 1024);
        let shared = [format!("<t>{big}</t>")];
        let shared: Vec<&str> = shared.iter().map(String::as_str).collect();
        let row: String = (0..200).map(|_| r#"<c t="s"><v>0</v></c>"#).collect();
        let data = format!("<row>{row}</row><row>{row}</row>");
        let p = preview_xlsx(&workbook(&[("S", &data)], Some(&shared)), None, true).unwrap();
        assert_eq!(p.columns.len(), 200);
        assert!(
            p.columns.iter().all(|c| c.len() <= MAX_CELL_BYTES + 8),
            "header cut"
        );
        assert!(p.sample_rows[0].iter().all(|c| c.len() == MAX_CELL_BYTES));
    }

    #[test]
    fn an_oversized_shared_strings_part_is_refused() {
        let big = "x".repeat(MAX_SHARED_STRINGS_BYTES + 1);
        let shared = [format!("<t>{big}</t>")];
        let shared: Vec<&str> = shared.iter().map(String::as_str).collect();
        let wb = workbook(
            &[("S", r#"<row><c t="s"><v>0</v></c></row>"#)],
            Some(&shared),
        );
        assert_eq!(
            preview_xlsx(&wb, None, true).unwrap_err(),
            PreviewError::TooLarge("its shared strings exceed 16 MiB")
        );
    }

    #[test]
    fn cell_value_copies_at_most_the_cell_cap_of_a_shared_string() {
        let strings = SharedStrings {
            text: "x".repeat(4 * 1024 * 1024),
            ends: vec![4 * 1024 * 1024],
        };
        let c = XlsxCell {
            col: 0,
            kind: Some("s".into()),
            raw: "0".into(),
        };
        let v = cell_value(&strings, c).unwrap();
        assert_eq!(v.len(), MAX_CELL_BYTES, "copied {} bytes", v.len());
    }

    #[test]
    fn too_many_shared_string_entries_are_refused() {
        let sst = format!(
            "<?xml version=\"1.0\"?><sst {NS}>{}</sst>",
            "<si/>".repeat(MAX_SHARED_STRINGS + 1)
        );
        let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
        let wb = workbook(&[("S", r#"<row><c><v>1</v></c></row>"#)], None);
        let mut zip = zip::ZipArchive::new(Cursor::new(wb.as_slice())).unwrap();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).unwrap();
            let mut b = Vec::new();
            f.read_to_end(&mut b).unwrap();
            parts.push((f.name().unwrap().to_string(), b));
        }
        parts.push(("xl/sharedStrings.xml".into(), sst.into_bytes()));
        let refs: Vec<(&str, &[u8])> = parts
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        assert_eq!(
            preview_xlsx(&zip_of(&refs), None, true).unwrap_err(),
            PreviewError::TooLarge("it holds more than 1,000,000 shared strings")
        );
    }

    #[test]
    fn a_cell_beyond_the_column_cap_is_refused_without_widening_the_row() {
        // ALL is column 1,000 (index 999): the last allowed; ALM (index 1,000) is past the cap; XFD is Excel's last.
        let ok = r#"<row><c r="A1"><v>1</v></c><c r="ALL1"><v>2</v></c></row>"#;
        let p = preview_xlsx(&workbook(&[("S", ok)], None), None, true).unwrap();
        assert_eq!(p.columns.len(), MAX_COLUMNS);
        // a narrow header and the far cell in a DATA row: only the sheet reader can refuse it
        for far in ["ALM", "XFD"] {
            let data = format!(
                r#"<row><c r="A1"><v>h</v></c></row><row><c r="A2"><v>1</v></c><c r="{far}2"><v>2</v></c></row>"#
            );
            assert_eq!(
                preview_xlsx(&workbook(&[("S", &data)], None), None, true).unwrap_err(),
                PreviewError::TooManyColumns,
                "{far}"
            );
        }
    }

    #[test]
    fn a_zip_bomb_hits_the_expansion_cap() {
        // ~51 MiB of whitespace inside the sheet deflates to a few dozen KiB.
        let pad = " ".repeat(51 * 1024 * 1024);
        let data = format!(r#"<row><c><v>1</v></c></row>{pad}"#);
        let wb = workbook(&[("S", &data)], None);
        assert!(wb.len() < 1024 * 1024, "compressed to {} bytes", wb.len());
        assert_eq!(
            preview_xlsx(&wb, None, true).unwrap_err(),
            PreviewError::TooLarge("it expands beyond 50 MiB")
        );
    }

    #[test]
    fn too_many_parts_are_refused_before_reading() {
        let names: Vec<String> = (0..=MAX_XLSX_ENTRIES).map(|i| format!("p{i}")).collect();
        let parts: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
        assert_eq!(
            preview_xlsx(&zip_of(&parts), None, true).unwrap_err(),
            PreviewError::TooLarge("it holds more than 10,000 parts")
        );
    }

    #[test]
    fn non_zip_and_incomplete_workbooks_are_unreadable() {
        assert_eq!(preview_xlsx(b"a,b\n1,2\n", None, true).unwrap_err(), XLSX);
        assert_eq!(preview_xlsx(b"PK\x03\x04", None, true).unwrap_err(), XLSX);
        let no_workbook = zip_of(&[("xl/worksheets/sheet1.xml", b"<worksheet/>")]);
        assert_eq!(preview_xlsx(&no_workbook, None, true).unwrap_err(), XLSX);
        let header_only_blank = workbook(&[("S", "<row/>")], None);
        assert_eq!(
            preview_xlsx(&header_only_blank, None, true).unwrap_err(),
            PreviewError::Empty
        );
        let bad_shared_index = workbook(&[("S", r#"<row><c t="s"><v>9</v></c></row>"#)], Some(&[]));
        assert_eq!(
            preview_xlsx(&bad_shared_index, None, true).unwrap_err(),
            XLSX
        );
    }

    #[test]
    fn column_letters_and_part_paths() {
        assert_eq!(column_index("A1"), Some(0));
        assert_eq!(column_index("z9"), Some(25));
        assert_eq!(column_index("AA10"), Some(26));
        assert_eq!(column_index("XFD1"), Some(16_383));
        assert_eq!(column_index("12"), None);
        assert_eq!(
            part_path("worksheets/sheet1.xml"),
            "xl/worksheets/sheet1.xml"
        );
        assert_eq!(part_path("/xl/worksheets/s.xml"), "xl/worksheets/s.xml");
        assert_eq!(part_path("../xl/./worksheets/s.xml"), "xl/worksheets/s.xml");
    }
}
