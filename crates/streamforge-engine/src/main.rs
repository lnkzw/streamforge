//! Headless StreamForge engine process.
//!
//! Serves the JSON-lines control protocol (see `docs/protocol.md`) on
//! stdin/stdout. stdout carries protocol messages only; diagnostics go to
//! stderr. Exits with status 0 when stdin closes.

use std::io;
use std::process::ExitCode;

use clap::Parser;
use engine::Engine;

#[derive(Debug, Parser)]
#[command(
    name = "streamforge-engine",
    version,
    about = "Headless StreamForge engine speaking JSON lines on stdin/stdout",
    long_about = "Headless StreamForge engine.\n\nReads one JSON request per line on stdin and writes one JSON response per line on stdout, e.g.\n  {\"id\":1,\"command\":\"get_status\"}\nExits when stdin is closed. See docs/protocol.md."
)]
struct Args {}

fn main() -> ExitCode {
    let Args {} = Args::parse();
    let mut engine = Engine::with_capture(capture_platform::backend());
    match control_protocol::serve(io::stdin().lock(), io::stdout().lock(), &mut engine) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("streamforge-engine: control channel I/O error: {err}");
            ExitCode::FAILURE
        }
    }
}
