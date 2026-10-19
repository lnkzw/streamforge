//! Command registry: wire names, metadata, and parameter decoding.

use std::path::PathBuf;
use std::time::Duration;

use capture_api::{
    AudioCaptureRequest, CursorMode, DEFAULT_PROBE_DURATION, MAX_PROBE_DURATION,
    MIN_PROBE_DURATION, ScreenCaptureRequest, ScreenSelection, VideoSourceKind,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{Command, CommandError, CompositeRequest, ErrorCode};

/// Permission class of a command.
///
/// Adapters must require explicit authorization for anything other than
/// [`CommandAccess::ReadOnly`]; enforcement arrives with agent integration (M8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandAccess {
    /// Inspects state without changing it.
    ReadOnly,
    /// Changes engine state, e.g. starts or stops an output.
    Control,
    /// Destructive, credential-related, or privacy-sensitive (e.g. microphone access).
    Sensitive,
}

/// Static metadata for one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CommandDescriptor {
    /// Stable `snake_case` wire name.
    pub name: &'static str,
    pub summary: &'static str,
    pub access: CommandAccess,
}

const GET_STATUS: CommandDescriptor = CommandDescriptor {
    name: "get_status",
    summary: "Report engine status.",
    access: CommandAccess::ReadOnly,
};

const LIST_COMMANDS: CommandDescriptor = CommandDescriptor {
    name: "list_commands",
    summary: "List commands supported by this engine.",
    access: CommandAccess::ReadOnly,
};

const GET_CAPTURE_CAPABILITIES: CommandDescriptor = CommandDescriptor {
    name: "get_capture_capabilities",
    summary: "Query screen and audio capture capabilities of the running system.",
    access: CommandAccess::ReadOnly,
};

const LIST_AUDIO_INPUTS: CommandDescriptor = CommandDescriptor {
    name: "list_audio_inputs",
    summary: "List audio input devices.",
    access: CommandAccess::ReadOnly,
};

const PROBE_SCREEN_CAPTURE: CommandDescriptor = CommandDescriptor {
    name: "probe_screen_capture",
    summary: "Capture a user-selected screen or window and report format, timestamps, FPS and drops. \
              Params: duration_ms (100..=60000, default 5000), source_types ([\"monitor\",\"window\",\"virtual\"], \
              default [\"monitor\",\"window\"]), cursor (\"hidden\"|\"embedded\"|\"metadata\", default \"embedded\").",
    access: CommandAccess::Sensitive,
};

const PROBE_AUDIO_CAPTURE: CommandDescriptor = CommandDescriptor {
    name: "probe_audio_capture",
    summary: "Capture an audio input and report format, levels, timestamps and drops; no audio is stored. \
              Params: input (name from list_audio_inputs, default: system default), duration_ms (100..=60000, default 5000).",
    access: CommandAccess::Sensitive,
};

const PROBE_COMPOSITE: CommandDescriptor = CommandDescriptor {
    name: "probe_composite",
    summary: "Composite a 1920x1080@60 test scene (color bars, moving overlay, optional live screen) \
              and report render time, late frames and source drops. Params: duration_ms (100..=60000, \
              default 5000), screen (bool, default false; opens the system picker), source_types and \
              cursor (only with screen), snapshot_path (plain .ppm file name, written to the engine's output \
              directory, never overwritten).",
    access: CommandAccess::Sensitive,
};

const COMMANDS: &[CommandDescriptor] = &[
    GET_STATUS,
    LIST_COMMANDS,
    GET_CAPTURE_CAPABILITIES,
    LIST_AUDIO_INPUTS,
    PROBE_SCREEN_CAPTURE,
    PROBE_AUDIO_CAPTURE,
    PROBE_COMPOSITE,
];

const DEFAULT_SOURCE_KINDS: [VideoSourceKind; 2] =
    [VideoSourceKind::Monitor, VideoSourceKind::Window];
const DEFAULT_CURSOR: CursorMode = CursorMode::Embedded;

/// All commands known to this build, in a stable order.
pub fn registry() -> &'static [CommandDescriptor] {
    COMMANDS
}

/// Result of `list_commands`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandList {
    pub commands: Vec<CommandDescriptor>,
}

impl CommandList {
    pub fn all() -> Self {
        Self {
            commands: COMMANDS.to_vec(),
        }
    }
}

impl Command {
    /// Decode a command from its wire name and optional JSON parameters.
    ///
    /// Absent, `null`, and empty-object parameters are equivalent and select
    /// each parameter's default.
    pub fn decode(name: &str, params: Option<&Value>) -> Result<Self, CommandError> {
        match name {
            "get_status" => expect_no_params(name, params).map(|()| Self::GetStatus),
            "list_commands" => expect_no_params(name, params).map(|()| Self::ListCommands),
            "get_capture_capabilities" => {
                expect_no_params(name, params).map(|()| Self::GetCaptureCapabilities)
            }
            "list_audio_inputs" => expect_no_params(name, params).map(|()| Self::ListAudioInputs),
            "probe_screen_capture" => {
                decode_screen_probe(name, params).map(Self::ProbeScreenCapture)
            }
            "probe_audio_capture" => decode_audio_probe(name, params).map(Self::ProbeAudioCapture),
            "probe_composite" => decode_composite_probe(name, params).map(Self::ProbeComposite),
            _ => Err(CommandError::new(
                ErrorCode::UnknownCommand,
                format!("unknown command `{name}`"),
            )
            .with_details(json!({
                "command": name,
                "available": COMMANDS.iter().map(|d| d.name).collect::<Vec<_>>(),
            }))),
        }
    }

    pub fn descriptor(&self) -> &'static CommandDescriptor {
        match self {
            Self::GetStatus => &GET_STATUS,
            Self::ListCommands => &LIST_COMMANDS,
            Self::GetCaptureCapabilities => &GET_CAPTURE_CAPABILITIES,
            Self::ListAudioInputs => &LIST_AUDIO_INPUTS,
            Self::ProbeScreenCapture(_) => &PROBE_SCREEN_CAPTURE,
            Self::ProbeAudioCapture(_) => &PROBE_AUDIO_CAPTURE,
            Self::ProbeComposite(_) => &PROBE_COMPOSITE,
        }
    }

    pub fn name(&self) -> &'static str {
        self.descriptor().name
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScreenProbeParams {
    duration_ms: Option<u64>,
    source_types: Option<Vec<VideoSourceKind>>,
    cursor: Option<CursorMode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioProbeParams {
    input: Option<String>,
    duration_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompositeProbeParams {
    duration_ms: Option<u64>,
    screen: Option<bool>,
    source_types: Option<Vec<VideoSourceKind>>,
    cursor: Option<CursorMode>,
    snapshot_path: Option<String>,
}

fn decode_composite_probe(
    command: &str,
    params: Option<&Value>,
) -> Result<CompositeRequest, CommandError> {
    let params: CompositeProbeParams = decode_params(command, params)?;
    let screen = if params.screen.unwrap_or(false) {
        Some(screen_selection(
            command,
            params.source_types,
            params.cursor,
        )?)
    } else if params.source_types.is_some() || params.cursor.is_some() {
        return Err(invalid_params(
            command,
            "`source_types` and `cursor` require `screen: true`",
        ));
    } else {
        None
    };
    Ok(CompositeRequest {
        duration: probe_duration(command, params.duration_ms)?,
        screen,
        snapshot_path: params
            .snapshot_path
            .map(|path| snapshot_path(command, path))
            .transpose()?,
    })
}

/// Snapshots are plain `.ppm` file names. The engine writes them into its
/// own output directory and never overwrites, so a remote client cannot
/// choose where files land.
fn snapshot_path(command: &str, path: String) -> Result<PathBuf, CommandError> {
    let path = PathBuf::from(path);
    let mut components = path.components();
    let plain_name = matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    );
    let is_ppm = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ppm"));
    if !plain_name || !is_ppm || path.file_stem().is_none() {
        return Err(invalid_params(
            command,
            "`snapshot_path` must be a plain file name ending in .ppm \
             (no directories); it is written to the engine's output directory",
        ));
    }
    Ok(path)
}

fn decode_screen_probe(
    command: &str,
    params: Option<&Value>,
) -> Result<ScreenCaptureRequest, CommandError> {
    let params: ScreenProbeParams = decode_params(command, params)?;
    Ok(ScreenCaptureRequest {
        selection: screen_selection(command, params.source_types, params.cursor)?,
        duration: probe_duration(command, params.duration_ms)?,
    })
}

/// Defaults, deduplication and validation shared by all screen commands.
fn screen_selection(
    command: &str,
    source_types: Option<Vec<VideoSourceKind>>,
    cursor: Option<CursorMode>,
) -> Result<ScreenSelection, CommandError> {
    let mut source_kinds = Vec::new();
    for kind in source_types.unwrap_or_else(|| DEFAULT_SOURCE_KINDS.to_vec()) {
        if !source_kinds.contains(&kind) {
            source_kinds.push(kind);
        }
    }
    if source_kinds.is_empty() {
        return Err(invalid_params(command, "`source_types` must not be empty"));
    }
    Ok(ScreenSelection {
        source_kinds,
        cursor: cursor.unwrap_or(DEFAULT_CURSOR),
    })
}

fn decode_audio_probe(
    command: &str,
    params: Option<&Value>,
) -> Result<AudioCaptureRequest, CommandError> {
    let params: AudioProbeParams = decode_params(command, params)?;
    if params.input.as_deref().is_some_and(str::is_empty) {
        return Err(invalid_params(command, "`input` must not be empty"));
    }
    Ok(AudioCaptureRequest {
        input: params.input,
        duration: probe_duration(command, params.duration_ms)?,
    })
}

fn probe_duration(command: &str, duration_ms: Option<u64>) -> Result<Duration, CommandError> {
    let duration = duration_ms.map_or(DEFAULT_PROBE_DURATION, Duration::from_millis);
    if (MIN_PROBE_DURATION..=MAX_PROBE_DURATION).contains(&duration) {
        return Ok(duration);
    }
    Err(CommandError::new(
        ErrorCode::InvalidParams,
        format!(
            "`duration_ms` must be between {} and {}",
            MIN_PROBE_DURATION.as_millis(),
            MAX_PROBE_DURATION.as_millis()
        ),
    )
    .with_details(json!({
        "command": command,
        "min_ms": MIN_PROBE_DURATION.as_millis(),
        "max_ms": MAX_PROBE_DURATION.as_millis(),
    })))
}

fn decode_params<T: DeserializeOwned>(
    command: &str,
    params: Option<&Value>,
) -> Result<T, CommandError> {
    let object = match params {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(object @ Value::Object(_)) => object.clone(),
        Some(other) => return Err(not_an_object(command, other)),
    };
    serde_json::from_value(object)
        .map_err(|err| invalid_params(command, format!("invalid parameters: {err}")))
}

fn expect_no_params(command: &str, params: Option<&Value>) -> Result<(), CommandError> {
    match params {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(map)) if map.is_empty() => Ok(()),
        Some(Value::Object(map)) => Err(CommandError::new(
            ErrorCode::InvalidParams,
            format!("`{command}` takes no parameters"),
        )
        .with_details(json!({
            "command": command,
            "unexpected": map.keys().collect::<Vec<_>>(),
        }))),
        Some(other) => Err(not_an_object(command, other)),
    }
}

fn invalid_params(command: &str, message: impl Into<String>) -> CommandError {
    CommandError::new(ErrorCode::InvalidParams, message).with_details(json!({ "command": command }))
}

fn not_an_object(command: &str, value: &Value) -> CommandError {
    invalid_params(
        command,
        format!("`params` must be an object, got {}", json_type(value)),
    )
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn registry_names_are_unique_snake_case() {
        let mut seen = HashSet::new();
        for descriptor in registry() {
            assert!(
                seen.insert(descriptor.name),
                "duplicate {}",
                descriptor.name
            );
            assert!(
                descriptor
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "{} is not snake_case",
                descriptor.name
            );
        }
    }

    #[test]
    fn every_registered_command_decodes_to_its_descriptor() {
        for descriptor in registry() {
            let command = Command::decode(descriptor.name, None)
                .unwrap_or_else(|err| panic!("{} does not decode: {err}", descriptor.name));
            assert_eq!(command.descriptor(), descriptor);
        }
    }

    #[test]
    fn unknown_command_lists_available_commands() {
        let err = Command::decode("start_teleport", None).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnknownCommand);
        let details = err.details.expect("details");
        assert_eq!(details["command"], "start_teleport");
        let available = details["available"].as_array().expect("array");
        assert_eq!(available.len(), registry().len());
        assert!(available.contains(&json!("get_status")));
    }

    #[test]
    fn absent_null_and_empty_params_are_equivalent() {
        for params in [None, Some(&Value::Null), Some(&json!({}))] {
            assert_eq!(
                Command::decode("get_status", params),
                Ok(Command::GetStatus)
            );
        }
    }

    #[test]
    fn unexpected_params_are_rejected() {
        let err = Command::decode("get_status", Some(&json!({"verbose": true}))).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert_eq!(
            err.details.expect("details")["unexpected"],
            json!(["verbose"])
        );
    }

    #[test]
    fn non_object_params_are_rejected() {
        let err = Command::decode("list_commands", Some(&json!([1]))).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert!(err.message.contains("array"), "{}", err.message);

        let err = Command::decode("probe_screen_capture", Some(&json!("x"))).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert!(err.message.contains("string"), "{}", err.message);
    }

    #[test]
    fn screen_probe_defaults() {
        assert_eq!(
            Command::decode("probe_screen_capture", None),
            Ok(Command::ProbeScreenCapture(ScreenCaptureRequest {
                selection: ScreenSelection {
                    source_kinds: vec![VideoSourceKind::Monitor, VideoSourceKind::Window],
                    cursor: CursorMode::Embedded,
                },
                duration: Duration::from_secs(5),
            }))
        );
    }

    #[test]
    fn screen_probe_params_are_decoded_and_deduplicated() {
        let params = json!({
            "duration_ms": 1500,
            "source_types": ["window", "window", "virtual"],
            "cursor": "hidden",
        });
        assert_eq!(
            Command::decode("probe_screen_capture", Some(&params)),
            Ok(Command::ProbeScreenCapture(ScreenCaptureRequest {
                selection: ScreenSelection {
                    source_kinds: vec![VideoSourceKind::Window, VideoSourceKind::Virtual],
                    cursor: CursorMode::Hidden,
                },
                duration: Duration::from_millis(1500),
            }))
        );
    }

    #[test]
    fn invalid_screen_probe_params_are_rejected() {
        for params in [
            json!({"duration_ms": 50}),
            json!({"duration_ms": 60_001}),
            json!({"duration_ms": -1}),
            json!({"source_types": []}),
            json!({"source_types": ["printer"]}),
            json!({"cursor": "sparkly"}),
            json!({"seconds": 5}),
        ] {
            let err = Command::decode("probe_screen_capture", Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{params}");
            assert_eq!(
                err.details.expect("details")["command"],
                "probe_screen_capture"
            );
        }
    }

    #[test]
    fn duration_error_reports_bounds() {
        let err =
            Command::decode("probe_audio_capture", Some(&json!({"duration_ms": 1}))).unwrap_err();
        let details = err.details.expect("details");
        assert_eq!(details["min_ms"], 100);
        assert_eq!(details["max_ms"], 60_000);
    }

    #[test]
    fn audio_probe_params() {
        assert_eq!(
            Command::decode("probe_audio_capture", None),
            Ok(Command::ProbeAudioCapture(AudioCaptureRequest {
                input: None,
                duration: Duration::from_secs(5),
            }))
        );
        assert_eq!(
            Command::decode(
                "probe_audio_capture",
                Some(&json!({"input": "alsa_input.usb", "duration_ms": 100}))
            ),
            Ok(Command::ProbeAudioCapture(AudioCaptureRequest {
                input: Some("alsa_input.usb".into()),
                duration: Duration::from_millis(100),
            }))
        );
        let err = Command::decode("probe_audio_capture", Some(&json!({"input": ""}))).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn composite_probe_defaults_to_test_pattern_only() {
        assert_eq!(
            Command::decode("probe_composite", None),
            Ok(Command::ProbeComposite(CompositeRequest {
                duration: Duration::from_secs(5),
                screen: None,
                snapshot_path: None,
            }))
        );
    }

    #[test]
    fn composite_probe_with_screen_and_snapshot() {
        let params = json!({
            "duration_ms": 2000,
            "screen": true,
            "source_types": ["window"],
            "snapshot_path": "frame.PPM",
        });
        assert_eq!(
            Command::decode("probe_composite", Some(&params)),
            Ok(Command::ProbeComposite(CompositeRequest {
                duration: Duration::from_secs(2),
                screen: Some(ScreenSelection {
                    source_kinds: vec![VideoSourceKind::Window],
                    cursor: CursorMode::Embedded,
                }),
                snapshot_path: Some(PathBuf::from("frame.PPM")),
            }))
        );
    }

    #[test]
    fn invalid_composite_params_are_rejected() {
        for params in [
            json!({"source_types": ["monitor"]}),
            json!({"cursor": "hidden"}),
            json!({"snapshot_path": "frame.png"}),
            json!({"snapshot_path": "/tmp/frame.ppm"}),
            json!({"snapshot_path": "../frame.ppm"}),
            json!({"snapshot_path": "out/frame.ppm"}),
            json!({"snapshot_path": "./frame.ppm"}),
            json!({"snapshot_path": ".ppm"}),
            json!({"snapshot_path": ""}),
            json!({"screen": "yes"}),
            json!({"duration_ms": 10}),
        ] {
            let err = Command::decode("probe_composite", Some(&params)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "{params}");
        }
    }

    #[test]
    fn access_classes_match_risk() {
        let access = |name| registry().iter().find(|d| d.name == name).map(|d| d.access);
        assert_eq!(
            access("get_capture_capabilities"),
            Some(CommandAccess::ReadOnly)
        );
        assert_eq!(access("list_audio_inputs"), Some(CommandAccess::ReadOnly));
        assert_eq!(
            access("probe_screen_capture"),
            Some(CommandAccess::Sensitive)
        );
        assert_eq!(
            access("probe_audio_capture"),
            Some(CommandAccess::Sensitive)
        );
        assert_eq!(access("probe_composite"), Some(CommandAccess::Sensitive));
    }

    #[test]
    fn command_list_serializes_with_access_class() {
        let value = serde_json::to_value(CommandList::all()).expect("serialize");
        assert_eq!(
            value["commands"][0],
            json!({
                "name": "get_status",
                "summary": "Report engine status.",
                "access": "read_only",
            })
        );
    }
}
