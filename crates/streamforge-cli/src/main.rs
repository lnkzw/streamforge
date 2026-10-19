use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use command_api::{Command, CommandHandler, CommandOutput};
use engine::Engine;

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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // In-process engine; the CLI shares command handlers with the protocol
    // server instead of computing results itself.
    let mut engine = Engine::new();

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
                    "Scaffold only: capture, recording, and streaming are not implemented yet."
                );
            }
        }
    }

    Ok(())
}
