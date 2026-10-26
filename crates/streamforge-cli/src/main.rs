use std::path::{Path, PathBuf};
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
    /// Print the engine status: version, capture availability, output states.
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect and test screen/audio capture. Output is JSON.
    #[command(subcommand)]
    Capture(CaptureCommand),
    /// Work with project files (saved scenes and audio setup).
    #[command(subcommand)]
    Project(ProjectCommand),
    /// Record the 1920x1080@60 reference scene and the audio mix to a new Matroska file
    /// (H.264 + AAC), then print the report. Output is JSON.
    Record {
        /// Plain .mkv file name, created in --output-dir; never overwritten.
        #[arg(long)]
        file_name: String,
        /// Recording length in milliseconds.
        #[arg(long, default_value_t = 10_000)]
        duration_ms: u64,
        /// H.264 encoder: auto (hardware if one works here, else software), software or hardware.
        #[arg(long)]
        video_encoder: Option<String>,
        #[command(flatten)]
        sources: SourceArgs,
    },
    /// Stream the 1920x1080@60 reference scene and the audio mix over RTMP (H.264 CBR + AAC),
    /// optionally recording at the same time, then print the reports. Output is JSON.
    ///
    /// The stream key is read from an environment variable, never from the command line
    /// (which other users and shell histories can see).
    Stream {
        /// Server URL without the key, e.g. rtmp://live.example.com/app.
        #[arg(long)]
        url: String,
        /// Environment variable holding the stream key (must be set if given; without this
        /// flag, STREAMFORGE_STREAM_KEY is used if set).
        #[arg(long)]
        stream_key_env: Option<String>,
        /// Streaming length in milliseconds.
        #[arg(long, default_value_t = 10_000)]
        duration_ms: u64,
        /// Video bit rate in kbit/s (constant).
        #[arg(long)]
        video_bitrate_kbps: Option<u32>,
        /// H.264 encoder for the stream (and the recording): auto, software or hardware.
        #[arg(long)]
        video_encoder: Option<String>,
        /// Reconnect attempts after the connection is lost.
        #[arg(long)]
        reconnect_attempts: Option<u32>,
        /// Also record to this plain .mkv file name in --output-dir.
        #[arg(long)]
        record: Option<String>,
        #[command(flatten)]
        sources: SourceArgs,
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
        /// Add the reference scene's live screen layer (the --capture-target-id target, else the
        /// system picker).
        #[arg(long)]
        screen: bool,
        /// With --screen: capture this target from `capture targets` (no picker).
        #[arg(long, requires = "screen")]
        capture_target_id: Option<String>,
        /// With --screen: offered target kind (monitor, window, virtual). Repeatable.
        #[arg(long = "target-kind", requires = "screen")]
        target_kinds: Vec<String>,
        /// With --screen: cursor mode (hidden, embedded, metadata).
        #[arg(long, requires = "screen")]
        cursor_mode: Option<String>,
        /// Write the last composited frame to this new .ppm file. A plain file
        /// name (no directories), created in --output-dir; never overwritten.
        #[arg(long)]
        snapshot_file_name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// Check a project file (schema version, every scene and source, the
    /// audio setup) without loading it. Prints the problems with their JSON
    /// paths as JSON; exits 1 if the file is not valid.
    Validate {
        /// Path to a .json project file.
        file: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum CaptureCommand {
    /// Report capture capabilities of the running system.
    Caps,
    /// List audio input devices.
    Inputs,
    /// List monitors and windows that can be captured by id (where the platform allows it).
    Targets,
    /// Capture a screen or window and report diagnostics: the --capture-target-id target, or the one
    /// chosen in the system dialog.
    Screen {
        /// Probe duration in milliseconds (100..=60000, default 5000).
        #[arg(long)]
        duration_ms: Option<u64>,
        /// Capture target id from `capture targets`.
        #[arg(long)]
        capture_target_id: Option<String>,
        /// Offered target kind: monitor, window or virtual. Repeatable; default monitor and window.
        #[arg(long = "target-kind")]
        target_kinds: Vec<String>,
        /// Cursor mode: hidden, embedded (default) or metadata.
        #[arg(long)]
        cursor_mode: Option<String>,
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
                println!("Capture available: {}", status.capture_available);
                println!(
                    "Recording: {}",
                    status.recording_state.map_or("off", |state| state.as_str())
                );
                println!(
                    "Streaming: {}",
                    status.streaming_state.map_or("off", |state| state.as_str())
                );
                println!();
                println!(
                    "Capture probes: `streamforge capture --help`. Recording and streaming: `streamforge record --help`, `streamforge stream --help`."
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
            file_name,
            duration_ms,
            video_encoder,
            sources,
        } => {
            if let Some(failed) = set_up_from(&mut engine, &sources) {
                return Ok(failed);
            }
            let mut params = Map::new();
            params.insert("file_name".into(), file_name.into());
            if let Some(encoder) = video_encoder {
                params.insert("video_encoder".into(), encoder.into());
            }
            record(&mut engine, params, Duration::from_millis(duration_ms))
        }
        Commands::Stream {
            url,
            stream_key_env,
            duration_ms,
            video_bitrate_kbps,
            video_encoder,
            reconnect_attempts,
            record,
            sources,
        } => {
            let explicit = stream_key_env.is_some();
            let stream_key_env = stream_key_env.unwrap_or_else(|| DEFAULT_KEY_ENV.to_owned());
            let key = match std::env::var(&stream_key_env) {
                Ok(key) => key,
                // A named but missing variable is a mistake, not "no key".
                Err(std::env::VarError::NotPresent) if explicit => {
                    eprintln!("error: --stream-key-env names ${stream_key_env}, which is not set");
                    return Ok(ExitCode::FAILURE);
                }
                Err(std::env::VarError::NotPresent) => String::new(),
                Err(std::env::VarError::NotUnicode(_)) => {
                    eprintln!("error: ${stream_key_env} is not valid UTF-8");
                    return Ok(ExitCode::FAILURE);
                }
            };
            let mut stream_params = Map::new();
            stream_params.insert("url".into(), url.into());
            if !key.is_empty() {
                stream_params.insert("stream_key".into(), key.into());
            }
            if let Some(kbps) = video_bitrate_kbps {
                stream_params.insert("video_bitrate_kbps".into(), kbps.into());
            }
            if let Some(attempts) = reconnect_attempts {
                stream_params.insert("reconnect_attempts".into(), attempts.into());
            }
            if let Some(encoder) = &video_encoder {
                stream_params.insert("video_encoder".into(), encoder.clone().into());
            }
            if let Some(failed) = set_up_from(&mut engine, &sources) {
                return Ok(failed);
            }
            let record_params = record.map(|file_name| {
                let mut params = Map::new();
                params.insert("file_name".into(), file_name.into());
                if let Some(encoder) = &video_encoder {
                    params.insert("video_encoder".into(), encoder.clone().into());
                }
                params
            });
            stream(
                &mut engine,
                record_params,
                stream_params,
                Duration::from_millis(duration_ms),
            )
        }
        Commands::Compose {
            duration_ms,
            screen,
            capture_target_id,
            target_kinds,
            cursor_mode,
            snapshot_file_name,
        } => {
            let mut params = Map::new();
            if let Some(duration_ms) = duration_ms {
                params.insert("duration_ms".into(), duration_ms.into());
            }
            if screen {
                params.insert("screen".into(), true.into());
            }
            if let Some(capture_target_id) = capture_target_id {
                params.insert("capture_target_id".into(), capture_target_id.into());
            }
            if !target_kinds.is_empty() {
                params.insert("target_kinds".into(), target_kinds.into());
            }
            if let Some(cursor_mode) = cursor_mode {
                params.insert("cursor_mode".into(), cursor_mode.into());
            }
            if let Some(snapshot_file_name) = snapshot_file_name {
                params.insert("snapshot_file_name".into(), snapshot_file_name.into());
            }
            run_command(&mut engine, "probe_composite", params)
        }
        Commands::Project(ProjectCommand::Validate { file }) => validate_project(&file),
    }
}

/// The engine's `validate_project`, with the file's directory as the
/// project directory: the same checks as for `load_project`.
fn validate_project(file: &Path) -> Result<ExitCode> {
    let (Some(name), dir) = (file.file_name(), file.parent()) else {
        eprintln!("error: {} is not a file", file.display());
        return Ok(ExitCode::FAILURE);
    };
    let dir = match dir {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let mut engine = Engine::new().with_project_dir(dir);
    let mut params = Map::new();
    params.insert(
        "file_name".into(),
        name.to_string_lossy().into_owned().into(),
    );
    let result = Command::decode("validate_project", Some(&Value::Object(params)))
        .and_then(|command| engine.handle(command));
    match result {
        Ok(CommandOutput::ProjectValidation(validation)) => {
            println!("{}", serde_json::to_string_pretty(&validation)?);
            Ok(if validation.valid {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Ok(other) => bail!("engine returned unexpected output for validate_project: {other:?}"),
        Err(err) => Ok(report_error(&err)),
    }
}

/// A finite number from `text`. Non-finite values would serialize as JSON
/// null and silently become defaults, so they are rejected here.
fn finite(text: &str, what: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("invalid {what} `{text}`: expected a finite number"))
}

/// A live input argument, `NAME[=GAIN_DB]`. Only a numeric suffix is a
/// gain; node names may contain '='.
fn input_spec(spec: &str) -> Result<(&str, Option<f64>), String> {
    match spec.rsplit_once('=') {
        Some((name, gain)) if !name.is_empty() && gain.parse::<f64>().is_ok() => {
            Ok((name, Some(finite(gain, "gain")?)))
        }
        _ => Ok((spec, None)),
    }
}

/// A test tone argument, `FREQ_HZ[@LEVEL_DBFS]`, as protocol fields.
fn tone_spec(spec: &str) -> Result<Map<String, Value>, String> {
    let (frequency, level) = match spec.split_once('@') {
        Some((frequency, level)) => (frequency, Some(finite(level, "level")?)),
        None => (spec, None),
    };
    let mut tone = Map::new();
    tone.insert(
        "frequency_hz".into(),
        finite(frequency, "frequency")?.into(),
    );
    if let Some(level) = level {
        tone.insert("level_dbfs".into(), level.into());
    }
    Ok(tone)
}

/// A finite master gain as a protocol value.
fn master_gain(gain: Option<f32>) -> Result<Option<Value>, String> {
    match gain {
        Some(gain) if !gain.is_finite() => Err(format!(
            "invalid master gain `{gain}`: expected a finite number"
        )),
        gain => Ok(gain.map(Value::from)),
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
    let mut params = Map::new();
    let mut input_list = Vec::new();
    for spec in inputs {
        let (name, gain) = input_spec(spec)?;
        let mut input = Map::new();
        input.insert("input".into(), name.into());
        if let Some(gain) = gain {
            input.insert("gain_db".into(), gain.into());
        }
        input_list.push(Value::Object(input));
    }
    let tone_list = tones
        .iter()
        .map(|spec| tone_spec(spec).map(Value::Object))
        .collect::<Result<Vec<_>, _>>()?;
    if !input_list.is_empty() {
        params.insert("inputs".into(), input_list.into());
    }
    if !tone_list.is_empty() {
        params.insert("tones".into(), tone_list.into());
    }
    if let Some(gain) = master_gain(master_gain_db)? {
        params.insert("master_gain_db".into(), gain);
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
        CaptureCommand::Targets => "list_capture_targets",
        CaptureCommand::Screen {
            duration_ms,
            capture_target_id,
            target_kinds,
            cursor_mode,
        } => {
            if let Some(duration_ms) = duration_ms {
                params.insert("duration_ms".into(), duration_ms.into());
            }
            if let Some(capture_target_id) = capture_target_id {
                params.insert("capture_target_id".into(), capture_target_id.into());
            }
            if !target_kinds.is_empty() {
                params.insert("target_kinds".into(), target_kinds.into());
            }
            if let Some(cursor_mode) = cursor_mode {
                params.insert("cursor_mode".into(), cursor_mode.into());
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

/// Read for the stream key when `--stream-key-env` is not given.
const DEFAULT_KEY_ENV: &str = "STREAMFORGE_STREAM_KEY";

/// Source flags shared by `record` and `stream`: a screen source in the
/// active scene, and the engine's mixer channels for this process.
#[derive(clap::Args, Debug)]
struct SourceArgs {
    /// Add a live screen source to the scene (the --capture-target-id target, else the system
    /// picker), fitted into 1152x648 at the lower right, at 90 % opacity.
    #[arg(long)]
    screen: bool,
    /// With --screen: capture this target from `capture targets` (no picker).
    #[arg(long, requires = "screen")]
    capture_target_id: Option<String>,
    /// With --screen: offered target kind (monitor, window, virtual). Repeatable.
    #[arg(long = "target-kind", requires = "screen")]
    target_kinds: Vec<String>,
    /// With --screen: cursor mode (hidden, embedded, metadata).
    #[arg(long, requires = "screen")]
    cursor_mode: Option<String>,
    /// Live input as NAME or NAME=GAIN_DB, as a mixer channel. Repeatable. A device that is
    /// not found leaves its channel silent (with a warning).
    #[arg(long = "input")]
    inputs: Vec<String>,
    /// Test tone as FREQ_HZ or FREQ_HZ@LEVEL_DBFS (default -12), as a mixer channel.
    /// Repeatable. Without --input, --tone or --no-tone, the default channel: a 1 kHz tone at
    /// -20 dBFS.
    #[arg(long = "tone", conflicts_with = "no_tone")]
    tones: Vec<String>,
    /// No default test tone.
    #[arg(long)]
    no_tone: bool,
    /// Program bus fader in dB (default 0).
    #[arg(long, allow_negative_numbers = true)]
    master_gain_db: Option<f32>,
}

/// How `record` and `stream` set up the engine's mixer before they start,
/// through the same commands as every other client.
#[derive(Debug, Default, PartialEq)]
struct MixerSetup {
    /// `add_channel` params for channels that replace the configured ones.
    channels: Option<Vec<Map<String, Value>>>,
    /// The program bus fader.
    master_gain_db: Option<Value>,
}

impl SourceArgs {
    /// Inputs, then tones, become channels named `Input 1`, `Tone 1`, ...
    /// (as a version 1 project's sources do).
    fn mixer_setup(&self) -> Result<MixerSetup, String> {
        let master_gain_db = master_gain(self.master_gain_db)?;
        if self.inputs.is_empty() && self.tones.is_empty() && !self.no_tone {
            return Ok(MixerSetup {
                channels: None,
                master_gain_db,
            });
        }
        // Checked before the mixer changes, so nothing is left half set up.
        let count = self.inputs.len() + self.tones.len();
        if count > command_api::MAX_CHANNELS {
            return Err(format!(
                "at most {} channels from --input and --tone, got {count}",
                command_api::MAX_CHANNELS
            ));
        }
        let mut channels = Vec::with_capacity(count);
        for (spec, number) in self.inputs.iter().zip(1..) {
            let (device, gain) = input_spec(spec)?;
            let mut channel = Map::new();
            channel.insert("name".into(), format!("Input {number}").into());
            channel.insert(
                "input".into(),
                serde_json::json!({"kind": "device", "device": device}),
            );
            if let Some(gain) = gain {
                channel.insert("gain_db".into(), gain.into());
            }
            channels.push(channel);
        }
        for (spec, number) in self.tones.iter().zip(1..) {
            let mut input = Map::new();
            input.insert("kind".into(), "tone".into());
            input.extend(tone_spec(spec)?);
            let mut channel = Map::new();
            channel.insert("name".into(), format!("Tone {number}").into());
            channel.insert("input".into(), Value::Object(input));
            channels.push(channel);
        }
        Ok(MixerSetup {
            channels: Some(channels),
            master_gain_db,
        })
    }

    /// `add_source` params for the screen source in scene `scene_id`, if
    /// `--screen` asks for one: where the reference scene draws its screen
    /// (`compose --screen`), on top of the scene.
    fn screen_source(&self, scene_id: &str) -> Option<Map<String, Value>> {
        if !self.screen {
            return None;
        }
        let mut params = Map::new();
        params.insert("scene_id".into(), scene_id.into());
        params.insert("name".into(), "Screen".into());
        params.insert("kind".into(), "screen".into());
        for (field, value) in [
            ("x", 704.0),
            ("y", 368.0),
            ("width", 1152.0),
            ("height", 648.0),
            ("opacity", 0.9),
        ] {
            params.insert(field.into(), value.into());
        }
        if let Some(capture_target_id) = &self.capture_target_id {
            params.insert("capture_target_id".into(), capture_target_id.clone().into());
        }
        if !self.target_kinds.is_empty() {
            params.insert("target_kinds".into(), self.target_kinds.clone().into());
        }
        if let Some(cursor_mode) = &self.cursor_mode {
            params.insert("cursor_mode".into(), cursor_mode.clone().into());
        }
        Some(params)
    }
}

/// Set up the mixer and the screen source from `sources`' flags; the exit
/// code if that fails.
fn set_up_from(engine: &mut Engine, sources: &SourceArgs) -> Option<ExitCode> {
    let setup = match sources.mixer_setup() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {message}");
            return Some(ExitCode::FAILURE);
        }
    };
    set_up_mixer(engine, setup)
        .and_then(|()| add_screen_source(engine, sources))
        .err()
        .map(|err| report_error(&err))
}

/// Add `--screen`'s source to the active scene; its capture starts with the
/// pipeline.
fn add_screen_source(
    engine: &mut Engine,
    sources: &SourceArgs,
) -> Result<(), command_api::CommandError> {
    if !sources.screen {
        return Ok(());
    }
    let scene_id = match start(engine, "list_scenes", Map::new())? {
        CommandOutput::SceneList(scenes) => scenes.active_scene_id,
        other => unreachable!("list_scenes returns the scene list, not {other:?}"),
    };
    if let Some(params) = sources.screen_source(&scene_id) {
        start(engine, "add_source", params)?;
    }
    Ok(())
}

/// Apply `setup` to the engine's mixer (before anything runs).
fn set_up_mixer(engine: &mut Engine, setup: MixerSetup) -> Result<(), command_api::CommandError> {
    if let Some(channels) = setup.channels {
        let configured = match start(engine, "get_mixer", Map::new())? {
            CommandOutput::MixerState(mixer) => mixer.channels,
            other => unreachable!("get_mixer returns the mixer state, not {other:?}"),
        };
        for channel in configured {
            let mut params = Map::new();
            params.insert("channel_id".into(), channel.spec.id.into());
            start(engine, "remove_channel", params)?;
        }
        for params in channels {
            start(engine, "add_channel", params)?;
        }
    }
    if let Some(gain) = setup.master_gain_db {
        let mut params = Map::new();
        params.insert("bus_id".into(), command_api::PROGRAM_BUS_ID.into());
        params.insert("gain_db".into(), gain);
        start(engine, "update_bus", params)?;
    }
    Ok(())
}

/// Stream (and optionally record) for `duration`, then stop both and print
/// `{"streaming": ..., "recording": ...}`. The stream is stopped first so a
/// recording covers all of it.
fn stream(
    engine: &mut Engine,
    record_params: Option<Map<String, Value>>,
    stream_params: Map<String, Value>,
    duration: Duration,
) -> Result<ExitCode> {
    let recording = record_params.is_some();
    if let Some(params) = record_params {
        match start(engine, "start_recording", params) {
            Ok(CommandOutput::RecordingStarted(started)) => {
                eprintln!("recording {} ...", started.path);
                print_start_warnings(&started.warnings);
            }
            Ok(other) => bail!("engine returned unexpected output for start_recording: {other:?}"),
            Err(err) => return Ok(report_error(&err)),
        }
    }
    match start(engine, "start_streaming", stream_params) {
        Ok(CommandOutput::StreamingStarted(started)) => {
            eprintln!(
                "streaming to {} for {:.1} s ...",
                started.target,
                duration.as_secs_f64()
            );
            print_start_warnings(&started.warnings);
        }
        Ok(other) => bail!("engine returned unexpected output for start_streaming: {other:?}"),
        Err(err) => {
            let code = report_error(&err);
            if recording {
                // Finalize the recording that was already started.
                let _ = engine.handle(Command::StopRecording);
            }
            return Ok(code);
        }
    }
    run_for(engine, duration);
    let mut output = Map::new();
    let mut failed = false;
    match engine.handle(Command::StopStreaming) {
        Ok(report) => {
            output.insert("streaming".into(), serde_json::to_value(&report)?);
        }
        Err(err) => {
            report_error(&err);
            failed = true;
        }
    }
    if recording {
        match engine.handle(Command::StopRecording) {
            Ok(report) => {
                output.insert("recording".into(), serde_json::to_value(&report)?);
            }
            Err(err) => {
                report_error(&err);
                failed = true;
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&Value::Object(output))?);
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn start(
    engine: &mut Engine,
    name: &str,
    params: Map<String, Value>,
) -> Result<CommandOutput, command_api::CommandError> {
    Command::decode(name, Some(&Value::Object(params))).and_then(|command| engine.handle(command))
}

/// What the start says to watch, e.g. a silent channel, on stderr.
fn print_start_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
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
            print_start_warnings(&started.warnings);
        }
        Ok(other) => bail!("engine returned unexpected output for start_recording: {other:?}"),
        Err(err) => return Ok(report_error(&err)),
    }
    run_for(engine, duration);
    match engine.handle(Command::StopRecording) {
        Ok(output) => {
            println!("{}", serde_json::to_string_pretty(&output)?);
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => Ok(report_error(&err)),
    }
}

/// Wait `duration`, printing one progress line per second (from
/// `get_health`) to stderr. Ends early once every open output stopped
/// running (the stop reports say why); a recording outlives a failed stream.
fn run_for(engine: &mut Engine, duration: Duration) {
    let started = std::time::Instant::now();
    loop {
        let left = duration.saturating_sub(started.elapsed());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_secs(1)));
        if let Ok(CommandOutput::Health(health)) = engine.handle(Command::GetHealth) {
            eprintln!("{}", progress_line(&health, started.elapsed()));
            let outputs = [
                health.recording.as_ref().map(|output| output.running),
                health.streaming.as_ref().map(|output| output.running),
            ];
            let mut outputs = outputs.into_iter().flatten().peekable();
            if outputs.peek().is_some() && outputs.all(|running| !running) {
                eprintln!("every output stopped; ending early");
                return;
            }
        }
    }
}

/// E.g. `  2.0 s  recording 120 frames, 0 dropped, queue 1 | render 0 late, 0 skipped`.
fn progress_line(health: &command_api::HealthReport, elapsed: Duration) -> String {
    let mut parts = Vec::new();
    let output = |name: &str, state: &str, counters: &command_api::LiveOutputStats| {
        format!(
            "{name}{state} {} frames, {} dropped, queue {}",
            counters.frames_encoded, counters.frames_dropped, counters.queue_depth
        )
    };
    if let Some(recording) = &health.recording {
        // A running recording needs no word.
        let state = match recording.state {
            command_api::RecordingState::Running => String::new(),
            state => format!(" {}", state.as_str()),
        };
        parts.push(output("recording", &state, &recording.counters));
    }
    if let Some(streaming) = &health.streaming {
        let state = format!(" {}", streaming.state.as_str());
        parts.push(output("stream", &state, &streaming.counters));
    }
    if let Some(render) = &health.render {
        parts.push(format!(
            "render {} late, {} skipped",
            render.late_frames, render.skipped_ticks
        ));
    }
    for capture in &health.captures {
        if let command_api::CaptureState::Ended { reason } = &capture.state {
            parts.push(format!(
                "{} `{}` ended: {reason}",
                capture.kind.as_str(),
                capture.name
            ));
        }
    }
    format!("{:>5.1} s  {}", elapsed.as_secs_f64(), parts.join(" | "))
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
            capture_target_id: None,
            target_kinds: vec!["window".into()],
            cursor_mode: Some("hidden".into()),
        });
        assert_eq!(name, "probe_screen_capture");
        assert_eq!(
            Value::Object(params),
            serde_json::json!({"duration_ms": 1500, "target_kinds": ["window"], "cursor_mode": "hidden"})
        );
        let (_, params) = capture_request(CaptureCommand::Screen {
            duration_ms: None,
            capture_target_id: Some("monitor:1".into()),
            target_kinds: Vec::new(),
            cursor_mode: None,
        });
        let params = Value::Object(params);
        assert_eq!(
            params,
            serde_json::json!({"capture_target_id": "monitor:1"})
        );
        assert!(Command::decode("probe_screen_capture", Some(&params)).is_ok());
        assert_eq!(
            capture_request(CaptureCommand::Targets).0,
            "list_capture_targets"
        );
    }

    #[test]
    fn progress_lines_summarize_health() {
        use command_api::{HealthReport, OutputHealth, VideoEncoderInfo};
        fn output<S>(state: S, running: bool) -> OutputHealth<S> {
            OutputHealth {
                target: "x".into(),
                elapsed_ms: 2000,
                running,
                state,
                video_encoder: VideoEncoderInfo {
                    encoder: "libx264".into(),
                    hardware: false,
                    skipped: Vec::new(),
                },
                counters: command_api::LiveOutputStats {
                    frames_encoded: 120,
                    frames_dropped: 2,
                    queue_depth: 1,
                    ..Default::default()
                },
                connection: None,
            }
        }
        let health = HealthReport {
            pipeline_running: true,
            monitor: None,
            render: None,
            mix: None,
            recording: Some(output(command_api::RecordingState::Failed, false)),
            streaming: Some(output(command_api::StreamState::Reconnecting, true)),
            captures: vec![command_api::CaptureHealth {
                kind: command_api::CaptureKind::Screen,
                id: "src_desk".to_owned(),
                name: "desk".into(),
                state: command_api::CaptureState::Ended {
                    reason: "the captured window was closed".into(),
                },
            }],
        };
        assert_eq!(
            progress_line(&health, Duration::from_secs(2)),
            "  2.0 s  recording failed 120 frames, 2 dropped, queue 1 | \
             stream reconnecting 120 frames, 2 dropped, queue 1 | \
             screen `desk` ended: the captured window was closed"
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

    fn source_args(inputs: &[&str], tones: &[&str], no_tone: bool) -> SourceArgs {
        SourceArgs {
            screen: false,
            capture_target_id: None,
            target_kinds: Vec::new(),
            cursor_mode: None,
            inputs: inputs.iter().map(|&input| input.to_owned()).collect(),
            tones: tones.iter().map(|&tone| tone.to_owned()).collect(),
            no_tone,
            master_gain_db: None,
        }
    }

    #[test]
    fn the_screen_flag_becomes_a_scene_source() {
        assert_eq!(source_args(&[], &[], false).screen_source("scene_a"), None);
        let mut args = source_args(&[], &[], false);
        args.screen = true;
        args.capture_target_id = Some("monitor:1".to_owned());
        args.target_kinds = vec!["window".to_owned()];
        let params = args.screen_source("scene_a").expect("params");
        assert_eq!(
            Value::Object(params.clone()),
            serde_json::json!({
                "scene_id": "scene_a", "name": "Screen", "kind": "screen",
                "x": 704.0, "y": 368.0, "width": 1152.0, "height": 648.0, "opacity": 0.9,
                "capture_target_id": "monitor:1", "target_kinds": ["window"],
            })
        );
        assert!(Command::decode("add_source", Some(&Value::Object(params))).is_ok());
    }

    #[test]
    fn source_flags_become_mixer_channels() {
        // No audio flag: the configured channels stay.
        assert_eq!(
            source_args(&[], &[], false).mixer_setup(),
            Ok(MixerSetup::default())
        );
        assert_eq!(
            source_args(&[], &[], true).mixer_setup(),
            Ok(MixerSetup {
                channels: Some(Vec::new()),
                master_gain_db: None,
            })
        );
        let mut args = source_args(&["mic=-6"], &["440@-18", "1000"], false);
        args.master_gain_db = Some(-1.0);
        let setup = args.mixer_setup().expect("setup");
        let channels: Vec<Value> = setup
            .channels
            .expect("channels")
            .into_iter()
            .map(Value::Object)
            .collect();
        assert_eq!(
            channels,
            [
                serde_json::json!({"name": "Input 1", "input": {"kind": "device", "device": "mic"},
                                   "gain_db": -6.0}),
                serde_json::json!({"name": "Tone 1",
                                   "input": {"kind": "tone", "frequency_hz": 440.0,
                                             "level_dbfs": -18.0}}),
                serde_json::json!({"name": "Tone 2",
                                   "input": {"kind": "tone", "frequency_hz": 1000.0}}),
            ]
        );
        assert_eq!(setup.master_gain_db, Some(serde_json::json!(-1.0)));
        for channel in channels {
            assert!(
                Command::decode("add_channel", Some(&channel)).is_ok(),
                "{channel}"
            );
        }
        // Too many channels fail before the mixer changes.
        let nine: Vec<String> = (0..9).map(|index| format!("in{index}")).collect();
        let nine: Vec<&str> = nine.iter().map(String::as_str).collect();
        let error = source_args(&nine, &[], false).mixer_setup().unwrap_err();
        assert!(error.contains("at most 8 channels"), "{error}");
        assert!(source_args(&[], &["loud"], false).mixer_setup().is_err());
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
    #[test]
    fn renamed_cli_flags_are_accepted_without_aliases() {
        for args in [
            vec![
                "streamforge",
                "record",
                "--file-name",
                "take.mkv",
                "--screen",
                "--capture-target-id",
                "monitor:1",
                "--target-kind",
                "monitor",
                "--target-kind",
                "window",
                "--cursor-mode",
                "hidden",
            ],
            vec![
                "streamforge",
                "stream",
                "--url",
                "rtmp://localhost/live",
                "--stream-key-env",
                "TEST_KEY",
                "--video-bitrate-kbps",
                "4500",
                "--record",
                "take.mkv",
            ],
            vec![
                "streamforge",
                "compose",
                "--snapshot-file-name",
                "frame.ppm",
            ],
            vec!["streamforge", "capture", "targets"],
            vec![
                "streamforge",
                "capture",
                "screen",
                "--capture-target-id",
                "monitor:1",
                "--target-kind",
                "window",
                "--cursor-mode",
                "hidden",
            ],
        ] {
            assert!(Cli::try_parse_from(&args).is_ok(), "{args:?}");
        }
        for args in [
            vec!["streamforge", "record", "--file", "take.mkv"],
            vec![
                "streamforge",
                "stream",
                "--url",
                "rtmp://localhost/live",
                "--key-env",
                "TEST_KEY",
            ],
            vec![
                "streamforge",
                "stream",
                "--url",
                "rtmp://localhost/live",
                "--bitrate-kbps",
                "4500",
            ],
            vec!["streamforge", "compose", "--snapshot", "frame.ppm"],
            vec!["streamforge", "capture", "sources"],
            vec![
                "streamforge",
                "capture",
                "screen",
                "--source-id",
                "monitor:1",
            ],
            vec!["streamforge", "capture", "screen", "--source", "window"],
            vec!["streamforge", "capture", "screen", "--cursor", "hidden"],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
    }
}
