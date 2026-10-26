//! M1 acceptance: drive the real `streamforge-engine` process over stdio.
//!
//! Every wait is bounded by `common::TIMEOUT`; the child is killed if a test
//! fails.

mod common;

use std::collections::HashMap;

use common::{EngineProcess, TempDir};
use control_protocol::MAX_REQUEST_BYTES;
use serde_json::{Value, json};

fn assert_error(response: &Value, id: &Value, code: &str) {
    assert_eq!(response["v"], 1, "{response}");
    assert_eq!(&response["id"], id, "{response}");
    assert_eq!(response["ok"], false, "{response}");
    assert_eq!(response["error"]["code"], code, "{response}");
    assert!(response["error"]["message"].is_string(), "{response}");
}

fn assert_status_ok(response: &Value, id: &Value) {
    assert_eq!(response["v"], 1, "{response}");
    assert_eq!(&response["id"], id, "{response}");
    assert_eq!(response["ok"], true, "{response}");
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["application"], "StreamForge");
    assert_eq!(response["result"]["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn get_status_returns_correlated_response() {
    let mut engine = EngineProcess::spawn();
    let response = engine.request(r#"{"v":1,"id":"status-1","command":"get_status"}"#);
    assert_status_ok(&response, &json!("status-1"));
    assert_eq!(response["result"]["recording_state"], json!(null));
    assert_eq!(response["result"]["streaming_state"], json!(null));
    assert!(response["result"].get("media_engine_ready").is_none());
    assert_eq!(response["result"]["pipeline_running"], false);
    assert!(engine.finish().success());
}

#[test]
fn health_follows_the_preview_pipeline() {
    let _media = common::media_lock();
    let mut engine = EngineProcess::spawn();
    let idle = engine.request(r#"{"id":1,"command":"get_health"}"#);
    assert_eq!(idle["ok"], true, "{idle}");
    assert_eq!(idle["result"]["pipeline_running"], false);
    assert_eq!(idle["result"]["render"], Value::Null);
    let preview = engine.request(r#"{"id":2,"command":"set_preview","params":{"enabled":true}}"#);
    assert_eq!(preview["ok"], true, "{preview}");
    // Counters appear and grow while the pipeline runs (refreshed every 250 ms).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let running = loop {
        let health = engine.request(r#"{"id":3,"command":"get_health"}"#);
        if health["result"]["render"]["frames_rendered"]
            .as_u64()
            .unwrap_or(0)
            > 0
        {
            break health;
        }
        assert!(std::time::Instant::now() < deadline, "{health}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(running["result"]["pipeline_running"], true, "{running}");
    assert!(
        running["result"]["mix"]["blocks_mixed"].is_u64(),
        "{running}"
    );
    assert_eq!(running["result"]["recording"], Value::Null);
    let off = engine.request(r#"{"id":4,"command":"set_preview","params":{"enabled":false}}"#);
    assert_eq!(off["ok"], true, "{off}");
    let after = engine.request(r#"{"id":5,"command":"get_health"}"#);
    assert_eq!(after["result"]["pipeline_running"], false, "{after}");
    assert!(engine.finish().success());
}

#[test]
fn pipelined_requests_are_correlated_by_id() {
    let mut engine = EngineProcess::spawn();
    engine.send_raw(
        concat!(
            "{\"id\":1,\"command\":\"get_status\"}\n",
            "{\"id\":\"two\",\"command\":\"list_commands\"}\n",
            "{\"id\":3,\"command\":\"nope\"}\n",
        )
        .as_bytes(),
    );
    let responses: HashMap<String, Value> = (0..3)
        .map(|_| {
            let response = engine.recv();
            (response["id"].to_string(), response)
        })
        .collect();
    assert_status_ok(&responses["1"], &json!(1));
    assert_eq!(responses["\"two\""]["ok"], true);
    assert_eq!(
        responses["\"two\""]["result"]["commands"][0]["name"],
        "get_status"
    );
    assert_error(&responses["3"], &json!(3), "unknown_command");
    assert!(engine.finish().success());
}

#[test]
fn invalid_requests_return_structured_errors_without_crashing() {
    let cases = [
        ("{not json", json!(null), "parse_error"),
        ("[1,2,3]", json!(null), "invalid_request"),
        (
            r#"{"command":"get_status"}"#,
            json!(null),
            "invalid_request",
        ),
        (
            r#"{"id":{"nested":true},"command":"get_status"}"#,
            json!(null),
            "invalid_request",
        ),
        (r#"{"id":"a"}"#, json!("a"), "invalid_request"),
        (r#"{"id":"b","command":42}"#, json!("b"), "invalid_request"),
        (
            r#"{"id":"c","command":"get_status","parms":{}}"#,
            json!("c"),
            "invalid_request",
        ),
        (
            r#"{"v":2,"id":"d","command":"get_status"}"#,
            json!("d"),
            "unsupported_version",
        ),
        (
            r#"{"id":"e","command":"start_teleport"}"#,
            json!("e"),
            "unknown_command",
        ),
        (
            r#"{"id":"f","command":"get_status","params":{"verbose":true}}"#,
            json!("f"),
            "invalid_params",
        ),
        (
            r#"{"id":"g","command":"get_status","params":[1]}"#,
            json!("g"),
            "invalid_params",
        ),
    ];
    let mut engine = EngineProcess::spawn();
    for (request, id, code) in cases {
        let response = engine.request(request);
        assert_error(&response, &id, code);
    }
    engine.send_raw(b"\xff\xfe\n");
    assert_error(&engine.recv(), &json!(null), "parse_error");

    // Still serving after every kind of bad input.
    let response = engine.request(r#"{"id":"after","command":"get_status"}"#);
    assert_status_ok(&response, &json!("after"));
    assert!(engine.finish().success());
}

#[test]
fn oversized_request_is_rejected_and_stream_recovers() {
    let mut engine = EngineProcess::spawn();
    let mut oversized = vec![b'x'; MAX_REQUEST_BYTES + 4096];
    oversized.push(b'\n');
    engine.send_raw(&oversized);
    let response = engine.recv();
    assert_error(&response, &json!(null), "request_too_large");
    assert_eq!(response["error"]["details"]["max_bytes"], MAX_REQUEST_BYTES);

    let response = engine.request(r#"{"id":5,"command":"get_status"}"#);
    assert_status_ok(&response, &json!(5));
    assert!(engine.finish().success());
}

#[test]
fn blank_lines_are_ignored_and_crlf_is_accepted() {
    let mut engine = EngineProcess::spawn();
    engine.send_raw(b"\n   \r\n{\"id\":9,\"command\":\"get_status\"}\r\n");
    assert_status_ok(&engine.recv(), &json!(9));
    // finish() fails if the blank lines produced any response.
    assert!(engine.finish().success());
}

#[test]
fn exits_cleanly_when_stdin_closes() {
    let engine = EngineProcess::spawn();
    assert!(engine.finish().success());
}

#[test]
fn capture_capabilities_are_structured_on_every_platform() {
    // Must answer on CI runners without a desktop session as well: unavailable
    // capabilities are reported with a reason, not as a crash or hang.
    let mut engine = EngineProcess::spawn();
    let response = engine.request(r#"{"id":"caps","command":"get_capture_capabilities"}"#);
    assert_eq!(response["id"], "caps", "{response}");
    assert_eq!(response["ok"], true, "{response}");
    let caps = &response["result"];
    assert_eq!(caps["platform"], std::env::consts::OS);
    for kind in ["screen", "audio"] {
        let capability = &caps[kind];
        let available = capability["available"].as_bool().expect("available flag");
        if !available {
            assert!(
                capability["unavailable_reason"]
                    .as_str()
                    .is_some_and(|reason| !reason.is_empty()),
                "{kind} unavailable without reason: {response}"
            );
        }
    }
    let status = engine.request(r#"{"id":"status","command":"get_status"}"#);
    let any_available = caps["screen"]["available"] == true || caps["audio"]["available"] == true;
    assert_eq!(status["result"]["capture_available"], any_available);
    assert!(engine.finish().success());
}

#[test]
fn invalid_capture_params_are_rejected_before_any_device_access() {
    let mut engine = EngineProcess::spawn();
    let response = engine.request(
        r#"{"id":1,"command":"probe_screen_capture","params":{"target_kinds":["printer"]}}"#,
    );
    assert_error(&response, &json!(1), "invalid_params");
    let response =
        engine.request(r#"{"id":2,"command":"probe_audio_capture","params":{"duration_ms":0}}"#);
    assert_error(&response, &json!(2), "invalid_params");
    assert!(engine.finish().success());
}

#[test]
fn composite_probe_runs_headless() {
    let _media = common::media_lock();
    // Test pattern only: needs no desktop session, so it runs on every CI OS.
    let mut engine = EngineProcess::spawn();
    let response =
        engine.request(r#"{"id":"c","command":"probe_composite","params":{"duration_ms":300}}"#);
    assert_eq!(response["id"], "c", "{response}");
    assert_eq!(response["ok"], true, "{response}");
    let render = &response["result"]["render"];
    assert_eq!(render["width"], 1920);
    assert_eq!(render["height"], 1080);
    assert_eq!(render["target_fps"], 60);
    assert!(
        render["frames_rendered"].as_u64().is_some_and(|n| n > 0),
        "{render}"
    );
    assert!(render["render_time"]["p99_ms"].is_number(), "{render}");
    // Source ids are strings on the wire (ADR 0014).
    let ids: Vec<_> = render["sources"]
        .as_array()
        .expect("sources")
        .iter()
        .map(|source| source["id"].clone())
        .collect();
    assert_eq!(ids, [json!("bars"), json!("overlay")], "{render}");
    assert!(response["result"]["screen_capture"].is_null());
    assert!(engine.finish().success());
}

#[test]
fn projects_save_and_load_in_the_project_directory() {
    let dir = TempDir::new("projects");
    let args = [std::ffi::OsStr::new("--project-dir"), dir.0.as_os_str()];
    let mut engine = EngineProcess::spawn_with(&args);
    let created = engine.request(r#"{"id":1,"command":"create_scene","params":{"name":"Show"}}"#);
    assert_eq!(created["ok"], true, "{created}");
    let saved =
        engine.request(r#"{"id":2,"command":"save_project","params":{"file_name":"show.json"}}"#);
    assert_eq!(saved["ok"], true, "{saved}");
    assert!(dir.0.join("show.json").is_file());
    // A path is refused: clients only name files in the directory.
    let escaped = engine
        .request(r#"{"id":3,"command":"save_project","params":{"file_name":"../show.json"}}"#);
    assert_eq!(escaped["error"]["code"], "invalid_params", "{escaped}");
    assert!(engine.finish().success());

    let mut fresh = EngineProcess::spawn_with(&args);
    let loaded =
        fresh.request(r#"{"id":4,"command":"load_project","params":{"file_name":"show.json"}}"#);
    assert_eq!(loaded["ok"], true, "{loaded}");
    assert_eq!(loaded["result"]["scenes"][1]["name"], "Show", "{loaded}");
    assert_eq!(
        loaded["result"]["scenes"][1]["id"],
        created["result"]["scenes"][1]["id"]
    );
    assert!(fresh.finish().success());

    // A project directory that does not exist stops the engine at start.
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_streamforge-engine"))
        .arg("--project-dir")
        .arg(dir.0.join("absent"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run");
    assert!(!status.success());

    // Without --project-dir the commands are unavailable.
    let mut plain = EngineProcess::spawn();
    let refused = plain
        .request(r#"{"id":5,"command":"validate_project","params":{"file_name":"show.json"}}"#);
    assert_eq!(refused["error"]["code"], "invalid_state", "{refused}");
    assert!(plain.finish().success());
}

#[test]
fn audio_mix_probe_runs_headless_with_test_tones() {
    // Tones only: no audio device needed, so it runs on every CI OS.
    let mut engine = EngineProcess::spawn();
    let response = engine.request(
        r#"{"id":"m","command":"probe_audio_mix","params":{"duration_ms":300,"tones":[{"frequency_hz":440,"level_dbfs":-6},{"frequency_hz":1000,"level_dbfs":-12}]}}"#,
    );
    assert_eq!(response["ok"], true, "{response}");
    let mix = &response["result"]["mix"];
    assert_eq!(mix["rate"], 48_000);
    assert_eq!(mix["channels"], 2);
    assert!(mix["blocks_mixed"].as_u64().is_some_and(|n| n > 0), "{mix}");
    assert_eq!(mix["inputs"].as_array().map(Vec::len), Some(2));
    let peak = mix["inputs"][0]["meter"]["peak_dbfs"]
        .as_f64()
        .expect("peak");
    assert!((peak + 6.0).abs() < 0.05, "{peak}");
    assert_eq!(mix["clipped_samples"], 0);
    assert!(engine.finish().success());
}

#[test]
fn allow_restricts_what_the_client_may_run() {
    let mut engine = EngineProcess::spawn_with(&["--allow", "read-only"]);
    let status = engine.request(r#"{"id":1,"command":"get_status"}"#);
    assert_eq!(status["ok"], true, "{status}");
    let create = engine.request(r#"{"id":2,"command":"create_scene","params":{"name":"x"}}"#);
    assert_error(&create, &json!(2), "permission_denied");
    assert_eq!(
        create["error"]["details"]["required"], "control",
        "{create}"
    );
    assert!(engine.finish().success());
}

#[test]
fn scene_and_mixer_results_use_explicit_reference_names() {
    let mut engine = EngineProcess::spawn();
    let scenes = engine.request(r#"{"id":1,"command":"list_scenes"}"#);
    assert_eq!(scenes["ok"], true, "{scenes}");
    assert_eq!(scenes["result"]["active_scene_name"], "Test pattern");
    assert_eq!(scenes["result"]["active_scene_id"], "scene_test_pattern");
    assert!(scenes["result"].get("active").is_none());
    assert!(scenes["result"].get("active_id").is_none());
    let added = engine.request(r#"{"id":4,"command":"add_source","params":{"scene_name":"Test pattern","name":"desk","kind":"screen","capture_target_id":"monitor:1","target_kinds":["monitor"],"cursor_mode":"hidden"}}"#);
    assert_eq!(added["ok"], true, "{added}");
    let source = &added["result"]["scenes"][0]["sources"][2];
    assert_eq!(source["capture_target_id"], "monitor:1");
    assert_eq!(source["target_kinds"], json!(["monitor"]));
    assert_eq!(source["cursor_mode"], "hidden");
    assert!(source.get("source_id").is_none());
    let mixer = engine.request(r#"{"id":2,"command":"get_mixer"}"#);
    for output in mixer["result"]["outputs"].as_array().unwrap() {
        assert_eq!(output["bus_id"], "bus_program");
        assert!(output.get("bus").is_none());
    }
    let caps = engine.request(r#"{"id":3,"command":"get_capture_capabilities"}"#);
    let screen = &caps["result"]["screen"];
    assert!(screen["target_kinds"].is_array(), "{caps}");
    assert!(screen["target_list"].is_boolean());
    assert!(screen.get("source_kinds").is_none() && screen.get("source_list").is_none());
    assert!(engine.finish().success());
}
