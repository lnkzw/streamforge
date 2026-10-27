//! How the desktop names itself to the system: the application id and the
//! window icon. On Wayland the compositor matches the window's app id to
//! the launcher of the same name (`packaging/linux/<APP_ID>.desktop`) and
//! shows that launcher's icon; X11, Windows and macOS take the window
//! icon.

use eframe::egui::IconData;

/// Reverse-DNS application id: the Wayland app id, the X11 window class,
/// and the base name of the launcher and its icon.
pub const APP_ID: &str = "io.github.lnkzw.StreamForge";

/// `assets/icon/window-256.png`, rendered from the icon's SVG source
/// beside it (the command is in the SVG).
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../assets/icon/window-256.png");

/// The window icon, decoded from the embedded PNG.
pub fn window_icon() -> IconData {
    // The PNG is part of the binary and checked by a test, so this cannot
    // fail at runtime; an empty icon would only fall back to eframe's.
    eframe::icon_data::from_png_bytes(WINDOW_ICON_PNG).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_icon_decodes_to_256_square_pixels() {
        let icon = eframe::icon_data::from_png_bytes(WINDOW_ICON_PNG).expect("decodes");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        assert_eq!(window_icon(), icon);
    }

    #[test]
    fn the_launcher_and_its_icon_carry_the_app_id() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
        let launcher = std::fs::read_to_string(format!("{root}/packaging/linux/{APP_ID}.desktop"))
            .expect("launcher named after the app id");
        for line in [
            format!("Icon={APP_ID}"),
            format!("StartupWMClass={APP_ID}"),
            "Exec=streamforge-desktop".to_owned(),
        ] {
            assert!(
                launcher.lines().any(|l| l == line),
                "the launcher has no line {line:?}"
            );
        }
        let svg = format!("{}/assets/icon/{APP_ID}.svg", env!("CARGO_MANIFEST_DIR"));
        assert!(std::path::Path::new(&svg).is_file(), "no icon at {svg}");
    }
}
