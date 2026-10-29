//! Windows: embeds the app icon and version information into
//! `streamforge-desktop.exe`, for Explorer, the Start menu and Task Manager
//! (ADR 0020). Nothing happens on other systems, nor when building for
//! Windows on another system (winresource needs Windows' resource
//! compiler).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon/windows.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        // FileVersion and ProductVersion come from the package version.
        resource
            .set_icon("assets/icon/windows.ico")
            .set("ProductName", "StreamForge")
            // What Task Manager lists the process as.
            .set("FileDescription", "StreamForge")
            .set("InternalName", "streamforge-desktop")
            .set("OriginalFilename", "streamforge-desktop.exe");
        if let Err(err) = resource.compile() {
            panic!("cannot embed the Windows resources: {err}");
        }
    }
}
