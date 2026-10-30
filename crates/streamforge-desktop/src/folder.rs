//! Folders: where recordings and projects go by default, and opening a
//! folder in the system's file manager, so the operator can put image
//! files into the project folder (ADR 0019 has no file dialog).

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The folder for recordings and projects without `--output-dir` or
/// `--project-dir`: the working directory, or the home folder when the
/// working directory is the file system's root. An app opened from
/// macOS's Finder or Dock starts in `/`, where it cannot write (ADR 0020).
pub fn default_dir(working_dir: Option<&Path>, home: Option<&Path>) -> PathBuf {
    match (working_dir, home) {
        (Some(dir), Some(home)) if dir.parent().is_none() && !home.as_os_str().is_empty() => {
            home.to_path_buf()
        }
        _ => PathBuf::from("."),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_folder_is_the_working_directory() {
        let home = Path::new("/Users/me");
        for dir in ["/Users/me/shows", "/tmp", "/Applications"] {
            assert_eq!(
                default_dir(Some(Path::new(dir)), Some(home)),
                Path::new(".")
            );
        }
        // An unknown working directory is still `.`.
        assert_eq!(default_dir(None, Some(home)), Path::new("."));
    }

    #[test]
    fn the_root_as_working_directory_means_the_home_folder() {
        let home = Path::new("/Users/me");
        assert_eq!(default_dir(Some(Path::new("/")), Some(home)), home);
        #[cfg(windows)]
        assert_eq!(default_dir(Some(Path::new(r"C:\")), Some(home)), home);
        // Without a (usable) home folder, it stays the working directory.
        assert_eq!(default_dir(Some(Path::new("/")), None), Path::new("."));
        assert_eq!(
            default_dir(Some(Path::new("/")), Some(Path::new(""))),
            Path::new(".")
        );
    }
}
