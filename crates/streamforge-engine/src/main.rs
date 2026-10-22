//! Headless StreamForge engine process.
//!
//! Serves the JSON-lines control protocol (see `docs/protocol.md`) on
//! stdin/stdout. stdout carries protocol messages only; diagnostics go to
//! stderr. Exits with status 0 when stdin closes.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use command_api::{AccessPolicy, CommandAccess, Gated};
use engine::Engine;

#[derive(Debug, Parser)]
#[command(
    name = "streamforge-engine",
    version,
    about = "Headless StreamForge engine speaking JSON lines on stdin/stdout",
    long_about = "Headless StreamForge engine.\n\nReads one JSON request per line on stdin and writes one JSON response per line on stdout, e.g.\n  {\"id\":1,\"command\":\"get_status\"}\nExits when stdin is closed. See docs/protocol.md."
)]
struct Args {
    /// Directory where commands may create files (e.g. composite snapshots).
    /// Clients only choose file names inside it.
    #[arg(long, default_value = ".")]
    output_dir: PathBuf,
    /// Directory for project files (save_project, load_project,
    /// validate_project); clients only choose file names inside it.
    /// Without it, the project commands are unavailable.
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
    match control_protocol::serve(io::stdin().lock(), io::stdout().lock(), &mut handler) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("streamforge-engine: control channel I/O error: {err}");
            ExitCode::FAILURE
        }
    }
}
