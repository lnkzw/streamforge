//! M9b acceptance: through the real engine process, record a screen
//! source named from `list_screen_sources`, on platforms whose capture
//! backend lists sources (Windows). Elsewhere the listing must fail with a
//! structured error, and the test says so.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::EngineProcess;
use serde_json::{Value, json};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("streamforge-screen-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
    let listed = call(&mut engine, 2, "list_screen_sources", json!({}));
    println!("{caps}\n{listed}");
    if caps["result"]["screen"]["source_list"] != true {
        assert_eq!(listed["ok"], false, "{listed}");
        assert!(listed["error"]["message"].is_string(), "{listed}");
        println!("this platform has no screen source list; nothing to record");
        assert!(engine.finish().success());
        return;
    }
    let sources = listed["result"]["sources"].as_array().expect("sources");
    let monitor = sources
        .iter()
        .find(|source| source["kind"] == "monitor")
        .unwrap_or_else(|| panic!("no monitor in {listed}"));
    let id = monitor["id"].as_str().expect("id");

    let steps = [
        ("create_scene", json!({"name": "Desk"})),
        (
            "add_source",
            json!({"scene": "Desk", "name": "monitor", "kind": "screen", "source_id": id}),
        ),
        ("set_active_scene", json!({"name": "Desk"})),
        ("start_recording", json!({"file_name": "screen.mkv"})),
    ];
    for (n, (command, params)) in steps.into_iter().enumerate() {
        let response = call(&mut engine, 10 + n as u64, command, params);
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
