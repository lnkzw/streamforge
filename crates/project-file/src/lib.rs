//! The StreamForge project file, schema version 4 (ADR 0015, ADR 0017, ADR 0018,
//! ADR 0019):
//! the scenes with their ids, the active scene and the audio graph, as JSON.
//!
//! The file reuses the command vocabulary instead of a second set of rules:
//! a source is the object `list_scenes` shows for it, without the screen
//! `capture_target_id`, and it is checked by the `add_source` decoder; a channel is
//! its `get_mixer` object without the level, checked by the `add_channel`
//! decoder, and the program bus by the `update_bus` decoder. This crate adds
//! what the commands cannot see: ids, unique names, references and limits
//! across the whole file. Every problem names its JSON path.
//!
//! Version 1 audio is migrated on load ([`v1`]); version 1 and 2 screen
//! fields are migrated by [`v2`], and versions 1 to 3 have no `visible`
//! and no image or camera sources ([`v3`]). Saving writes version 4.
//!
//! An image source stores its plain file name in the project directory, a
//! camera source its `camera_id` (it names a device, not private content)
//! and the mode it asks for.
//! Not stored: stream keys, file paths, and what screen sources capture
//! (machine-specific, and window titles can be private, ADR 0010).

mod store;
mod strict;
mod v1;
mod v2;
mod v3;

use std::collections::HashSet;

use command_api::{
    AudioGraphSpec, BUS_ID_PREFIX, BusSpec, ChannelInput, ChannelSpec, Command,
    DEFAULT_VIDEO_BITRATE_KBPS, MAX_SCENE_SOURCES, MAX_SCENES, MonitorProfile, NAME_RULE,
    OutputKind, OutputProfiles, PROGRAM_BUS_ID, ProgramBusSpec, ProjectProblem, RecordingProfile,
    SCENE_ID_PREFIX, SOURCE_ID_PREFIX, SceneSpec, SendSpec, SendTap, SourceKind, SourceRect,
    SourceSpec, StreamProfile, id_rule, is_valid_id, is_valid_name,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub use store::{Entry, MAX_FILE_BYTES, Saved, list, read, write};

/// The schema version this build writes. It also reads versions 1 to 3
/// and migrates them.
pub const SCHEMA_VERSION: u64 = 4;
/// Most problems reported for one file; a last one says how many more
/// there were.
pub const MAX_PROBLEMS: usize = 100;

/// What a project file holds, in the engine's command types.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub scenes: Vec<SceneSpec>,
    /// Id of the active scene.
    pub active: String,
    pub audio: AudioGraphSpec,
    /// Each output's mix bus and encoder settings.
    pub outputs: OutputProfiles,
}

/// A project read from a file.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub project: Project,
    /// The file's schema version when it was migrated; saving writes
    /// [`SCHEMA_VERSION`].
    pub migrated_from: Option<u64>,
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
    audio: &'a AudioGraphSpec,
    outputs: &'a OutputProfiles,
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
    /// Checked by its version's rules.
    audio: Value,
    /// The output profiles; absent: the defaults (version 1 has none).
    #[serde(default)]
    outputs: Option<Value>,
}

/// Each profile with every field; a profile left out is the default.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // checked for its structure
struct FileOutputs {
    #[serde(default)]
    recording: Option<FileRecordingProfile>,
    #[serde(default)]
    stream: Option<FileStreamProfile>,
    #[serde(default)]
    monitor: Option<FileMonitorProfile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // checked for its structure
struct FileRecordingProfile {
    mix: String,
    /// Absent: no further tracks.
    #[serde(default)]
    tracks: Vec<String>,
    video_encoder: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // checked for its structure
struct FileStreamProfile {
    mix: String,
    video_bitrate_kbps: Value,
    video_encoder: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // checked for its structure
struct FileMonitorProfile {
    mix: String,
    enabled: Value,
    /// Absent or `null`: the system default.
    #[serde(default)]
    device: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileScene {
    id: String,
    name: String,
    sources: Vec<Map<String, Value>>,
}

/// Version 2's audio. Read for the structure and the paths of its errors;
/// the decoders get the file's own JSON, so no number is rounded on the way
/// (a gain of 1e39 is an error, not 0 dB).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileAudio {
    channels: Vec<Value>,
    /// Aux buses; absent: none.
    #[serde(default)]
    buses: Vec<Value>,
    program: Value,
}

/// Every field explicit, except `sends`: a send that is left out is the
/// default (on to the program bus, off to an aux bus).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // checked for its structure
struct FileChannel {
    id: String,
    name: String,
    input: FileInput,
    gain_db: f32,
    mute: bool,
    balance: f32,
    #[serde(default)]
    sends: Vec<FileSend>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // checked for its structure
struct FileSend {
    bus: String,
    enabled: bool,
    level_db: f32,
    tap: SendTap,
}

/// Every field explicit, except that `assign_db` may be left out (or
/// `null`) for a separate mix.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // checked for its structure
struct FileBus {
    id: String,
    name: String,
    gain_db: f32,
    mute: bool,
    assign_db: Option<f32>,
}

/// Every field explicit, except that a device channel's `device` may be
/// left out (or `null`) for the system default input.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(dead_code)] // checked for its structure
enum FileInput {
    Device { device: Option<String> },
    Tone { frequency_hz: f64, level_dbfs: f64 },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // checked for its structure
struct FileProgram {
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
    let file = FileOut {
        schema_version: SCHEMA_VERSION,
        canvas,
        active_scene: &project.active,
        scenes,
        audio: &project.audio,
        outputs: &project.outputs,
    };
    let mut bytes = serde_json::to_vec_pretty(&file).expect("project JSON");
    bytes.push(b'\n');
    bytes
}

fn without_screen_target(source: &SourceSpec) -> SourceSpec {
    let mut source = source.clone();
    if let SourceKind::Screen {
        capture_target_id, ..
    } = &mut source.kind
    {
        *capture_target_id = None;
    }
    source
}

/// Read and check a project file for a `canvas`-sized engine; version 1 to 3
/// files are migrated.
pub fn parse(bytes: &[u8], canvas: Canvas) -> Result<Loaded, Invalid> {
    let invalid = |schema_version, path: &str, message: String| Invalid {
        schema_version,
        problems: vec![problem(path, message)],
    };
    let value = strict::parse(bytes)
        .map_err(|(path, message)| invalid(None, &path, format!("not valid JSON: {message}")))?;
    let version = match value.get("schema_version") {
        Some(Value::Number(number)) => number.as_u64(),
        _ => None,
    };
    let Some(version) = version else {
        let found = if value.is_object() {
            "`schema_version` must be a whole number (this build reads 1 to 4)"
        } else {
            "a project file is a JSON object"
        };
        return Err(invalid(None, "schema_version", found.to_owned()));
    };
    if !(1..=SCHEMA_VERSION).contains(&version) {
        let message = if version > SCHEMA_VERSION {
            format!(
                "schema version {version} is newer than this build reads \
                 ({SCHEMA_VERSION}); update StreamForge"
            )
        } else {
            format!("schema version {version} is not supported (this build reads 1 to 4)")
        };
        return Err(invalid(Some(version), "schema_version", message));
    }
    let file: FileIn = serde_path_to_error::deserialize(value).map_err(|error| {
        let path = error.path().to_string();
        let path = if path == "." { String::new() } else { path };
        invalid(Some(version), &path, error.into_inner().to_string())
    })?;
    check(file, version, canvas).map_err(|mut problems| {
        if problems.len() > MAX_PROBLEMS {
            let more = problems.len() - MAX_PROBLEMS;
            problems.truncate(MAX_PROBLEMS);
            problems.push(problem("", format!("and {more} more problem(s)")));
        }
        Invalid {
            schema_version: Some(version),
            problems,
        }
    })
}

/// The checks across the whole file, collecting every problem.
fn check(file: FileIn, version: u64, canvas: Canvas) -> Result<Loaded, Vec<ProjectProblem>> {
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
        for (position, mut object) in scene.sources.into_iter().enumerate() {
            let at = format!("{at}.sources[{position}]");
            if version <= 2 {
                problems.extend(v2::migrate(&mut object, &at));
            }
            if version <= 3 {
                problems.extend(v3::migrate(&mut object, &at));
            }
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
    let audio = if version == 1 {
        v1::migrate(file.audio)
    } else {
        audio(file.audio)
    };
    let audio = match audio {
        Ok(audio) => Some(audio),
        Err(found) => {
            problems.extend(found);
            None
        }
    };
    let outputs = match file.outputs {
        None => OutputProfiles::default(),
        // Version 1's rules are kept as they were: it had no profiles.
        Some(_) if version == 1 => {
            problems.push(problem(
                "outputs",
                "unknown field `outputs`: version 1 has no output profiles".to_owned(),
            ));
            OutputProfiles::default()
        }
        Some(outputs) => self::outputs(outputs, audio.as_ref(), &mut problems),
    };
    match audio {
        Some(audio) if problems.is_empty() => Ok(Loaded {
            project: Project {
                scenes,
                active: file.active_scene,
                audio,
                outputs,
            },
            migrated_from: (version != SCHEMA_VERSION).then_some(version),
        }),
        _ => Err(problems),
    }
}

/// One source object: an id, then exactly what `add_source` accepts, except
/// the screen `capture_target_id`.
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
    for old in ["source_types", "cursor"] {
        if object.contains_key(old) {
            problems.push(problem(
                &format!("{at}.{old}"),
                format!("unknown field `{old}`"),
            ));
        }
    }
    if object.contains_key("capture_target_id") {
        problems.push(problem(
            &format!("{at}.capture_target_id"),
            "what a screen source captures is not stored: it is machine-specific \
             (choose it after loading with set_capture_target)"
                .to_owned(),
        ));
    }
    if object.contains_key("scene_name") || object.contains_key("scene_id") {
        problems.push(problem(
            at,
            "a source's scene is the one it is listed in; remove `scene_name` and `scene_id`"
                .to_owned(),
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
        Err(error) => return Err(vec![problem(at, without_expected(error.message))]),
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
        visible: request.visible,
    })
}

/// Version 2's audio: each bus an id and what `add_bus` accepts, each
/// channel an id and what `add_channel` accepts with its sends as
/// `set_send` accepts them, the program bus what `update_bus` accepts.
/// Then the rules across them, as the engine keeps them
/// ([`AudioGraphSpec::problems`]); an entry that is wrong on its own keeps
/// its place, id and name for that, so every problem is found at once.
fn audio(audio: Value) -> Result<AudioGraphSpec, Vec<ProjectProblem>> {
    let audio: FileAudio = structure("audio", &audio).map_err(|found| vec![found])?;
    let mut problems = Vec::new();
    // Entries without a readable id and name: the graph check skips them.
    let mut unreadable = Vec::new();
    let buses = buses(audio.buses, &mut problems, &mut unreadable);
    let mut channels = Vec::with_capacity(audio.channels.len());
    for (index, value) in audio.channels.into_iter().enumerate() {
        let at = format!("audio.channels[{index}]");
        let mut placeholder = ChannelSpec {
            id: String::new(),
            name: format!("\u{0}{index}"),
            input: ChannelInput::Device { device: None },
            gain_db: 0.0,
            muted: false,
            balance: 0.0,
            sends: default_sends(&buses),
        };
        if let Err(found) = structure::<FileChannel>(&at, &value) {
            problems.push(found);
            unreadable.push(format!("channels[{index}]"));
            channels.push(placeholder);
            continue;
        }
        let Value::Object(mut object) = value else {
            unreachable!("the structure is an object");
        };
        let (Some(Value::String(id)), Some(Value::String(name))) =
            (object.remove("id"), object.get("name").cloned())
        else {
            unreachable!("the structure has a string id and name");
        };
        let sends = match object.remove("sends") {
            Some(Value::Array(sends)) => channel_sends(sends, &buses, &at, &mut problems),
            _ => default_sends(&buses),
        };
        let request = match Command::decode("add_channel", Some(&Value::Object(object))) {
            Ok(Command::AddChannel(request)) => request,
            Ok(_) => unreachable!("add_channel decodes to AddChannel"),
            Err(error) => {
                problems.push(problem(&at, error.message));
                placeholder.id = id;
                placeholder.name = name;
                channels.push(placeholder);
                continue;
            }
        };
        channels.push(ChannelSpec {
            id,
            name: request.name,
            input: request.input,
            gain_db: request.gain_db,
            muted: request.muted,
            balance: request.balance,
            sends,
        });
    }
    let program = match structure::<FileProgram>("audio.program", &audio.program) {
        Ok(_) => {
            let Value::Object(mut params) = audio.program else {
                unreachable!("the structure is an object");
            };
            params.insert("bus_id".to_owned(), json!(PROGRAM_BUS_ID));
            match Command::decode("update_bus", Some(&Value::Object(params))) {
                Ok(Command::UpdateBus(request)) => Some(ProgramBusSpec {
                    gain_db: request.gain_db.unwrap_or(0.0),
                    muted: request.muted.unwrap_or(false),
                }),
                Ok(_) => unreachable!("update_bus decodes to UpdateBus"),
                Err(error) => {
                    problems.push(problem("audio.program", error.message));
                    None
                }
            }
        }
        Err(found) => {
            problems.push(found);
            None
        }
    };
    let graph = AudioGraphSpec {
        channels,
        buses,
        program: program.unwrap_or(ProgramBusSpec {
            gain_db: 0.0,
            muted: false,
        }),
    };
    problems.extend(
        graph
            .problems()
            .into_iter()
            .filter(|found| {
                !unreadable.iter().any(|entry| {
                    found
                        .path
                        .strip_prefix(entry.as_str())
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
                })
            })
            .map(|found| problem(&format!("audio.{}", found.path), found.message)),
    );
    if problems.is_empty() {
        Ok(graph)
    } else {
        Err(problems)
    }
}

/// The aux buses, in file order. A bus that is wrong on its own keeps its
/// place, id and name (or is listed in `unreadable` without them).
fn buses(
    values: Vec<Value>,
    problems: &mut Vec<ProjectProblem>,
    unreadable: &mut Vec<String>,
) -> Vec<BusSpec> {
    let mut buses = Vec::with_capacity(values.len());
    for (index, value) in values.into_iter().enumerate() {
        let at = format!("audio.buses[{index}]");
        let mut placeholder = BusSpec {
            id: value
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            name: format!("\u{0}{index}"),
            gain_db: 0.0,
            muted: false,
            assign_db: None,
        };
        if let Err(found) = structure::<FileBus>(&at, &value) {
            problems.push(found);
            unreadable.push(format!("buses[{index}]"));
            buses.push(placeholder);
            continue;
        }
        let Value::Object(mut object) = value else {
            unreachable!("the structure is an object");
        };
        let (Some(Value::String(id)), Some(Value::String(name))) =
            (object.remove("id"), object.get("name").cloned())
        else {
            unreachable!("the structure has a string id and name");
        };
        let request = match Command::decode("add_bus", Some(&Value::Object(object))) {
            Ok(Command::AddBus(request)) => request,
            Ok(_) => unreachable!("add_bus decodes to AddBus"),
            Err(error) => {
                problems.push(problem(&at, error.message));
                placeholder.id = id;
                placeholder.name = name;
                buses.push(placeholder);
                continue;
            }
        };
        buses.push(BusSpec {
            id,
            name: request.name,
            gain_db: request.gain_db,
            muted: request.muted,
            assign_db: request.assign_db,
        });
    }
    buses
}

/// A new channel's sends: on to program, off to every aux bus.
fn default_sends(buses: &[BusSpec]) -> Vec<SendSpec> {
    std::iter::once(SendSpec::program())
        .chain(buses.iter().map(|bus| SendSpec::off(&bus.id)))
        .collect()
}

/// A channel's sends, one per bus in bus order: the file's, or the
/// default for a bus it leaves out (or gives wrongly, with a problem).
fn channel_sends(
    values: Vec<Value>,
    buses: &[BusSpec],
    at: &str,
    problems: &mut Vec<ProjectProblem>,
) -> Vec<SendSpec> {
    let mut sends = default_sends(buses);
    let mut given = vec![false; sends.len()];
    for (index, value) in values.into_iter().enumerate() {
        let at = format!("{at}.sends[{index}]");
        let Value::Object(mut params) = value else {
            unreachable!("the structure is an object");
        };
        let Some(Value::String(bus)) = params.remove("bus") else {
            unreachable!("the structure has a string bus");
        };
        let Some(column) = sends.iter().position(|send| send.bus == bus) else {
            problems.push(problem(
                &format!("{at}.bus"),
                format!("no bus has the id `{bus}`"),
            ));
            continue;
        };
        if given[column] {
            problems.push(problem(
                &format!("{at}.bus"),
                format!("this channel's send to `{bus}` is given twice"),
            ));
            continue;
        }
        given[column] = true;
        // Any valid ids will do: the decoder only checks their form.
        params.insert("channel_id".to_owned(), json!("ch_file"));
        params.insert(
            "bus_id".to_owned(),
            json!(BUS_ID_PREFIX.to_owned() + "file"),
        );
        match Command::decode("set_send", Some(&Value::Object(params))) {
            Ok(Command::SetSend(request)) => {
                sends[column] = SendSpec {
                    bus,
                    enabled: request.enabled.unwrap_or(false),
                    level_db: request.level_db.unwrap_or(0.0),
                    tap: request.tap.unwrap_or(SendTap::PostFader),
                };
            }
            Ok(_) => unreachable!("set_send decodes to SetSend"),
            Err(error) => problems.push(problem(&at, error.message)),
        }
    }
    sends
}

/// The output profiles: each checked by the `set_output_profile` rules
/// (the monitor's device by `set_monitor_device`'s), with its mix naming a
/// bus of `audio` (when the audio is valid).
fn outputs(
    value: Value,
    audio: Option<&AudioGraphSpec>,
    problems: &mut Vec<ProjectProblem>,
) -> OutputProfiles {
    let mut profiles = OutputProfiles::default();
    if let Err(found) = structure::<FileOutputs>("outputs", &value) {
        problems.push(found);
        return profiles;
    }
    let Value::Object(mut outputs) = value else {
        unreachable!("the structure is an object");
    };
    let mut decoded = Vec::new();
    for kind in OutputKind::ALL {
        let output = kind.as_str();
        let Some(Value::Object(mut params)) = outputs.remove(output) else {
            continue;
        };
        let at = format!("outputs.{output}");
        let Some(Value::String(mix)) = params.remove("mix") else {
            unreachable!("the structure has a string mix");
        };
        let device = params.remove("device");
        params.insert("output".to_owned(), json!(output));
        params.insert("bus_id".to_owned(), json!(mix));
        // A file names track buses by id; the request takes `{bus_id}`.
        if let Some(Value::Array(tracks)) = params.get_mut("tracks") {
            let mut invalid = false;
            for (index, bus) in tracks.iter_mut().enumerate() {
                if !bus
                    .as_str()
                    .is_some_and(|id| is_valid_id(BUS_ID_PREFIX, id))
                {
                    problems.push(problem(
                        &format!("{at}.tracks[{index}]"),
                        "a track names its bus by id".to_owned(),
                    ));
                    invalid = true;
                }
                *bus = json!({ "bus_id": bus.take() });
            }
            if invalid {
                continue;
            }
        }
        match Command::decode("set_output_profile", Some(&Value::Object(params))) {
            Ok(Command::SetOutputProfile(request)) => {
                decoded.push(kind);
                let encoder = request.video_encoder.unwrap_or_default();
                match kind {
                    OutputKind::Recording => {
                        profiles.recording = RecordingProfile {
                            mix,
                            // Every entry is `{bus_id}` (checked above).
                            tracks: request
                                .tracks
                                .unwrap_or_default()
                                .into_iter()
                                .filter_map(|bus| bus.id)
                                .collect(),
                            video_encoder: encoder,
                        };
                    }
                    OutputKind::Stream => {
                        profiles.stream = StreamProfile {
                            mix,
                            video_bitrate_kbps: request
                                .video_bitrate_kbps
                                .unwrap_or(DEFAULT_VIDEO_BITRATE_KBPS),
                            video_encoder: encoder,
                        };
                    }
                    OutputKind::Monitor => {
                        let params = json!({ "device": device.unwrap_or(Value::Null) });
                        let device = match Command::decode("set_monitor_device", Some(&params)) {
                            Ok(Command::SetMonitorDevice { device }) => device,
                            Ok(_) => unreachable!("set_monitor_device decodes to SetMonitorDevice"),
                            Err(error) => {
                                problems.push(problem(&format!("{at}.device"), error.message));
                                None
                            }
                        };
                        profiles.monitor = MonitorProfile {
                            mix,
                            enabled: request.enabled.unwrap_or(false),
                            device,
                        };
                    }
                }
            }
            Ok(_) => unreachable!("set_output_profile decodes to SetOutputProfile"),
            Err(error) => problems.push(problem(&at, without_expected(error.message))),
        }
    }
    // Then the rules across the profiles and the graph, as the engine
    // keeps them, for the profiles that decoded (with valid audio).
    if let Some(audio) = audio {
        problems.extend(
            profiles
                .problems(audio)
                .into_iter()
                .filter(|found| {
                    decoded
                        .iter()
                        .any(|kind| found.path.starts_with(&format!("{}.", kind.as_str())))
                })
                .map(|found| problem(&format!("outputs.{}", found.path), found.message)),
        );
    }
    profiles
}

/// `value` as `T`, or a structure problem with its path below `at`.
pub(crate) fn structure<T: DeserializeOwned>(at: &str, value: &Value) -> Result<T, ProjectProblem> {
    serde_path_to_error::deserialize(value).map_err(|error| {
        let path = error.path().to_string();
        let path = if path == "." {
            at.to_owned()
        } else {
            format!("{at}.{path}")
        };
        problem(&path, error.into_inner().to_string())
    })
}

/// serde's unknown-field message lists the command's fields, among them
/// `scene_id`, `scene_name` and `capture_target_id`, which a file must not
/// use; keep the first part.
fn without_expected(message: String) -> String {
    match message.find(", expected one of") {
        Some(end) if message.contains("unknown field") => message[..end].to_owned(),
        _ => message,
    }
}

pub(crate) fn problem(path: &str, message: String) -> ProjectProblem {
    ProjectProblem {
        path: path.to_owned(),
        message,
    }
}

#[cfg(test)]
mod tests;
