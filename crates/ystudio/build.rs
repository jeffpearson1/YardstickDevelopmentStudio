//! Embeds the Yardstick icon and version information in the Windows executable,
//! so Explorer, shortcuts and the taskbar show the brand mark.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/branding/yardstick.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../assets/branding/yardstick.ico")
        .set("ProductName", "Yardstick Development Studio")
        .set("FileDescription", "Yardstick Development Studio");
    if let Err(e) = res.compile() {
        // A missing resource compiler shouldn't block a build; the window icon is set at runtime too.
        println!("cargo:warning=could not embed the app icon: {e}");
    }
}
