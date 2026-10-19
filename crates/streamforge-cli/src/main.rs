use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

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
    /// Directory for files created by commands (snapshots).
    #[arg(long, global = true, default_value = ".")]
    output_dir: PathBuf,
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
    /// Record the 1920x1080@60 reference scene and the audio mix to a new Matroska file
    /// (H.264 + AAC), then print the report. Output is JSON.
    Record {
        /// Plain .mkv file name, created in --output-dir; never overwritten.
        #[arg(long)]
        file: String,
        /// Recording length in milliseconds.
        #[arg(long, default_value_t = 10_000)]
        duration_ms: u64,
        /// Add a live screen layer (opens the system picker).
        #[arg(long)]
        screen: bool,
        /// With --screen: offered source type (monitor, window, virtual). Repeatable.
        #[arg(long = "source", requires = "screen")]
        sources: Vec<String>,
        /// With --screen: cursor mode (hidden, embedded, metadata).
        #[arg(long, requires = "screen")]
        cursor: Option<String>,
        /// Live input as NAME or NAME=GAIN_DB. Repeatable.
        #[arg(long = "input")]
        inputs: Vec<String>,
        /// Test tone as FREQ_HZ or FREQ_HZ@LEVEL_DBFS. Repeatable; default one 1 kHz tone at -20 dBFS.
        #[arg(long = "tone", conflicts_with = "no_tone")]
        tones: Vec<String>,
        /// Record without the default test tone.
        #[arg(long)]
        no_tone: bool,
        /// Master gain in dB (default 0).
        #[arg(long)]
        master_gain_db: Option<f32>,
    },
    /// Mix audio inputs and test tones at 48 kHz stereo; reports meters and buffering. Output is JSON.
    Mix {
        /// Live input as NAME or NAME=GAIN_DB (name from `capture inputs`). Repeatable.
        #[arg(long = "input")]
        inputs: Vec<String>,
        /// Test tone as FREQ_HZ or FREQ_HZ@LEVEL_DBFS. Repeatable; default one 1 kHz tone
        /// when no input or tone is given.
        #[arg(long = "tone")]
        tones: Vec<String>,
        /// Master gain in dB (default 0).
        #[arg(long)]
        master_gain_db: Option<f32>,
        /// Run time in milliseconds (100..=900000, default 5000).
        #[arg(long)]
        duration_ms: Option<u64>,
    },
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
        /// Write the last composited frame to this new .ppm file. A plain file
        /// name (no directories), created in --output-dir; never overwritten.
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
    let mut engine =
        Engine::with_capture(capture_platform::backend()).with_output_dir(cli.output_dir);

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
        Commands::Mix {
            inputs,
            tones,
            master_gain_db,
            duration_ms,
        } => match mix_params(&inputs, &tones, master_gain_db, duration_ms) {
            Ok(params) => run_command(&mut engine, "probe_audio_mix", params),
            Err(message) => {
                eprintln!("error: {message}");
                Ok(ExitCode::FAILURE)
            }
        },
        Commands::Record {
            file,
            duration_ms,
            screen,
            sources,
            cursor,
            inputs,
            tones,
            no_tone,
            master_gain_db,
        } => {
            let mut params = match mix_params(&inputs, &tones, master_gain_db, None) {
                Ok(params) => params,
                Err(message) => {
                    eprintln!("error: {message}");
                    return Ok(ExitCode::FAILURE);
                }
            };
            params.insert("file_name".into(), file.into());
            if no_tone {
                params.insert("tones".into(), Value::Array(Vec::new()));
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
            record(&mut engine, params, Duration::from_millis(duration_ms))
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

/// Build `probe_audio_mix` params from `NAME[=GAIN_DB]` and `FREQ[@LEVEL]`
/// arguments; value validation is left to `Command::decode`.
fn mix_params(
    inputs: &[String],
    tones: &[String],
    master_gain_db: Option<f32>,
    duration_ms: Option<u64>,
) -> Result<Map<String, Value>, String> {
    // Non-finite values would serialize as JSON null and silently become
    // defaults, so they are rejected here.
    let number = |text: &str, what: &str| -> Result<f64, String> {
        text.parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .ok_or_else(|| format!("invalid {what} `{text}`: expected a finite number"))
    };
    let mut params = Map::new();
    let mut input_list = Vec::new();
    for spec in inputs {
        // Only a numeric suffix is a gain; node names may contain '='.
        let (name, gain) = match spec.rsplit_once('=') {
            Some((name, gain)) if !name.is_empty() && gain.parse::<f64>().is_ok() => {
                (name, Some(number(gain, "gain")?))
            }
            _ => (spec.as_str(), None),
        };
        let mut input = Map::new();
        input.insert("input".into(), name.into());
        if let Some(gain) = gain {
            input.insert("gain_db".into(), gain.into());
        }
        input_list.push(Value::Object(input));
    }
    let mut tone_list = Vec::new();
    for spec in tones {
        let (frequency, level) = match spec.split_once('@') {
            Some((frequency, level)) => (frequency, Some(number(level, "level")?)),
            None => (spec.as_str(), None),
        };
        let mut tone = Map::new();
        tone.insert(
            "frequency_hz".into(),
            number(frequency, "frequency")?.into(),
        );
        if let Some(level) = level {
            tone.insert("level_dbfs".into(), level.into());
        }
        tone_list.push(Value::Object(tone));
    }
    if !input_list.is_empty() {
        params.insert("inputs".into(), input_list.into());
    }
    if !tone_list.is_empty() {
        params.insert("tones".into(), tone_list.into());
    }
    if let Some(gain) = master_gain_db {
        if !gain.is_finite() {
            return Err(format!(
                "invalid master gain `{gain}`: expected a finite number"
            ));
        }
        params.insert("master_gain_db".into(), gain.into());
    }
    if let Some(duration_ms) = duration_ms {
        params.insert("duration_ms".into(), duration_ms.into());
    }
    Ok(params)
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

/// Start a recording, wait, stop it and print the report. Progress goes to
/// stderr so stdout carries only the JSON report.
fn record(engine: &mut Engine, params: Map<String, Value>, duration: Duration) -> Result<ExitCode> {
    let started = Command::decode("start_recording", Some(&Value::Object(params)))
        .and_then(|command| engine.handle(command));
    match started {
        Ok(CommandOutput::RecordingStarted(started)) => {
            eprintln!(
                "recording {} for {:.1} s ...",
                started.path,
                duration.as_secs_f64()
            );
        }
        Ok(other) => bail!("engine returned unexpected output for start_recording: {other:?}"),
        Err(err) => return Ok(report_error(&err)),
    }
    std::thread::sleep(duration);
    match engine.handle(Command::StopRecording) {
        Ok(output) => {
            println!("{}", serde_json::to_string_pretty(&output)?);
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => Ok(report_error(&err)),
    }
}

fn report_error(err: &command_api::CommandError) -> ExitCode {
    eprintln!("error: {err}");
    if let Some(details) = &err.details {
        eprintln!("details: {details}");
    }
    ExitCode::FAILURE
}

fn run_command(engine: &mut Engine, name: &str, params: Map<String, Value>) -> Result<ExitCode> {
    let result = Command::decode(name, Some(&Value::Object(params)))
        .and_then(|command| engine.handle(command));
    match result {
        Ok(output) => {
            println!("{}", serde_json::to_string_pretty(&output)?);
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => Ok(report_error(&err)),
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
    fn mix_arguments_become_protocol_params() {
        let params = mix_params(
            &["mic=-6".into(), "alsa.input".into()],
            &["440@-18".into(), "1000".into()],
            Some(-1.0),
            Some(600_000),
        )
        .expect("params");
        assert_eq!(
            Value::Object(params.clone()),
            serde_json::json!({
                "inputs": [{"input": "mic", "gain_db": -6.0}, {"input": "alsa.input"}],
                "tones": [{"frequency_hz": 440.0, "level_dbfs": -18.0}, {"frequency_hz": 1000.0}],
                "master_gain_db": -1.0,
                "duration_ms": 600_000,
            })
        );
        assert!(Command::decode("probe_audio_mix", Some(&Value::Object(params))).is_ok());
        assert!(mix_params(&[], &["loud".into()], None, None).is_err());
    }

    #[test]
    fn non_finite_values_are_rejected_and_names_may_contain_equals() {
        assert!(mix_params(&["mic=nan".into()], &[], None, None).is_err());
        assert!(mix_params(&[], &["440@inf".into()], None, None).is_err());
        assert!(mix_params(&[], &[], Some(f32::NAN), None).is_err());
        let params = mix_params(&["node=with=equals".into()], &[], None, None).expect("params");
        assert_eq!(params["inputs"][0]["input"], "node=with=equals");
        let params = mix_params(&["a=b=-3".into()], &[], None, None).expect("params");
        assert_eq!(params["inputs"][0]["input"], "a=b");
        assert_eq!(params["inputs"][0]["gain_db"], -3.0);
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
