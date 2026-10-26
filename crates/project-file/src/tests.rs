use capture_api::{CursorMode, VideoSourceKind};
use command_api::{
    ChannelInput, EncoderPreference, MAX_AUX_BUSES, MAX_CHANNELS, Rgba, SendSpec, SendTap,
};

use super::*;

const CANVAS: Canvas = Canvas {
    width: 1920,
    height: 1080,
};

fn rect(x: f32, y: f32, width: f32, height: f32) -> SourceRect {
    SourceRect {
        x,
        y,
        width,
        height,
    }
}

fn project() -> Project {
    Project {
        scenes: vec![
            SceneSpec {
                id: "scene_test_pattern".to_owned(),
                name: "Test pattern".to_owned(),
                sources: vec![
                    SourceSpec {
                        id: "src_color_bars".to_owned(),
                        name: "color_bars".to_owned(),
                        kind: SourceKind::ColorBars,
                        rect: rect(0.0, 0.0, 1920.0, 1080.0),
                        opacity: 1.0,
                    },
                    SourceSpec {
                        id: "src_overlay".to_owned(),
                        name: "overlay".to_owned(),
                        kind: SourceKind::Solid {
                            color: Rgba([153, 153, 153, 153]),
                        },
                        rect: rect(64.0, 120.0, 160.0, 160.0),
                        opacity: 0.6,
                    },
                ],
            },
            SceneSpec {
                id: "scene_k3j9x2m4q8w1".to_owned(),
                name: "Show".to_owned(),
                sources: vec![SourceSpec {
                    id: "src_a1b2c3d4e5f6".to_owned(),
                    name: "desk".to_owned(),
                    kind: SourceKind::Screen {
                        target_kinds: vec![VideoSourceKind::Window],
                        cursor_mode: CursorMode::Hidden,
                        capture_target_id: None,
                    },
                    rect: rect(-10.5, 20.25, 960.0, 540.0),
                    opacity: 0.9,
                }],
            },
        ],
        active: "scene_k3j9x2m4q8w1".to_owned(),
        audio: AudioGraphSpec {
            channels: vec![
                ChannelSpec {
                    id: "ch_4k2m9x0q1w7e".to_owned(),
                    name: "Mic".to_owned(),
                    input: ChannelInput::Device {
                        device: Some("alsa_input.usb-mic".to_owned()),
                    },
                    gain_db: -3.5,
                    muted: true,
                    balance: -0.25,
                    sends: vec![
                        SendSpec {
                            enabled: false,
                            ..SendSpec::program()
                        },
                        SendSpec {
                            bus: "bus_voice00000a".to_owned(),
                            enabled: true,
                            level_db: -6.0,
                            tap: SendTap::PreFader,
                        },
                    ],
                },
                ChannelSpec {
                    id: "ch_test_tone".to_owned(),
                    name: "Tone".to_owned(),
                    input: ChannelInput::Tone {
                        frequency_hz: 440.0,
                        level_dbfs: -20.0,
                    },
                    gain_db: 0.0,
                    muted: false,
                    balance: 0.0,
                    sends: vec![SendSpec::program(), SendSpec::off("bus_voice00000a")],
                },
            ],
            buses: vec![BusSpec {
                id: "bus_voice00000a".to_owned(),
                name: "Voice".to_owned(),
                gain_db: -2.0,
                muted: false,
                assign_db: Some(0.0),
            }],
            program: ProgramBusSpec {
                gain_db: -1.0,
                muted: false,
            },
        },
        outputs: OutputProfiles {
            recording: RecordingProfile {
                mix: "bus_voice00000a".to_owned(),
                tracks: vec!["bus_program".to_owned()],
                video_encoder: EncoderPreference::Software,
            },
            monitor: MonitorProfile {
                mix: "bus_voice00000a".to_owned(),
                enabled: true,
                device: Some("alsa_output.usb-headset".to_owned()),
            },
            ..OutputProfiles::default()
        },
    }
}

/// `project()` as it reads back from its file.
fn loaded() -> Loaded {
    Loaded {
        project: project(),
        migrated_from: None,
    }
}

fn old_screen_fields(value: &mut Value) {
    for scene in value["scenes"].as_array_mut().unwrap() {
        for source in scene["sources"].as_array_mut().unwrap() {
            if source["kind"] == "screen" {
                let object = source.as_object_mut().unwrap();
                let kinds = object.remove("target_kinds").unwrap();
                let cursor = object.remove("cursor_mode").unwrap();
                object.insert("source_types".into(), kinds);
                object.insert("cursor".into(), cursor);
            }
        }
    }
}

/// `project()` as a version 1 file: its audio as `set_audio_sources`
/// parameters, as version 1 stored it.
fn v1_file() -> Value {
    let mut value = file();
    value["schema_version"] = json!(1);
    old_screen_fields(&mut value);
    // Version 1 had no output profiles.
    value.as_object_mut().expect("file").remove("outputs");
    value["audio"] = json!({
        "inputs": [{"input": "alsa_input.usb-mic", "gain_db": -3.5, "mute": true}],
        "tones": [{"frequency_hz": 440.0, "level_dbfs": -20.0, "gain_db": 0.0, "mute": false}],
        "master_gain_db": -1.0
    });
    value
}

fn file() -> Value {
    serde_json::from_slice(&to_json(&project(), CANVAS)).expect("json")
}

/// The problems of `value` as (path, message) pairs.
fn problems(value: &Value) -> Vec<(String, String)> {
    let bytes = serde_json::to_vec(value).expect("bytes");
    let invalid = parse(&bytes, CANVAS).expect_err("invalid");
    invalid
        .problems
        .into_iter()
        .map(|problem| (problem.path, problem.message))
        .collect()
}

/// The only problem's path, checking that its message contains `text`.
fn only(value: &Value, text: &str) -> String {
    let found = problems(value);
    assert_eq!(found.len(), 1, "{found:?}");
    let (path, message) = &found[0];
    assert!(message.contains(text), "{message} lacks {text}");
    path.clone()
}

#[test]
fn a_saved_project_loads_back_unchanged() {
    let original = project();
    let bytes = to_json(&original, CANVAS);
    assert!(bytes.ends_with(b"}\n"));
    assert_eq!(parse(&bytes, CANVAS), Ok(loaded()));
}

#[test]
fn screen_capture_targets_are_not_saved() {
    let mut original = project();
    let SourceKind::Screen {
        capture_target_id, ..
    } = &mut original.scenes[1].sources[0].kind
    else {
        panic!("screen source");
    };
    *capture_target_id = Some("window:7:0x2a".to_owned());
    let bytes = to_json(&original, CANVAS);
    let text = String::from_utf8(bytes.clone()).expect("utf-8");
    assert!(
        !text.contains("capture_target_id") && !text.contains("window:7"),
        "{text}"
    );
    // Everything else comes back.
    let found = parse(&bytes, CANVAS).expect("valid");
    assert_eq!(found, loaded());
}

#[test]
fn the_file_reads_like_the_command_results() {
    let value = file();
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["canvas"], json!({"width": 1920, "height": 1080}));
    assert_eq!(value["active_scene"], "scene_k3j9x2m4q8w1");
    // A source is its `list_scenes` object.
    assert_eq!(
        value["scenes"][0]["sources"][1],
        json!({"id": "src_overlay", "name": "overlay", "kind": "solid", "color": "#99999999",
               "x": 64.0, "y": 120.0, "width": 160.0, "height": 160.0, "opacity": 0.6})
    );
    // A channel is its `get_mixer` object without the level.
    assert_eq!(
        value["audio"]["channels"][0],
        json!({"id": "ch_4k2m9x0q1w7e", "name": "Mic",
        "input": {"kind": "device", "device": "alsa_input.usb-mic"},
        "gain_db": -3.5, "mute": true, "balance": -0.25,
        "sends": [
            {"bus": "bus_program", "enabled": false, "level_db": 0.0, "tap": "post_fader"},
            {"bus": "bus_voice00000a", "enabled": true, "level_db": -6.0, "tap": "pre_fader"}
        ]})
    );
    assert_eq!(
        value["outputs"],
        json!({"recording": {"mix": "bus_voice00000a", "tracks": ["bus_program"],
                             "video_encoder": "software"},
               "stream": {"mix": "bus_program", "video_bitrate_kbps": 6000,
                          "video_encoder": "auto"},
               "monitor": {"mix": "bus_voice00000a", "enabled": true,
                           "device": "alsa_output.usb-headset"}})
    );
    assert_eq!(
        value["audio"]["buses"],
        json!([{"id": "bus_voice00000a", "name": "Voice", "gain_db": -2.0, "mute": false,
                "assign_db": 0.0}])
    );
    assert_eq!(
        value["audio"]["program"],
        json!({"gain_db": -1.0, "mute": false})
    );
}

#[test]
fn a_hand_written_source_may_leave_out_its_rectangle_and_opacity() {
    let mut value = file();
    value["scenes"][0]["sources"][0] =
        json!({"id": "src_bars", "name": "bars", "kind": "color_bars"});
    let bytes = serde_json::to_vec(&value).expect("bytes");
    let loaded = parse(&bytes, CANVAS).expect("valid");
    let bars = &loaded.project.scenes[0].sources[0];
    assert_eq!(bars.rect, rect(0.0, 0.0, 1920.0, 1080.0));
    assert_eq!(bars.opacity, 1.0);
}

#[test]
fn unreadable_files_and_other_versions_are_refused() {
    let invalid = parse(b"{\"schema_version\": 1,", CANVAS).unwrap_err();
    assert_eq!(invalid.problems[0].path, "");
    assert!(invalid.problems[0].message.starts_with("not valid JSON"));
    assert_eq!(only(&json!([1, 2]), "JSON object"), "schema_version");
    assert_eq!(
        only(&json!({"scenes": []}), "whole number"),
        "schema_version"
    );
    let mut value = file();
    value["schema_version"] = json!(4);
    assert_eq!(
        only(&value, "newer than this build reads"),
        "schema_version"
    );
    let invalid = parse(&serde_json::to_vec(&value).expect("bytes"), CANVAS).unwrap_err();
    assert_eq!(invalid.schema_version, Some(4));
    value["schema_version"] = json!(0);
    assert_eq!(only(&value, "not supported"), "schema_version");
}

#[test]
fn structure_errors_name_their_path() {
    let mut value = file();
    value["extra"] = json!(true);
    assert_eq!(only(&value, "unknown field `extra`"), "extra");
    let mut value = file();
    value["scenes"][1]["name"] = json!(7);
    assert_eq!(only(&value, "invalid type"), "scenes[1].name");
    let mut value = file();
    value["audio"]["channels"][1]["mute"] = json!("no");
    assert_eq!(only(&value, "invalid type"), "audio.channels[1].mute");
    let mut value = file();
    value["audio"]["channels"][0]
        .as_object_mut()
        .expect("channel")
        .remove("balance");
    assert_eq!(only(&value, "missing field `balance`"), "audio.channels[0]");
    let mut value = file();
    value["audio"]["program"]["solo"] = json!(true);
    assert_eq!(only(&value, "unknown field `solo`"), "audio.program.solo");
    // Every input field is given too, but a device may be left out (the
    // default input).
    let mut value = file();
    value["audio"]["channels"][1]["input"]
        .as_object_mut()
        .expect("input")
        .remove("level_dbfs");
    assert_eq!(
        only(&value, "missing field `level_dbfs`"),
        "audio.channels[1].input"
    );
    let mut value = file();
    value["audio"]["channels"][1]["input"]["kind"] = json!("file");
    assert_eq!(
        only(&value, "unknown variant"),
        "audio.channels[1].input.kind"
    );
    let mut value = file();
    value["audio"]["channels"][0]["input"] = json!({"kind": "device"});
    let bytes = serde_json::to_vec(&value).expect("bytes");
    let loaded = parse(&bytes, CANVAS).expect("valid");
    assert_eq!(
        loaded.project.audio.channels[0].input,
        ChannelInput::Device { device: None }
    );
    let mut value = file();
    value["scenes"][0]
        .as_object_mut()
        .expect("scene")
        .remove("sources");
    assert_eq!(only(&value, "missing field `sources`"), "scenes[0]");
}

#[test]
fn the_canvas_and_the_scene_count_must_fit() {
    let mut value = file();
    value["canvas"] = json!({"width": 1280, "height": 720});
    assert_eq!(only(&value, "this engine renders 1920x1080"), "canvas");
    let mut value = file();
    value["scenes"] = json!([]);
    let found = problems(&value);
    assert_eq!(found[0].0, "scenes", "{found:?}");
    // With no scenes, the active scene names none either.
    assert_eq!(found[1].0, "active_scene", "{found:?}");
    let mut value = file();
    let scene = value["scenes"][1].clone();
    let scenes: Vec<_> = (0..MAX_SCENES)
        .map(|index| {
            let mut scene = scene.clone();
            scene["id"] = json!(format!("scene_{index}"));
            scene["name"] = json!(format!("Scene {index}"));
            scene["sources"] = json!([]);
            scene
        })
        .collect();
    value["scenes"]
        .as_array_mut()
        .expect("scenes")
        .extend(scenes);
    let found = problems(&value);
    assert_eq!(found[0].0, "scenes", "{found:?}");
}

#[test]
fn scene_ids_and_names_must_be_valid_and_unique() {
    let mut value = file();
    value["scenes"][1]["id"] = json!("src_k3j9x2m4q8w1");
    let found = problems(&value);
    assert_eq!(found[0].0, "scenes[1].id", "{found:?}");
    let mut value = file();
    value["scenes"][1]["id"] = json!("scene_test_pattern");
    value["active_scene"] = json!("scene_test_pattern");
    assert_eq!(only(&value, "used twice"), "scenes[1].id");
    let mut value = file();
    value["scenes"][1]["name"] = json!(" padded ");
    assert_eq!(only(&value, "surrounding whitespace"), "scenes[1].name");
    let mut value = file();
    value["scenes"][1]["name"] = json!("Test pattern");
    assert_eq!(only(&value, "used twice"), "scenes[1].name");
    let mut value = file();
    value["active_scene"] = json!("scene_gone");
    assert_eq!(only(&value, "no scene has the id"), "active_scene");
}

#[test]
fn source_problems_name_the_source() {
    let at = "scenes[0].sources[1]";
    let mut value = file();
    value["scenes"][0]["sources"][1]["opacity"] = json!(2);
    assert_eq!(only(&value, "`opacity` must be between 0 and 1"), at);
    let mut value = file();
    value["scenes"][0]["sources"][1]["kind"] = json!("video");
    assert_eq!(only(&value, "`kind` must be"), at);
    let mut value = file();
    value["scenes"][0]["sources"][1]["color"] = json!("red");
    assert_eq!(only(&value, "`color`"), at);
    let mut value = file();
    value["scenes"][0]["sources"][1]["width"] = json!(0);
    assert_eq!(only(&value, "`width`"), at);
    let mut value = file();
    value["scenes"][0]["sources"][1]["blur"] = json!(3);
    assert_eq!(only(&value, "unknown field `blur`"), at);
    // Without the command's field list, which names fields a file refuses.
    assert!(!problems(&value)[0].1.contains("capture_target_id"));
    let mut value = file();
    value["scenes"][0]["sources"][1]["name"] = json!("color_bars");
    assert_eq!(
        only(&value, "used twice in this scene"),
        "scenes[0].sources[1].name"
    );
    let mut value = file();
    value["scenes"][0]["sources"][1]["scene_name"] = json!("Show");
    assert_eq!(only(&value, "the one it is listed in"), at);
    let mut value = file();
    value["scenes"][1]["sources"][0]["capture_target_id"] = json!("monitor:1");
    assert_eq!(
        only(&value, "not stored"),
        "scenes[1].sources[0].capture_target_id"
    );
}

#[test]
fn source_ids_must_be_valid_and_unique_across_scenes() {
    let mut value = file();
    value["scenes"][0]["sources"][1]
        .as_object_mut()
        .expect("source")
        .remove("id");
    assert_eq!(only(&value, "`src_`"), "scenes[0].sources[1].id");
    let mut value = file();
    value["scenes"][0]["sources"][1]["id"] = json!("scene_overlay");
    assert_eq!(only(&value, "`src_`"), "scenes[0].sources[1].id");
    let mut value = file();
    value["scenes"][1]["sources"][0]["id"] = json!("src_overlay");
    assert_eq!(
        only(&value, "unique across scenes"),
        "scenes[1].sources[0].id"
    );
    let mut value = file();
    let source = value["scenes"][1]["sources"][0].clone();
    value["scenes"][1]["sources"] = json!(vec![source; MAX_SCENE_SOURCES + 1]);
    let found = problems(&value);
    assert_eq!(found[0].0, "scenes[1].sources", "{found:?}");
}

#[test]
fn audio_problems_come_from_the_command_rules() {
    let mut value = file();
    value["audio"]["program"]["gain_db"] = json!(99);
    assert_eq!(only(&value, "`gain_db` must be between"), "audio.program");
    // Beyond f32: refused, not rounded to infinity and then to a default.
    let mut value = file();
    value["audio"]["channels"][1]["gain_db"] = json!(1e39);
    assert_eq!(
        only(&value, "`gain_db` must be between"),
        "audio.channels[1]"
    );
    let mut value = file();
    value["audio"]["channels"][1]["input"]["frequency_hz"] = json!(30_000);
    assert_eq!(only(&value, "`frequency_hz`"), "audio.channels[1]");
    let mut value = file();
    value["audio"]["channels"][0]["input"]["device"] = json!("");
    assert_eq!(
        only(&value, "`device` must not be empty"),
        "audio.channels[0]"
    );
    let mut value = file();
    value["audio"]["channels"][0]["balance"] = json!(2);
    assert_eq!(only(&value, "`balance`"), "audio.channels[0]");
}

#[test]
fn channel_ids_and_names_must_be_valid_and_unique_and_within_the_limit() {
    let mut value = file();
    value["audio"]["channels"][1]["id"] = json!("ch_4k2m9x0q1w7e");
    value["audio"]["channels"][1]["name"] = json!("Mic");
    let found = problems(&value);
    assert_eq!(
        found
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        ["audio.channels[1].id", "audio.channels[1].name"],
        "{found:?}"
    );
    let mut value = file();
    value["audio"]["channels"][0]["id"] = json!("scene_mic");
    assert_eq!(only(&value, "`ch_`"), "audio.channels[0].id");
    let mut value = file();
    let channel = value["audio"]["channels"][1].clone();
    value["audio"]["channels"] = json!(vec![channel; MAX_CHANNELS + 1]);
    let found = problems(&value);
    assert_eq!(found[0].0, "audio.channels", "{found:?}");
    // No channels is a silent mix.
    let mut value = file();
    value["audio"]["channels"] = json!([]);
    let bytes = serde_json::to_vec(&value).expect("bytes");
    assert!(
        parse(&bytes, CANVAS)
            .expect("valid")
            .project
            .audio
            .channels
            .is_empty()
    );
}

#[test]
fn sends_left_out_are_the_default_and_buses_may_be_left_out() {
    let mut value = file();
    value["audio"]["channels"][1]
        .as_object_mut()
        .expect("channel")
        .remove("sends");
    // Out of bus order is fine; the reader puts them in order.
    let sends = value["audio"]["channels"][0]["sends"].clone();
    value["audio"]["channels"][0]["sends"] = json!([sends[1], sends[0]]);
    let bytes = serde_json::to_vec(&value).expect("bytes");
    assert_eq!(parse(&bytes, CANVAS), Ok(loaded()));
    // Without buses and profiles, a file is version 2 as step 2a wrote it.
    let mut value = file();
    value.as_object_mut().expect("file").remove("outputs");
    let audio = value["audio"].as_object_mut().expect("audio");
    audio.remove("buses");
    for channel in audio["channels"].as_array_mut().expect("channels") {
        channel.as_object_mut().expect("channel").remove("sends");
    }
    let bytes = serde_json::to_vec(&value).expect("bytes");
    let audio = parse(&bytes, CANVAS).expect("valid").project.audio;
    assert!(audio.buses.is_empty());
    assert!(
        audio
            .channels
            .iter()
            .all(|channel| channel.sends == [SendSpec::program()])
    );
}

#[test]
fn bus_and_send_problems_name_their_path() {
    let mut value = file();
    value["audio"]["buses"][0]["id"] = json!("bus_program");
    // All at once: the id, and the sends that named the bus by its old id.
    let paths: Vec<String> = problems(&value).into_iter().map(|(path, _)| path).collect();
    assert_eq!(
        paths,
        [
            "audio.channels[0].sends[1].bus",
            "audio.channels[1].sends[1].bus",
            "audio.buses[0].id"
        ]
    );
    // A bus that is wrong on its own keeps its id: the sends to it add
    // nothing, and a duplicate name next to it is still found.
    let mut value = file();
    value["audio"]["buses"][0]["gain_db"] = json!(99);
    value["audio"]["channels"][1]["name"] = json!("Mic");
    let paths: Vec<String> = problems(&value).into_iter().map(|(path, _)| path).collect();
    assert_eq!(paths, ["audio.buses[0]", "audio.channels[1].name"]);
    let mut value = file();
    value["audio"]["buses"][0]["name"] = json!("Program");
    assert_eq!(only(&value, "used twice"), "audio.buses[0].name");
    let mut value = file();
    value["audio"]["buses"][0]["assign_db"] = json!(99);
    assert_eq!(only(&value, "`assign_db`"), "audio.buses[0]");
    let mut value = file();
    value["audio"]["channels"][0]["sends"][1]["bus"] = json!("bus_gone");
    assert_eq!(only(&value, "no bus"), "audio.channels[0].sends[1].bus");
    let mut value = file();
    value["audio"]["channels"][0]["sends"][1]["bus"] = json!("bus_program");
    assert_eq!(only(&value, "twice"), "audio.channels[0].sends[1].bus");
    let mut value = file();
    value["audio"]["channels"][0]["sends"][1]["level_db"] = json!(-200);
    assert_eq!(only(&value, "`level_db`"), "audio.channels[0].sends[1]");
    let mut value = file();
    value["audio"]["channels"][0]["sends"][1]["tap"] = json!("sideways");
    assert_eq!(
        only(&value, "unknown variant"),
        "audio.channels[0].sends[1].tap"
    );
    let mut value = file();
    let bus = value["audio"]["buses"][0].clone();
    let buses: Vec<Value> = (0..=MAX_AUX_BUSES)
        .map(|index| {
            let mut bus = bus.clone();
            bus["id"] = json!(format!("bus_{index}"));
            bus["name"] = json!(format!("Bus {index}"));
            bus
        })
        .collect();
    value["audio"]["buses"] = json!(buses);
    let found = problems(&value);
    assert!(
        found
            .iter()
            .any(|(path, message)| path == "audio.buses" && message.contains("at most")),
        "{found:?}"
    );
}

#[test]
fn output_profiles_name_existing_buses_and_follow_their_rules() {
    // Left out: the defaults.
    let mut value = file();
    value.as_object_mut().expect("file").remove("outputs");
    let bytes = serde_json::to_vec(&value).expect("bytes");
    assert_eq!(
        parse(&bytes, CANVAS).expect("valid").project.outputs,
        OutputProfiles::default()
    );
    let mut value = file();
    value["outputs"]["recording"]["mix"] = json!("bus_gone");
    assert_eq!(only(&value, "no bus"), "outputs.recording.mix");
    // Not an id at all: one problem, from the command's rules.
    let mut value = file();
    value["outputs"]["recording"]["mix"] = json!("Voice");
    assert_eq!(only(&value, "`bus_id`"), "outputs.recording");
    let mut value = file();
    value["outputs"]["stream"]["video_bitrate_kbps"] = json!(1);
    assert_eq!(only(&value, "`video_bitrate_kbps`"), "outputs.stream");
    let mut value = file();
    value["outputs"]["recording"]["video_encoder"] = json!("quantum");
    assert_eq!(only(&value, "unknown variant"), "outputs.recording");
    // Tracks name buses of the file, once each, not the mix again.
    let mut value = file();
    value["outputs"]["recording"]["tracks"] = json!(["bus_gone"]);
    assert_eq!(only(&value, "no bus"), "outputs.recording.tracks[0]");
    let mut value = file();
    value["outputs"]["recording"]["tracks"] = json!(["bus_program", "bus_program"]);
    assert_eq!(
        only(&value, "already a track"),
        "outputs.recording.tracks[1]"
    );
    let mut value = file();
    value["outputs"]["recording"]["tracks"] = json!(["bus_voice00000a"]);
    assert_eq!(
        only(&value, "already a track"),
        "outputs.recording.tracks[0]"
    );
    let mut value = file();
    value["outputs"]["recording"]["tracks"] = json!(["Voice"]);
    assert_eq!(only(&value, "by id"), "outputs.recording.tracks[0]");
    let mut value = file();
    value["outputs"]["stream"]["tracks"] = json!([]);
    assert_eq!(
        only(&value, "unknown field `tracks`"),
        "outputs.stream.tracks"
    );
    let mut value = file();
    value["outputs"]["speakers"] = json!({});
    assert_eq!(only(&value, "unknown field `speakers`"), "outputs.speakers");
    // The monitor: a bus of the file, on or off, a device or the default.
    let mut value = file();
    value["outputs"]["monitor"]["mix"] = json!("bus_gone");
    assert_eq!(only(&value, "no bus"), "outputs.monitor.mix");
    let mut value = file();
    value["outputs"]["monitor"]["enabled"] = json!("yes");
    assert_eq!(only(&value, "invalid type"), "outputs.monitor");
    let mut value = file();
    value["outputs"]["monitor"]["device"] = json!("");
    assert_eq!(only(&value, "must not be empty"), "outputs.monitor.device");
    let mut value = file();
    value["outputs"]["monitor"]["video_encoder"] = json!("auto");
    assert_eq!(
        only(&value, "unknown field `video_encoder`"),
        "outputs.monitor.video_encoder"
    );
    let mut value = file();
    value["outputs"]["monitor"]
        .as_object_mut()
        .expect("monitor")
        .remove("device");
    let bytes = serde_json::to_vec(&value).expect("bytes");
    let loaded = parse(&bytes, CANVAS).expect("valid");
    assert_eq!(loaded.project.outputs.monitor.device, None);
}

#[test]
fn a_version_1_file_is_migrated() {
    let bytes = serde_json::to_vec(&v1_file()).expect("bytes");
    let loaded = parse(&bytes, CANVAS).expect("valid");
    assert_eq!(loaded.migrated_from, Some(1));
    // Ids and names from the position, everything else as version 1 had it.
    let mut expected = project().audio;
    expected.channels[0].id = "ch_input_1".to_owned();
    expected.channels[0].name = "Input 1".to_owned();
    expected.channels[0].balance = 0.0;
    expected.channels[1].id = "ch_tone_1".to_owned();
    expected.channels[1].name = "Tone 1".to_owned();
    // Version 1 had no buses: every channel sends to program only.
    expected.buses.clear();
    for channel in &mut expected.channels {
        channel.sends = vec![SendSpec::program()];
    }
    assert_eq!(loaded.project.audio, expected);
    assert_eq!(loaded.project.scenes, project().scenes);
    assert_eq!(loaded.project.outputs, OutputProfiles::default());
    // The same ids every time, until the file is saved as version 3.
    assert_eq!(parse(&bytes, CANVAS), Ok(loaded));
}

#[test]
fn version_1_audio_keeps_its_rules_with_paths() {
    let mut value = v1_file();
    value["audio"]["master_gain_db"] = json!(99);
    assert_eq!(only(&value, "between"), "audio.master_gain_db");
    let mut value = v1_file();
    value["audio"]["tones"][0]["gain_db"] = json!(1e39);
    assert_eq!(only(&value, "between"), "audio.tones[0].gain_db");
    let mut value = v1_file();
    value["audio"]["tones"][0]["frequency_hz"] = json!(30_000);
    assert_eq!(only(&value, "between 0"), "audio.tones[0].frequency_hz");
    let mut value = v1_file();
    value["audio"]["inputs"][0]["input"] = json!("");
    assert_eq!(only(&value, "must not be empty"), "audio.inputs[0].input");
    let mut value = v1_file();
    value["audio"]["tones"][0]["mute"] = json!("no");
    assert_eq!(only(&value, "invalid type"), "audio.tones[0].mute");
    let mut value = v1_file();
    let tone = value["audio"]["tones"][0].clone();
    value["audio"]["tones"] = json!(vec![tone; 9]);
    assert_eq!(only(&value, "at most 8 sources"), "audio");
    // Version 2's shape is not version 1's: neither its audio nor its
    // output profiles.
    let mut value = v1_file();
    value["audio"] = file()["audio"].clone();
    assert_eq!(only(&value, "unknown field `buses`"), "audio.buses");
    let mut value = v1_file();
    value["outputs"] = json!({});
    assert_eq!(only(&value, "version 1 has no output profiles"), "outputs");
}

#[test]
fn duplicate_keys_are_refused() {
    let text = serde_json::to_string(&file()).expect("text");
    let doubled = text.replacen("\"opacity\":0.6", "\"opacity\":5,\"opacity\":0.6", 1);
    assert_ne!(doubled, text);
    let invalid = parse(doubled.as_bytes(), CANVAS).unwrap_err();
    assert_eq!(invalid.problems.len(), 1);
    assert_eq!(invalid.problems[0].path, "scenes[0].sources[1]");
    assert!(
        invalid.problems[0]
            .message
            .contains("duplicate field `opacity`")
    );
}

#[test]
fn many_problems_are_capped() {
    let mut value = file();
    let scenes: Vec<_> = (0..MAX_SCENES)
        .map(|index| {
            let source = json!({"id": "bad", "name": format!("s{index}"), "kind": "color_bars"});
            json!({"id": format!("scene_{index}"), "name": format!("S{index}"),
                   "sources": vec![source; MAX_SCENE_SOURCES]})
        })
        .collect();
    value["scenes"] = json!(scenes);
    let found = problems(&value);
    assert_eq!(found.len(), MAX_PROBLEMS + 1);
    assert!(
        found[MAX_PROBLEMS].1.contains("more problem(s)"),
        "{found:?}"
    );
}

#[test]
fn every_problem_is_reported_at_once() {
    let mut value = file();
    value["canvas"] = json!({"width": 1280, "height": 720});
    value["scenes"][0]["sources"][1]["opacity"] = json!(2);
    value["active_scene"] = json!("scene_gone");
    value["audio"]["program"]["gain_db"] = json!(99);
    let paths: Vec<_> = problems(&value).into_iter().map(|(path, _)| path).collect();
    assert_eq!(
        paths,
        [
            "canvas",
            "scenes[0].sources[1]",
            "active_scene",
            "audio.program"
        ]
    );
}

#[test]
fn version_2_screen_fields_migrate_and_save_as_version_3() {
    let mut old = file();
    old["schema_version"] = json!(2);
    old_screen_fields(&mut old);
    let loaded = parse(&serde_json::to_vec(&old).unwrap(), CANVAS).expect("v2 migrates");
    assert_eq!(loaded.migrated_from, Some(2));
    assert_eq!(loaded.project, project());
    let saved = to_json(&loaded.project, CANVAS);
    let value: Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(value["schema_version"], 3);
    let screen = &value["scenes"][1]["sources"][0];
    assert_eq!(screen["target_kinds"], json!(["window"]));
    assert_eq!(screen["cursor_mode"], "hidden");
    assert!(screen.get("source_types").is_none() && screen.get("cursor").is_none());
    assert_eq!(
        parse(&saved, CANVAS).unwrap(),
        super::Loaded {
            project: loaded.project,
            migrated_from: None
        }
    );
}

#[test]
fn old_screen_fields_in_version_3_and_stored_targets_have_paths() {
    for (old, new) in [("source_types", "target_kinds"), ("cursor", "cursor_mode")] {
        let mut value = file();
        let screen = value["scenes"][1]["sources"][0].as_object_mut().unwrap();
        let field = screen.remove(new).unwrap();
        screen.insert(old.into(), field);
        assert_eq!(
            only(&value, &format!("unknown field `{old}`")),
            format!("scenes[1].sources[0].{old}")
        );
    }
    let mut old = file();
    old["schema_version"] = json!(2);
    old_screen_fields(&mut old);
    old["scenes"][1]["sources"][0]["source_id"] = json!("monitor:1");
    assert_eq!(only(&old, "not stored"), "scenes[1].sources[0].source_id");
    // The rest of that source is still checked.
    old["scenes"][1]["sources"][0]["id"] = json!("src_color_bars");
    let paths: Vec<String> = problems(&old).into_iter().map(|(path, _)| path).collect();
    assert!(
        paths.contains(&"scenes[1].sources[0].source_id".to_owned())
            && paths.contains(&"scenes[1].sources[0].id".to_owned()),
        "{paths:?}"
    );
    let mut current = file();
    current["scenes"][1]["sources"][0]["capture_target_id"] = json!("monitor:1");
    assert_eq!(
        only(&current, "not stored"),
        "scenes[1].sources[0].capture_target_id"
    );
}

#[test]
fn migration_refuses_conflicting_spellings_and_preserves_invalid_value_paths() {
    let mut old = file();
    old["schema_version"] = json!(2);
    old_screen_fields(&mut old);
    old["scenes"][1]["sources"][0]["cursor_mode"] = json!("embedded");
    assert_eq!(
        only(&old, "unknown field `cursor_mode`"),
        "scenes[1].sources[0].cursor_mode"
    );
    let mut old = file();
    old["schema_version"] = json!(2);
    old_screen_fields(&mut old);
    old["scenes"][1]["sources"][0]["source_types"] = json!([]);
    assert_eq!(only(&old, "must not be empty"), "scenes[1].sources[0]");
}
