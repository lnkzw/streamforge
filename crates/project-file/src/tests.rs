use capture_api::{CursorMode, VideoSourceKind};
use command_api::{LiveInputSpec, Rgba, ToneSpec};

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
                        source_types: vec![VideoSourceKind::Window],
                        cursor: CursorMode::Hidden,
                        source_id: None,
                    },
                    rect: rect(-10.5, 20.25, 960.0, 540.0),
                    opacity: 0.9,
                }],
            },
        ],
        active: "scene_k3j9x2m4q8w1".to_owned(),
        audio: AudioSetup {
            inputs: vec![LiveInputSpec {
                input: Some("alsa_input.usb-mic".to_owned()),
                gain_db: -3.5,
                muted: true,
            }],
            tones: vec![ToneSpec {
                frequency_hz: 440.0,
                level_dbfs: -20.0,
                gain_db: 0.0,
                muted: false,
            }],
            master_gain_db: -1.0,
        },
    }
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
    assert_eq!(parse(&bytes, CANVAS), Ok(original));
}

#[test]
fn screen_capture_targets_are_not_saved() {
    let mut original = project();
    let SourceKind::Screen { source_id, .. } = &mut original.scenes[1].sources[0].kind else {
        panic!("screen source");
    };
    *source_id = Some("window:7:0x2a".to_owned());
    let bytes = to_json(&original, CANVAS);
    let text = String::from_utf8(bytes.clone()).expect("utf-8");
    assert!(
        !text.contains("source_id") && !text.contains("window:7"),
        "{text}"
    );
    // Everything else comes back.
    let loaded = parse(&bytes, CANVAS).expect("valid");
    assert_eq!(loaded, project());
}

#[test]
fn the_file_reads_like_the_command_results() {
    let value = file();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["canvas"], json!({"width": 1920, "height": 1080}));
    assert_eq!(value["active_scene"], "scene_k3j9x2m4q8w1");
    // A source is its `list_scenes` object.
    assert_eq!(
        value["scenes"][0]["sources"][1],
        json!({"id": "src_overlay", "name": "overlay", "kind": "solid", "color": "#99999999",
               "x": 64.0, "y": 120.0, "width": 160.0, "height": 160.0, "opacity": 0.6})
    );
    // The audio is `set_audio_sources` parameters.
    assert_eq!(
        value["audio"]["inputs"][0],
        json!({"input": "alsa_input.usb-mic", "gain_db": -3.5, "mute": true})
    );
}

#[test]
fn a_hand_written_source_may_leave_out_its_rectangle_and_opacity() {
    let mut value = file();
    value["scenes"][0]["sources"][0] =
        json!({"id": "src_bars", "name": "bars", "kind": "color_bars"});
    let bytes = serde_json::to_vec(&value).expect("bytes");
    let loaded = parse(&bytes, CANVAS).expect("valid");
    let bars = &loaded.scenes[0].sources[0];
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
    value["schema_version"] = json!(2);
    assert_eq!(
        only(&value, "newer than this build reads"),
        "schema_version"
    );
    let invalid = parse(&serde_json::to_vec(&value).expect("bytes"), CANVAS).unwrap_err();
    assert_eq!(invalid.schema_version, Some(2));
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
    value["audio"]["tones"][0]["mute"] = json!("no");
    assert_eq!(only(&value, "invalid type"), "audio.tones[0].mute");
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
    assert!(!problems(&value)[0].1.contains("source_id"));
    let mut value = file();
    value["scenes"][0]["sources"][1]["name"] = json!("color_bars");
    assert_eq!(
        only(&value, "used twice in this scene"),
        "scenes[0].sources[1].name"
    );
    let mut value = file();
    value["scenes"][0]["sources"][1]["scene"] = json!("Show");
    assert_eq!(only(&value, "the one it is listed in"), at);
    let mut value = file();
    value["scenes"][1]["sources"][0]["source_id"] = json!("monitor:1");
    assert_eq!(only(&value, "not stored"), "scenes[1].sources[0].source_id");
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
fn audio_problems_come_from_the_set_audio_sources_rules() {
    let mut value = file();
    value["audio"]["master_gain_db"] = json!(99);
    assert_eq!(only(&value, "`master_gain_db` must be between"), "audio");
    // Beyond f32: refused, not rounded to infinity and then to a default.
    let mut value = file();
    value["audio"]["tones"][0]["gain_db"] = json!(1e39);
    assert_eq!(only(&value, "`gain_db` must be between"), "audio");
    let mut value = file();
    value["audio"]["tones"][0]["frequency_hz"] = json!(30_000);
    assert_eq!(only(&value, "`frequency_hz`"), "audio");
    let mut value = file();
    value["audio"]["inputs"][0]["input"] = json!("");
    assert_eq!(only(&value, "`input` must not be empty"), "audio");
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
    value["audio"]["master_gain_db"] = json!(99);
    let paths: Vec<_> = problems(&value).into_iter().map(|(path, _)| path).collect();
    assert_eq!(
        paths,
        ["canvas", "scenes[0].sources[1]", "active_scene", "audio"]
    );
}
