//! The data grid: a custom-painted, virtualised table.
//!
//! Only the visible rows and columns are read and drawn, so the cost per
//! frame does not depend on the table size. Vertical scrolling is tracked in
//! rows (not pixels) because egui works in `f32`, which can't address pixel
//! offsets of tables with millions of rows exactly.

use std::collections::{HashMap, HashSet};

use egui::{
    Align2, Color32, CursorIcon, Event, EventFilter, FontId, Key, Modifiers, Pos2, Rect, Sense, Stroke, StrokeKind,
    TextEdit, Ui, Vec2, pos2, vec2,
};
use polars::prelude::*;

use crate::ops::Op;
use crate::stats::ColumnProfile;

pub const ROW_H: f32 = 22.0;
pub const HEADER_H: f32 = 60.0;
const SCROLLBAR: f32 = 12.0;
const MAX_CELL_CHARS: usize = 400;
const MAX_COPY_CELLS: usize = 2_000_000;
const MIN_COL_W: f32 = 48.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub row: usize,
    pub col: usize,
}

pub struct Editing {
    pub cell: Cell,
    pub text: String,
    pub original: Option<String>,
    focus: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum ContextTarget {
    Header(usize),
    Row(usize),
    Cell(Cell),
}

#[derive(Clone, Debug)]
pub enum ColumnCommand {
    SortAsc,
    SortDesc,
    Filter,
    FilterValue(Option<String>),
    /// Opens the filter-by-values list at this screen position.
    ValueFilter(Pos2),
    Rename,
    ChangeType,
    FillMissing,
    FindReplace,
    Delete,
    InsertLeft,
    InsertRight,
    DropDuplicates,
    DropMissing,
    MoveLeft,
    MoveRight,
}

pub enum GridAction {
    Apply(Op),
    Column(usize, ColumnCommand),
    Status(String),
}

#[derive(Default)]
pub struct GridState {
    pub widths: HashMap<String, f32>,
    pub top_row: f64,
    pub scroll_x: f32,
    pub anchor: Option<Cell>,
    pub cursor: Option<Cell>,
    pub whole_cols: bool,
    pub whole_rows: bool,
    pub editing: Option<Editing>,
    context: Option<ContextTarget>,
    context_pos: Pos2,
    resizing: Option<(usize, f32, f32)>,
    drag_vbar: Option<f32>,
    drag_hbar: Option<f32>,
    selecting: bool,
    ensure_visible: bool,
    request_focus: bool,
}

/// Scroll position and selection of a sheet that is not on screen.
#[derive(Default)]
pub struct GridView {
    top_row: f64,
    scroll_x: f32,
    anchor: Option<Cell>,
    cursor: Option<Cell>,
    whole_cols: bool,
    whole_rows: bool,
}

impl GridState {
    /// Clears selection and scroll position (new file).
    pub fn reset(&mut self) {
        *self = GridState { widths: std::mem::take(&mut self.widths), ..Default::default() };
    }

    /// Takes the scroll position and selection out, for a sheet that goes
    /// off screen, and leaves the grid as for a new table.
    pub fn take_view(&mut self) -> GridView {
        let view = GridView {
            top_row: self.top_row,
            scroll_x: self.scroll_x,
            anchor: self.anchor,
            cursor: self.cursor,
            whole_cols: self.whole_cols,
            whole_rows: self.whole_rows,
        };
        self.reset();
        view
    }

    /// Puts a sheet back where it was left, with the keyboard in the grid.
    pub fn restore_view(&mut self, view: GridView) {
        self.reset();
        self.top_row = view.top_row;
        self.scroll_x = view.scroll_x;
        self.anchor = view.anchor;
        self.cursor = view.cursor;
        self.whole_cols = view.whole_cols;
        self.whole_rows = view.whole_rows;
        self.request_focus = true;
    }

    /// Keeps selection inside a table that changed shape.
    pub fn clamp(&mut self, nrows: usize, ncols: usize) {
        let fix = |c: &mut Option<Cell>| {
            if let Some(cell) = c {
                if nrows == 0 || ncols == 0 {
                    *c = None;
                } else {
                    cell.row = cell.row.min(nrows - 1);
                    cell.col = cell.col.min(ncols - 1);
                }
            }
        };
        fix(&mut self.anchor);
        fix(&mut self.cursor);
        if let Some(ed) = &self.editing {
            if ed.cell.row >= nrows || ed.cell.col >= ncols {
                self.editing = None;
            }
        }
    }

    /// Selected range as (row0, row1, col0, col1), inclusive.
    pub fn selection(&self, nrows: usize, ncols: usize) -> Option<(usize, usize, usize, usize)> {
        let (a, c) = (self.anchor?, self.cursor?);
        if nrows == 0 || ncols == 0 {
            return None;
        }
        let (mut r0, mut r1) = (a.row.min(c.row), a.row.max(c.row));
        let (mut c0, mut c1) = (a.col.min(c.col), a.col.max(c.col));
        if self.whole_cols {
            r0 = 0;
            r1 = nrows - 1;
        }
        if self.whole_rows {
            c0 = 0;
            c1 = ncols - 1;
        }
        Some((r0, r1.min(nrows - 1), c0, c1.min(ncols - 1)))
    }

    pub fn select_cell(&mut self, cell: Cell) {
        self.anchor = Some(cell);
        self.cursor = Some(cell);
        self.whole_cols = false;
        self.whole_rows = false;
        self.ensure_visible = true;
    }

    pub fn select_column(&mut self, col: usize) {
        self.anchor = Some(Cell { row: 0, col });
        self.cursor = Some(Cell { row: 0, col });
        self.whole_cols = true;
        self.whole_rows = false;
        self.ensure_visible = true;
    }

    /// Selected rows when whole rows are selected, else the cursor row.
    pub fn selected_rows(&self, nrows: usize, ncols: usize) -> Vec<usize> {
        match self.selection(nrows, ncols) {
            Some((r0, r1, _, _)) => (r0..=r1).collect(),
            None => Vec::new(),
        }
    }
}

/// Text shown for a value; `None` means missing.
pub fn cell_text(v: &AnyValue) -> Option<String> {
    match v {
        AnyValue::Null => None,
        AnyValue::String(s) => Some(truncate(s)),
        AnyValue::StringOwned(s) => Some(truncate(s.as_str())),
        AnyValue::Float64(f) => Some(fmt_f64(*f)),
        AnyValue::Float32(f) => Some(fmt_f64(*f as f64)),
        AnyValue::Boolean(b) => Some(b.to_string()),
        other => Some(truncate(&other.str_value())),
    }
}

/// Full text of a value for editing and copying (no truncation).
pub fn value_text(v: &AnyValue) -> Option<String> {
    match v {
        AnyValue::Null => None,
        AnyValue::String(s) => Some(s.to_string()),
        AnyValue::StringOwned(s) => Some(s.to_string()),
        AnyValue::Float64(f) => Some(fmt_f64(*f)),
        AnyValue::Float32(f) => Some(fmt_f64(*f as f64)),
        other => Some(other.str_value().to_string()),
    }
}

fn fmt_f64(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{f:.1}")
    } else {
        // Shortest representation that round-trips.
        format!("{f}")
    }
}

fn truncate(s: &str) -> String {
    let first_line = s.lines().next().unwrap_or("");
    let multi = first_line.len() < s.len();
    if first_line.chars().count() > MAX_CELL_CHARS {
        let mut t: String = first_line.chars().take(MAX_CELL_CHARS).collect();
        t.push('…');
        t
    } else if multi {
        format!("{first_line} ⏎…")
    } else {
        first_line.to_string()
    }
}

fn estimate_width(df: &DataFrame, ci: usize) -> f32 {
    let col = &df.columns()[ci];
    let mut chars = col.name().chars().count().max(6) as f32 + 2.0;
    let n = df.height().min(200);
    for r in 0..n {
        if let Ok(v) = col.get(r) {
            if let Some(t) = cell_text(&v) {
                chars = chars.max(t.chars().count() as f32);
            }
        }
    }
    (chars * 7.2 + 18.0).clamp(70.0, 360.0)
}

struct Layout {
    full: Rect,
    data: Rect,
    rownum_w: f32,
    xs: Vec<f32>, // left x of each column in content space (0 = first column)
    widths: Vec<f32>,
    content_w: f32,
    page_rows: usize,
}

impl Layout {
    fn col_left(&self, ci: usize, scroll_x: f32) -> f32 {
        self.data.min.x + self.xs[ci] - scroll_x
    }
    fn row_top(&self, r: usize, top_row: f64) -> f32 {
        self.data.min.y + ((r as f64 - top_row) * ROW_H as f64) as f32
    }
    fn col_at(&self, x: f32, scroll_x: f32) -> Option<usize> {
        let cx = x - self.data.min.x + scroll_x;
        if cx < 0.0 || cx >= self.content_w {
            return None;
        }
        let i = self.xs.partition_point(|&s| s <= cx);
        Some(i.saturating_sub(1))
    }
    fn row_at(&self, y: f32, top_row: f64, nrows: usize) -> Option<usize> {
        let r = top_row + ((y - self.data.min.y) / ROW_H) as f64;
        if r < 0.0 {
            return None;
        }
        let r = r.floor() as usize;
        (r < nrows).then_some(r)
    }
    fn vtrack(&self) -> Rect {
        Rect::from_min_max(pos2(self.full.max.x - SCROLLBAR, self.data.min.y), pos2(self.full.max.x, self.data.max.y))
    }
    fn htrack(&self) -> Rect {
        Rect::from_min_max(pos2(self.data.min.x, self.full.max.y - SCROLLBAR), pos2(self.data.max.x, self.full.max.y))
    }
    /// The filter button in the top right corner of a column header, if the
    /// column is wide enough for one.
    fn filter_button(&self, ci: usize, scroll_x: f32) -> Option<Rect> {
        if self.widths[ci] < 64.0 {
            return None;
        }
        let right = self.col_left(ci, scroll_x) + self.widths[ci];
        Some(Rect::from_min_size(pos2(right - FILTER_BTN - 4.0, self.full.min.y + 4.0), Vec2::splat(FILTER_BTN)))
    }
}

const FILTER_BTN: f32 = 18.0;

#[derive(Clone, Copy, Debug)]
enum Hit {
    Header { col: usize, edge: Option<usize> },
    FilterButton(usize),
    RowNum(usize),
    Cell(Cell),
    VBar,
    HBar,
    Empty,
}

fn hit(l: &Layout, st: &GridState, p: Pos2, nrows: usize) -> Hit {
    if l.vtrack().contains(p) {
        return Hit::VBar;
    }
    if l.htrack().contains(p) {
        return Hit::HBar;
    }
    if p.y < l.data.min.y {
        // Header: check column edges for resizing first.
        for ci in 0..l.xs.len() {
            let right = l.col_left(ci, st.scroll_x) + l.widths[ci];
            if (p.x - right).abs() <= 4.0 && p.x > l.data.min.x {
                return Hit::Header { col: ci, edge: Some(ci) };
            }
        }
        return match l.col_at(p.x, st.scroll_x) {
            Some(c) if p.x >= l.data.min.x && l.filter_button(c, st.scroll_x).is_some_and(|b| b.contains(p)) => {
                Hit::FilterButton(c)
            }
            Some(c) if p.x >= l.data.min.x => Hit::Header { col: c, edge: None },
            _ => Hit::Empty,
        };
    }
    let row = l.row_at(p.y, st.top_row, nrows);
    if p.x < l.data.min.x {
        return row.map_or(Hit::Empty, Hit::RowNum);
    }
    match (row, l.col_at(p.x, st.scroll_x)) {
        (Some(row), Some(col)) => Hit::Cell(Cell { row, col }),
        _ => Hit::Empty,
    }
}

/// Draws the grid and handles its input. Columns named in `filtered` get a
/// highlighted filter button.
pub fn show(
    ui: &mut Ui,
    df: &DataFrame,
    profiles: &HashMap<String, ColumnProfile>,
    filtered: &HashSet<String>,
    st: &mut GridState,
) -> Vec<GridAction> {
    let mut actions = Vec::new();
    let nrows = df.height();
    let ncols = df.width();
    let columns = df.columns();

    for (ci, c) in columns.iter().enumerate() {
        if !st.widths.contains_key(c.name().as_str()) {
            st.widths.insert(c.name().to_string(), estimate_width(df, ci));
        }
    }
    let widths: Vec<f32> = columns.iter().map(|c| st.widths[c.name().as_str()]).collect();
    let mut xs = Vec::with_capacity(ncols);
    let mut acc = 0.0;
    for w in &widths {
        xs.push(acc);
        acc += w;
    }
    let digits = nrows.max(1).to_string().len().max(2) as f32;
    let rownum_w = digits * 8.0 + 20.0;

    let full = ui.available_rect_before_wrap();
    let resp = ui.allocate_rect(full, Sense::click_and_drag());
    let data = Rect::from_min_max(
        full.min + vec2(rownum_w, HEADER_H),
        full.max - vec2(SCROLLBAR, SCROLLBAR),
    );
    let page_rows = ((data.height() / ROW_H).floor() as usize).max(1);
    let l = Layout { full, data, rownum_w, xs, widths, content_w: acc, page_rows };
    st.clamp(nrows, ncols);

    if st.request_focus {
        resp.request_focus();
        st.request_focus = false;
    }
    if resp.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                resp.id,
                EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
            )
        });
    }

    // ---- scrolling input --------------------------------------------------
    let max_top = (nrows as f64 - l.page_rows as f64).max(0.0);
    let max_sx = (l.content_w - data.width()).max(0.0);
    if ui.rect_contains_pointer(full) {
        let d = ui.input(|i| i.smooth_scroll_delta);
        if d != Vec2::ZERO {
            st.top_row -= (d.y / ROW_H) as f64;
            st.scroll_x -= d.x;
        }
    }

    // ---- pointer input ----------------------------------------------------
    let (pressed, down, released, ptr, mods, dbl) = ui.input(|i| {
        (
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
            i.pointer.primary_released(),
            i.pointer.interact_pos().or(i.pointer.hover_pos()),
            i.modifiers,
            i.pointer.button_double_clicked(egui::PointerButton::Primary),
        )
    });
    let editing_rect = st.editing.as_ref().map(|e| cell_rect(&l, st, e.cell));
    if let Some(p) = ptr {
        let in_full = full.contains(p) && editing_rect.is_none_or(|r| !r.contains(p));
        let h = hit(&l, st, p, nrows);
        if in_full {
            if let Hit::Header { edge: Some(_), .. } = h {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
            if let (Hit::FilterButton(_), true) = (h, resp.hovered()) {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                resp.clone().on_hover_text_at_pointer("Filter by values (Alt+Down)");
            }
        }
        if pressed && in_full && resp.hovered() {
            match h {
                Hit::FilterButton(col) => {
                    commit_edit(st, df, &mut actions);
                    let b = l.filter_button(col, st.scroll_x).unwrap_or(Rect::from_min_size(p, Vec2::ZERO));
                    actions.push(GridAction::Column(col, ColumnCommand::ValueFilter(b.left_bottom())));
                }
                Hit::Header { edge: Some(ci), .. } => st.resizing = Some((ci, l.widths[ci], p.x)),
                Hit::Header { col, edge: None } => {
                    if mods.shift && st.whole_cols {
                        st.cursor = Some(Cell { row: 0, col });
                    } else {
                        st.select_column(col);
                    }
                    st.selecting = false;
                    resp.request_focus();
                }
                Hit::RowNum(row) => {
                    let ncell = Cell { row, col: 0 };
                    if mods.shift && st.whole_rows {
                        st.cursor = Some(Cell { row, col: st.cursor.map_or(0, |c| c.col) });
                    } else {
                        st.anchor = Some(ncell);
                        st.cursor = Some(ncell);
                        st.whole_rows = true;
                        st.whole_cols = false;
                    }
                    resp.request_focus();
                }
                Hit::Cell(cell) => {
                    commit_edit(st, df, &mut actions);
                    if mods.shift && st.anchor.is_some() {
                        st.cursor = Some(cell);
                        st.whole_cols = false;
                        st.whole_rows = false;
                    } else {
                        st.select_cell(cell);
                    }
                    st.selecting = true;
                    resp.request_focus();
                }
                Hit::VBar => {
                    let (thumb, _) = vthumb(&l, st, nrows);
                    if thumb.contains(p) {
                        st.drag_vbar = Some(p.y - thumb.min.y);
                    } else {
                        let page = l.page_rows as f64;
                        st.top_row += if p.y < thumb.min.y { -page } else { page };
                    }
                }
                Hit::HBar => {
                    let thumb = hthumb(&l, st);
                    if thumb.contains(p) {
                        st.drag_hbar = Some(p.x - thumb.min.x);
                    } else {
                        let page = data.width() * 0.9;
                        st.scroll_x += if p.x < thumb.min.x { -page } else { page };
                    }
                }
                Hit::Empty => {}
            }
        }
        if down {
            if let Some((ci, w0, x0)) = st.resizing {
                let name = columns[ci].name().to_string();
                st.widths.insert(name, (w0 + p.x - x0).max(MIN_COL_W));
            } else if let Some(off) = st.drag_vbar {
                let track = l.vtrack();
                let (thumb, _) = vthumb(&l, st, nrows);
                let free = (track.height() - thumb.height()).max(1.0);
                let t = ((p.y - off - track.min.y) / free).clamp(0.0, 1.0);
                st.top_row = t as f64 * max_top;
            } else if let Some(off) = st.drag_hbar {
                let track = l.htrack();
                let thumb = hthumb(&l, st);
                let free = (track.width() - thumb.width()).max(1.0);
                let t = ((p.x - off - track.min.x) / free).clamp(0.0, 1.0);
                st.scroll_x = t * max_sx;
            } else if st.selecting && nrows > 0 && ncols > 0 {
                // Drag-select, auto-scrolling at the edges.
                if p.y > data.max.y {
                    st.top_row += 1.0;
                } else if p.y < data.min.y {
                    st.top_row -= 1.0;
                }
                if p.x > data.max.x {
                    st.scroll_x += 20.0;
                } else if p.x < data.min.x {
                    st.scroll_x -= 20.0;
                }
                let cp = pos2(p.x.clamp(data.min.x, data.max.x - 1.0), p.y.clamp(data.min.y, data.max.y - 1.0));
                if let Hit::Cell(cell) = hit(&l, st, cp, nrows) {
                    st.cursor = Some(cell);
                }
                ui.ctx().request_repaint();
            }
        }
    }
    // egui reports a double-click on the second release, not on the press.
    if dbl && st.editing.is_none() {
        if let Some(p) = ptr.filter(|p| l.data.contains(*p)) {
            if let Hit::Cell(cell) = hit(&l, st, p, nrows) {
                st.select_cell(cell);
                start_edit(st, df, cell, None);
            }
        }
    }
    if released || !down {
        st.resizing = None;
        st.drag_vbar = None;
        st.drag_hbar = None;
        st.selecting = false;
    }

    // Right click: remember the target and make sure it is selected.
    if resp.secondary_clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            st.context_pos = p;
            let target = match hit(&l, st, p, nrows) {
                Hit::Header { col, .. } | Hit::FilterButton(col) => {
                    if !(st.whole_cols && st.selection(nrows, ncols).is_some_and(|(_, _, c0, c1)| (c0..=c1).contains(&col))) {
                        st.select_column(col);
                    }
                    Some(ContextTarget::Header(col))
                }
                Hit::RowNum(row) => {
                    let inside = st.whole_rows && st.selection(nrows, ncols).is_some_and(|(r0, r1, _, _)| (r0..=r1).contains(&row));
                    if !inside {
                        st.anchor = Some(Cell { row, col: 0 });
                        st.cursor = Some(Cell { row, col: 0 });
                        st.whole_rows = true;
                        st.whole_cols = false;
                    }
                    Some(ContextTarget::Row(row))
                }
                Hit::Cell(cell) => {
                    let inside = st.selection(nrows, ncols).is_some_and(|(r0, r1, c0, c1)| {
                        (r0..=r1).contains(&cell.row) && (c0..=c1).contains(&cell.col)
                    });
                    if !inside {
                        st.select_cell(cell);
                    }
                    Some(ContextTarget::Cell(cell))
                }
                _ => None,
            };
            st.context = target;
        }
    }

    // ---- keyboard ---------------------------------------------------------
    if resp.has_focus() && st.editing.is_none() {
        keyboard(ui, df, st, &l, &mut actions);
    }

    st.top_row = st.top_row.clamp(0.0, max_top);
    if st.ensure_visible {
        st.ensure_visible = false;
        if let Some(c) = st.cursor {
            if !st.whole_cols {
                let r = c.row as f64;
                if r < st.top_row {
                    st.top_row = r;
                } else if r >= st.top_row + l.page_rows as f64 - 0.01 {
                    st.top_row = r + 1.0 - l.page_rows as f64;
                }
            }
            if !st.whole_rows && c.col < ncols {
                let left = l.xs[c.col];
                let right = left + l.widths[c.col];
                if left < st.scroll_x {
                    st.scroll_x = left;
                } else if right > st.scroll_x + data.width() {
                    st.scroll_x = (right - data.width()).min(left);
                }
            }
        }
    }
    st.top_row = st.top_row.clamp(0.0, max_top);
    st.scroll_x = st.scroll_x.clamp(0.0, max_sx);

    paint(ui, df, profiles, filtered, st, &l);

    // ---- inline editor ----------------------------------------------------
    if st.editing.is_some() {
        edit_overlay(ui, df, st, &l, &mut actions);
    }

    // ---- context menu -----------------------------------------------------
    resp.context_menu(|ui| context_menu(ui, df, st, &mut actions));

    actions
}

fn cell_rect(l: &Layout, st: &GridState, c: Cell) -> Rect {
    let x = l.col_left(c.col, st.scroll_x);
    let y = l.row_top(c.row, st.top_row);
    Rect::from_min_size(pos2(x, y), vec2(l.widths[c.col], ROW_H))
}

fn vthumb(l: &Layout, st: &GridState, nrows: usize) -> (Rect, bool) {
    let track = l.vtrack();
    let total = nrows.max(1) as f32;
    let frac = (l.page_rows as f32 / total).min(1.0);
    let h = (track.height() * frac).max(24.0).min(track.height());
    let max_top = (nrows as f64 - l.page_rows as f64).max(0.0);
    let t = if max_top > 0.0 { (st.top_row / max_top) as f32 } else { 0.0 };
    let y = track.min.y + (track.height() - h) * t;
    (Rect::from_min_size(pos2(track.min.x + 2.0, y), vec2(SCROLLBAR - 4.0, h)), frac < 1.0)
}

fn hthumb(l: &Layout, st: &GridState) -> Rect {
    let track = l.htrack();
    let frac = (l.data.width() / l.content_w.max(1.0)).min(1.0);
    let w = (track.width() * frac).max(24.0).min(track.width());
    let max_sx = (l.content_w - l.data.width()).max(0.0);
    let t = if max_sx > 0.0 { st.scroll_x / max_sx } else { 0.0 };
    let x = track.min.x + (track.width() - w) * t;
    Rect::from_min_size(pos2(x, track.min.y + 2.0), vec2(w, SCROLLBAR - 4.0))
}

fn paint(
    ui: &Ui,
    df: &DataFrame,
    profiles: &HashMap<String, ColumnProfile>,
    filtered: &HashSet<String>,
    st: &GridState,
    l: &Layout,
) {
    let v = ui.visuals();
    let painter = ui.painter_at(l.full);
    let nrows = df.height();
    let ncols = df.width();
    let columns = df.columns();
    let text_color = v.text_color();
    let weak = v.weak_text_color();
    let grid_line = v.widgets.noninteractive.bg_stroke.color.gamma_multiply(0.6);
    let header_bg = v.widgets.noninteractive.weak_bg_fill;
    let stripe = v.faint_bg_color;
    let sel_bg = v.selection.bg_fill.gamma_multiply(0.35);
    let sel_stroke = Stroke::new(1.5, v.selection.stroke.color);
    let accent = v.hyperlink_color;
    let font = FontId::proportional(13.0);
    let small = FontId::proportional(11.0);

    painter.rect_filled(l.full, 0.0, v.extreme_bg_color);

    let sel = st.selection(nrows, ncols);
    let in_sel = |r: usize, c: usize| sel.is_some_and(|(r0, r1, c0, c1)| r >= r0 && r <= r1 && c >= c0 && c <= c1);

    // Visible range.
    let first_row = st.top_row.floor() as usize;
    let last_row = ((st.top_row + l.page_rows as f64 + 1.0).ceil() as usize).min(nrows);
    let first_col = l.col_at(l.data.min.x, st.scroll_x).unwrap_or(0);
    let last_col = l
        .col_at(l.data.max.x - 1.0, st.scroll_x)
        .map(|c| c + 1)
        .unwrap_or(ncols)
        .min(ncols);

    // ---- cells
    let cells = painter.with_clip_rect(l.data);
    for r in first_row..last_row {
        let y = l.row_top(r, st.top_row);
        let row_rect = Rect::from_min_max(pos2(l.data.min.x, y), pos2(l.data.max.x, y + ROW_H));
        if r % 2 == 1 {
            cells.rect_filled(row_rect, 0.0, stripe);
        }
        for c in first_col..last_col {
            let x = l.col_left(c, st.scroll_x);
            let rect = Rect::from_min_size(pos2(x, y), vec2(l.widths[c], ROW_H));
            if in_sel(r, c) {
                cells.rect_filled(rect, 0.0, sel_bg);
            }
            let col = &columns[c];
            let value = col.get(r).unwrap_or(AnyValue::Null);
            let clip = cells.with_clip_rect(rect.shrink2(vec2(4.0, 0.0)).intersect(l.data));
            match cell_text(&value) {
                None => {
                    clip.text(
                        pos2(rect.min.x + 6.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        "null",
                        FontId::new(12.0, egui::FontFamily::Proportional),
                        weak.gamma_multiply(0.7),
                    );
                }
                Some(t) => {
                    let numeric = col.dtype().is_primitive_numeric();
                    let (pos, align) = if numeric {
                        (pos2(rect.max.x - 6.0, rect.center().y), Align2::RIGHT_CENTER)
                    } else {
                        (pos2(rect.min.x + 6.0, rect.center().y), Align2::LEFT_CENTER)
                    };
                    clip.text(pos, align, t, font.clone(), text_color);
                }
            }
        }
        cells.line_segment([pos2(l.data.min.x, y + ROW_H), pos2(l.data.max.x, y + ROW_H)], Stroke::new(1.0, grid_line));
    }
    // Vertical grid lines.
    for c in first_col..last_col {
        let x = l.col_left(c, st.scroll_x) + l.widths[c];
        cells.line_segment([pos2(x, l.data.min.y), pos2(x, l.data.max.y)], Stroke::new(1.0, grid_line));
    }
    // Cursor outline.
    if let (Some(cur), false, false) = (st.cursor, st.whole_cols, st.whole_rows) {
        if cur.row < nrows && cur.col < ncols {
            cells.rect_stroke(cell_rect(l, st, cur), 0.0, sel_stroke, StrokeKind::Inside);
        }
    }
    if nrows == 0 {
        painter.text(
            l.data.center_top() + vec2(0.0, 30.0),
            Align2::CENTER_CENTER,
            if ncols == 0 { "Empty table" } else { "No rows" },
            font.clone(),
            weak,
        );
    }

    // ---- header
    let header = Rect::from_min_max(pos2(l.data.min.x, l.full.min.y), pos2(l.data.max.x, l.data.min.y));
    let hp = painter.with_clip_rect(header);
    hp.rect_filled(header, 0.0, header_bg);
    for c in first_col..last_col {
        let x = l.col_left(c, st.scroll_x);
        let rect = Rect::from_min_size(pos2(x, header.min.y), vec2(l.widths[c], HEADER_H));
        let selected = sel.is_some_and(|(_, _, c0, c1)| c >= c0 && c <= c1);
        if selected {
            hp.rect_filled(rect, 0.0, sel_bg);
        }
        let inner = hp.with_clip_rect(rect.shrink2(vec2(5.0, 0.0)));
        let col = &columns[c];
        let button = l.filter_button(c, st.scroll_x);
        let name_clip = match button {
            Some(b) => Rect::from_min_max(rect.min + vec2(5.0, 0.0), pos2(b.min.x - 2.0, rect.max.y)),
            None => rect.shrink2(vec2(5.0, 0.0)),
        };
        hp.with_clip_rect(name_clip.intersect(header)).text(
            pos2(rect.min.x + 6.0, rect.min.y + 5.0),
            Align2::LEFT_TOP,
            col.name().as_str(),
            FontId::proportional(13.5),
            v.strong_text_color(),
        );
        if let Some(b) = button {
            let active = filtered.contains(col.name().as_str());
            let hovered = ui.rect_contains_pointer(b);
            if hovered || active {
                let fill = if hovered { v.widgets.hovered.weak_bg_fill } else { v.widgets.inactive.weak_bg_fill };
                hp.rect_filled(b, 3.0, fill);
            }
            hp.rect_stroke(b, 3.0, Stroke::new(1.0, grid_line), StrokeKind::Inside);
            let cx = b.center();
            if active {
                // A funnel.
                let pts = vec![
                    cx + vec2(-5.0, -4.5),
                    cx + vec2(5.0, -4.5),
                    cx + vec2(1.2, 0.5),
                    cx + vec2(1.2, 5.0),
                    cx + vec2(-1.2, 3.5),
                    cx + vec2(-1.2, 0.5),
                ];
                hp.add(egui::Shape::convex_polygon(pts, accent, Stroke::NONE));
            } else {
                let pts = vec![cx + vec2(-4.0, -2.0), cx + vec2(4.0, -2.0), cx + vec2(0.0, 2.5)];
                hp.add(egui::Shape::convex_polygon(pts, if hovered { text_color } else { weak }, Stroke::NONE));
            }
        }
        let profile = profiles.get(col.name().as_str());
        let dtype = crate::stats::dtype_label(col.dtype());
        let mut info = dtype;
        if let Some(p) = profile {
            if p.numeric.is_none() {
                info.push_str(&format!(" · {} distinct", p.unique));
            }
            if p.missing > 0 {
                info.push_str(&format!(" · {:.1}% missing", p.missing_pct()));
            }
        }
        inner.text(pos2(rect.min.x + 6.0, rect.min.y + 23.0), Align2::LEFT_TOP, info, small.clone(), weak);
        // Mini chart.
        let chart = Rect::from_min_max(pos2(rect.min.x + 6.0, rect.max.y - 18.0), pos2(rect.max.x - 6.0, rect.max.y - 4.0));
        if let Some(p) = profile {
            mini_chart(&inner, chart, p, accent, weak);
            if p.missing > 0 {
                let w = chart.width() * (p.missing as f32 / p.rows.max(1) as f32);
                inner.rect_filled(
                    Rect::from_min_size(pos2(chart.min.x, rect.max.y - 2.5), vec2(w.max(2.0), 2.0)),
                    0.0,
                    Color32::from_rgb(220, 80, 80),
                );
            }
        }
        hp.line_segment(
            [pos2(rect.max.x, header.min.y + 4.0), pos2(rect.max.x, header.max.y - 4.0)],
            Stroke::new(1.0, grid_line),
        );
    }
    hp.line_segment([pos2(header.min.x, header.max.y), pos2(header.max.x, header.max.y)], Stroke::new(1.0, grid_line));

    // ---- row numbers
    let rn = Rect::from_min_max(pos2(l.full.min.x, l.data.min.y), pos2(l.data.min.x, l.data.max.y));
    let rp = painter.with_clip_rect(rn);
    rp.rect_filled(rn, 0.0, header_bg);
    for r in first_row..last_row {
        let y = l.row_top(r, st.top_row);
        let rect = Rect::from_min_size(pos2(rn.min.x, y), vec2(l.rownum_w, ROW_H));
        if sel.is_some_and(|(r0, r1, _, _)| r >= r0 && r <= r1) {
            rp.rect_filled(rect, 0.0, sel_bg);
        }
        rp.text(
            pos2(rect.max.x - 8.0, rect.center().y),
            Align2::RIGHT_CENTER,
            (r + 1).to_string(),
            small.clone(),
            weak,
        );
    }
    rp.line_segment([pos2(rn.max.x, rn.min.y), pos2(rn.max.x, rn.max.y)], Stroke::new(1.0, grid_line));

    // ---- corner
    let corner = Rect::from_min_max(l.full.min, pos2(l.data.min.x, l.data.min.y));
    painter.rect_filled(corner, 0.0, header_bg);
    painter.text(corner.center(), Align2::CENTER_CENTER, "#", small.clone(), weak);

    // ---- scrollbars
    let bar_bg = v.extreme_bg_color;
    let thumb_c = v.widgets.inactive.bg_fill;
    painter.rect_filled(l.vtrack(), 0.0, bar_bg);
    painter.rect_filled(l.htrack(), 0.0, bar_bg);
    let (vt, vshow) = vthumb(l, st, nrows);
    if vshow {
        painter.rect_filled(vt, 4.0, thumb_c);
    }
    if l.content_w > l.data.width() {
        painter.rect_filled(hthumb(l, st), 4.0, thumb_c);
    }
}

fn mini_chart(p: &egui::Painter, rect: Rect, prof: &ColumnProfile, accent: Color32, weak: Color32) {
    if let Some(n) = &prof.numeric {
        let max = *n.hist.iter().max().unwrap_or(&1) as f32;
        let bw = rect.width() / n.hist.len() as f32;
        for (i, &h) in n.hist.iter().enumerate() {
            if h == 0 {
                continue;
            }
            let bh = (h as f32 / max.max(1.0) * rect.height()).max(1.5);
            let x = rect.min.x + i as f32 * bw;
            p.rect_filled(
                Rect::from_min_max(pos2(x + 0.5, rect.max.y - bh), pos2(x + bw - 0.5, rect.max.y)),
                1.0,
                accent.gamma_multiply(0.8),
            );
        }
    } else if !prof.top.is_empty() {
        let non_null = (prof.rows - prof.missing).max(1) as f32;
        let mut x = rect.min.x;
        // Stacked bar of the top categories, the rest in grey.
        for (i, (_, n)) in prof.top.iter().take(5).enumerate() {
            let w = rect.width() * (*n as f32 / non_null);
            let alpha = 0.95 - i as f32 * 0.15;
            p.rect_filled(
                Rect::from_min_max(pos2(x, rect.min.y + 4.0), pos2((x + w - 1.0).max(x + 1.0), rect.max.y)),
                1.0,
                accent.gamma_multiply(alpha),
            );
            x += w;
        }
        if x < rect.max.x - 1.0 {
            p.rect_filled(
                Rect::from_min_max(pos2(x, rect.min.y + 4.0), pos2(rect.max.x, rect.max.y)),
                1.0,
                weak.gamma_multiply(0.25),
            );
        }
    }
}

fn start_edit(st: &mut GridState, df: &DataFrame, cell: Cell, initial: Option<String>) {
    if cell.row >= df.height() || cell.col >= df.width() {
        return;
    }
    let col = &df.columns()[cell.col];
    if !editable(col.dtype()) {
        return;
    }
    let original = col.get(cell.row).ok().and_then(|v| value_text(&v));
    let text = initial.unwrap_or_else(|| original.clone().unwrap_or_default());
    st.editing = Some(Editing { cell, text, original, focus: true });
    st.ensure_visible = true;
}

fn editable(d: &DataType) -> bool {
    !matches!(d, DataType::List(_) | DataType::Array(_, _) | DataType::Struct(_) | DataType::Binary)
}

fn commit_edit(st: &mut GridState, df: &DataFrame, actions: &mut Vec<GridAction>) {
    let Some(ed) = st.editing.take() else { return };
    let new = if ed.text.is_empty() { None } else { Some(ed.text.clone()) };
    if new == ed.original || ed.cell.col >= df.width() {
        return;
    }
    let column = df.columns()[ed.cell.col].name().to_string();
    actions.push(GridAction::Apply(Op::SetCells { column, rows: vec![ed.cell.row], values: vec![new] }));
}

fn edit_overlay(ui: &mut Ui, df: &DataFrame, st: &mut GridState, l: &Layout, actions: &mut Vec<GridAction>) {
    let Some(cell) = st.editing.as_ref().map(|e| e.cell) else { return };
    let rect = cell_rect(l, st, cell);
    let ed = st.editing.as_mut().unwrap();
    let visible = l.data.intersects(rect);
    if !visible {
        return;
    }
    let r = ui.put(
        rect.expand2(vec2(1.0, 1.0)),
        TextEdit::singleline(&mut ed.text).margin(vec2(5.0, 2.0)).font(FontId::proportional(13.0)),
    );
    if ed.focus {
        r.request_focus();
        ed.focus = false;
    }
    if r.lost_focus() {
        let (enter, tab, esc, shift) = ui.input(|i| {
            (i.key_pressed(Key::Enter), i.key_pressed(Key::Tab), i.key_pressed(Key::Escape), i.modifiers.shift)
        });
        let cell = ed.cell;
        if esc {
            st.editing = None;
        } else {
            commit_edit(st, df, actions);
            if enter {
                let row = if shift { cell.row.saturating_sub(1) } else { (cell.row + 1).min(df.height().saturating_sub(1)) };
                st.select_cell(Cell { row, ..cell });
            } else if tab {
                let col = if shift { cell.col.saturating_sub(1) } else { (cell.col + 1).min(df.width().saturating_sub(1)) };
                st.select_cell(Cell { col, ..cell });
            }
        }
        st.request_focus = true;
    }
}

fn move_cursor(st: &mut GridState, nrows: usize, ncols: usize, dr: i64, dc: i64, extend: bool) {
    if nrows == 0 || ncols == 0 {
        return;
    }
    let cur = st.cursor.unwrap_or(Cell { row: 0, col: 0 });
    let row = (cur.row as i64 + dr).clamp(0, nrows as i64 - 1) as usize;
    let col = (cur.col as i64 + dc).clamp(0, ncols as i64 - 1) as usize;
    let cell = Cell { row, col };
    if extend {
        if st.anchor.is_none() {
            st.anchor = Some(cur);
        }
        st.cursor = Some(cell);
        st.ensure_visible = true;
    } else {
        st.select_cell(cell);
    }
}

fn keyboard(ui: &mut Ui, df: &DataFrame, st: &mut GridState, l: &Layout, actions: &mut Vec<GridAction>) {
    let nrows = df.height();
    let ncols = df.width();
    let events = ui.input(|i| i.events.clone());
    let page = l.page_rows as i64;
    for ev in events {
        match ev {
            Event::Key { key, pressed: true, modifiers, .. } => {
                let ext = modifiers.shift;
                let ctrl = modifiers.command;
                match key {
                    Key::ArrowUp => move_cursor(st, nrows, ncols, if ctrl { -(nrows as i64) } else { -1 }, 0, ext),
                    Key::ArrowDown if modifiers.alt => {
                        if let Some(c) = st.cursor.filter(|c| c.col < ncols) {
                            let at = l.filter_button(c.col, st.scroll_x).map_or(
                                pos2(l.col_left(c.col, st.scroll_x).max(l.data.min.x), l.data.min.y),
                                |b| b.left_bottom(),
                            );
                            actions.push(GridAction::Column(c.col, ColumnCommand::ValueFilter(at)));
                        }
                    }
                    Key::ArrowDown => move_cursor(st, nrows, ncols, if ctrl { nrows as i64 } else { 1 }, 0, ext),
                    Key::ArrowLeft => move_cursor(st, nrows, ncols, 0, if ctrl { -(ncols as i64) } else { -1 }, ext),
                    Key::ArrowRight => move_cursor(st, nrows, ncols, 0, if ctrl { ncols as i64 } else { 1 }, ext),
                    Key::PageUp => move_cursor(st, nrows, ncols, -page, 0, ext),
                    Key::PageDown => move_cursor(st, nrows, ncols, page, 0, ext),
                    Key::Home if ctrl => move_cursor(st, nrows, ncols, -(nrows as i64), -(ncols as i64), ext),
                    Key::End if ctrl => move_cursor(st, nrows, ncols, nrows as i64, ncols as i64, ext),
                    Key::Home => move_cursor(st, nrows, ncols, 0, -(ncols as i64), ext),
                    Key::End => move_cursor(st, nrows, ncols, 0, ncols as i64, ext),
                    Key::Tab => move_cursor(st, nrows, ncols, 0, if ext { -1 } else { 1 }, false),
                    Key::Enter | Key::F2 => {
                        if let Some(c) = st.cursor {
                            start_edit(st, df, c, None);
                        }
                    }
                    Key::Delete | Key::Backspace => clear_selection(st, df, actions),
                    Key::Escape => {
                        if let Some(c) = st.cursor {
                            st.select_cell(c);
                        }
                    }
                    Key::A if ctrl && nrows > 0 && ncols > 0 => {
                        // The active cell (cursor) stays top-left, like Excel.
                        st.anchor = Some(Cell { row: nrows - 1, col: ncols - 1 });
                        st.cursor = Some(Cell { row: 0, col: 0 });
                        st.whole_cols = false;
                        st.whole_rows = false;
                    }
                    _ => {}
                }
            }
            Event::Text(t) => {
                if let Some(c) = st.cursor {
                    if !t.chars().all(char::is_control) {
                        start_edit(st, df, c, Some(t));
                        break;
                    }
                }
            }
            Event::Copy => copy_selection(ui, st, df, actions),
            Event::Cut => {
                copy_selection(ui, st, df, actions);
                clear_selection(st, df, actions);
            }
            Event::Paste(text) => paste(st, df, &text, actions),
            _ => {}
        }
    }
    let _ = Modifiers::NONE;
}

fn copy_selection(ui: &Ui, st: &GridState, df: &DataFrame, actions: &mut Vec<GridAction>) {
    let Some((r0, r1, c0, c1)) = st.selection(df.height(), df.width()) else { return };
    let cells = (r1 - r0 + 1) * (c1 - c0 + 1);
    if cells > MAX_COPY_CELLS {
        actions.push(GridAction::Status(format!(
            "Selection too large to copy ({cells} cells, limit {MAX_COPY_CELLS}). Use File > Save As instead."
        )));
        return;
    }
    let columns = df.columns();
    let mut out = String::new();
    if st.whole_cols {
        let names: Vec<&str> = (c0..=c1).map(|c| columns[c].name().as_str()).collect();
        out.push_str(&names.join("\t"));
        out.push('\n');
    }
    for r in r0..=r1 {
        for c in c0..=c1 {
            if c > c0 {
                out.push('\t');
            }
            if let Some(t) = columns[c].get(r).ok().and_then(|v| value_text(&v)) {
                out.push_str(&tsv_field(&t));
            }
        }
        out.push('\n');
    }
    ui.ctx().copy_text(out);
    actions.push(GridAction::Status(format!("Copied {cells} cell(s)")));
}

/// Quotes a value for tab-separated clipboard text the way Excel does.
pub fn tsv_field(t: &str) -> std::borrow::Cow<'_, str> {
    if t.contains(['\t', '\n', '\r', '"']) {
        format!("\"{}\"", t.replace('"', "\"\"")).into()
    } else {
        t.into()
    }
}

fn clear_selection(st: &GridState, df: &DataFrame, actions: &mut Vec<GridAction>) {
    let Some((r0, r1, c0, c1)) = st.selection(df.height(), df.width()) else { return };
    let cells = (r1 - r0 + 1) * (c1 - c0 + 1);
    if cells > 5_000_000 {
        actions.push(GridAction::Status("Selection too large to clear; use a Fill or Delete column step".into()));
        return;
    }
    let rows: Vec<usize> = (r0..=r1).collect();
    let ops: Vec<Op> = (c0..=c1)
        .filter(|&c| editable(df.columns()[c].dtype()))
        .map(|c| Op::SetCells {
            column: df.columns()[c].name().to_string(),
            rows: rows.clone(),
            values: vec![None; rows.len()],
        })
        .collect();
    push_batch(actions, format!("Clear {cells} cell(s)"), ops);
}

fn push_batch(actions: &mut Vec<GridAction>, label: String, mut ops: Vec<Op>) {
    match ops.len() {
        0 => {}
        1 => actions.push(GridAction::Apply(ops.pop().unwrap())),
        _ => actions.push(GridAction::Apply(Op::Batch { label, ops })),
    }
}

/// Parses tab-separated clipboard text (Excel/Sheets format).
pub fn parse_tsv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut in_q = false;
    let mut at_field_start = true;
    while let Some(ch) = chars.next() {
        if in_q {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_q = false;
                }
            } else {
                field.push(ch);
            }
            continue;
        }
        match ch {
            '"' if at_field_start => {
                in_q = true;
                at_field_start = false;
            }
            '\t' => {
                row.push(std::mem::take(&mut field));
                at_field_start = true;
            }
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                at_field_start = true;
            }
            c => {
                field.push(c);
                at_field_start = false;
            }
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

fn paste(st: &GridState, df: &DataFrame, text: &str, actions: &mut Vec<GridAction>) {
    let Some((r0, r1, c0, c1)) = st.selection(df.height(), df.width()) else { return };
    let block = parse_tsv(text);
    if block.is_empty() {
        return;
    }
    let nrows = df.height();
    let ncols = df.width();
    let columns = df.columns();
    let mut ops = Vec::new();
    let single = block.len() == 1 && block[0].len() == 1;
    let mut cut = false;
    if single {
        // One value fills the whole selection.
        let v = &block[0][0];
        let rows: Vec<usize> = (r0..=r1).collect();
        for c in c0..=c1 {
            ops.push(Op::SetCells {
                column: columns[c].name().to_string(),
                rows: rows.clone(),
                values: vec![(!v.is_empty()).then(|| v.clone()); rows.len()],
            });
        }
    } else {
        let width = block.iter().map(|r| r.len()).max().unwrap_or(0);
        for dc in 0..width {
            let c = c0 + dc;
            if c >= ncols {
                cut = true;
                break;
            }
            let mut rows = Vec::new();
            let mut values = Vec::new();
            for (dr, line) in block.iter().enumerate() {
                let r = r0 + dr;
                if r >= nrows {
                    cut = true;
                    break;
                }
                let v = line.get(dc).cloned().unwrap_or_default();
                rows.push(r);
                values.push((!v.is_empty()).then_some(v));
            }
            ops.push(Op::SetCells { column: columns[c].name().to_string(), rows, values });
        }
    }
    let n: usize = ops.iter().map(|o| if let Op::SetCells { rows, .. } = o { rows.len() } else { 0 }).sum();
    push_batch(actions, format!("Paste {n} cell(s)"), ops);
    if cut {
        actions.push(GridAction::Status(
            "Part of the pasted block fell outside the table and was skipped (insert rows or columns first)".into(),
        ));
    }
}

fn context_menu(ui: &mut Ui, df: &DataFrame, st: &mut GridState, actions: &mut Vec<GridAction>) {
    let nrows = df.height();
    let ncols = df.width();
    let Some(target) = st.context else {
        ui.close();
        return;
    };
    let at = st.context_pos;
    let col_cmds = |ui: &mut Ui, c: usize, actions: &mut Vec<GridAction>| {
        let mut cmd = |ui: &mut Ui, label: &str, cc: ColumnCommand| {
            if ui.button(label).clicked() {
                actions.push(GridAction::Column(c, cc));
                ui.close();
            }
        };
        cmd(ui, "Sort ascending", ColumnCommand::SortAsc);
        cmd(ui, "Sort descending", ColumnCommand::SortDesc);
        cmd(ui, "Filter by values…", ColumnCommand::ValueFilter(at));
        cmd(ui, "Filter by condition…", ColumnCommand::Filter);
        ui.separator();
        cmd(ui, "Rename…", ColumnCommand::Rename);
        cmd(ui, "Change type…", ColumnCommand::ChangeType);
        cmd(ui, "Fill missing values…", ColumnCommand::FillMissing);
        cmd(ui, "Find and replace…", ColumnCommand::FindReplace);
        cmd(ui, "Drop rows with missing values", ColumnCommand::DropMissing);
        cmd(ui, "Drop duplicate rows by this column", ColumnCommand::DropDuplicates);
        ui.separator();
        cmd(ui, "Move column left", ColumnCommand::MoveLeft);
        cmd(ui, "Move column right", ColumnCommand::MoveRight);
        cmd(ui, "Insert column left", ColumnCommand::InsertLeft);
        cmd(ui, "Insert column right", ColumnCommand::InsertRight);
        cmd(ui, "Delete column", ColumnCommand::Delete);
    };
    match target {
        ContextTarget::Header(c) if c < ncols => col_cmds(ui, c, actions),
        ContextTarget::Row(r) if r < nrows => row_menu(ui, df, st, actions),
        ContextTarget::Cell(cell) if cell.row < nrows && cell.col < ncols => {
            if ui.button("Copy").clicked() {
                copy_selection(ui, st, df, actions);
                ui.close();
            }
            if ui.button("Edit cell").clicked() {
                start_edit(st, df, cell, None);
                ui.close();
            }
            if ui.button("Set to missing").clicked() {
                clear_selection(st, df, actions);
                ui.close();
            }
            let value = df.columns()[cell.col].get(cell.row).ok().and_then(|v| value_text(&v));
            let label = match &value {
                Some(v) => format!("Keep rows where value = \"{}\"", truncate_label(v)),
                None => "Keep rows where value is missing".into(),
            };
            if ui.button(label).clicked() {
                actions.push(GridAction::Column(cell.col, ColumnCommand::FilterValue(value)));
                ui.close();
            }
            ui.separator();
            ui.menu_button("Column", |ui| col_cmds(ui, cell.col, actions));
            ui.menu_button("Row", |ui| row_menu(ui, df, st, actions));
        }
        _ => {
            ui.close();
        }
    }
}

fn truncate_label(s: &str) -> String {
    if s.chars().count() > 24 { format!("{}…", s.chars().take(23).collect::<String>()) } else { s.to_string() }
}

fn row_menu(ui: &mut Ui, df: &DataFrame, st: &mut GridState, actions: &mut Vec<GridAction>) {
    let rows = st.selected_rows(df.height(), df.width());
    let Some(&first) = rows.first() else { return };
    let last = *rows.last().unwrap();
    if ui.button("Insert row above").clicked() {
        actions.push(GridAction::Apply(Op::InsertRows { at: first, count: 1 }));
        ui.close();
    }
    if ui.button("Insert row below").clicked() {
        actions.push(GridAction::Apply(Op::InsertRows { at: last + 1, count: 1 }));
        st.select_cell(Cell { row: last + 1, col: st.cursor.map_or(0, |c| c.col) });
        ui.close();
    }
    let label = if rows.len() == 1 { "Delete row".to_string() } else { format!("Delete {} rows", rows.len()) };
    if ui.button(label).clicked() {
        actions.push(GridAction::Apply(Op::DeleteRows { rows }));
        ui.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_excel_clipboard() {
        assert_eq!(parse_tsv("a\tb\r\n1\t2\r\n"), vec![vec!["a", "b"], vec!["1", "2"]]);
        assert_eq!(parse_tsv("\"x\ty\"\t\"q\"\"\"\n"), vec![vec!["x\ty", "q\""]]);
        assert_eq!(parse_tsv("solo"), vec![vec!["solo"]]);
    }

    #[test]
    fn formats_floats() {
        assert_eq!(fmt_f64(2.0), "2.0");
        assert_eq!(fmt_f64(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(fmt_f64(f64::NAN), "NaN");
    }
}
