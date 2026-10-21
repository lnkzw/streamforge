//! Measurement harness (architecture assessment §9, step 7): records the
//! reference scene for a fixed time, alone or next to a stream to an
//! in-process RTMP ingest, with or without polling `get_health`, and
//! reports render time, skipped and late ticks, encoder drops and the
//! process's CPU time.
//!
//! It drives the engine through the same commands as every client. CPU
//! time is measured between the outputs' start and stop, so encoder probing
//! and finalizing are not part of it. It is the whole process, so the
//! stream scenarios include the in-process ingest, which only demuxes.
//! There are no thresholds: the numbers are a baseline to compare against
//! on the same machine (see `docs/testing.md`).

mod cpu;
mod stats;

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use command_api::{Command, CommandHandler, CommandOutput, RecordingReport, StreamingReport};
use engine::Engine;
use rtmp_test_server::{ServerOptions, TestServer};
use serde::Serialize;
use serde_json::{Value, json};

use crate::stats::{Summary, summarize};

/// How often the polled scenarios call `get_health` (a UI polls about as
/// often).
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Parser)]
#[command(
    name = "streamforge-bench",
    about = "Record (and stream) the reference scene for a fixed time and report render, encoder \
             and CPU figures. No thresholds: compare runs on the same machine."
)]
struct Args {
    /// Seconds each run measures, after its outputs started.
    #[arg(long, default_value_t = 10)]
    seconds: u64,
    /// Runs per scenario; summaries are medians. Runs alternate between
    /// scenarios, so drift of the machine spreads over all of them.
    #[arg(long, default_value_t = 3)]
    runs: u32,
    /// H.264 encoder: auto (hardware if one works here, else software),
    /// software or hardware.
    #[arg(long, default_value = "auto")]
    video_encoder: String,
    /// Scenarios to run; repeatable. Default: all.
    #[arg(long = "scenario", value_enum)]
    scenarios: Vec<Scenario>,
    /// Print every run and the summaries as JSON instead of a table.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Scenario {
    /// One recording.
    Record,
    /// One recording, polling `get_health` every 100 ms.
    RecordPolled,
    /// A recording and a stream from the same pipeline.
    RecordStream,
    /// A recording and a stream, polling `get_health` every 100 ms.
    RecordStreamPolled,
}

impl Scenario {
    const ALL: [Self; 4] = [
        Self::Record,
        Self::RecordPolled,
        Self::RecordStream,
        Self::RecordStreamPolled,
    ];

    fn streams(self) -> bool {
        matches!(self, Self::RecordStream | Self::RecordStreamPolled)
    }

    fn polls(self) -> bool {
        matches!(self, Self::RecordPolled | Self::RecordStreamPolled)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::RecordPolled => "record-polled",
            Self::RecordStream => "record-stream",
            Self::RecordStreamPolled => "record-stream-polled",
        }
    }
}

#[derive(Debug, Serialize)]
struct Run {
    wall_s: f64,
    cpu_s: f64,
    /// CPU time over wall time, in percent of one core.
    cpu_pct: f64,
    frames_rendered: u64,
    skipped_ticks: u64,
    late_frames: u64,
    /// Mean render time: the stable measure of rendering work. p95 and
    /// above follow scheduling jitter and vary more between runs.
    render_mean_ms: Option<f64>,
    render_p95_ms: Option<f64>,
    render_p99_ms: Option<f64>,
    render_max_ms: Option<f64>,
    health_polls: u64,
    outputs: Vec<OutputRun>,
}

#[derive(Debug, Serialize)]
struct OutputRun {
    output: &'static str,
    encoder: String,
    hardware: bool,
    frames_encoded: u64,
    /// Every dropped frame or audio block, whatever the cause.
    frames_dropped: u64,
    blocks_dropped: u64,
    /// Streams: of those, media while not connected. This includes audio
    /// from before the stream's first video frame, dropped to start both
    /// together (one or two blocks).
    frames_dropped_offline: Option<u64>,
    blocks_dropped_offline: Option<u64>,
    /// Streams: the state at the stop (`connected` unless it failed).
    state: Option<String>,
}

impl Run {
    /// Frames and blocks dropped while the outputs were connected: media
    /// that was lost, as opposed to the stream's start alignment.
    fn dropped(&self) -> (u64, u64) {
        self.outputs
            .iter()
            .fold((0, 0), |(frames, blocks), output| {
                (
                    frames
                        + output
                            .frames_dropped
                            .saturating_sub(output.frames_dropped_offline.unwrap_or(0)),
                    blocks
                        + output
                            .blocks_dropped
                            .saturating_sub(output.blocks_dropped_offline.unwrap_or(0)),
                )
            })
    }

    /// Frames and blocks a stream dropped while not connected.
    fn offline(&self) -> (u64, u64) {
        self.outputs
            .iter()
            .fold((0, 0), |(frames, blocks), output| {
                (
                    frames + output.frames_dropped_offline.unwrap_or(0),
                    blocks + output.blocks_dropped_offline.unwrap_or(0),
                )
            })
    }
}

#[derive(Debug, Serialize)]
struct ScenarioResult {
    scenario: Scenario,
    cpu_pct: Option<Summary>,
    render_mean_ms: Option<Summary>,
    render_p95_ms: Option<Summary>,
    runs: Vec<Run>,
}

#[derive(Debug, Serialize)]
struct Results {
    os: &'static str,
    arch: &'static str,
    cpus: usize,
    seconds: u64,
    runs: u32,
    video_encoder: String,
    scenarios: Vec<ScenarioResult>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.seconds == 0 || args.runs == 0 {
        bail!("--seconds and --runs must be at least 1");
    }
    let scenarios = if args.scenarios.is_empty() {
        Scenario::ALL.to_vec()
    } else {
        args.scenarios.clone()
    };
    let dir = std::env::temp_dir().join(format!("streamforge-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let measured = measure(&args, &scenarios, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    let results = Results {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        cpus: thread::available_parallelism().map_or(1, usize::from),
        seconds: args.seconds,
        runs: args.runs,
        video_encoder: args.video_encoder.clone(),
        scenarios: measured?,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        print!("{}", table(&results));
    }
    Ok(())
}

fn measure(args: &Args, scenarios: &[Scenario], dir: &Path) -> Result<Vec<ScenarioResult>> {
    let mut runs: Vec<Vec<Run>> = scenarios.iter().map(|_| Vec::new()).collect();
    let mut file_number = 0u32;
    for round in 1..=args.runs {
        for (index, scenario) in scenarios.iter().enumerate() {
            file_number += 1;
            let file = dir.join(format!("bench-{file_number}.mkv"));
            let run = run_once(*scenario, args, &file)
                .with_context(|| format!("{} run {round}", scenario.name()))?;
            // Each recording is only needed for its report.
            let _ = std::fs::remove_file(&file);
            eprintln!(
                "{} run {round}/{}: {}",
                scenario.name(),
                args.runs,
                run_line(&run)
            );
            runs[index].push(run);
        }
    }
    Ok(scenarios
        .iter()
        .zip(runs)
        .map(|(scenario, runs)| {
            let cpu: Vec<_> = runs.iter().map(|run| run.cpu_pct).collect();
            let mean: Vec<_> = runs.iter().filter_map(|run| run.render_mean_ms).collect();
            let p95: Vec<_> = runs.iter().filter_map(|run| run.render_p95_ms).collect();
            ScenarioResult {
                scenario: *scenario,
                cpu_pct: summarize(&cpu),
                render_mean_ms: summarize(&mean),
                render_p95_ms: summarize(&p95),
                runs,
            }
        })
        .collect())
}

fn run_once(scenario: Scenario, args: &Args, file: &Path) -> Result<Run> {
    let dir: PathBuf = file.parent().context("file has no directory")?.to_owned();
    let file_name = file
        .file_name()
        .and_then(|name| name.to_str())
        .context("file name")?
        .to_owned();
    let mut engine = Engine::new().with_output_dir(dir);
    let server = scenario.streams().then(|| {
        TestServer::start(ServerOptions {
            accept_timeout: Duration::from_secs(60),
            ..ServerOptions::default()
        })
    });
    run_command(
        &mut engine,
        "start_recording",
        json!({"file_name": file_name, "video_encoder": args.video_encoder}),
    )?;
    if let Some(server) = &server {
        run_command(
            &mut engine,
            "start_streaming",
            json!({"url": server.url(), "video_encoder": args.video_encoder}),
        )?;
    }

    let cpu_before = cpu::process_time().context("cannot read the CPU time")?;
    let start = Instant::now();
    let length = Duration::from_secs(args.seconds);
    let mut health_polls = 0;
    while start.elapsed() < length {
        if scenario.polls() {
            run_command(&mut engine, "get_health", json!({}))?;
            health_polls += 1;
        }
        thread::sleep(POLL_INTERVAL.min(length.saturating_sub(start.elapsed())));
    }
    let wall = start.elapsed();
    let cpu = cpu::process_time().context("cannot read the CPU time")? - cpu_before;

    let mut outputs = Vec::new();
    if let Some(server) = server {
        let report = match run_command(&mut engine, "stop_streaming", json!({}))? {
            CommandOutput::StreamingReport(report) => report,
            other => bail!("unexpected stop_streaming output: {other:?}"),
        };
        outputs.push(stream_output(&report));
        for warning in &report.warnings {
            eprintln!("stream: {warning}");
        }
        let arrived = server.join();
        if let Some(error) = arrived.error {
            eprintln!("warning: the ingest reported: {error}");
        }
    }
    let report = match run_command(&mut engine, "stop_recording", json!({}))? {
        CommandOutput::RecordingReport(report) => report,
        other => bail!("unexpected stop_recording output: {other:?}"),
    };
    outputs.insert(0, recording_output(&report));
    for warning in &report.warnings {
        eprintln!("recording: {warning}");
    }
    let render = report
        .pipeline
        .render
        .as_ref()
        .context("the recording report has no render statistics")?;
    let wall_s = wall.as_secs_f64();
    let cpu_s = cpu.as_secs_f64();
    let time = render.render_time.as_ref();
    Ok(Run {
        wall_s,
        cpu_s,
        cpu_pct: cpu_s / wall_s * 100.0,
        frames_rendered: render.frames_rendered,
        skipped_ticks: render.skipped_ticks,
        late_frames: render.late_frames,
        render_mean_ms: time.map(|time| time.mean_ms),
        render_p95_ms: time.map(|time| time.p95_ms),
        render_p99_ms: time.map(|time| time.p99_ms),
        render_max_ms: time.map(|time| time.max_ms),
        health_polls,
        outputs,
    })
}

fn run_command(engine: &mut Engine, name: &str, params: Value) -> Result<CommandOutput> {
    let command = Command::decode(name, Some(&params)).map_err(|err| anyhow!("{name}: {err}"))?;
    engine
        .handle(command)
        .map_err(|err| anyhow!("{name}: {}", err.message))
}

fn recording_output(report: &RecordingReport) -> OutputRun {
    let (video, audio) = (&report.recording.video, &report.recording.audio);
    OutputRun {
        output: "recording",
        encoder: video.encoder.encoder.clone(),
        hardware: video.encoder.hardware,
        frames_encoded: video.frames_encoded,
        frames_dropped: video.frames_dropped,
        blocks_dropped: audio.blocks_dropped,
        frames_dropped_offline: None,
        blocks_dropped_offline: None,
        state: None,
    }
}

fn stream_output(report: &StreamingReport) -> OutputRun {
    let stream = &report.streaming;
    OutputRun {
        output: "stream",
        encoder: stream.video.encoder.encoder.clone(),
        hardware: stream.video.encoder.hardware,
        frames_encoded: stream.video.frames_encoded,
        // The track counts include media dropped while offline.
        frames_dropped: stream.video.frames_dropped,
        blocks_dropped: stream.audio.blocks_dropped,
        frames_dropped_offline: Some(stream.frames_dropped_offline),
        blocks_dropped_offline: Some(stream.blocks_dropped_offline),
        state: Some(stream.state.as_str().to_owned()),
    }
}

fn run_line(run: &Run) -> String {
    let (frames, blocks) = run.dropped();
    let (offline_frames, offline_blocks) = run.offline();
    let encoders: Vec<_> = run
        .outputs
        .iter()
        .map(|output| format!("{} {}", output.output, output.encoder))
        .collect();
    format!(
        "cpu {:.1} %, render mean {} p95 {}, skipped {}, late {}, dropped {frames} frames / \
         {blocks} blocks, offline {offline_frames}/{offline_blocks} ({})",
        run.cpu_pct,
        optional_ms(run.render_mean_ms),
        optional_ms(run.render_p95_ms),
        run.skipped_ticks,
        run.late_frames,
        encoders.join(", ")
    )
}

fn optional_ms(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |ms| format!("{ms:.2} ms"))
}

fn summary_cell(summary: Option<Summary>, unit: &str) -> String {
    match summary {
        Some(summary) => match summary.spread_pct {
            Some(spread) => format!("{:.2}{unit} ±{:.0}%", summary.median, spread / 2.0),
            None => format!("{:.2}{unit}", summary.median),
        },
        None => "-".to_owned(),
    }
}

/// Medians with half the spread (`±`), and the worst run's ticks and drops.
/// `offline` is what a stream dropped while not connected, including the
/// audio before its first video frame (one or two blocks).
fn table(results: &Results) -> String {
    let mut out = format!(
        "{} {}, {} CPUs, {} s per run, {} run(s), video encoder {}\n",
        results.os,
        results.arch,
        results.cpus,
        results.seconds,
        results.runs,
        results.video_encoder
    );
    out.push_str(&format!(
        "{:<22} {:>18} {:>16} {:>16} {:>8} {:>6} {:>12} {:>12}  {}\n",
        "scenario",
        "CPU (% of 1 core)",
        "render mean",
        "render p95",
        "skipped",
        "late",
        "dropped f/b",
        "offline f/b",
        "encoders"
    ));
    for scenario in &results.scenarios {
        let worst = |value: fn(&Run) -> u64| scenario.runs.iter().map(value).max().unwrap_or(0);
        let dropped_frames = worst(|run| run.dropped().0);
        let dropped_blocks = worst(|run| run.dropped().1);
        let offline_frames = worst(|run| run.offline().0);
        let offline_blocks = worst(|run| run.offline().1);
        let encoders = scenario.runs.first().map_or_else(String::new, |run| {
            run.outputs
                .iter()
                .map(|output| output.encoder.as_str())
                .collect::<Vec<_>>()
                .join(" + ")
        });
        out.push_str(&format!(
            "{:<22} {:>18} {:>16} {:>16} {:>8} {:>6} {:>12} {:>12}  {}\n",
            scenario.scenario.name(),
            summary_cell(scenario.cpu_pct, " %"),
            summary_cell(scenario.render_mean_ms, " ms"),
            summary_cell(scenario.render_p95_ms, " ms"),
            worst(|run| run.skipped_ticks),
            worst(|run| run.late_frames),
            format!("{dropped_frames}/{dropped_blocks}"),
            format!("{offline_frames}/{offline_blocks}"),
            encoders
        ));
    }
    out
}
