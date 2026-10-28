//! Types of `list_images` (ADR 0019): the image files an image source can
//! show, in the engine's project directory.

use serde::Serialize;

/// Most files `list_images` returns; the rest are counted in
/// [`ImageFileList::omitted`].
pub const MAX_LISTED_IMAGES: usize = 256;

/// Result of `list_images`: the PNG and JPEG files in the project
/// directory, by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageFileList {
    pub files: Vec<ImageFileInfo>,
    /// Files past [`MAX_LISTED_IMAGES`], not listed.
    pub omitted: usize,
}

/// One image file. Only its header is read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageFileInfo {
    pub file_name: String,
    pub bytes: u64,
    /// When it was last written, in milliseconds since the Unix epoch, if
    /// the system says.
    pub modified_unix_ms: Option<u64>,
    /// Its size as shown (a JPEG's orientation applied), from the header;
    /// `None` if the header cannot be read.
    pub width: Option<u32>,
    pub height: Option<u32>,
}
