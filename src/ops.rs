//! Data operations. Every change to the table is an `Op`, so it can be shown
//! in the steps list, undone, and replayed.

use anyhow::{Result, anyhow, bail};
use polars::prelude::*;

const MASK: &str = "__datavivid_mask__";
const VAL: &str = "__datavivid_val__";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    NotEq,
    Gt,
    GtEq,
    Lt,
    LtEq,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    Regex,
    IsNull,
    IsNotNull,
}

impl Cmp {
    pub const ALL: [Cmp; 13] = [
        Cmp::Eq,
        Cmp::NotEq,
        Cmp::Gt,
        Cmp::GtEq,
        Cmp::Lt,
        Cmp::LtEq,
        Cmp::Contains,
        Cmp::NotContains,
        Cmp::StartsWith,
        Cmp::EndsWith,
        Cmp::Regex,
        Cmp::IsNull,
        Cmp::IsNotNull,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Cmp::Eq => "equals",
            Cmp::NotEq => "does not equal",
            Cmp::Gt => "greater than",
            Cmp::GtEq => "greater or equal",
            Cmp::Lt => "less than",
            Cmp::LtEq => "less or equal",
            Cmp::Contains => "contains",
            Cmp::NotContains => "does not contain",
            Cmp::StartsWith => "starts with",
            Cmp::EndsWith => "ends with",
            Cmp::Regex => "matches regex",
            Cmp::IsNull => "is missing",
            Cmp::IsNotNull => "is not missing",
        }
    }
    pub fn needs_value(self) -> bool {
        !matches!(self, Cmp::IsNull | Cmp::IsNotNull)
    }
    fn is_text(self) -> bool {
        matches!(
            self,
            Cmp::Contains | Cmp::NotContains | Cmp::StartsWith | Cmp::EndsWith | Cmp::Regex
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetType {
    Int64,
    Float64,
    String,
    Boolean,
    Date,
    Datetime,
    Categorical,
}

impl TargetType {
    pub const ALL: [TargetType; 7] = [
        TargetType::String,
        TargetType::Int64,
        TargetType::Float64,
        TargetType::Boolean,
        TargetType::Date,
        TargetType::Datetime,
        TargetType::Categorical,
    ];
    pub fn label(self) -> &'static str {
        match self {
            TargetType::Int64 => "Integer (Int64)",
            TargetType::Float64 => "Decimal (Float64)",
            TargetType::String => "Text (String)",
            TargetType::Boolean => "Boolean",
            TargetType::Date => "Date",
            TargetType::Datetime => "Date & time",
            TargetType::Categorical => "Category",
        }
    }
    fn dtype(self) -> DataType {
        match self {
            TargetType::Int64 => DataType::Int64,
            TargetType::Float64 => DataType::Float64,
            TargetType::String => DataType::String,
            TargetType::Boolean => DataType::Boolean,
            TargetType::Date => DataType::Date,
            TargetType::Datetime => DataType::Datetime(TimeUnit::Microseconds, None),
            TargetType::Categorical => DataType::from_categories(Categories::global()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillStrategy {
    Value,
    Mean,
    Median,
    Min,
    Max,
    Forward,
    Backward,
}

impl FillStrategy {
    pub const ALL: [FillStrategy; 7] = [
        FillStrategy::Value,
        FillStrategy::Mean,
        FillStrategy::Median,
        FillStrategy::Min,
        FillStrategy::Max,
        FillStrategy::Forward,
        FillStrategy::Backward,
    ];
    pub fn label(self) -> &'static str {
        match self {
            FillStrategy::Value => "a fixed value",
            FillStrategy::Mean => "the mean",
            FillStrategy::Median => "the median",
            FillStrategy::Min => "the minimum",
            FillStrategy::Max => "the maximum",
            FillStrategy::Forward => "the previous value",
            FillStrategy::Backward => "the next value",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextTransform {
    Trim,
    Lower,
    Upper,
    Title,
}

impl TextTransform {
    pub const ALL: [TextTransform; 4] =
        [TextTransform::Trim, TextTransform::Lower, TextTransform::Upper, TextTransform::Title];
    pub fn label(self) -> &'static str {
        match self {
            TextTransform::Trim => "Trim whitespace",
            TextTransform::Lower => "lowercase",
            TextTransform::Upper => "UPPERCASE",
            TextTransform::Title => "Title Case",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// Sets cells of one column. `None` stores a missing value.
    SetCells { column: String, rows: Vec<usize>, values: Vec<Option<String>> },
    Sort { columns: Vec<(String, bool)> },
    Filter { column: String, cmp: Cmp, value: String, case_sensitive: bool },
    Rename { from: String, to: String },
    DropColumns { columns: Vec<String> },
    /// Adds a column computed from a SQL expression such as `price * qty`;
    /// an empty formula adds an empty text column.
    AddColumn { name: String, formula: String, position: usize },
    Cast { column: String, to: TargetType, format: String, decimal_comma: bool },
    FillMissing { column: String, strategy: FillStrategy, value: String },
    DropMissing { columns: Vec<String> },
    DropDuplicates { columns: Vec<String> },
    FindReplace {
        columns: Vec<String>,
        find: String,
        replace: String,
        regex: bool,
        match_case: bool,
        whole_cell: bool,
    },
    Text { column: String, transform: TextTransform },
    Round { column: String, decimals: u32 },
    InsertRows { at: usize, count: usize },
    DeleteRows { rows: Vec<usize> },
    MoveColumn { column: String, to: usize },
    Sql { query: String },
    /// Several operations shown and undone as one step (e.g. a paste).
    Batch { label: String, ops: Vec<Op> },
}

impl Op {
    /// One line for the steps list.
    pub fn describe(&self) -> String {
        let list = |v: &[String]| {
            if v.is_empty() { "all columns".to_string() } else { v.join(", ") }
        };
        match self {
            Op::Batch { label, .. } => label.clone(),
            Op::SetCells { column, rows, values } if rows.len() == 1 => format!(
                "Edit {column} row {} = {}",
                rows[0] + 1,
                values[0].as_deref().map_or("(missing)".into(), |v| format!("\"{}\"", short(v)))
            ),
            Op::SetCells { column, rows, .. } => format!("Edit {} cells in {column}", rows.len()),
            Op::Sort { columns } => format!(
                "Sort by {}",
                columns
                    .iter()
                    .map(|(c, d)| format!("{c} {}", if *d { "↓" } else { "↑" }))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Op::Filter { column, cmp, value, .. } => {
                if cmp.needs_value() {
                    format!("Keep rows where {column} {} \"{}\"", cmp.label(), short(value))
                } else {
                    format!("Keep rows where {column} {}", cmp.label())
                }
            }
            Op::Rename { from, to } => format!("Rename {from} → {to}"),
            Op::DropColumns { columns } => format!("Delete column(s) {}", list(columns)),
            Op::AddColumn { name, formula, .. } if formula.trim().is_empty() => format!("Add column {name}"),
            Op::AddColumn { name, formula, .. } => format!("Add column {name} = {}", short(formula)),
            Op::Cast { column, to, .. } => format!("Change {column} to {}", to.label()),
            Op::FillMissing { column, strategy, value } => match strategy {
                FillStrategy::Value => format!("Fill missing {column} with \"{}\"", short(value)),
                s => format!("Fill missing {column} with {}", s.label()),
            },
            Op::DropMissing { columns } => format!("Drop rows with missing values in {}", list(columns)),
            Op::DropDuplicates { columns } => format!("Drop duplicate rows by {}", list(columns)),
            Op::FindReplace { columns, find, replace, .. } => {
                format!("Replace \"{}\" with \"{}\" in {}", short(find), short(replace), list(columns))
            }
            Op::Text { column, transform } => format!("{} in {column}", transform.label()),
            Op::Round { column, decimals } => format!("Round {column} to {decimals} decimals"),
            Op::InsertRows { at, count } => format!("Insert {count} row(s) at {}", at + 1),
            Op::DeleteRows { rows } => format!("Delete {} row(s)", rows.len()),
            Op::MoveColumn { column, to } => format!("Move {column} to position {}", to + 1),
            Op::Sql { query } => format!("SQL: {}", short(query)),
        }
    }

    pub fn apply(&self, df: &DataFrame) -> Result<DataFrame> {
        let out = match self {
            Op::Batch { ops, .. } => {
                let mut cur = df.clone();
                for op in ops {
                    cur = op.apply(&cur)?;
                }
                cur
            }
            Op::SetCells { column, rows, values } => set_cells(df, column, rows, values)?,
            Op::Sort { columns } => {
                let names: Vec<&str> = columns.iter().map(|(c, _)| c.as_str()).collect();
                let desc: Vec<bool> = columns.iter().map(|(_, d)| *d).collect();
                df.sort(
                    names,
                    SortMultipleOptions::default()
                        .with_order_descending_multi(desc)
                        .with_nulls_last(true)
                        .with_maintain_order(true),
                )?
            }
            Op::Filter { column, cmp, value, case_sensitive } => {
                let pred = filter_expr(df, column, *cmp, value, *case_sensitive)?;
                df.clone().lazy().filter(pred).collect()?
            }
            Op::Rename { from, to } => {
                let to = to.trim();
                if to.is_empty() {
                    bail!("The new name is empty");
                }
                if to != from && df.get_column_index(to).is_some() {
                    bail!("A column named \"{to}\" already exists");
                }
                let mut out = df.clone();
                out.rename(from, PlSmallStr::from_str(to))?;
                out
            }
            Op::DropColumns { columns } => {
                for c in columns {
                    df.try_get_column_index(c)?;
                }
                df.drop_many(columns.iter().map(|s| s.as_str()))
            }
            Op::AddColumn { name, formula, position } => {
                let name = name.trim();
                if name.is_empty() {
                    bail!("The column name is empty");
                }
                if df.get_column_index(name).is_some() {
                    bail!("A column named \"{name}\" already exists");
                }
                let expr = if formula.trim().is_empty() {
                    lit(NULL).cast(DataType::String)
                } else {
                    polars::sql::sql_expr(formula).map_err(|e| anyhow!("Formula: {e}"))?
                };
                let mut out = df.clone().lazy().with_column(expr.alias(name)).collect()?;
                let pos = (*position).min(df.width());
                let col = out.column(name)?.clone();
                out = out.drop(name)?;
                out.insert_column(pos, col)?;
                out
            }
            Op::Cast { column, to, format, decimal_comma } => {
                df.clone().lazy().with_column(cast_expr(df, column, *to, format, *decimal_comma)?).collect()?
            }
            Op::FillMissing { column, strategy, value } => {
                let dtype = df.column(column)?.dtype().clone();
                let c = col(column.as_str());
                let e = match strategy {
                    FillStrategy::Value => c.fill_null(typed_lit(value, &dtype)?),
                    FillStrategy::Mean => c.clone().fill_null(col(column.as_str()).mean()),
                    FillStrategy::Median => c.clone().fill_null(col(column.as_str()).median()),
                    FillStrategy::Min => c.clone().fill_null(col(column.as_str()).min()),
                    FillStrategy::Max => c.clone().fill_null(col(column.as_str()).max()),
                    FillStrategy::Forward => c.fill_null_with_strategy(FillNullStrategy::Forward(None)),
                    FillStrategy::Backward => c.fill_null_with_strategy(FillNullStrategy::Backward(None)),
                };
                // Mean/median of an integer column is fractional: keep the type.
                let e = if matches!(strategy, FillStrategy::Mean | FillStrategy::Median) && dtype.is_integer() {
                    e.cast(DataType::Float64)
                } else {
                    e
                };
                df.clone().lazy().with_column(e.alias(column.as_str())).collect()?
            }
            Op::DropMissing { columns } => {
                if columns.is_empty() {
                    df.drop_nulls::<String>(None)?
                } else {
                    df.drop_nulls(Some(columns.as_slice()))?
                }
            }
            Op::DropDuplicates { columns } => {
                let subset = if columns.is_empty() { None } else { Some(columns.as_slice()) };
                df.unique_stable(subset, UniqueKeepStrategy::First, None)?
            }
            Op::FindReplace { columns, find, replace, regex, match_case, whole_cell } => {
                find_replace(df, columns, find, replace, *regex, *match_case, *whole_cell)?
            }
            Op::Text { column, transform } => {
                let c = col(column.as_str()).cast(DataType::String);
                let e = match transform {
                    TextTransform::Trim => c.str().strip_chars(lit(NULL)),
                    TextTransform::Lower => c.str().to_lowercase(),
                    TextTransform::Upper => c.str().to_uppercase(),
                    TextTransform::Title => {
                        return title_case(df, column);
                    }
                };
                df.clone().lazy().with_column(e.alias(column.as_str())).collect()?
            }
            Op::Round { column, decimals } => {
                let dtype = df.column(column)?.dtype().clone();
                if !dtype.is_float() {
                    bail!("Only decimal columns can be rounded ({column} is {dtype})");
                }
                df.clone()
                    .lazy()
                    .with_column(col(column.as_str()).round(*decimals, RoundMode::HalfAwayFromZero))
                    .collect()?
            }
            Op::InsertRows { at, count } => {
                let at = (*at).min(df.height());
                let blank = DataFrame::full_null(df.schema(), *count);
                let mut out = df.slice(0, at);
                out.vstack_mut(&blank)?;
                out.vstack_mut(&df.slice(at as i64, df.height() - at))?;
                out.rechunk_mut_par();
                out
            }
            Op::DeleteRows { rows } => {
                let set: std::collections::HashSet<usize> = rows.iter().copied().collect();
                let mask: BooleanChunked = (0..df.height()).map(|i| !set.contains(&i)).collect();
                df.filter(&mask)?
            }
            Op::MoveColumn { column, to } => {
                let mut out = df.clone();
                let i = out.try_get_column_index(column)?;
                let c = out.column(column)?.clone();
                out = out.drop(column)?;
                out.insert_column((*to).min(out.width()), c)?;
                let _ = i;
                out
            }
            Op::Sql { query } => {
                let mut ctx = polars::sql::SQLContext::new();
                ctx.register("data", df.clone().lazy());
                ctx.register("df", df.clone().lazy());
                ctx.execute(query).map_err(|e| anyhow!("SQL: {e}"))?.collect()?
            }
        };
        Ok(out)
    }
}

fn short(s: &str) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > 40 {
        format!("{}…", s.chars().take(39).collect::<String>())
    } else {
        s
    }
}

/// A literal of `dtype` parsed from user text, or an error that names the type.
pub fn typed_lit(value: &str, dtype: &DataType) -> Result<Expr> {
    let s = Series::new(PlSmallStr::from_static("v"), [value]);
    let v = parse_series(&s, dtype).map_err(|_| anyhow!("\"{value}\" is not a valid {}", type_name(dtype)))?;
    Ok(lit(v).first())
}

pub fn type_name(dtype: &DataType) -> String {
    match dtype {
        d if d.is_integer() => "whole number".into(),
        d if d.is_float() => "number".into(),
        DataType::Boolean => "true/false value".into(),
        DataType::Date => "date (YYYY-MM-DD)".into(),
        DataType::Datetime(_, _) => "date & time (YYYY-MM-DD HH:MM:SS)".into(),
        DataType::Time => "time (HH:MM:SS)".into(),
        d => d.to_string(),
    }
}

/// Strictly converts a String series to `dtype`. Boolean accepts
/// true/false/yes/no/1/0, everything else uses Polars' own string casts.
fn parse_series(s: &Series, dtype: &DataType) -> Result<Series> {
    if *dtype == DataType::String {
        return Ok(s.clone());
    }
    if *dtype == DataType::Boolean {
        let ca = s.str()?;
        let mut bad = None;
        let out: BooleanChunked = ca
            .iter()
            .map(|v| {
                v.and_then(|t| match t.trim().to_ascii_lowercase().as_str() {
                    "true" | "yes" | "y" | "1" | "wahr" | "ja" => Some(true),
                    "false" | "no" | "n" | "0" | "falsch" | "nein" => Some(false),
                    "" => None,
                    other => {
                        bad = Some(other.to_string());
                        None
                    }
                })
            })
            .collect();
        if let Some(b) = bad {
            bail!("\"{b}\" is not true/false");
        }
        return Ok(out.with_name(s.name().clone()).into_series());
    }
    let trimmed = s.str()?.apply_values(|v| std::borrow::Cow::Owned(v.trim().to_string()));
    Ok(trimmed.into_series().strict_cast(dtype)?)
}

fn set_cells(df: &DataFrame, column: &str, rows: &[usize], values: &[Option<String>]) -> Result<DataFrame> {
    if rows.len() != values.len() {
        bail!("internal: rows and values differ in length");
    }
    let target = df.column(column)?;
    let dtype = target.dtype().clone();
    let h = df.height();
    let mut mask = vec![false; h];
    let mut vals: Vec<Option<&str>> = vec![None; h];
    for (&r, v) in rows.iter().zip(values) {
        if r >= h {
            bail!("Row {} does not exist", r + 1);
        }
        mask[r] = true;
        vals[r] = v.as_deref();
    }
    let val_series = Series::new(PlSmallStr::from_static(VAL), vals);
    let parsed = parse_series(&val_series, &dtype).map_err(|_| {
        let bad = values
            .iter()
            .flatten()
            .find(|v| {
                let s = Series::new(PlSmallStr::from_static("v"), [v.as_str()]);
                parse_series(&s, &dtype).is_err()
            })
            .cloned()
            .unwrap_or_default();
        anyhow!("\"{bad}\" is not a valid {} for column {column}", type_name(&dtype))
    })?;
    let helpers = DataFrame::new(
        h,
        vec![
            Series::new(PlSmallStr::from_static(MASK), mask).into_column(),
            parsed.with_name(PlSmallStr::from_static(VAL)).into_column(),
        ],
    )?;
    let out = df
        .hstack(helpers.columns())?
        .lazy()
        .with_column(
            when(col(MASK))
                .then(col(VAL))
                .otherwise(col(column))
                .alias(column),
        )
        .drop(cols([MASK, VAL]))
        .collect()?;
    Ok(out)
}

fn filter_expr(df: &DataFrame, column: &str, cmp: Cmp, value: &str, case_sensitive: bool) -> Result<Expr> {
    let dtype = df.column(column)?.dtype().clone();
    let c = col(column);
    if !cmp.needs_value() {
        return Ok(if cmp == Cmp::IsNull { c.is_null() } else { c.is_not_null() });
    }
    if cmp.is_text() {
        let mut text = c.cast(DataType::String);
        let mut v = value.to_string();
        if !case_sensitive && cmp != Cmp::Regex {
            text = text.str().to_lowercase();
            v = v.to_lowercase();
        }
        let e = match cmp {
            Cmp::Contains => text.str().contains_literal(lit(v)),
            Cmp::NotContains => text.str().contains_literal(lit(v)).not(),
            Cmp::StartsWith => text.str().starts_with(lit(v)),
            Cmp::EndsWith => text.str().ends_with(lit(v)),
            Cmp::Regex => {
                let pat = if case_sensitive { v } else { format!("(?i){v}") };
                regex::Regex::new(&pat).map_err(|e| anyhow!("Invalid regex: {e}"))?;
                text.str().contains(lit(pat), true)
            }
            _ => unreachable!(),
        };
        // Missing cells never match a text condition.
        return Ok(e.fill_null(lit(false)));
    }
    let (lhs, rhs) = if dtype == DataType::String && !case_sensitive {
        (c.str().to_lowercase(), lit(value.to_lowercase()))
    } else {
        (c, typed_lit(value, &dtype)?)
    };
    Ok(match cmp {
        Cmp::Eq => lhs.eq(rhs),
        Cmp::NotEq => lhs.neq(rhs).fill_null(lit(true)),
        Cmp::Gt => lhs.gt(rhs),
        Cmp::GtEq => lhs.gt_eq(rhs),
        Cmp::Lt => lhs.lt(rhs),
        Cmp::LtEq => lhs.lt_eq(rhs),
        _ => unreachable!(),
    })
}

fn cast_expr(df: &DataFrame, column: &str, to: TargetType, format: &str, decimal_comma: bool) -> Result<Expr> {
    let src = df.column(column)?.dtype().clone();
    let mut c = col(column);
    let format = format.trim();
    if src == DataType::String {
        if decimal_comma && matches!(to, TargetType::Float64 | TargetType::Int64) {
            c = c
                .str()
                .replace_all(lit("."), lit(""), true)
                .str()
                .replace_all(lit(","), lit("."), true);
        }
        c = c.str().strip_chars(lit(NULL));
        match to {
            TargetType::Date => {
                return Ok(c
                    .str()
                    .to_date(StrptimeOptions {
                        format: (!format.is_empty()).then(|| format.into()),
                        strict: false,
                        exact: true,
                        cache: true,
                    })
                    .alias(column));
            }
            TargetType::Datetime => {
                return Ok(c
                    .str()
                    .to_datetime(
                        Some(TimeUnit::Microseconds),
                        None,
                        StrptimeOptions {
                            format: (!format.is_empty()).then(|| format.into()),
                            strict: false,
                            exact: true,
                            cache: true,
                        },
                        lit("raise"),
                    )
                    .alias(column));
            }
            TargetType::Boolean => {
                let l = c.str().to_lowercase();
                return Ok(when(l.clone().is_in(lit(Series::new("t".into(), ["true", "yes", "y", "1", "wahr", "ja"])).implode(false), false))
                    .then(lit(true))
                    .when(l.is_in(lit(Series::new("f".into(), ["false", "no", "n", "0", "falsch", "nein"])).implode(false), false))
                    .then(lit(false))
                    .otherwise(lit(NULL).cast(DataType::Boolean))
                    .alias(column));
            }
            TargetType::Int64 => {
                // "3.0" should become 3, so go through Float64.
                return Ok(c
                    .cast(DataType::Float64)
                    .cast(DataType::Int64)
                    .alias(column));
            }
            _ => {}
        }
    }
    Ok(c.cast(to.dtype()).alias(column))
}

/// How many non-missing values a conversion would turn into missing values.
pub fn cast_failures(df: &DataFrame, column: &str, to: TargetType, format: &str, decimal_comma: bool) -> Result<usize> {
    let before = df.column(column)?.null_count();
    let out = df
        .clone()
        .lazy()
        .select([cast_expr(df, column, to, format, decimal_comma)?])
        .collect()?;
    Ok(out.column(column)?.null_count().saturating_sub(before))
}

fn find_replace(
    df: &DataFrame,
    columns: &[String],
    find: &str,
    replace: &str,
    regex: bool,
    match_case: bool,
    whole_cell: bool,
) -> Result<DataFrame> {
    if find.is_empty() {
        bail!("Nothing to find");
    }
    let targets: Vec<String> = if columns.is_empty() {
        df.columns()
            .iter()
            .filter(|c| c.dtype() == &DataType::String)
            .map(|c| c.name().to_string())
            .collect()
    } else {
        columns.to_vec()
    };
    if targets.is_empty() {
        bail!("There are no text columns to search; change a column's type to Text first");
    }
    let mut pat = if regex { find.to_string() } else { regex::escape(find) };
    if whole_cell {
        pat = format!("^(?:{pat})$");
    }
    if !match_case {
        pat = format!("(?i){pat}");
    }
    regex::Regex::new(&pat).map_err(|e| anyhow!("Invalid regex: {e}"))?;
    // In literal mode `$` in the replacement must stay a literal dollar.
    let repl = if regex { replace.to_string() } else { replace.replace('$', "$$") };
    let exprs: Vec<Expr> = targets
        .iter()
        .map(|t| {
            col(t.as_str())
                .cast(DataType::String)
                .str()
                .replace_all(lit(pat.clone()), lit(repl.clone()), false)
                .alias(t.as_str())
        })
        .collect();
    Ok(df.clone().lazy().with_columns(exprs).collect()?)
}

fn title_case(df: &DataFrame, column: &str) -> Result<DataFrame> {
    let s = df.column(column)?.cast(&DataType::String)?;
    let ca = s.str()?.apply_values(|v| {
        let mut out = String::with_capacity(v.len());
        let mut start = true;
        for ch in v.chars() {
            if ch.is_alphanumeric() {
                if start {
                    out.extend(ch.to_uppercase());
                } else {
                    out.extend(ch.to_lowercase());
                }
                start = false;
            } else {
                out.push(ch);
                start = true;
            }
        }
        std::borrow::Cow::Owned(out)
    });
    let mut out = df.clone();
    let idx = out.try_get_column_index(column)?;
    out.replace_column(idx, ca.with_name(PlSmallStr::from_str(column)).into_column())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DataFrame {
        df!(
            "id" => [1i64, 2, 3, 4],
            "name" => [Some("Anna"), None, Some(" bob "), Some("Anna")],
            "score" => [Some(1.5f64), Some(2.25), None, Some(1.5)],
            "when" => ["2024-01-05", "2024-02-10", "bad", "2024-03-15"],
        )
        .unwrap()
    }

    fn s(df: &DataFrame, c: &str, r: usize) -> String {
        df.column(c).unwrap().get(r).unwrap().str_value().to_string()
    }

    #[test]
    fn set_cells_parses_and_validates() {
        let df = sample();
        let op = Op::SetCells { column: "id".into(), rows: vec![1], values: vec![Some("42".into())] };
        let out = op.apply(&df).unwrap();
        assert_eq!(out.column("id").unwrap().i64().unwrap().get(1), Some(42));
        assert_eq!(out.column("id").unwrap().dtype(), &DataType::Int64);
        let bad = Op::SetCells { column: "id".into(), rows: vec![0], values: vec![Some("x".into())] };
        assert!(bad.apply(&df).unwrap_err().to_string().contains("not a valid"));
        let null = Op::SetCells { column: "name".into(), rows: vec![0], values: vec![None] };
        assert_eq!(null.apply(&df).unwrap().column("name").unwrap().null_count(), 2);
    }

    #[test]
    fn filter_sort_and_dedup() {
        let df = sample();
        let f = Op::Filter { column: "score".into(), cmp: Cmp::Gt, value: "2".into(), case_sensitive: true };
        assert_eq!(f.apply(&df).unwrap().height(), 1);
        let f = Op::Filter { column: "name".into(), cmp: Cmp::Contains, value: "ANN".into(), case_sensitive: false };
        assert_eq!(f.apply(&df).unwrap().height(), 2);
        let srt = Op::Sort { columns: vec![("score".into(), true)] };
        let out = srt.apply(&df).unwrap();
        assert_eq!(out.column("id").unwrap().i64().unwrap().get(0), Some(2));
        assert_eq!(out.column("score").unwrap().get(3).unwrap(), AnyValue::Null);
        let d = Op::DropDuplicates { columns: vec!["name".into(), "score".into()] };
        assert_eq!(d.apply(&df).unwrap().height(), 3);
    }

    #[test]
    fn cast_and_fill() {
        let df = sample();
        assert_eq!(cast_failures(&df, "when", TargetType::Date, "", false).unwrap(), 1);
        let c = Op::Cast { column: "when".into(), to: TargetType::Date, format: String::new(), decimal_comma: false };
        let out = c.apply(&df).unwrap();
        assert_eq!(out.column("when").unwrap().dtype(), &DataType::Date);
        let fill = Op::FillMissing { column: "score".into(), strategy: FillStrategy::Mean, value: String::new() };
        let out = fill.apply(&df).unwrap();
        assert_eq!(out.column("score").unwrap().null_count(), 0);
        let fill = Op::FillMissing { column: "name".into(), strategy: FillStrategy::Value, value: "?".into() };
        assert_eq!(s(&fill.apply(&df).unwrap(), "name", 1), "?");
    }

    #[test]
    fn text_ops() {
        let df = sample();
        let fr = Op::FindReplace {
            columns: vec!["name".into()],
            find: "anna".into(),
            replace: "Ann$".into(),
            regex: false,
            match_case: false,
            whole_cell: true,
        };
        assert_eq!(s(&fr.apply(&df).unwrap(), "name", 0), "Ann$");
        let t = Op::Text { column: "name".into(), transform: TextTransform::Trim };
        assert_eq!(s(&t.apply(&df).unwrap(), "name", 2), "bob");
        let t = Op::Text { column: "name".into(), transform: TextTransform::Title };
        assert_eq!(s(&t.apply(&df).unwrap(), "name", 2), " Bob ");
    }

    #[test]
    fn structure_ops() {
        let df = sample();
        let out = Op::InsertRows { at: 1, count: 2 }.apply(&df).unwrap();
        assert_eq!(out.height(), 6);
        assert_eq!(out.column("id").unwrap().get(1).unwrap(), AnyValue::Null);
        let out = Op::DeleteRows { rows: vec![0, 3] }.apply(&df).unwrap();
        assert_eq!(out.height(), 2);
        let out = Op::AddColumn { name: "double".into(), formula: "score * 2".into(), position: 1 }
            .apply(&df)
            .unwrap();
        assert_eq!(out.get_column_names()[1].as_str(), "double");
        assert_eq!(out.column("double").unwrap().f64().unwrap().get(1), Some(4.5));
        let out = Op::Sql { query: "SELECT name, COUNT(*) AS n FROM data GROUP BY name ORDER BY n DESC".into() }
            .apply(&df)
            .unwrap();
        assert_eq!(out.width(), 2);
        let out = Op::Rename { from: "id".into(), to: "ID".into() }.apply(&df).unwrap();
        assert!(out.column("ID").is_ok());
        let out = Op::MoveColumn { column: "when".into(), to: 0 }.apply(&df).unwrap();
        assert_eq!(out.get_column_names()[0].as_str(), "when");
    }
}
