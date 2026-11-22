// Generates the Windows icon at build time and embeds it, together with the
// version information, into datavivid.exe. Explorer uses resource icon 0
// ("datavivid.exe,0") for the file types DataVivid registers.

#[allow(dead_code)]
mod icon_art {
    include!("src/icon_art.rs");
}

fn main() {
    println!("cargo:rerun-if-changed=src/icon_art.rs");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let ico = out_dir.join("datavivid.ico");
        std::fs::write(&ico, icon_art::ico_bytes(&[16, 20, 24, 32, 40, 48, 64, 256]))
            .expect("write icon");

        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico.to_str().unwrap());
        res.set("ProductName", "DataVivid");
        res.set("FileDescription", "DataVivid - tabular data viewer and editor");
        res.set("CompanyName", "Prevail Team, FH Steyr");
        res.set("LegalCopyright", "(c) 2026 Prevail Team, FH Steyr. MIT License");
        if let Err(e) = res.compile() {
            panic!("failed to embed Windows resources: {e}");
        }
    }
}
