//! Project files on disk: read with a size limit, written atomically.
//!
//! A save writes a temporary file next to the target, flushes it to disk
//! and renames it over the target. A crash or error before the rename
//! leaves the previous file as it was; the rename itself is atomic on
//! every supported system (on Windows, `MoveFileExW` replacing the
//! target).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Largest project file read, in bytes: far more than 16 scenes of 16
/// sources need, and small enough to read in one go.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Read `file_name` in `dir`.
pub fn read(dir: &Path, file_name: &Path) -> io::Result<Vec<u8>> {
    let file = File::open(dir.join(file_name))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("larger than {MAX_FILE_BYTES} bytes"),
        ));
    }
    Ok(bytes)
}

/// A finished save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub path: PathBuf,
    /// An existing file was replaced.
    pub replaced: bool,
}

/// Write `bytes` to `file_name` in `dir`. An existing file is an
/// [`io::ErrorKind::AlreadyExists`] error unless `overwrite` is set.
pub fn write(dir: &Path, file_name: &Path, bytes: &[u8], overwrite: bool) -> io::Result<Saved> {
    write_with(dir, file_name, bytes, overwrite, |_| Ok(()))
}

/// [`write`], calling `before_rename` with the finished temporary file; a
/// test uses it to fail where a crash would.
fn write_with(
    dir: &Path,
    file_name: &Path,
    bytes: &[u8],
    overwrite: bool,
    before_rename: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<Saved> {
    let path = dir.join(file_name);
    let replaced = path.try_exists()?;
    if replaced && !overwrite {
        // Checked before writing; another program creating the file in the
        // moment until the rename would be replaced.
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the file exists",
        ));
    }
    let temp = dir.join(temp_name(file_name));
    let written = write_temp(&temp, bytes)
        .and_then(|()| before_rename(&temp))
        .and_then(|()| fs::rename(&temp, &path));
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    sync_dir(dir)?;
    Ok(Saved { path, replaced })
}

fn write_temp(temp: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(temp)?;
    file.write_all(bytes)?;
    // On disk before the rename makes it the project file.
    file.sync_all()
}

/// A hidden name next to the target, unique to this process and moment.
fn temp_name(file_name: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    PathBuf::from(format!(
        ".{}.{}-{nanos}.tmp",
        file_name.display(),
        std::process::id()
    ))
}

/// Make the rename itself durable (Unix keeps it in the directory entry).
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows cannot sync a directory like this. After a power failure right
/// after the save, the rename may be lost, but then the old file is there,
/// whole; either way one complete file remains.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("streamforge-project-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        fn entries(&self) -> Vec<String> {
            let mut names: Vec<_> = fs::read_dir(&self.0)
                .expect("list")
                .map(|entry| {
                    entry
                        .expect("entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_save_creates_refuses_to_overwrite_and_then_replaces() {
        let dir = TempDir::new("save");
        let name = Path::new("show.json");
        let first = write(&dir.0, name, b"one", false).expect("create");
        assert!(!first.replaced);
        let refused = write(&dir.0, name, b"two", false).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(&dir.0, name).expect("read"), b"one");
        let second = write(&dir.0, name, b"two", true).expect("replace");
        assert!(second.replaced);
        assert_eq!(read(&dir.0, name).expect("read"), b"two");
        // No temporary file is left behind.
        assert_eq!(dir.entries(), ["show.json"]);
    }

    #[test]
    fn a_save_that_fails_before_the_rename_keeps_the_old_file() {
        let dir = TempDir::new("crash");
        let name = Path::new("show.json");
        write(&dir.0, name, b"old", false).expect("create");
        let mut seen = None;
        let error = write_with(&dir.0, name, b"new", true, |temp| {
            // The new content is complete on disk, but not yet the file.
            seen = Some(fs::read(temp).expect("temp"));
            Err(io::Error::other("crash"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "crash");
        assert_eq!(seen.as_deref(), Some(&b"new"[..]));
        assert_eq!(read(&dir.0, name).expect("read"), b"old");
        assert_eq!(dir.entries(), ["show.json"]);
    }

    #[test]
    fn large_and_missing_files_are_errors() {
        let dir = TempDir::new("read");
        let missing = read(&dir.0, Path::new("none.json")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        let big = vec![b' '; MAX_FILE_BYTES as usize + 1];
        fs::write(dir.0.join("big.json"), &big).expect("write");
        let error = read(&dir.0, Path::new("big.json")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
