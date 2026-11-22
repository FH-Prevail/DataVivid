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
    let sheets = wb.sheet_names();
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
    let file_name = path.file_name().ok_or_else(|| anyhow!("Invalid file name"))?;
    let tmp = path.with_file_name(format!(".{}.datavivid-tmp", file_name.to_string_lossy()));
    let result = write_to(df, &tmp, opts);
    if let Err(e) = result {
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

fn write_xlsx(df: &DataFrame, path: &Path, sheet: &str) -> Result<()> {
    use rust_xlsxwriter::{Format as XFormat, Workbook};
    if df.height() > XLSX_MAX_ROWS || df.width() > XLSX_MAX_COLS {
        bail!(
            "Excel sheets hold at most {XLSX_MAX_ROWS} rows and {XLSX_MAX_COLS} columns; this table has {} x {}",
            df.height(),
            df.width()
        );
    }
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet();
    let sheet: String = sheet.chars().filter(|c| !"[]:*?/\\".contains(*c)).take(31).collect();
    ws.set_name(if sheet.is_empty() { "Sheet1" } else { &sheet })?;
    let bold = XFormat::new().set_bold();
    let date_fmt = XFormat::new().set_num_format("yyyy-mm-dd");
    let dt_fmt = XFormat::new().set_num_format("yyyy-mm-dd hh:mm:ss");

    for (ci, col) in df.columns().iter().enumerate() {
        let c = ci as u16;
        ws.write_string_with_format(0, c, col.name().as_str(), &bold)?;
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
            let fmt = if dtype == DataType::Date { &date_fmt } else { &dt_fmt };
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
    wb.save(path)?;
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
