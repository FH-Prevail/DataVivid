//! Reading and writing data files.

use std::borrow::Cow;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use polars::prelude::*;

use crate::formats::Format;

/// Text encodings offered for delimited files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TextEncoding {
    Auto,
    Utf8,
    Windows1252,
    Utf16,
}

impl TextEncoding {
    pub const ALL: [TextEncoding; 4] = [
        TextEncoding::Auto,
        TextEncoding::Utf8,
        TextEncoding::Windows1252,
        TextEncoding::Utf16,
    ];
    pub fn label(self) -> &'static str {
        match self {
            TextEncoding::Auto => "Auto-detect",
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::Windows1252 => "Windows-1252 (Western)",
            TextEncoding::Utf16 => "UTF-16",
        }
    }
}

/// Options for delimited text (CSV/TSV/TXT).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CsvOptions {
    /// `None` = sniff from the file.
    pub separator: Option<u8>,
    pub has_header: bool,
    pub encoding: TextEncoding,
    /// `None` = guess (only for `;`-separated files).
    pub decimal_comma: Option<bool>,
    pub skip_rows: usize,
    /// Detect numbers, booleans and dates. When off every column is text,
    /// which keeps identifiers such as `00123` intact.
    pub infer_types: bool,
    /// Extra cell values treated as missing, comma-separated (e.g. `NA,n/a`).
    pub null_values: String,
}

impl Default for CsvOptions {
    fn default() -> Self {
        CsvOptions {
            separator: None,
            has_header: true,
            encoding: TextEncoding::Auto,
            decimal_comma: None,
            skip_rows: 0,
            infer_types: true,
            null_values: String::new(),
        }
    }
}

/// What to load.
#[derive(Clone, Debug)]
pub struct LoadRequest {
    pub path: PathBuf,
    pub format: Format,
    pub csv: CsvOptions,
    /// Excel/ODS sheet or SQLite table; `None` = the first one.
    pub table: Option<String>,
}

impl LoadRequest {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let format = Format::from_path(&path).ok_or_else(|| {
            anyhow!(
                "Unsupported file type: {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        })?;
        Ok(LoadRequest { path, format, csv: CsvOptions::default(), table: None })
    }
}

/// Details of how a delimited file was actually read.
#[derive(Clone, Debug, Default)]
pub struct CsvResolved {
    pub separator: u8,
    pub encoding: &'static str,
    pub had_bom: bool,
    pub decimal_comma: bool,
}

/// A loaded table.
pub struct Loaded {
    pub df: DataFrame,
    pub request: LoadRequest,
    /// Sheets (Excel/ODS) or tables (SQLite) available in the file.
    pub tables: Vec<String>,
    pub table: Option<String>,
    pub csv: Option<CsvResolved>,
    pub notes: Vec<String>,
}

pub fn load(req: LoadRequest) -> Result<Loaded> {
    let path = req.path.clone();
    let mut loaded = Loaded {
        df: DataFrame::empty(),
        request: req.clone(),
        tables: Vec::new(),
        table: None,
        csv: None,
        notes: Vec::new(),
    };
    match req.format {
        Format::Csv | Format::Tsv => {
            let (df, resolved) = read_delimited(&path, req.format, &req.csv)?;
            loaded.df = df;
            loaded.csv = Some(resolved);
        }
        Format::Parquet => {
            let f = File::open(&path)?;
            loaded.df = ParquetReader::new(f).finish()?;
        }
        Format::Json => {
            let f = File::open(&path)?;
            loaded.df = JsonReader::new(f)
                .with_json_format(JsonFormat::Json)
                .infer_schema_len(std::num::NonZeroUsize::new(10_000))
                .finish()
                .map_err(anyhow::Error::from)
                .context("Reading JSON (DataVivid expects an array of flat records)")?;
        }
        Format::NdJson => {
            let f = File::open(&path)?;
            loaded.df = JsonReader::new(f)
                .with_json_format(JsonFormat::JsonLines)
                .infer_schema_len(std::num::NonZeroUsize::new(10_000))
                .finish()?;
        }
        Format::ArrowIpc => {
            let f = File::open(&path)?;
            loaded.df = IpcReader::new(f).finish()?;
        }
        Format::Avro => {
            let f = File::open(&path)?;
            loaded.df = polars::io::avro::AvroReader::new(f).finish()?;
        }
        Format::Excel | Format::Ods => {
            let (df, sheets, sheet) = read_spreadsheet(&path, req.table.as_deref(), &req.csv)?;
            loaded.df = df;
            loaded.tables = sheets;
            loaded.table = Some(sheet);
        }
        Format::Sqlite => {
            let (df, tables, table) = read_sqlite(&path, req.table.as_deref())?;
            loaded.df = df;
            loaded.tables = tables;
            loaded.table = Some(table);
        }
    }
    loaded.df.rechunk_mut_par();
    Ok(loaded)
}

// ---------------------------------------------------------------------------
// Delimited text
// ---------------------------------------------------------------------------

fn read_delimited(path: &Path, format: Format, opts: &CsvOptions) -> Result<(DataFrame, CsvResolved)> {
    let mut raw = Vec::new();
    File::open(path)
        .with_context(|| format!("Opening {}", path.display()))?
        .read_to_end(&mut raw)?;

    // Transparent gzip support (data.csv.gz).
    if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::MultiGzDecoder::new(&raw[..])
            .read_to_end(&mut out)
            .context("Decompressing gzip data")?;
        raw = out;
    }

    let (text, encoding, had_bom) = decode_text(raw, opts.encoding)?;
    let sample = sample_lines(&text, opts.skip_rows, 200);
    let separator = opts.separator.unwrap_or_else(|| {
        sniff_separator(&sample).unwrap_or(if format == Format::Tsv { b'\t' } else { b',' })
    });
    let decimal_comma = opts
        .decimal_comma
        .unwrap_or_else(|| separator != b',' && guess_decimal_comma(&sample, separator));

    let null_values: Vec<PlSmallStr> = opts
        .null_values
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PlSmallStr::from_str)
        .collect();

    let parse = CsvParseOptions::default()
        .with_separator(separator)
        .with_try_parse_dates(opts.infer_types)
        .with_decimal_comma(decimal_comma)
        .with_truncate_ragged_lines(true)
        .with_missing_is_null(true)
        .with_null_values(if null_values.is_empty() {
            None
        } else {
            Some(NullValues::AllColumns(null_values))
        });
    let read_opts = CsvReadOptions::default()
        .with_has_header(opts.has_header)
        .with_skip_rows(opts.skip_rows)
        .with_infer_schema_length(if opts.infer_types { Some(10_000) } else { Some(0) })
        .with_raise_if_empty(false)
        .with_parse_options(parse);

    let bytes: Vec<u8> = text.into_owned().into_bytes();
    let df = read_opts
        .clone()
        .into_reader_with_file_handle(Cursor::new(bytes.clone()))
        .finish()
        .or_else(|first_err| {
            // A column inferred as a number may hold text further down. Retry
            // with a full-file schema scan before giving up.
            read_opts
                .with_infer_schema_length(None)
                .into_reader_with_file_handle(Cursor::new(bytes))
                .finish()
                .map_err(|_| first_err)
        })
        .map_err(anyhow::Error::from)
        .context("Parsing delimited text")?;

    Ok((df, CsvResolved { separator, encoding, had_bom, decimal_comma }))
}

/// Decodes raw bytes to UTF-8 text, honouring a BOM, the user's choice or a
/// best guess (UTF-8 if valid, otherwise Windows-1252).
fn decode_text(raw: Vec<u8>, choice: TextEncoding) -> Result<(Cow<'static, str>, &'static str, bool)> {
    use encoding_rs::{UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};

    let (bom_enc, bom_len) = match encoding_rs::Encoding::for_bom(&raw) {
        Some((enc, len)) => (Some(enc), len),
        None => (None, 0),
    };
    let had_bom = bom_len > 0;
    let enc = match choice {
        TextEncoding::Utf8 => UTF_8,
        TextEncoding::Windows1252 => WINDOWS_1252,
        TextEncoding::Utf16 => bom_enc.filter(|e| *e == UTF_16BE).unwrap_or(UTF_16LE),
        TextEncoding::Auto => match bom_enc {
            Some(e) => e,
            None if std::str::from_utf8(&raw).is_ok() => UTF_8,
            None if looks_like_utf16le(&raw) => UTF_16LE,
            None => WINDOWS_1252,
        },
    };
    let body = if bom_enc == Some(enc) { &raw[bom_len..] } else { &raw[..] };

    if enc == UTF_8 {
        let owned = body.to_vec();
        return match String::from_utf8(owned) {
            Ok(s) => Ok((Cow::Owned(s), "UTF-8", had_bom)),
            Err(e) => Ok((
                Cow::Owned(String::from_utf8_lossy(e.as_bytes()).into_owned()),
                "UTF-8 (invalid bytes replaced)",
                had_bom,
            )),
        };
    }
    let (text, _) = enc.decode_without_bom_handling(body);
    let name = if enc == WINDOWS_1252 {
        "Windows-1252"
    } else if enc == UTF_16LE {
        "UTF-16 LE"
    } else {
        "UTF-16 BE"
    };
    Ok((Cow::Owned(text.into_owned()), name, had_bom))
}

fn looks_like_utf16le(raw: &[u8]) -> bool {
    let n = raw.len().min(4096) / 2 * 2;
    if n < 8 {
        return false;
    }
    let zeros_odd = raw[..n].iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
    zeros_odd * 10 > (n / 2) * 7
}

fn sample_lines(text: &str, skip: usize, max: usize) -> Vec<&str> {
    text.lines()
        .skip(skip)
        .filter(|l| !l.trim().is_empty())
        .take(max)
        .collect()
}

/// Counts separator characters outside double quotes.
fn count_outside_quotes(line: &str, sep: u8) -> usize {
    let mut in_q = false;
    let mut n = 0;
    for b in line.bytes() {
        if b == b'"' {
            in_q = !in_q;
        } else if b == sep && !in_q {
            n += 1;
        }
    }
    n
}

/// Picks the separator that splits the sample into the most consistent,
/// non-trivial number of fields.
pub fn sniff_separator(lines: &[&str]) -> Option<u8> {
    let candidates = [b',', b';', b'\t', b'|'];
    let mut best: Option<(u8, f64)> = None;
    for &sep in &candidates {
        let counts: Vec<usize> = lines.iter().take(50).map(|l| count_outside_quotes(l, sep)).collect();
        if counts.is_empty() {
            continue;
        }
        let first = counts[0];
        if first == 0 {
            continue;
        }
        let consistent = counts.iter().filter(|&&c| c == first).count() as f64 / counts.len() as f64;
        // Prefer consistency, then more columns.
        let score = consistent * 1000.0 + first.min(999) as f64;
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((sep, score));
        }
    }
    best.map(|(s, _)| s)
}

/// Heuristic for European CSV exports: `;`-separated with `1,5`-style numbers.
fn guess_decimal_comma(lines: &[&str], sep: u8) -> bool {
    let sep = sep as char;
    let mut comma_nums = 0;
    let mut dot_nums = 0;
    for line in lines.iter().skip(1).take(100) {
        for field in line.split(sep) {
            let f = field.trim().trim_matches('"');
            let is_num = |c: char| c.is_ascii_digit() || c == '-' || c == '+';
            if f.is_empty() || !f.chars().all(|c| is_num(c) || c == ',' || c == '.') {
                continue;
            }
            if f.matches(',').count() == 1 && !f.contains('.') {
                comma_nums += 1;
            } else if f.matches('.').count() == 1 && !f.contains(',') {
                dot_nums += 1;
            }
        }
    }
    comma_nums > 0 && comma_nums >= dot_nums
}

/// The first `max_lines` lines of a text file, decoded, for the import dialog.
pub fn preview_text(path: &Path, encoding: TextEncoding, max_lines: usize) -> Result<String> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 256 * 1024];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    if buf.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        let _ = flate2::read::MultiGzDecoder::new(&buf[..]).take(256 * 1024).read_to_end(&mut out);
        buf = out;
    }
    let (text, _, _) = decode_text(buf, encoding)?;
    Ok(text.lines().take(max_lines).collect::<Vec<_>>().join("\n"))
}

// ---------------------------------------------------------------------------
// Spreadsheets
// ---------------------------------------------------------------------------

fn read_spreadsheet(path: &Path, sheet: Option<&str>, opts: &CsvOptions) -> Result<(DataFrame, Vec<String>, String)> {
    use calamine::{Data, Reader, open_workbook_auto};
    let mut wb = open_workbook_auto(path).with_context(|| format!("Opening {}", path.display()))?;
    let sheets = worksheet_names(&wb);
    if sheets.is_empty() {
        bail!("The workbook has no sheets");
    }
    let name = match sheet {
        Some(s) if sheets.iter().any(|n| n == s) => s.to_string(),
        _ => sheets[0].clone(),
    };
    let range = wb.worksheet_range(&name).with_context(|| format!("Reading sheet '{name}'"))?;

    let rows: Vec<&[Data]> = range.rows().skip(opts.skip_rows).collect();
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let (header, body): (Vec<String>, &[&[Data]]) = if opts.has_header && !rows.is_empty() {
        let h = (0..width)
            .map(|i| rows[0].get(i).map(cell_to_string).unwrap_or_default())
            .collect();
        (h, &rows[1..])
    } else {
        (vec![String::new(); width], &rows[..])
    };
    let names = unique_names(header);

    let mut columns = Vec::with_capacity(width);
    for (ci, name) in names.iter().enumerate() {
        let cells: Vec<&Data> = body.iter().map(|r| r.get(ci).unwrap_or(&Data::Empty)).collect();
        columns.push(spreadsheet_column(name, &cells)?);
    }
    let df = DataFrame::new(body.len(), columns)?;
    Ok((df, sheets, name))
}

/// The sheets that hold cells. Chart, macro and dialog sheets have none, so
/// they are left out of the tabs (and are not kept when the workbook is saved).
fn worksheet_names<R: calamine::Reader<std::io::BufReader<File>>>(wb: &R) -> Vec<String> {
    let names: Vec<String> = wb
        .sheets_metadata()
        .iter()
        .filter(|s| s.typ == calamine::SheetType::WorkSheet)
        .map(|s| s.name.clone())
        .collect();
    if names.is_empty() { wb.sheet_names() } else { names }
}

fn cell_to_string(d: &calamine::Data) -> String {
    use calamine::Data;
    match d {
        Data::Empty => String::new(),
        Data::String(s) | Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Float(f) => format_float(*f),
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::DateTime(dt) => format_float(dt.as_f64()),
        Data::Error(e) => format!("#{e:?}"),
    }
}

fn format_float(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 { format!("{}", f as i64) } else { f.to_string() }
}

/// Builds a typed column from spreadsheet cells: numbers, booleans and
/// dates stay typed when the whole column agrees; anything mixed is text.
fn spreadsheet_column(name: &str, cells: &[&calamine::Data]) -> Result<Column> {
    use calamine::Data;
    #[derive(PartialEq, Clone, Copy)]
    enum Kind {
        Unknown,
        Int,
        Float,
        Bool,
        Date,
        Text,
    }
    let mut kind = Kind::Unknown;
    for c in cells {
        let k = match c {
            Data::Empty | Data::Error(_) => continue,
            Data::Int(_) => Kind::Int,
            Data::Float(f) => {
                if f.fract() == 0.0 { Kind::Int } else { Kind::Float }
            }
            Data::Bool(_) => Kind::Bool,
            Data::DateTime(dt) if !dt.is_duration() => Kind::Date,
            _ => Kind::Text,
        };
        kind = match (kind, k) {
            (Kind::Unknown, k) => k,
            (a, b) if a == b => a,
            (Kind::Int, Kind::Float) | (Kind::Float, Kind::Int) => Kind::Float,
            _ => Kind::Text,
        };
        if kind == Kind::Text {
            break;
        }
    }
    let name = PlSmallStr::from_str(name);
    let s = match kind {
        Kind::Int => Series::new(
            name,
            cells
                .iter()
                .map(|c| match c {
                    Data::Int(i) => Some(*i),
                    Data::Float(f) => Some(*f as i64),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        ),
        Kind::Float => Series::new(
            name,
            cells
                .iter()
                .map(|c| match c {
                    Data::Int(i) => Some(*i as f64),
                    Data::Float(f) => Some(*f),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        ),
        Kind::Bool => Series::new(
            name,
            cells
                .iter()
                .map(|c| match c {
                    Data::Bool(b) => Some(*b),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        ),
        Kind::Date => {
            // Excel serial days since 1899-12-30 -> milliseconds since 1970.
            let ms: Vec<Option<i64>> = cells
                .iter()
                .map(|c| match c {
                    Data::DateTime(dt) => Some(((dt.as_f64() - 25569.0) * 86_400_000.0).round() as i64),
                    _ => None,
                })
                .collect();
            let all_midnight = ms.iter().flatten().all(|v| v.rem_euclid(86_400_000) == 0);
            let s = Series::new(name, ms).cast(&DataType::Datetime(TimeUnit::Milliseconds, None))?;
            if all_midnight { s.cast(&DataType::Date)? } else { s }
        }
        Kind::Text | Kind::Unknown => Series::new(
            name,
            cells
                .iter()
                .map(|c| match c {
                    Data::Empty => None,
                    other => Some(cell_to_string(other)),
                })
                .collect::<Vec<_>>(),
        ),
    };
    Ok(s.into_column())
}

/// Makes column names non-empty and unique.
pub fn unique_names(names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            let base = if n.trim().is_empty() { format!("column_{}", i + 1) } else { n.trim().to_string() };
            let mut cand = base.clone();
            let mut k = 2;
            while !seen.insert(cand.clone()) {
                cand = format!("{base}_{k}");
                k += 1;
            }
            cand
        })
        .collect()
}

// ---------------------------------------------------------------------------
// SQLite (read-only)
// ---------------------------------------------------------------------------

fn read_sqlite(path: &Path, table: Option<&str>) -> Result<(DataFrame, Vec<String>, String)> {
    use rusqlite::types::ValueRef;
    use rusqlite::{Connection, OpenFlags};

    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("Opening SQLite database {}", path.display()))?;
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type IN ('table','view') \
         AND name NOT LIKE 'sqlite_%' ORDER BY type, name",
    )?;
    let tables: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    if tables.is_empty() {
        bail!("The database has no tables");
    }
    let name = match table {
        Some(t) if tables.iter().any(|n| n == t) => t.to_string(),
        _ => tables[0].clone(),
    };

    let mut stmt = conn.prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))?;
    let names = unique_names(stmt.column_names().map(|s| s.to_string()).collect());
    let ncol = names.len();

    #[derive(Clone)]
    enum V {
        Null,
        I(i64),
        F(f64),
        T(String),
    }
    let mut cols: Vec<Vec<V>> = vec![Vec::new(); ncol];
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        for (i, col) in cols.iter_mut().enumerate() {
            col.push(match row.get_ref(i)? {
                ValueRef::Null => V::Null,
                ValueRef::Integer(v) => V::I(v),
                ValueRef::Real(v) => V::F(v),
                ValueRef::Text(t) => V::T(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => V::T(format!("<blob {} bytes>", b.len())),
            });
        }
    }
    let height = cols.first().map_or(0, |c| c.len());
    let mut columns = Vec::with_capacity(ncol);
    for (vals, n) in cols.into_iter().zip(&names) {
        let has_text = vals.iter().any(|v| matches!(v, V::T(_)));
        let has_float = vals.iter().any(|v| matches!(v, V::F(_)));
        let n = PlSmallStr::from_str(n);
        let s = if has_text {
            Series::new(
                n,
                vals.into_iter()
                    .map(|v| match v {
                        V::Null => None,
                        V::I(i) => Some(i.to_string()),
                        V::F(f) => Some(f.to_string()),
                        V::T(t) => Some(t),
                    })
                    .collect::<Vec<_>>(),
            )
        } else if has_float {
            Series::new(
                n,
                vals.into_iter()
                    .map(|v| match v {
                        V::I(i) => Some(i as f64),
                        V::F(f) => Some(f),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            Series::new(
                n,
                vals.into_iter()
                    .map(|v| match v {
                        V::I(i) => Some(i),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            )
        };
        columns.push(s.into_column());
    }
    Ok((DataFrame::new(height, columns)?, tables, name))
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// How to write a file.
#[derive(Clone, Debug)]
pub struct SaveOptions {
    pub format: Format,
    pub separator: u8,
    pub bom: bool,
    pub decimal_comma: bool,
    pub sheet_name: String,
}

impl SaveOptions {
    pub fn for_format(format: Format) -> Self {
        SaveOptions {
            format,
            separator: if format == Format::Tsv { b'\t' } else { b',' },
            bom: false,
            decimal_comma: false,
            sheet_name: "Sheet1".into(),
        }
    }
}

/// Writes `df` to a temporary sibling and then replaces `path`, so a failed
/// write never destroys the original file.
pub fn save(df: &DataFrame, path: &Path, opts: &SaveOptions) -> Result<()> {
    if !opts.format.can_write() {
        bail!("{} files can't be written; use Save As with another format", opts.format.label());
    }
    replace_file(path, |tmp| write_to(df, tmp, opts))
}

/// One sheet of a workbook to write.
pub struct SheetOut {
    /// Its name in the written file.
    pub name: String,
    /// Its name in the workbook it comes from.
    pub source: String,
    /// The table to write. `None` copies the cell values from the source
    /// workbook as they are, which is right for a sheet nobody changed: no
    /// row is taken for a header and no column is forced into one type.
    pub df: Option<DataFrame>,
}

/// Writes all sheets as one `.xlsx` that replaces `path`, in the same safe
/// way as [`save`]. `source` is the workbook the unchanged sheets are copied
/// from; it may be `path` itself.
pub fn save_workbook(sheets: &[SheetOut], source: &Path, path: &Path) -> Result<()> {
    replace_file(path, |tmp| write_workbook(sheets, source, tmp))
}

/// Runs `write` on a temporary sibling of `path`, then moves it over `path`.
fn replace_file(path: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let file_name = path.file_name().ok_or_else(|| anyhow!("Invalid file name"))?;
    let tmp = path.with_file_name(format!(".{}.datavivid-tmp", file_name.to_string_lossy()));
    if let Err(e) = write(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("Replacing {} (is it open in another program?)", path.display())
    })?;
    Ok(())
}

fn write_to(df: &DataFrame, path: &Path, opts: &SaveOptions) -> Result<()> {
    let mut df = df.clone();
    match opts.format {
        Format::Csv | Format::Tsv => {
            let mut f = std::io::BufWriter::new(File::create(path)?);
            CsvWriter::new(&mut f)
                .include_header(true)
                .include_bom(opts.bom)
                .with_separator(opts.separator)
                .with_decimal_comma(opts.decimal_comma)
                .finish(&mut df)?;
        }
        Format::Parquet => {
            let f = File::create(path)?;
            ParquetWriter::new(f)
                .with_compression(ParquetCompression::Zstd(None))
                .finish(&mut df)?;
        }
        Format::Json => {
            let mut f = std::io::BufWriter::new(File::create(path)?);
            JsonWriter::new(&mut f).with_json_format(JsonFormat::Json).finish(&mut df)?;
        }
        Format::NdJson => {
            let mut f = std::io::BufWriter::new(File::create(path)?);
            JsonWriter::new(&mut f).with_json_format(JsonFormat::JsonLines).finish(&mut df)?;
        }
        Format::ArrowIpc => {
            let f = File::create(path)?;
            IpcWriter::new(f).finish(&mut df)?;
        }
        Format::Avro => {
            let f = File::create(path)?;
            polars::io::avro::AvroWriter::new(f).finish(&mut df)?;
        }
        Format::Excel => write_xlsx(&df, path, &opts.sheet_name)?,
        Format::Sqlite => write_sqlite(&df, path, &opts.sheet_name)?,
        Format::Ods => unreachable!(),
    }
    Ok(())
}

const XLSX_MAX_ROWS: usize = 1_048_575; // plus the header row
const XLSX_MAX_COLS: usize = 16_384;

/// Cell formats shared by the sheets of one written workbook.
struct XlsxFormats {
    bold: rust_xlsxwriter::Format,
    date: rust_xlsxwriter::Format,
    datetime: rust_xlsxwriter::Format,
    duration: rust_xlsxwriter::Format,
}

impl XlsxFormats {
    fn new() -> Self {
        use rust_xlsxwriter::Format as XFormat;
        XlsxFormats {
            bold: XFormat::new().set_bold(),
            date: XFormat::new().set_num_format("yyyy-mm-dd"),
            datetime: XFormat::new().set_num_format("yyyy-mm-dd hh:mm:ss"),
            duration: XFormat::new().set_num_format("[h]:mm:ss"),
        }
    }
}

/// Makes sheet names Excel accepts: at most 31 characters, none of
/// `[]:*?/\`, no apostrophe at either end, not empty, and no two alike
/// (Excel ignores case). Names that are fine already come back unchanged.
pub fn xlsx_sheet_names(names: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let clean: String = name.chars().filter(|c| !"[]:*?/\\\0".contains(*c)).collect();
            let clean = clip_utf16(clean.trim_matches('\''), 31);
            // Clipping may leave an apostrophe at the end again.
            let clean = clean.trim_end_matches('\'');
            let base = if clean.is_empty() { format!("Sheet{}", i + 1) } else { clean.to_string() };
            let mut cand = base.clone();
            let mut k = 2;
            while !seen.insert(cand.to_lowercase()) {
                let suffix = format!("_{k}");
                cand = clip_utf16(&base, 31 - suffix.len()) + &suffix;
                k += 1;
            }
            cand
        })
        .collect()
}

/// The start of `s` that fits in `max` UTF-16 units, which is how Excel
/// counts the length of a sheet name.
fn clip_utf16(s: &str, max: usize) -> String {
    let mut len = 0;
    s.chars()
        .take_while(|c| {
            len += c.len_utf16();
            len <= max
        })
        .collect()
}

fn write_xlsx(df: &DataFrame, path: &Path, sheet: &str) -> Result<()> {
    let mut wb = rust_xlsxwriter::Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name(&xlsx_sheet_names(&[sheet.to_string()])[0])?;
    write_table(ws, df, &XlsxFormats::new())?;
    wb.save(path)?;
    Ok(())
}

fn write_workbook(sheets: &[SheetOut], source: &Path, path: &Path) -> Result<()> {
    use calamine::{Reader, SheetVisible, open_workbook_auto};
    // The source is read in full before `path` is written, and it is closed
    // when this function returns, so `source` and the final file may be the
    // same one.
    let mut src = match open_workbook_auto(source) {
        Ok(wb) => Some(wb),
        Err(e) if sheets.iter().any(|s| s.df.is_none()) => {
            return Err(e).with_context(|| format!("Reading the other sheets from {}", source.display()));
        }
        Err(_) => None,
    };
    let hidden: Vec<bool> = sheets
        .iter()
        .map(|s| {
            src.as_ref().is_some_and(|wb| {
                wb.sheets_metadata().iter().any(|m| m.name == s.source && m.visible != SheetVisible::Visible)
            })
        })
        .collect();
    // Excel needs a visible sheet to open on.
    let first_visible = hidden.iter().position(|h| !h);

    let formats = XlsxFormats::new();
    let mut wb = rust_xlsxwriter::Workbook::new();
    for (i, sheet) in sheets.iter().enumerate() {
        let ws = wb.add_worksheet();
        ws.set_name(&sheet.name)?;
        match (&sheet.df, &mut src) {
            (Some(df), _) => write_table(ws, df, &formats).with_context(|| format!("Sheet '{}'", sheet.name))?,
            (None, Some(src)) => {
                let range = src
                    .worksheet_range(&sheet.source)
                    .with_context(|| format!("Reading sheet '{}' from {}", sheet.source, source.display()))?;
                copy_cells(ws, &range, &formats).with_context(|| format!("Sheet '{}'", sheet.name))?;
            }
            (None, None) => unreachable!("the source was opened for the sheets that are copied"),
        }
        if hidden[i] && first_visible.is_some() {
            ws.set_hidden(true);
        } else if first_visible == Some(i) && i > 0 {
            ws.set_active(true);
        }
    }
    wb.save(path)?;
    Ok(())
}

/// Writes a table as a sheet: a bold header row, then typed values.
fn write_table(ws: &mut rust_xlsxwriter::Worksheet, df: &DataFrame, formats: &XlsxFormats) -> Result<()> {
    if df.height() > XLSX_MAX_ROWS || df.width() > XLSX_MAX_COLS {
        bail!(
            "Excel sheets hold at most {XLSX_MAX_ROWS} rows and {XLSX_MAX_COLS} columns; this table has {} x {}",
            df.height(),
            df.width()
        );
    }
    for (ci, col) in df.columns().iter().enumerate() {
        let c = ci as u16;
        ws.write_string_with_format(0, c, col.name().as_str(), &formats.bold)?;
        let dtype = col.dtype().clone();
        let series = col.as_materialized_series();
        if dtype.is_primitive_numeric() {
            let f = series.cast(&DataType::Float64)?;
            for (ri, v) in f.f64()?.iter().enumerate() {
                if let Some(v) = v {
                    if v.is_finite() {
                        ws.write_number(ri as u32 + 1, c, v)?;
                    }
                }
            }
        } else if dtype == DataType::Boolean {
            for (ri, v) in series.bool()?.iter().enumerate() {
                if let Some(v) = v {
                    ws.write_boolean(ri as u32 + 1, c, v)?;
                }
            }
        } else if matches!(dtype, DataType::Date | DataType::Datetime(_, _)) {
            // Excel serial = days since 1899-12-30.
            let ms = series
                .cast(&DataType::Datetime(TimeUnit::Milliseconds, None))?
                .cast(&DataType::Int64)?;
            let fmt = if dtype == DataType::Date { &formats.date } else { &formats.datetime };
            for (ri, v) in ms.i64()?.iter().enumerate() {
                if let Some(v) = v {
                    let serial = v as f64 / 86_400_000.0 + 25569.0;
                    ws.write_number_with_format(ri as u32 + 1, c, serial, fmt)?;
                }
            }
            ws.set_column_width(c, if dtype == DataType::Date { 11 } else { 19 })?;
        } else {
            let s = series.cast(&DataType::String)?;
            for (ri, v) in s.str()?.iter().enumerate() {
                if let Some(v) = v {
                    ws.write_string(ri as u32 + 1, c, v)?;
                }
            }
        }
    }
    ws.set_freeze_panes(1, 0)?;
    Ok(())
}

/// Copies the cell values of a sheet to the same cells of a new one.
/// Formulas arrive as their last results; formatting is not copied.
fn copy_cells(
    ws: &mut rust_xlsxwriter::Worksheet,
    range: &calamine::Range<calamine::Data>,
    formats: &XlsxFormats,
) -> Result<()> {
    use calamine::Data;
    let Some((row0, col0)) = range.start() else { return Ok(()) };
    let (rows, cols) = range.get_size();
    if row0 as usize + rows > XLSX_MAX_ROWS + 1 || col0 as usize + cols > XLSX_MAX_COLS {
        bail!("Excel sheets hold at most {} rows and {XLSX_MAX_COLS} columns", XLSX_MAX_ROWS + 1);
    }
    for (r, c, cell) in range.used_cells() {
        let (row, col) = (row0 + r as u32, (col0 as usize + c) as u16);
        match cell {
            Data::Empty => {}
            Data::String(s) | Data::DateTimeIso(s) | Data::DurationIso(s) => {
                ws.write_string(row, col, s)?;
            }
            Data::Float(f) if f.is_finite() => {
                ws.write_number(row, col, *f)?;
            }
            Data::Float(_) => {}
            Data::Int(i) => {
                ws.write_number(row, col, *i as f64)?;
            }
            Data::Bool(b) => {
                ws.write_boolean(row, col, *b)?;
            }
            Data::DateTime(dt) => {
                let serial = dt.as_f64();
                let fmt = if dt.is_duration() {
                    &formats.duration
                } else if serial.fract() == 0.0 {
                    &formats.date
                } else {
                    &formats.datetime
                };
                ws.write_number_with_format(row, col, serial, fmt)?;
            }
            Data::Error(e) => {
                ws.write_string(row, col, e.to_string())?;
            }
        }
    }
    Ok(())
}

/// Writes a new SQLite database file holding one table.
fn write_sqlite(df: &DataFrame, path: &Path, table: &str) -> Result<()> {
    use rusqlite::{Connection, params_from_iter, types::Value};
    let _ = std::fs::remove_file(path); // `path` is our fresh temp file
    let mut conn = Connection::open(path)?;
    let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let table = if table.trim().is_empty() { "data" } else { table.trim() };

    let mut defs = Vec::new();
    for col in df.columns() {
        let t = match col.dtype() {
            d if d.is_integer() || *d == DataType::Boolean => "INTEGER",
            d if d.is_float() => "REAL",
            _ => "TEXT",
        };
        defs.push(format!("{} {t}", quote(col.name())));
    }
    conn.execute(&format!("CREATE TABLE {} ({})", quote(table), defs.join(", ")), [])?;

    // Materialise column values in SQLite's types.
    let mut cols: Vec<Vec<Value>> = Vec::with_capacity(df.width());
    for col in df.columns() {
        let s = col.as_materialized_series();
        let d = s.dtype();
        let vals: Vec<Value> = if d.is_integer() {
            s.cast(&DataType::Int64)?.i64()?.iter().map(|v| v.map_or(Value::Null, Value::Integer)).collect()
        } else if *d == DataType::Boolean {
            s.bool()?.iter().map(|v| v.map_or(Value::Null, |b| Value::Integer(b as i64))).collect()
        } else if d.is_float() {
            s.cast(&DataType::Float64)?.f64()?.iter().map(|v| v.map_or(Value::Null, Value::Real)).collect()
        } else {
            s.cast(&DataType::String)?
                .str()?
                .iter()
                .map(|v| v.map_or(Value::Null, |t| Value::Text(t.to_string())))
                .collect()
        };
        cols.push(vals);
    }

    let tx = conn.transaction()?;
    {
        let placeholders = vec!["?"; df.width()].join(", ");
        let mut stmt = tx.prepare(&format!("INSERT INTO {} VALUES ({placeholders})", quote(table)))?;
        for r in 0..df.height() {
            stmt.execute(params_from_iter(cols.iter().map(|c| &c[r])))?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_separators() {
        assert_eq!(sniff_separator(&["a;b;c", "1;2;3"]), Some(b';'));
        assert_eq!(sniff_separator(&["a,b,c", "1,\"x;y\",3"]), Some(b','));
        assert_eq!(sniff_separator(&["a\tb", "1\t2"]), Some(b'\t'));
        assert_eq!(sniff_separator(&["a|b|c|d", "1|2|3|4"]), Some(b'|'));
    }

    #[test]
    fn decimal_comma_guess() {
        assert!(guess_decimal_comma(&["a;b", "1,5;2,25", "3,0;4"], b';'));
        assert!(!guess_decimal_comma(&["a;b", "1.5;2.25"], b';'));
    }

    #[test]
    fn names_are_unique() {
        assert_eq!(
            unique_names(vec!["a".into(), "".into(), "a".into()]),
            vec!["a", "column_2", "a_2"]
        );
    }

    fn roundtrip(ext: &str) {
        let dir = tempfile::tempdir().unwrap();
        let df = df!(
            "id" => [1i64, 2, 3],
            "name" => [Some("Anna"), None, Some("Jörg; \"quoted\"")],
            "score" => [1.5f64, 2.25, -3.0],
            "ok" => [true, false, true],
        )
        .unwrap();
        let path = dir.path().join(format!("t.{ext}"));
        let fmt = Format::from_path(&path).unwrap();
        save(&df, &path, &SaveOptions::for_format(fmt)).unwrap();
        let back = load(LoadRequest::new(&path).unwrap()).unwrap().df;
        assert_eq!(back.shape(), df.shape(), "{ext}");
        assert_eq!(
            back.column("name").unwrap().get(2).unwrap().str_value(),
            "Jörg; \"quoted\"",
            "{ext}"
        );
        let score = back.column("score").unwrap().cast(&DataType::Float64).unwrap();
        assert_eq!(score.f64().unwrap().get(1), Some(2.25), "{ext}");
    }

    #[test]
    fn roundtrip_all_writable_formats() {
        for ext in ["csv", "tsv", "parquet", "json", "jsonl", "arrow", "avro", "xlsx", "sqlite"] {
            roundtrip(ext);
        }
    }

    /// A workbook as a user might have it: a plain table, a sheet whose
    /// cells are no table at all, and a hidden sheet.
    fn three_sheet_workbook(path: &Path) {
        use rust_xlsxwriter::Workbook;
        let mut wb = Workbook::new();
        let ws = wb.add_worksheet();
        ws.set_name("Sales").unwrap();
        ws.write_string(0, 0, "region").unwrap();
        ws.write_string(0, 1, "amount").unwrap();
        for (i, (r, a)) in [("North", 10.5), ("South", 20.0), ("West", 7.25)].iter().enumerate() {
            ws.write_string(i as u32 + 1, 0, *r).unwrap();
            ws.write_number(i as u32 + 1, 1, *a).unwrap();
        }
        let ws = wb.add_worksheet();
        ws.set_name("Notes").unwrap();
        // Starts at C3, mixes text and numbers in one column, repeats a header.
        ws.write_string(2, 2, "Report 2026").unwrap();
        ws.write_string(4, 2, "x").unwrap();
        ws.write_string(4, 3, "x").unwrap();
        ws.write_number(5, 2, 1.0).unwrap();
        ws.write_string(5, 3, "one").unwrap();
        ws.write_string(6, 2, "n/a").unwrap();
        ws.write_boolean(6, 3, true).unwrap();
        let ws = wb.add_worksheet();
        ws.set_name("Lookup").unwrap();
        ws.write_string(0, 0, "code").unwrap();
        ws.write_number(1, 0, 42.0).unwrap();
        ws.set_hidden(true);
        wb.save(path).unwrap();
    }

    fn sheet(path: &Path, name: &str) -> Loaded {
        let mut req = LoadRequest::new(path).unwrap();
        req.table = Some(name.to_string());
        let l = load(req).unwrap();
        assert_eq!(l.table.as_deref(), Some(name));
        l
    }

    #[test]
    fn saving_a_workbook_keeps_every_sheet() {
        use calamine::{Data, Reader, SheetVisible, open_workbook_auto};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        three_sheet_workbook(&path);
        assert_eq!(sheet(&path, "Sales").tables, ["Sales", "Notes", "Lookup"]);

        // Change one sheet, as the app would, and save over the same file.
        let edited = df!("region" => ["North", "South"], "amount" => [11.5f64, 20.0]).unwrap();
        let out = |df: Option<DataFrame>, name: &str| SheetOut { name: name.into(), source: name.into(), df };
        let sheets = [out(Some(edited), "Sales"), out(None, "Notes"), out(None, "Lookup")];
        save_workbook(&sheets, &path, &path).unwrap();

        let sales = sheet(&path, "Sales");
        assert_eq!(sales.tables, ["Sales", "Notes", "Lookup"]);
        assert_eq!(sales.df.shape(), (2, 2));
        assert_eq!(sales.df.column("amount").unwrap().f64().unwrap().get(0), Some(11.5));

        // The untouched sheets keep their cells where they were, with their types.
        let mut wb = open_workbook_auto(&path).unwrap();
        let notes = wb.worksheet_range("Notes").unwrap();
        assert_eq!(notes.start(), Some((2, 2)));
        assert_eq!(notes.get_value((2, 2)), Some(&Data::String("Report 2026".into())));
        assert_eq!(notes.get_value((5, 2)), Some(&Data::Float(1.0)));
        assert_eq!(notes.get_value((6, 2)), Some(&Data::String("n/a".into())));
        assert_eq!(notes.get_value((6, 3)), Some(&Data::Bool(true)));
        assert_eq!(notes.get_value((4, 3)), Some(&Data::String("x".into())));
        let lookup = wb.sheets_metadata().iter().find(|s| s.name == "Lookup").unwrap();
        assert_eq!(lookup.visible, SheetVisible::Hidden);
        assert_eq!(sheet(&path, "Lookup").df.column("code").unwrap().i64().unwrap().get(0), Some(42));
        // No temporary file is left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_hidden_first_sheet_stays_hidden() {
        use calamine::{Reader, SheetVisible, open_workbook_auto};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        let mut wb = rust_xlsxwriter::Workbook::new();
        let ws = wb.add_worksheet();
        ws.set_name("Config").unwrap();
        ws.write_string(0, 0, "key").unwrap();
        ws.set_hidden(true);
        let ws = wb.add_worksheet();
        ws.set_name("Data").unwrap();
        ws.write_string(0, 0, "x").unwrap();
        ws.write_number(1, 0, 1.0).unwrap();
        ws.set_active(true);
        wb.save(&path).unwrap();

        let out = |name: &str| SheetOut { name: name.into(), source: name.into(), df: None };
        save_workbook(&[out("Config"), out("Data")], &path, &path).unwrap();
        let wb = open_workbook_auto(&path).unwrap();
        let visible: Vec<_> = wb.sheets_metadata().iter().map(|s| (s.name.as_str(), s.visible)).collect();
        assert_eq!(visible, [("Config", SheetVisible::Hidden), ("Data", SheetVisible::Visible)]);
        assert_eq!(sheet(&path, "Data").df.shape(), (1, 1));
    }

    #[test]
    fn a_failed_workbook_save_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        three_sheet_workbook(&path);
        let before = std::fs::read(&path).unwrap();
        let sheets = [SheetOut { name: "Gone".into(), source: "Gone".into(), df: None }];
        assert!(save_workbook(&sheets, &path, &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn sheet_names_for_excel() {
        let names = |v: &[&str]| xlsx_sheet_names(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(names(&["Sales", "Q1 2026", "Übersicht"]), ["Sales", "Q1 2026", "Übersicht"]);
        assert_eq!(names(&["a/b", "'quoted'", "", "data", "DATA"]), ["ab", "quoted", "Sheet3", "data", "DATA_2"]);
        let long = "x".repeat(40);
        let out = names(&[&long, &long]);
        assert_eq!(out[0].len(), 31);
        assert_eq!(out[1].len(), 31);
        assert!(out[1].ends_with("_2"));
    }

    #[test]
    fn reads_semicolon_windows1252_with_decimal_comma() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eu.csv");
        // "Größe" in Windows-1252
        let bytes = b"Name;Gr\xf6\xdfe\nA;1,5\nB;2,25\n".to_vec();
        std::fs::write(&path, bytes).unwrap();
        let l = load(LoadRequest::new(&path).unwrap()).unwrap();
        let csv = l.csv.unwrap();
        assert_eq!(csv.separator, b';');
        assert_eq!(csv.encoding, "Windows-1252");
        assert!(csv.decimal_comma);
        assert_eq!(l.df.get_column_names()[1].as_str(), "Größe");
        assert_eq!(l.df.column("Größe").unwrap().f64().unwrap().get(1), Some(2.25));
    }
}
