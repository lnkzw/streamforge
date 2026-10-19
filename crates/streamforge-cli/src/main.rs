use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use command_api::{Command, CommandHandler, CommandOutput};
use engine::Engine;
use serde_json::{Map, Value};

#[derive(Debug, Parser)]
#[command(
    name = "streamforge",
    version,
    about = "Agent-friendly cross-platform streaming engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Print current engine status (initial scaffold only).
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect and test screen/audio capture. Output is JSON.
    #[command(subcommand)]
    Capture(CaptureCommand),
    /// Composite the 1920x1080@60 reference scene and report render timing. Output is JSON.
    Compose {
        /// Run time in milliseconds (100..=60000, default 5000).
        #[arg(long)]
        duration_ms: Option<u64>,
        /// Add a live screen layer (opens the system picker).
        #[arg(long)]
        screen: bool,
        /// With --screen: offered source type (monitor, window, virtual). Repeatable.
        #[arg(long = "source", requires = "screen")]
        sources: Vec<String>,
        /// With --screen: cursor mode (hidden, embedded, metadata).
        #[arg(long, requires = "screen")]
        cursor: Option<String>,
        /// Write the last composited frame to this new .ppm file.
        #[arg(long)]
        snapshot: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum CaptureCommand {
    /// Report capture capabilities of the running system.
    Caps,
    /// List audio input devices.
    Inputs,
    /// Capture a screen or window chosen in the system dialog and report diagnostics.
    Screen {
        /// Probe duration in milliseconds (100..=60000, default 5000).
        #[arg(long)]
        duration_ms: Option<u64>,
        /// Offered source type: monitor, window or virtual. Repeatable; default monitor and window.
        #[arg(long = "source")]
        sources: Vec<String>,
        /// Cursor mode: hidden, embedded (default) or metadata.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Capture an audio input and report format, levels and drops. No audio is stored.
    Audio {
        /// Input name from `capture inputs`; default is the system default input.
        #[arg(long)]
        input: Option<String>,
        /// Probe duration in milliseconds (100..=60000, default 5000).
        #[arg(long)]
        duration_ms: Option<u64>,
    },
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    // In-process engine; the CLI shares command handlers with the protocol
    // server instead of computing results itself.
    let mut engine = Engine::with_capture(capture_platform::backend());

    match cli.command {
        Commands::Status { json } => {
            let status = match engine.handle(Command::GetStatus)? {
                CommandOutput::Status(status) => status,
                other => bail!("engine returned unexpected output for get_status: {other:?}"),
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("{} {}", status.application, status.version);
                println!("Media engine ready: {}", status.media_engine_ready);
                println!("Capture available: {}", status.capture_available);
                println!("Streaming active: {}", status.streaming_active);
                println!("Recording active: {}", status.recording_active);
                println!();
                println!(
                    "Capture probes: `streamforge capture --help`. Recording and streaming are not implemented yet."
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Capture(capture) => {
            let (name, params) = capture_request(capture);
            run_command(&mut engine, name, params)
        }
        Commands::Compose {
            duration_ms,
            screen,
            sources,
            cursor,
            snapshot,
        } => {
            let mut params = Map::new();
            if let Some(duration_ms) = duration_ms {
                params.insert("duration_ms".into(), duration_ms.into());
            }
            if screen {
                params.insert("screen".into(), true.into());
            }
            if !sources.is_empty() {
                params.insert("source_types".into(), sources.into());
            }
            if let Some(cursor) = cursor {
                params.insert("cursor".into(), cursor.into());
            }
            if let Some(snapshot) = snapshot {
                params.insert("snapshot_path".into(), snapshot.into());
            }
            run_command(&mut engine, "probe_composite", params)
        }
    }
}

/// Translate CLI arguments into the protocol's command name and parameters, so
/// defaults and validation come from `Command::decode` exactly as for JSON clients.
fn capture_request(command: CaptureCommand) -> (&'static str, Map<String, Value>) {
    let mut params = Map::new();
    let name = match command {
        CaptureCommand::Caps => "get_capture_capabilities",
        CaptureCommand::Inputs => "list_audio_inputs",
        CaptureCommand::Screen {
            duration_ms,
            sources,
            cursor,
        } => {
            if let Some(duration_ms) = duration_ms {
                params.insert("duration_ms".into(), duration_ms.into());
            }
            if !sources.is_empty() {
                params.insert("source_types".into(), sources.into());
            }
            if let Some(cursor) = cursor {
                params.insert("cursor".into(), cursor.into());
            }
            "probe_screen_capture"
        }
        CaptureCommand::Audio { input, duration_ms } => {
            if let Some(input) = input {
                params.insert("input".into(), input.into());
            }
            if let Some(duration_ms) = duration_ms {
                params.insert("duration_ms".into(), duration_ms.into());
            }
            "probe_audio_capture"
        }
    };
    (name, params)
}

fn run_command(engine: &mut Engine, name: &str, params: Map<String, Value>) -> Result<ExitCode> {
    let result = Command::decode(name, Some(&Value::Object(params)))
        .and_then(|command| engine.handle(command));
    match result {
        Ok(output) => {
            println!("{}", serde_json::to_string_pretty(&output)?);
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            eprintln!("error: {err}");
            if let Some(details) = &err.details {
                eprintln!("details: {details}");
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_arguments_become_protocol_params() {
        let (name, params) = capture_request(CaptureCommand::Screen {
            duration_ms: Some(1500),
            sources: vec!["window".into()],
            cursor: Some("hidden".into()),
        });
        assert_eq!(name, "probe_screen_capture");
        assert_eq!(
            Value::Object(params),
            serde_json::json!({"duration_ms": 1500, "source_types": ["window"], "cursor": "hidden"})
        );
    }

    #[test]
    fn omitted_arguments_use_protocol_defaults() {
        let (name, params) = capture_request(CaptureCommand::Audio {
            input: None,
            duration_ms: None,
        });
        assert_eq!(name, "probe_audio_capture");
        assert!(params.is_empty());
        assert!(Command::decode(name, Some(&Value::Object(params))).is_ok());
    }
}
