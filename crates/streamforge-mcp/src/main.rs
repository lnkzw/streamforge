//! `streamforge-mcp`: an MCP server on stdin/stdout. Agents get the tools
//! the person starting it allows; diagnostics go to stderr (stdout is the
//! protocol).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use command_api::{AccessPolicy, CommandAccess};
use encoder_api::{StreamKey, StreamTarget};
use engine::{Engine, EngineThread};
use rmcp::ServiceExt;
use streamforge_mcp::{Config, StreamforgeMcp};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Allow {
    /// Inspect status, scenes, mixer and capabilities only.
    ReadOnly,
    /// Also change scenes and mixer, add color bars and solid sources, and
    /// stop outputs.
    Control,
    /// Also capture (screen, microphones), record, stream, delete scenes.
    Sensitive,
}

#[derive(Debug, Parser)]
#[command(version, about = "StreamForge MCP server (stdio)")]
struct Args {
    /// Highest class of commands agents may run.
    #[arg(long, value_enum, default_value_t = Allow::ReadOnly)]
    allow: Allow,
    /// Directory for files the engine creates (recordings, snapshots).
    #[arg(long, default_value = ".")]
    output_dir: PathBuf,
    /// The only RTMP target agents may stream to (needs --allow sensitive);
    /// without it, start_streaming is not offered.
    #[arg(long)]
    stream_url: Option<String>,
    /// Environment variable holding the stream key for --stream-url.
    #[arg(long, requires = "stream_url")]
    stream_key_env: Option<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let stream = match stream_target(&args) {
        Ok(stream) => stream,
        Err(message) => {
            eprintln!("streamforge-mcp: {message}");
            return ExitCode::FAILURE;
        }
    };
    let policy = AccessPolicy::allowing(match args.allow {
        Allow::ReadOnly => CommandAccess::ReadOnly,
        Allow::Control => CommandAccess::Control,
        Allow::Sensitive => CommandAccess::Sensitive,
    });
    eprintln!(
        "streamforge-mcp: allowing {} commands{}",
        command_api::access_label(policy.max()),
        stream
            .as_ref()
            .map(|target| format!("; streaming target {}", target.redacted()))
            .unwrap_or_default()
    );
    let output_dir = args.output_dir;
    let engine = match EngineThread::spawn(move || {
        Engine::with_capture(capture_platform::backend()).with_output_dir(output_dir)
    }) {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("streamforge-mcp: cannot start the engine: {err}");
            return ExitCode::FAILURE;
        }
    };
    let server = StreamforgeMcp::new(engine.handle(), Config { policy, stream });
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("streamforge-mcp: cannot start the runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let served = runtime.block_on(async {
        let service = server.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    });
    // Dropping the runtime and then the engine finalizes open outputs.
    drop(runtime);
    drop(engine);
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("streamforge-mcp: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The configured streaming target, validated now so misconfiguration fails
/// at startup. Errors never contain the key.
fn stream_target(args: &Args) -> Result<Option<StreamTarget>, String> {
    let Some(url) = &args.stream_url else {
        return Ok(None);
    };
    if args.allow != Allow::Sensitive {
        return Err("--stream-url needs --allow sensitive".to_owned());
    }
    let key = match &args.stream_key_env {
        Some(name) => std::env::var(name)
            .map_err(|_| format!("${name} (from --stream-key-env) is not set or not UTF-8"))?,
        None => String::new(),
    };
    StreamTarget::new(url, StreamKey::new(key))
        .map(Some)
        .map_err(|message| format!("invalid streaming target: {message}"))
}
