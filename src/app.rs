//! The DataVivid window: menus, side panels, dialogs and background jobs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use egui::{Color32, Id, Key, KeyboardShortcut, Modifiers, RichText, Ui};
use polars::prelude::*;

use crate::formats::{self, Format};
use crate::grid::{self, ColumnCommand, GridAction, GridState};
use crate::history::History;
use crate::io::{self, CsvResolved, LoadRequest, Loaded, SaveOptions, TextEncoding};
use crate::ops::{Cmp, FillStrategy, Op, TargetType, TextTransform};
use crate::stats::{self, ColumnProfile};
use crate::winassoc;

const SETTINGS_KEY: &str = "datavivid-settings";
const MAX_RECENT: usize = 12;

pub fn run(file: Option<PathBuf>) -> Result<()> {
    let icon = egui::IconData { rgba: crate::icon_art::icon_rgba(64), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DataVivid")
            // Linux: the app_id must match datavivid.desktop for the icon and
            // name. Elsewhere it also names the settings folder, so keep it.
            .with_app_id(if cfg!(target_os = "linux") { winassoc::LINUX_APP_ID } else { "DataVivid" })
            .with_inner_size([1360.0, 840.0])
            .with_min_inner_size([720.0, 420.0])
            .with_icon(Arc::new(icon))
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "DataVivid",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, file)))),
    )
    .map_err(|e| anyhow!("{e}"))
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(default)]
struct Settings {
    recent: Vec<PathBuf>,
    theme: ThemeChoice,
    show_ops: bool,
    show_summary: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { recent: Vec::new(), theme: ThemeChoice::System, show_ops: true, show_summary: true }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq)]
enum ThemeChoice {
    System,
    Light,
    Dark,
}

struct Doc {
    path: PathBuf,
    format: Format,
    request: LoadRequest,
    tables: Vec<String>,
    table: Option<String>,
    csv: Option<CsvResolved>,
    history: History,
    loaded_shape: (usize, usize),
    load_time: Duration,
}

impl Doc {
    fn df(&self) -> &DataFrame {
        self.history.current()
    }
    fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

struct Preview {
    op: Op,
    df: DataFrame,
    base_version: u64,
}

/// What to do after the user confirmed discarding unsaved changes.
#[derive(Clone)]
enum After {
    Open(LoadRequest),
    Close,
    Quit,
}

enum Dialog {
    Import { req: LoadRequest, preview: String, sep_text: String },
    Discard(After),
    ExcelSave,
    About,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OpKind {
    Sort,
    Filter,
    Rename,
    DropColumns,
    AddColumn,
    Cast,
    FillMissing,
    DropMissing,
    DropDuplicates,
    FindReplace,
    Text,
    Round,
    Sql,
}

impl OpKind {
    const ALL: [OpKind; 13] = [
        OpKind::Sort,
        OpKind::Filter,
        OpKind::FindReplace,
        OpKind::Cast,
        OpKind::FillMissing,
        OpKind::DropMissing,
        OpKind::DropDuplicates,
        OpKind::Rename,
        OpKind::AddColumn,
        OpKind::DropColumns,
        OpKind::Text,
        OpKind::Round,
        OpKind::Sql,
    ];
    fn label(self) -> &'static str {
        match self {
            OpKind::Sort => "Sort",
            OpKind::Filter => "Filter rows",
            OpKind::Rename => "Rename column",
            OpKind::DropColumns => "Delete columns",
            OpKind::AddColumn => "Add column (formula)",
            OpKind::Cast => "Change column type",
            OpKind::FillMissing => "Fill missing values",
            OpKind::DropMissing => "Drop missing values",
            OpKind::DropDuplicates => "Drop duplicate rows",
            OpKind::FindReplace => "Find and replace",
            OpKind::Text => "Text transform",
            OpKind::Round => "Round numbers",
            OpKind::Sql => "SQL query",
        }
    }
    fn help(self) -> &'static str {
        match self {
            OpKind::Sort => "Reorders the rows by a column. Missing values go last.",
            OpKind::Filter => "Keeps only the rows that match the condition.",
            OpKind::Rename => "Gives a column a new name.",
            OpKind::DropColumns => "Removes the selected columns.",
            OpKind::AddColumn => {
                "Adds a column computed with a SQL expression, e.g. price * qty, \
                 upper(name), CASE WHEN x > 0 THEN 'pos' ELSE 'neg' END. Leave empty for a blank column."
            }
            OpKind::Cast => "Converts values to another type. Values that can't be converted become missing.",
            OpKind::FillMissing => "Replaces missing values in a column.",
            OpKind::DropMissing => "Removes rows with a missing value in the chosen columns (none chosen = any column).",
            OpKind::DropDuplicates => "Keeps the first of rows that are equal in the chosen columns (none chosen = all).",
            OpKind::FindReplace => "Replaces text in text columns (none chosen = all text columns).",
            OpKind::Text => "Changes letter case or trims spaces.",
            OpKind::Round => "Rounds decimal numbers.",
            OpKind::Sql => "Runs a query against the table, which is called data. The result replaces the table.",
        }
    }
}

struct OpForm {
    kind: OpKind,
    column: String,
    columns: Vec<String>,
    descending: bool,
    cmp: Cmp,
    value: String,
    case_sensitive: bool,
    new_name: String,
    formula: String,
    position: usize,
    target: TargetType,
    format: String,
    decimal_comma: bool,
    strategy: FillStrategy,
    find: String,
    replace: String,
    regex: bool,
    match_case: bool,
    whole_cell: bool,
    transform: TextTransform,
    decimals: u32,
    sql: String,
    cast_check: Option<(String, TargetType, String, bool, u64, Result<usize, String>)>,
}

impl Default for OpForm {
    fn default() -> Self {
        OpForm {
            kind: OpKind::Sort,
            column: String::new(),
            columns: Vec::new(),
            descending: false,
            cmp: Cmp::Eq,
            value: String::new(),
            case_sensitive: false,
            new_name: String::new(),
            formula: String::new(),
            position: usize::MAX,
            target: TargetType::String,
            format: String::new(),
            decimal_comma: false,
            strategy: FillStrategy::Value,
            find: String::new(),
            replace: String::new(),
            regex: false,
            match_case: false,
            whole_cell: false,
            transform: TextTransform::Trim,
            decimals: 2,
            sql: "SELECT * FROM data LIMIT 100".into(),
            cast_check: None,
        }
    }
}

enum Job {
    Load { rx: Receiver<Result<Loaded>>, path: PathBuf, started: Instant },
    Save { rx: Receiver<Result<()>>, path: PathBuf, opts: SaveOptions, version: u64 },
}

pub struct App {
    settings: Settings,
    doc: Option<Doc>,
    grid: GridState,
    form: OpForm,
    profiles: HashMap<String, ColumnProfile>,
    profiles_for: Option<u64>,
    profile_rx: Option<Receiver<(u64, ColumnProfile)>>,
    profile_gen: Arc<AtomicU64>,
    job: Option<Job>,
    preview: Option<Preview>,
    dialog: Option<Dialog>,
    status: Option<(String, bool, Instant)>,
    title: String,
    reg_status: winassoc::Status,
    allow_close: bool,
    /// Action to continue with after a save started from the unsaved-changes prompt.
    after_save: Option<After>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, file: Option<PathBuf>) -> Self {
        install_fonts(&cc.egui_ctx);
        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();
        apply_theme(&cc.egui_ctx, settings.theme);
        cc.egui_ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 6.0);
            s.spacing.button_padding = egui::vec2(8.0, 3.0);
        });
        let mut app = App {
            settings,
            doc: None,
            grid: GridState::default(),
            form: OpForm::default(),
            profiles: HashMap::new(),
            profiles_for: None,
            profile_rx: None,
            profile_gen: Arc::new(AtomicU64::new(0)),
            job: None,
            preview: None,
            dialog: None,
            status: None,
            title: String::new(),
            reg_status: winassoc::status(),
            allow_close: false,
            after_save: None,
        };
        if let Some(f) = file {
            app.open_path(f, &cc.egui_ctx);
        }
        app
    }

    // ---- status -----------------------------------------------------------

    fn info(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), false, Instant::now()));
    }
    fn error(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), true, Instant::now()));
    }

    // ---- loading ----------------------------------------------------------

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let mut dlg = rfd::FileDialog::new()
            .set_title("Open data file")
            .add_filter("All supported data files", &formats::open_dialog_extensions());
        for f in Format::ALL {
            dlg = dlg.add_filter(f.label(), f.extensions());
        }
        dlg = dlg.add_filter("All files", &["*"]);
        if let Some(dir) = self.doc.as_ref().and_then(|d| d.path.parent()) {
            dlg = dlg.set_directory(dir);
        }
        if let Some(p) = dlg.pick_file() {
            self.open_path(p, ctx);
        }
    }

    fn open_path(&mut self, path: PathBuf, ctx: &egui::Context) {
        match LoadRequest::new(&path) {
            Ok(req) => self.request_open(req, ctx),
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn request_open(&mut self, req: LoadRequest, ctx: &egui::Context) {
        if self.doc.as_ref().is_some_and(|d| d.history.is_dirty()) {
            self.dialog = Some(Dialog::Discard(After::Open(req)));
        } else {
            self.start_load(req, ctx);
        }
    }

    fn start_load(&mut self, req: LoadRequest, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let path = req.path.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(io::load(req));
            ctx2.request_repaint();
        });
        self.job = Some(Job::Load { rx, path, started: Instant::now() });
        self.preview = None;
    }

    fn finish_load(&mut self, loaded: Loaded, started: Instant) {
        let path = loaded.request.path.clone();
        let mut request = loaded.request.clone();
        request.table = loaded.table.clone();
        let shape = loaded.df.shape();
        let note = loaded.notes.join("; ");
        self.doc = Some(Doc {
            path: path.clone(),
            format: request.format,
            request,
            tables: loaded.tables,
            table: loaded.table,
            csv: loaded.csv,
            history: History::new(loaded.df),
            loaded_shape: shape,
            load_time: started.elapsed(),
        });
        self.grid.reset();
        self.profiles.clear();
        self.profiles_for = None;
        self.form.column.clear();
        self.form.columns.clear();
        self.settings.recent.retain(|p| p != &path);
        self.settings.recent.insert(0, path);
        self.settings.recent.truncate(MAX_RECENT);
        let secs = started.elapsed().as_secs_f32();
        self.info(format!(
            "Loaded {} rows × {} columns in {secs:.2} s{}",
            fmt_int(shape.0),
            shape.1,
            if note.is_empty() { String::new() } else { format!(" — {note}") }
        ));
    }

    fn reload(&mut self, ctx: &egui::Context) {
        if let Some(d) = &self.doc {
            let req = d.request.clone();
            self.request_open(req, ctx);
        }
    }

    // ---- saving -----------------------------------------------------------

    fn save(&mut self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        match doc.format {
            Format::Sqlite | Format::Ods => self.save_as(ctx),
            Format::Excel => {
                let ext = doc.path.extension().map(|e| e.to_string_lossy().to_lowercase());
                if ext.as_deref() == Some("xlsx") {
                    self.dialog = Some(Dialog::ExcelSave);
                } else {
                    self.save_as(ctx);
                }
            }
            _ => {
                let opts = self.save_options_for(doc.format, &doc.path);
                let path = doc.path.clone();
                self.start_save(path, opts, ctx);
            }
        }
    }

    fn save_options_for(&self, format: Format, path: &Path) -> SaveOptions {
        let mut o = SaveOptions::for_format(format);
        if let Some(doc) = &self.doc {
            if format.is_delimited() && doc.format.is_delimited() {
                if let Some(csv) = &doc.csv {
                    // Keep the original dialect when writing the same kind of
                    // file; CSV <-> TSV uses that format's own separator.
                    if format == doc.format {
                        o.separator = csv.separator;
                    }
                    // Files read as Windows-1252/UTF-16 are written as UTF-8 with a
                    // BOM: nothing is lost and Excel still shows umlauts correctly.
                    o.bom = csv.had_bom || !csv.encoding.starts_with("UTF-8");
                    o.decimal_comma = csv.decimal_comma && o.separator != b',';
                }
            }
            o.sheet_name = doc
                .table
                .clone()
                .unwrap_or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        }
        o
    }

    fn save_as(&mut self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        let stem = doc.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or("data".into());
        let default_fmt = if doc.format.can_write() && doc.format != Format::Sqlite { doc.format } else { Format::Csv };
        let mut dlg = rfd::FileDialog::new()
            .set_title("Save as")
            .set_file_name(format!("{stem}.{}", default_fmt.extensions()[0]));
        if let Some(dir) = doc.path.parent() {
            dlg = dlg.set_directory(dir);
        }
        // The first filter is the default choice in the dialog.
        let mut order: Vec<Format> = vec![default_fmt];
        order.extend(Format::ALL.into_iter().filter(|f| f.can_write() && *f != default_fmt));
        for f in order {
            let exts: Vec<&str> = if f == Format::Excel { vec!["xlsx"] } else { f.extensions().to_vec() };
            dlg = dlg.add_filter(f.label(), &exts);
        }
        let Some(mut path) = dlg.save_file() else {
            self.after_save = None;
            return;
        };
        if path.extension().is_none() {
            path.set_extension(default_fmt.extensions()[0]);
        }
        let Some(fmt) = Format::from_path(&path) else {
            self.error("Unknown file extension; use e.g. .csv, .parquet or .xlsx");
            return;
        };
        if !fmt.can_write() {
            self.error(format!("{} files can't be written", fmt.label()));
            return;
        }
        if fmt == Format::Excel && path.extension().is_some_and(|e| !e.eq_ignore_ascii_case("xlsx")) {
            path.set_extension("xlsx");
        }
        let opts = self.save_options_for(fmt, &path);
        self.start_save(path, opts, ctx);
    }

    fn start_save(&mut self, path: PathBuf, opts: SaveOptions, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        if self.preview.is_some() {
            self.after_save = None;
            self.error("Apply or discard the preview before saving");
            return;
        }
        let df = doc.df().clone();
        let version = doc.history.version();
        let (tx, rx) = channel();
        let ctx2 = ctx.clone();
        let p2 = path.clone();
        let o2 = opts.clone();
        std::thread::spawn(move || {
            let _ = tx.send(io::save(&df, &p2, &o2));
            ctx2.request_repaint();
        });
        self.job = Some(Job::Save { rx, path, opts, version });
    }

    fn finish_save(&mut self, path: PathBuf, opts: SaveOptions, version: u64) {
        let format = opts.format;
        if let Some(doc) = &mut self.doc {
            if doc.history.version() == version {
                doc.history.mark_saved();
            }
            if path != doc.path {
                // The document now lives in the new file.
                doc.path = path.clone();
                doc.format = format;
                doc.request.path = path.clone();
                doc.request.format = format;
                if !format.is_delimited() {
                    doc.csv = None;
                }
            }
        }
        self.settings.recent.retain(|p| p != &path);
        self.settings.recent.insert(0, path.clone());
        self.settings.recent.truncate(MAX_RECENT);
        let mut note = String::new();
        if let Some(doc) = &mut self.doc {
            if format.is_delimited() {
                if let Some(c) = &mut doc.csv {
                    if !c.encoding.starts_with("UTF-8") {
                        note = format!(" (as UTF-8 with BOM; the original was {})", c.encoding);
                        c.encoding = "UTF-8";
                    }
                    c.separator = opts.separator;
                    c.had_bom = opts.bom;
                    c.decimal_comma = opts.decimal_comma;
                }
                // The file on disk is now UTF-8 in DataVivid's own dialect, so a
                // reload should detect it afresh rather than reuse old overrides.
                doc.request.csv.encoding = TextEncoding::Auto;
                doc.request.csv.separator = None;
                doc.request.csv.decimal_comma = None;
            }
        }
        self.info(format!("Saved {}{note}", path.display()));
    }

    // ---- operations -------------------------------------------------------

    fn apply(&mut self, op: Op) {
        let Some(doc) = &mut self.doc else { return };
        if self.preview.is_some() {
            self.preview = None;
        }
        let before = doc.df().shape();
        let t = Instant::now();
        match op.apply(doc.df()) {
            Ok(df) => {
                let after = df.shape();
                let desc = op.describe();
                doc.history.push(op, df);
                let mut msg = desc;
                if before.0 != after.0 {
                    msg.push_str(&format!(" — {} → {} rows", fmt_int(before.0), fmt_int(after.0)));
                }
                if t.elapsed() > Duration::from_millis(300) {
                    msg.push_str(&format!(" ({:.1} s)", t.elapsed().as_secs_f32()));
                }
                self.info(msg);
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn undo(&mut self) {
        if let Some(doc) = &mut self.doc {
            self.preview = None;
            if let Some(op) = doc.history.undo() {
                let m = format!("Undid: {}", op.describe());
                self.info(m);
            }
        }
    }

    fn redo(&mut self) {
        if let Some(doc) = &mut self.doc {
            self.preview = None;
            if let Some(op) = doc.history.redo() {
                let m = format!("Redid: {}", op.describe());
                self.info(m);
            }
        }
    }

    fn column_command(&mut self, ci: usize, cmd: ColumnCommand) {
        let Some(doc) = &self.doc else { return };
        let df = doc.df();
        let Some(name) = df.get_column_names().get(ci).map(|s| s.to_string()) else { return };
        self.form.column = name.clone();
        let open = |app: &mut App, kind: OpKind| {
            app.form.kind = kind;
            app.settings.show_ops = true;
        };
        match cmd {
            ColumnCommand::SortAsc => self.apply(Op::Sort { columns: vec![(name, false)] }),
            ColumnCommand::SortDesc => self.apply(Op::Sort { columns: vec![(name, true)] }),
            ColumnCommand::Filter => open(self, OpKind::Filter),
            ColumnCommand::FilterValue(v) => {
                let op = match v {
                    Some(v) => Op::Filter { column: name, cmp: Cmp::Eq, value: v, case_sensitive: true },
                    None => Op::Filter { column: name, cmp: Cmp::IsNull, value: String::new(), case_sensitive: true },
                };
                self.apply(op);
            }
            ColumnCommand::Rename => {
                self.form.new_name = name;
                open(self, OpKind::Rename);
            }
            ColumnCommand::ChangeType => open(self, OpKind::Cast),
            ColumnCommand::FillMissing => open(self, OpKind::FillMissing),
            ColumnCommand::FindReplace => {
                self.form.columns = vec![name];
                open(self, OpKind::FindReplace);
            }
            ColumnCommand::Delete => self.apply(Op::DropColumns { columns: vec![name] }),
            ColumnCommand::InsertLeft | ColumnCommand::InsertRight => {
                let pos = if matches!(cmd, ColumnCommand::InsertLeft) { ci } else { ci + 1 };
                let mut n = 1;
                let names: Vec<String> = df.get_column_names().iter().map(|s| s.to_string()).collect();
                while names.contains(&format!("new_column_{n}")) {
                    n += 1;
                }
                self.apply(Op::AddColumn { name: format!("new_column_{n}"), formula: String::new(), position: pos });
            }
            ColumnCommand::DropDuplicates => self.apply(Op::DropDuplicates { columns: vec![name] }),
            ColumnCommand::DropMissing => self.apply(Op::DropMissing { columns: vec![name] }),
            ColumnCommand::MoveLeft if ci > 0 => {
                self.apply(Op::MoveColumn { column: name, to: ci - 1 });
                self.grid.select_column(ci - 1);
            }
            ColumnCommand::MoveRight if ci + 1 < df.width() => {
                self.apply(Op::MoveColumn { column: name, to: ci + 1 });
                self.grid.select_column(ci + 1);
            }
            ColumnCommand::MoveLeft | ColumnCommand::MoveRight => {}
        }
    }

    fn build_op(&self, df: &DataFrame) -> Result<Op> {
        let f = &self.form;
        let need_col = || -> Result<String> {
            if f.column.is_empty() || df.column(&f.column).is_err() {
                Err(anyhow!("Choose a column"))
            } else {
                Ok(f.column.clone())
            }
        };
        Ok(match f.kind {
            OpKind::Sort => Op::Sort { columns: vec![(need_col()?, f.descending)] },
            OpKind::Filter => Op::Filter {
                column: need_col()?,
                cmp: f.cmp,
                value: f.value.clone(),
                case_sensitive: f.case_sensitive,
            },
            OpKind::Rename => Op::Rename { from: need_col()?, to: f.new_name.trim().to_string() },
            OpKind::DropColumns => {
                if f.columns.is_empty() {
                    return Err(anyhow!("Tick the columns to delete"));
                }
                Op::DropColumns { columns: f.columns.clone() }
            }
            OpKind::AddColumn => Op::AddColumn {
                name: f.new_name.trim().to_string(),
                formula: f.formula.clone(),
                position: f.position.min(df.width()),
            },
            OpKind::Cast => Op::Cast {
                column: need_col()?,
                to: f.target,
                format: f.format.clone(),
                decimal_comma: f.decimal_comma,
            },
            OpKind::FillMissing => Op::FillMissing { column: need_col()?, strategy: f.strategy, value: f.value.clone() },
            OpKind::DropMissing => Op::DropMissing { columns: f.columns.clone() },
            OpKind::DropDuplicates => Op::DropDuplicates { columns: f.columns.clone() },
            OpKind::FindReplace => Op::FindReplace {
                columns: f.columns.clone(),
                find: f.find.clone(),
                replace: f.replace.clone(),
                regex: f.regex,
                match_case: f.match_case,
                whole_cell: f.whole_cell,
            },
            OpKind::Text => Op::Text { column: need_col()?, transform: f.transform },
            OpKind::Round => Op::Round { column: need_col()?, decimals: f.decimals },
            OpKind::Sql => Op::Sql { query: f.sql.clone() },
        })
    }

    // ---- profiling --------------------------------------------------------

    fn update_profiles(&mut self, ctx: &egui::Context) {
        // Collect finished results.
        if let Some(rx) = &self.profile_rx {
            let cur = self.profile_gen.load(Ordering::Relaxed);
            while let Ok((v, p)) = rx.try_recv() {
                if v == cur {
                    self.profiles.insert(p.name.clone(), p);
                }
            }
        }
        let Some(doc) = &self.doc else { return };
        let version = doc.history.version();
        if self.profiles_for == Some(version) {
            return;
        }
        self.profiles_for = Some(version);
        let generation = self.profile_gen.fetch_add(1, Ordering::Relaxed) + 1;
        let df = doc.df().clone();
        // Columns that no longer exist lose their profile; the rest keep the
        // old one until the new one arrives (no flicker).
        let names: std::collections::HashSet<String> =
            df.get_column_names().iter().map(|s| s.to_string()).collect();
        self.profiles.retain(|k, _| names.contains(k));

        // Selected column first.
        let mut order: Vec<String> = df.get_column_names().iter().map(|s| s.to_string()).collect();
        if let Some(c) = self.grid.cursor.and_then(|c| order.get(c.col).cloned()) {
            order.retain(|n| n != &c);
            order.insert(0, c);
        }
        let (tx, rx) = channel();
        self.profile_rx = Some(rx);
        let gen_ref = self.profile_gen.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            for name in order {
                if gen_ref.load(Ordering::Relaxed) != generation {
                    return; // superseded by a newer version
                }
                if let Ok(p) = stats::profile(&df, &name) {
                    if tx.send((generation, p)).is_err() {
                        return;
                    }
                    ctx2.request_repaint();
                }
            }
        });
    }

    // ---- frame ------------------------------------------------------------

    fn poll_jobs(&mut self, ctx: &egui::Context) {
        let Some(job) = self.job.take() else { return };
        match job {
            Job::Load { rx, path, started } => match rx.try_recv() {
                Ok(Ok(loaded)) => self.finish_load(loaded, started),
                Ok(Err(e)) => self.error(format!("Could not open {}: {e:#}", path.display())),
                Err(std::sync::mpsc::TryRecvError::Empty) => self.job = Some(Job::Load { rx, path, started }),
                Err(_) => self.error("Loading failed unexpectedly"),
            },
            Job::Save { rx, path, opts, version } => match rx.try_recv() {
                Ok(Ok(())) => {
                    self.finish_save(path, opts, version);
                    if let Some(a) = self.after_save.take() {
                        self.continue_after(a, ctx);
                    }
                }
                Ok(Err(e)) => {
                    self.after_save = None;
                    self.error(format!("Could not save {}: {e:#}", path.display()))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.job = Some(Job::Save { rx, path, opts, version })
                }
                Err(_) => self.error("Saving failed unexpectedly"),
            },
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match &self.doc {
            Some(d) => format!(
                "{}{} — DataVivid",
                if d.history.is_dirty() { "● " } else { "" },
                d.name()
            ),
            None => "DataVivid".into(),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let sc = |m: Modifiers, k: Key| KeyboardShortcut::new(m, k);
        let typing = ctx.egui_wants_keyboard_input();
        if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::COMMAND, Key::O))) {
            self.open_dialog(ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::S))) {
            self.save_as(ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::COMMAND, Key::S))) {
            self.save(ctx);
        }
        if !typing {
            if ctx.input_mut(|i| {
                i.consume_shortcut(&sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z))
                    || i.consume_shortcut(&sc(Modifiers::COMMAND, Key::Y))
            }) {
                self.redo();
            }
            if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::COMMAND, Key::Z))) {
                self.undo();
            }
            if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::COMMAND, Key::F))) {
                self.form.kind = OpKind::FindReplace;
                self.settings.show_ops = true;
            }
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::NONE, Key::F5))) {
            self.reload(ctx);
        }
        // Drag & drop.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if let Some(p) = dropped.into_iter().next() {
            self.open_path(p, ctx);
        }
    }

    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            if self.doc.as_ref().is_some_and(|d| d.history.is_dirty()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.dialog = Some(Dialog::Discard(After::Quit));
            }
        }
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_jobs(&ctx);
        self.shortcuts(&ctx);
        self.handle_close(&ctx);
        self.update_profiles(&ctx);
        self.update_title(&ctx);

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui, &ctx));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        if self.doc.is_some() {
            if self.settings.show_ops {
                egui::Panel::left("ops")
                    .resizable(true)
                    .default_size(290.0)
                    .size_range(220.0..=520.0)
                    .show(ui, |ui| self.ops_panel(ui));
            }
            if self.settings.show_summary {
                egui::Panel::right("summary")
                    .resizable(true)
                    .default_size(300.0)
                    .size_range(220.0..=560.0)
                    .show(ui, |ui| self.summary_panel(ui));
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(0.0))
            .show(ui, |ui| self.central(ui, &ctx));

        self.dialogs(&ctx);

        if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if let Some((_, _, t)) = &self.status {
            if t.elapsed() < Duration::from_secs(12) {
                ctx.request_repaint_after(Duration::from_secs(1));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UI pieces
// ---------------------------------------------------------------------------

impl App {
    fn menu_bar(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        egui::MenuBar::new().ui(ui, |ui| {
            let has_doc = self.doc.is_some();
            ui.menu_button("File", |ui| {
                if ui.add(egui::Button::new("Open…").shortcut_text("Ctrl+O")).clicked() {
                    ui.close();
                    self.open_dialog(ctx);
                }
                ui.menu_button("Open recent", |ui| {
                    if self.settings.recent.is_empty() {
                        ui.label(RichText::new("No recent files").weak());
                    }
                    for p in self.settings.recent.clone() {
                        if ui.button(p.display().to_string()).clicked() {
                            ui.close();
                            self.open_path(p, ctx);
                        }
                    }
                    if !self.settings.recent.is_empty() {
                        ui.separator();
                        if ui.button("Clear list").clicked() {
                            self.settings.recent.clear();
                            ui.close();
                        }
                    }
                });
                if ui.add_enabled(has_doc, egui::Button::new("Open in new window…")).clicked() {
                    ui.close();
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Data files", &formats::open_dialog_extensions())
                        .pick_file()
                    {
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(exe).arg(p).spawn();
                        }
                    }
                }
                let import_ok = self.doc.as_ref().is_some_and(|d| {
                    d.format.is_delimited() || matches!(d.format, Format::Excel | Format::Ods)
                });
                if ui.add_enabled(import_ok, egui::Button::new("Import options…")).clicked() {
                    ui.close();
                    self.open_import_dialog();
                }
                if ui.add_enabled(has_doc, egui::Button::new("Reload from disk").shortcut_text("F5")).clicked() {
                    ui.close();
                    self.reload(ctx);
                }
                ui.separator();
                if ui.add_enabled(has_doc, egui::Button::new("Save").shortcut_text("Ctrl+S")).clicked() {
                    ui.close();
                    self.save(ctx);
                }
                if ui.add_enabled(has_doc, egui::Button::new("Save as / Export…").shortcut_text("Ctrl+Shift+S")).clicked() {
                    ui.close();
                    self.save_as(ctx);
                }
                ui.separator();
                if ui.add_enabled(has_doc, egui::Button::new("Close file")).clicked() {
                    ui.close();
                    if self.doc.as_ref().is_some_and(|d| d.history.is_dirty()) {
                        self.dialog = Some(Dialog::Discard(After::Close));
                    } else {
                        self.close_doc();
                    }
                }
                if ui.button("Exit").clicked() {
                    ui.close();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });

            ui.menu_button("Edit", |ui| {
                let (u, r) = self.doc.as_ref().map_or((false, false), |d| (d.history.can_undo(), d.history.can_redo()));
                if ui.add_enabled(u, egui::Button::new("Undo").shortcut_text("Ctrl+Z")).clicked() {
                    self.undo();
                    ui.close();
                }
                if ui.add_enabled(r, egui::Button::new("Redo").shortcut_text("Ctrl+Y")).clicked() {
                    self.redo();
                    ui.close();
                }
                ui.separator();
                ui.label(RichText::new("In the grid:").weak());
                ui.label(RichText::new("Enter / F2 / typing — edit cell").weak());
                ui.label(RichText::new("Ctrl+C / Ctrl+V — copy / paste (Excel compatible)").weak());
                ui.label(RichText::new("Delete — clear selected cells").weak());
                ui.label(RichText::new("Right-click — column and row actions").weak());
                ui.separator();
                if ui.add_enabled(has_doc, egui::Button::new("Insert row at end")).clicked() {
                    if let Some(d) = &self.doc {
                        let at = d.df().height();
                        self.apply(Op::InsertRows { at, count: 1 });
                        self.grid.select_cell(grid::Cell { row: at, col: 0 });
                    }
                    ui.close();
                }
                if ui.add_enabled(has_doc, egui::Button::new("Find and replace…").shortcut_text("Ctrl+F")).clicked() {
                    self.form.kind = OpKind::FindReplace;
                    self.settings.show_ops = true;
                    ui.close();
                }
            });

            ui.menu_button("Data", |ui| {
                for k in OpKind::ALL {
                    if ui.add_enabled(has_doc, egui::Button::new(k.label())).clicked() {
                        self.form.kind = k;
                        self.settings.show_ops = true;
                        ui.close();
                    }
                }
            });

            ui.menu_button("View", |ui| {
                ui.checkbox(&mut self.settings.show_ops, "Operations & steps panel");
                ui.checkbox(&mut self.settings.show_summary, "Summary panel");
                ui.separator();
                let before = self.settings.theme;
                ui.radio_value(&mut self.settings.theme, ThemeChoice::System, "Follow Windows theme");
                ui.radio_value(&mut self.settings.theme, ThemeChoice::Light, "Light");
                ui.radio_value(&mut self.settings.theme, ThemeChoice::Dark, "Dark");
                if before != self.settings.theme {
                    apply_theme(ctx, self.settings.theme);
                }
                ui.separator();
                if ui.button("Reset column widths").clicked() {
                    self.grid.widths.clear();
                    ui.close();
                }
            });

            ui.menu_button(if cfg!(windows) { "Windows" } else { "Desktop" }, |ui| {
                self.windows_menu(ui);
            });

            ui.menu_button("Help", |ui| {
                if ui.button("About DataVivid").clicked() {
                    self.dialog = Some(Dialog::About);
                    ui.close();
                }
            });

            // Right side: quick actions.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(doc) = &self.doc {
                    if doc.tables.len() > 1 {
                        let mut chosen = doc.table.clone().unwrap_or_default();
                        let label = if doc.format == Format::Sqlite { "Table" } else { "Sheet" };
                        egui::ComboBox::from_id_salt("table")
                            .selected_text(&chosen)
                            .show_ui(ui, |ui| {
                                for t in &doc.tables {
                                    ui.selectable_value(&mut chosen, t.clone(), t);
                                }
                            });
                        ui.label(label);
                        if Some(&chosen) != doc.table.as_ref() {
                            let mut req = doc.request.clone();
                            req.table = Some(chosen);
                            self.request_open(req, ctx);
                        }
                    }
                }
            });
        });
    }

    fn windows_menu(&mut self, ui: &mut Ui) {
        let status = match &self.reg_status {
            winassoc::Status::Registered => "DataVivid is registered for data files.".to_string(),
            winassoc::Status::NotRegistered => "DataVivid is not registered yet.".to_string(),
            winassoc::Status::RegisteredElsewhere(p) => format!("Registered for another copy:\n{p}"),
        };
        ui.label(RichText::new(status).weak());
        ui.separator();
        let label = if self.reg_status == winassoc::Status::Registered {
            "Repair file-type registration"
        } else {
            "Register DataVivid for data files"
        };
        let (register_hint, registered_msg, default_label, default_hint) = if cfg!(windows) {
            (
                "Adds DataVivid to Explorer's \"Open with\" menu and to Settings > Default apps \
                 for CSV, TSV, Parquet, JSON, Arrow, Avro, Excel, ODS and SQLite. No admin rights needed.",
                "Registered. Choose DataVivid as default in the Default apps page.",
                "Choose DataVivid as default app…",
                "Windows only lets you pick default apps yourself. This opens Settings > Default apps on DataVivid's page.",
            )
        } else {
            (
                "Adds DataVivid to the application menu and to the file manager's \"Open with\" list \
                 for CSV, TSV, Parquet, JSON, Arrow, Avro, Excel, ODS and SQLite (current user only, no root).",
                "Registered. DataVivid now appears in the application menu and under Open with.",
                "Make DataVivid the default app",
                "Opens CSV, TSV, Parquet, JSON Lines, Arrow, Avro and SQLite files with DataVivid on double-click. \
                 JSON, Excel and ODS keep their current app.",
            )
        };
        if ui.button(label).on_hover_text(register_hint).clicked() {
            match winassoc::register() {
                Ok(()) => self.info(registered_msg),
                Err(e) => self.error(format!("Registration failed: {e}")),
            }
            self.reg_status = winassoc::status();
            ui.close();
        }
        if ui.button(default_label).on_hover_text(default_hint).clicked() {
            match winassoc::choose_defaults() {
                Ok(()) if !cfg!(windows) => self.info("DataVivid is now the default app for data files."),
                Ok(()) => {}
                Err(e) => self.error(format!("Could not set the default app: {e}")),
            }
            self.reg_status = winassoc::status();
            ui.close();
        }
        if ui.add_enabled(self.reg_status != winassoc::Status::NotRegistered, egui::Button::new("Remove registration")).clicked() {
            match winassoc::unregister() {
                Ok(()) => self.info("DataVivid's file-type registration was removed."),
                Err(e) => self.error(format!("Could not remove registration: {e}")),
            }
            self.reg_status = winassoc::status();
            ui.close();
        }
    }

    fn continue_after(&mut self, after: After, ctx: &egui::Context) {
        match after {
            After::Open(req) => self.start_load(req, ctx),
            After::Close => self.close_doc(),
            After::Quit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn close_doc(&mut self) {
        self.doc = None;
        self.preview = None;
        self.profiles.clear();
        self.profiles_for = None;
        self.grid.reset();
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if let Some(Job::Load { path, started, .. }) = &self.job {
                ui.spinner();
                ui.label(format!(
                    "Opening {} … {:.0} s",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    started.elapsed().as_secs_f32()
                ));
            } else if let Some(Job::Save { path, .. }) = &self.job {
                ui.spinner();
                ui.label(format!("Saving {} …", path.file_name().unwrap_or_default().to_string_lossy()));
            } else if let Some(doc) = &self.doc {
                let mut parts = vec![if doc.csv.is_some() { "Delimited text".to_string() } else { doc.format.label().to_string() }];
                if let Some(csv) = &doc.csv {
                    let sep = match csv.separator {
                        b',' => "comma".to_string(),
                        b';' => "semicolon".to_string(),
                        b'\t' => "tab".to_string(),
                        b'|' => "pipe".to_string(),
                        c => format!("'{}'", c as char),
                    };
                    parts.push(format!("{sep}-separated"));
                    parts.push(csv.encoding.to_string());
                    if csv.decimal_comma {
                        parts.push("decimal comma".into());
                    }
                }
                if let Some(t) = &doc.table {
                    parts.push(t.clone());
                }
                ui.label(RichText::new(parts.join(" · ")).weak())
                    .on_hover_text(format!("{}\nloaded in {:.2} s", doc.path.display(), doc.load_time.as_secs_f32()));
            }
            ui.separator();
            if let Some((msg, err, _)) = &self.status {
                let color = if *err { Color32::from_rgb(230, 90, 90) } else { ui.visuals().text_color() };
                ui.label(RichText::new(msg).color(color));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(doc) = &self.doc {
                    let df = self.preview.as_ref().map_or(doc.df(), |p| &p.df);
                    ui.label(format!("{} rows × {} columns", fmt_int(df.height()), df.width()));
                    if doc.history.is_dirty() {
                        ui.label(RichText::new("● modified").color(Color32::from_rgb(230, 160, 60)));
                    }
                    if let Some((r0, r1, c0, c1)) = self.grid.selection(df.height(), df.width()) {
                        let n = (r1 - r0 + 1) * (c1 - c0 + 1);
                        if n > 1 {
                            ui.label(RichText::new(format!("{} cells selected", fmt_int(n))).weak());
                        }
                    }
                }
            });
        });
    }

    fn central(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        if self.doc.is_none() {
            self.start_screen(ui, ctx);
            return;
        }
        // Preview banner.
        let mut apply_preview = false;
        let mut discard_preview = false;
        if let Some(p) = &self.preview {
            let doc = self.doc.as_ref().unwrap();
            if p.base_version != doc.history.version() {
                discard_preview = true;
            } else {
                egui::Frame::new()
                    .fill(ui.visuals().selection.bg_fill.gamma_multiply(0.25))
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Preview").strong());
                            ui.label(p.op.describe());
                            let (b, a) = (doc.df().shape(), p.df.shape());
                            ui.label(
                                RichText::new(format!(
                                    "rows {} → {}, columns {} → {}",
                                    fmt_int(b.0),
                                    fmt_int(a.0),
                                    b.1,
                                    a.1
                                ))
                                .weak(),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Discard").clicked() {
                                    discard_preview = true;
                                }
                                if ui.button(RichText::new("Apply").strong()).clicked() {
                                    apply_preview = true;
                                }
                            });
                        });
                    });
            }
        }
        if discard_preview {
            self.preview = None;
        }
        if apply_preview {
            if let Some(p) = self.preview.take() {
                let doc = self.doc.as_mut().unwrap();
                let desc = p.op.describe();
                doc.history.push(p.op, p.df);
                self.info(desc);
            }
        }

        let actions = {
            let doc = self.doc.as_ref().unwrap();
            let df = self.preview.as_ref().map_or(doc.df(), |p| &p.df);
            grid::show(ui, df, &self.profiles, &mut self.grid)
        };
        for a in actions {
            match a {
                GridAction::Apply(op) => {
                    if self.preview.is_some() {
                        self.error("Apply or discard the preview first");
                    } else {
                        self.apply(op);
                    }
                }
                GridAction::Column(ci, cmd) => {
                    if self.preview.is_some() {
                        self.error("Apply or discard the preview first");
                    } else {
                        self.column_command(ci, cmd);
                    }
                }
                GridAction::Status(s) => self.info(s),
            }
        }
        // Keep the operation form pointed at the selected column.
        if let (Some(doc), Some(c)) = (&self.doc, self.grid.cursor) {
            if let Some(n) = doc.df().get_column_names().get(c.col) {
                if self.form.column.is_empty() {
                    self.form.column = n.to_string();
                }
            }
        }
    }

    fn start_screen(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        if let Some(Job::Load { path, .. }) = &self.job {
            ui.centered_and_justified(|ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.35);
                    ui.add(egui::Spinner::new().size(36.0));
                    ui.label(format!("Opening {} …", path.display()));
                });
            });
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.heading(RichText::new("DataVivid").size(34.0).strong());
                ui.label(RichText::new("View, clean and convert tabular data").size(15.0).weak());
                ui.add_space(20.0);
                if ui.add(egui::Button::new(RichText::new("  Open a file…  ").size(16.0))).clicked() {
                    self.open_dialog(ctx);
                }
                ui.add_space(6.0);
                ui.label(RichText::new("or drop a file onto this window").weak());
                ui.add_space(22.0);

                if !self.settings.recent.is_empty() {
                    ui.label(RichText::new("Recent files").strong());
                    for p in self.settings.recent.clone() {
                        let exists = p.exists();
                        let label = format!(
                            "{}   —   {}",
                            p.file_name().unwrap_or_default().to_string_lossy(),
                            p.parent().map(|d| d.display().to_string()).unwrap_or_default()
                        );
                        if ui.add_enabled(exists, egui::Link::new(label)).clicked() {
                            self.open_path(p, ctx);
                        }
                    }
                    ui.add_space(22.0);
                }

                ui.label(RichText::new("Supported formats").strong());
                ui.label(
                    RichText::new(
                        "CSV · TSV · TXT (any delimiter, any encoding, .gz) · Parquet · JSON · JSON Lines · \
                         Arrow IPC / Feather · Avro · Excel (xlsx, xlsm, xlsb, xls) · ODS · SQLite",
                    )
                    .weak(),
                );
                ui.add_space(22.0);
                if (cfg!(windows) || cfg!(target_os = "linux")) && self.reg_status != winassoc::Status::Registered {
                    egui::Frame::group(ui.style()).inner_margin(12.0).show(ui, |ui| {
                        ui.set_max_width(520.0);
                        ui.label(
                            RichText::new(if cfg!(windows) {
                                "Open data files from Explorer"
                            } else {
                                "Open data files from your file manager"
                            })
                            .strong(),
                        );
                        ui.label(
                            "Register DataVivid so it appears in \"Open with\" for CSV, Parquet, Excel and the \
                             other formats, then pick it as the default app.",
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Register").clicked() {
                                match winassoc::register() {
                                    Ok(()) => self.info("Registered for data files."),
                                    Err(e) => self.error(format!("Registration failed: {e}")),
                                }
                                self.reg_status = winassoc::status();
                            }
                            let label = if cfg!(windows) {
                                "Register and choose defaults…"
                            } else {
                                "Register and make default"
                            };
                            if ui.button(label).clicked() {
                                match winassoc::choose_defaults() {
                                    Ok(()) if !cfg!(windows) => {
                                        self.info("DataVivid is now the default app for data files.")
                                    }
                                    Ok(()) => {}
                                    Err(e) => self.error(format!("Could not set the default app: {e}")),
                                }
                                self.reg_status = winassoc::status();
                            }
                        });
                    });
                }
            });
        });
    }

    // ---- left panel: operations and steps ---------------------------------

    fn ops_panel(&mut self, ui: &mut Ui) {
        let Some(doc) = &self.doc else { return };
        let df = doc.df().clone();
        let names: Vec<String> = df.get_column_names().iter().map(|s| s.to_string()).collect();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(4.0);
            ui.heading("Operations");
            egui::ComboBox::from_id_salt("opkind")
                .width(ui.available_width())
                .selected_text(self.form.kind.label())
                .show_ui(ui, |ui| {
                    for k in OpKind::ALL {
                        ui.selectable_value(&mut self.form.kind, k, k.label());
                    }
                });
            ui.label(RichText::new(self.form.kind.help()).weak().size(12.0));
            ui.add_space(4.0);
            self.op_form(ui, &df, &names);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Apply").strong()).clicked() {
                    match self.build_op(&df) {
                        Ok(op) => self.apply(op),
                        Err(e) => self.error(format!("{e:#}")),
                    }
                }
                if ui.button("Preview").on_hover_text("Show the result without changing the data yet").clicked() {
                    match self.build_op(&df).and_then(|op| {
                        let out = op.apply(&df)?;
                        Ok((op, out))
                    }) {
                        Ok((op, out)) => {
                            let v = self.doc.as_ref().unwrap().history.version();
                            self.preview = Some(Preview { op, df: out, base_version: v });
                        }
                        Err(e) => self.error(format!("{e:#}")),
                    }
                }
            });

            ui.add_space(14.0);
            ui.separator();
            self.steps_list(ui);
        });
    }

    fn op_form(&mut self, ui: &mut Ui, df: &DataFrame, names: &[String]) {
        let version = self.doc.as_ref().map_or(0, |d| d.history.version());
        let f = &mut self.form;
        let column_pick = |ui: &mut Ui, f: &mut OpForm, label: &str| {
            ui.label(label);
            egui::ComboBox::from_id_salt(("col", label))
                .width(ui.available_width())
                .selected_text(if f.column.is_empty() { "choose…" } else { &f.column })
                .show_ui(ui, |ui| {
                    for n in names {
                        ui.selectable_value(&mut f.column, n.clone(), n);
                    }
                });
        };
        let multi_pick = |ui: &mut Ui, f: &mut OpForm, label: &str, empty_means: &str| {
            ui.horizontal(|ui| {
                ui.label(label);
                if ui.small_button("all").clicked() {
                    f.columns = names.to_vec();
                }
                if ui.small_button("none").clicked() {
                    f.columns.clear();
                }
            });
            if f.columns.is_empty() && !empty_means.is_empty() {
                ui.label(RichText::new(empty_means).weak().size(12.0));
            }
            egui::ScrollArea::vertical().id_salt(("multi", label)).max_height(160.0).show(ui, |ui| {
                for n in names {
                    let mut on = f.columns.contains(n);
                    if ui.checkbox(&mut on, n).changed() {
                        if on {
                            f.columns.push(n.clone());
                        } else {
                            f.columns.retain(|c| c != n);
                        }
                    }
                }
            });
        };

        match f.kind {
            OpKind::Sort => {
                column_pick(ui, f, "Column");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut f.descending, false, "Ascending");
                    ui.radio_value(&mut f.descending, true, "Descending");
                });
            }
            OpKind::Filter => {
                column_pick(ui, f, "Column");
                egui::ComboBox::from_id_salt("cmp")
                    .width(ui.available_width())
                    .selected_text(f.cmp.label())
                    .show_ui(ui, |ui| {
                        for c in Cmp::ALL {
                            ui.selectable_value(&mut f.cmp, c, c.label());
                        }
                    });
                if f.cmp.needs_value() {
                    ui.add(egui::TextEdit::singleline(&mut f.value).hint_text("value").desired_width(f32::INFINITY));
                    ui.checkbox(&mut f.case_sensitive, "Match case");
                }
            }
            OpKind::Rename => {
                column_pick(ui, f, "Column");
                ui.label("New name");
                ui.add(egui::TextEdit::singleline(&mut f.new_name).desired_width(f32::INFINITY));
            }
            OpKind::DropColumns => multi_pick(ui, f, "Columns to delete", ""),
            OpKind::AddColumn => {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut f.new_name).hint_text("new_column").desired_width(f32::INFINITY));
                ui.label("Formula (SQL expression)");
                ui.add(
                    egui::TextEdit::multiline(&mut f.formula)
                        .hint_text("e.g. price * quantity")
                        .code_editor()
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    ui.label("Insert after");
                    let sel = if f.position >= names.len() {
                        "the last column".to_string()
                    } else if f.position == 0 {
                        "(first position)".into()
                    } else {
                        names[f.position - 1].clone()
                    };
                    egui::ComboBox::from_id_salt("pos").selected_text(sel).show_ui(ui, |ui| {
                        ui.selectable_value(&mut f.position, 0, "(first position)");
                        for (i, n) in names.iter().enumerate() {
                            ui.selectable_value(&mut f.position, i + 1, n);
                        }
                    });
                });
            }
            OpKind::Cast => {
                column_pick(ui, f, "Column");
                if let Ok(c) = df.column(&f.column) {
                    ui.label(RichText::new(format!("Current type: {}", stats::dtype_label(c.dtype()))).weak());
                }
                ui.label("New type");
                egui::ComboBox::from_id_salt("target")
                    .width(ui.available_width())
                    .selected_text(f.target.label())
                    .show_ui(ui, |ui| {
                        for t in TargetType::ALL {
                            ui.selectable_value(&mut f.target, t, t.label());
                        }
                    });
                let is_text = df.column(&f.column).is_ok_and(|c| c.dtype() == &DataType::String);
                if is_text && matches!(f.target, TargetType::Date | TargetType::Datetime) {
                    ui.label("Format (optional, strftime)");
                    ui.add(
                        egui::TextEdit::singleline(&mut f.format)
                            .hint_text("e.g. %d.%m.%Y or %m/%d/%Y %H:%M")
                            .desired_width(f32::INFINITY),
                    );
                }
                if is_text && matches!(f.target, TargetType::Float64 | TargetType::Int64) {
                    ui.checkbox(&mut f.decimal_comma, "Numbers use a decimal comma (1.234,5)");
                }
                // Conversion check: how many values would be lost.
                if !f.column.is_empty() && df.column(&f.column).is_ok() {
                    let key = (f.column.clone(), f.target, f.format.clone(), f.decimal_comma, version);
                    let stale = f
                        .cast_check
                        .as_ref()
                        .is_none_or(|(c, t, fm, dc, v, _)| (c, t, fm, dc, v) != (&key.0, &key.1, &key.2, &key.3, &key.4));
                    if stale && df.height() <= 5_000_000 {
                        let r = crate::ops::cast_failures(df, &f.column, f.target, &f.format, f.decimal_comma)
                            .map_err(|e| format!("{e:#}"));
                        f.cast_check = Some((key.0, key.1, key.2, key.3, key.4, r));
                    }
                    if let Some((_, _, _, _, _, r)) = &f.cast_check {
                        match r {
                            Ok(0) => {
                                ui.label(RichText::new("✔ All values convert").color(Color32::from_rgb(80, 170, 100)));
                            }
                            Ok(n) => {
                                ui.label(
                                    RichText::new(format!("⚠ {} value(s) can't be converted and will become missing", fmt_int(*n)))
                                        .color(Color32::from_rgb(230, 160, 60)),
                                );
                            }
                            Err(e) => {
                                ui.label(RichText::new(e).color(Color32::from_rgb(230, 90, 90)));
                            }
                        }
                    }
                }
            }
            OpKind::FillMissing => {
                column_pick(ui, f, "Column");
                ui.label("Fill with");
                egui::ComboBox::from_id_salt("strategy")
                    .width(ui.available_width())
                    .selected_text(f.strategy.label())
                    .show_ui(ui, |ui| {
                        for s in FillStrategy::ALL {
                            ui.selectable_value(&mut f.strategy, s, s.label());
                        }
                    });
                if f.strategy == FillStrategy::Value {
                    ui.add(egui::TextEdit::singleline(&mut f.value).hint_text("value").desired_width(f32::INFINITY));
                }
            }
            OpKind::DropMissing => multi_pick(ui, f, "Columns", "No columns ticked: a missing value in any column drops the row."),
            OpKind::DropDuplicates => multi_pick(ui, f, "Compare columns", "No columns ticked: rows must be equal in every column."),
            OpKind::FindReplace => {
                ui.label("Find");
                ui.add(egui::TextEdit::singleline(&mut f.find).desired_width(f32::INFINITY));
                ui.label("Replace with");
                ui.add(egui::TextEdit::singleline(&mut f.replace).desired_width(f32::INFINITY));
                ui.checkbox(&mut f.match_case, "Match case");
                ui.checkbox(&mut f.whole_cell, "Match entire cell");
                ui.checkbox(&mut f.regex, "Regular expression ($1 = group 1)");
                multi_pick(ui, f, "In columns", "No columns ticked: all text columns.");
            }
            OpKind::Text => {
                column_pick(ui, f, "Column");
                for t in TextTransform::ALL {
                    ui.radio_value(&mut f.transform, t, t.label());
                }
            }
            OpKind::Round => {
                column_pick(ui, f, "Column");
                ui.horizontal(|ui| {
                    ui.label("Decimals");
                    ui.add(egui::DragValue::new(&mut f.decimals).range(0..=12));
                });
            }
            OpKind::Sql => {
                ui.label("Query (the table is called data)");
                ui.add(
                    egui::TextEdit::multiline(&mut f.sql)
                        .code_editor()
                        .desired_rows(6)
                        .desired_width(f32::INFINITY),
                );
                ui.label(
                    RichText::new("e.g. SELECT region, SUM(sales) AS total FROM data GROUP BY region ORDER BY total DESC")
                        .weak()
                        .size(12.0),
                );
            }
        }
    }

    fn steps_list(&mut self, ui: &mut Ui) {
        let Some(doc) = &mut self.doc else { return };
        ui.horizontal(|ui| {
            ui.heading("Steps");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let can_redo = doc.history.can_redo();
                let can_undo = doc.history.can_undo();
                if ui.add_enabled(can_redo, egui::Button::new("Redo")).clicked() {
                    doc.history.redo();
                }
                if ui.add_enabled(can_undo, egui::Button::new("Undo")).clicked() {
                    doc.history.undo();
                }
            });
        });
        ui.label(RichText::new("Click a step to go back to it. A new change after going back discards the later steps.").weak().size(12.0));
        let cursor = doc.history.cursor();
        let mut jump = None;
        let base_label = if doc.history.folded() > 0 {
            format!("Loaded file (+{} older steps)", doc.history.folded())
        } else {
            format!("Loaded file ({} × {})", fmt_int(doc.loaded_shape.0), doc.loaded_shape.1)
        };
        if ui.selectable_label(cursor == 0, base_label).clicked() {
            jump = Some(0);
        }
        for (i, step) in doc.history.steps().iter().enumerate() {
            let n = i + 1;
            let text = format!("{n}. {}", step.op.describe());
            let rt = if n > cursor { RichText::new(text).weak().strikethrough() } else { RichText::new(text) };
            if ui.selectable_label(n == cursor, rt).clicked() {
                jump = Some(n);
            }
        }
        if let Some(n) = jump {
            doc.history.jump_to(n);
            self.preview = None;
        }
    }

    // ---- right panel: summary ---------------------------------------------

    fn summary_panel(&mut self, ui: &mut Ui) {
        let Some(doc) = &self.doc else { return };
        let df = doc.df();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(4.0);
            let col = self.grid.cursor.and_then(|c| df.get_column_names().get(c.col).map(|s| s.to_string()));
            match col {
                Some(name) => {
                    ui.heading(&name);
                    match self.profiles.get(&name) {
                        Some(p) => profile_ui(ui, p),
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Computing statistics…");
                            });
                        }
                    }
                    ui.add_space(10.0);
                    ui.label(RichText::new("Right-click the column header for column actions.").weak().size(12.0));
                }
                None => {
                    ui.heading("Dataset");
                    let missing: usize = df.columns().iter().map(|c| c.null_count()).sum();
                    let cells = (df.height() * df.width()).max(1);
                    egui::Grid::new("ds").num_columns(2).striped(true).show(ui, |ui| {
                        ui.label("Rows");
                        ui.label(fmt_int(df.height()));
                        ui.end_row();
                        ui.label("Columns");
                        ui.label(df.width().to_string());
                        ui.end_row();
                        ui.label("Missing cells");
                        ui.label(format!("{} ({:.1}%)", fmt_int(missing), missing as f64 * 100.0 / cells as f64));
                        ui.end_row();
                        ui.label("Memory");
                        ui.label(fmt_bytes(df.estimated_size()));
                        ui.end_row();
                        if let Ok(meta) = std::fs::metadata(&doc.path) {
                            ui.label("File size");
                            ui.label(fmt_bytes(meta.len() as usize));
                            ui.end_row();
                        }
                    });
                    ui.add_space(10.0);
                    ui.label(RichText::new("Columns").strong());
                    egui::Grid::new("cols").num_columns(3).striped(true).show(ui, |ui| {
                        for (i, c) in df.columns().iter().enumerate() {
                            if ui.link(c.name().as_str()).clicked() {
                                self.grid.select_column(i);
                            }
                            ui.label(RichText::new(stats::dtype_label(c.dtype())).weak());
                            let m = c.null_count();
                            if m > 0 {
                                ui.label(RichText::new(format!("{:.1}% missing", m as f64 * 100.0 / df.height().max(1) as f64)).weak());
                            } else {
                                ui.label("");
                            }
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                    ui.label(RichText::new("Click a cell or column header to see column statistics.").weak().size(12.0));
                }
            }
        });
    }

    // ---- dialogs ----------------------------------------------------------

    fn open_import_dialog(&mut self) {
        let Some(doc) = &self.doc else { return };
        let mut req = doc.request.clone();
        // Show what was detected rather than "auto".
        if let Some(csv) = &doc.csv {
            if req.csv.separator.is_none() {
                req.csv.separator = Some(csv.separator);
            }
            if req.csv.decimal_comma.is_none() {
                req.csv.decimal_comma = Some(csv.decimal_comma);
            }
        }
        let preview = if req.format.is_delimited() {
            io::preview_text(&req.path, req.csv.encoding, 40).unwrap_or_else(|e| format!("{e:#}"))
        } else {
            String::new()
        };
        let sep_text = req.csv.separator.map(|b| (b as char).to_string()).unwrap_or_default();
        self.dialog = Some(Dialog::Import { req, preview, sep_text });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.dialog.take() else { return };
        let mut keep = true;
        let mut then: Option<Box<dyn FnOnce(&mut App, &egui::Context)>> = None;
        match dialog {
            Dialog::About => {
                let r = egui::Modal::new(Id::unique("about")).show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.heading("DataVivid");
                    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                    ui.add_space(6.0);
                    ui.label("A standalone viewer and editor for tabular data, built with Rust, Polars and egui.");
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Originally developed by the Prevail Team at FH Steyr \
                             (University of Applied Sciences Upper Austria).",
                        )
                        .strong(),
                    );
                    ui.label("Open source under the MIT License: free for everyone to use, change and share.");
                    ui.hyperlink_to("github.com/FH-Prevail/DataVivid", env!("CARGO_PKG_REPOSITORY"));
                    ui.add_space(6.0);
                    ui.label(RichText::new("Reads").strong());
                    ui.label("CSV, TSV, TXT (.gz), Parquet, JSON, JSON Lines, Arrow IPC/Feather, Avro, Excel (xlsx/xlsm/xlsb/xls), ODS, SQLite");
                    ui.label(RichText::new("Writes").strong());
                    ui.label("CSV, TSV, Parquet, JSON, JSON Lines, Arrow IPC/Feather, Avro, Excel (xlsx), SQLite");
                    ui.add_space(6.0);
                    ui.label(RichText::new("Shortcuts").strong());
                    egui::Grid::new("keys").num_columns(2).show(ui, |ui| {
                        for (k, d) in [
                            ("Ctrl+O", "Open"),
                            ("Ctrl+S / Ctrl+Shift+S", "Save / Save as"),
                            ("Ctrl+Z / Ctrl+Y", "Undo / Redo"),
                            ("Ctrl+F", "Find and replace"),
                            ("F5", "Reload from disk"),
                            ("Enter, F2 or typing", "Edit the selected cell"),
                            ("Ctrl+C / Ctrl+V", "Copy / paste cells"),
                            ("Delete", "Clear selected cells"),
                            ("Ctrl+Arrows, Ctrl+Home/End", "Jump to edges"),
                        ] {
                            ui.label(RichText::new(k).monospace());
                            ui.label(d);
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                    if ui.button("Close").clicked() {
                        ui.close();
                    }
                });
                if r.should_close() {
                    keep = false;
                }
                if keep {
                    self.dialog = Some(Dialog::About);
                }
            }
            Dialog::Discard(after) => {
                let mut choice = None;
                let r = egui::Modal::new(Id::unique("discard")).show(ctx, |ui| {
                    ui.set_width(400.0);
                    ui.heading("Unsaved changes");
                    ui.label("The current file has changes that are not saved.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Save").strong()).clicked() {
                            choice = Some(0);
                        }
                        if ui.button("Discard changes").clicked() {
                            choice = Some(1);
                        }
                        if ui.button("Cancel").clicked() {
                            choice = Some(2);
                        }
                    });
                });
                if r.should_close() && choice.is_none() {
                    choice = Some(2);
                }
                match choice {
                    Some(0) => {
                        // Save first; the user repeats the action afterwards.
                        let a = after.clone();
                        then = Some(Box::new(move |app, ctx| {
                            app.after_save = Some(a);
                            app.save(ctx);
                        }));
                    }
                    Some(1) => {
                        let a = after.clone();
                        then = Some(Box::new(move |app, ctx| app.continue_after(a, ctx)));
                    }
                    Some(_) => {}
                    None => self.dialog = Some(Dialog::Discard(after)),
                }
            }
            Dialog::ExcelSave => {
                let mut choice = None;
                egui::Modal::new(Id::unique("excelsave")).show(ctx, |ui| {
                    ui.set_width(460.0);
                    ui.heading("Save Excel workbook");
                    let sheets = self.doc.as_ref().map_or(1, |d| d.tables.len());
                    ui.label(format!(
                        "DataVivid writes plain values. Overwriting keeps only this sheet{}; formatting, formulas, \
                         charts{} are lost.",
                        if sheets > 1 { format!(" (the other {} sheet(s) are removed)", sheets - 1) } else { String::new() },
                        if sheets > 1 { "" } else { " and other workbook features" }
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Save as new file…").strong()).clicked() {
                            choice = Some(0);
                        }
                        if ui.button("Overwrite").clicked() {
                            choice = Some(1);
                        }
                        if ui.button("Cancel").clicked() {
                            choice = Some(2);
                        }
                    });
                });
                match choice {
                    Some(0) => then = Some(Box::new(|app, ctx| app.save_as(ctx))),
                    Some(1) => {
                        then = Some(Box::new(|app, ctx| {
                            if let Some(d) = &app.doc {
                                let path = d.path.clone();
                                let opts = app.save_options_for(Format::Excel, &path);
                                app.start_save(path, opts, ctx);
                            }
                        }))
                    }
                    Some(_) => self.after_save = None,
                    None => self.dialog = Some(Dialog::ExcelSave),
                }
            }
            Dialog::Import { mut req, mut preview, mut sep_text } => {
                let mut action = None;
                egui::Modal::new(Id::unique("import")).show(ctx, |ui| {
                    ui.set_width(640.0);
                    ui.heading("Import options");
                    ui.label(RichText::new(req.path.display().to_string()).weak());
                    ui.add_space(6.0);
                    let delimited = req.format.is_delimited();
                    egui::Grid::new("imp").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        if delimited {
                            ui.label("Separator");
                            ui.horizontal(|ui| {
                                for (label, b) in [("Comma", b','), ("Semicolon", b';'), ("Tab", b'\t'), ("Pipe", b'|')] {
                                    if ui.radio(req.csv.separator == Some(b), label).clicked() {
                                        req.csv.separator = Some(b);
                                        sep_text = (b as char).to_string();
                                    }
                                }
                                ui.label("Other:");
                                let r = ui.add(egui::TextEdit::singleline(&mut sep_text).desired_width(24.0).char_limit(1));
                                if r.changed() {
                                    if let Some(c) = sep_text.chars().next().filter(|c| c.is_ascii()) {
                                        req.csv.separator = Some(c as u8);
                                    }
                                }
                            });
                            ui.end_row();
                            ui.label("Encoding");
                            let before = req.csv.encoding;
                            egui::ComboBox::from_id_salt("enc").selected_text(req.csv.encoding.label()).show_ui(ui, |ui| {
                                for e in TextEncoding::ALL {
                                    ui.selectable_value(&mut req.csv.encoding, e, e.label());
                                }
                            });
                            if before != req.csv.encoding {
                                preview = io::preview_text(&req.path, req.csv.encoding, 40).unwrap_or_else(|e| format!("{e:#}"));
                            }
                            ui.end_row();
                            ui.label("Decimal separator");
                            ui.horizontal(|ui| {
                                let mut dc = req.csv.decimal_comma.unwrap_or(false);
                                ui.radio_value(&mut dc, false, "Point (1.5)");
                                ui.radio_value(&mut dc, true, "Comma (1,5)");
                                req.csv.decimal_comma = Some(dc);
                            });
                            ui.end_row();
                        }
                        ui.label("Header");
                        ui.checkbox(&mut req.csv.has_header, "First row holds the column names");
                        ui.end_row();
                        ui.label("Skip rows");
                        ui.add(egui::DragValue::new(&mut req.csv.skip_rows).range(0..=100_000));
                        ui.end_row();
                        if delimited {
                            ui.label("Detect types");
                            ui.checkbox(&mut req.csv.infer_types, "Numbers, dates and booleans (off = everything is text, keeps leading zeros)");
                            ui.end_row();
                            ui.label("Missing values");
                            ui.add(egui::TextEdit::singleline(&mut req.csv.null_values).hint_text("e.g. NA, n/a, -").desired_width(200.0));
                            ui.end_row();
                        }
                    });
                    if delimited {
                        ui.add_space(6.0);
                        ui.label(RichText::new("File preview").strong());
                        egui::ScrollArea::both().max_height(220.0).show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(&preview).monospace().size(12.0)).extend());
                        });
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Reload with these options").strong()).clicked() {
                            action = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            action = Some(false);
                        }
                    });
                });
                match action {
                    Some(true) => {
                        let r = req.clone();
                        then = Some(Box::new(move |app, ctx| app.request_open(r, ctx)));
                    }
                    Some(false) => {}
                    None => self.dialog = Some(Dialog::Import { req, preview, sep_text }),
                }
            }
        }
        if let Some(f) = then {
            f(self, ctx);
        }
    }
}

fn profile_ui(ui: &mut Ui, p: &ColumnProfile) {
    egui::Grid::new("profile").num_columns(2).striped(true).show(ui, |ui| {
        let row = |ui: &mut Ui, k: &str, v: String| {
            ui.label(k);
            ui.label(v);
            ui.end_row();
        };
        row(ui, "Type", p.dtype.clone());
        row(ui, "Rows", fmt_int(p.rows));
        row(ui, "Missing", format!("{} ({:.1}%)", fmt_int(p.missing), p.missing_pct()));
        row(ui, "Distinct", fmt_int(p.unique));
        if let Some(n) = &p.numeric {
            row(ui, "Min", fmt_num(n.min));
            row(ui, "25%", fmt_num(n.q25));
            row(ui, "Median", fmt_num(n.median));
            row(ui, "Mean", fmt_num(n.mean));
            row(ui, "75%", fmt_num(n.q75));
            row(ui, "Max", fmt_num(n.max));
            row(ui, "Std. dev.", fmt_num(n.std));
            row(ui, "Zeros", fmt_int(n.zeros));
            row(ui, "Negative", fmt_int(n.negatives));
            if n.nan > 0 {
                row(ui, "NaN", fmt_int(n.nan));
            }
        }
        if let Some((a, b)) = &p.min_max {
            row(ui, "First (sorted)", a.clone());
            row(ui, "Last (sorted)", b.clone());
        }
        if let Some((a, b)) = p.text_len {
            row(ui, "Length", format!("{a} – {b} characters"));
        }
    });

    if let Some(n) = &p.numeric {
        ui.add_space(10.0);
        ui.label(RichText::new("Distribution").strong());
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 120.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        let max = *n.hist.iter().max().unwrap_or(&1) as f32;
        let bw = rect.width() / n.hist.len() as f32;
        let accent = ui.visuals().hyperlink_color;
        let mut hovered = None;
        for (i, &h) in n.hist.iter().enumerate() {
            let bh = (h as f32 / max.max(1.0)) * (rect.height() - 4.0);
            let bar = egui::Rect::from_min_max(
                egui::pos2(rect.min.x + i as f32 * bw + 1.0, rect.max.y - bh.max(if h > 0 { 1.5 } else { 0.0 })),
                egui::pos2(rect.min.x + (i + 1) as f32 * bw - 1.0, rect.max.y),
            );
            let col_rect = egui::Rect::from_min_max(egui::pos2(bar.min.x, rect.min.y), bar.max);
            let is_hover = resp.hover_pos().is_some_and(|pp| col_rect.contains(pp));
            if is_hover {
                hovered = Some((i, h));
            }
            painter.rect_filled(bar, 2.0, if is_hover { accent } else { accent.gamma_multiply(0.75) });
        }
        if let Some((i, h)) = hovered {
            let one_per_value = n.min.fract() == 0.0 && (n.max - n.min) as usize + 1 == n.hist.len();
            if one_per_value {
                resp.on_hover_text(format!("{}: {} rows", fmt_num(n.min + i as f64), fmt_int(h as usize)));
            } else {
                let w = (n.max - n.min) / n.hist.len() as f64;
                let lo = n.min + w * i as f64;
                resp.on_hover_text(format!("{} – {}: {} rows", fmt_num(lo), fmt_num(lo + w), fmt_int(h as usize)));
            }
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(fmt_num(n.min)).weak().size(11.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(fmt_num(n.max)).weak().size(11.0));
            });
        });
    }

    if !p.top.is_empty() {
        ui.add_space(10.0);
        ui.label(RichText::new("Most frequent values").strong());
        let total = (p.rows - p.missing).max(1) as f32;
        let accent = ui.visuals().hyperlink_color;
        for (v, n) in &p.top {
            let frac = *n as f32 / total;
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::hover());
            let painter = ui.painter_at(rect);
            painter.rect_filled(
                egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac, rect.height())),
                3.0,
                accent.gamma_multiply(0.3),
            );
            let label = if v.is_empty() { "(empty)".to_string() } else { v.clone() };
            painter.text(
                rect.left_center() + egui::vec2(6.0, 0.0),
                egui::Align2::LEFT_CENTER,
                label,
                egui::FontId::proportional(12.5),
                ui.visuals().text_color(),
            );
            painter.text(
                rect.right_center() - egui::vec2(6.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                format!("{} · {:.1}%", fmt_int(*n as usize), frac * 100.0),
                egui::FontId::proportional(11.5),
                ui.visuals().weak_text_color(),
            );
        }
    }
}

fn apply_theme(ctx: &egui::Context, t: ThemeChoice) {
    ctx.set_theme(match t {
        ThemeChoice::System => egui::ThemePreference::System,
        ThemeChoice::Light => egui::ThemePreference::Light,
        ThemeChoice::Dark => egui::ThemePreference::Dark,
    });
}

/// Uses Segoe UI (and Segoe UI Symbol as fallback) on Windows for a native
/// look and wider Unicode coverage; egui's built-in fonts stay as fallback.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let candidates = [("segoe-ui", "segoeui.ttf"), ("segoe-symbol", "seguisym.ttf")];
    let mut added = Vec::new();
    for (name, file) in candidates {
        let path = Path::new(&windir).join("Fonts").join(file);
        if let Ok(bytes) = std::fs::read(&path) {
            fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(bytes)));
            added.push(name);
        }
    }
    if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        for (i, n) in added.iter().enumerate() {
            if *n == "segoe-ui" {
                fam.insert(i, n.to_string());
            } else {
                fam.push(n.to_string());
            }
        }
    }
    ctx.set_fonts(fonts);
}

pub fn fmt_int(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn fmt_num(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    let a = v.abs();
    if a != 0.0 && (a >= 1e12 || a < 1e-4) {
        format!("{v:.4e}")
    } else if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        let s = format!("{v:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn fmt_bytes(b: usize) -> String {
    let b = b as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", b / 1024.0 / 1024.0 / 1024.0)
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / 1024.0 / 1024.0)
    } else if b >= 1024.0 {
        format!("{:.1} KB", b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
}
