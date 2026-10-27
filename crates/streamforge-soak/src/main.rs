//! Soak test harness (M13, `docs/testing.md`): records and streams for a
//! long time through the real `streamforge-engine` process, polling it as
//! the desktop does, and judges the run: no lost media, no growing memory,
//! threads or file descriptors, a recording that decodes from start to end
//! and a stream that arrived whole: at a local RTMP ingest, or read back
//! from a server such as MediaMTX on the local network (`--url`).
//!
//! The engine runs as a child process, so its own resources are measured
//! (from `/proc` on Linux) and the shipped binary and protocol are what is
//! tested. The ingest or reader runs in this process. A JSON-lines timeline records a
//! sample every 10 s and every loss or state change as it happens.

mod client;
mod media;
mod process;
mod receiver;
mod reports;
mod trend;
mod verdict;
mod watch;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;
use ffmpeg_next as ff;
use serde::Serialize;
use serde_json::{Value, json};

use crate::client::Engine;
use crate::process::ProcessReading;
use crate::receiver::Receiver;
use crate::reports::{Health, RecordingStop, StreamingStop};
use crate::verdict::{Check, Outcome, Status};
use crate::watch::{QueueDepths, Watch};

/// What the desktop polls, and how often (`streamforge-desktop`'s app):
/// status and meters, health, and the scene list.
const STATUS_EVERY: Duration = Duration::from_millis(100);
const HEALTH_EVERY: Duration = Duration::from_millis(250);
/// How often the engine refreshes the pipeline's health counters, plus a
/// margin.
const HEALTH_REFRESH: Duration = Duration::from_millis(300);
const SCENES_EVERY: Duration = Duration::from_secs(2);
/// A timeline sample with the engine's resources.
const SAMPLE_EVERY: Duration = Duration::from_secs(10);
/// A progress line on stderr.
const PROGRESS_EVERY: Duration = Duration::from_secs(60);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Starting a pipeline or an output opens encoders and devices.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// Stops drain the encoders and finalize the file.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);
/// The portal's picker waits for a human (it gives up after 120 s).
const PICKER_TIMEOUT: Duration = Duration::from_secs(150);
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the local ingest waits for the stream to connect.
const INGEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Parser)]
#[command(
    name = "streamforge-soak",
    about = "Record and stream through streamforge-engine for a long time, polling it as the \
             desktop does, then judge lost media, resource growth, the file and the stream."
)]
struct Args {
    /// How long to record and stream (default: one hour).
    #[arg(long, default_value_t = 3600)]
    seconds: u64,
    /// The engine binary [default: streamforge-engine next to this one].
    #[arg(long)]
    engine: Option<PathBuf>,
    /// Where the recording, the timeline and the engine's log go. Not a
    /// tmpfs such as /tmp on many systems: an hour of 1080p60 is a few GB.
    #[arg(long, default_value = ".")]
    output_dir: PathBuf,
    /// H.264 encoder of both outputs: auto, software or hardware.
    #[arg(long, default_value = "auto")]
    video_encoder: String,
    /// Add a screen source to the scene. The system picker asks which
    /// screen, so someone must be there at the start.
    #[arg(long)]
    screen: bool,
    /// Add a channel that captures this audio input (a name from
    /// `list_audio_inputs`, e.g. a sink's `.monitor` for desktop audio);
    /// repeatable.
    #[arg(long = "input")]
    inputs: Vec<String>,
    /// Stream to a server instead of a local ingest, and read the stream
    /// back from it (e.g. MediaMTX): the URL a player reads,
    /// rtmp://host/app/name. Its last segment is sent as the stream key.
    #[arg(long)]
    url: Option<String>,
    /// Keep the recording after a passing run (a failing run keeps it).
    #[arg(long)]
    keep: bool,
    /// Print the result as JSON instead of a table.
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("streamforge-soak: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// The timeline: one JSON object per line, flushed as written so that it
/// survives a crash.
struct Timeline(BufWriter<File>);

impl Timeline {
    fn write(&mut self, kind: &str, mut line: Value) -> Result<()> {
        line["type"] = json!(kind);
        writeln!(self.0, "{line}")?;
        self.0.flush()?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct Info {
    os: &'static str,
    cpus: usize,
    engine_version: String,
    encoders: Vec<String>,
    max_queue_depth: QueueDepths,
    engine_cpu_pct: Option<f64>,
    max_load_1m: Option<f64>,
    events: usize,
    warnings: Vec<String>,
}

fn run(args: &Args) -> Result<bool> {
    if args.seconds == 0 {
        bail!("--seconds must be at least 1");
    }
    let exe = match &args.engine {
        Some(exe) => exe.clone(),
        None => std::env::current_exe()
            .context("cannot find this executable")?
            .with_file_name(format!(
                "streamforge-engine{}",
                std::env::consts::EXE_SUFFIX
            )),
    };
    if !exe.is_file() {
        bail!(
            "no engine at {} (build it: cargo build --release -p streamforge-engine, or pass \
             --engine)",
            exe.display()
        );
    }
    if !args.output_dir.is_dir() {
        bail!(
            "--output-dir {} is not a directory",
            args.output_dir.display()
        );
    }
    let read_back = args
        .url
        .as_deref()
        .map(receiver::split_stream_url)
        .transpose()?;
    ff::init().context("cannot initialize FFmpeg")?;
    // FFmpeg's own warnings (e.g. about the stream it reads back) would
    // flood the progress lines; errors still show.
    ff::log::set_level(ff::log::Level::Error);
    let unix_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs());
    let stem = format!("soak-{unix_s}");
    let file_name = format!("{stem}.mkv");
    let recording_path = args.output_dir.join(&file_name);
    let timeline_path = args.output_dir.join(format!("{stem}.jsonl"));
    let log_path = args.output_dir.join(format!("{stem}.engine.log"));
    let mut timeline = Timeline(BufWriter::new(
        File::create(&timeline_path)
            .with_context(|| format!("cannot create {}", timeline_path.display()))?,
    ));
    let log =
        File::create(&log_path).with_context(|| format!("cannot create {}", log_path.display()))?;

    let mut engine = Engine::spawn(&exe, &args.output_dir, log)?;
    let status = engine.request("get_status", Value::Null, REQUEST_TIMEOUT)?;
    let capabilities = engine.request("get_capture_capabilities", Value::Null, REQUEST_TIMEOUT)?;
    for (index, input) in args.inputs.iter().enumerate() {
        engine.request(
            "add_channel",
            json!({"name": format!("Input {}", index + 1), "input": {"kind": "device", "device": input}}),
            REQUEST_TIMEOUT,
        )?;
    }
    // The desktop always shows the preview.
    engine.request("set_preview", json!({"enabled": true}), START_TIMEOUT)?;
    if args.screen {
        let scenes = engine.request("list_scenes", Value::Null, REQUEST_TIMEOUT)?;
        eprintln!("Choose the screen to capture in the system picker now (it waits up to 2 min).");
        // Picker sources can only be added while no output runs.
        engine.request(
            "add_source",
            json!({"scene_name": scenes["active_scene_name"], "name": "Screen", "kind": "screen"}),
            PICKER_TIMEOUT,
        )?;
    }
    let mixer = engine.request("get_mixer", Value::Null, REQUEST_TIMEOUT)?;
    // The baseline of the pipeline's counters, so that losses while the
    // outputs start count too.
    let mut watch = Watch::default();
    watch.observe(0.0, health(&mut engine)?);

    let (local, mut stream) = match &read_back {
        Some((server, key)) => (None, json!({"url": server, "stream_key": key})),
        None => {
            let (ingest, url) = Receiver::local(INGEST_TIMEOUT);
            (Some(ingest), json!({"url": url}))
        }
    };
    stream["video_encoder"] = json!(args.video_encoder);
    let recording_start = engine.request(
        "start_recording",
        json!({"file_name": file_name, "video_encoder": args.video_encoder}),
        START_TIMEOUT,
    )?;
    let streaming_start = match engine.request("start_streaming", stream, START_TIMEOUT) {
        Ok(result) => result,
        Err(error) => {
            let _ = engine.request("stop_recording", Value::Null, STOP_TIMEOUT);
            return Err(error);
        }
    };
    let started = Instant::now();
    let receiver = match (local, &args.url) {
        (Some(ingest), _) => ingest,
        (None, Some(url)) => {
            let fps = streaming_start["fps"]["num"].as_f64().unwrap_or(60.0)
                / streaming_start["fps"]["den"].as_f64().unwrap_or(1.0);
            Receiver::read_back(url.clone(), fps, started)?
        }
        (None, None) => unreachable!("a local ingest when no --url is given"),
    };
    timeline.write(
        "start",
        json!({
            "unix_s": unix_s,
            "seconds": args.seconds,
            "engine": exe,
            "status": status,
            "capabilities": capabilities,
            "channels": mixer["channels"],
            "screen": args.screen,
            "read_back": read_back.is_some(),
            "start_recording": recording_start,
            "start_streaming": streaming_start,
        }),
    )?;
    eprintln!(
        "Recording and streaming for {} s; timeline in {}.",
        args.seconds,
        timeline_path.display()
    );

    let mut soak = Soak {
        engine: &mut engine,
        timeline: &mut timeline,
        started,
        watch,
        process: Vec::new(),
        max_load: None,
    };
    let aborted = soak.poll(Duration::from_secs(args.seconds)).err();
    let Soak {
        mut watch,
        process,
        max_load,
        ..
    } = soak;
    let seconds_run = started.elapsed().as_secs_f64();
    if let Some(error) = &aborted {
        eprintln!("Stopping early: {error}");
    }

    let streaming = stop::<StreamingStop>(&mut engine, "stop_streaming");
    let recording = stop::<RecordingStop>(&mut engine, "stop_recording");
    // The pipeline still runs for the preview: a reading after its next
    // refresh covers the stops too.
    thread::sleep(HEALTH_REFRESH);
    match health(&mut engine) {
        Ok(end) if end.render.is_some() && end.mix.is_some() => {
            let t_s = started.elapsed().as_secs_f64();
            for event in watch.observe_end(t_s, end).to_vec() {
                eprintln!("{event}");
                timeline.write("event", serde_json::to_value(&event)?)?;
            }
        }
        Ok(_) => eprintln!("No pipeline counters after the stops; the last reading counts."),
        Err(error) => eprintln!("No reading after the stops ({error:#}); the last one counts."),
    }
    let preview = engine.request("set_preview", json!({"enabled": false}), STOP_TIMEOUT);
    let engine_exit = match engine.finish(EXIT_TIMEOUT) {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("the engine exited with {status}")),
        Err(error) => Err(format!("{error:#}")),
    };
    // The stream has ended (or the engine is gone), so the receiver returns.
    let received = receiver.finish();

    let fps = recording
        .as_ref()
        .map_or(60.0, |stop| stop.recording.video.fps.value());
    eprintln!("Decoding {} …", recording_path.display());
    let file = media::inspect(&recording_path, fps, |at| {
        eprintln!("  decoded {}", clock(at));
    })
    .map_err(|error| format!("{error:#}"));

    let outcome = Outcome {
        seconds: args.seconds as f64,
        seconds_run,
        aborted: aborted.map(|error| format!("{error:#}")),
        watch: &watch,
        recording,
        streaming,
        engine_exit,
        process: &process,
        measured: cfg!(target_os = "linux"),
        file,
        received: &received,
    };
    let checks = verdict::judge(&outcome);
    let passed = verdict::passed(&checks);
    let mut warnings = Vec::new();
    if let Ok(stop) = &outcome.recording {
        warnings.extend(
            stop.warnings
                .iter()
                .map(|warning| format!("recording: {warning}")),
        );
    }
    if let Ok(stop) = &outcome.streaming {
        warnings.extend(
            stop.warnings
                .iter()
                .map(|warning| format!("stream: {warning}")),
        );
    }
    if let Err(error) = &preview {
        warnings.push(format!("set_preview off: {error:#}"));
    }
    let info = Info {
        os: std::env::consts::OS,
        cpus: thread::available_parallelism().map_or(1, usize::from),
        engine_version: status["version"].as_str().unwrap_or_default().to_owned(),
        encoders: encoders(&outcome),
        max_queue_depth: watch.max_queue_depth,
        engine_cpu_pct: cpu_pct(&process),
        max_load_1m: max_load,
        events: watch.events.len(),
        warnings,
    };
    let result = json!({"passed": passed, "checks": checks, "info": info, "file": outcome.file.as_ref().ok()});
    timeline.write("result", result.clone())?;

    if passed && !args.keep {
        let _ = std::fs::remove_file(&recording_path);
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print!("{}", table(args, &checks, &info, &watch, &timeline_path));
    }
    Ok(passed)
}

/// The polling loop and what it collects.
struct Soak<'a> {
    engine: &'a mut Engine,
    timeline: &'a mut Timeline,
    started: Instant,
    watch: Watch,
    process: Vec<(f64, ProcessReading)>,
    max_load: Option<f64>,
}

impl Soak<'_> {
    /// Poll as the desktop does until `length` has passed. Returns early
    /// with an error when the engine stops answering or an output failed.
    fn poll(&mut self, length: Duration) -> Result<()> {
        let pid = self.engine.pid();
        let end = self.started + length;
        let mut next = Next::new(self.started);
        let mut last_cpu: Option<(Instant, f64)> = None;
        loop {
            let now = Instant::now();
            if now >= end {
                return Ok(());
            }
            if next.due(now, Every::Status) {
                self.engine
                    .request("get_status", Value::Null, REQUEST_TIMEOUT)?;
                self.engine
                    .request("get_mixer", Value::Null, REQUEST_TIMEOUT)?;
            }
            if next.due(now, Every::Scenes) {
                self.engine
                    .request("list_scenes", Value::Null, REQUEST_TIMEOUT)?;
            }
            if next.due(now, Every::Health) {
                let health = self
                    .engine
                    .request("get_health", Value::Null, REQUEST_TIMEOUT)?;
                let health: Health =
                    serde_json::from_value(health).context("an unexpected get_health result")?;
                let t_s = self.started.elapsed().as_secs_f64();
                let events = self.watch.observe(t_s, health.clone()).to_vec();
                for event in events {
                    eprintln!("{event}");
                    self.timeline
                        .write("event", serde_json::to_value(&event)?)?;
                }
                for (output, health) in [
                    ("recording", &health.recording),
                    ("stream", &health.streaming),
                ] {
                    if health
                        .as_ref()
                        .is_some_and(|health| health.state == "failed")
                    {
                        bail!("the {output} failed");
                    }
                }
            }
            if next.due(now, Every::Sample) {
                let t_s = self.started.elapsed().as_secs_f64();
                let reading = process::read(pid);
                let load = process::load_average();
                self.max_load = match (self.max_load, load) {
                    (Some(highest), Some(load)) => Some(highest.max(load)),
                    (highest, load) => highest.or(load),
                };
                // CPU in percent of one core since the previous sample.
                let cpu_pct = reading.and_then(|reading| {
                    let pct = last_cpu.map(|(at, cpu_s)| {
                        (reading.cpu_s - cpu_s) / at.elapsed().as_secs_f64() * 100.0
                    });
                    last_cpu = Some((Instant::now(), reading.cpu_s));
                    pct
                });
                if let Some(reading) = reading {
                    self.process.push((t_s, reading));
                }
                self.timeline.write(
                    "sample",
                    json!({"t_s": t_s, "engine": reading, "engine_cpu_pct": cpu_pct, "load_1m": load, "health": self.watch.last}),
                )?;
                if next.due(now, Every::Progress) {
                    eprintln!("{}", progress(t_s, reading, cpu_pct, load, &self.watch));
                }
            }
            thread::sleep(next.earliest().saturating_duration_since(Instant::now()));
        }
    }
}

#[derive(Clone, Copy)]
enum Every {
    Status,
    Health,
    Scenes,
    Sample,
    Progress,
}

impl Every {
    const ALL: [Every; 5] = [
        Every::Status,
        Every::Health,
        Every::Scenes,
        Every::Sample,
        Every::Progress,
    ];

    fn period(self) -> Duration {
        match self {
            Every::Status => STATUS_EVERY,
            Every::Health => HEALTH_EVERY,
            Every::Scenes => SCENES_EVERY,
            Every::Sample => SAMPLE_EVERY,
            Every::Progress => PROGRESS_EVERY,
        }
    }
}

/// When each poll is due next. A poll that falls behind (the engine was
/// slow to answer) is not repeated in a burst: it is due one period later.
struct Next([Instant; 5]);

impl Next {
    fn new(start: Instant) -> Self {
        Self(Every::ALL.map(|every| match every {
            // Status, health and scenes start right away; the first sample
            // and progress line after one period.
            Every::Sample | Every::Progress => start + every.period(),
            _ => start,
        }))
    }

    fn due(&mut self, now: Instant, every: Every) -> bool {
        let next = &mut self.0[every as usize];
        if now < *next {
            return false;
        }
        *next += every.period();
        if *next <= now {
            *next = now + every.period();
        }
        true
    }

    /// The progress line is printed with a sample, so it is not waited for.
    fn earliest(&self) -> Instant {
        self.0[..4]
            .iter()
            .copied()
            .min()
            .unwrap_or_else(Instant::now)
    }
}

fn health(engine: &mut Engine) -> Result<Health> {
    let health = engine.request("get_health", Value::Null, REQUEST_TIMEOUT)?;
    serde_json::from_value(health).context("an unexpected get_health result")
}

/// Stop an output and read its report; a failed stop keeps its message and
/// details (partial reports).
fn stop<T: serde::de::DeserializeOwned>(engine: &mut Engine, command: &str) -> Result<T, String> {
    let result = engine
        .request(command, Value::Null, STOP_TIMEOUT)
        .map_err(|error| format!("{error:#}"))?;
    serde_json::from_value(result)
        .map_err(|error| format!("{command}: an unexpected result: {error}"))
}

fn cpu_pct(process: &[(f64, ProcessReading)]) -> Option<f64> {
    let ((t0, first), (t1, last)) = (process.first()?, process.last()?);
    (t1 > t0).then(|| (last.cpu_s - first.cpu_s) / (t1 - t0) * 100.0)
}

fn encoders(outcome: &Outcome) -> Vec<String> {
    let mut encoders = Vec::new();
    if let Ok(stop) = &outcome.recording {
        let video = &stop.recording.video;
        encoders.push(format!(
            "recording {}{}",
            video.encoder,
            hardware(video.hardware)
        ));
    }
    if let Ok(stop) = &outcome.streaming {
        let video = &stop.streaming.video;
        encoders.push(format!(
            "stream {}{}",
            video.encoder,
            hardware(video.hardware)
        ));
    }
    encoders
}

fn hardware(hardware: bool) -> &'static str {
    if hardware { " (hardware)" } else { "" }
}

/// `mm:ss` (or `h:mm:ss`) since the outputs started.
fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn progress(
    t_s: f64,
    reading: Option<ProcessReading>,
    cpu_pct: Option<f64>,
    load: Option<f64>,
    watch: &Watch,
) -> String {
    let mut line = format!("[{}]", clock(t_s));
    if let Some(reading) = reading {
        line.push_str(&format!(
            " engine RSS {:.0} MiB, {} threads, {} fds,",
            reading.rss_bytes as f64 / (1024.0 * 1024.0),
            reading.threads,
            reading.fds
        ));
    }
    if let Some(cpu) = cpu_pct {
        line.push_str(&format!(" CPU {cpu:.0} %,"));
    }
    if let Some(load) = load {
        line.push_str(&format!(" load {load:.2},"));
    }
    if let Some(last) = &watch.last {
        let frames = |output: &Option<reports::OutputHealth>| {
            output.as_ref().map_or(0, |output| output.frames_encoded)
        };
        line.push_str(&format!(
            " frames encoded {} / {},",
            frames(&last.recording),
            frames(&last.streaming)
        ));
    }
    line.push_str(&format!(" events {}", watch.events.len()));
    line
}

fn table(args: &Args, checks: &[Check], info: &Info, watch: &Watch, timeline: &Path) -> String {
    let mut out = format!(
        "streamforge-soak: {} s on {} ({} CPUs), engine {}, {}\n",
        args.seconds,
        info.os,
        info.cpus,
        info.engine_version,
        info.encoders.join(", ")
    );
    for check in checks {
        let status = match check.status {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
        };
        out.push_str(&format!("{status} {:<16} {}\n", check.name, check.detail));
    }
    out.push_str(&format!(
        "info: deepest queues: recording {}, stream {}; engine CPU {}; highest load {}\n",
        info.max_queue_depth.recording,
        info.max_queue_depth.streaming,
        info.engine_cpu_pct
            .map_or_else(|| "-".to_owned(), |cpu| format!("{cpu:.0} % of one core")),
        info.max_load_1m
            .map_or_else(|| "-".to_owned(), |load| format!("{load:.2}"))
    ));
    for warning in &info.warnings {
        out.push_str(&format!("warning: {warning}\n"));
    }
    for event in &watch.events {
        out.push_str(&format!("event {event}\n"));
    }
    let verdict = if verdict::passed(checks) {
        "PASS"
    } else {
        "FAIL"
    };
    out.push_str(&format!(
        "{verdict} ({} events; timeline {})\n",
        info.events,
        timeline.display()
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polls_fall_due_by_period_without_bursts() {
        let start = Instant::now();
        let mut next = Next::new(start);
        assert!(next.due(start, Every::Status));
        assert!(!next.due(start, Every::Status));
        assert!(!next.due(start, Every::Sample));
        assert!(next.due(start + STATUS_EVERY, Every::Status));
        // A second behind: due once, then a full period later.
        let late = start + Duration::from_secs(1) + STATUS_EVERY;
        assert!(next.due(late, Every::Status));
        assert!(!next.due(late, Every::Status));
        assert!(next.due(late + STATUS_EVERY, Every::Status));
        assert!(next.due(start + SAMPLE_EVERY, Every::Sample));
    }

    #[test]
    fn clock_reads_minutes_and_hours() {
        assert_eq!(clock(59.9), "00:59");
        assert_eq!(clock(754.0), "12:34");
        assert_eq!(clock(3600.0), "1:00:00");
    }
}
