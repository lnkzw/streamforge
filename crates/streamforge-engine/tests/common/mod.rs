//! Shared harness: the real `streamforge-engine` process over stdio.
//!
//! Every wait is bounded by [`TIMEOUT`]; the child is killed on drop, so a
//! failing test never leaves a process behind.
#![allow(dead_code)] // each test binary uses a different subset

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Tests that run the media pipeline (1080p60 compositor, x264 encoders)
/// take turns within a test binary: in parallel they overload small CI
/// runners until encoders fall seconds behind and drop frames.
pub fn media_lock() -> MutexGuard<'static, ()> {
    static MEDIA: Mutex<()> = Mutex::new(());
    // A failed test only poisons the lock; the next one still runs.
    MEDIA
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An empty directory for one test, removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "streamforge-engine-test-{}-{name}",
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

pub struct EngineProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    // Test-only channel; a reader thread forwards stdout lines so that every
    // receive can time out instead of blocking forever.
    lines: Receiver<String>,
}

impl EngineProcess {
    pub fn spawn() -> Self {
        Self::spawn_with::<&str>(&[])
    }

    pub fn spawn_with<S: AsRef<OsStr>>(args: &[S]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_streamforge-engine"))
            .args(args)
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

    pub fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(bytes).expect("write to engine");
        stdin.flush().expect("flush engine stdin");
    }

    pub fn send(&mut self, request: &str) {
        self.send_raw(format!("{request}\n").as_bytes());
    }

    pub fn recv(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|err| panic!("no response within {TIMEOUT:?}: {err}"));
        serde_json::from_str(&line)
            .unwrap_or_else(|err| panic!("response is not JSON ({err}): {line}"))
    }

    pub fn request(&mut self, request: &str) -> Value {
        self.send(request);
        self.recv()
    }

    /// Close stdin, wait for exit, and assert no unanswered output remains.
    pub fn finish(mut self) -> ExitStatus {
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
