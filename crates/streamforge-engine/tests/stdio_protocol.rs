//! M1 acceptance: drive the real `streamforge-engine` process over stdio.
//!
//! Every wait is bounded by [`TIMEOUT`]; the child is killed if a test fails.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use control_protocol::MAX_REQUEST_BYTES;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

struct EngineProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    // Test-only channel; a reader thread forwards stdout lines so that every
    // receive can time out instead of blocking forever.
    lines: Receiver<String>,
}

impl EngineProcess {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_streamforge-engine"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn streamforge-engine");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(bytes).expect("write to engine");
        stdin.flush().expect("flush engine stdin");
    }

    fn send(&mut self, request: &str) {
        self.send_raw(format!("{request}\n").as_bytes());
    }

    fn recv(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|err| panic!("no response within {TIMEOUT:?}: {err}"));
        serde_json::from_str(&line)
            .unwrap_or_else(|err| panic!("response is not JSON ({err}): {line}"))
    }

    fn request(&mut self, request: &str) -> Value {
        self.send(request);
        self.recv()
    }

    /// Close stdin, wait for exit, and assert no unanswered output remains.
    fn finish(mut self) -> ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll engine") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "engine did not exit within {TIMEOUT:?} after stdin closed"
            );
            thread::sleep(Duration::from_millis(10));
        };
        match self.lines.recv_timeout(TIMEOUT) {
            Err(RecvTimeoutError::Disconnected) => {}
            Ok(line) => panic!("unexpected extra output: {line}"),
            Err(RecvTimeoutError::Timeout) => panic!("stdout not closed after exit"),
        }
        status
    }
}

impl Drop for EngineProcess {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

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
    assert_eq!(response["result"]["recording_active"], false);
    assert_eq!(response["result"]["streaming_active"], false);
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
        r#"{"id":1,"command":"probe_screen_capture","params":{"source_types":["printer"]}}"#,
    );
    assert_error(&response, &json!(1), "invalid_params");
    let response =
        engine.request(r#"{"id":2,"command":"probe_audio_capture","params":{"duration_ms":0}}"#);
    assert_error(&response, &json!(2), "invalid_params");
    assert!(engine.finish().success());
}
