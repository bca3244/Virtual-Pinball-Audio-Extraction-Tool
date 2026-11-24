//! Embeds the app icon and version details in both exes (Windows only).

fn main() {
    println!("cargo:rerun-if-changed=assets/vp-audio-extract.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/vp-audio-extract.ico")
            .set("ProductName", "VP Audio Extract")
            .set(
                "FileDescription",
                "VP Audio Extract - records table music into PinUP Popper's Audio media",
            )
            .set("LegalCopyright", "MIT OR Apache-2.0");
        if let Err(e) = res.compile() {
            // A missing resource compiler shouldn't stop the build; the exe just has no icon.
            println!("cargo:warning=could not embed the icon: {e}");
        }
    }
}
