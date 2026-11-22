//! Windows file-type registration for the current user (no admin rights).
//!
//! Windows 10/11 do not let programs set themselves as the default opener
//! (the per-user `UserChoice` key is hash-protected). What a program can and
//! should do is register as a *candidate*:
//!
//! * a ProgID per format under `HKCU\Software\Classes\DataVivid.<Fmt>` with an
//!   icon and an `open` command,
//! * the ProgID under `HKCU\Software\Classes\.<ext>\OpenWithProgids`, so it
//!   appears in Explorer's "Open with" menu,
//! * `HKCU\Software\Classes\Applications\datavivid.exe` with `SupportedTypes`,
//! * Default Programs capabilities (`HKCU\Software\DataVivid\Capabilities`)
//!   listed in `HKCU\Software\RegisteredApplications`, so DataVivid shows up in
//!   Settings > Apps > Default apps.
//!
//! The user then picks DataVivid as the default, either in "Open with >
//! Choose another app > Always" or in the Default apps page that
//! [`open_default_apps_settings`] opens.

use crate::formats::Format;

pub const APP_NAME: &str = "DataVivid";
const EXE_NAME: &str = "datavivid.exe";
const CAPABILITIES: &str = r"Software\DataVivid\Capabilities";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    NotRegistered,
    /// Registered and pointing at this executable.
    Registered,
    /// Registered, but for an executable at another path.
    RegisteredElsewhere(String),
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::io;
    use winreg::RegKey;
    use winreg::enums::*;

    fn classes() -> io::Result<RegKey> {
        RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(r"Software\Classes", KEY_ALL_ACCESS)
    }

    fn prog_id(f: Format) -> String {
        format!("{APP_NAME}.{}", f.key())
    }

    fn exe_path() -> io::Result<String> {
        Ok(std::env::current_exe()?.to_string_lossy().into_owned())
    }

    pub fn register() -> io::Result<()> {
        let exe = exe_path()?;
        let open_cmd = format!("\"{exe}\" \"%1\"");
        let icon = format!("\"{exe}\",0");
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let classes = classes()?;

        // 1. One ProgID per format.
        for f in Format::ALL {
            let (pk, _) = classes.create_subkey(prog_id(f))?;
            pk.set_value("", &format!("{} ({APP_NAME})", f.label()))?;
            pk.set_value("FriendlyTypeName", &f.label())?;
            let (ik, _) = pk.create_subkey("DefaultIcon")?;
            ik.set_value("", &icon)?;
            let (ck, _) = pk.create_subkey(r"shell\open\command")?;
            ck.set_value("", &open_cmd)?;
            let (sk, _) = pk.create_subkey(r"shell\open")?;
            sk.set_value("FriendlyAppName", &APP_NAME)?;

            // 2. Advertise the ProgID for each extension ("Open with" list).
            for ext in f.extensions() {
                let (ek, _) = classes.create_subkey(format!(r".{ext}\OpenWithProgids"))?;
                ek.set_value(prog_id(f), &"")?;
            }
        }

        // 3. Applications\datavivid.exe
        let (ak, _) = classes.create_subkey(format!(r"Applications\{EXE_NAME}"))?;
        ak.set_value("FriendlyAppName", &APP_NAME)?;
        let (aik, _) = ak.create_subkey("DefaultIcon")?;
        aik.set_value("", &icon)?;
        let (ack, _) = ak.create_subkey(r"shell\open\command")?;
        ack.set_value("", &open_cmd)?;
        let (stk, _) = ak.create_subkey("SupportedTypes")?;
        for f in Format::ALL {
            for ext in f.extensions() {
                stk.set_value(format!(".{ext}"), &"")?;
            }
        }

        // 4. Default Programs capabilities.
        let (cap, _) = hkcu.create_subkey(CAPABILITIES)?;
        cap.set_value("ApplicationName", &APP_NAME)?;
        cap.set_value(
            "ApplicationDescription",
            &"View, clean and convert CSV, Parquet, Excel, JSON, Arrow, Avro and SQLite data.",
        )?;
        cap.set_value("ApplicationIcon", &icon)?;
        let (fa, _) = cap.create_subkey("FileAssociations")?;
        for f in Format::ALL {
            for ext in f.extensions() {
                fa.set_value(format!(".{ext}"), &prog_id(f))?;
            }
        }
        let (app_key, _) = hkcu.create_subkey(r"Software\DataVivid")?;
        app_key.set_value("InstallPath", &exe)?;
        let (ra, _) = hkcu.create_subkey(r"Software\RegisteredApplications")?;
        ra.set_value(APP_NAME, &CAPABILITIES)?;

        notify_shell();
        Ok(())
    }

    pub fn unregister() -> io::Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let classes = classes()?;
        for f in Format::ALL {
            let _ = classes.delete_subkey_all(prog_id(f));
            for ext in f.extensions() {
                if let Ok(ek) = classes
                    .open_subkey_with_flags(format!(r".{ext}\OpenWithProgids"), KEY_ALL_ACCESS)
                {
                    let _ = ek.delete_value(prog_id(f));
                }
            }
        }
        let _ = classes.delete_subkey_all(format!(r"Applications\{EXE_NAME}"));
        if let Ok(ra) =
            hkcu.open_subkey_with_flags(r"Software\RegisteredApplications", KEY_ALL_ACCESS)
        {
            let _ = ra.delete_value(APP_NAME);
        }
        let _ = hkcu.delete_subkey_all(r"Software\DataVivid");
        notify_shell();
        Ok(())
    }

    pub fn status() -> Status {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let Ok(k) = hkcu.open_subkey(r"Software\DataVivid") else {
            return Status::NotRegistered;
        };
        if hkcu.open_subkey(CAPABILITIES).is_err() {
            return Status::NotRegistered;
        }
        let registered: String = k.get_value("InstallPath").unwrap_or_default();
        let current = exe_path().unwrap_or_default();
        if registered.eq_ignore_ascii_case(&current) {
            Status::Registered
        } else {
            Status::RegisteredElsewhere(registered)
        }
    }

    fn notify_shell() {
        use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
        unsafe {
            SHChangeNotify(
                SHCNE_ASSOCCHANGED as _,
                SHCNF_IDLIST as _,
                std::ptr::null(),
                std::ptr::null(),
            );
        }
    }

    pub fn open_default_apps_settings() -> io::Result<()> {
        // Windows 11 jumps straight to DataVivid's page; older builds ignore
        // the query and show the Default apps overview.
        open_uri("ms-settings:defaultapps?registeredAppUser=DataVivid")
    }

    fn open_uri(uri: &str) -> io::Result<()> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/C", "start", "", uri])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    use std::io;

    fn unsupported() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "file registration is only implemented on Windows")
    }
    pub fn register() -> io::Result<()> {
        Err(unsupported())
    }
    pub fn unregister() -> io::Result<()> {
        Err(unsupported())
    }
    pub fn status() -> Status {
        Status::NotRegistered
    }
    pub fn open_default_apps_settings() -> io::Result<()> {
        Err(unsupported())
    }
}

pub use imp::{open_default_apps_settings, register, status, unregister};

/// If DataVivid is registered for an executable that no longer exists (it was
/// moved or deleted), point the registration at this executable. A second,
/// still-existing copy (e.g. a development build) never takes over.
pub fn refresh_if_moved() {
    if let Status::RegisteredElsewhere(old) = status() {
        if !std::path::Path::new(&old).exists() {
            let _ = register();
        }
    }
}
