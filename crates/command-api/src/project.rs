//! Types of the project commands (`list_projects`, `save_project`,
//! `load_project`, `validate_project`; ADR 0015).
//!
//! A project file holds the scenes with their ids, the active scene and the
//! audio graph. It lives in the engine's project directory (`--project-dir`),
//! and commands name it by a plain file name.

use std::path::PathBuf;

use serde::{Serialize, Serializer};

use crate::{MixerState, SceneList};

/// Most files `list_projects` returns; the rest are counted in
/// [`ProjectFileList::omitted`].
pub const MAX_LISTED_PROJECTS: usize = 256;

/// Result of `list_projects`: the project files in the project directory,
/// by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectFileList {
    pub files: Vec<ProjectFileInfo>,
    /// Files past [`MAX_LISTED_PROJECTS`], not listed.
    pub omitted: usize,
}

/// One project file. It is not read: `validate_project` checks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectFileInfo {
    pub file_name: String,
    pub bytes: u64,
    /// When it was last written, in milliseconds since the Unix epoch, if
    /// the system says.
    pub modified_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveProjectRequest {
    /// Plain `.json` file name in the project directory.
    pub file_name: PathBuf,
    /// Replace an existing file (atomically); without it, an existing file
    /// is refused.
    pub overwrite: bool,
}

/// Result of `save_project`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectSaved {
    pub file_name: String,
    pub path: String,
    /// The file existed and was replaced.
    pub replaced: bool,
    pub scenes: usize,
    pub bytes: u64,
}

/// Result of `load_project`: the loaded scenes and audio.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectLoaded {
    pub file_name: String,
    /// Without its `warnings`: this result has its own, and a project loads
    /// only while idle, so loading starts no capture to warn about.
    #[serde(flatten, serialize_with = "scenes_without_warnings")]
    pub scenes: SceneList,
    pub mixer: MixerState,
    /// E.g. screen sources whose capture target has to be chosen again.
    pub warnings: Vec<String>,
}

/// One problem in a project file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectProblem {
    /// Where in the file: a JSON path such as `scenes[1].sources[0]`, or
    /// empty for the file as a whole.
    pub path: String,
    pub message: String,
}

/// Result of `validate_project`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectValidation {
    pub file_name: String,
    pub valid: bool,
    /// The file's `schema_version`, if it could be read.
    pub schema_version: Option<u64>,
    /// A valid file of an older version: loading migrates it, and saving
    /// writes the current version.
    pub migrates: bool,
    pub problems: Vec<ProjectProblem>,
}

/// The scene list's fields for [`ProjectLoaded`], whose `warnings` is the
/// only one in the result.
fn scenes_without_warnings<S: Serializer>(
    scenes: &SceneList,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    SceneList {
        warnings: Vec::new(),
        ..scenes.clone()
    }
    .serialize(serializer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loaded_project_has_one_warnings_field() {
        let loaded = ProjectLoaded {
            file_name: "show.json".to_owned(),
            scenes: SceneList {
                canvas_width: 1920,
                canvas_height: 1080,
                active_scene_name: "Show".to_owned(),
                active_scene_id: "scene_show".to_owned(),
                scenes: Vec::new(),
                // A scene list's own warnings stay out of this result.
                warnings: vec!["camera".to_owned()],
            },
            mixer: MixerState {
                running: false,
                channels: Vec::new(),
                buses: Vec::new(),
                outputs: Vec::new(),
                warnings: Vec::new(),
            },
            warnings: vec!["migrated".to_owned()],
        };
        let text = serde_json::to_string(&loaded).expect("serialize");
        // `serde_json::Value` would keep one of two duplicate keys, so count
        // them in the text: the result's and the mixer's.
        assert_eq!(text.matches("\"warnings\"").count(), 2, "{text}");
        assert!(text.contains("\"warnings\":[\"migrated\"]"), "{text}");
    }
}
