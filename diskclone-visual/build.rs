//! Embeds Windows executable icon (VimAndTonic YouTube channel avatar).

use std::path::Path;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let icon_path = Path::new(&manifest_dir)
            .parent()
            .unwrap()
            .join("assets")
            .join("icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon(icon_path.to_str().expect("icon path"));
        if std::env::var("HOST").as_deref() != std::env::var("TARGET").as_deref() {
            res.set_toolkit_path("/usr/bin");
            res.set_windres_path("x86_64-w64-mingw32-windres");
            res.set_ar_path("x86_64-w64-mingw32-ar");
        }
        res.compile().expect("Failed to embed Windows icon");
    }
}
