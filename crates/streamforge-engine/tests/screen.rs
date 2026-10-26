//! M9b acceptance: through the real engine process, record a screen
//! source named from `list_capture_targets`, on platforms whose capture
//! backend lists sources (Windows, macOS). Elsewhere the listing must fail
//! with a structured error, and so must it where screen recording is not
//! permitted (macOS without the permission); the test says which.

mod common;

use std::time::Duration;

use common::{EngineProcess, TempDir};
use serde_json::{Value, json};

fn call(engine: &mut EngineProcess, id: u64, command: &str, params: Value) -> Value {
    let request = json!({"id": id, "command": command, "params": params});
    engine.request(&request.to_string())
}

#[test]
fn a_listed_monitor_is_recorded_from_a_scene() {
    let dir = TempDir::new("record");
    let mut engine =
        EngineProcess::spawn_with(&[std::ffi::OsStr::new("--output-dir"), dir.0.as_os_str()]);
    let caps = call(&mut engine, 1, "get_capture_capabilities", json!({}));
    let listed = call(&mut engine, 2, "list_capture_targets", json!({}));
    println!("{caps}\n{listed}");
    if caps["result"]["screen"]["target_list"] != true {
        assert_eq!(listed["ok"], false, "{listed}");
        assert!(listed["error"]["message"].is_string(), "{listed}");
        println!("this platform has no screen source list; nothing to record");
        assert!(engine.finish().success());
        return;
    }
    // macOS CI runners cannot grant screen recording; the error must say so.
    if listed["error"]["code"] == "permission_denied" {
        println!("screen recording is not permitted here; nothing to record");
        assert!(engine.finish().success());
        return;
    }
    let sources = listed["result"]["targets"].as_array().expect("sources");
    let monitor = sources
        .iter()
        .find(|source| source["kind"] == "monitor")
        .unwrap_or_else(|| panic!("no monitor in {listed}"));
    let id = monitor["id"].as_str().expect("id");

    let steps = [
        ("create_scene", json!({"name": "Desk"})),
        (
            "add_source",
            json!({"scene_name": "Desk", "name": "monitor", "kind": "screen", "capture_target_id": id}),
        ),
        ("set_active_scene", json!({"scene_name": "Desk"})),
        ("start_recording", json!({"file_name": "screen.mkv"})),
    ];
    for (n, (command, params)) in steps.into_iter().enumerate() {
        let response = call(&mut engine, 10 + n as u64, command, params);
        // Listing can be allowed while capturing is not (macOS).
        if command == "start_recording" && response["error"]["code"] == "permission_denied" {
            println!("capturing is not permitted here: {response}");
            assert!(engine.finish().success());
            return;
        }
        assert_eq!(response["ok"], true, "{command}: {response}");
    }
    std::thread::sleep(Duration::from_millis(1500));
    let stopped = call(&mut engine, 20, "stop_recording", json!({}));
    assert!(engine.finish().success());
    println!("{stopped}");
    assert_eq!(stopped["ok"], true, "{stopped}");
    let report = &stopped["result"];
    let captures = report["scene_captures"].as_array().expect("scene captures");
    let capture = captures
        .iter()
        .find(|capture| capture["source"] == "monitor")
        .unwrap_or_else(|| panic!("no capture outcome: {report}"));
    assert!(capture["error"].is_null(), "{capture}");
    let frames = capture["report"]["stats"]["frames"].as_u64().unwrap_or(0);
    assert!(frames >= 1, "the monitor delivered no frame: {capture}");
    assert!(
        report["recording"]["video"]["frames_encoded"]
            .as_u64()
            .is_some_and(|encoded| encoded > 0),
        "{report}"
    );
    let file = dir.0.join("screen.mkv");
    assert!(std::fs::metadata(&file).expect("recording file").len() > 0);
}
