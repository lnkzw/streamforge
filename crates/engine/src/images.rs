//! Image sources' frames (ADR 0019): PNG and JPEG files in the project
//! directory, decoded on the engine thread when a source is added, a
//! project loads, or `restart_source` reads the file again.
//!
//! One frame per file name: every source showing a file shows the same
//! frame, and an unchanged file (same modification time and length) is not
//! decoded again. All frames together stay within [`MAX_TOTAL_BYTES`].

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use command_api::{
    CommandError, ErrorCode, ImageFileInfo, ImageFileList, MAX_LISTED_IMAGES, SourceRect,
};
use frame_decode::{DecodeError, Limits};
use media_core::VideoFrame;
use serde_json::json;

use crate::Engine;
use crate::composite::{CANVAS_HEIGHT, CANVAS_WIDTH};

/// Most bytes all decoded images take together (as BGRA).
pub(crate) const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// Largest image file read: more than a maximal image needs, even as an
/// uncompressed 16-bit PNG.
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// How much of a file `list_images` reads for its size: a PNG's header is
/// in the first bytes, a JPEG's may follow an EXIF thumbnail.
const HEADER_PREFIXES: [u64; 2] = [4 * 1024, 256 * 1024];

/// What makes two reads of a file the same file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    modified: Option<SystemTime>,
    bytes: u64,
}

impl Identity {
    /// Without a modification time, a file is never taken as unchanged.
    fn unchanged(self, before: Identity) -> bool {
        self.modified.is_some() && self == before
    }
}

#[derive(Debug)]
pub(crate) struct Entry {
    identity: Identity,
    frame: Arc<VideoFrame>,
}

/// The frame a read replaced: put it back if the command fails later.
#[derive(Debug)]
pub(crate) struct Replaced {
    file: String,
    previous: Option<Entry>,
}

#[derive(Debug)]
pub(crate) struct ImageStore {
    entries: HashMap<String, Entry>,
    max_total_bytes: u64,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            max_total_bytes: MAX_TOTAL_BYTES,
        }
    }
}

impl ImageStore {
    /// A store with a smaller total, for tests.
    #[cfg(test)]
    pub fn with_max_total_bytes(max_total_bytes: u64) -> Self {
        Self {
            entries: HashMap::new(),
            max_total_bytes,
        }
    }

    pub fn frame(&self, file: &str) -> Option<&Arc<VideoFrame>> {
        self.entries.get(file).map(|entry| &entry.frame)
    }

    /// Read `file` in `dir` into the store, unless it holds the same file
    /// already and `reread` is not set. Returns the replaced frame when the
    /// store changed. On error the store is unchanged.
    pub fn read(
        &mut self,
        dir: &Path,
        file: &str,
        reread: bool,
    ) -> Result<Option<Replaced>, CommandError> {
        let path = dir.join(file);
        let metadata = fs::metadata(&path).map_err(|error| io_error(file, &error))?;
        if !metadata.is_file() {
            return Err(image_error(
                ErrorCode::InvalidParams,
                file,
                format!("`{file}` is not a regular file (e.g. a directory or a device)"),
            ));
        }
        let identity = Identity {
            modified: metadata.modified().ok(),
            bytes: metadata.len(),
        };
        if !reread
            && self
                .entries
                .get(file)
                .is_some_and(|entry| identity.unchanged(entry.identity))
        {
            return Ok(None);
        }
        if identity.bytes > MAX_FILE_BYTES {
            return Err(image_error(
                ErrorCode::InvalidParams,
                file,
                format!(
                    "`{file}` has {} bytes; image files are read up to {MAX_FILE_BYTES} bytes",
                    identity.bytes
                ),
            ));
        }
        let bytes = read_up_to(&path, MAX_FILE_BYTES).map_err(|error| io_error(file, &error))?;
        let info = frame_decode::probe(&bytes).map_err(|error| decode_error(file, error))?;
        let needed = u64::from(info.width) * u64::from(info.height) * 4;
        let others: u64 = self
            .entries
            .iter()
            .filter(|(name, _)| name.as_str() != file)
            .map(|(_, entry)| frame_bytes(&entry.frame))
            .sum();
        if others + needed > self.max_total_bytes {
            return Err(image_error(
                ErrorCode::InvalidParams,
                file,
                format!(
                    "`{file}` ({}×{} pixels, {needed} bytes decoded) does not fit: the images \
                     shown take {others} of at most {} bytes; remove an image source or use a \
                     smaller image",
                    info.width, info.height, self.max_total_bytes
                ),
            )
            .with_details(json!({
                "file": file,
                "bytes_needed": needed,
                "bytes_used": others,
                "max_total_bytes": self.max_total_bytes,
            })));
        }
        let frame = frame_decode::decode_image(&bytes, Limits::IMAGE)
            .map_err(|error| decode_error(file, error))?;
        let previous = self.entries.insert(
            file.to_owned(),
            Entry {
                identity,
                frame: Arc::new(frame),
            },
        );
        Ok(Some(Replaced {
            file: file.to_owned(),
            previous,
        }))
    }

    /// Undo a [`read`](Self::read).
    pub fn put_back(&mut self, replaced: Replaced) {
        match replaced.previous {
            Some(previous) => {
                self.entries.insert(replaced.file, previous);
            }
            None => {
                self.entries.remove(&replaced.file);
            }
        }
    }

    /// Drop the frames of files that no source shows.
    pub fn retain(&mut self, shown: &HashSet<&str>) {
        self.entries.retain(|file, _| shown.contains(file.as_str()));
    }

    /// Where an image goes without a rectangle: its own size at the top
    /// left, scaled down to the canvas if larger.
    pub fn natural_rect(&self, file: &str) -> Option<SourceRect> {
        let frame = self.frame(file)?;
        let (width, height) = (frame.width() as f32, frame.height() as f32);
        let scale = (CANVAS_WIDTH as f32 / width)
            .min(CANVAS_HEIGHT as f32 / height)
            .min(1.0);
        Some(SourceRect {
            x: 0.0,
            y: 0.0,
            width: width * scale,
            height: height * scale,
        })
    }
}

fn frame_bytes(frame: &VideoFrame) -> u64 {
    u64::from(frame.width()) * u64::from(frame.height()) * 4
}

/// The image files in `dir` that an image source could name, by name, at
/// most [`MAX_LISTED_IMAGES`]; only their headers are read.
pub(crate) fn list(dir: &Path) -> io::Result<ImageFileList> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        if command_api::image_file_name("add_source", &file_name).is_err() {
            continue;
        }
        // Followed, as a read follows it; one that vanished meanwhile, or a
        // broken link, is not a file to list.
        let Ok(metadata) = fs::metadata(entry.path()) else {
            continue;
        };
        if metadata.is_file() {
            files.push((file_name, metadata));
        }
    }
    files.sort_by(|(a, _), (b, _)| a.cmp(b));
    let omitted = files.len().saturating_sub(MAX_LISTED_IMAGES);
    files.truncate(MAX_LISTED_IMAGES);
    let files = files
        .into_iter()
        .map(|(file_name, metadata)| {
            let size = header_size(&dir.join(&file_name), metadata.len());
            ImageFileInfo {
                bytes: metadata.len(),
                modified_unix_ms: metadata
                    .modified()
                    .ok()
                    .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                    .and_then(|since| u64::try_from(since.as_millis()).ok()),
                width: size.map(|(width, _)| width),
                height: size.map(|(_, height)| height),
                file_name,
            }
        })
        .collect();
    Ok(ImageFileList { files, omitted })
}

/// The size an image file's header gives, from as little of it as needed.
fn header_size(path: &Path, len: u64) -> Option<(u32, u32)> {
    for prefix in HEADER_PREFIXES {
        let bytes = read_up_to(path, prefix).ok()?;
        if let Ok(info) = frame_decode::probe(&bytes) {
            return Some((info.width, info.height));
        }
        if len <= prefix {
            return None;
        }
    }
    None
}

fn read_up_to(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn image_error(code: ErrorCode, file: &str, message: String) -> CommandError {
    CommandError::new(code, message).with_details(json!({ "file": file }))
}

fn io_error(file: &str, error: &io::Error) -> CommandError {
    let (code, message) = match error.kind() {
        io::ErrorKind::NotFound => (
            ErrorCode::NotFound,
            format!(
                "no image file `{file}` in the project directory; list_images lists the files \
                 there"
            ),
        ),
        io::ErrorKind::PermissionDenied => (
            ErrorCode::PermissionDenied,
            format!("cannot read `{file}` in the project directory: {error}"),
        ),
        _ => (
            ErrorCode::BackendError,
            format!("cannot read `{file}` in the project directory: {error}"),
        ),
    };
    image_error(code, file, message)
}

fn decode_error(file: &str, error: DecodeError) -> CommandError {
    let message = format!("`{file}` cannot be shown: {error}");
    match error {
        DecodeError::TooLarge {
            width,
            height,
            limits,
        } => CommandError::new(ErrorCode::InvalidParams, message).with_details(json!({
            "file": file,
            "width": width,
            "height": height,
            "max_side": limits.max_side,
            "max_pixels": limits.max_pixels,
        })),
        DecodeError::Unsupported(_) | DecodeError::Invalid(_) => {
            image_error(ErrorCode::InvalidParams, file, message)
        }
    }
}

impl Engine {
    fn image_dir(&self) -> Result<&Path, CommandError> {
        self.project_dir.as_deref().ok_or_else(|| {
            CommandError::new(
                ErrorCode::InvalidState,
                "image files live in the project directory, and none is configured: start the \
                 engine with --project-dir",
            )
        })
    }

    /// The image files an image source can show.
    pub(crate) fn list_images(&self) -> Result<ImageFileList, CommandError> {
        let dir = self.image_dir()?;
        list(dir).map_err(|error| {
            let code = match error.kind() {
                io::ErrorKind::NotFound => ErrorCode::NotFound,
                io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
                _ => ErrorCode::BackendError,
            };
            CommandError::new(
                code,
                format!(
                    "cannot list the project directory {}: {error}",
                    dir.display()
                ),
            )
        })
    }

    /// Read image `file` into the store (see [`ImageStore::read`]).
    pub(crate) fn read_image(
        &mut self,
        file: &str,
        reread: bool,
    ) -> Result<Option<Replaced>, CommandError> {
        let dir = self.image_dir()?.to_owned();
        self.images.read(&dir, file, reread)
    }

    /// Read every image the scenes show, after a project loaded; returns a
    /// warning for each file that cannot be shown (its sources draw
    /// nothing). Frames of files no source shows are dropped.
    pub(crate) fn read_scene_images(&mut self) -> Vec<String> {
        let mut files: Vec<String> = self
            .scenes
            .image_files()
            .into_iter()
            .map(str::to_owned)
            .collect();
        files.sort();
        // Before reading, so frames of the previous scenes do not count
        // against the total.
        self.images.retain(&self.scenes.image_files());
        let mut warnings = Vec::new();
        for file in files {
            if let Err(error) = self.read_image(&file, false) {
                let names = self.scenes.sources_showing(&file).join("`, `");
                warnings.push(format!(
                    "image source(s) `{names}` draw nothing: {} (restart_source reads the file \
                     again)",
                    error.message
                ));
            }
        }
        warnings
    }
}
