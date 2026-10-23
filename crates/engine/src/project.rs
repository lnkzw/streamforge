//! The project commands (ADR 0015): the scenes and the audio setup to and
//! from a file in the project directory.

use std::io;
use std::path::Path;

use command_api::{
    CommandError, ErrorCode, ProjectLoaded, ProjectSaved, ProjectValidation, SaveProjectRequest,
    SourceKind,
};
use project_file::{Canvas, Project};
use serde_json::json;

use crate::Engine;
use crate::composite::{CANVAS_HEIGHT, CANVAS_WIDTH};

const CANVAS: Canvas = Canvas {
    width: CANVAS_WIDTH,
    height: CANVAS_HEIGHT,
};

impl Engine {
    fn project_dir(&self) -> Result<&Path, CommandError> {
        self.project_dir.as_deref().ok_or_else(|| {
            CommandError::new(
                ErrorCode::InvalidState,
                "no project directory is configured: start the engine with --project-dir",
            )
        })
    }

    /// Write the scenes, the active scene and the audio setup. Allowed while
    /// the pipeline runs: it saves the setup, not the running outputs'
    /// own sources.
    pub(crate) fn save_project(
        &self,
        request: &SaveProjectRequest,
    ) -> Result<ProjectSaved, CommandError> {
        let dir = self.project_dir()?;
        let (scenes, active) = self.scenes.snapshot();
        let project = Project {
            scenes,
            active,
            audio: self.audio.clone(),
        };
        let bytes = project_file::to_json(&project, CANVAS);
        let saved = project_file::write(dir, &request.file_name, &bytes, request.overwrite)
            .map_err(|error| io_error(&request.file_name, Access::Write, error))?;
        Ok(ProjectSaved {
            file_name: request.file_name.display().to_string(),
            path: saved.path.display().to_string(),
            replaced: saved.replaced,
            scenes: project.scenes.len(),
            bytes: bytes.len() as u64,
        })
    }

    /// Replace the scenes and the audio setup with the file's. Only while
    /// idle: captures and the mixer would otherwise run for sources that
    /// no longer exist.
    pub(crate) fn load_project(&mut self, file_name: &Path) -> Result<ProjectLoaded, CommandError> {
        let dir = self.project_dir()?;
        if self.pipeline.is_some() {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                "a project can only be loaded while the pipeline is idle; stop the preview and \
                 outputs first",
            ));
        }
        let bytes = project_file::read(dir, file_name)
            .map_err(|error| io_error(file_name, Access::Read, error))?;
        let loaded = project_file::parse(&bytes, CANVAS).map_err(|invalid| {
            let first = match invalid.problems.first() {
                Some(first) if first.path.is_empty() => first.message.clone(),
                Some(first) => format!("{}: {}", first.path, first.message),
                None => "unknown problem".to_owned(),
            };
            CommandError::new(
                ErrorCode::InvalidParams,
                format!(
                    "`{}` is not a valid project ({} problem(s); first: {first})",
                    file_name.display(),
                    invalid.problems.len(),
                ),
            )
            .with_details(json!({
                "file_name": file_name.display().to_string(),
                "problems": invalid.problems,
            }))
        })?;
        let project = loaded.project;
        let screens = project
            .scenes
            .iter()
            .flat_map(|scene| &scene.sources)
            .filter(|source| matches!(source.kind, SourceKind::Screen { .. }))
            .count();
        self.scenes.restore((project.scenes, project.active));
        self.audio = project.audio;
        let mut warnings = Vec::new();
        if let Some(version) = loaded.migrated_from {
            warnings.push(format!(
                "the file has schema version {version} and was migrated: its audio inputs and \
                 tones became channels with new ids; save it to write version {}",
                project_file::SCHEMA_VERSION
            ));
        }
        if screens > 0 {
            warnings.push(format!(
                "{screens} screen source(s) have no capture target (it is not saved): choose one \
                 with set_screen_source; only where a system picker exists (Linux) can it ask \
                 instead when the scene is rendered"
            ));
        }
        Ok(ProjectLoaded {
            file_name: file_name.display().to_string(),
            scenes: self.scenes.list(),
            mixer: self.mixer_state(),
            warnings,
        })
    }

    pub(crate) fn validate_project(
        &self,
        file_name: &Path,
    ) -> Result<ProjectValidation, CommandError> {
        let dir = self.project_dir()?;
        let bytes = project_file::read(dir, file_name)
            .map_err(|error| io_error(file_name, Access::Read, error))?;
        let (schema_version, migrated, problems) = match project_file::parse(&bytes, CANVAS) {
            Ok(loaded) => (
                Some(loaded.migrated_from.unwrap_or(project_file::SCHEMA_VERSION)),
                loaded.migrated_from.is_some(),
                Vec::new(),
            ),
            Err(invalid) => (invalid.schema_version, false, invalid.problems),
        };
        Ok(ProjectValidation {
            file_name: file_name.display().to_string(),
            valid: problems.is_empty(),
            schema_version,
            migrates: migrated,
            problems,
        })
    }
}

#[derive(Clone, Copy)]
enum Access {
    Read,
    Write,
}

fn io_error(file_name: &Path, access: Access, error: io::Error) -> CommandError {
    let name = file_name.display();
    let (code, message) = match (error.kind(), access) {
        (io::ErrorKind::NotFound, Access::Read) => (
            ErrorCode::NotFound,
            format!(
                "no project file `{name}` in the project directory (or the directory is missing)"
            ),
        ),
        (io::ErrorKind::NotFound, Access::Write) => (
            ErrorCode::NotFound,
            format!("cannot save `{name}`: the project directory does not exist"),
        ),
        (io::ErrorKind::InvalidInput, _) => (
            ErrorCode::InvalidParams,
            format!(
                "`{name}` is not a regular file (e.g. a directory, a link or a device): {error}"
            ),
        ),
        (io::ErrorKind::AlreadyExists, _) => (
            ErrorCode::InvalidParams,
            format!("`{name}` exists; pass `overwrite: true` to replace it"),
        ),
        (io::ErrorKind::PermissionDenied, _) => (
            ErrorCode::PermissionDenied,
            format!("cannot access `{name}` in the project directory: {error}"),
        ),
        (io::ErrorKind::InvalidData, _) => (
            ErrorCode::InvalidParams,
            format!("`{name}` cannot be read as a project: {error}"),
        ),
        _ => (
            ErrorCode::BackendError,
            format!("cannot access `{name}` in the project directory: {error}"),
        ),
    };
    CommandError::new(code, message).with_details(json!({ "file_name": name.to_string() }))
}
