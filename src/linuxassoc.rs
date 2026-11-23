// Linux desktop integration for the current user (no root), following the
// XDG specifications. Included by `winassoc.rs` as its `imp` module.
//
// `register` writes:
//
// * `$XDG_DATA_HOME/applications/datavivid.desktop`, with the absolute path
//   of this executable and the MIME types DataVivid opens, so it shows up in
//   the application menu and in the file manager's "Open with" list,
// * the icon as PNGs under `$XDG_DATA_HOME/icons/hicolor/<n>x<n>/apps/`,
// * `$XDG_DATA_HOME/mime/packages/datavivid.xml`, which defines the data
//   formats that shared-mime-info does not know (Parquet, Arrow/Feather, Avro,
//   JSON Lines, `.sqlite`),
//
// and then refreshes the desktop, MIME and icon caches. Unlike Windows, Linux
// lets a program set the default opener itself: `choose_defaults` runs
// `xdg-mime default` for the pure data formats (see `DEFAULT_MIME`).
//
// `$XDG_DATA_HOME` defaults to `~/.local/share`.

use super::*;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const DESKTOP_ID: &str = "datavivid.desktop";
const ICON_NAME: &str = "datavivid";
const ICON_SIZES: [u32; 6] = [16, 32, 48, 64, 128, 256];
/// Key in the desktop file that records which executable was registered.
const PATH_KEY: &str = "X-DataVivid-Path";

/// MIME types listed in the desktop entry ("Open with" candidates).
const MIME_TYPES: [&str; 14] = [
    "text/csv",
    "text/tab-separated-values",
    "application/vnd.apache.parquet",
    "application/json",
    "application/x-ndjson",
    "application/vnd.apache.arrow.file",
    "application/vnd.apache.avro",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/vnd.ms-excel.sheet.macroEnabled.12",
    "application/vnd.ms-excel.sheet.binary.macroEnabled.12",
    "application/vnd.ms-excel",
    "application/vnd.oasis.opendocument.spreadsheet",
    "application/vnd.sqlite3",
    "application/x-sqlite3",
];

/// MIME types DataVivid becomes the default for in `choose_defaults`. JSON,
/// Excel and ODS stay with the user's editor or office suite; DataVivid is
/// still offered for them under "Open with".
pub const DEFAULT_MIME: [&str; 7] = [
    "text/csv",
    "text/tab-separated-values",
    "application/vnd.apache.parquet",
    "application/x-ndjson",
    "application/vnd.apache.arrow.file",
    "application/vnd.apache.avro",
    "application/vnd.sqlite3",
];

const MIME_PACKAGE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- Installed by DataVivid when it registers for data files; removed when it unregisters.
     (No double hyphens in XML comments, so the command-line flags are not spelled out.) -->
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="application/vnd.apache.parquet">
    <comment>Apache Parquet</comment>
    <generic-icon name="x-office-spreadsheet"/>
    <magic priority="50"><match type="string" offset="0" value="PAR1"/></magic>
    <glob pattern="*.parquet"/>
    <glob pattern="*.pq"/>
  </mime-type>
  <mime-type type="application/vnd.apache.arrow.file">
    <comment>Apache Arrow IPC / Feather</comment>
    <generic-icon name="x-office-spreadsheet"/>
    <magic priority="50"><match type="string" offset="0" value="ARROW1"/></magic>
    <glob pattern="*.arrow"/>
    <glob pattern="*.feather"/>
    <glob pattern="*.ipc"/>
  </mime-type>
  <mime-type type="application/vnd.apache.avro">
    <comment>Apache Avro</comment>
    <generic-icon name="x-office-spreadsheet"/>
    <magic priority="50"><match type="string" offset="0" value="Obj\001"/></magic>
    <glob pattern="*.avro"/>
  </mime-type>
  <mime-type type="application/x-ndjson">
    <comment>JSON Lines</comment>
    <sub-class-of type="text/plain"/>
    <generic-icon name="text-x-generic"/>
    <glob pattern="*.jsonl"/>
    <glob pattern="*.ndjson"/>
  </mime-type>
  <mime-type type="application/vnd.sqlite3">
    <glob pattern="*.sqlite"/>
  </mime-type>
</mime-info>
"#;

fn data_home() -> io::Result<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local/share"))
}

fn desktop_file() -> io::Result<PathBuf> {
    Ok(data_home()?.join("applications").join(DESKTOP_ID))
}

fn mime_package() -> io::Result<PathBuf> {
    Ok(data_home()?.join("mime/packages/datavivid.xml"))
}

fn icon_file(size: u32) -> io::Result<PathBuf> {
    Ok(data_home()?.join(format!("icons/hicolor/{size}x{size}/apps/{ICON_NAME}.png")))
}

fn exe_path() -> io::Result<String> {
    Ok(std::env::current_exe()?.to_string_lossy().into_owned())
}

/// Quotes one argument for the `Exec` key (Desktop Entry spec, "The Exec
/// key"): reserved characters need double quotes, and inside them `"`, `` ` ``,
/// `$` and `\` are backslash-escaped. The string-level escaping of the key
/// value then doubles every backslash.
fn exec_arg(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg.chars().all(|c| c.is_alphanumeric() || "/._-+,:@".contains(c));
    let quoted = if plain {
        arg.to_string()
    } else {
        let mut s = String::from("\"");
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                s.push('\\');
            }
            s.push(c);
        }
        s.push('"');
        s
    };
    quoted.replace('\\', "\\\\").replace('%', "%%")
}

fn desktop_entry(exe: &str) -> String {
    let path_value = exe.replace('\\', "\\\\").replace('\n', "\\n");
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.5\n\
         Name={APP_NAME}\n\
         GenericName=Data Viewer\n\
         Comment=View, clean and convert CSV, Parquet, Excel, JSON, Arrow, Avro and SQLite data\n\
         Exec={} %f\n\
         TryExec={path_value}\n\
         Icon={ICON_NAME}\n\
         Terminal=false\n\
         StartupNotify=true\n\
         StartupWMClass={}\n\
         Categories=Office;Spreadsheet;Database;\n\
         Keywords=csv;parquet;excel;json;arrow;avro;sqlite;dataframe;table;\n\
         MimeType={};\n\
         {PATH_KEY}={path_value}\n",
        exec_arg(exe),
        super::LINUX_APP_ID,
        MIME_TYPES.join(";"),
    )
}

pub fn register() -> io::Result<()> {
    let exe = exe_path()?;
    let desktop = desktop_file()?;
    write_file(&desktop, desktop_entry(&exe).as_bytes())?;
    for size in ICON_SIZES {
        write_file(&icon_file(size)?, &png_bytes(size, &crate::icon_art::icon_rgba(size)))?;
    }
    write_file(&mime_package()?, MIME_PACKAGE.as_bytes())?;
    refresh_caches()?;
    Ok(())
}

pub fn unregister() -> io::Result<()> {
    remove_if_exists(&desktop_file()?)?;
    for size in ICON_SIZES {
        remove_if_exists(&icon_file(size)?)?;
    }
    remove_if_exists(&mime_package()?)?;
    remove_defaults()?;
    refresh_caches()?;
    Ok(())
}

pub fn status() -> Status {
    let Ok(path) = desktop_file() else {
        return Status::NotRegistered;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Status::NotRegistered;
    };
    let prefix = format!("{PATH_KEY}=");
    let registered = text
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .map(|v| v.replace("\\n", "\n").replace("\\\\", "\\"))
        .unwrap_or_default();
    if registered.is_empty() {
        // A desktop file written by hand or by an older install.
        return Status::RegisteredElsewhere(path.to_string_lossy().into_owned());
    }
    let current = exe_path().unwrap_or_default();
    if same_file(&registered, &current) {
        Status::Registered
    } else {
        Status::RegisteredElsewhere(registered)
    }
}

fn same_file(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Registers DataVivid (if needed) and makes it the default application for
/// the data formats in [`DEFAULT_MIME`].
pub fn choose_defaults() -> io::Result<()> {
    if status() != Status::Registered {
        register()?;
    }
    // xdg-mime silently writes nothing (and still exits 0) when the config
    // folder is missing, so create it and check the result afterwards.
    if let Some(config) = config_home() {
        std::fs::create_dir_all(config)?;
    }
    let out = Command::new("xdg-mime")
        .arg("default")
        .arg(DESKTOP_ID)
        .args(DEFAULT_MIME)
        .output()
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                io::Error::other("xdg-mime was not found; install the xdg-utils package")
            } else {
                e
            }
        })?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "xdg-mime failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let check = Command::new("xdg-mime").args(["query", "default", DEFAULT_MIME[0]]).output()?;
    if String::from_utf8_lossy(&check.stdout).trim() != DESKTOP_ID {
        return Err(io::Error::other(
            "xdg-mime did not record the default; check that ~/.config/mimeapps.list is writable",
        ));
    }
    Ok(())
}

fn config_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        Some(d) => Some(PathBuf::from(d)),
        None => std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".config")),
    }
}

/// Removes `datavivid.desktop` from the user's `mimeapps.list`
/// (`[Default Applications]` and `[Added Associations]`), leaving every other
/// line untouched.
fn remove_defaults() -> io::Result<()> {
    let Some(config) = config_home() else {
        return Ok(());
    };
    let path = config.join("mimeapps.list");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let (new_text, changed) = strip_desktop_id(&text, DESKTOP_ID);
    if changed {
        write_file(&path, new_text.as_bytes())?;
    }
    Ok(())
}

fn strip_desktop_id(text: &str, id: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut in_assoc = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_assoc = trimmed == "[Default Applications]" || trimmed == "[Added Associations]";
        } else if in_assoc && !trimmed.starts_with('#') {
            if let Some((key, value)) = line.split_once('=') {
                let apps: Vec<&str> = value.split(';').filter(|a| !a.trim().is_empty()).collect();
                if apps.iter().any(|a| a.trim() == id) {
                    changed = true;
                    let kept: Vec<&str> = apps.into_iter().filter(|a| a.trim() != id).collect();
                    if !kept.is_empty() {
                        out.push_str(&format!("{key}={};\n", kept.join(";")));
                    }
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, changed)
}

/// Refreshes the caches that desktops read. The tools are optional; when one
/// is missing the desktop picks the change up on its own a little later.
fn refresh_caches() -> io::Result<()> {
    let data = data_home()?;
    let run = |cmd: &str, arg: &Path| {
        let _ = Command::new(cmd)
            .arg(arg)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    };
    run("update-desktop-database", &data.join("applications"));
    run("update-mime-database", &data.join("mime"));
    let _ = Command::new("gtk-update-icon-cache")
        .args(["-q", "-t", "-f"])
        .arg(data.join("icons/hicolor"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("datavivid-tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Encodes RGBA pixels as a PNG (8-bit RGBA, zlib via flate2).
fn png_bytes(size: u32, rgba: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut raw = Vec::with_capacity((size * (size * 4 + 1)) as usize);
    for row in rgba.chunks_exact((size * 4) as usize) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&raw).expect("zlib in memory");
    let idat = z.finish().expect("zlib in memory");

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, adaptive, no interlace

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(&b"IHDR"[..], &ihdr[..]), (b"IDAT", &idat[..]), (b"IEND", &[])] {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(data);
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc.sum().to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting() {
        assert_eq!(exec_arg("/usr/bin/datavivid"), "/usr/bin/datavivid");
        assert_eq!(exec_arg("/home/a b/datavivid"), "\"/home/a b/datavivid\"");
        assert_eq!(exec_arg("/x/$y"), "\"/x/\\\\$y\"");
        assert_eq!(exec_arg("/x/100%"), "\"/x/100%%\"");
    }

    #[test]
    fn strips_only_our_entries() {
        let text = "[Default Applications]\ntext/csv=datavivid.desktop;\n\
                    application/json=code.desktop;datavivid.desktop;\n\
                    text/plain=gedit.desktop\n[Added Associations]\n\
                    text/csv=datavivid.desktop;libreoffice-calc.desktop;\n";
        let (out, changed) = strip_desktop_id(text, DESKTOP_ID);
        assert!(changed);
        assert_eq!(
            out,
            "[Default Applications]\napplication/json=code.desktop;\n\
             text/plain=gedit.desktop\n[Added Associations]\n\
             text/csv=libreoffice-calc.desktop;\n"
        );
        assert!(!strip_desktop_id("[Default Applications]\ntext/plain=gedit.desktop\n", DESKTOP_ID).1);
    }

    #[test]
    fn mime_package_comment_is_valid_xml() {
        // `--` inside an XML comment makes update-mime-database reject the file.
        for comment in MIME_PACKAGE.split("<!--").skip(1) {
            let body = comment.split("-->").next().unwrap();
            assert!(!body.contains("--"), "double hyphen in XML comment");
        }
    }

    #[test]
    fn png_has_signature_and_chunks() {
        let png = png_bytes(16, &crate::icon_art::icon_rgba(16));
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(&png[12..16], b"IHDR");
        assert!(png.ends_with(&[0xAE, 0x42, 0x60, 0x82])); // CRC of an empty IEND
    }
}
