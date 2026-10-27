//! Ctrl+C on `streamforge record` through the real binary: the recording
//! stops as at the end of `--duration-ms` and its report follows.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(20);

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

/// Killed on drop, so a failing test leaves no process behind.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn an_interrupt_stops_the_recording_and_prints_its_report() {
    let dir = TempDir::new("interrupt");
    let mut child = Running(
        Command::new(env!("CARGO_BIN_EXE_streamforge"))
            .arg("--output-dir")
            .arg(&dir.0)
            .args([
                "record",
                "--file-name",
                "take.mkv",
                "--duration-ms",
                "60000",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn streamforge"),
    );
    let mut stdout = child.0.stdout.take().expect("piped stdout");
    let stderr = child.0.stderr.take().expect("piped stderr");
    let (lines, progress) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let mut seen = Vec::new();
    let mut wait_for = |what: &str| {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match progress.recv_timeout(left) {
                Ok(line) if line.contains(what) => return,
                Ok(line) => seen.push(line),
                Err(err) => panic!("no {what:?} on stderr ({err}); saw {seen:?}"),
            }
        }
    };
    // A second of recording (the first progress line), then Ctrl+C.
    wait_for(" s  recording");
    let sent = Command::new("kill")
        .arg("-INT")
        .arg(child.0.id().to_string())
        .status()
        .expect("run kill");
    assert!(sent.success());
    wait_for("interrupted after");

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("poll streamforge") {
            break status;
        }
        assert!(Instant::now() < deadline, "no exit within {TIMEOUT:?}");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "{status}");
    let mut report = String::new();
    stdout.read_to_string(&mut report).expect("read stdout");
    let report: Value = serde_json::from_str(&report).expect("a JSON report");
    let seconds = report["recording"]["duration_ms"]
        .as_f64()
        .unwrap_or_else(|| panic!("{report}"))
        / 1000.0;
    assert!((0.8..20.0).contains(&seconds), "{seconds} s: {report}");
    assert!(dir.0.join("take.mkv").is_file());
}
