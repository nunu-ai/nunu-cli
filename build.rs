#[cfg(windows)]
fn main() {
    let mut res = winres::WindowsResource::new();

    // Add icon if it exists
    if std::path::Path::new("icon.ico").exists() {
        res.set_icon("icon.ico");
    }

    // Get version from Cargo.toml
    let version = env!("CARGO_PKG_VERSION");

    res.set("ProductName", "Nunu CLI")
        .set("FileDescription", "Build artifact upload tool for Nunu.ai")
        .set("CompanyName", "Nunu.ai")
        .set("LegalCopyright", "Copyright (C) 2025 Nunu.ai")
        .set("OriginalFilename", "nunu-cli.exe")
        .set("FileVersion", version)
        .set("ProductVersion", version);

    if let Err(e) = res.compile() {
        eprintln!("Warning: Failed to set Windows resource metadata: {}", e);
    }
}

#[cfg(not(windows))]
fn main() {
    // No-op for non-Windows builds
}
