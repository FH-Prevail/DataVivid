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
  "Open with" or default app. Linux is supported since 2026-11-23 (same
  single binary, XDG registration); macOS is planned (see README).

## Status (v0.1.5, 2026-11-25)

v0.1.5 (asked for by Sina) makes multi-sheet files workable.

- **Sheet tabs:** a `Panel::bottom("sheets")` right under the grid
  (`App::sheet_tabs`), shown when the file has more than one sheet (Excel,
  ODS) or table (SQLite). It replaces the dropdown in the menu bar.
  Ctrl+Page Down / Ctrl+Page Up step through the sheets (`App::step_sheet`,
  consumed in `shortcuts()` before the grid sees Page Down). The button at
  the left lists all sheets.
- **Sheets stay in memory:** `Doc` holds the sheet on screen in its own
  fields and the other opened sheets in `Doc::parked` (`Parked`: history,
  load request, grid view, chart, profiles). `App::show_sheet` swaps them, so
  going back is instant and keeps steps, scroll, selection, statistics and
  chart. A sheet is read from the file on its first visit
  (`LoadKind::Sheet`, in the background, the old sheet stays on screen).
  **Nothing is written to disk on a switch.**
- **Unsaved changes are per file, not per sheet:** `Doc::is_dirty()` and
  `App::unsaved_before(&After)` look at all sheets. Title, status bar, Close,
  Open, Reload and Exit use them; the prompt names the sheets
  (`App::unsaved_text`) and "Discard all changes" drops every sheet's
  changes. "Save" in the prompt goes to a sheet with unsaved changes first
  (`App::save_then`), and after the save the prompt comes back if something
  is still unsaved.
- **What a save writes** (`Doc::save_scope` -> `SaveScope`):
  - `Workbook`: Excel/ODS file with several sheets saved as `.xlsx`. All
    sheets are written by `io::save_workbook`: sheets that differ from the
    file come from memory, the others (also never-opened and hidden ones)
    are copied cell by cell from the source file (`copy_cells`), at their
    original positions. The document moves to the written file.
  - `Sheet`: a file with several sheets saved to any other format (and every
    save of a multi-table SQLite file). Only the sheet on screen is written,
    the document stays where it is, and the sheet counts as saved
    (`History::mark_exported`) although the workbook lacks the change.
    Saving one table over the SQLite file it came from is refused.
  - `Table`: a file with one table, as before.
- **`History` has two marks:** `saved_at` (last save or export, for the
  prompt) and `file_at` (what the file itself holds, for deciding whether a
  workbook save copies the sheet from the file). `mark_unsaved()` clears
  both when a table changed while its file was being written.
- **Import options** in a multi-sheet file read only the sheet on screen
  again (`LoadKind::Reimport`); the other sheets keep their changes.
- **Also fixed:** chart/macro/dialog sheets are no longer listed
  (`io::worksheet_names`); after a save, `skip_rows` and `has_header` of the
  request are reset, because the written file starts with a header row; a
  single sheet saved as CSV loses its old sheet name; a second Ctrl+S while
  a save runs is ignored.
- **Tested:** 36 unit tests pass (11 new: sheet switching, dirty state,
  workbook save, export, rename, save-while-editing, raw copy of untouched
  and hidden sheets). GUI tested on Linux in a container with a virtual
  display (see Testing): tabs, keyboard switching, a 120,000-row sheet,
  30 sheets, the exit prompt from an untouched third sheet, Save and Discard
  from that prompt (file checked with openpyxl and by hash), Import options
  on one sheet, a two-table SQLite file. Not tested: the Save As file dialog
  with a multi-sheet file (no portal in the container), ODS and `.xls`
  sources by hand, and Windows.
- **Limits:** `--convert` has no sheet option (first sheet only). SQLite is
  still written as a new file with one table. Sheets are read one at a time,
  each read opens the workbook again.
- **Published 2026-11-25:**
  https://github.com/FH-Prevail/DataVivid/releases/tag/v0.1.5 (tag on
  `2d3c03b`, run 45588267317, 19 min with the warm main cache).
  - Before tagging, a manual run on `main` (45583462270) passed on Windows
    and Linux.
  - Anonymous downloads of all 5 files pass `sha256sum -c`.
  - SHA-256:
    - setup `e4f2c2fe797b1014b27a2b5add65d937d591ecd816a208dae3756f73c4b65ae3`
    - zip `6db61a8eb0535b493f660c61cde2b3057ab2f1375fc0f5ae1792e33560df1332`
    - exe `f703c42728822048877d8885be19126b48ffb0cc8af0256b559d77ded40f8dd0`
    - tarball `966ff6c44ba5b8ff5c3bef1c46f84ef8635a653ea6a272657514395a4628118a`
    - Linux binary `dd11ec55bc3b4d17187704b47d968d65d80147ee2c7f652779b6d30d714f4e34`
  - The Linux laptop runs 0.1.5 from the tarball (`install.sh --no-default`;
    `--version`, `--status` and `--convert` of an `.xlsx` checked). The GUI of
    the released binary was not opened; the dev build of the same commit was
    tested. The sheet tabs have not been tried on Windows by hand.

### v0.1.4 (2026-11-24)

v0.1.4 adds the chart panel (`chart.rs`, egui_plot 0.38), step 1 of the
plan agreed with Sina:

- Single-column and two-column charts, click-to-filter on category bars, and
  Copy data.
- Codex (gpt-6-astra, read-only) reviewed it; the accepted points are fixed:
  - the load counter in the request;
  - one worker at a time;
  - stale charts are faded and not clickable;
  - exact numbers in the copy;
  - "no values" instead of 0 for empty groups;
  - Welford correlation and a seeded reservoir sample;
  - running totals per period;
  - zero only on bar axes;
  - a missing-values bar outside "Other";
  - a note when a preview is open.
- Also: `App::apply` skips Filter/FilterValues ops that remove no rows (a
  second click on the same bar used to add a duplicate step).
- Tested on Linux/X11 with the 1M-row CSV: every chart type, click-to-filter
  (including clicks beside a bar), tooltips, Copy data. 25 unit tests pass.
- **Published 2026-11-24:**
  https://github.com/FH-Prevail/DataVivid/releases/tag/v0.1.4 (tag on
  `a3e5318`, run 45465737115).
  - Anonymous downloads of all 5 files pass `sha256sum -c`.
  - SHA-256:
    - setup `ff71c21b9199f780df137af50371dad5ce991fc3bc1a51ec14866ecd60017973`
    - zip `156b19e2aecfb005cad6f7b4327b26b02484b7d08971dbef797bcce8efc4170e`
    - exe `c28382ed2a1c5815049c828aeb52701d26bd15f620a369db39a4e9f06686b18c`
    - tarball `58bea7c4a89df9ed3d3f66b7aa2225b7754fd5245e6ba2c17b5ed44447f88e84`
    - Linux binary `c5a5f710516b7d157b1af57239f1507331d4d2ca4ed630b558bdc36e4bf98d8b`
  - The Linux laptop runs 0.1.4 from the tarball. The chart panel has not
    been tried on Windows by hand.

### v0.1.3 (2026-11-24)

v0.1.3 (asked for by a colleague of Sina's) adds:

- **Filter by values (Excel-style):**
  - Each header has a filter button (`Layout::filter_button`, hidden on
    columns narrower than 64 px). Alt+Down and the header menu open it too.
  - The button sends `ColumnCommand::ValueFilter(pos)`. The app shows the
    list as an `egui::Area` popup (`App::value_filter_popup`), built from
    `stats::distinct_values`: group_by, sorted in the column's own type,
    then cast to String.
  - OK applies `Op::FilterValues { values, exclude }`. It matches on
    `cast(String)`, the same text as the list, and stores whichever side
    (kept or removed) is shorter.
  - If the newest step is a FilterValues on the same column, the list is
    built from `History::before_last()`, and OK undoes that step before
    applying, so it edits the filter instead of stacking another.
  - With a search active, OK keeps only the ticked values that match (as
    Excel does). Clicking outside or Escape cancels.
  - Columns with a Filter or FilterValues among the applied steps get a
    funnel icon (`App::filtered_columns`).
- **Copy from the summary panel:**
  - `StatRow` holds the shown text plus the copy cells, which have no
    thousands separators.
  - `profile_text` and `dataset_text` build tab-separated tables.
  - There is a Copy button, and right-click menus on stat rows, frequent
    values (with "Keep only rows with this value" for text, category and
    bool columns) and the histogram.
- **Fix:** `shortcuts()` used `egui_wants_keyboard_input()`, which is true
  whenever any widget has focus, including the grid. So Ctrl+Z, Ctrl+Y and
  Ctrl+F never fired after clicking a cell. It now uses
  `text_edit_focused()`.
- **Linux font fallback:** `install_fonts` adds DejaVu Sans or Noto Sans
  after egui's font for symbols it lacks (→, ●, ↑). Before, they showed as
  boxes.
- **Testing:** 21 unit tests pass. The GUI was tested on Linux (X11) with
  the 1M-row CSV: filter, reopen and replace, search + Enter, numeric order,
  a 134k-value list, Escape and click-outside, all copy entries (checked
  with xclip), keep-only, Ctrl+Z/Ctrl+Y, and Ctrl+Z inside the cell
  editor.
- **Published 2026-11-24:**
  https://github.com/FH-Prevail/DataVivid/releases/tag/v0.1.3 (tag on
  `0f030c7`, run 45440981048). It was tagged without a manual `main` run
  first, because Cargo.lock dependencies were unchanged and main's cache was
  still valid.
  - Anonymous downloads of all 5 files pass `sha256sum -c`.
  - SHA-256:
    - setup `4bfc9ff00cb4af4d42f7a0a264e9ba8a061fdd98ed202ad702ceec75347bff0e`
    - zip `89b7cf3783dbf17362daaa80f468ddd34a9252d1a467a02845daa10c6e4abdbe`
    - exe `2ac051f1380ac6a595d55e503318f4d663fc0a48848588b77afbaf458c5c1204`
    - tarball `dbdce489b079ad7de45305fc76b75d2f6e757d907d5677c8ab52c0405145174f`
    - Linux binary `7e54c32da284e2b2851abeb8c8351575ad5d61633554008c117b186a1d7d127f`
  - The Linux laptop runs 0.1.3 from the tarball. The Windows GUI has not
    been tested by hand.

### v0.1.2 (2026-11-23)

v0.1.2 adds a Windows installer; the program itself is unchanged since 0.1.1.

- **Installer:** `packaging/windows/datavivid.iss` (Inno Setup) produces
  `DataVivid-<ver>-windows-x64-setup.exe`.
  - It is per user (`PrivilegesRequired=lowest`) and installs into
    `%LOCALAPPDATA%\Programs\DataVivid`, the folder `install.ps1` uses.
  - It replaces a zip install: it deletes that install's Settings > Apps key
    (`HKCU\...\Uninstall\DataVivid`, in `[Code]`) and `uninstall.ps1`.
  - It closes a running DataVivid through the Restart Manager, runs
    `datavivid.exe --register` after installing, and its uninstaller runs
    `--unregister`.
  - English and German.
  - The setup icon is the `datavivid.ico` that build.rs writes to OUT_DIR.
  - **Never change `AppId` {EEAF0EC4-489A-414E-89DA-9EADC0DD4AF8}.** Newer
    setups find and replace older ones through it, and the CI test checks the
    matching `..._is1` uninstall key.
  - The script is UTF-8 **with BOM**, because of the German "für";
    without the BOM, ISCC would read it as ANSI.
  - ISCC comes from the runner image, or from `choco install innosetup` if
    the image lacks it. It cannot be built or tested on the Linux laptop, so
    the workflow tests it on windows-latest.
- **Not the other way round:** running `install.ps1` after the setup would
  give two Settings > Apps entries. The docs recommend the setup.
- **Published 2026-11-23:**
  https://github.com/FH-Prevail/DataVivid/releases/tag/v0.1.2 (tag on
  `8a81bd3`, run 45216920061, about 30 min with the warm main cache).
  - Before tagging, a manual run on `main` (45208819283) passed all 17
    installer checks.
  - Anonymous downloads of all 5 files pass `sha256sum -c`.
  - SHA-256:
    - setup `906d888eaf5bc2837380696d1e0316ec6043d88a29f42e9bcca6adbe64212369`
    - zip `55228e48424b439dccbb130d923d1bf052384338c7dc7ee2fb71784d087eff7d`
    - exe `6f02b6d37aee1afd842fdec2c1024681c80b5921a675b00a836e2743faacda98`
    - tarball `95dbbb1f47df39ff077efa7b376e956d06d41eb1b9be454e2fe476253848974c`
    - Linux binary `2fff11168bee69056aba77a329fa07eaffb4b8bd8a122c8ea74b0f5c56c69c19`
  - The Linux laptop runs 0.1.2 from the tarball.
  - The setup's wizard UI has not been seen by a person yet; CI only ran it
    silently.

v0.1.1 was the first release built by GitHub Actions
(`.github/workflows/release.yml`). Changes since v0.1.0:

- **Clipboard fix:** `Cargo.toml` enabled eframe with
  `default-features = false` but without the `clipboard` and `links`
  features. egui then keeps copied text in a private in-app clipboard, so
  Ctrl+C never reached other apps (the status bar still said "Copied N
  cell(s)"). Pasting from other apps also failed, and Help > About's link did
  nothing. Both features are now on. On Linux they add only pure-Rust crates
  (arboard via x11rb, smithay-clipboard, webbrowser), so no new system
  libraries are needed. **Keep both features when touching the eframe line.**
- **Linux support**, the release workflow and `scripts/package-linux.sh`
  (see the Linux section and the release procedure below).

Published 2026-11-23:

- **Release:** https://github.com/FH-Prevail/DataVivid/releases/tag/v0.1.1.
  The tag is on `64a77d9`.
- **Workflow run:** https://github.com/FH-Prevail/DataVivid/actions/runs/45187340177.
  - Linux took 15 min and Windows 40 min, both cold.
  - The first run, on `403c6d5`, failed in the Windows smoke test: my
    `Set-Content` call took the CSV text as the file path. It was fixed in
    `64a77d9` and the tag was moved there. No release had been created.
- **SHA-256 of the assets:**
  - `datavivid.exe`: `4cc877c610a64283d0f7f2989e8804386cd8ecd0525f744ac80106b94485265b`
  - `DataVivid-0.1.1-windows-x64.zip`: `2cd7f0e04123e136550a70d3bf8ad6d4dfcd96de56d9542c0144e5c146a32fdb`
  - `datavivid-0.1.1-linux-x64`: `4c1b0244429aec5d2fc2a3837bc3a4944ca98bd30c2b576fa62d77e28c227b6c`
  - `DataVivid-0.1.1-linux-x64.tar.gz`: `c5f2ac46af57020351330943e22826fd04a075992d9b088ef5c2410cac750fdb`
- **Checks:**
  - Anonymous downloads pass `sha256sum -c SHA256SUMS.txt`.
  - The zip has the same flat five-file layout as v0.1.0.
  - The Linux tarball installed on the Fedora 43 laptop: the binary reports
    0.1.1 and is registered, `--convert` works, and copying cells reaches
    the X clipboard.
  - The Windows exe was only smoke-tested by CI (`--version`, `--convert`).
    The GUI and clipboard on Windows are still to be tested by hand.

v0.1.0, published 2026-11-22:

- **Repository:** public, MIT, default branch `main`.
- **Release:** https://github.com/FH-Prevail/DataVivid/releases/tag/v0.1.0.
  Its assets are `datavivid.exe` (128.8 MB), `DataVivid-0.1.0-windows-x64.zip`
  (the exe plus install/uninstall scripts, README and LICENSE) and
  `SHA256SUMS.txt`.
- **exe SHA-256:** `84f04461ac623d7d2365a1311b8a274864fb87b6220774960df6647202d93d60`.
- **Checks:** the exe was built from commit `847c776`. The anonymous download
  hash matched, and installing from the extracted zip with Windows PowerShell
  5.1 was tested.

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

Linux (added 2026-11-23, verified on Fedora 43, i3 on X11):

- **Build:** a cold `cargo build --release` takes about 3.5 min on a 20-core
  laptop; only a C compiler is needed. winit and glow load Wayland, X11,
  xkbcommon and OpenGL at run time, so no `-dev` packages are required.
- **Tests:** all 19 unit tests pass (`cargo test`), including the 4 Linux
  ones in `linuxassoc.rs`.
- **Checked by hand:** `--convert` (1M-row CSV to Parquet in 0.5 s); the GUI
  opens the 1M-row CSV in about 0.56 s; the window class is `datavivid`; File
  > Open shows the GTK portal dialog with the format filter;
  `--register/--make-default/--unregister` against a scratch `XDG_DATA_HOME`
  (desktop entry passes `desktop-file-validate`, GIO detects Parquet, Arrow,
  Avro and JSON Lines by extension and by magic bytes, and `--unregister`
  removes only DataVivid's lines from `mimeapps.list`).
- **Clipboard and links (v0.1.1, X11):**
  - With the 0.1.0 build, Ctrl+C left the X clipboard unchanged (checked with
    `xclip -o -selection clipboard`).
  - With the fix, copying 3x3 cells gives tab-separated rows with UTF-8
    intact ("Möbel").
  - Text put on the clipboard with `xclip` pastes into the grid ("Edit 2
    cells in …").
  - Help > About's link calls the browser: tested with `BROWSER` pointing at
    a logging script, which received `https://github.com/FH-Prevail/DataVivid`.
  - Wayland (`wl-paste`) was not tested, because this laptop runs i3 on X11.
- **Release assets:** built by the workflow on `ubuntu-22.04`, so they need
  glibc 2.35 or newer. A preview Linux build was attached to the v0.1.0
  release on 2026-11-23. It was built from `03e495b`, after the `v0.1.0` tag.
- **Decisions made against the original brief** (noted for the maintainer):
  - `--register` on Linux really registers (desktop entry, icons, MIME
    types) instead of printing a hint to the README. It was already
    implemented and tested before the brief arrived.
  - `install.sh` uses `datavivid --register`, not the static desktop file.
    That entry's `Exec` holds the absolute binary path, so it works even when
    `~/.local/bin` is not on the GUI session's PATH. The tarball still ships
    `packaging/linux/datavivid.desktop` (`Exec=datavivid %f`) and the 256 px
    PNG for system-wide or manual setups.
  - `install.sh` sets the default apps unless given `--no-default`.
    `--default` is still accepted.
  - CI runs `cargo test --release --locked` rather than `cargo test --locked`.
    It reuses the release build's dependencies instead of compiling Polars a
    second time in the dev profile.

Not done or known gaps:

- **macOS:** untested. `winassoc.rs` has stubs there, and `build.rs` only
  embeds resources on Windows.
- **Linux on Wayland and other desktops:** only tested on i3/X11. GNOME and KDE
  should match the window to `datavivid.desktop` through the app_id.
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
| `app.rs` | eframe `App`: menu bar, left panel (Operations form + Steps list), right panel (column summary / dataset summary), central grid, sheet tabs, modal dialogs (Import options, Unsaved changes, Excel overwrite, About), background jobs (load, save, profiling) via `std::thread` + `mpsc`. `Doc` is the open file: the sheet on screen plus the `parked` sheets, with the save bookkeeping (`save_scope`, `workbook_sheets`, `finish_save`), which is unit tested without a window. Settings (recent files, theme, panels) persisted via eframe storage (`%APPDATA%\DataVivid\data\app.ron`). |
| `grid.rs` | Custom-painted virtualised grid (no egui_extras). Vertical scroll is tracked in **rows** (`top_row: f64`), not pixels, because f32 pixel offsets break beyond ~700k rows. Own scrollbars, selection (cell/range/whole columns/whole rows), inline `TextEdit` editor, keyboard nav, Excel-compatible TSV copy/paste, context menus. Returns `GridAction`s to the app. |
| `ops.rs` | `Op` enum. Every data change (including cell edits and pastes) is an `Op` with `describe()` and `apply(&DataFrame) -> Result<DataFrame>`. `Op::Batch` groups multi-column changes. `SetCells` builds a full-length mask and value column and uses `when/then/otherwise`, so it validates types strictly. |
| `history.rs` | Undo/redo as a list of `(Op, DataFrame-after)` snapshots. This is cheap because Polars columns are Arc-shared. It keeps at most 200 steps, folding the oldest into the base. `version()` increments on every change and drives cache invalidation. It tracks the dirty state against two cursors: the last save or export (`is_dirty`) and what the file itself holds (`differs_from_file`). |
| `io.rs` | `load(LoadRequest) -> Loaded`, `save(df, path, SaveOptions)` for one table and `save_workbook(&[SheetOut], source, path)` for all sheets of an `.xlsx`. See the delimited-text, spreadsheet and SQLite notes below this table. |
| `chart.rs` | Chart panel. `compute(df, Request) -> ChartData` picks the chart from the column kinds (`Kind::{Number, Time, Category, Other}`) and aggregates before drawing: histogram (two passes, no copy), counts (Polars group_by, top 24 + Other, missing always its own bar), groups (Polars group_by; quantiles only for the box plot), over time (dates bucketed in Rust with Hinnant's civil-date math, running totals per period, values kept only for the median), scatter (seeded reservoir sample of `MAX_POINTS` = 50,000, Welford correlation). `ChartState` runs one background computation at a time (Polars can't cancel); `Request` holds `doc` (load counter) + history version + settings, and a stale chart is faded and not clickable. Horizontal category charts and vertical bars draw their own pointer tooltips (`category_plot`, `column_bars`) because egui_plot puts hover labels at the bar end, where they get cut off. Click on a category bar/box returns `Action::Apply(Op::FilterValues)`. `to_tsv` copies with `exact()` (round-trip) numbers; `num()` is the short display form. |
| `stats.rs` | `ColumnProfile`: missing, distinct, quantiles, mean/std, histogram (one bin per value for small integer ranges), top values, min/max, text length. Computed in a background thread per history version, selected column first; stale runs cancel via an `AtomicU64` generation. |
| `formats.rs` | Format enum ↔ extensions, labels, ProgID keys, `can_write`. |
| `linuxassoc.rs` | Linux implementation of the `winassoc` API (pulled in with `#[path]` as `winassoc::imp`). It writes `$XDG_DATA_HOME/applications/datavivid.desktop` (with an `X-DataVivid-Path` key for `status()`), PNG icons into `icons/hicolor` (its own small PNG encoder using flate2), and `mime/packages/datavivid.xml` for Parquet, Arrow, Avro, JSON Lines and `*.sqlite`, then runs the update-*-database tools. `choose_defaults()` runs `xdg-mime default` for `DEFAULT_MIME` (not JSON, Excel or ODS). On Linux the app_id is `datavivid` (`winassoc::LINUX_APP_ID`), so settings live in `~/.local/share/datavivid`. Never put `--` inside the XML comment, because update-mime-database rejects the whole file (a test guards this). |
| `winassoc.rs` | Shared registration API (`register/unregister/status/choose_defaults`). On Windows it does HKCU-only registration: ProgID per format, `OpenWithProgids`, `Applications\datavivid.exe\SupportedTypes`, `Software\DataVivid\Capabilities` + `RegisteredApplications`, `SHChangeNotify`. Never touches `UserChoice` (Windows forbids it; the user picks the default in Settings). `refresh_if_moved` re-registers only if the registered exe path no longer exists. |
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
  - `save_workbook` writes every sheet into a new `.xlsx`: a `SheetOut` with
    a table is written like a single sheet (bold header row, typed values),
    one without is copied from the source workbook with calamine, value by
    value. The source is read before the target is replaced, so both may be
    the same file. `xlsx_sheet_names` makes names Excel accepts (ODS allows
    more); the app renames its sheets to match after the save.
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

egui_plot 0.38 (the release built on egui 0.37; its versions don't follow egui's):

- `HoverPosition` is an enum (`NearDataPoint { position, .. }` / `Elsewhere`).
- Items take `.allow_hover(false)`; `Plot::show_x/show_y(false)` hide the
  coordinate readout.
- `BoxElem` needs an explicit `.stroke(..)`, or the whiskers and median line
  don't show when a fill is set.

Other crates:

- **calamine 0.38:** `Data::{Int, Float, String, Bool, DateTime(ExcelDateTime), DateTimeIso, DurationIso, Error, Empty}`.
- **rusqlite 0.43:** `stmt.column_names()` returns an iterator.
- **encoding_rs:** `decode_without_bom_handling` returns `(Cow<str>, bool)`.

## Testing

GUI tests on Linux when the laptop screen is locked (2026-11-25: it was, so
`DISPLAY=:0` only shows the lock screen; never type into it): run the dev
binary in a `fedora:43` container with a virtual display.

- `podman run -d --security-opt label=disable -v <scratch>:/work fedora:43 sleep infinity`,
  then `dnf install` `xorg-x11-server-Xvfb mesa-dri-drivers mesa-libGL
  mesa-libEGL libxkbcommon-x11 libXcursor libXi libXrandr xdotool
  ImageMagick openbox dejavu-sans-fonts`. `label=disable` avoids the `:Z`
  relabeling problem noted under Release procedure.
- In it: `Xvfb :99 -screen 0 1400x900x24`, `openbox`, then the binary with
  `DISPLAY=:99 LIBGL_ALWAYS_SOFTWARE=1 XDG_DATA_HOME=/work/xdg`. Drive it
  with `xdotool` (`key ctrl+Next`, `mousemove X Y click 1`, `key alt+F4`
  for the close button) and take screenshots with `import -window root`.
- File dialogs do not open there (no XDG portal), so Save As can't be driven.

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

Commit as `Sina <sina.mirshahi@fh-steyr.at>` (set repo-locally). That email is
linked to the FH-Prevail GitHub account, so GitHub credits the commits to it.
The first two commits used `p42872@fhooe.at`, which no GitHub account claims,
so GitHub shows them without an author.

Releases are built and published by `.github/workflows/release.yml`:

1. Bump `version` in `Cargo.toml` and run `cargo update -p datavivid`, so
   `Cargo.lock` matches. CI builds with `--locked`.
2. Write `.github/release-notes/v<ver>.md`; the workflow appends the
   checksums. Without the file the notes are only a title line.
3. Run `cargo test`, commit and push `main`.
4. `git tag v<ver> && git push origin v<ver>`. The tag must equal the
   `Cargo.toml` version, otherwise both build jobs fail at once.
5. Watch the run: `gh run list --workflow release.yml -L 1`, then
   `gh run watch <id> --exit-status`. A cold run takes about 30–40 min,
   because Windows is the slow job.
   - The **windows-latest** job builds, tests and smoke-tests
     `--version`/`--convert` through `Start-Process`. It packages
     `DataVivid-<ver>-windows-x64.zip` (a flat zip with exe, install.ps1,
     uninstall.ps1, README.md and LICENSE, the same layout as v0.1.0) plus
     the bare `datavivid.exe`. Since 0.1.2 it also builds
     `DataVivid-<ver>-windows-x64-setup.exe` with Inno Setup and tests it.
     The test makes an install with `install.ps1`, upgrades it silently with
     the setup, checks files, registry, shortcut and the single Settings >
     Apps entry, runs the setup again, then uninstalls and checks that
     everything is gone.
   - The **ubuntu-22.04** job does the same and runs
     `scripts/package-linux.sh`, which writes the tarball and
     `datavivid-<ver>-linux-x64`.
   - The **release** job writes `SHA256SUMS.txt` and runs
     `gh release create` with `GITHUB_TOKEN`. If the release already exists,
     it runs `upload --clobber` and `edit` instead, so a rerun is safe.
6. **If the run fails:** fix it, push `main`, then move the tag:
   `git tag -d v<ver>; git push origin :refs/tags/v<ver>`. If a release was
   already created, also run `gh release delete v<ver> --yes`. Tag and push
   again.
7. **Verify:**
   - `gh release view v<ver>` lists all 6 assets: setup, zip, exe, tarball,
     Linux binary and SHA256SUMS.
   - Download them and run `sha256sum -c SHA256SUMS.txt`.
   - Install the Linux tarball on a Linux machine.
   - The Windows exe can only be smoke-tested by CI. Test it by hand on
     Windows.

Notes:

- **Caches:** GitHub does not share Actions caches between tags. A cache
  saved on `v0.1.1` is invisible to `v0.1.2`, but caches from `main` are
  visible to every tag. Running the workflow by hand on `main` (Actions >
  Release > Run workflow) builds and tests without releasing, and warms that
  cache. Do it before tagging.
- **Action versions:** every action is pinned to a version tag. Bump them
  deliberately.
- **Local Linux build:** `scripts/release-linux.sh` builds the same Linux
  files in an Ubuntu 22.04 container, which is handy for testing before
  tagging.
- **Don't run two podman containers with `:Z` mounts of this repo at the same
  time.** Each relabels the tree for SELinux and locks the other out of the
  cargo cache. A parallel actionlint run broke a build this way.
- **Without the `gh` CLI:** use the REST API. Get a token from
  `git credential fill`, and never print or store it.

## Next steps (ideas, roughly by value)

1. A macOS job in the release workflow, then a macOS `.app` (cargo-bundle,
   `CFBundleDocumentTypes`).
2. Keep the cell editor open on invalid input instead of moving down.
3. Lazy or streaming open for files larger than RAM (Polars `scan_*`), with
   the grid reading slices.
4. Export the steps as Python (pandas/Polars) code; Ops are already a
   replayable recipe.
5. Sheets: a `--sheet` option for `--convert`; write all tables of a SQLite file; add, rename and delete sheets.
6. Group-by, pivot, and join/merge UIs. Charts step 2/3 (planned with Sina, 2026-11-24): click a histogram bin or a period to filter a range; correlation heatmap; missing-values map; save a chart as PNG; break lines across empty periods.
7. Code-sign the setup and exe (SmartScreen); smaller exe (fat LTO, trimming Polars features).
