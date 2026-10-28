//! Headless StreamForge engine process.
//!
//! Serves the JSON-lines control protocol (see `docs/protocol.md`) on
//! stdin/stdout. stdout carries protocol messages only; diagnostics go to
//! stderr. Exits with status 0 when stdin closes or at Ctrl+C or SIGTERM,
//! finalizing open recordings and streams.

use std::io::{self, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use clap::Parser;
use command_api::{AccessPolicy, CommandAccess, Gated};
use engine::Engine;

#[derive(Debug, Parser)]
#[command(
    name = "streamforge-engine",
    version,
    about = "Headless StreamForge engine speaking JSON lines on stdin/stdout",
    long_about = "Headless StreamForge engine.\n\nReads one JSON request per line on stdin and writes one JSON response per line on stdout, e.g.\n  {\"id\":1,\"command\":\"get_status\"}\nExits when stdin is closed or at Ctrl+C or SIGTERM, finalizing open recordings and streams. See docs/protocol.md."
)]
struct Args {
    /// Directory where commands may create files (e.g. composite snapshots).
    /// Clients only choose file names inside it.
    #[arg(long, default_value = ".")]
    output_dir: PathBuf,
    /// Directory for project files (list_projects, save_project,
    /// load_project, validate_project) and the images image sources show
    /// (list_images); clients only choose file names inside it.
    /// Without it, the project commands and image sources are unavailable.
    #[arg(long)]
    project_dir: Option<PathBuf>,
    /// Highest class of commands the client may run: read-only, control or
    /// sensitive (the client is whoever controls stdin; restrict it when
    /// that is not you).
    #[arg(long, default_value = "sensitive")]
    allow: CommandAccess,
}

fn main() -> ExitCode {
    let Args {
        output_dir,
        project_dir,
        allow,
    } = Args::parse();
    if let Some(dir) = &project_dir
        && !dir.is_dir()
    {
        eprintln!(
            "streamforge-engine: --project-dir {} is not a directory",
            dir.display()
        );
        return ExitCode::FAILURE;
    }
    let mut engine = Engine::with_capture(capture_platform::backend()).with_output_dir(output_dir);
    if let Some(dir) = project_dir {
        engine = engine.with_project_dir(dir);
    }
    let mut handler = Gated::new(engine, AccessPolicy::allowing(allow));
    let input = match Input::spawn() {
        Ok(input) => BufReader::new(input),
        Err(err) => {
            eprintln!("streamforge-engine: cannot read stdin: {err}");
            return ExitCode::FAILURE;
        }
    };
    match control_protocol::serve(input, io::stdout().lock(), &mut handler) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("streamforge-engine: control channel I/O error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Chunks of stdin in flight between its reader thread and the engine.
const CHUNKS: usize = 16;
const CHUNK_BYTES: usize = 8192;

/// Stdin, read on its own thread so that a quit signal can end it while a
/// read blocks: the engine then exits as at EOF, so open outputs are
/// finalized (dropping the engine does that). Requests not read by then
/// stay unanswered. A second signal exits at once (status 130),
/// unfinalized.
struct Input {
    chunks: Receiver<Chunk>,
    signalled: Arc<AtomicBool>,
    chunk: Vec<u8>,
    at: usize,
    ended: bool,
}

enum Chunk {
    Data(Vec<u8>),
    /// EOF, or a signal.
    End,
    Failed(io::Error),
}

impl Input {
    fn spawn() -> io::Result<Self> {
        let (sender, chunks) = mpsc::sync_channel(CHUNKS);
        let signalled = Arc::new(AtomicBool::new(false));
        let wake = sender.clone();
        let flag = Arc::clone(&signalled);
        let installed = quit_signal::on_quit("streamforge-engine", move || {
            flag.store(true, Ordering::Relaxed);
            // Wakes a blocked read; a full queue needs no wake, the next read
            // sees the flag.
            let _ = wake.try_send(Chunk::End);
            let _ = writeln!(
                io::stderr(),
                "streamforge-engine: stopping; signal again to quit at once"
            );
        });
        if let Err(err) = installed {
            eprintln!("streamforge-engine: cannot handle quit signals: {err}");
        }
        // Blocks in its read until EOF; the process may exit around it.
        thread::Builder::new()
            .name("stdin".to_owned())
            .spawn(move || read_stdin(&sender))?;
        Ok(Self {
            chunks,
            signalled,
            chunk: Vec::new(),
            at: 0,
            ended: false,
        })
    }
}

fn read_stdin(sender: &SyncSender<Chunk>) {
    let mut stdin = io::stdin().lock();
    let mut buffer = vec![0; CHUNK_BYTES];
    loop {
        let chunk = match stdin.read(&mut buffer) {
            Ok(0) => Chunk::End,
            Ok(read) => Chunk::Data(buffer[..read].to_vec()),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => Chunk::Failed(err),
        };
        let last = !matches!(chunk, Chunk::Data(_));
        if sender.send(chunk).is_err() || last {
            return;
        }
    }
}

impl Read for Input {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.chunk.len() {
            if self.ended || self.signalled.load(Ordering::Relaxed) {
                return Ok(0);
            }
            match self.chunks.recv() {
                Ok(Chunk::Data(data)) => {
                    self.chunk = data;
                    self.at = 0;
                }
                Ok(Chunk::End) | Err(_) => self.ended = true,
                Ok(Chunk::Failed(err)) => {
                    self.ended = true;
                    return Err(err);
                }
            }
        }
        let read = buf.len().min(self.chunk.len() - self.at);
        buf[..read].copy_from_slice(&self.chunk[self.at..self.at + read]);
        self.at += read;
        Ok(read)
    }
}
