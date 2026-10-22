//! `streamforge-mcp`: an MCP server on stdin/stdout. Agents get the tools
//! the person starting it allows; diagnostics go to stderr (stdout is the
//! protocol).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use command_api::{AccessPolicy, CommandAccess, access_label};
use encoder_api::{StreamKey, StreamTarget};
use engine::{Engine, EngineThread};
use rmcp::ServiceExt;
use streamforge_mcp::{Config, StreamforgeMcp};

#[derive(Debug, Parser)]
#[command(version, about = "StreamForge MCP server (stdio)")]
struct Args {
    /// Highest class of commands agents may run: read-only (inspect
    /// status, scenes, mixer), control (also edit scenes and mixer, add
    /// color bars and solid sources, stop outputs) or sensitive (also
    /// capture, record, stream, delete scenes).
    #[arg(long, default_value = "read-only")]
    allow: CommandAccess,
    /// Directory for files the engine creates (recordings, snapshots).
    #[arg(long, default_value = ".")]
    output_dir: PathBuf,
    /// Directory for project files (save_project and load_project need
    /// --allow sensitive; validate_project is read-only). Without it, the
    /// project commands are unavailable.
    #[arg(long)]
    project_dir: Option<PathBuf>,
    /// The only RTMP target agents may stream to (needs --allow sensitive);
    /// without it, start_streaming is not offered.
    #[arg(long)]
    stream_url: Option<String>,
    /// Name of the environment variable holding the stream key for
    /// --stream-url (the key itself never goes on the command line).
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
    let policy = AccessPolicy::allowing(args.allow);
    eprintln!(
        "streamforge-mcp: allowing {} commands{}",
        access_label(policy.max()),
        stream
            .as_ref()
            .map(|target| format!("; streaming target {}", target.redacted()))
            .unwrap_or_default()
    );
    let (output_dir, project_dir) = (args.output_dir, args.project_dir);
    let engine = match EngineThread::spawn(move || {
        let engine = Engine::with_capture(capture_platform::backend()).with_output_dir(output_dir);
        match project_dir {
            Some(dir) => engine.with_project_dir(dir),
            None => engine,
        }
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
        // Clients usually stop stdio servers with a signal; finish cleanly
        // then too, so open recordings and streams are finalized.
        tokio::select! {
            quit = service.waiting() => {
                quit?;
            }
            () = shutdown_signal() => {}
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    });
    // The engine first: queued commands are cancelled (their callers on the
    // runtime's blocking threads get answers), the running one finishes,
    // open outputs are finalized. Then the runtime, without waiting for its
    // stdin reader, which blocks until the client closes stdin.
    drop(engine);
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("streamforge-mcp: {err}");
            ExitCode::FAILURE
        }
    }
}

/// SIGINT or SIGTERM (Ctrl-C elsewhere).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// The configured streaming target, validated now so misconfiguration fails
/// at startup. Errors contain neither the key nor the variable name (in
/// case the key was passed where the name belongs).
fn stream_target(args: &Args) -> Result<Option<StreamTarget>, String> {
    let Some(url) = &args.stream_url else {
        return Ok(None);
    };
    if args.allow != CommandAccess::Sensitive {
        return Err("--stream-url needs --allow sensitive".to_owned());
    }
    let key = match &args.stream_key_env {
        Some(name) => match std::env::var(name) {
            Ok(key) if !key.is_empty() => key,
            Ok(_) => {
                return Err("the variable named by --stream-key-env is empty".to_owned());
            }
            Err(_) => {
                return Err(
                    "the variable named by --stream-key-env is not set (or not UTF-8)".to_owned(),
                );
            }
        },
        None => String::new(),
    };
    StreamTarget::new(url, StreamKey::new(key))
        .map(Some)
        .map_err(|message| format!("invalid streaming target: {message}"))
}
