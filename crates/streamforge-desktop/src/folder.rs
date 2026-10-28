//! Open a folder in the system's file manager, so the operator can put
//! image files into the project folder (ADR 0019 has no file dialog).

use std::io;
use std::path::Path;
use std::process::Command;

/// Show `folder` in the file manager without waiting for it.
pub fn open(folder: &Path) -> io::Result<()> {
    let mut child = file_manager(folder).spawn()?;
    // Reaped in the background, so no zombie is left behind.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(target_os = "windows")]
fn file_manager(folder: &Path) -> Command {
    let mut command = Command::new("explorer");
    command.arg(folder);
    command
}

#[cfg(target_os = "macos")]
fn file_manager(folder: &Path) -> Command {
    let mut command = Command::new("open");
    command.arg(folder);
    command
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn file_manager(folder: &Path) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(folder);
    command
}
