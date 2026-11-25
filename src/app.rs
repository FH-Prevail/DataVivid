//! The DataVivid window: menus, side panels, dialogs and background jobs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use egui::{Color32, Id, Key, KeyboardShortcut, Modifiers, RichText, Ui};
use polars::prelude::*;

use crate::chart;
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
    show_chart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { recent: Vec::new(), theme: ThemeChoice::System, show_ops: true, show_summary: true, show_chart: true }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq)]
enum ThemeChoice {
    System,
    Light,
    Dark,
}

/// The open file. `request`, `history`, `loaded_shape`, `load_time` and `id`
/// belong to the sheet on screen (`table`); the other sheets that were opened
/// wait in `parked`.
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
    /// Number of the table on screen. No two loaded tables share one, so a
    /// chart is never taken for the chart of another sheet.
    id: u64,
    /// Number of this file among the opened ones. A sheet read in the
    /// background is dropped if another file was opened meanwhile.
    serial: u64,
    /// Sheets that were opened before and are off screen now, with their
    /// changes and steps. Going back to one is instant and loses nothing;
    /// nothing is written to disk until the user saves.
    parked: HashMap<String, Parked>,
}

/// A sheet of the open file that is not on screen.
struct Parked {
    request: LoadRequest,
    history: History,
    loaded_shape: (usize, usize),
    load_time: Duration,
    id: u64,
    grid: grid::GridView,
    /// `None` until the sheet has been on screen.
    chart: Option<chart::ChartState>,
    profiles: HashMap<String, ColumnProfile>,
    profiles_for: Option<u64>,
}

/// What a save writes, which decides what counts as saved afterwards.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SaveScope {
    /// The table of a file that holds one. The document moves to that file.
    Table,
    /// Every sheet of a workbook, into one `.xlsx`.
    Workbook,
    /// One sheet of several, into a file of its own. The open file stays
    /// the document, and its other sheets are not in the written file.
    Sheet,
}

impl Doc {
    fn df(&self) -> &DataFrame {
        self.history.current()
    }
    fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
    /// "sheet" or "table", as the file calls its parts.
    fn noun(&self) -> &'static str {
        if self.format == Format::Sqlite { "table" } else { "sheet" }
    }
    /// A spreadsheet with several sheets, which is saved as a whole.
    fn is_workbook(&self) -> bool {
        matches!(self.format, Format::Excel | Format::Ods) && self.tables.len() > 1
    }

    /// The steps of a sheet that is in memory.
    fn sheet(&self, table: Option<&str>) -> Option<&History> {
        if table == self.table.as_deref() { Some(&self.history) } else { self.parked.get(table?).map(|p| &p.history) }
    }
    fn sheet_mut(&mut self, table: Option<&str>) -> Option<(&mut History, &mut LoadRequest)> {
        if table == self.table.as_deref() {
            Some((&mut self.history, &mut self.request))
        } else {
            self.parked.get_mut(table?).map(|p| (&mut p.history, &mut p.request))
        }
    }

    /// Makes `name` the sheet on screen, with the table and steps in
    /// `sheet`. `sheet` gets those of the sheet that was on screen, whose
    /// name is returned.
    fn swap_sheet(&mut self, name: String, sheet: &mut Parked) -> Option<String> {
        std::mem::swap(&mut self.request, &mut sheet.request);
        std::mem::swap(&mut self.history, &mut sheet.history);
        std::mem::swap(&mut self.loaded_shape, &mut sheet.loaded_shape);
        std::mem::swap(&mut self.load_time, &mut sheet.load_time);
        std::mem::swap(&mut self.id, &mut sheet.id);
        self.table.replace(name)
    }
    /// Keeps a sheet that left the screen. One that was replaced by a new
    /// reading of itself is dropped.
    fn park(&mut self, name: Option<String>, sheet: Parked) {
        if let Some(name) = name.filter(|n| Some(n) != self.table.as_ref()) {
            self.parked.insert(name, sheet);
        }
    }

    /// Unsaved changes on any sheet, also the ones off screen.
    fn is_dirty(&self) -> bool {
        self.history.is_dirty() || self.parked.values().any(|p| p.history.is_dirty())
    }
    /// The sheets with unsaved changes, in tab order.
    fn dirty_sheets(&self) -> Vec<String> {
        self.tables.iter().filter(|t| self.sheet(Some(t.as_str())).is_some_and(|h| h.is_dirty())).cloned().collect()
    }

    fn save_scope(&self, format: Format) -> SaveScope {
        if self.tables.len() <= 1 {
            SaveScope::Table
        } else if format == Format::Excel && self.is_workbook() {
            SaveScope::Workbook
        } else {
            SaveScope::Sheet
        }
    }

    /// The sheets of the workbook as they go into an `.xlsx`, and which of
    /// them are written from memory (with their history version). A sheet
    /// that is as in the file, or was never opened, is copied from the file.
    fn workbook_sheets(&self) -> (Vec<io::SheetOut>, Vec<(Option<String>, u64)>) {
        let mut written = Vec::new();
        let sheets = self
            .tables
            .iter()
            .zip(io::xlsx_sheet_names(&self.tables))
            .map(|(source, name)| {
                let changed = self.sheet(Some(source.as_str())).filter(|h| h.differs_from_file());
                if let Some(h) = changed {
                    written.push((Some(source.clone()), h.version()));
                }
                io::SheetOut { name, source: source.clone(), df: changed.map(|h| h.current().clone()) }
            })
            .collect();
        (sheets, written)
    }

    /// Gives the sheets the names they have in the workbook just written.
    fn rename_sheets(&mut self, names: Vec<String>) {
        if names.len() != self.tables.len() {
            return;
        }
        let old = std::mem::replace(&mut self.tables, names);
        let renamed = |name: &str| old.iter().position(|o| o == name).map(|i| self.tables[i].clone());
        if let Some(new) = self.table.as_deref().and_then(renamed) {
            self.request.table = Some(new.clone());
            self.table = Some(new);
        }
        let parked = std::mem::take(&mut self.parked);
        for (name, mut p) in parked {
            let new = renamed(&name).unwrap_or(name);
            p.request.table = Some(new.clone());
            self.parked.insert(new, p);
        }
    }

    /// Records a finished save and returns the line for the status bar.
    fn finish_save(&mut self, save: SaveJob) -> String {
        let SaveJob { path, opts, scope, written, names } = save;
        let format = opts.format;
        // What was written is saved, unless it changed while the file was
        // being written: then the file holds a table that is not on screen.
        for (table, version) in &written {
            let Some((history, request)) = self.sheet_mut(table.as_deref()) else { continue };
            let unchanged = history.version() == *version;
            match scope {
                SaveScope::Sheet if unchanged => history.mark_exported(),
                SaveScope::Sheet => {}
                _ if unchanged => history.mark_saved(),
                _ => history.mark_unsaved(),
            }
            if scope != SaveScope::Sheet {
                // The file now starts with a header row, whatever it had before.
                request.csv.has_header = true;
                request.csv.skip_rows = 0;
            }
        }
        match scope {
            SaveScope::Sheet => format!(
                "Exported the {} '{}' to {} (the other {}s are not in that file)",
                self.noun(),
                written.first().and_then(|(t, _)| t.clone()).unwrap_or_default(),
                path.display(),
                self.noun()
            ),
            SaveScope::Workbook => {
                self.rename_sheets(names);
                self.move_to(&path, format);
                format!("Saved {} with all {} sheets", path.display(), self.tables.len())
            }
            SaveScope::Table => {
                if path != self.path {
                    // The document now lives in the new file.
                    self.move_to(&path, format);
                    if !format.is_delimited() {
                        self.csv = None;
                    }
                    if !matches!(format, Format::Excel | Format::Sqlite) {
                        // Not a file with named sheets any more.
                        self.tables.clear();
                        self.table = None;
                        self.request.table = None;
                    }
                }
                let mut note = String::new();
                if format.is_delimited() {
                    if let Some(c) = &mut self.csv {
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
                    self.request.csv.encoding = TextEncoding::Auto;
                    self.request.csv.separator = None;
                    self.request.csv.decimal_comma = None;
                }
                format!("Saved {}{note}", path.display())
            }
        }
    }

    /// The document now lives in `path`.
    fn move_to(&mut self, path: &Path, format: Format) {
        self.path = path.to_path_buf();
        self.format = format;
        for request in std::iter::once(&mut self.request).chain(self.parked.values_mut().map(|p| &mut p.request)) {
            request.path = path.to_path_buf();
            request.format = format;
        }
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
    Load(LoadRequest, LoadKind),
    Close,
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LoadKind {
    /// Opens a file, or reads the open one again: replaces everything.
    Open,
    /// Another sheet of the open file. The others stay in memory.
    Sheet,
    /// The sheet on screen again, with other import options. The other
    /// sheets keep their changes.
    Reimport,
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

/// The Excel-style "filter by values" list of one column.
struct ValueFilter {
    column: String,
    /// Distinct values (as text, `None` = missing) with their row counts.
    items: Vec<(Option<String>, u64)>,
    checked: Vec<bool>,
    search: String,
    /// Indices into `items` that match `search`, and the search they are for.
    shown: Vec<usize>,
    shown_for: Option<String>,
    /// The column's value filter is the last step, so OK replaces that step
    /// (and the list shows the values from before it), like reopening a
    /// filter in Excel.
    replaces: bool,
    pos: egui::Pos2,
    /// History version the list was built for; any change closes it.
    version: u64,
    first_frame: bool,
    numeric: bool,
}

enum ValueFilterAction {
    Ok,
    Cancel,
    Clear,
    Sort(bool),
    Condition,
}

enum Job {
    Load {
        rx: Receiver<Result<Loaded>>,
        path: PathBuf,
        started: Instant,
        kind: LoadKind,
        /// `Doc::serial` of the file a sheet is read for.
        serial: u64,
        table: Option<String>,
    },
    Save { rx: Receiver<Result<()>>, save: SaveJob },
}

struct SaveJob {
    path: PathBuf,
    opts: SaveOptions,
    scope: SaveScope,
    /// The sheets written from memory, with their history version when the
    /// save started: one that changed since is not saved after all.
    written: Vec<(Option<String>, u64)>,
    /// For a workbook: the sheet names in the written file, in tab order.
    names: Vec<String>,
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
    value_filter: Option<ValueFilter>,
    chart: chart::ChartState,
    /// Counts loaded tables, so charts of different files or sheets never mix.
    loads: u64,
    /// Scroll the tab of the sheet on screen into view.
    reveal_tab: bool,
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
            value_filter: None,
            chart: chart::ChartState::default(),
            loads: 0,
            reveal_tab: false,
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
        self.request_load(req, LoadKind::Open, ctx);
    }

    fn request_load(&mut self, req: LoadRequest, kind: LoadKind, ctx: &egui::Context) {
        let after = After::Load(req, kind);
        if self.unsaved_before(&after) {
            self.dialog = Some(Dialog::Discard(after));
        } else {
            self.continue_after(after, ctx);
        }
    }

    /// Whether `after` would drop changes that are not saved. Changes on a
    /// sheet that is off screen count as much as those on screen.
    fn unsaved_before(&self, after: &After) -> bool {
        let Some(doc) = &self.doc else { return false };
        match after {
            After::Load(_, LoadKind::Sheet) => false,
            After::Load(_, LoadKind::Reimport) => doc.history.is_dirty(),
            After::Load(_, LoadKind::Open) | After::Close | After::Quit => doc.is_dirty(),
        }
    }

    fn start_load(&mut self, req: LoadRequest, kind: LoadKind, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let path = req.path.clone();
        let table = req.table.clone();
        let serial = self.doc.as_ref().map_or(0, |d| d.serial);
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(io::load(req));
            ctx2.request_repaint();
        });
        self.job = Some(Job::Load { rx, path, started: Instant::now(), kind, serial, table });
        if kind != LoadKind::Sheet {
            self.preview = None;
        }
    }

    fn finish_load(&mut self, loaded: Loaded, started: Instant, kind: LoadKind, serial: u64) {
        match kind {
            LoadKind::Open => self.open_loaded(loaded, started),
            // The file was closed or replaced while the sheet was read.
            _ if self.doc.as_ref().is_none_or(|d| d.serial != serial) => {}
            _ => self.sheet_loaded(loaded, started, kind),
        }
    }

    fn open_loaded(&mut self, loaded: Loaded, started: Instant) {
        let path = loaded.request.path.clone();
        let mut request = loaded.request.clone();
        request.table = loaded.table.clone();
        let shape = loaded.df.shape();
        let note = loaded.notes.join("; ");
        self.loads += 1;
        self.chart.reset();
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
            id: self.loads,
            serial: self.loads,
            parked: HashMap::new(),
        });
        self.grid.reset();
        self.profiles.clear();
        self.profiles_for = None;
        self.value_filter = None;
        self.form.column.clear();
        self.form.columns.clear();
        self.form.cast_check = None;
        self.reveal_tab = true;
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

    // ---- sheets -----------------------------------------------------------

    /// Shows another sheet of the open file. One that was open before comes
    /// back as it was left. Otherwise it is read from the file in the
    /// background, while the current sheet stays on screen.
    fn switch_sheet(&mut self, name: &str, ctx: &egui::Context) {
        let Some(doc) = &mut self.doc else { return };
        if !doc.tables.iter().any(|t| t == name) {
            return;
        }
        // The newest choice wins over a sheet that is still being read.
        let reading_sheet = matches!(self.job, Some(Job::Load { kind: LoadKind::Sheet, .. }));
        if doc.table.as_deref() == Some(name) {
            // Choosing the sheet on screen means staying here after all.
            if reading_sheet {
                self.job = None;
            }
            return;
        }
        if let Some(parked) = doc.parked.remove(name) {
            if reading_sheet {
                self.job = None;
            }
            self.show_sheet(name.to_string(), parked);
            return;
        }
        if self.job.is_some() && !reading_sheet {
            let noun = doc.noun();
            self.info(format!("One moment, the file is being read or written. Then choose the {noun} again."));
            return;
        }
        let mut req = doc.request.clone();
        req.table = Some(name.to_string());
        self.start_load(req, LoadKind::Sheet, ctx);
    }

    /// Goes to the next (`by` = 1) or previous (-1) sheet.
    fn step_sheet(&mut self, by: isize, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        // Counted from the sheet that is on its way, so pressing twice moves two.
        let from = match &self.job {
            Some(Job::Load { kind: LoadKind::Sheet, table: Some(t), .. }) => Some(t),
            _ => doc.table.as_ref(),
        };
        let Some(i) = doc.tables.iter().position(|t| Some(t) == from) else { return };
        let j = i.saturating_add_signed(by).min(doc.tables.len() - 1);
        if j != i {
            let name = doc.tables[j].clone();
            self.switch_sheet(&name, ctx);
        }
    }

    /// A sheet arrived from the file.
    fn sheet_loaded(&mut self, loaded: Loaded, started: Instant, kind: LoadKind) {
        let Some(doc) = &mut self.doc else { return };
        let wanted = loaded.request.table.clone();
        let Some(name) = loaded.table.clone().filter(|n| Some(n) == wanted.as_ref()) else {
            let msg = format!(
                "The {} '{}' is no longer in the file. Reload the file (F5) to see what it holds now.",
                doc.noun(),
                wanted.unwrap_or_default()
            );
            self.error(msg);
            return;
        };
        let shape = loaded.df.shape();
        let note = loaded.notes.join("; ");
        self.loads += 1;
        let incoming = Parked {
            request: loaded.request,
            history: History::new(loaded.df),
            loaded_shape: shape,
            load_time: started.elapsed(),
            id: self.loads,
            grid: grid::GridView::default(),
            chart: None,
            profiles: HashMap::new(),
            profiles_for: None,
        };
        if kind == LoadKind::Reimport && doc.table.as_ref() != Some(&name) {
            // The user moved to another sheet while this one was read again.
            doc.parked.insert(name, incoming);
            return;
        }
        self.show_sheet(name, incoming);
        let secs = started.elapsed().as_secs_f32();
        self.info(format!(
            "Loaded {} rows × {} columns in {secs:.2} s{}",
            fmt_int(shape.0),
            shape.1,
            if note.is_empty() { String::new() } else { format!(" — {note}") }
        ));
    }

    /// Puts `incoming` on screen as the sheet `name`. The sheet that was on
    /// screen is kept in memory with its steps, scroll position, selection,
    /// statistics and chart, unless it is `name` itself being replaced.
    fn show_sheet(&mut self, name: String, mut sheet: Parked) {
        let Some(doc) = &mut self.doc else { return };
        // Statistics still being computed are computed again on return.
        let complete = self.profiles.len() >= doc.df().width();
        // After these swaps `sheet` is the one that leaves the screen.
        let old = doc.swap_sheet(name, &mut sheet);
        let view = std::mem::replace(&mut sheet.grid, self.grid.take_view());
        sheet.chart = Some(self.chart.swap(sheet.chart.take()));
        std::mem::swap(&mut self.profiles, &mut sheet.profiles);
        std::mem::swap(&mut self.profiles_for, &mut sheet.profiles_for);
        if !complete {
            sheet.profiles_for = None;
        }
        doc.park(old, sheet);
        self.grid.restore_view(view);
        // Results on their way belong to the sheet that left.
        self.profile_gen.fetch_add(1, Ordering::Relaxed);
        self.profile_rx = None;
        // These belong to one table; history versions of two sheets can be equal.
        self.preview = None;
        self.value_filter = None;
        self.form.column.clear();
        self.form.columns.clear();
        self.form.cast_check = None;
        self.reveal_tab = true;
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
        let default_fmt = if doc.format.can_write() && doc.format != Format::Sqlite {
            doc.format
        } else if doc.is_workbook() {
            // An .xlsx holds all the sheets; any other format takes just one.
            Format::Excel
        } else {
            Format::Csv
        };
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
        match &self.job {
            // That save reports when it is done; a second one would write the same file.
            Some(Job::Save { .. }) => return,
            Some(Job::Load { .. }) => {
                self.after_save = None;
                self.error("Wait until the file has been read, then save again");
                return;
            }
            None => {}
        }
        let scope = doc.save_scope(opts.format);
        if scope == SaveScope::Sheet && same_file(&path, &doc.path) {
            self.after_save = None;
            let msg = format!(
                "That would replace all {} {}s of {} with this one. Choose another file name.",
                doc.tables.len(),
                doc.noun(),
                doc.name()
            );
            self.error(msg);
            return;
        }
        let (tx, rx) = channel();
        let ctx2 = ctx.clone();
        let p2 = path.clone();
        let save = if scope == SaveScope::Workbook {
            let (sheets, written) = doc.workbook_sheets();
            let names = sheets.iter().map(|s| s.name.clone()).collect();
            let source = doc.path.clone();
            std::thread::spawn(move || {
                let _ = tx.send(io::save_workbook(&sheets, &source, &p2));
                ctx2.request_repaint();
            });
            SaveJob { path, opts, scope, written, names }
        } else {
            let df = doc.df().clone();
            let o2 = opts.clone();
            std::thread::spawn(move || {
                let _ = tx.send(io::save(&df, &p2, &o2));
                ctx2.request_repaint();
            });
            let written = vec![(doc.table.clone(), doc.history.version())];
            SaveJob { path, opts, scope, written, names: Vec::new() }
        };
        self.job = Some(Job::Save { rx, save });
    }

    fn finish_save(&mut self, save: SaveJob) {
        let path = save.path.clone();
        self.settings.recent.retain(|p| p != &path);
        self.settings.recent.insert(0, path);
        self.settings.recent.truncate(MAX_RECENT);
        // The file may have been closed while it was written.
        if let Some(doc) = &mut self.doc {
            let msg = doc.finish_save(save);
            self.info(msg);
        }
    }

    /// "Save" in the unsaved-changes prompt: saves, then goes on with `after`.
    fn save_then(&mut self, after: After, ctx: &egui::Context) {
        // A save writes the sheet on screen, or the whole workbook. So go to
        // a sheet that still has unsaved changes if this one has none.
        if let Some(doc) = &self.doc {
            if !doc.history.is_dirty() {
                if let Some(name) = doc.dirty_sheets().first().cloned() {
                    self.switch_sheet(&name, ctx);
                }
            }
        }
        self.after_save = Some(after);
        self.save(ctx);
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
            Ok(df) if matches!(op, Op::Filter { .. } | Op::FilterValues { .. }) && df.height() == before.0 => {
                self.info(format!("Every row already matches, nothing to filter ({})", op.describe()));
            }
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
            ColumnCommand::ValueFilter(pos) => self.open_value_filter(name, pos),
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

    // ---- filter by values -------------------------------------------------

    fn open_value_filter(&mut self, column: String, pos: egui::Pos2) {
        let Some(doc) = &self.doc else { return };
        let last = match doc.history.last_op() {
            Some(op @ Op::FilterValues { column: c, .. }) if *c == column => Some(op.clone()),
            _ => None,
        };
        let df = if last.is_some() { doc.history.before_last() } else { doc.df() };
        let numeric = df.column(&column).is_ok_and(|c| c.dtype().is_primitive_numeric());
        let items = match stats::distinct_values(df, &column) {
            Ok(items) => items,
            Err(e) => {
                self.error(format!("Can't list the values of {column}: {e:#}"));
                return;
            }
        };
        let checked = match &last {
            Some(Op::FilterValues { values, exclude, .. }) => {
                let set: HashSet<&Option<String>> = values.iter().collect();
                items.iter().map(|(v, _)| set.contains(v) != *exclude).collect()
            }
            _ => vec![true; items.len()],
        };
        self.value_filter = Some(ValueFilter {
            column,
            items,
            checked,
            search: String::new(),
            shown: Vec::new(),
            shown_for: None,
            replaces: last.is_some(),
            pos,
            version: doc.history.version(),
            first_frame: true,
            numeric,
        });
    }

    fn value_filter_popup(&mut self, ctx: &egui::Context) {
        let Some(vf) = &mut self.value_filter else { return };
        let current = self.doc.as_ref().map(|d| d.history.version());
        if current != Some(vf.version) || self.preview.is_some() {
            self.value_filter = None;
            return;
        }
        if vf.shown_for.as_deref() != Some(vf.search.as_str()) {
            let needle = vf.search.trim().to_lowercase();
            vf.shown = (0..vf.items.len())
                .filter(|&i| needle.is_empty() || value_label(&vf.items[i].0).to_lowercase().contains(&needle))
                .collect();
            vf.shown_for = Some(vf.search.clone());
        }
        let searching = !vf.search.trim().is_empty();
        let first = std::mem::replace(&mut vf.first_frame, false);
        let mut action = None;
        let area = egui::Area::new(Id::unique("value-filter"))
            .order(egui::Order::Foreground)
            .fixed_pos(vf.pos)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(300.0);
                    ui.label(RichText::new(format!("Filter {}", vf.column)).strong());
                    let (asc, desc) = if vf.numeric {
                        ("Sort smallest to largest", "Sort largest to smallest")
                    } else {
                        ("Sort A to Z", "Sort Z to A")
                    };
                    if ui.add(egui::Button::new(asc).frame(false)).clicked() {
                        action = Some(ValueFilterAction::Sort(false));
                    }
                    if ui.add(egui::Button::new(desc).frame(false)).clicked() {
                        action = Some(ValueFilterAction::Sort(true));
                    }
                    if vf.replaces
                        && ui.add(egui::Button::new(format!("Clear filter from {}", vf.column)).frame(false)).clicked()
                    {
                        action = Some(ValueFilterAction::Clear);
                    }
                    if ui.add(egui::Button::new("Filter by condition…").frame(false)).clicked() {
                        action = Some(ValueFilterAction::Condition);
                    }
                    ui.separator();
                    let search = ui.add(
                        egui::TextEdit::singleline(&mut vf.search).hint_text("Search").desired_width(f32::INFINITY),
                    );
                    if first {
                        search.request_focus();
                    }
                    let on = vf.shown.iter().filter(|&&i| vf.checked[i]).count();
                    let mut all = on == vf.shown.len() && on > 0;
                    let label = if searching { "(Select all search results)" } else { "(Select all)" };
                    let indeterminate = on > 0 && on < vf.shown.len();
                    if ui.add(egui::Checkbox::new(&mut all, label).indeterminate(indeterminate)).changed() {
                        for &i in &vf.shown {
                            vf.checked[i] = all;
                        }
                    }
                    let row_h = ui.spacing().interact_size.y;
                    ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        egui::ScrollArea::vertical().max_height(300.0).auto_shrink([false, true]).show_rows(
                            ui,
                            row_h,
                            vf.shown.len(),
                            |ui, range| {
                                for &i in &vf.shown[range] {
                                    ui.horizontal(|ui| {
                                        ui.set_height(row_h);
                                        ui.add(egui::Checkbox::without_text(&mut vf.checked[i]));
                                        let (v, n) = &vf.items[i];
                                        let text = match v {
                                            Some(s) if !s.is_empty() => RichText::new(s.replace(['\n', '\r'], " ")),
                                            _ => RichText::new(value_label(v)).italics().weak(),
                                        };
                                        let w = (ui.available_width() - 70.0).max(40.0);
                                        let clicked = ui
                                            .allocate_ui_with_layout(
                                                egui::vec2(w, row_h),
                                                egui::Layout::left_to_right(egui::Align::Center),
                                                |ui| {
                                                    ui.set_max_width(w);
                                                    ui.add(egui::Label::new(text).truncate().sense(egui::Sense::click()))
                                                        .clicked()
                                                },
                                            )
                                            .inner;
                                        if clicked {
                                            vf.checked[i] = !vf.checked[i];
                                        }
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.add_space(10.0); // clear of the scroll bar
                                            ui.label(RichText::new(fmt_int(*n as usize)).weak().size(11.5));
                                        });
                                    });
                                }
                            },
                        );
                    });
                    if vf.shown.is_empty() {
                        ui.label(RichText::new("No matching values").weak());
                    }
                    ui.separator();
                    let considered: Vec<usize> =
                        if searching { vf.shown.clone() } else { (0..vf.items.len()).collect() };
                    let keep = considered.iter().filter(|&&i| vf.checked[i]).count();
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{} of {} values", fmt_int(keep), fmt_int(vf.items.len())))
                                .weak()
                                .size(12.0),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Cancel").clicked() {
                                action = Some(ValueFilterAction::Cancel);
                            }
                            if ui.add_enabled(keep > 0, egui::Button::new(RichText::new("  OK  ").strong())).clicked() {
                                action = Some(ValueFilterAction::Ok);
                            }
                        });
                    });
                    if searching {
                        ui.label(RichText::new("OK keeps only the ticked values that match the search.").weak().size(11.5));
                    }
                    if keep > 0 && ui.input(|i| i.key_pressed(Key::Enter)) {
                        action = Some(ValueFilterAction::Ok);
                    }
                });
            });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            action = Some(ValueFilterAction::Cancel);
        }
        // A click outside closes the list without filtering, as in Excel.
        if !first && action.is_none() {
            let outside = ctx.input(|i| {
                i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !area.response.rect.contains(p))
            });
            if outside {
                action = Some(ValueFilterAction::Cancel);
            }
        }
        let Some(action) = action else { return };
        let Some(vf) = self.value_filter.take() else { return };
        match action {
            ValueFilterAction::Cancel => {}
            ValueFilterAction::Ok => self.apply_value_filter(vf),
            ValueFilterAction::Clear => {
                if let Some(doc) = &mut self.doc {
                    doc.history.undo();
                }
                self.info(format!("Cleared the filter on {}", vf.column));
            }
            ValueFilterAction::Sort(desc) => self.apply(Op::Sort { columns: vec![(vf.column, desc)] }),
            ValueFilterAction::Condition => {
                self.form.column = vf.column;
                self.form.kind = OpKind::Filter;
                self.settings.show_ops = true;
            }
        }
    }

    fn apply_value_filter(&mut self, vf: ValueFilter) {
        let searching = !vf.search.trim().is_empty();
        let n = vf.items.len();
        let keep: Vec<bool> = if searching {
            let shown: HashSet<usize> = vf.shown.iter().copied().collect();
            (0..n).map(|i| vf.checked[i] && shown.contains(&i)).collect()
        } else {
            vf.checked.clone()
        };
        let kept = keep.iter().filter(|k| **k).count();
        if kept == 0 {
            return;
        }
        if vf.replaces {
            if let Some(doc) = &mut self.doc {
                doc.history.undo();
            }
        }
        if kept == n {
            if vf.replaces {
                self.info(format!("Cleared the filter on {}", vf.column));
            }
            return;
        }
        // Store whichever list is shorter.
        let exclude = kept * 2 > n;
        let values =
            vf.items.into_iter().zip(keep).filter(|(_, k)| *k != exclude).map(|((v, _), _)| v).collect();
        self.apply(Op::FilterValues { column: vf.column, values, exclude });
    }

    /// Columns with a filter among the applied steps (for the header buttons).
    fn filtered_columns(&self) -> HashSet<String> {
        let Some(doc) = &self.doc else { return HashSet::new() };
        doc.history.steps()[..doc.history.cursor()]
            .iter()
            .filter_map(|s| match &s.op {
                Op::Filter { column, .. } | Op::FilterValues { column, .. } => Some(column.clone()),
                _ => None,
            })
            .collect()
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
            Job::Load { rx, path, started, kind, serial, table } => match rx.try_recv() {
                Ok(Ok(loaded)) => self.finish_load(loaded, started, kind, serial),
                Ok(Err(e)) => self.error(format!("Could not open {}: {e:#}", path.display())),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.job = Some(Job::Load { rx, path, started, kind, serial, table })
                }
                Err(_) => self.error("Loading failed unexpectedly"),
            },
            Job::Save { rx, save } => match rx.try_recv() {
                Ok(Ok(())) => {
                    self.finish_save(save);
                    if let Some(a) = self.after_save.take() {
                        // Another sheet may still have changes (a save to a
                        // file that holds one table), or the table changed
                        // while it was written: ask again instead of going on.
                        if self.unsaved_before(&a) {
                            self.dialog = Some(Dialog::Discard(a));
                        } else {
                            self.continue_after(a, ctx);
                        }
                    }
                }
                Ok(Err(e)) => {
                    self.after_save = None;
                    self.error(format!("Could not save {}: {e:#}", save.path.display()))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => self.job = Some(Job::Save { rx, save }),
                Err(_) => {
                    self.after_save = None;
                    self.error("Saving failed unexpectedly")
                }
            },
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match &self.doc {
            Some(d) => format!(
                "{}{} — DataVivid",
                if d.is_dirty() { "● " } else { "" },
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
        // Only a text field keeps Ctrl+Z etc. for itself; the grid has
        // keyboard focus most of the time and must not block undo.
        let typing = ctx.text_edit_focused();
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
            // Next and previous sheet, as in Excel. Taken here, before the
            // grid reads Page Down as "one page down".
            let next = ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::COMMAND, Key::PageDown)));
            let prev = ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::COMMAND, Key::PageUp)));
            if next != prev && self.dialog.is_none() {
                self.step_sheet(if next { 1 } else { -1 }, ctx);
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
            if self.unsaved_before(&After::Quit) {
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
            if self.settings.show_chart {
                egui::Panel::bottom("chart")
                    .resizable(true)
                    .default_size(300.0)
                    .size_range(140.0..=900.0)
                    .show(ui, |ui| self.chart_panel(ui));
            }
            // Directly under the grid, where Excel has its sheet tabs.
            if self.doc.as_ref().is_some_and(|d| d.tables.len() > 1) {
                egui::Panel::bottom("sheets").show(ui, |ui| self.sheet_tabs(ui, &ctx));
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(0.0))
            .show(ui, |ui| self.central(ui, &ctx));

        self.value_filter_popup(&ctx);
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
                    if self.unsaved_before(&After::Close) {
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
                ui.checkbox(&mut self.settings.show_chart, "Chart panel");
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
        });
    }

    /// The sheets of a workbook (or tables of a database) as tabs under the
    /// grid. A dot marks a sheet with unsaved changes.
    fn sheet_tabs(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        let reading = match &self.job {
            Some(Job::Load { kind: LoadKind::Sheet, table, .. }) => table.clone(),
            _ => None,
        };
        let reveal = std::mem::take(&mut self.reveal_tab);
        let label = |t: &String| {
            let dirty = doc.sheet(Some(t.as_str())).is_some_and(|h| h.is_dirty());
            if dirty { format!("● {t}") } else { t.clone() }
        };
        let mut go = None;
        ui.horizontal(|ui| {
            ui.menu_button(format!("{} {}s", doc.tables.len(), doc.noun()), |ui| {
                egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                    for t in &doc.tables {
                        if ui.selectable_label(doc.table.as_ref() == Some(t), label(t)).clicked() {
                            go = Some(t.clone());
                            ui.close();
                        }
                    }
                });
            })
            .response
            .on_hover_text(format!(
                "List of all {}s. Ctrl+Page Down and Ctrl+Page Up go to the next and the previous one.",
                doc.noun()
            ));
            ui.separator();
            egui::ScrollArea::horizontal().auto_shrink([false, true]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    for t in &doc.tables {
                        let active = doc.table.as_ref() == Some(t);
                        if reading.as_ref() == Some(t) {
                            ui.add(egui::Spinner::new().size(12.0));
                        }
                        let r = ui.selectable_label(active, label(t));
                        if active && reveal {
                            r.scroll_to_me(None);
                        }
                        if r.clicked() {
                            go = Some(t.clone());
                        }
                    }
                });
            });
        });
        if let Some(name) = go {
            self.switch_sheet(&name, ctx);
        }
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
            After::Load(req, kind) => self.start_load(req, kind, ctx),
            After::Close => self.close_doc(),
            After::Quit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn close_doc(&mut self) {
        self.doc = None;
        self.chart.reset();
        self.preview = None;
        self.profiles.clear();
        self.profiles_for = None;
        self.grid.reset();
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if let Some(Job::Load { path, started, kind, table, .. }) = &self.job {
                ui.spinner();
                let what = match (kind, table) {
                    (LoadKind::Open, _) | (_, None) => path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                    (_, Some(t)) => format!("'{t}'"),
                };
                ui.label(format!("Opening {what} … {:.0} s", started.elapsed().as_secs_f32()));
            } else if let Some(Job::Save { save, .. }) = &self.job {
                ui.spinner();
                ui.label(format!("Saving {} …", save.path.file_name().unwrap_or_default().to_string_lossy()));
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
                    if doc.is_dirty() {
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

        let filtered = self.filtered_columns();
        let actions = {
            let doc = self.doc.as_ref().unwrap();
            let df = self.preview.as_ref().map_or(doc.df(), |p| &p.df);
            grid::show(ui, df, &self.profiles, &filtered, &mut self.grid)
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
        let mut action = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(4.0);
            let col = self.grid.cursor.and_then(|c| df.get_column_names().get(c.col).map(|s| s.to_string()));
            match col {
                Some(name) => {
                    let profile = self.profiles.get(&name);
                    ui.horizontal(|ui| {
                        if let Some(p) = profile {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if copy_button(ui) {
                                    action = Some(PanelAction::Copy(profile_text(p), "column statistics".into()));
                                }
                                ui.add(egui::Label::new(RichText::new(&name).heading()).truncate());
                            });
                        } else {
                            ui.heading(&name);
                        }
                    });
                    match profile {
                        Some(p) => {
                            if let Some(a) = profile_ui(ui, p) {
                                action = Some(a);
                            }
                        }
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Computing statistics…");
                            });
                        }
                    }
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("Right-click a value to copy it, or the column header for column actions.")
                            .weak()
                            .size(12.0),
                    );
                }
                None => {
                    let rows = dataset_rows(df, &doc.path);
                    let all = dataset_text(df, &rows);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if copy_button(ui) {
                                action = Some(PanelAction::Copy(all.clone(), "dataset summary".into()));
                            }
                            ui.heading("Dataset");
                        });
                    });
                    egui::Grid::new("ds").num_columns(2).striped(true).show(ui, |ui| {
                        for r in &rows {
                            if let Some(a) = stat_row(ui, r, &all, "dataset summary") {
                                action = Some(a);
                            }
                        }
                    });
                    ui.add_space(10.0);
                    ui.label(RichText::new("Columns").strong());
                    egui::Grid::new("cols").num_columns(3).striped(true).show(ui, |ui| {
                        for (i, c) in df.columns().iter().enumerate() {
                            let r = ui.link(c.name().as_str());
                            if r.clicked() {
                                self.grid.select_column(i);
                            }
                            let dtype = stats::dtype_label(c.dtype());
                            let m = c.null_count();
                            let pct = format!("{:.1}% missing", m as f64 * 100.0 / df.height().max(1) as f64);
                            r.context_menu(|ui| {
                                if ui.button("Copy name").clicked() {
                                    action = Some(PanelAction::Copy(c.name().to_string(), "column name".into()));
                                }
                                if ui.button("Copy all column names").clicked() {
                                    let names: Vec<&str> = df.get_column_names().iter().map(|s| s.as_str()).collect();
                                    action = Some(PanelAction::Copy(names.join("\n"), "column names".into()));
                                }
                                if ui.button("Copy dataset summary").clicked() {
                                    action = Some(PanelAction::Copy(all.clone(), "dataset summary".into()));
                                }
                            });
                            ui.label(RichText::new(dtype).weak());
                            ui.label(if m > 0 { RichText::new(pct).weak() } else { RichText::new("") });
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                    ui.label(RichText::new("Click a cell or column header to see column statistics.").weak().size(12.0));
                }
            }
        });
        match action {
            Some(PanelAction::Copy(text, what)) => {
                ui.ctx().copy_text(text);
                self.info(format!("Copied {what}"));
            }
            Some(PanelAction::Keep(column, value)) => {
                if self.preview.is_some() {
                    self.error("Apply or discard the preview first");
                } else {
                    self.apply(Op::FilterValues { column, values: vec![Some(value)], exclude: false });
                }
            }
            None => {}
        }
    }

    // ---- chart panel ------------------------------------------------------

    fn chart_panel(&mut self, ui: &mut Ui) {
        let Some(doc) = &self.doc else { return };
        let df = doc.df().clone();
        let version = doc.history.version();
        let table = doc.id;
        let names: Vec<String> = df.get_column_names().iter().map(|s| s.to_string()).collect();
        let sel = if self.grid.whole_rows {
            None
        } else {
            self.grid.selection(df.height().max(1), df.width()).map(|(_, _, c0, c1)| (c0, c1))
        };
        self.chart.follow_selection(&names, sel, version);
        self.chart.update(&df, table, version, ui.ctx());
        match self.chart.ui(ui, &df, self.preview.is_some()) {
            Some(chart::Action::Apply(op)) => {
                if self.preview.is_some() {
                    self.error("Apply or discard the preview first");
                } else {
                    self.apply(op);
                }
            }
            Some(chart::Action::Copied(m)) => self.info(m),
            Some(chart::Action::Hide) => self.settings.show_chart = false,
            None => {}
        }
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

    /// The text of the unsaved-changes prompt, and whether "discard" covers
    /// more than one sheet.
    fn unsaved_text(&self, after: &After) -> (String, bool) {
        let single = ("The current file has changes that are not saved.".to_string(), false);
        let Some(doc) = &self.doc else { return single };
        if doc.tables.len() <= 1 {
            return single;
        }
        let noun = doc.noun();
        if matches!(after, After::Load(_, LoadKind::Reimport)) {
            let name = doc.table.clone().unwrap_or_default();
            return (format!("The {noun} '{name}' has changes that are not saved. Reading it again discards them."), false);
        }
        let dirty = doc.dirty_sheets();
        match dirty.as_slice() {
            [one] => (format!("The {noun} '{one}' has changes that are not saved."), false),
            many => {
                let mut list = many.iter().take(8).map(|n| format!("'{n}'")).collect::<Vec<_>>().join(", ");
                if many.len() > 8 {
                    list.push_str(&format!(" and {} more", many.len() - 8));
                }
                (format!("{} {noun}s have changes that are not saved: {list}.", many.len()), true)
            }
        }
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
                            ("Ctrl+Page Down / Page Up", "Next / previous sheet"),
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
                let (text, several) = self.unsaved_text(&after);
                let r = egui::Modal::new(Id::unique("discard")).show(ctx, |ui| {
                    ui.set_width(400.0);
                    ui.heading("Unsaved changes");
                    ui.label(text);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Save").strong()).clicked() {
                            choice = Some(0);
                        }
                        if ui.button(if several { "Discard all changes" } else { "Discard changes" }).clicked() {
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
                        // Save first; `after` follows when the file is written.
                        let a = after.clone();
                        then = Some(Box::new(move |app, ctx| app.save_then(a, ctx)));
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
                    ui.label(if sheets > 1 {
                        format!(
                            "DataVivid writes plain values. Overwriting keeps all {sheets} sheets with their \
                             values, but formatting, formulas, charts and other workbook features are lost."
                        )
                    } else {
                        "DataVivid writes plain values. Overwriting keeps the values, but formatting, formulas, \
                         charts and other workbook features are lost."
                            .to_string()
                    });
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
                        // In a workbook only this sheet is read again; the
                        // others keep their changes.
                        let several = self.doc.as_ref().is_some_and(|d| d.tables.len() > 1);
                        let kind = if several { LoadKind::Reimport } else { LoadKind::Open };
                        then = Some(Box::new(move |app, ctx| app.request_load(r, kind, ctx)));
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

/// Whether two paths name the same file (also through a link or `..`).
fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

// ---------------------------------------------------------------------------
// Summary panel pieces
// ---------------------------------------------------------------------------

enum PanelAction {
    /// Text for the clipboard and what it is, for the status bar.
    Copy(String, String),
    /// Keep only the rows where a column has this value.
    Keep(String, String),
}

/// One line of the statistics table: what is shown and what is copied
/// (numbers without thousands separators, so spreadsheets read them).
struct StatRow {
    label: String,
    shown: String,
    /// One or more cells (e.g. count and percent).
    cells: Vec<String>,
}

impl StatRow {
    fn new(label: &str, shown: String, cells: &[&str]) -> Self {
        StatRow { label: label.into(), shown, cells: cells.iter().map(|c| c.to_string()).collect() }
    }
    fn same(label: &str, text: String) -> Self {
        StatRow { label: label.into(), cells: vec![text.clone()], shown: text }
    }
    /// The cells as clipboard text: a single value as is, several as
    /// tab-separated cells.
    fn value(&self) -> String {
        match self.cells.as_slice() {
            [one] => one.clone(),
            cells => tsv_row(cells),
        }
    }
    /// Label and cells as one tab-separated line.
    fn line(&self) -> String {
        let mut cells = vec![self.label.clone()];
        cells.extend(self.cells.iter().cloned());
        tsv_row(&cells)
    }
}

fn tsv_row(cells: &[String]) -> String {
    cells.iter().map(|c| grid::tsv_field(c)).collect::<Vec<_>>().join("\t")
}

/// Label for a distinct value in lists.
fn value_label(v: &Option<String>) -> String {
    match v {
        None => "(missing)".into(),
        Some(s) if s.is_empty() => "(empty)".into(),
        Some(s) => s.clone(),
    }
}

fn copy_button(ui: &mut Ui) -> bool {
    ui.small_button("Copy")
        .on_hover_text("Copy everything shown here as a table (pastes into Excel). Right-click a value to copy just that.")
        .clicked()
}

fn stat_row(ui: &mut Ui, r: &StatRow, all: &str, what: &str) -> Option<PanelAction> {
    let mut action = None;
    let k = ui.label(&r.label);
    let v = ui.label(&r.shown);
    for resp in [k, v] {
        resp.context_menu(|ui| {
            if ui.button("Copy value").clicked() {
                action = Some(PanelAction::Copy(r.value(), r.label.to_lowercase()));
            }
            if ui.button("Copy row").clicked() {
                action = Some(PanelAction::Copy(r.line(), format!("row \"{}\"", r.label)));
            }
            if ui.button("Copy all").clicked() {
                action = Some(PanelAction::Copy(all.to_string(), what.to_string()));
            }
        });
    }
    ui.end_row();
    action
}

fn profile_rows(p: &ColumnProfile) -> Vec<StatRow> {
    let int = |label: &str, n: usize| StatRow::new(label, fmt_int(n), &[&n.to_string()]);
    let mut rows = vec![StatRow::same("Type", p.dtype.clone()), int("Rows", p.rows)];
    rows.push(StatRow::new(
        "Missing",
        format!("{} ({:.1}%)", fmt_int(p.missing), p.missing_pct()),
        &[&p.missing.to_string(), &format!("{:.1}%", p.missing_pct())],
    ));
    rows.push(int("Distinct", p.unique));
    if let Some(n) = &p.numeric {
        for (k, v) in [
            ("Min", n.min),
            ("25%", n.q25),
            ("Median", n.median),
            ("Mean", n.mean),
            ("75%", n.q75),
            ("Max", n.max),
            ("Std. dev.", n.std),
        ] {
            rows.push(StatRow::same(k, fmt_num(v)));
        }
        let mut counts = vec![("Zeros", n.zeros), ("Negative", n.negatives)];
        if n.nan > 0 {
            counts.push(("NaN", n.nan));
        }
        for (k, v) in counts {
            rows.push(int(k, v));
        }
    }
    if let Some((a, b)) = &p.min_max {
        rows.push(StatRow::same("First (sorted)", a.clone()));
        rows.push(StatRow::same("Last (sorted)", b.clone()));
    }
    if let Some((a, b)) = p.text_len {
        rows.push(StatRow::new("Length", format!("{a} – {b} characters"), &[&a.to_string(), &b.to_string()]));
    }
    rows
}

/// Histogram bar `i` as (from, to); `to` is `None` when each bar is one value.
fn hist_bin(n: &stats::NumericStats, i: usize) -> (f64, Option<f64>) {
    let one_per_value = n.min.fract() == 0.0 && (n.max - n.min) as usize + 1 == n.hist.len();
    if one_per_value {
        (n.min + i as f64, None)
    } else {
        let w = (n.max - n.min) / n.hist.len() as f64;
        let lo = n.min + w * i as f64;
        (lo, Some(lo + w))
    }
}

/// Everything the column summary shows, as tab-separated text.
fn profile_text(p: &ColumnProfile) -> String {
    use grid::tsv_field;
    let mut out = format!("Column\t{}\n", tsv_field(&p.name));
    for r in profile_rows(p) {
        out.push_str(&r.line());
        out.push('\n');
    }
    if !p.top.is_empty() {
        let total = (p.rows - p.missing).max(1) as f64;
        out.push_str("\nMost frequent values\tRows\tShare\n");
        for (v, n) in &p.top {
            out.push_str(&format!("{}\t{n}\t{:.1}%\n", tsv_field(v), *n as f64 * 100.0 / total));
        }
    }
    if let Some(n) = &p.numeric {
        let single = hist_bin(n, 0).1.is_none();
        out.push_str(if single { "\nValue\tRows\n" } else { "\nFrom\tTo\tRows\n" });
        for (i, h) in n.hist.iter().enumerate() {
            match hist_bin(n, i) {
                (v, None) => out.push_str(&format!("{}\t{h}\n", fmt_num(v))),
                (lo, Some(hi)) => out.push_str(&format!("{}\t{}\t{h}\n", fmt_num(lo), fmt_num(hi))),
            }
        }
    }
    out
}

fn dataset_rows(df: &DataFrame, path: &Path) -> Vec<StatRow> {
    let missing: usize = df.columns().iter().map(|c| c.null_count()).sum();
    let pct = missing as f64 * 100.0 / (df.height() * df.width()).max(1) as f64;
    let mut rows = vec![
        StatRow::new("Rows", fmt_int(df.height()), &[&df.height().to_string()]),
        StatRow::same("Columns", df.width().to_string()),
        StatRow::new(
            "Missing cells",
            format!("{} ({pct:.1}%)", fmt_int(missing)),
            &[&missing.to_string(), &format!("{pct:.1}%")],
        ),
        StatRow::same("Memory", fmt_bytes(df.estimated_size())),
    ];
    if let Ok(meta) = std::fs::metadata(path) {
        rows.push(StatRow::same("File size", fmt_bytes(meta.len() as usize)));
    }
    rows
}

fn dataset_text(df: &DataFrame, rows: &[StatRow]) -> String {
    use grid::tsv_field;
    let mut out = String::new();
    for r in rows {
        out.push_str(&r.line());
        out.push('\n');
    }
    out.push_str("\nColumn\tType\tMissing\tMissing %\n");
    for c in df.columns() {
        let m = c.null_count();
        out.push_str(&format!(
            "{}\t{}\t{m}\t{:.1}%\n",
            tsv_field(c.name()),
            stats::dtype_label(c.dtype()),
            m as f64 * 100.0 / df.height().max(1) as f64
        ));
    }
    out
}

fn profile_ui(ui: &mut Ui, p: &ColumnProfile) -> Option<PanelAction> {
    let mut action = None;
    let all = profile_text(p);
    egui::Grid::new("profile").num_columns(2).striped(true).show(ui, |ui| {
        for r in profile_rows(p) {
            if let Some(a) = stat_row(ui, &r, &all, "column statistics") {
                action = Some(a);
            }
        }
    });

    if let Some(n) = &p.numeric {
        ui.add_space(10.0);
        ui.label(RichText::new("Distribution").strong());
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 120.0), egui::Sense::click());
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
        let resp = match hovered {
            Some((i, h)) => match hist_bin(n, i) {
                (v, None) => resp.on_hover_text(format!("{}: {} rows", fmt_num(v), fmt_int(h as usize))),
                (lo, Some(hi)) => {
                    resp.on_hover_text(format!("{} – {}: {} rows", fmt_num(lo), fmt_num(hi), fmt_int(h as usize)))
                }
            },
            None => resp,
        };
        resp.context_menu(|ui| {
            if ui.button("Copy distribution").clicked() {
                let text = all.split("\n\n").last().unwrap_or_default().to_string();
                action = Some(PanelAction::Copy(text, "distribution".into()));
            }
            if ui.button("Copy all").clicked() {
                action = Some(PanelAction::Copy(all.clone(), "column statistics".into()));
            }
        });
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
        // Filtering by a listed value needs its text to match the column's
        // own text form, which holds for text, categories and booleans.
        let filterable = matches!(p.dtype.as_str(), "text" | "category" | "bool");
        for (v, n) in &p.top {
            let frac = *n as f32 / total;
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::click());
            let painter = ui.painter_at(rect);
            painter.rect_filled(
                egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac, rect.height())),
                3.0,
                accent.gamma_multiply(if resp.hovered() { 0.45 } else { 0.3 }),
            );
            let label = if v.is_empty() { "(empty)".to_string() } else { v.clone() };
            let count = format!("{} · {:.1}%", fmt_int(*n as usize), frac * 100.0);
            let count_w = 8.0 * count.len() as f32;
            painter.with_clip_rect(rect.with_max_x(rect.max.x - count_w)).text(
                rect.left_center() + egui::vec2(6.0, 0.0),
                egui::Align2::LEFT_CENTER,
                &label,
                egui::FontId::proportional(12.5),
                ui.visuals().text_color(),
            );
            painter.text(
                rect.right_center() - egui::vec2(6.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                count,
                egui::FontId::proportional(11.5),
                ui.visuals().weak_text_color(),
            );
            let resp = resp.on_hover_text(format!("{label}\n{} rows · {:.1}%\nRight-click to copy", fmt_int(*n as usize), frac * 100.0));
            resp.context_menu(|ui| {
                if ui.button("Copy value").clicked() {
                    action = Some(PanelAction::Copy(v.clone(), "value".into()));
                }
                if ui.button("Copy value and count").clicked() {
                    action = Some(PanelAction::Copy(
                        format!("{}\t{n}\t{:.1}%", grid::tsv_field(v), frac * 100.0),
                        "value and count".into(),
                    ));
                }
                if ui.button("Copy all values").clicked() {
                    let mut text = String::from("Value\tRows\tShare\n");
                    for (v, n) in &p.top {
                        text.push_str(&format!("{}\t{n}\t{:.1}%\n", grid::tsv_field(v), *n as f32 * 100.0 / total));
                    }
                    action = Some(PanelAction::Copy(text, "most frequent values".into()));
                }
                if ui.button("Copy all").clicked() {
                    action = Some(PanelAction::Copy(all.clone(), "column statistics".into()));
                }
                if filterable {
                    ui.separator();
                    if ui.button("Keep only rows with this value").clicked() {
                        action = Some(PanelAction::Keep(p.name.clone(), v.clone()));
                    }
                }
            });
        }
    }
    action
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
    // Elsewhere egui's own font stays first, and a common system font fills
    // in symbols it lacks (arrows, bullets).
    if !cfg!(windows) {
        let fallbacks = [
            "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ];
        if let Some(bytes) = fallbacks.iter().find_map(|p| std::fs::read(p).ok()) {
            fonts.font_data.insert("system-fallback".into(), Arc::new(egui::FontData::from_owned(bytes)));
            added.push("system-fallback");
        }
    }
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let Some(fam) = fonts.families.get_mut(&family) else { continue };
        for (i, n) in added.iter().enumerate() {
            if *n == "segoe-ui" && family == egui::FontFamily::Proportional {
                fam.insert(i, n.to_string());
            } else if *n != "segoe-ui" {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_copies_as_a_table() {
        let df = df!(
            "t" => [Some("a"), Some("b\tc"), Some("a"), None],
            "x" => [1i64, 2, 2, 1500],
        )
        .unwrap();
        let text = profile_text(&stats::profile(&df, "t").unwrap());
        assert!(text.starts_with("Column\tt\nType\ttext\nRows\t4\nMissing\t1\t25.0%\nDistinct\t3\n"), "{text}");
        assert!(text.contains("\nMost frequent values\tRows\tShare\na\t2\t66.7%\n\"b\tc\"\t1\t33.3%\n"), "{text}");
        let text = profile_text(&stats::profile(&df, "x").unwrap());
        assert!(text.contains("Rows\t4\n") && text.contains("Max\t1500\n"), "no thousands separators: {text}");
        assert!(text.contains("\nFrom\tTo\tRows\n1\t"), "{text}");
        let rows = dataset_rows(&df, Path::new(""));
        assert_eq!(rows[2].value(), "1\t12.5%");
        assert!(dataset_text(&df, &rows).ends_with("Column\tType\tMissing\tMissing %\nt\ttext\t1\t25.0%\nx\tint64\t0\t0.0%\n"));
    }

    // ---- sheets of a workbook ------------------------------------------------

    /// Three sheets: two tables and one that is only a title above a table.
    fn workbook(path: &Path) {
        let mut wb = rust_xlsxwriter::Workbook::new();
        for (name, rows) in [
            ("Sales", vec![("North", 10.5), ("South", 20.0), ("West", 7.25)]),
            ("Costs", vec![("Rent", 900.0), ("Power", 120.5)]),
        ] {
            let ws = wb.add_worksheet();
            ws.set_name(name).unwrap();
            ws.write_string(0, 0, "what").unwrap();
            ws.write_string(0, 1, "amount").unwrap();
            for (i, (what, amount)) in rows.iter().enumerate() {
                ws.write_string(i as u32 + 1, 0, *what).unwrap();
                ws.write_number(i as u32 + 1, 1, *amount).unwrap();
            }
        }
        let ws = wb.add_worksheet();
        ws.set_name("Notes").unwrap();
        ws.write_string(1, 1, "Report 2026").unwrap();
        ws.write_string(3, 1, "checked").unwrap();
        ws.write_boolean(3, 2, true).unwrap();
        wb.save(path).unwrap();
    }

    fn read(path: &Path, sheet: &str) -> Loaded {
        let mut req = LoadRequest::new(path).unwrap();
        req.table = Some(sheet.to_string());
        io::load(req).unwrap()
    }

    fn sheet_of(loaded: Loaded, id: u64) -> Parked {
        Parked {
            loaded_shape: loaded.df.shape(),
            history: History::new(loaded.df),
            request: loaded.request,
            load_time: Duration::ZERO,
            id,
            grid: grid::GridView::default(),
            chart: None,
            profiles: HashMap::new(),
            profiles_for: None,
        }
    }

    /// Opens a file as the app does, on one sheet.
    fn open(path: &Path, sheet: &str) -> Doc {
        let loaded = read(path, sheet);
        let (tables, table) = (loaded.tables.clone(), loaded.table.clone());
        let first = sheet_of(loaded, 1);
        Doc {
            path: path.to_path_buf(),
            format: first.request.format,
            request: first.request,
            tables,
            table,
            csv: None,
            history: first.history,
            loaded_shape: first.loaded_shape,
            load_time: first.load_time,
            id: 1,
            serial: 1,
            parked: HashMap::new(),
        }
    }

    /// Goes to another sheet as `App::switch_sheet` does: one that was open
    /// comes back from memory, another is read from the file.
    fn go(doc: &mut Doc, sheet: &str) {
        let mut next = match doc.parked.remove(sheet) {
            Some(parked) => parked,
            None => sheet_of(read(&doc.path, sheet), 99),
        };
        let old = doc.swap_sheet(sheet.to_string(), &mut next);
        doc.park(old, next);
    }

    fn edit(doc: &mut Doc, row: usize, text: &str) {
        let op = Op::SetCells { column: "what".into(), rows: vec![row], values: vec![Some(text.into())] };
        let df = op.apply(doc.df()).unwrap();
        doc.history.push(op, df);
    }

    fn cell(df: &DataFrame, row: usize) -> String {
        df.column("what").unwrap().str().unwrap().get(row).unwrap().to_string()
    }

    /// Saves as the app does, without the background thread.
    fn save(doc: &mut Doc, path: &Path, format: Format) -> String {
        let scope = doc.save_scope(format);
        let opts = SaveOptions::for_format(format);
        let job = if scope == SaveScope::Workbook {
            let (sheets, written) = doc.workbook_sheets();
            let names = sheets.iter().map(|s| s.name.clone()).collect();
            io::save_workbook(&sheets, &doc.path, path).unwrap();
            SaveJob { path: path.to_path_buf(), opts, scope, written, names }
        } else {
            io::save(doc.df(), path, &opts).unwrap();
            let written = vec![(doc.table.clone(), doc.history.version())];
            SaveJob { path: path.to_path_buf(), opts, scope, written, names: Vec::new() }
        };
        doc.finish_save(job)
    }

    #[test]
    fn a_change_survives_a_switch_and_is_asked_about_from_any_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        workbook(&path);
        let on_disk = std::fs::read(&path).unwrap();

        let mut doc = open(&path, "Sales");
        assert_eq!(doc.tables, ["Sales", "Costs", "Notes"]);
        assert!(!doc.is_dirty());
        edit(&mut doc, 0, "Nord");
        go(&mut doc, "Costs");

        // On the second sheet: nothing changed here, but the file has unsaved work.
        assert_eq!(doc.table.as_deref(), Some("Costs"));
        assert!(!doc.history.is_dirty());
        assert!(doc.is_dirty());
        assert_eq!(doc.dirty_sheets(), ["Sales"]);
        // Switching wrote nothing, so "discard" at exit really discards it.
        assert_eq!(std::fs::read(&path).unwrap(), on_disk);

        // Back on the first sheet the change and its undo step are there.
        edit(&mut doc, 1, "Strom");
        go(&mut doc, "Sales");
        assert_eq!(cell(doc.df(), 0), "Nord");
        assert!(doc.history.can_undo());
        assert_eq!(doc.dirty_sheets(), ["Sales", "Costs"]);
        doc.history.undo();
        assert_eq!(cell(doc.df(), 0), "North");
        assert_eq!(doc.dirty_sheets(), ["Costs"]);
    }

    #[test]
    fn saving_a_workbook_writes_the_changes_of_every_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        workbook(&path);
        let mut doc = open(&path, "Sales");
        edit(&mut doc, 0, "Nord");
        go(&mut doc, "Costs");
        edit(&mut doc, 1, "Strom");

        // Only the changed sheets come from memory; "Notes" was never opened.
        let (sheets, written) = doc.workbook_sheets();
        let from_memory: Vec<&str> = sheets.iter().filter(|s| s.df.is_some()).map(|s| s.name.as_str()).collect();
        assert_eq!(from_memory, ["Sales", "Costs"]);
        assert_eq!(written.len(), 2);

        let msg = save(&mut doc, &path, Format::Excel);
        assert!(msg.ends_with("with all 3 sheets"), "{msg}");
        assert!(!doc.is_dirty());
        assert!(doc.dirty_sheets().is_empty());
        assert_eq!(cell(&read(&path, "Sales").df, 0), "Nord");
        assert_eq!(cell(&read(&path, "Costs").df, 1), "Strom");
        // The sheet nobody touched kept its cells where they were.
        use calamine::{Data, Reader};
        let mut wb = calamine::open_workbook_auto(&path).unwrap();
        let notes = wb.worksheet_range("Notes").unwrap();
        assert_eq!(notes.get_value((1, 1)), Some(&Data::String("Report 2026".into())));
        assert_eq!(notes.get_value((3, 2)), Some(&Data::Bool(true)));

        // Saved sheets are copied from the file next time, not written again.
        let (sheets, written) = doc.workbook_sheets();
        assert!(sheets.iter().all(|s| s.df.is_none()));
        assert!(written.is_empty());
    }

    #[test]
    fn save_as_moves_the_whole_workbook() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        workbook(&path);
        let on_disk = std::fs::read(&path).unwrap();
        let mut doc = open(&path, "Sales");
        edit(&mut doc, 0, "Nord");
        go(&mut doc, "Costs");

        let copy = dir.path().join("copy.xlsx");
        save(&mut doc, &copy, Format::Excel);
        assert_eq!(doc.path, copy);
        assert!(!doc.is_dirty());
        assert_eq!(std::fs::read(&path).unwrap(), on_disk);
        // A sheet opened later, and one that was parked, come from the new file.
        assert_eq!(doc.request.path, copy);
        assert_eq!(doc.parked["Sales"].request.path, copy);
        assert_eq!(read(&copy, "Notes").tables, ["Sales", "Costs", "Notes"]);
        assert_eq!(cell(&read(&copy, "Sales").df, 0), "Nord");
    }

    #[test]
    fn exporting_one_sheet_leaves_the_workbook_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        workbook(&path);
        let on_disk = std::fs::read(&path).unwrap();
        let mut doc = open(&path, "Sales");
        edit(&mut doc, 0, "Nord");
        go(&mut doc, "Costs");
        edit(&mut doc, 1, "Strom");

        // CSV holds one table: the sheet on screen.
        assert_eq!(doc.save_scope(Format::Csv), SaveScope::Sheet);
        let csv = dir.path().join("costs.csv");
        let msg = save(&mut doc, &csv, Format::Csv);
        assert!(msg.starts_with("Exported the sheet 'Costs'"), "{msg}");
        assert_eq!(doc.path, path);
        assert_eq!(doc.tables.len(), 3);
        assert_eq!(std::fs::read(&path).unwrap(), on_disk);
        // The other sheet's change is still unsaved, so leaving asks again.
        assert_eq!(doc.dirty_sheets(), ["Sales"]);

        // The workbook itself still lacks both changes; saving it writes both.
        let (_, written) = doc.workbook_sheets();
        assert_eq!(written.len(), 2);
        save(&mut doc, &path, Format::Excel);
        assert!(!doc.is_dirty());
        assert_eq!(cell(&read(&path, "Costs").df, 1), "Strom");
        assert_eq!(cell(&read(&path, "Sales").df, 0), "Nord");
    }

    #[test]
    fn a_change_made_while_saving_is_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        workbook(&path);
        let mut doc = open(&path, "Sales");
        edit(&mut doc, 0, "Nord");
        let (sheets, written) = doc.workbook_sheets();
        let names = sheets.iter().map(|s| s.name.clone()).collect();
        io::save_workbook(&sheets, &path, &path).unwrap();
        // The user undoes the change before the save is recorded.
        doc.history.undo();
        let opts = SaveOptions::for_format(Format::Excel);
        doc.finish_save(SaveJob { path: path.clone(), opts, scope: SaveScope::Workbook, written, names });
        // The file says "Nord", the screen "North": that must not pass as saved.
        assert!(doc.is_dirty());
        assert_eq!(doc.workbook_sheets().1.len(), 1);
    }

    #[test]
    fn sheets_follow_the_names_excel_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        workbook(&path);
        let mut doc = open(&path, "Sales");
        go(&mut doc, "Costs");
        // As if the names came from an .ods, where Excel's rules don't apply.
        doc.rename_sheets(vec!["Sales 2026".into(), "Costs".into(), "Notes".into()]);
        assert_eq!(doc.tables, ["Sales 2026", "Costs", "Notes"]);
        assert_eq!(doc.table.as_deref(), Some("Costs"));
        assert!(doc.parked.contains_key("Sales 2026"));
        assert_eq!(doc.parked["Sales 2026"].request.table.as_deref(), Some("Sales 2026"));
    }

    #[test]
    fn a_single_table_saved_elsewhere_moves_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.xlsx");
        let mut wb = rust_xlsxwriter::Workbook::new();
        let ws = wb.add_worksheet();
        ws.write_string(0, 0, "what").unwrap();
        ws.write_string(1, 0, "a").unwrap();
        wb.save(&path).unwrap();
        let mut doc = open(&path, "Sheet1");
        doc.request.csv.skip_rows = 3;
        edit(&mut doc, 0, "b");
        assert_eq!(doc.save_scope(Format::Csv), SaveScope::Table);
        let csv = dir.path().join("one.csv");
        save(&mut doc, &csv, Format::Csv);
        assert!(!doc.is_dirty());
        assert_eq!(doc.path, csv);
        // No sheet names and no skipped rows are carried over to the CSV.
        assert!(doc.tables.is_empty() && doc.table.is_none() && doc.request.table.is_none());
        assert_eq!(doc.request.csv.skip_rows, 0);
        assert_eq!(cell(&io::load(doc.request.clone()).unwrap().df, 0), "b");
    }
}
