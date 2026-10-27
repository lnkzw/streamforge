//! The engine under test: a `streamforge-engine` child process, spoken to
//! over its JSON-lines protocol as every client does. Every wait is
//! bounded, and the child is killed on drop.

use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

/// A command the engine answered with an error.
#[derive(Debug)]
pub struct CommandFailed {
    pub command: String,
    pub code: String,
    pub message: String,
    /// Partial reports of a failed stop, warnings.
    pub details: Value,
}

impl fmt::Display for CommandFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.command, self.message, self.code)?;
        if !self.details.is_null() {
            write!(f, "; details: {}", self.details)?;
        }
        Ok(())
    }
}

impl std::error::Error for CommandFailed {}

pub struct Engine {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Lines from the engine's stdout, forwarded by a reader thread so that
    /// every wait can time out.
    lines: Receiver<String>,
    next_id: u64,
}

impl Engine {
    /// Start `exe` with its files in `output_dir`; its diagnostics (stderr)
    /// go to `log`.
    pub fn spawn(exe: &Path, output_dir: &Path, log: File) -> Result<Self> {
        let mut child = Command::new(exe)
            .arg("--output-dir")
            .arg(output_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .with_context(|| format!("cannot start {}", exe.display()))?;
        let stdin = child.stdin.take().context("the engine has no stdin")?;
        let stdout = child.stdout.take().context("the engine has no stdout")?;
        // Responses come one per request; a few lines of room are enough.
        let (sender, lines) = mpsc::sync_channel(16);
        thread::Builder::new()
            .name("engine-stdout".to_owned())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if sender.send(line).is_err() {
                        break;
                    }
                }
            })
            .context("cannot start the engine reader")?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            lines,
            next_id: 1,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Send one command and wait up to `timeout` for its result. An error
    /// response is an `Err` holding a [`CommandFailed`].
    pub fn request(&mut self, command: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"id": id, "command": command});
        if !params.is_null() {
            request["params"] = params;
        }
        let stdin = self
            .stdin
            .as_mut()
            .context("the engine's stdin is closed")?;
        writeln!(stdin, "{request}")
            .and_then(|()| stdin.flush())
            .with_context(|| format!("{command}: cannot write to the engine (did it exit?)"))?;
        // One response per request, in order (`docs/protocol.md`).
        let line = match self.lines.recv_timeout(timeout) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => bail!("{command}: no response within {timeout:?}"),
            Err(RecvTimeoutError::Disconnected) => {
                bail!("{command}: the engine closed its output (did it exit?)")
            }
        };
        let response: Value = serde_json::from_str(&line)
            .with_context(|| format!("{command}: not a JSON response: {line}"))?;
        if response["id"] != id {
            bail!("{command}: a response for another request: {line}");
        }
        if response["ok"] == true {
            return Ok(response["result"].clone());
        }
        let error = &response["error"];
        Err(anyhow!(CommandFailed {
            command: command.to_owned(),
            code: error["code"].as_str().unwrap_or_default().to_owned(),
            message: error["message"].as_str().unwrap_or_default().to_owned(),
            details: error["details"].clone(),
        }))
    }

    /// Close stdin, which ends the engine, and wait up to `timeout` for it
    /// to exit.
    pub fn finish(mut self, timeout: Duration) -> Result<ExitStatus> {
        drop(self.stdin.take());
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .context("cannot wait for the engine")?
            {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                bail!("the engine did not exit within {timeout:?} after its stdin closed");
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
