# DataVivid: working notes for AI assistants and maintainers

Read this first; it holds the context needed to continue work quickly.
User-facing documentation is in `README.md`.

## What this is

- A standalone desktop alternative to VS Code's *Data Wrangler*: open, view,
  clean, edit and convert tabular files (CSV/TSV/TXT(.gz), Parquet, JSON,
  NDJSON, Arrow IPC/Feather, Avro, Excel, ODS, SQLite).
- Origin: Prevail Team, FH Steyr (University of Applied Sciences Upper
  Austria). License: MIT (`LICENSE`). Repo: https://github.com/FH-Prevail/DataVivid
- Windows first. It must be a single exe that can be registered as an
  "Open with" or default app. macOS and Linux builds are planned (see README).

## Status (v0.1.0, 2026-11-22)

Done and verified on Windows 11:

- **Tests:** 15 unit tests pass (`cargo test`). They cover read/write
  round-trips for all 9 writable formats, CSV sniffing, Windows-1252 with
  decimal comma, operations, history, clipboard parsing and float formatting.
- **Speed:** a 1M-row CSV (57 MB) opens in about 0.3 s, and the same data as
  Parquet in about 0.13 s.
- **Conversions:** `--convert` takes about 1 s per format for 1M rows; xlsx
  takes about 15 s.
- **GUI smoke test:** with the 1M-row file, using `scripts/dev`:
  - Selecting a column shows its statistics and histogram.
  - The header context menu works.
  - Sort works and is stable.
  - Filter Preview, then Apply, then Ctrl+Z undo all work.
  - Double-click editing works, and typing text into an int column is
    rejected with a clear message.
  - Ctrl+S on a Windows-1252 `;` file writes UTF-8 with a BOM and keeps `;`
    and the decimal comma.
  - Closing with unsaved changes shows the Save/Discard/Cancel prompt.
  - The start screen works.
- **Registration:** `--register` and `install.ps1` were verified. The HKCU
  keys are correct, existing OpenWithProgids were kept, and opening a file
  through the registered command works.

Not done or known gaps:

- **Other platforms:** macOS and Linux builds are untested. The code is
  `cfg`-gated: `winassoc.rs` has stubs and `build.rs` only embeds resources on
  Windows.
- **Code signing:** none. SmartScreen warns on first start of a downloaded exe.
- **Memory:** whole files are loaded into memory; there are no lazy scans yet.
- **Invalid edits:** after an invalid cell edit, Enter still moves the cursor
  down, and the error only shows in the status bar.
- **Dates:** date columns are written as ISO (`2024-01-01`) even if the
  source used `dd.mm.yyyy`, because type detection parsed them.
- **Exe size:** about 129 MB, almost all Polars. Trying `lto = "fat"` and
  `codegen-units = 1` would shrink it, but costs much longer builds.

## Architecture (src/)

| Module | Responsibility |
|---|---|
| `main.rs` | CLI flags (`--register/--unregister/--status/--convert/--help/--version`), else starts the GUI with an optional file path. Release builds use `windows_subsystem = "windows"`; CLI output works via `AttachConsole`. |
| `app.rs` | eframe `App`: menu bar, left panel (Operations form + Steps list), right panel (column summary / dataset summary), central grid, modal dialogs (Import options, Unsaved changes, Excel overwrite, About), background jobs (load, save, profiling) via `std::thread` + `mpsc`. Settings (recent files, theme, panels) persisted via eframe storage (`%APPDATA%\DataVivid\data\app.ron`). |
| `grid.rs` | Custom-painted virtualised grid (no egui_extras). Vertical scroll is tracked in **rows** (`top_row: f64`), not pixels, because f32 pixel offsets break beyond ~700k rows. Own scrollbars, selection (cell/range/whole columns/whole rows), inline `TextEdit` editor, keyboard nav, Excel-compatible TSV copy/paste, context menus. Returns `GridAction`s to the app. |
| `ops.rs` | `Op` enum. Every data change (including cell edits and pastes) is an `Op` with `describe()` and `apply(&DataFrame) -> Result<DataFrame>`. `Op::Batch` groups multi-column changes. `SetCells` builds a full-length mask and value column and uses `when/then/otherwise`, so it validates types strictly. |
| `history.rs` | Undo/redo as a list of `(Op, DataFrame-after)` snapshots. This is cheap because Polars columns are Arc-shared. It keeps at most 200 steps, folding the oldest into the base. `version()` increments on every change and drives cache invalidation. It tracks the dirty state against the saved cursor. |
| `io.rs` | `load(LoadRequest) -> Loaded` and `save(df, path, SaveOptions)`. See the delimited-text, spreadsheet and SQLite notes below this table. |
| `stats.rs` | `ColumnProfile`: missing, distinct, quantiles, mean/std, histogram (one bin per value for small integer ranges), top values, min/max, text length. Computed in a background thread per history version, selected column first; stale runs cancel via an `AtomicU64` generation. |
| `formats.rs` | Format enum ↔ extensions, labels, ProgID keys, `can_write`. |
| `winassoc.rs` | HKCU-only registration: ProgID per format, `OpenWithProgids`, `Applications\datavivid.exe\SupportedTypes`, `Software\DataVivid\Capabilities` + `RegisteredApplications`, `SHChangeNotify`. Never touches `UserChoice` (Windows forbids it; the user picks the default in Settings). `refresh_if_moved` re-registers only if the registered exe path no longer exists. |
| `icon_art.rs` | Procedural icon. It is used by the app for the window icon, and `include!`d by `build.rs` to write the `.ico` that `winresource` embeds. |

`io.rs` details:

- **Delimited text:**
  - gzip is detected automatically.
  - Encoding comes from the BOM, otherwise UTF-8 if the bytes are valid,
    UTF-16LE if they look like it, else Windows-1252.
  - The separator is sniffed from `, ; \t |`. A decimal comma is guessed for
    files that aren't comma-separated.
  - If a typed parse fails, it is retried with a full-file schema scan.
- **Spreadsheets:** read via calamine with typed column inference. Excel serial
  dates become Datetime, or Date when every value is at midnight.
- **SQLite:** read-only via rusqlite.
- **Writers:**
  - Excel uses rust_xlsxwriter and is limited to 1,048,575 rows.
  - SQLite always writes a new file with one table.
  - Every save goes to a temporary sibling `.<name>.datavivid-tmp` first, then
    is renamed over the target.

Design decisions (the Codex CLI agreed when consulted, 2026-11-22):

- egui/eframe rather than Slint, Tauri or iced: all Rust, a single exe, and
  portable later.
- Polars plus calamine plus rust_xlsxwriter, with rusqlite (bundled).
- Data Wrangler-style operations, preview, steps and undo.
- Per-user registration without admin rights, and an "Open Default apps
  settings" button (`ms-settings:defaultapps?registeredAppUser=DataVivid`).

## Build environment and gotchas

- **Toolchain:** Rust ≥ 1.95, because eframe/egui 0.37 require it. On
  Windows use the MSVC toolchain plus the VS C++ Build Tools. winresource
  needs `rc.exe` from the Windows SDK.
- **OneDrive/Dropbox checkouts:** set a local, gitignored
  `.cargo/config.toml` with `[build] target-dir = "…/AppData/Local/datavivid-target"`.
- **Build times:** a cold `cargo build --release` takes **15–35 min** (thin
  LTO over Polars); the dev profile takes about 2–4 min incremental. Iterate
  on the GUI with the dev build; it opens an extra console window and keeps
  stdout.
- **First launch of a new exe is slow:** Windows Defender scans it (about
  20 s), which once looked like a slow Parquet write. Benchmark the second run.
- **GUI-subsystem exe from scripts:** `cmd /c datavivid --status` does not
  wait for it. Use `Start-Process -Wait -PassThru -RedirectStandardOutput`.
- **Line endings:** the working tree has CRLF on Windows; `.gitattributes`
  normalises to LF (except `*.ps1`). When editing files with Python, keep the
  existing newline style (`newline=''`).

### Library API notes (versions in Cargo.lock)

egui / eframe 0.37:

- `App::ui(&mut self, ui, frame)`; there is no `update(ctx)`.
- Panels are `egui::Panel::top/left/right/bottom(id).show(ui, …)` and
  `CentralPanel::default().show(ui, …)`.
- `egui::MenuBar::new().ui(ui, …)`; close menus with `ui.close()`.
- `Modal::new(Id::unique("x")).show(ctx, …)`. `Id::new` is deprecated.
- `ctx.all_styles_mut(…)` replaces `style_mut`.
- `dropped_files[i].path()` is a method on a trait object.
- Double-click is reported on the second **release**
  (`pointer.button_double_clicked`), not on press.

polars 0.55:

- `DataFrame::new(height, columns)`.
- `Expr::is_in(other, nulls_equal)` needs `lit(series).implode(false)`.
- `LazyFrame::drop(cols([...]))`.
- `round(decimals, RoundMode)`.
- Avro types are at `polars::io::avro::{AvroReader, AvroWriter}`.
- `ChunkedArray::iter()`, not `into_iter()` on references.
- `.context()` is ambiguous between anyhow and polars, so use
  `.map_err(anyhow::Error::from).context(..)`.
- `DataType::Object` doesn't exist without the `object` feature.

Other crates:

- **calamine 0.38:** `Data::{Int, Float, String, Bool, DateTime(ExcelDateTime), DateTimeIso, DurationIso, Error, Empty}`.
- **rusqlite 0.43:** `stmt.column_names()` returns an iterator.
- **encoding_rs:** `decode_without_bom_handling` returns `(Cow<str>, bool)`.

## Testing

```powershell
cargo test                                          # unit tests
python scripts/dev/make_test_data.py testdata       # 1M-row CSV + Windows-1252 file (gitignored)
target\debug\datavivid.exe testdata\sales_1m.csv    # or the release exe
```

GUI smoke tests are driven from PowerShell:

- **Screenshot:** `scripts/dev/shot.ps1 -ProcId <pid> -Out x.png -Front`
  captures the window, DPI-aware. With the dev build, `MainWindowHandle` may
  be the console window, so capture a fixed screen rectangle instead.
- **Input:** `scripts/dev/input.ps1 -ProcId <pid> -Action click|dbl|rclick|keys|wheel -X -Y -Keys`
  clicks at window-relative coordinates or sends keys (SendKeys syntax).
- **Layout at 100 % scaling with the default window size:**
  - The header row is at y ≈ 80.
  - Data rows start at y ≈ 128 and are 22 px tall.
  - The left panel is about 290 px wide.

## Release procedure

1. Bump `version` in `Cargo.toml`, then run `cargo test` and
   `cargo build --release`.
2. Package `DataVivid-<ver>-windows-x64.zip` containing `datavivid.exe`,
   `install.ps1`, `uninstall.ps1`, `README.md` and `LICENSE`.
   `install.ps1` picks up the exe next to it.
3. Write `SHA256SUMS.txt` for the exe and the zip.
4. Tag `v<ver>` and create the GitHub release with the exe, zip and checksums
   as assets.
5. Without the `gh` CLI, use the REST API:
   - Get a token from `git credential fill`. Never print or store it.
   - `POST /repos/FH-Prevail/DataVivid/releases`, then upload the assets to
     `uploads.github.com/.../releases/<id>/assets?name=…` with
     `Content-Type: application/octet-stream`.

## Next steps (ideas, roughly by value)

1. CI (GitHub Actions) building Windows, macOS and Linux artifacts; then
   macOS `.app` (cargo-bundle, `CFBundleDocumentTypes`) and Linux `.desktop`
   integration in code.
2. Keep the cell editor open on invalid input instead of moving down.
3. Lazy or streaming open for files larger than RAM (Polars `scan_*`), with
   the grid reading slices.
4. Export the steps as Python (pandas/Polars) code; Ops are already a
   replayable recipe.
5. Group-by, pivot, and join/merge UIs; charts.
6. Signed MSI/MSIX installer; smaller exe (fat LTO, trimming Polars features).
