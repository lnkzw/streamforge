//! The RTMP ingest of the stream scenarios, in a child process (this
//! binary started with `--ingest`). Its receive, demux and stream-probe
//! work would otherwise count as the measured process's CPU time.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rtmp_test_server::{ServerOptions, TestServer};

/// The child side: listen, print `url <url>`, read one publisher until it
/// disconnects, and print `error <message>` if the ingest failed.
pub fn serve() -> Result<()> {
    let server = TestServer::start(ServerOptions {
        accept_timeout: Duration::from_secs(60),
        ..ServerOptions::default()
    });
    let mut stdout = std::io::stdout();
    writeln!(stdout, "url {}", server.url())?;
    stdout.flush()?;
    let report = server.join();
    if let Some(error) = report.error {
        writeln!(stdout, "error {error}")?;
    }
    Ok(())
}

/// A running ingest child; killed on drop unless [`Ingest::finish`] ran.
pub struct Ingest {
    child: Option<Child>,
    stdout: BufReader<ChildStdout>,
    pub url: String,
}

impl Ingest {
    pub fn spawn() -> Result<Self> {
        let exe = std::env::current_exe().context("cannot find this executable")?;
        let mut child = Command::new(exe)
            .arg("--ingest")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .context("cannot start the ingest process")?;
        let stdout = child.stdout.take().context("the ingest has no stdout")?;
        let mut ingest = Self {
            child: Some(child),
            stdout: BufReader::new(stdout),
            url: String::new(),
        };
        let mut line = String::new();
        ingest
            .stdout
            .read_line(&mut line)
            .context("cannot read from the ingest")?;
        match line.trim().strip_prefix("url ") {
            Some(url) => ingest.url = url.to_owned(),
            None => bail!("the ingest did not start: {line:?}"),
        }
        Ok(ingest)
    }

    /// After the stream stopped: wait for the ingest and return its error,
    /// if it reported one.
    pub fn finish(mut self) -> Result<Option<String>> {
        let mut rest = String::new();
        self.stdout
            .read_to_string(&mut rest)
            .context("cannot read from the ingest")?;
        let status = self
            .child
            .take()
            .context("the ingest already ended")?
            .wait()
            .context("cannot wait for the ingest")?;
        let error = rest
            .lines()
            .find_map(|line| line.strip_prefix("error "))
            .map(str::to_owned);
        if !status.success() && error.is_none() {
            return Ok(Some(format!("the ingest exited with {status}")));
        }
        Ok(error)
    }
}

impl Drop for Ingest {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
