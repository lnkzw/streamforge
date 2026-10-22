//! The StreamForge project file, schema version 1 (ADR 0015): the scenes
//! with their ids, the active scene and the audio setup, as JSON.
//!
//! The file reuses the command vocabulary instead of a second set of rules:
//! a source is the object `list_scenes` shows for it, without the screen
//! `source_id`, and it is checked by the `add_source` decoder; the audio is
//! checked by the `set_audio_sources` decoder. This crate adds what the
//! commands cannot see: ids, unique names, references and limits across
//! the whole file. Every problem names its JSON path.
//!
//! Not stored: stream keys, file paths, and what screen sources capture
//! (machine-specific, and window titles can be private, ADR 0010).

mod store;

use std::collections::HashSet;

use command_api::{
    AudioSetup, Command, MAX_ID_LEN, MAX_SCENE_SOURCES, MAX_SCENES, NAME_RULE, ProjectProblem,
    SCENE_ID_PREFIX, SOURCE_ID_PREFIX, SceneSpec, SourceKind, SourceRect, SourceSpec, is_valid_id,
    is_valid_name,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub use store::{MAX_FILE_BYTES, Saved, read, write};

/// The schema version this build writes and reads.
pub const SCHEMA_VERSION: u64 = 1;

/// What a project file holds, in the engine's command types.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub scenes: Vec<SceneSpec>,
    /// Id of the active scene.
    pub active: String,
    pub audio: AudioSetup,
}

/// The canvas the source rectangles are in, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
}

/// A file that cannot be loaded, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    /// The file's `schema_version`, if it could be read.
    pub schema_version: Option<u64>,
    pub problems: Vec<ProjectProblem>,
}

/// The file as written; scene and source objects are the command types'.
#[derive(Serialize)]
struct FileOut<'a> {
    schema_version: u64,
    canvas: Canvas,
    active_scene: &'a str,
    scenes: Vec<SceneSpec>,
    audio: FileAudio,
}

/// The file as read: sources stay JSON objects for the `add_source` decoder.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileIn {
    #[allow(dead_code)] // read and checked before the rest
    schema_version: u64,
    canvas: Canvas,
    active_scene: String,
    scenes: Vec<FileScene>,
    audio: FileAudio,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileScene {
    id: String,
    name: String,
    sources: Vec<Map<String, Value>>,
}

/// `set_audio_sources` parameters, every field explicit.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileAudio {
    inputs: Vec<FileInput>,
    tones: Vec<FileTone>,
    master_gain_db: f32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileInput {
    /// `null`: the system default input.
    input: Option<String>,
    gain_db: f32,
    mute: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileTone {
    frequency_hz: f64,
    level_dbfs: f64,
    gain_db: f32,
    mute: bool,
}

/// The file's bytes: pretty-printed JSON with a final newline, without the
/// screen sources' capture targets.
pub fn to_json(project: &Project, canvas: Canvas) -> Vec<u8> {
    let scenes = project
        .scenes
        .iter()
        .map(|scene| SceneSpec {
            sources: scene.sources.iter().map(without_screen_target).collect(),
            ..scene.clone()
        })
        .collect();
    let audio = &project.audio;
    let file = FileOut {
        schema_version: SCHEMA_VERSION,
        canvas,
        active_scene: &project.active,
        scenes,
        audio: FileAudio {
            inputs: audio
                .inputs
                .iter()
                .map(|input| FileInput {
                    input: input.input.clone(),
                    gain_db: input.gain_db,
                    mute: input.muted,
                })
                .collect(),
            tones: audio
                .tones
                .iter()
                .map(|tone| FileTone {
                    frequency_hz: tone.frequency_hz,
                    level_dbfs: tone.level_dbfs,
                    gain_db: tone.gain_db,
                    mute: tone.muted,
                })
                .collect(),
            master_gain_db: audio.master_gain_db,
        },
    };
    let mut bytes = serde_json::to_vec_pretty(&file).expect("project JSON");
    bytes.push(b'\n');
    bytes
}

fn without_screen_target(source: &SourceSpec) -> SourceSpec {
    let mut source = source.clone();
    if let SourceKind::Screen { source_id, .. } = &mut source.kind {
        *source_id = None;
    }
    source
}

/// Read and check a project file for a `canvas`-sized engine.
pub fn parse(bytes: &[u8], canvas: Canvas) -> Result<Project, Invalid> {
    let invalid = |schema_version, path: &str, message: String| Invalid {
        schema_version,
        problems: vec![problem(path, message)],
    };
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| invalid(None, "", format!("not valid JSON: {error}")))?;
    let version = match value.get("schema_version") {
        Some(Value::Number(number)) => number.as_u64(),
        _ => None,
    };
    let Some(version) = version else {
        let found = if value.is_object() {
            "`schema_version` must be a whole number (this build reads 1)"
        } else {
            "a project file is a JSON object"
        };
        return Err(invalid(None, "schema_version", found.to_owned()));
    };
    if version != SCHEMA_VERSION {
        let message = if version > SCHEMA_VERSION {
            format!(
                "schema version {version} is newer than this build reads \
                 ({SCHEMA_VERSION}); update StreamForge"
            )
        } else {
            format!("schema version {version} is not supported (this build reads {SCHEMA_VERSION})")
        };
        return Err(invalid(Some(version), "schema_version", message));
    }
    let file: FileIn = serde_path_to_error::deserialize(value).map_err(|error| {
        let path = error.path().to_string();
        let path = if path == "." { String::new() } else { path };
        invalid(Some(version), &path, error.into_inner().to_string())
    })?;
    check(file, canvas).map_err(|problems| Invalid {
        schema_version: Some(version),
        problems,
    })
}

/// The checks across the whole file, collecting every problem.
fn check(file: FileIn, canvas: Canvas) -> Result<Project, Vec<ProjectProblem>> {
    let mut problems = Vec::new();
    if file.canvas != canvas {
        problems.push(problem(
            "canvas",
            format!(
                "the canvas is {}x{}, but this engine renders {}x{}",
                file.canvas.width, file.canvas.height, canvas.width, canvas.height
            ),
        ));
    }
    if file.scenes.is_empty() || file.scenes.len() > MAX_SCENES {
        problems.push(problem(
            "scenes",
            format!("1 to {MAX_SCENES} scenes, got {}", file.scenes.len()),
        ));
    }
    let mut scene_ids = HashSet::new();
    let mut scene_names = HashSet::new();
    let mut source_ids = HashSet::new();
    let mut scenes = Vec::with_capacity(file.scenes.len());
    for (index, scene) in file.scenes.into_iter().enumerate() {
        let at = format!("scenes[{index}]");
        if !is_valid_id(SCENE_ID_PREFIX, &scene.id) {
            problems.push(problem(&format!("{at}.id"), id_rule(SCENE_ID_PREFIX)));
        } else if !scene_ids.insert(scene.id.clone()) {
            problems.push(problem(
                &format!("{at}.id"),
                format!("scene id `{}` is used twice", scene.id),
            ));
        }
        if !is_valid_name(&scene.name) {
            problems.push(problem(
                &format!("{at}.name"),
                format!("a name {NAME_RULE}"),
            ));
        } else if !scene_names.insert(scene.name.clone()) {
            problems.push(problem(
                &format!("{at}.name"),
                format!("scene name `{}` is used twice", scene.name),
            ));
        }
        if scene.sources.len() > MAX_SCENE_SOURCES {
            problems.push(problem(
                &format!("{at}.sources"),
                format!(
                    "at most {MAX_SCENE_SOURCES} sources per scene, got {}",
                    scene.sources.len()
                ),
            ));
        }
        let mut names = HashSet::new();
        let mut sources = Vec::with_capacity(scene.sources.len());
        for (position, object) in scene.sources.into_iter().enumerate() {
            let at = format!("{at}.sources[{position}]");
            match source(object, &at, canvas, &mut source_ids) {
                Ok(spec) => {
                    if !names.insert(spec.name.clone()) {
                        problems.push(problem(
                            &format!("{at}.name"),
                            format!("source name `{}` is used twice in this scene", spec.name),
                        ));
                    }
                    sources.push(spec);
                }
                Err(found) => problems.extend(found),
            }
        }
        scenes.push(SceneSpec {
            id: scene.id,
            name: scene.name,
            sources,
        });
    }
    if !scenes.iter().any(|scene| scene.id == file.active_scene) {
        problems.push(problem(
            "active_scene",
            format!("no scene has the id `{}`", file.active_scene),
        ));
    }
    let audio = audio(&file.audio).map_err(|message| problem("audio", message));
    let audio = match audio {
        Ok(audio) => Some(audio),
        Err(found) => {
            problems.push(found);
            None
        }
    };
    match audio {
        Some(audio) if problems.is_empty() => Ok(Project {
            scenes,
            active: file.active_scene,
            audio,
        }),
        _ => Err(problems),
    }
}

/// One source object: an id, then exactly what `add_source` accepts, except
/// the screen `source_id`.
fn source(
    mut object: Map<String, Value>,
    at: &str,
    canvas: Canvas,
    ids: &mut HashSet<String>,
) -> Result<SourceSpec, Vec<ProjectProblem>> {
    let mut problems = Vec::new();
    let id = match object.remove("id") {
        Some(Value::String(id)) if is_valid_id(SOURCE_ID_PREFIX, &id) => {
            if !ids.insert(id.clone()) {
                problems.push(problem(
                    &format!("{at}.id"),
                    format!("source id `{id}` is used twice (ids are unique across scenes)"),
                ));
            }
            id
        }
        _ => {
            problems.push(problem(&format!("{at}.id"), id_rule(SOURCE_ID_PREFIX)));
            String::new()
        }
    };
    if object.contains_key("source_id") {
        problems.push(problem(
            &format!("{at}.source_id"),
            "what a screen source captures is not stored: it is machine-specific \
             (choose it after loading with set_screen_source)"
                .to_owned(),
        ));
    }
    if object.contains_key("scene") || object.contains_key("scene_id") {
        problems.push(problem(
            at,
            "a source's scene is the one it is listed in; remove `scene` and `scene_id`".to_owned(),
        ));
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    // Any valid scene id will do: the decoder only checks its form.
    object.insert("scene_id".to_owned(), json!("scene_file"));
    let request = match Command::decode("add_source", Some(&Value::Object(object))) {
        Ok(Command::AddSource(request)) => request,
        Ok(_) => unreachable!("add_source decodes to AddSource"),
        Err(error) => return Err(vec![problem(at, error.message)]),
    };
    Ok(SourceSpec {
        id,
        name: request.name,
        kind: request.kind,
        rect: request.rect.unwrap_or(SourceRect {
            x: 0.0,
            y: 0.0,
            width: canvas.width as f32,
            height: canvas.height as f32,
        }),
        opacity: request.opacity,
    })
}

fn audio(audio: &FileAudio) -> Result<AudioSetup, String> {
    let params = serde_json::to_value(audio).expect("audio JSON");
    match Command::decode("set_audio_sources", Some(&params)) {
        Ok(Command::SetAudioSources(setup)) => Ok(setup),
        Ok(_) => unreachable!("set_audio_sources decodes to SetAudioSources"),
        Err(error) => Err(error.message),
    }
}

fn id_rule(prefix: &str) -> String {
    format!("an id must be `{prefix}`, then `a-z`, `0-9` or `_` (at most {MAX_ID_LEN} bytes)")
}

fn problem(path: &str, message: String) -> ProjectProblem {
    ProjectProblem {
        path: path.to_owned(),
        message,
    }
}

#[cfg(test)]
mod tests;
