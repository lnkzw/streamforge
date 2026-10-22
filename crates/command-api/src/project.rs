//! Types of the project commands (`save_project`, `load_project`,
//! `validate_project`; ADR 0015).
//!
//! A project file holds the scenes with their ids, the active scene and the
//! audio setup. It lives in the engine's project directory (`--project-dir`),
//! and commands name it by a plain file name.

use std::path::PathBuf;

use serde::Serialize;

use crate::{MixerState, SceneList};

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
    #[serde(flatten)]
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
    pub problems: Vec<ProjectProblem>,
}
