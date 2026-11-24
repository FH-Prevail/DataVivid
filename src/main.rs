// Release builds are GUI programs (no console window). Command-line flags
// still print to the console they were started from (see `attach_console`).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod chart;
mod formats;
mod grid;
mod history;
mod icon_art;
mod io;
mod ops;
mod stats;
mod winassoc;

use std::ffi::OsString;
use std::path::PathBuf;

#[cfg(windows)]
const HELP: &str = "\
DataVivid - view, clean and convert tabular data
Originally developed by the Prevail Team, FH Steyr. MIT License.

Usage:
  datavivid [FILE]              open FILE (CSV, TSV, Parquet, JSON, NDJSON,
                                Arrow/Feather, Avro, Excel, ODS, SQLite)
  datavivid --register          offer DataVivid in Explorer's \"Open with\" and
                                in Settings > Default apps (current user only)
  datavivid --unregister        remove that registration
  datavivid --status            show the registration status
  datavivid --convert IN OUT    convert IN to OUT (format from the extension)
  datavivid --help | --version
";

#[cfg(not(windows))]
const HELP: &str = "\
DataVivid - view, clean and convert tabular data
Originally developed by the Prevail Team, FH Steyr. MIT License.

Usage:
  datavivid [FILE]              open FILE (CSV, TSV, Parquet, JSON, NDJSON,
                                Arrow/Feather, Avro, Excel, ODS, SQLite)
  datavivid --register          add DataVivid to the application menu and to
                                the file manager's \"Open with\" (current user)
  datavivid --make-default      register, then make DataVivid the default app
                                for CSV, TSV, Parquet, JSON Lines, Arrow, Avro
                                and SQLite (JSON, Excel and ODS are left alone)
  datavivid --unregister        remove the registration and those defaults
  datavivid --status            show the registration status
  datavivid --convert IN OUT    convert IN to OUT (format from the extension)
  datavivid --help | --version
";

#[cfg(windows)]
const REGISTERED_MSG: &str = "DataVivid is registered. Pick it as the default in Settings > Apps > Default apps, \
                              or via Open with > Choose another app > Always.";
#[cfg(not(windows))]
const REGISTERED_MSG: &str = "DataVivid is registered. It is in the application menu and under Open with; \
                              run `datavivid --make-default` to open data files with it on double-click.";

fn main() -> std::process::ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let first = args.first().and_then(|a| a.to_str()).unwrap_or("");

    if first.starts_with("--") {
        attach_console();
        let code = match first {
            "--help" | "-h" => {
                print!("{HELP}");
                0
            }
            "--version" => {
                println!("DataVivid {}", env!("CARGO_PKG_VERSION"));
                0
            }
            "--register" => report(winassoc::register().map(|_| REGISTERED_MSG.to_string())),
            #[cfg(not(windows))]
            "--make-default" => report(
                winassoc::choose_defaults()
                    .map(|_| "DataVivid is registered and is now the default app for data files.".to_string()),
            ),
            "--unregister" => report(winassoc::unregister().map(|_| "DataVivid registration removed.".to_string())),
            "--status" => {
                println!("{:?}", winassoc::status());
                0
            }
            "--convert" if args.len() == 3 => report(convert(PathBuf::from(&args[1]), PathBuf::from(&args[2]))),
            _ => {
                eprint!("Unknown or incomplete option.\n\n{HELP}");
                2
            }
        };
        return std::process::ExitCode::from(code);
    }

    winassoc::refresh_if_moved();
    let file = args.first().map(PathBuf::from);
    match app::run(file) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            attach_console();
            eprintln!("DataVivid failed to start: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn report(r: std::io::Result<String>) -> u8 {
    match r {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

fn convert(input: PathBuf, output: PathBuf) -> std::io::Result<String> {
    let to_io = |e: anyhow::Error| std::io::Error::other(format!("{e:#}"));
    let req = io::LoadRequest::new(&input).map_err(to_io)?;
    let loaded = io::load(req).map_err(to_io)?;
    let fmt = formats::Format::from_path(&output)
        .ok_or_else(|| std::io::Error::other("Unknown output format"))?;
    let mut opts = io::SaveOptions::for_format(fmt);
    if let Some(stem) = output.file_stem() {
        opts.sheet_name = stem.to_string_lossy().into_owned();
    }
    io::save(&loaded.df, &output, &opts).map_err(to_io)?;
    Ok(format!(
        "Wrote {} rows x {} columns to {}",
        loaded.df.height(),
        loaded.df.width(),
        output.display()
    ))
}

/// Lets a GUI-subsystem exe print to the console it was started from.
fn attach_console() {
    #[cfg(all(windows, not(debug_assertions)))]
    unsafe {
        use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
