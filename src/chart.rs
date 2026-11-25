//! The chart panel: a quick chart of the selected column(s).
//!
//! The chart type follows from the column types, so there is nothing to set
//! up: one number column gives a histogram, one text column the counts of
//! its values, one date column the rows per day/week/month; two columns give
//! a scatter plot, a value per category, or a value over time.
//!
//! Everything is aggregated first (Polars or a single pass in Rust) on a
//! background thread, and only the aggregate is drawn, so a chart of a
//! million rows draws a few dozen bars. Scatter plots draw a random sample
//! of at most `MAX_POINTS` points. One computation runs at a time.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use anyhow::Result;
use egui::{Color32, RichText, Ui};
use egui_plot::{Bar, BarChart, BoxElem, BoxPlot, BoxSpread, GridMark, Line, Plot, PlotPoints, Points};
use polars::prelude::*;

use crate::grid::tsv_field;
use crate::ops::Op;

/// Most points a scatter plot draws; larger tables are randomly sampled.
pub const MAX_POINTS: usize = 50_000;
/// Most bars in a category chart; the rest are summed up as "Other".
const MAX_BARS: usize = 25;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Agg {
    Count,
    Sum,
    Mean,
    Median,
    Min,
    Max,
    /// Box plot (quartiles, 5 % and 95 % whiskers); categories only.
    Spread,
}

impl Agg {
    const ALL: [Agg; 7] = [Agg::Mean, Agg::Sum, Agg::Median, Agg::Min, Agg::Max, Agg::Count, Agg::Spread];
    fn label(self) -> &'static str {
        match self {
            Agg::Count => "Count of rows",
            Agg::Sum => "Sum",
            Agg::Mean => "Mean",
            Agg::Median => "Median",
            Agg::Min => "Minimum",
            Agg::Max => "Maximum",
            Agg::Spread => "Spread (box plot)",
        }
    }
    fn short(self) -> &'static str {
        match self {
            Agg::Count => "rows",
            Agg::Sum => "sum",
            Agg::Mean => "mean",
            Agg::Median => "median",
            Agg::Min => "min",
            Agg::Max => "max",
            Agg::Spread => "median",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Period {
    Auto,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl Period {
    const ALL: [Period; 6] = [Period::Auto, Period::Day, Period::Week, Period::Month, Period::Quarter, Period::Year];
    fn label(self) -> &'static str {
        match self {
            Period::Auto => "Auto",
            Period::Day => "Day",
            Period::Week => "Week",
            Period::Month => "Month",
            Period::Quarter => "Quarter",
            Period::Year => "Year",
        }
    }
    /// Picks a period that gives at most about 120 points.
    fn resolve(self, span_days: i64) -> Period {
        match self {
            Period::Auto if span_days <= 120 => Period::Day,
            Period::Auto if span_days <= 7 * 120 => Period::Week,
            Period::Auto if span_days <= 3650 => Period::Month,
            Period::Auto => Period::Year,
            p => p,
        }
    }
    /// Approximate length in days (bar width).
    fn days(self) -> f64 {
        match self {
            Period::Day | Period::Auto => 1.0,
            Period::Week => 7.0,
            Period::Month => 30.44,
            Period::Quarter => 91.3,
            Period::Year => 365.25,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bins {
    Auto,
    N(usize),
}

impl Bins {
    const ALL: [Bins; 5] = [Bins::Auto, Bins::N(10), Bins::N(20), Bins::N(50), Bins::N(100)];
    fn label(self) -> String {
        match self {
            Bins::Auto => "Auto".into(),
            Bins::N(n) => n.to_string(),
        }
    }
}

/// What a chart is computed from; a new request means a new computation.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// Which loaded file (two unedited files both have version 0).
    pub doc: u64,
    pub version: u64,
    pub x: String,
    pub y: Option<String>,
    pub agg: Agg,
    pub period: Period,
    pub bins: Bins,
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Number,
    Time,
    Category,
    Other,
}

pub fn kind(d: &DataType) -> Kind {
    match d {
        d if d.is_primitive_numeric() => Kind::Number,
        DataType::Decimal(_, _) => Kind::Number,
        DataType::Date | DataType::Datetime(_, _) => Kind::Time,
        DataType::String | DataType::Boolean | DataType::Categorical(_, _) | DataType::Enum(_, _) => Kind::Category,
        _ => Kind::Other,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupStat {
    pub label: Option<String>,
    pub rows: u64,
    /// Rows with a value (not missing or NaN).
    pub values: u64,
    pub sum: f64,
    pub mean: f64,
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub q05: f64,
    pub q25: f64,
    pub q75: f64,
    pub q95: f64,
}

impl GroupStat {
    fn value(&self, agg: Agg) -> f64 {
        match agg {
            Agg::Count => self.rows as f64,
            Agg::Sum => self.sum,
            Agg::Mean => self.mean,
            Agg::Median | Agg::Spread => self.median,
            Agg::Min => self.min,
            Agg::Max => self.max,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ChartData {
    /// One number column.
    Histogram { column: String, bins: Vec<(f64, f64, u64)>, one_per_value: bool, skipped: usize },
    /// One text column: rows per value, biggest first. `other` = (values, rows).
    Counts { column: String, values: Vec<(Option<String>, u64)>, other: Option<(usize, u64)>, total: u64 },
    /// A date column, alone (rows per period) or with a number column.
    OverTime {
        column: String,
        y: Option<(String, Agg)>,
        period: Period,
        /// (start of the period in days since 1970-01-01, rows, value)
        points: Vec<(i32, u64, f64)>,
        skipped: usize,
    },
    /// Two number columns.
    Scatter { x: String, y: String, points: Vec<[f64; 2]>, total: usize, r: f64 },
    /// A text and a number column: one value per category, biggest first.
    Groups { category: String, value: String, agg: Agg, groups: Vec<GroupStat>, total_groups: usize },
    /// No chart for this combination; the text says why.
    Message(String),
}

pub fn compute(df: &DataFrame, req: &Request) -> Result<ChartData> {
    let xk = kind(df.column(&req.x)?.dtype());
    let y = req.y.as_deref().filter(|y| *y != req.x);
    let Some(y) = y else {
        return match xk {
            Kind::Number => histogram(df, &req.x, req.bins),
            Kind::Category => counts(df, &req.x),
            Kind::Time => over_time(df, &req.x, None, req.period),
            Kind::Other => Ok(cannot(df, &req.x)),
        };
    };
    let yk = kind(df.column(y)?.dtype());
    let agg = req.agg;
    Ok(match (xk, yk) {
        (Kind::Number, Kind::Number) => scatter(df, &req.x, y)?,
        (Kind::Category, Kind::Number) => groups(df, &req.x, y, agg)?,
        (Kind::Number, Kind::Category) => groups(df, y, &req.x, agg)?,
        (Kind::Time, Kind::Number) => over_time(df, &req.x, Some((y, agg)), req.period)?,
        (Kind::Number, Kind::Time) => over_time(df, y, Some((&req.x, agg)), req.period)?,
        (Kind::Other, _) => cannot(df, &req.x),
        (_, Kind::Other) => cannot(df, y),
        _ => ChartData::Message(format!(
            "{} and {} are both text or dates. Choose a number column for Y to compare a value, \
             or set Y to (none) to count the values of X.",
            req.x, y
        )),
    })
}

fn cannot(df: &DataFrame, column: &str) -> ChartData {
    let t = df.column(column).map(|c| crate::stats::dtype_label(c.dtype())).unwrap_or_default();
    ChartData::Message(format!("{column} is a {t} column, which can't be charted."))
}

fn floats(df: &DataFrame, column: &str) -> Result<Float64Chunked> {
    Ok(df.column(column)?.as_materialized_series().cast(&DataType::Float64)?.f64()?.clone())
}

fn histogram(df: &DataFrame, column: &str, bins: Bins) -> Result<ChartData> {
    let ca = floats(df, column)?;
    // Two passes over the column instead of copying the values.
    let (mut min, mut max, mut count, mut integral) = (f64::INFINITY, f64::NEG_INFINITY, 0usize, true);
    for v in ca.iter().flatten().filter(|v| v.is_finite()) {
        min = min.min(v);
        max = max.max(v);
        count += 1;
        integral &= v.fract() == 0.0;
    }
    let skipped = ca.len() - count;
    if count == 0 {
        return Ok(ChartData::Message(format!("{column} has no values to chart.")));
    }
    let one_per_value = bins == Bins::Auto && integral && max - min < 60.0;
    let n = match bins {
        _ if one_per_value => (max - min) as usize + 1,
        Bins::Auto => 40,
        Bins::N(n) => n,
    };
    let width = if one_per_value { 1.0 } else if max > min { (max - min) / n as f64 } else { 1.0 };
    let start = if one_per_value || max <= min { min - 0.5 } else { min };
    let mut counts = vec![0u64; n];
    for v in ca.iter().flatten().filter(|v| v.is_finite()) {
        let i = (((v - start) / width).floor() as usize).min(n - 1);
        counts[i] += 1;
    }
    let bins = counts
        .into_iter()
        .enumerate()
        .map(|(i, c)| (start + width * i as f64, start + width * (i + 1) as f64, c))
        .collect();
    Ok(ChartData::Histogram { column: column.into(), bins, one_per_value, skipped })
}

fn counts(df: &DataFrame, column: &str) -> Result<ChartData> {
    let out = df
        .clone()
        .lazy()
        .select([col(column).cast(DataType::String).alias("v")])
        .group_by([col("v")])
        .agg([len().cast(DataType::UInt64).alias("n")])
        .sort(["n", "v"], SortMultipleOptions::default().with_order_descending_multi([true, false]).with_nulls_last(true))
        .collect()?;
    let v = out.column("v")?.str()?;
    let n = out.column("n")?.u64()?;
    let mut all: Vec<(Option<String>, u64)> =
        v.iter().zip(n.iter()).map(|(v, n)| (v.map(String::from), n.unwrap_or(0))).collect();
    let total = all.iter().map(|(_, n)| n).sum();
    // Missing values always get their own bar (last), never part of "Other".
    let missing = all.iter().position(|(v, _)| v.is_none()).map(|i| all.remove(i));
    let room = MAX_BARS - usize::from(missing.is_some());
    let keep = if all.len() > room { room - 1 } else { all.len() };
    let rest = all.split_off(keep);
    let other = (!rest.is_empty()).then(|| (rest.len(), rest.iter().map(|(_, n)| n).sum()));
    all.extend(missing);
    Ok(ChartData::Counts { column: column.into(), values: all, other, total })
}

/// Small deterministic random numbers (SplitMix64), so the same table always
/// gives the same sample.
struct SplitMix(u64);

impl SplitMix {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z % n as u64) as usize
    }
}

/// Pearson correlation, updated one pair at a time (Welford), so large
/// values don't overflow and nothing has to be kept in memory.
#[derive(Default)]
struct Correlation {
    n: f64,
    mx: f64,
    my: f64,
    cxy: f64,
    vx: f64,
    vy: f64,
}

impl Correlation {
    fn add(&mut self, x: f64, y: f64) {
        self.n += 1.0;
        let dx = x - self.mx;
        let dy = y - self.my;
        self.mx += dx / self.n;
        self.my += dy / self.n;
        self.cxy += dx * (y - self.my);
        self.vx += dx * (x - self.mx);
        self.vy += dy * (y - self.my);
    }
    fn r(&self) -> f64 {
        if self.n < 2.0 { f64::NAN } else { self.cxy / (self.vx.sqrt() * self.vy.sqrt()) }
    }
}

fn scatter(df: &DataFrame, x: &str, y: &str) -> Result<ChartData> {
    let (xs, ys) = (floats(df, x)?, floats(df, y)?);
    // A uniform random sample of at most MAX_POINTS pairs (reservoir
    // sampling): unlike every k-th row, it can't miss rows that alternate.
    let mut rng = SplitMix(0x5EED_DA7A);
    let mut points: Vec<[f64; 2]> = Vec::with_capacity(MAX_POINTS.min(xs.len()));
    let mut corr = Correlation::default();
    let mut total = 0usize;
    for (a, b) in xs.iter().zip(ys.iter()) {
        let (Some(a), Some(b)) = (a, b) else { continue };
        if !a.is_finite() || !b.is_finite() {
            continue;
        }
        corr.add(a, b);
        total += 1;
        if points.len() < MAX_POINTS {
            points.push([a, b]);
        } else {
            let j = rng.below(total);
            if j < MAX_POINTS {
                points[j] = [a, b];
            }
        }
    }
    Ok(ChartData::Scatter { x: x.into(), y: y.into(), points, total, r: corr.r() })
}

fn groups(df: &DataFrame, category: &str, value: &str, agg: Agg) -> Result<ChartData> {
    let y = || col("y");
    let sort_by = match agg {
        Agg::Count => "rows",
        Agg::Sum => "sum",
        Agg::Mean => "mean",
        Agg::Min => "min",
        Agg::Max => "max",
        Agg::Median | Agg::Spread => "median",
    };
    let mut aggs = vec![
        len().cast(DataType::UInt64).alias("rows"),
        y().count().cast(DataType::UInt64).alias("values"),
        y().sum().alias("sum"),
        y().mean().alias("mean"),
        y().median().alias("median"),
        y().min().alias("min"),
        y().max().alias("max"),
    ];
    // Quantiles are the slow part; only the box plot needs them.
    let spread = agg == Agg::Spread;
    if spread {
        for (p, name) in [(0.05, "q05"), (0.25, "q25"), (0.75, "q75"), (0.95, "q95")] {
            aggs.push(y().quantile(lit(p), QuantileMethod::Linear).alias(name));
        }
    }
    let out = df
        .clone()
        .lazy()
        .select([
            col(category).cast(DataType::String).alias("g"),
            // NaN counts as missing, as everywhere else in the charts.
            col(value).cast(DataType::Float64).fill_nan(lit(NULL)).alias("y"),
        ])
        .group_by([col("g")])
        .agg(aggs)
        .sort([sort_by, "g"], SortMultipleOptions::default().with_order_descending_multi([true, false]).with_nulls_last(true))
        .collect()?;
    let total_groups = out.height();
    let out = out.head(Some(MAX_BARS));
    let f = |name: &str| -> Result<Vec<f64>> {
        match out.column(name) {
            Ok(c) => Ok(c.cast(&DataType::Float64)?.f64()?.iter().map(|v| v.unwrap_or(f64::NAN)).collect()),
            Err(_) => Ok(vec![f64::NAN; out.height()]),
        }
    };
    let labels: Vec<Option<String>> = out.column("g")?.str()?.iter().map(|v| v.map(String::from)).collect();
    let rows = out.column("rows")?.u64()?.iter().map(|v| v.unwrap_or(0)).collect::<Vec<_>>();
    let values = out.column("values")?.u64()?.iter().map(|v| v.unwrap_or(0)).collect::<Vec<_>>();
    let [sum, mean, median, min, max, q05, q25, q75, q95] =
        ["sum", "mean", "median", "min", "max", "q05", "q25", "q75", "q95"].map(f);
    let (sum, mean, median, min, max, q05, q25, q75, q95) = (sum?, mean?, median?, min?, max?, q05?, q25?, q75?, q95?);
    let groups = (0..out.height())
        .map(|i| GroupStat {
            label: labels[i].clone(),
            rows: rows[i],
            values: values[i],
            // A sum over no values is undefined here, not 0.
            sum: if values[i] == 0 { f64::NAN } else { sum[i] },
            mean: mean[i],
            median: median[i],
            min: min[i],
            max: max[i],
            q05: q05[i],
            q25: q25[i],
            q75: q75[i],
            q95: q95[i],
        })
        .collect();
    Ok(ChartData::Groups { category: category.into(), value: value.into(), agg, groups, total_groups })
}

/// Running totals for one period; values are only kept for the median.
struct Acc {
    rows: u64,
    n: u64,
    sum: f64,
    min: f64,
    max: f64,
    vals: Vec<f64>,
}

impl Acc {
    fn new() -> Self {
        Acc { rows: 0, n: 0, sum: 0.0, min: f64::INFINITY, max: f64::NEG_INFINITY, vals: Vec::new() }
    }
    fn value(&mut self, agg: Option<Agg>) -> f64 {
        if agg.is_some_and(|a| a != Agg::Count) && self.n == 0 {
            return f64::NAN;
        }
        match agg {
            None | Some(Agg::Count) => self.rows as f64,
            Some(Agg::Sum) => self.sum,
            Some(Agg::Mean) => self.sum / self.n as f64,
            Some(Agg::Min) => self.min,
            Some(Agg::Max) => self.max,
            Some(Agg::Median | Agg::Spread) => {
                let v = &mut self.vals;
                v.sort_unstable_by(f64::total_cmp);
                let n = v.len();
                if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
            }
        }
    }
}

fn over_time(df: &DataFrame, date: &str, y: Option<(&str, Agg)>, period: Period) -> Result<ChartData> {
    let days = df.column(date)?.as_materialized_series().cast(&DataType::Date)?.cast(&DataType::Int32)?;
    let days = days.i32()?;
    let ys = match y {
        Some((c, _)) => Some(floats(df, c)?),
        None => None,
    };
    let (Some(lo), Some(hi)) = (days.min(), days.max()) else {
        return Ok(ChartData::Message(format!("{date} has no dates to chart.")));
    };
    let period = period.resolve(hi as i64 - lo as i64);
    let agg = y.map(|(_, a)| if a == Agg::Spread { Agg::Median } else { a });
    let keep_values = agg == Some(Agg::Median);
    let mut acc: BTreeMap<i32, Acc> = BTreeMap::new();
    let mut skipped = 0;
    for (i, d) in days.iter().enumerate() {
        let Some(d) = d else {
            skipped += 1;
            continue;
        };
        let e = acc.entry(period_start(d, period)).or_insert_with(Acc::new);
        e.rows += 1;
        if let Some(ys) = &ys {
            match ys.get(i) {
                Some(v) if v.is_finite() => {
                    e.n += 1;
                    e.sum += v;
                    e.min = e.min.min(v);
                    e.max = e.max.max(v);
                    if keep_values {
                        e.vals.push(v);
                    }
                }
                _ => skipped += 1,
            }
        }
    }
    let points = acc.into_iter().map(|(start, mut a)| (start, a.rows, a.value(agg))).collect();
    Ok(ChartData::OverTime {
        column: date.into(),
        y: y.map(|(c, _)| (c.to_string(), agg.unwrap_or(Agg::Count))),
        period,
        points,
        skipped,
    })
}

// ---- dates (days since 1970-01-01, proleptic Gregorian) --------------------

/// (year, month, day) of a day number (Howard Hinnant's algorithm).
pub fn civil_from_days(z: i32) -> (i32, u32, u32) {
    let z = z as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (yoe + era * 400 + i64::from(m <= 2)) as i32;
    (y, m, d)
}

pub fn days_from_civil(y: i32, m: u32, d: u32) -> i32 {
    let y = y as i64 - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as i32
}

fn period_start(d: i32, p: Period) -> i32 {
    match p {
        Period::Day | Period::Auto => d,
        // 1970-01-01 was a Thursday; weeks start on Monday.
        Period::Week => d - (d + 3).rem_euclid(7),
        Period::Month | Period::Quarter | Period::Year => {
            let (y, m, _) = civil_from_days(d);
            let m = match p {
                Period::Month => m,
                Period::Quarter => (m - 1) / 3 * 3 + 1,
                _ => 1,
            };
            days_from_civil(y, m, 1)
        }
    }
}

fn period_label(d: i32, p: Period) -> String {
    let (y, m, day) = civil_from_days(d);
    match p {
        Period::Day | Period::Week | Period::Auto => format!("{y:04}-{m:02}-{day:02}"),
        Period::Month => format!("{y:04}-{m:02}"),
        Period::Quarter => format!("{y:04} Q{}", (m - 1) / 3 + 1),
        Period::Year => format!("{y:04}"),
    }
}

// ---------------------------------------------------------------------------
// Copying
// ---------------------------------------------------------------------------

fn label(v: &Option<String>) -> String {
    match v {
        None => "(missing)".into(),
        Some(s) if s.is_empty() => "(empty)".into(),
        Some(s) => s.clone(),
    }
}

/// The numbers behind a chart as tab-separated text (pastes into Excel).
pub fn to_tsv(data: &ChartData) -> String {
    let mut out = String::new();
    let mut row = |cells: &[String]| {
        out.push_str(&cells.iter().map(|c| tsv_field(c).into_owned()).collect::<Vec<_>>().join("\t"));
        out.push('\n');
    };
    match data {
        ChartData::Histogram { bins, one_per_value, .. } => {
            if *one_per_value {
                row(&["Value".into(), "Rows".into()]);
                for (lo, _, n) in bins {
                    row(&[exact(lo + 0.5), n.to_string()]);
                }
            } else {
                row(&["From".into(), "To".into(), "Rows".into()]);
                for (lo, hi, n) in bins {
                    row(&[exact(*lo), exact(*hi), n.to_string()]);
                }
            }
        }
        ChartData::Counts { column, values, other, .. } => {
            row(&[column.clone(), "Rows".into()]);
            for (v, n) in values {
                row(&[label(v), n.to_string()]);
            }
            if let Some((k, n)) = other {
                row(&[format!("Other ({k} values)"), n.to_string()]);
            }
        }
        ChartData::OverTime { column, y, period, points, .. } => {
            let head = match y {
                Some((c, a)) => format!("{} of {c}", a.short()),
                None => "Rows".into(),
            };
            row(&[format!("{column} ({})", period.label().to_lowercase()), head, "Rows".into()]);
            for (d, rows, v) in points {
                row(&[period_label(*d, *period), exact(*v), rows.to_string()]);
            }
        }
        ChartData::Scatter { x, y, points, .. } => {
            row(&[x.clone(), y.clone()]);
            for [a, b] in points {
                row(&[exact(*a), exact(*b)]);
            }
        }
        ChartData::Groups { category, groups, agg, .. } => {
            let spread = *agg == Agg::Spread;
            let mut head: Vec<String> =
                [category.as_str(), "Rows", "Values", "Sum", "Mean", "Median", "Min", "Max"].map(String::from).to_vec();
            if spread {
                head.extend(["5%", "25%", "75%", "95%"].map(String::from));
            }
            row(&head);
            for g in groups {
                let mut cells = vec![
                    label(&g.label),
                    g.rows.to_string(),
                    g.values.to_string(),
                    exact(g.sum),
                    exact(g.mean),
                    exact(g.median),
                    exact(g.min),
                    exact(g.max),
                ];
                if spread {
                    cells.extend([exact(g.q05), exact(g.q25), exact(g.q75), exact(g.q95)]);
                }
                row(&cells);
            }
        }
        ChartData::Message(m) => row(&[m.clone()]),
    }
    out
}

/// Shortest readable form of a number (no thousands separators).
/// Short form for labels and tooltips: up to 6 decimals, or scientific
/// notation for very small or large numbers.
fn num(v: f64) -> String {
    let a = v.abs();
    if v.is_nan() {
        String::new()
    } else if v.fract() == 0.0 && a < 1e15 {
        format!("{v:.0}")
    } else if a < 1e-4 || a >= 1e15 {
        format!("{v:.4e}")
    } else {
        let s = format!("{v:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Exact form for copying: the shortest text that reads back as the same
/// number (no rounding), which Excel also understands.
fn exact(v: f64) -> String {
    let a = v.abs();
    if v.is_nan() {
        String::new()
    } else if v.fract() == 0.0 && a < 1e15 {
        format!("{v:.0}")
    } else if a != 0.0 && (a < 1e-6 || a >= 1e15) {
        format!("{v:e}")
    } else {
        format!("{v}")
    }
}

// ---------------------------------------------------------------------------
// Panel state and background job
// ---------------------------------------------------------------------------

pub enum Action {
    Apply(Op),
    Copied(String),
    Hide,
}

pub struct ChartState {
    pub x: String,
    pub y: Option<String>,
    pub agg: Agg,
    pub period: Period,
    pub bins: Bins,
    /// Selected column range the X/Y choice was last taken from.
    followed: Option<(usize, usize, u64)>,
    current: Option<(Request, Result<ChartData, String>)>,
    /// The one computation running at a time; a newer request waits for it.
    pending: Option<(Request, Receiver<Result<ChartData, String>>)>,
    /// What the chart should show now; `current` is stale while it differs.
    wanted: Option<Request>,
    /// Bumped for every new chart, so the plot starts with fresh bounds.
    generation: u64,
}

impl Default for ChartState {
    fn default() -> Self {
        ChartState {
            x: String::new(),
            y: None,
            agg: Agg::Mean,
            period: Period::Auto,
            bins: Bins::Auto,
            followed: None,
            current: None,
            pending: None,
            wanted: None,
            generation: 0,
        }
    }
}

impl ChartState {
    /// Points X/Y at the selected columns when the selection changes.
    pub fn follow_selection(&mut self, names: &[String], sel: Option<(usize, usize)>, version: u64) {
        if let Some((c0, c1)) = sel {
            let key = (c0, c1, version);
            if self.followed.map(|(a, b, _)| (a, b)) != Some((c0, c1)) && c0 < names.len() {
                self.x = names[c0].clone();
                // A range of columns charts its first against its last.
                self.y = (c1 > c0).then(|| names.get(c1).cloned()).flatten();
            }
            self.followed = Some(key);
        }
        if !names.contains(&self.x) {
            self.x = names.first().cloned().unwrap_or_default();
        }
        if self.y.as_ref().is_some_and(|y| !names.contains(y) || *y == self.x) {
            self.y = None;
        }
    }

    /// Forgets the chart of the previous file; keeps the chosen settings.
    pub fn reset(&mut self) {
        self.swap(None);
    }

    /// Shows the chart of another sheet and returns the one that was on
    /// screen, to keep with its sheet. `None` is a sheet without a chart
    /// yet; it starts with the settings in use.
    pub fn swap(&mut self, other: Option<ChartState>) -> ChartState {
        let mut next = other.unwrap_or_else(|| ChartState {
            agg: self.agg,
            period: self.period,
            bins: self.bins,
            ..Default::default()
        });
        // A plot number of its own, so it never takes over another chart's zoom.
        next.generation = self.generation.max(next.generation) + 1;
        std::mem::replace(self, next)
    }

    /// Starts a computation if the chart is out of date and collects results.
    /// Only one runs at a time (Polars work can't be cancelled), so moving
    /// quickly across columns of a big file doesn't pile up threads.
    pub fn update(&mut self, df: &DataFrame, doc: u64, version: u64, ctx: &egui::Context) {
        let req = Request {
            doc,
            version,
            x: self.x.clone(),
            y: self.y.clone(),
            agg: self.agg,
            period: self.period,
            bins: self.bins,
        };
        if let Some((r, rx)) = &self.pending {
            let done = match rx.try_recv() {
                Ok(res) => Some(res),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err("The chart could not be computed".into())),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            };
            if let Some(res) = done {
                self.current = Some((r.clone(), res));
                self.pending = None;
                self.generation += 1;
            }
        }
        if req.x.is_empty() || df.column(&req.x).is_err() {
            self.wanted = None;
            return;
        }
        self.wanted = Some(req.clone());
        let fresh = self.current.as_ref().is_some_and(|(r, _)| *r == req);
        if !fresh && self.pending.is_none() {
            let (tx, rx) = channel();
            let df = df.clone();
            let r = req.clone();
            let ctx2 = ctx.clone();
            std::thread::spawn(move || {
                let res = compute(&df, &r).map_err(|e| format!("{e:#}"));
                let _ = tx.send(res);
                ctx2.request_repaint();
            });
            self.pending = Some((req, rx));
        }
        if self.pending.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Whether the chart on screen matches the table and settings.
    fn is_fresh(&self) -> bool {
        matches!((&self.current, &self.wanted), (Some((r, _)), Some(w)) if r == w)
    }

    pub fn ui(&mut self, ui: &mut Ui, df: &DataFrame, preview: bool) -> Option<Action> {
        let mut action = None;
        let fresh = self.is_fresh();
        let names: Vec<String> = df.get_column_names().iter().map(|s| s.to_string()).collect();
        let kind_of = |c: &str| df.column(c).map(|c| kind(c.dtype())).unwrap_or(Kind::Other);
        let xk = kind_of(&self.x);
        let yk = self.y.as_deref().map(kind_of);

        // ---- controls
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Chart").strong());
            ui.label("X");
            egui::ComboBox::from_id_salt("chart-x").selected_text(self.x.as_str()).width(140.0).show_ui(ui, |ui| {
                for n in &names {
                    ui.selectable_value(&mut self.x, n.clone(), n);
                }
            });
            ui.label("Y");
            egui::ComboBox::from_id_salt("chart-y")
                .selected_text(self.y.as_deref().unwrap_or("(none)"))
                .width(140.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.y, None, "(none)");
                    for n in names.iter().filter(|n| **n != self.x) {
                        ui.selectable_value(&mut self.y, Some(n.clone()), n);
                    }
                });
            let has_number_y = matches!((xk, yk), (_, Some(Kind::Number)) | (Kind::Number, Some(_)));
            let has_time = xk == Kind::Time || yk == Some(Kind::Time);
            let has_category = xk == Kind::Category || yk == Some(Kind::Category);
            if yk.is_some() && has_number_y && (has_time || has_category) {
                egui::ComboBox::from_id_salt("chart-agg").selected_text(self.agg.label()).show_ui(ui, |ui| {
                    for a in Agg::ALL {
                        if a == Agg::Spread && !has_category {
                            continue;
                        }
                        ui.selectable_value(&mut self.agg, a, a.label());
                    }
                });
            }
            if has_time && !has_category && self.agg == Agg::Spread {
                self.agg = Agg::Median;
            }
            if has_time && (yk.is_none() || has_number_y) {
                let shown = match (&self.current, self.period) {
                    (Some((_, Ok(ChartData::OverTime { period, .. }))), Period::Auto) => {
                        format!("per {} (auto)", period.label().to_lowercase())
                    }
                    (_, Period::Auto) => "per period (auto)".into(),
                    (_, p) => format!("per {}", p.label().to_lowercase()),
                };
                egui::ComboBox::from_id_salt("chart-period").selected_text(shown).show_ui(ui, |ui| {
                    for p in Period::ALL {
                        ui.selectable_value(&mut self.period, p, p.label());
                    }
                });
            }
            if xk == Kind::Number && yk.is_none() {
                egui::ComboBox::from_id_salt("chart-bins").selected_text(format!("{} bins", self.bins.label())).show_ui(ui, |ui| {
                    for b in Bins::ALL {
                        ui.selectable_value(&mut self.bins, b, b.label());
                    }
                });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Hide").on_hover_text("Hide the chart panel (View > Chart)").clicked() {
                    action = Some(Action::Hide);
                }
                let copyable = self.current.as_ref().filter(|(_, d)| fresh && !matches!(d, Ok(ChartData::Message(_))));
                if let Some((_, Ok(data))) = copyable {
                    if ui.small_button("Copy data").on_hover_text("Copy the numbers behind the chart (pastes into Excel)").clicked() {
                        ui.ctx().copy_text(to_tsv(data));
                        action = Some(Action::Copied("Copied the chart data".into()));
                    }
                }
                if self.pending.is_some() {
                    ui.spinner();
                }
            });
        });

        // ---- chart
        match &self.current {
            None => {
                ui.centered_and_justified(|ui| ui.label(RichText::new("Select a column to chart it.").weak()));
            }
            Some((_, Err(e))) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            Some((_, Ok(data))) => {
                if preview {
                    note(ui, "The chart shows the table without the preview.".into());
                }
                // A stale chart is shown faded and can't be clicked, so a
                // click never filters by numbers from an older table.
                if !fresh {
                    ui.multiply_opacity(0.45);
                }
                let a = draw(ui, data, self.generation);
                if fresh && a.is_some() {
                    action = a;
                }
            }
        }
        action
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn fmt_count(n: u64) -> String {
    crate::app::fmt_int(n as usize)
}

fn note(ui: &mut Ui, text: String) {
    ui.label(RichText::new(text).weak().size(11.5));
}

/// Labels for horizontal bars at y = 0, 1, … (top bar = first label).
fn category_axis(labels: Vec<String>) -> impl Fn(GridMark, &std::ops::RangeInclusive<f64>) -> String {
    move |mark, _| {
        let n = labels.len();
        let k = mark.value.round();
        if (mark.value - k).abs() > 1e-6 || k < 0.0 || k as usize >= n {
            return String::new();
        }
        let s = &labels[n - 1 - k as usize];
        if s.chars().count() > 22 { format!("{}…", s.chars().take(21).collect::<String>()) } else { s.clone() }
    }
}

/// Grid marks on every whole number, for category axes.
fn integer_marks(input: egui_plot::GridInput) -> Vec<GridMark> {
    let (lo, hi) = input.bounds;
    let (lo, hi) = (lo.ceil().max(0.0) as i64, hi.floor() as i64);
    (lo..=hi.min(lo + 200)).map(|v| GridMark { value: v as f64, step_size: 1.0 }).collect()
}

fn draw(ui: &mut Ui, data: &ChartData, generation: u64) -> Option<Action> {
    let accent = ui.visuals().hyperlink_color;
    let fill = accent.gamma_multiply(0.75);
    let id = ("chart-plot", generation);
    match data {
        ChartData::Message(m) => {
            ui.centered_and_justified(|ui| ui.label(RichText::new(m).weak()));
            None
        }
        ChartData::Histogram { column, bins, one_per_value, skipped } => {
            let total: u64 = bins.iter().map(|b| b.2).sum();
            let mut t = format!("{} values", fmt_count(total));
            if *skipped > 0 {
                t.push_str(&format!(" · {} missing not shown", fmt_count(*skipped as u64)));
            }
            note(ui, t);
            let bars = bins
                .iter()
                .map(|(lo, hi, n)| {
                    let name = if *one_per_value { num(lo + 0.5) } else { format!("{} – {}", num(*lo), num(*hi)) };
                    ((lo + hi) / 2.0, (hi - lo) * 0.95, *n as f64, format!("{name}\n{} rows", fmt_count(*n)))
                })
                .collect();
            column_bars(ui, Plot::new(id).x_axis_label(column.as_str()).y_axis_label("Rows"), bars, fill, accent);
            None
        }
        ChartData::Counts { column, values, other, total } => {
            let mut t = format!("{} rows · {} values", fmt_count(*total), fmt_count((values.len() + other.map_or(0, |o| o.0)) as u64));
            if other.is_some() {
                t.push_str(&format!(" (top {} shown, the rest as Other)", values.len()));
            }
            t.push_str(" · click a bar to keep only those rows");
            note(ui, t);
            let mut items: Vec<(String, u64, Option<Option<String>>)> =
                values.iter().map(|(v, n)| (label(v), *n, Some(v.clone()))).collect();
            if let Some((k, n)) = other {
                items.push((format!("Other ({} values)", fmt_count(*k as u64)), *n, None));
            }
            category_bars(ui, id, column, "Rows", &items, *total, accent)
        }
        ChartData::Groups { category, value, agg, groups, total_groups } => {
            let mut t = format!("{} of {value} per {category}", agg.short());
            if *total_groups > groups.len() {
                t.push_str(&format!(" · top {} of {} groups", groups.len(), fmt_count(*total_groups as u64)));
            }
            if *agg == Agg::Spread {
                t.push_str(" · box: 25 % to 75 %, line: median, whiskers: 5 % and 95 %");
            }
            t.push_str(" · click a bar to keep only those rows");
            note(ui, t);
            if *agg == Agg::Spread {
                let n = groups.len();
                let boxes: Vec<BoxElem> = groups
                    .iter()
                    .enumerate()
                    .map(|(i, g)| {
                        BoxElem::new((n - 1 - i) as f64, BoxSpread::new(g.q05, g.q25, g.median, g.q75, g.q95))
                            .fill(fill.gamma_multiply(0.5))
                            .stroke(egui::Stroke::new(1.5, accent))
                            .box_width(0.6)
                            .whisker_width(0.3)
                    })
                    .collect();
                let plot = BoxPlot::new(value.as_str(), boxes).horizontal().color(accent).allow_hover(false);
                return category_plot(
                    ui,
                    id,
                    value,
                    groups.iter().map(|g| label(&g.label)).collect(),
                    true,
                    |p| p.box_plot(plot),
                    |i| (groups[i].q05, groups[i].q95),
                    |i| {
                        let g = &groups[i];
                        format!(
                            "{}\n{} rows\n95%: {}\n75%: {}\nmedian: {}\n25%: {}\n5%: {}",
                            label(&g.label),
                            fmt_count(g.rows),
                            num(g.q95),
                            num(g.q75),
                            num(g.median),
                            num(g.q25),
                            num(g.q05)
                        )
                    },
                    |i| keep(category, &groups[i].label),
                );
            }
            let items: Vec<(String, u64, Option<Option<String>>)> = groups
                .iter()
                .map(|g| (label(&g.label), 0, Some(g.label.clone())))
                .collect();
            let values: Vec<f64> = groups.iter().map(|g| g.value(*agg)).collect();
            let rows: Vec<u64> = groups.iter().map(|g| g.rows).collect();
            value_bars(ui, id, category, &format!("{} of {value}", agg.short()), &items, &values, &rows, accent)
        }
        ChartData::OverTime { column, y, period, points, skipped } => {
            let p = *period;
            let mut t = match y {
                Some((c, a)) => format!("{} of {c} per {}", a.short(), p.label().to_lowercase()),
                None => format!("Rows per {}", p.label().to_lowercase()),
            };
            if *skipped > 0 {
                t.push_str(&format!(" · {} rows with missing values not shown", fmt_count(*skipped as u64)));
            }
            note(ui, t);
            let w = p.days();
            let x_fmt = move |m: GridMark, _: &std::ops::RangeInclusive<f64>| period_label(m.value.floor() as i32, p);
            let plot = Plot::new(id)
                .x_axis_formatter(x_fmt)
                .x_axis_label(column.as_str())
                .allow_scroll(false)
                .label_formatter(move |hp| match hp {
                    egui_plot::HoverPosition::NearDataPoint { position: v, .. } => {
                        Some(format!("{}\n{}", period_label((v.x - w / 2.0).round() as i32, p), num(v.y)))
                    }
                    egui_plot::HoverPosition::Elsewhere { .. } => None,
                });
            match y {
                None => {
                    let bars = points
                        .iter()
                        .map(|(d, n, _)| {
                            let tip = format!("{}\n{} rows", period_label(*d, p), fmt_count(*n));
                            (*d as f64 + w / 2.0, w * 0.9, *n as f64, tip)
                        })
                        .collect();
                    column_bars(ui, plot.y_axis_label("Rows"), bars, fill, accent);
                }
                Some((c, a)) => {
                    let pts: Vec<[f64; 2]> =
                        points.iter().filter(|q| q.2.is_finite()).map(|(d, _, v)| [*d as f64 + w / 2.0, *v]).collect();
                    let line = Line::new(c.as_str(), PlotPoints::new(pts.clone())).color(accent).width(1.5);
                    let dots = Points::new(c.as_str(), PlotPoints::new(pts)).color(accent).radius(2.5);
                    plot.y_axis_label(format!("{} of {c}", a.short())).show(ui, |pu| {
                        pu.line(line);
                        pu.points(dots);
                    });
                }
            }
            None
        }
        ChartData::Scatter { x, y, points, total, r } => {
            let mut t = format!("{} points", fmt_count(*total as u64));
            if points.len() < *total {
                t = format!(
                    "{} of {} points shown (a random sample; the correlation uses all of them)",
                    fmt_count(points.len() as u64),
                    fmt_count(*total as u64)
                );
            }
            if r.is_finite() {
                t.push_str(&format!(" · correlation r = {r:.3}"));
            }
            note(ui, t);
            let radius = if points.len() > 5_000 { 1.2 } else { 2.0 };
            let dots = Points::new(format!("{x} / {y}"), PlotPoints::new(points.clone()))
                .color(accent.gamma_multiply(if points.len() > 5_000 { 0.5 } else { 0.85 }))
                .radius(radius)
                .filled(true);
            Plot::new(id).x_axis_label(x.as_str()).y_axis_label(y.as_str()).allow_scroll(false).show(ui, |p| p.points(dots));
            None
        }
    }
}

/// Vertical bars given as (center, width, height, tooltip), with the tooltip
/// shown at the pointer (see `category_plot` for why not egui_plot's own).
fn column_bars(ui: &mut Ui, plot: Plot<'_>, bars: Vec<(f64, f64, f64, String)>, fill: Color32, accent: Color32) {
    let chart = BarChart::new(
        "bars",
        bars.iter().map(|(c, w, h, _)| Bar::new(*c, *h).width(*w).fill(fill)).collect(),
    )
    .color(accent)
    .allow_hover(false);
    let resp = plot.show_x(false).show_y(false).allow_scroll(false).show(ui, |p| p.bar_chart(chart));
    let Some(pos) = resp.response.hover_pos() else { return };
    let v = resp.transform.value_from_position(pos);
    let slack = resp.transform.bounds().height().abs() * 0.02;
    let hit = bars.iter().find(|(c, w, h, _)| (v.x - c).abs() <= w / 2.0 && v.y >= -slack && v.y <= h + slack);
    if let Some((_, _, _, tip)) = hit {
        resp.response.clone().on_hover_text_at_pointer(tip);
    }
}

/// The bar (0 = top) under `pos` on a horizontal category chart. `span`
/// gives the x range each bar covers; points beside a bar don't count.
fn bar_at<R>(
    resp: &egui_plot::PlotResponse<R>,
    pos: egui::Pos2,
    n: usize,
    span: &impl Fn(usize) -> (f64, f64),
) -> Option<usize> {
    if !resp.response.rect.contains(pos) {
        return None;
    }
    let v = resp.transform.value_from_position(pos);
    let k = v.y.round();
    if (v.y - k).abs() > 0.4 || k < 0.0 || k as usize >= n {
        return None;
    }
    let i = n - 1 - k as usize;
    let (a, b) = span(i);
    // A little slack, so very short bars can still be hit.
    let slack = (resp.transform.bounds().width() * 0.01).abs();
    (v.x >= a.min(b) - slack && v.x <= a.max(b) + slack).then_some(i)
}

fn keep(column: &str, value: &Option<String>) -> Option<Action> {
    Some(Action::Apply(Op::FilterValues { column: column.into(), values: vec![value.clone()], exclude: false }))
}

/// A horizontal chart with one row per category (top row = index 0). Shows
/// `tip(i)` as a tooltip on the hovered bar and returns `click(i)` when a
/// bar is clicked. The plot's own hover labels are off: egui_plot draws them
/// at the bar end, where they get cut off at the right edge. `zoomable` is
/// set for box plots, which can be zoomed and don't start at zero.
#[allow(clippy::too_many_arguments)]
fn category_plot(
    ui: &mut Ui,
    id: (&str, u64),
    axis: &str,
    labels: Vec<String>,
    zoomable: bool,
    add: impl FnOnce(&mut egui_plot::PlotUi<'_>),
    span: impl Fn(usize) -> (f64, f64),
    tip: impl Fn(usize) -> String,
    click: impl Fn(usize) -> Option<Action>,
) -> Option<Action> {
    let n = labels.len();
    let mut plot = Plot::new(id)
        .y_axis_formatter(category_axis(labels))
        .y_grid_spacer(integer_marks)
        .y_axis_min_width(130.0)
        .x_axis_label(axis)
        .show_x(false)
        .show_y(false)
        .allow_drag(zoomable)
        .allow_zoom(zoomable)
        .allow_scroll(false)
        .allow_boxed_zoom(zoomable);
    if !zoomable {
        // Bars start at zero; a box plot of values around 1,000 must not.
        plot = plot.include_x(0.0);
    }
    let resp = plot.show(ui, add);
    if let Some(i) = resp.response.hover_pos().and_then(|p| bar_at(&resp, p, n, &span)) {
        let mut text = tip(i);
        if click(i).is_some() {
            text.push_str("\nClick to keep only these rows");
        }
        resp.response.clone().on_hover_text_at_pointer(text);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.response.clicked() {
        let i = resp.response.interact_pointer_pos().and_then(|p| bar_at(&resp, p, n, &span))?;
        return click(i);
    }
    None
}

/// Horizontal bars of row counts; the last element of each item is the value
/// to filter on (`None` = not clickable, e.g. "Other").
fn category_bars(
    ui: &mut Ui,
    id: (&str, u64),
    column: &str,
    axis: &str,
    items: &[(String, u64, Option<Option<String>>)],
    total: u64,
    accent: Color32,
) -> Option<Action> {
    let n = items.len();
    let bars: Vec<Bar> = items
        .iter()
        .enumerate()
        .map(|(i, (_, c, v))| {
            let fill = if v.is_some() { accent.gamma_multiply(0.75) } else { accent.gamma_multiply(0.35) };
            Bar::new((n - 1 - i) as f64, *c as f64).width(0.75).fill(fill)
        })
        .collect();
    let chart = BarChart::new(column, bars).horizontal().color(accent).allow_hover(false);
    category_plot(
        ui,
        id,
        axis,
        items.iter().map(|i| i.0.clone()).collect(),
        false,
        |p| p.bar_chart(chart),
        |i| (0.0, items[i].1 as f64),
        |i| {
            let pct = items[i].1 as f64 * 100.0 / total.max(1) as f64;
            format!("{}\n{} rows · {pct:.1}%", items[i].0, fmt_count(items[i].1))
        },
        |i| items[i].2.as_ref().and_then(|v| keep(column, v)),
    )
}

#[allow(clippy::too_many_arguments)]
fn value_bars(
    ui: &mut Ui,
    id: (&str, u64),
    column: &str,
    axis: &str,
    items: &[(String, u64, Option<Option<String>>)],
    values: &[f64],
    rows: &[u64],
    accent: Color32,
) -> Option<Action> {
    let n = items.len();
    let v = |i: usize| if values[i].is_finite() { values[i] } else { 0.0 };
    let bars: Vec<Bar> =
        (0..n).map(|i| Bar::new((n - 1 - i) as f64, v(i)).width(0.75).fill(accent.gamma_multiply(0.75))).collect();
    let chart = BarChart::new(axis, bars).horizontal().color(accent).allow_hover(false);
    category_plot(
        ui,
        id,
        axis,
        items.iter().map(|i| i.0.clone()).collect(),
        false,
        |p| p.bar_chart(chart),
        |i| (0.0, v(i)),
        |i| {
            let v = if values[i].is_finite() { num(values[i]) } else { "no values".into() };
            format!("{}\n{axis}: {v}\n{} rows", items[i].0, fmt_count(rows[i]))
        },
        |i| items[i].2.as_ref().and_then(|v| keep(column, v)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DataFrame {
        df!(
            "region" => [Some("N"), Some("S"), Some("N"), None, Some("N")],
            "qty" => [Some(1i64), Some(2), Some(3), Some(4), None],
            "price" => [1.0f64, 2.0, 3.0, 4.0, 5.0],
            "day" => ["2024-01-01", "2024-01-02", "2024-02-10", "2024-02-11", "2024-03-31"],
        )
        .unwrap()
        .lazy()
        .with_column(col("day").str().to_date(StrptimeOptions::default()))
        .collect()
        .unwrap()
    }

    fn req(x: &str, y: Option<&str>, agg: Agg) -> Request {
        Request { doc: 1, version: 0, x: x.into(), y: y.map(String::from), agg, period: Period::Month, bins: Bins::Auto }
    }

    #[test]
    fn dates_round_trip() {
        for d in [-800_000, -1, 0, 59, 19_723, 20_000, 2_000_000] {
            let (y, m, day) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, day), d);
        }
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(period_label(period_start(19_723 + 40, Period::Month), Period::Month), "2024-02");
        // 2024-01-03 (a Wednesday) is in the week starting Monday 2024-01-01.
        assert_eq!(period_start(19_725, Period::Week), 19_723);
        assert_eq!(period_label(19_723 + 100, Period::Quarter), "2024 Q2");
    }

    #[test]
    fn picks_the_chart_from_the_column_types() {
        let df = sample();
        match compute(&df, &req("region", None, Agg::Mean)).unwrap() {
            ChartData::Counts { values, total, other, .. } => {
                assert_eq!(values[0], (Some("N".into()), 3));
                assert_eq!(values.last().unwrap(), &(None, 1));
                assert_eq!((total, other), (5, None));
            }
            d => panic!("{d:?}"),
        }
        match compute(&df, &req("qty", None, Agg::Mean)).unwrap() {
            ChartData::Histogram { bins, one_per_value, skipped, .. } => {
                assert!(one_per_value);
                assert_eq!(bins.iter().map(|b| b.2).collect::<Vec<_>>(), vec![1, 1, 1, 1]);
                assert_eq!(skipped, 1);
            }
            d => panic!("{d:?}"),
        }
        match compute(&df, &req("price", Some("region"), Agg::Sum)).unwrap() {
            ChartData::Groups { category, groups, total_groups, .. } => {
                assert_eq!((category.as_str(), total_groups), ("region", 3));
                assert_eq!((groups[0].label.as_deref(), groups[0].sum), (Some("N"), 9.0));
            }
            d => panic!("{d:?}"),
        }
        match compute(&df, &req("day", None, Agg::Mean)).unwrap() {
            ChartData::OverTime { points, .. } => {
                assert_eq!(points.iter().map(|p| p.1).collect::<Vec<_>>(), vec![2, 2, 1]);
            }
            d => panic!("{d:?}"),
        }
        match compute(&df, &req("day", Some("price"), Agg::Mean)).unwrap() {
            ChartData::OverTime { points, .. } => assert_eq!(points[1].2, 3.5),
            d => panic!("{d:?}"),
        }
        match compute(&df, &req("qty", Some("price"), Agg::Mean)).unwrap() {
            ChartData::Scatter { points, total, r, .. } => {
                assert_eq!((points.len(), total), (4, 4));
                assert!((r - 1.0).abs() < 1e-9);
            }
            d => panic!("{d:?}"),
        }
        assert!(matches!(compute(&df, &req("region", Some("day"), Agg::Mean)).unwrap(), ChartData::Message(_)));
        let tsv = to_tsv(&compute(&df, &req("region", None, Agg::Mean)).unwrap());
        assert_eq!(tsv, "region\tRows\nN\t3\nS\t1\n(missing)\t1\n");
    }

    #[test]
    fn many_values_become_other_and_scatter_is_thinned() {
        let n = 120_000;
        let df = df!(
            "id" => (0..n).map(|i| format!("v{}", i % 40)).collect::<Vec<_>>(),
            "a" => (0..n).map(|i| i as f64).collect::<Vec<_>>(),
        )
        .unwrap();
        match compute(&df, &req("id", None, Agg::Mean)).unwrap() {
            ChartData::Counts { values, other, total, .. } => {
                assert_eq!(values.len(), MAX_BARS - 1);
                assert_eq!(other, Some((40 - (MAX_BARS - 1), 3_000 * (40 - (MAX_BARS - 1)) as u64)));
                assert_eq!(total, n as u64);
            }
            d => panic!("{d:?}"),
        }
        match compute(&df, &req("a", Some("a"), Agg::Mean)).unwrap() {
            ChartData::Histogram { .. } => {}
            d => panic!("same column twice is one column: {d:?}"),
        }
        let df = df.lazy().with_column((col("a") * lit(2.0)).alias("b")).collect().unwrap();
        match compute(&df, &req("a", Some("b"), Agg::Mean)).unwrap() {
            ChartData::Scatter { points, total, .. } => {
                assert_eq!(total, n);
                assert!(points.len() <= MAX_POINTS && points.len() > MAX_POINTS / 2);
            }
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn review_fixes() {
        // Missing values keep their own bar even when there are many values.
        let n = 2_000;
        let ids: Vec<Option<String>> = (0..n).map(|i| if i % 500 == 0 { None } else { Some(format!("v{}", i % 60)) }).collect();
        let df = df!("id" => ids).unwrap();
        match compute(&df, &req("id", None, Agg::Mean)).unwrap() {
            ChartData::Counts { values, other, total, .. } => {
                assert_eq!(values.len(), MAX_BARS - 1);
                assert_eq!(values.last().unwrap(), &(None, 4));
                let shown: u64 = values.iter().map(|v| v.1).sum();
                assert_eq!(shown + other.unwrap().1, total);
            }
            d => panic!("{d:?}"),
        }
        // Copying keeps every digit; labels stay short.
        assert_eq!(exact(0.123456789), "0.123456789");
        assert_eq!(exact(1e-7).parse::<f64>().unwrap(), 1e-7);
        assert_eq!(exact(2.0), "2");
        assert_eq!(num(1e-7), "1.0000e-7");
        assert_eq!(num(0.123456789), "0.123457");
        // Huge values don't overflow the correlation.
        let mut c = Correlation::default();
        c.add(1e100, 1e100);
        c.add(2e100, 2e100);
        assert!((c.r() - 1.0).abs() < 1e-12);
        // Interleaved rows: a random sample sees both, every 2nd row would not.
        let rows = 2 * MAX_POINTS + 10;
        let df = df!(
            "x" => (0..rows).map(|i| i as f64).collect::<Vec<_>>(),
            "y" => (0..rows).map(|i| (i % 2) as f64).collect::<Vec<_>>(),
        )
        .unwrap();
        match compute(&df, &req("x", Some("y"), Agg::Mean)).unwrap() {
            ChartData::Scatter { points, total, .. } => {
                assert_eq!((points.len(), total), (MAX_POINTS, rows));
                let ones = points.iter().filter(|p| p[1] == 1.0).count();
                assert!(ones > MAX_POINTS / 3 && ones < 2 * MAX_POINTS / 3, "{ones}");
            }
            d => panic!("{d:?}"),
        }
        // A group without values has no mean or sum (not 0); NaN is missing.
        let df = df!("g" => ["a", "a", "b"], "v" => [Some(1.0f64), Some(f64::NAN), None]).unwrap();
        match compute(&df, &req("v", Some("g"), Agg::Sum)).unwrap() {
            ChartData::Groups { groups, .. } => {
                let a = groups.iter().find(|g| g.label.as_deref() == Some("a")).unwrap();
                let b = groups.iter().find(|g| g.label.as_deref() == Some("b")).unwrap();
                assert_eq!((a.rows, a.values, a.sum, a.mean), (2, 1, 1.0, 1.0));
                assert_eq!(b.values, 0);
                assert!(b.sum.is_nan() && b.mean.is_nan());
                assert!(a.q25.is_nan(), "quantiles only for the box plot");
            }
            d => panic!("{d:?}"),
        }
        let tsv = to_tsv(&compute(&df, &req("v", Some("g"), Agg::Spread)).unwrap());
        assert!(tsv.starts_with("g\tRows\tValues\tSum\tMean\tMedian\tMin\tMax\t5%\t25%\t75%\t95%\n"), "{tsv}");
    }
}
