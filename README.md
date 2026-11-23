# DataVivid

A fast, standalone desktop app for viewing, cleaning and converting tabular
data, in the spirit of VS Code's Data Wrangler but without VS Code, Python or
a browser. Written in Rust with [Polars](https://pola.rs) (data engine) and
[egui](https://github.com/emilk/egui) (UI).

Originally developed by the **Prevail Team at FH Steyr** (University of
Applied Sciences Upper Austria). DataVivid is open source under the
[MIT License](LICENSE): free for everyone to use, change and share.

Releases are available for **Windows** and **Linux** (x64). They are built,
tested and published by GitHub Actions
([`.github/workflows/release.yml`](.github/workflows/release.yml)) whenever a
version tag is pushed. The code also builds on macOS (see
[Building for other operating systems](#building-for-other-operating-systems)).

## Download and install (Windows)

**Easiest: the installer.** Download
`DataVivid-<version>-windows-x64-setup.exe` from the
[latest release](https://github.com/FH-Prevail/DataVivid/releases/latest) and
run it.

- It installs for your user only (no admin rights) to
  `%LOCALAPPDATA%\Programs\DataVivid`, and adds DataVivid to the Start menu
  (optionally the desktop), to *Settings > Apps* and to Explorer's
  *Open with* menu for all supported formats.
- At the end it can open *Settings > Default apps*, so you can make it the
  default app.
- **It replaces an older DataVivid:** an install from the zip's
  `install.ps1`, or one from an earlier setup, is upgraded in place, and a
  running DataVivid is closed first.
- Uninstall from *Settings > Apps > DataVivid*.

For scripted installs the setup accepts the usual Inno Setup switches, e.g.
`DataVivid-<version>-windows-x64-setup.exe /VERYSILENT /SUPPRESSMSGBOXES`.

The zip and the portable exe are still available:

1. Download from the [latest release](https://github.com/FH-Prevail/DataVivid/releases/latest)
   either
   - `DataVivid-<version>-windows-x64.zip` (exe + install scripts), or
   - `datavivid.exe` alone (portable).
2. **With the zip**: unpack it and run in PowerShell

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install.ps1
   ```

   This installs DataVivid for your user only (no admin rights) to
   `%LOCALAPPDATA%\Programs\DataVivid`, adds a Start menu shortcut and an
   entry in *Settings > Apps*, registers the file types and opens *Settings >
   Default apps* so you can pick DataVivid as the default.
3. **Portable exe**: put `datavivid.exe` wherever you like, start it and use
   *Windows > Register DataVivid for data files* (or run
   `datavivid.exe --register`). If you move the exe later, register again.

The setup and the exe are not code-signed yet, so Windows SmartScreen may warn
on first start: choose *More info > Run anyway*. Each release lists SHA-256 checksums
you can compare with `Get-FileHash datavivid.exe`.

### Making it the default app

Windows 10/11 do not allow programs to make themselves the default opener;
that choice is always yours. After registration either

- right-click a file > **Open with > Choose another app > DataVivid >
  Always**, or
- open **Settings > Apps > Default apps > DataVivid** and choose the file
  types (*Windows > Choose DataVivid as default app…* in the app opens this
  page).

Uninstall via *Settings > Apps > DataVivid*, or run
`datavivid.exe --unregister` for the portable exe.

## Download and install (Linux)

The Linux build runs on x64 distributions with glibc 2.35 or newer: Ubuntu
22.04+, Debian 12+, Fedora 36+, Linux Mint 21+ and others of that age, on
both Wayland and X11.

1. Download from the [latest release](https://github.com/FH-Prevail/DataVivid/releases/latest)
   either
   - `DataVivid-<version>-linux-x64.tar.gz` (binary, install scripts,
     `datavivid.desktop`, `datavivid.png`, README, LICENSE), or
   - `datavivid-<version>-linux-x64` alone (portable).
2. **With the tarball**: unpack it and run the installer (no sudo)

   ```bash
   tar xzf DataVivid-<version>-linux-x64.tar.gz
   cd DataVivid-<version>-linux-x64
   ./install.sh             # or ./install.sh --no-default
   ```

   This installs DataVivid for your user only to `~/.local/bin/datavivid`,
   adds it to the application menu and to the file manager's *Open with* list
   for all supported formats, and makes it the default app for CSV, TSV,
   Parquet, JSON Lines, Arrow, Avro and SQLite files. JSON, Excel and ODS keep
   their current app. With `--no-default` your default apps stay as they are.
   For a system-wide or hand-made setup, the tarball also has
   `datavivid.desktop` (`Exec=datavivid %f`) and the 256 px icon.
3. **Portable binary**: `chmod +x` it, start it and use *Desktop > Register
   DataVivid for data files* (or run `datavivid --register`). If you move the
   binary later, register again.

What `--register` writes, all under `~/.local/share`: the desktop entry
`applications/datavivid.desktop`, the icon in `icons/hicolor/*/apps/`, and
`mime/packages/datavivid.xml`, which teaches the desktop the Parquet,
Arrow/Feather, Avro and JSON Lines file types. `--make-default` (or *Desktop >
Make DataVivid the default app*) runs `xdg-mime default`. `--unregister`, or
`~/.local/share/datavivid/uninstall.sh` after a tarball install, removes all of
it again.

File dialogs use the XDG desktop portal, which GNOME, KDE and most other
desktops install by default (otherwise install `xdg-desktop-portal` plus
`xdg-desktop-portal-gtk` or `-kde`).

## Formats

| Format | Extensions | Read | Write |
|---|---|:-:|:-:|
| CSV / delimited text | `.csv` `.txt` `.psv` (also `.gz`) | ✔ | ✔ |
| TSV | `.tsv` `.tab` | ✔ | ✔ |
| Apache Parquet | `.parquet` `.pq` | ✔ | ✔ (zstd) |
| JSON (array of records) | `.json` | ✔ | ✔ |
| JSON Lines / NDJSON | `.jsonl` `.ndjson` | ✔ | ✔ |
| Arrow IPC / Feather | `.arrow` `.feather` `.ipc` | ✔ | ✔ |
| Apache Avro | `.avro` | ✔ | ✔ |
| Excel | `.xlsx` `.xlsm` `.xlsb` `.xls` | ✔ (sheet picker) | ✔ `.xlsx` |
| OpenDocument | `.ods` | ✔ (sheet picker) | – |
| SQLite | `.sqlite` `.sqlite3` `.db` | ✔ (table picker) | ✔ new file |

Delimited text is sniffed automatically: separator (`,` `;` tab `|`),
encoding (UTF-8, UTF-8/16 with BOM, otherwise Windows-1252) and the European
decimal comma. *File > Import options* overrides any of these, and can turn
type detection off so IDs such as `00123` keep their leading zeros. Files that
were not UTF-8 are saved as UTF-8 with a BOM, so nothing is lost and Excel
still shows special characters correctly.

Excel, ODS and SQLite are imported as plain values. Saving an `.xlsx` in place
asks first, because formatting, formulas and other sheets are not kept.
SQLite databases are never overwritten in place (*Save* becomes *Save as*).

## Features

- **Virtualised grid** for millions of rows: only visible cells are read and
  drawn. Column headers show type, distinct values, missing-value share and a
  mini histogram or category bar.
- **Editing**: double-click, Enter, F2 or just type to edit a cell (values are
  validated against the column type). Excel-compatible copy/paste (Ctrl+C,
  Ctrl+V), Delete clears cells, right-click for row/column actions.
- **Operations** (left panel, *Data* menu or header right-click): sort, filter
  (13 conditions incl. regex), find & replace, change type (with a check of how
  many values would be lost), fill missing (value, mean, median, min, max,
  previous, next), drop missing, drop duplicates, rename, move, insert or
  delete columns, add a column from a SQL formula, trim/case, round, and a full
  **SQL query** against the table (`SELECT … FROM data`).
- **Preview** any operation before applying it.
- **Steps & undo**: every change is a step; undo/redo (Ctrl+Z/Ctrl+Y) or click
  a step to go back to it.
- **Summary panel**: per-column statistics (missing, distinct, quantiles, mean,
  std, histogram, most frequent values) computed in the background.
- **Safe saving**: writes to a temporary file first and then replaces the
  original, so a failed save never destroys your data. Unsaved changes are
  never discarded without asking.
- Recent files, drag & drop, light/dark theme, opens files passed on the
  command line (so Explorer's "Open with" works).

On a laptop, a 1,000,000-row CSV (57 MB) opens in about 0.3 s, the same data
as Parquet in about 0.15 s.

## Command line

```text
datavivid [FILE]                          open a file
datavivid --register                      "Open with" + Default apps entries (current user)
datavivid --make-default                  Linux only: also become the default app for data files
datavivid --unregister                    remove them again
datavivid --status                        show the registration status
datavivid --convert in.csv out.parquet    headless conversion (format from extension)
datavivid --help | --version
```

On Windows, what `--register` writes (all under `HKEY_CURRENT_USER`): one ProgID per
format (`Software\Classes\DataVivid.Csv`, `…Parquet`, …) with icon and open
command, the ProgIDs under each extension's `OpenWithProgids`,
`Software\Classes\Applications\datavivid.exe` with `SupportedTypes`, and
Default Programs capabilities in `Software\DataVivid\Capabilities`, listed in
`Software\RegisteredApplications`. Existing defaults are never changed.

## Building from source (Windows)

Requirements: [Rust](https://rustup.rs) 1.95 or newer (stable, MSVC
toolchain) and the Visual Studio C++ Build Tools ("Desktop development with
C++").

```powershell
git clone https://github.com/FH-Prevail/DataVivid.git
cd DataVivid
cargo test                      # IO round-trips, operations, history, parsing
cargo build --release           # target\release\datavivid.exe
pwsh -File scripts\install.ps1  # optional: build + install + register
```

The first build compiles Polars and takes 15–35 minutes for `--release`
(thin LTO); `cargo build` (dev profile, already optimised for dependencies) is
much faster for development. If the checkout lives in OneDrive or Dropbox,
keep the build output out of the synced folder with a local, gitignored
`.cargo/config.toml`:

```toml
[build]
target-dir = "C:/Users/<you>/AppData/Local/datavivid-target"
```

## Building for other operating systems

DataVivid is plain Rust and all dependencies are cross-platform. File type
registration is implemented for Windows (registry, `src/winassoc.rs`) and
Linux (XDG desktop entry and MIME types, `src/linuxassoc.rs`); on macOS the
*Desktop* menu reports that registration is not available and file
associations come from the app bundle described below.

**Build natively on each operating system.** Cross-compiling from Windows is
not practical because Polars, SQLite (bundled) and zstd compile C code for the
target. For automated builds use a CI runner per OS (e.g. GitHub Actions
`windows-latest`, `macos-latest`, `ubuntu-latest`).

> Status: Windows and Linux are built and tested by the maintainers. The
> macOS build follows the steps below but has not been tested yet; reports and
> pull requests are welcome.

### Linux

```bash
# 1. Toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# 2. A C compiler (Debian/Ubuntu; use the equivalent packages elsewhere).
#    The window system libraries (Wayland, X11, xkbcommon, OpenGL) are loaded
#    at run time, so no -dev packages are needed.
sudo apt install build-essential pkg-config
# 3. Build, test and install for the current user
cargo test
cargo build --release            # -> target/release/datavivid
scripts/install.sh               # copies to ~/.local/bin, registers, sets defaults
```

Official releases come from the release workflow, which builds on Ubuntu
22.04. To produce the same tarball locally, `scripts/release-linux.sh` builds
inside an Ubuntu 22.04 container (podman or docker) and packages the result in
`dist/` with `scripts/package-linux.sh`.

### macOS

```bash
xcode-select --install                                   # compilers
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo build --release                                    # -> target/release/datavivid
```

A universal binary for Apple Silicon and Intel:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
lipo -create -output datavivid \
  target/aarch64-apple-darwin/release/datavivid \
  target/x86_64-apple-darwin/release/datavivid
```

To appear in Finder's "Open With" menu, the binary must live in an app bundle
whose `Info.plist` declares `CFBundleDocumentTypes` (e.g. for `public.comma-separated-values-text`,
`.parquet`, `.xlsx`). The easiest way is [`cargo-bundle`](https://github.com/burtonageo/cargo-bundle):

```bash
cargo install cargo-bundle
cargo bundle --release           # -> target/release/bundle/osx/DataVivid.app
```

Unsigned apps need a right-click > *Open* on first start (Gatekeeper).

## Code layout

| File | Purpose |
|---|---|
| `src/main.rs` | command line, registration flags, `--convert` |
| `src/app.rs` | window, menus, panels, dialogs, background jobs |
| `src/grid.rs` | virtualised, custom-painted data grid |
| `src/ops.rs` | all data operations (`Op`), each undoable |
| `src/history.rs` | undo/redo snapshots |
| `src/io.rs` | readers/writers, CSV sniffing, encodings, safe save |
| `src/stats.rs` | column profiles for headers and the summary panel |
| `src/formats.rs` | format/extension table |
| `src/winassoc.rs` | file-type registration: Windows (HKCU) and the shared API |
| `src/linuxassoc.rs` | file-type registration on Linux (desktop entry, icons, MIME) |
| `src/icon_art.rs` | procedural app icon (window + embedded `.ico`) |
| `build.rs` | embeds icon and version info into the Windows exe |
| `scripts/install.ps1`, `uninstall.ps1` | per-user install/uninstall on Windows |
| `scripts/install.sh`, `uninstall.sh` | per-user install/uninstall on Linux |
| `scripts/release-linux.sh` | local portable Linux build (Ubuntu 22.04 container) |
| `scripts/package-linux.sh` | packs the Linux tarball (used by CI and `release-linux.sh`) |
| `packaging/linux/datavivid.desktop` | desktop entry shipped in the Linux tarball |
| `packaging/windows/datavivid.iss` | Inno Setup script for the Windows installer |
| `.github/workflows/release.yml` | builds, tests and publishes releases on `v*` tags |
| `.github/release-notes/v<ver>.md` | release notes the workflow uses for that tag |
| `scripts/dev/` | screenshot and input helpers for GUI smoke tests |
| `CLAUDE.md` | working notes for AI coding assistants (architecture, gotchas, status) |

## Roadmap

- A tested macOS build with file associations (`Info.plist`), added to the
  release workflow.
- Code-signed Windows installer and exe.
- Larger-than-memory files via Polars lazy scans and streaming.
- Exporting the step list as Python (pandas/Polars) code.
- Joins/merge, pivot, group-by UI, charts.

## Contributing

Issues and pull requests are welcome. Please run `cargo test` before
submitting, and describe how you tested GUI changes.

## License

[MIT](LICENSE) © 2026 Prevail Team, FH Steyr. Free to use, modify and
distribute, including commercially.
