//! Injects git commit hash into build for versioning.
//! Embeds Windows executable icon (VimAndTonic YouTube channel avatar).

use std::path::Path;
use std::process::Command;

fn main() {
    // Embed icon for Windows targets
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let icon_path = Path::new(&manifest_dir).join("assets").join("icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon(icon_path.to_str().expect("icon path"));
        // Cross-compile: use explicit paths for windres/ar (Docker/Linux)
        if std::env::var("HOST").as_deref() != std::env::var("TARGET").as_deref() {
            res.set_toolkit_path("/usr/bin");
            res.set_windres_path("x86_64-w64-mingw32-windres");
            res.set_ar_path("x86_64-w64-mingw32-ar");
        }
        res.compile().expect("Failed to embed Windows icon");
    }

    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let version = format!("{}+{}", env!("CARGO_PKG_VERSION"), hash);
    println!("cargo:rustc-env=DISKCLONE_VERSION={}", version);
    println!("cargo:rustc-env=DISKCLONE_GIT_HASH={}", hash);
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
