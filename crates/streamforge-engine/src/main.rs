//! Headless StreamForge engine process.
//!
//! Serves the JSON-lines control protocol (see `docs/protocol.md`) on
//! stdin/stdout. stdout carries protocol messages only; diagnostics go to
//! stderr. Exits with status 0 when stdin closes.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use command_api::{AccessPolicy, CommandAccess, Gated};
use engine::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Allow {
    ReadOnly,
    Control,
    Sensitive,
}

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
    /// Highest class of commands the client may run (the client is whoever
    /// controls stdin; restrict it when that is not you).
    #[arg(long, value_enum, default_value_t = Allow::Sensitive)]
    allow: Allow,
}

fn main() -> ExitCode {
    let Args { output_dir, allow } = Args::parse();
    let engine = Engine::with_capture(capture_platform::backend()).with_output_dir(output_dir);
    let policy = AccessPolicy::allowing(match allow {
        Allow::ReadOnly => CommandAccess::ReadOnly,
        Allow::Control => CommandAccess::Control,
        Allow::Sensitive => CommandAccess::Sensitive,
    });
    let mut handler = Gated::new(engine, policy);
    match control_protocol::serve(io::stdin().lock(), io::stdout().lock(), &mut handler) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("streamforge-engine: control channel I/O error: {err}");
            ExitCode::FAILURE
        }
    }
}
