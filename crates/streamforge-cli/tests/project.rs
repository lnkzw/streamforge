//! `streamforge project validate FILE` through the real binary.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "streamforge-cli-test-{}-{name}",
            std::process::id()
        ));
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

fn project() -> Value {
    json!({
        "schema_version": 2,
        "canvas": {"width": 1920, "height": 1080},
        "active_scene": "scene_main",
        "scenes": [{"id": "scene_main", "name": "Main", "sources": [
            {"id": "src_bars", "name": "bars", "kind": "color_bars",
             "x": 0, "y": 0, "width": 1920, "height": 1080, "opacity": 1}
        ]}],
        "audio": {"channels": [], "program": {"gain_db": 0, "mute": false}}
    })
}

/// Run `streamforge project validate PATH`: exit success and stdout JSON.
fn validate(path: &Path) -> (bool, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_streamforge"))
        .args(["project", "validate"])
        .arg(path)
        .output()
        .expect("run streamforge");
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let value = serde_json::from_str(&stdout).unwrap_or(Value::Null);
    (output.status.success(), value)
}

#[test]
fn project_validate_reports_problems_with_their_path() {
    let dir = TempDir::new("validate");
    let good = dir.0.join("good.json");
    std::fs::write(&good, project().to_string()).expect("write");
    let (ok, report) = validate(&good);
    assert!(ok, "{report}");
    assert_eq!(report["valid"], true);
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["migrates"], false);

    // A version 1 file is valid and would be migrated.
    let mut old = project();
    old["schema_version"] = json!(1);
    old["audio"] = json!({"inputs": [], "tones": [], "master_gain_db": 0});
    std::fs::write(&good, old.to_string()).expect("write");
    let (ok, report) = validate(&good);
    assert!(ok, "{report}");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["migrates"], true);

    let mut broken = project();
    broken["scenes"][0]["sources"][0]["opacity"] = json!(5);
    let bad = dir.0.join("bad.json");
    std::fs::write(&bad, broken.to_string()).expect("write");
    let (ok, report) = validate(&bad);
    assert!(!ok, "{report}");
    assert_eq!(report["valid"], false);
    assert_eq!(report["problems"][0]["path"], "scenes[0].sources[0]");

    let mut newer = project();
    newer["schema_version"] = json!(3);
    std::fs::write(&bad, newer.to_string()).expect("write");
    let (ok, report) = validate(&bad);
    assert!(!ok);
    assert_eq!(report["problems"][0]["path"], "schema_version");

    // A missing file is an error, not a report.
    let (ok, report) = validate(&dir.0.join("none.json"));
    assert!(!ok && report.is_null());
}
