//! Measurement harness (architecture assessment §9, step 7): records the
//! reference scene for a fixed time, alone or next to a stream to a local
//! RTMP ingest, with or without polling `get_health`, and reports render
//! and mix timing, lost media and the process's CPU time.
//!
//! It drives the engine through the same commands as every client. Each
//! run measures one window: it starts after the outputs started and
//! settled, and ends before they stop. CPU time, render and mix counters
//! and the render mean cover that window (the latter from `get_health`
//! readings at both ends). Render percentiles and drops cover the whole
//! output, from the stop reports. The ingest runs in a child process, so
//! its work is not measured. There are no thresholds: the numbers are a
//! baseline to compare against on the same machine (`docs/testing.md`).

mod cpu;
mod ingest;
mod stats;

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use command_api::{
    Command, CommandHandler, CommandOutput, HealthReport, RecordingReport, StreamState,
    StreamingReport,
};
use engine::Engine;
use serde::Serialize;
use serde_json::{Value, json};

use crate::ingest::Ingest;
use crate::stats::{Summary, summarize};

/// How often the polled scenarios call `get_health` (a UI polls about as
/// often).
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Time between the outputs' start and the measured window.
const SETTLE: Duration = Duration::from_millis(500);
/// The unmeasured first run that warms caches, pools and encoders.
const WARM_UP_SECONDS: u64 = 2;

#[derive(Debug, Parser)]
#[command(
    name = "streamforge-bench",
    about = "Record (and stream) the reference scene for a fixed time and report render, mix, \
             loss and CPU figures. No thresholds: compare runs on the same machine."
)]
struct Args {
    /// Seconds each run measures, after its outputs started and settled.
    #[arg(long, default_value_t = 10)]
    seconds: u64,
    /// Runs per scenario; summaries are medians of the valid runs. Each
    /// round runs every scenario, starting with a different one.
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
    /// Internal: serve the RTMP ingest of a stream scenario.
    #[arg(long, hide = true)]
    ingest: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
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
    fn streams(self) -> bool {
        matches!(self, Self::RecordStream | Self::RecordStreamPolled)
    }

    fn polls(self) -> bool {
        matches!(self, Self::RecordPolled | Self::RecordStreamPolled)
    }

    /// The command-line name, e.g. `record-stream`.
    fn name(self) -> String {
        self.to_possible_value()
            .map(|value| value.get_name().to_owned())
            .unwrap_or_default()
    }
}

/// Counters over the measured window, from `get_health` at both ends.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct Window {
    frames_rendered: u64,
    skipped_ticks: u64,
    late_frames: u64,
    /// The stable measure of rendering work.
    render_mean_ms: Option<f64>,
    mix_late_blocks: u64,
    mix_skipped_blocks: u64,
}

#[derive(Debug, Serialize)]
struct Run {
    wall_s: f64,
    cpu_s: f64,
    /// CPU time over wall time, in percent of one core.
    cpu_pct: f64,
    #[serde(flatten)]
    window: Window,
    /// Over the whole pipeline run, setup and flush included; they follow
    /// scheduling jitter and vary more than the mean.
    render_p95_ms: Option<f64>,
    render_p99_ms: Option<f64>,
    render_max_ms: Option<f64>,
    health_polls: u64,
    outputs: Vec<OutputRun>,
    /// Why the run is left out of the summaries (a stream that lost its
    /// connection, an ingest error).
    invalid: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct OutputRun {
    output: &'static str,
    encoder: String,
    hardware: bool,
    frames_encoded: u64,
    /// Every dropped frame or audio block, whatever the cause.
    frames_dropped: u64,
    blocks_dropped: u64,
    /// Silence the recorder inserted for missing audio, and timeline jumps.
    gap_samples: u64,
    discontinuities: u64,
    /// Streams: media while not connected (included in the drops above).
    frames_dropped_offline: Option<u64>,
    blocks_dropped_offline: Option<u64>,
    /// Streams: encoded but never delivered (the connection broke).
    frames_undelivered: Option<u64>,
    samples_undelivered: Option<u64>,
    /// Streams: disconnects during the run.
    disconnects: Option<u32>,
    /// Streams: the state at the stop.
    state: Option<String>,
}

impl OutputRun {
    /// Offline drops of a stream that never disconnected: audio from before
    /// its first video frame, dropped by design to start both together.
    fn start_drops(&self) -> (u64, u64) {
        if self.disconnects.unwrap_or(0) > 0 {
            return (0, 0);
        }
        (
            self.frames_dropped_offline.unwrap_or(0),
            self.blocks_dropped_offline.unwrap_or(0),
        )
    }
}

impl Run {
    /// Media that was lost: frames and audio blocks dropped or never
    /// delivered (start drops aside), and blocks the mixer skipped.
    fn lost(&self) -> (u64, u64) {
        let (frames, blocks) = self
            .outputs
            .iter()
            .fold((0, 0), |(frames, blocks), output| {
                let (start_frames, start_blocks) = output.start_drops();
                (
                    frames
                        + output.frames_dropped.saturating_sub(start_frames)
                        + output.frames_undelivered.unwrap_or(0),
                    blocks + output.blocks_dropped.saturating_sub(start_blocks),
                )
            });
        (frames, blocks + self.window.mix_skipped_blocks)
    }

    /// Audio samples the outputs filled with silence or never delivered.
    fn gap_samples(&self) -> u64 {
        self.outputs
            .iter()
            .map(|output| output.gap_samples + output.samples_undelivered.unwrap_or(0))
            .sum()
    }

    fn start_drops(&self) -> (u64, u64) {
        self.outputs
            .iter()
            .fold((0, 0), |(frames, blocks), output| {
                let (more_frames, more_blocks) = output.start_drops();
                (frames + more_frames, blocks + more_blocks)
            })
    }

    fn encoders(&self) -> String {
        self.outputs
            .iter()
            .map(|output| output.encoder.as_str())
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

#[derive(Debug, Serialize)]
struct ScenarioResult {
    scenario: String,
    /// Over the valid runs.
    cpu_pct: Option<Summary>,
    render_mean_ms: Option<Summary>,
    render_p95_ms: Option<Summary>,
    runs: Vec<Run>,
    /// Runs that failed before they finished.
    errors: Vec<String>,
}

impl ScenarioResult {
    fn valid(&self) -> impl Iterator<Item = &Run> {
        self.runs.iter().filter(|run| run.invalid.is_none())
    }
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
    if args.ingest {
        return ingest::serve();
    }
    if args.seconds == 0 || args.runs == 0 {
        bail!("--seconds and --runs must be at least 1");
    }
    let scenarios = if args.scenarios.is_empty() {
        Scenario::value_variants().to_vec()
    } else {
        args.scenarios.clone()
    };
    let dir = scratch_dir()?;
    let measured = measure(&args, &scenarios, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    let results = Results {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        cpus: thread::available_parallelism().map_or(1, usize::from),
        seconds: args.seconds,
        runs: args.runs,
        video_encoder: args.video_encoder.clone(),
        scenarios: measured,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        print!("{}", table(&results));
    }
    let failed: usize = results
        .scenarios
        .iter()
        .map(|scenario| scenario.errors.len())
        .sum();
    if failed > 0 {
        bail!("{failed} run(s) failed");
    }
    Ok(())
}

/// A new directory for this run's recordings; never one that exists.
fn scratch_dir() -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.subsec_nanos());
    let dir =
        std::env::temp_dir().join(format!("streamforge-bench-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

fn measure(args: &Args, scenarios: &[Scenario], dir: &Path) -> Vec<ScenarioResult> {
    let mut file_number = 0u32;
    let mut next_file = || {
        file_number += 1;
        format!("bench-{file_number}.mkv")
    };
    let warm_up = if scenarios.iter().any(|scenario| scenario.streams()) {
        Scenario::RecordStream
    } else {
        Scenario::Record
    };
    let file = next_file();
    if let Err(error) = run_once(warm_up, WARM_UP_SECONDS, args, dir, &file) {
        eprintln!("warm-up: {error:#}");
    }
    let _ = std::fs::remove_file(dir.join(&file));
    let mut results: Vec<_> = scenarios
        .iter()
        .map(|scenario| ScenarioResult {
            scenario: scenario.name(),
            cpu_pct: None,
            render_mean_ms: None,
            render_p95_ms: None,
            runs: Vec::new(),
            errors: Vec::new(),
        })
        .collect();
    let count = scenarios.len();
    for round in 0..args.runs {
        // Each round starts with another scenario, so none always runs
        // first after the warm-up.
        for step in 0..count {
            let index = (step + round as usize) % count;
            let scenario = scenarios[index];
            let file = next_file();
            let outcome = run_once(scenario, args.seconds, args, dir, &file);
            // Each recording is only needed for its report.
            let _ = std::fs::remove_file(dir.join(&file));
            let label = format!("{} run {}/{}", scenario.name(), round + 1, args.runs);
            match outcome {
                Ok(run) => {
                    eprintln!("{label}: {}", run_line(&run));
                    results[index].runs.push(run);
                }
                Err(error) => {
                    eprintln!("{label} failed: {error:#}");
                    results[index]
                        .errors
                        .push(format!("run {}: {error:#}", round + 1));
                }
            }
        }
    }
    for result in &mut results {
        let valid: Vec<_> = result.valid().collect();
        let cpu: Vec<_> = valid.iter().map(|run| run.cpu_pct).collect();
        let mean: Vec<_> = valid
            .iter()
            .filter_map(|run| run.window.render_mean_ms)
            .collect();
        let p95: Vec<_> = valid.iter().filter_map(|run| run.render_p95_ms).collect();
        result.cpu_pct = summarize(&cpu);
        result.render_mean_ms = summarize(&mean);
        result.render_p95_ms = summarize(&p95);
    }
    results
}

fn run_once(
    scenario: Scenario,
    seconds: u64,
    args: &Args,
    dir: &Path,
    file_name: &str,
) -> Result<Run> {
    let mut engine = Engine::new().with_output_dir(dir);
    let ingest = scenario.streams().then(Ingest::spawn).transpose()?;
    run_command(
        &mut engine,
        "start_recording",
        json!({"file_name": file_name, "video_encoder": args.video_encoder}),
    )?;
    if let Some(ingest) = &ingest {
        run_command(
            &mut engine,
            "start_streaming",
            json!({"url": ingest.url, "video_encoder": args.video_encoder}),
        )?;
    }
    thread::sleep(SETTLE);

    let first = health(&mut engine)?;
    let cpu_before = cpu::process_time().context("cannot read the CPU time")?;
    let start = Instant::now();
    let length = Duration::from_secs(seconds);
    let mut health_polls = 0;
    while start.elapsed() < length {
        if scenario.polls() {
            // As an adapter would: read, then serialize for the client.
            let output = run_command(&mut engine, "get_health", json!({}))?;
            std::hint::black_box(serde_json::to_vec(&output)?);
            health_polls += 1;
        }
        thread::sleep(POLL_INTERVAL.min(length.saturating_sub(start.elapsed())));
    }
    let wall = start.elapsed();
    let cpu = cpu::process_time()
        .context("cannot read the CPU time")?
        .checked_sub(cpu_before)
        .context("the process CPU time went backwards")?;
    let last = health(&mut engine)?;

    let mut outputs = Vec::new();
    let mut invalid = None;
    if let Some(ingest) = ingest {
        let report = match run_command(&mut engine, "stop_streaming", json!({}))? {
            CommandOutput::StreamingReport(report) => report,
            other => bail!("unexpected stop_streaming output: {other:?}"),
        };
        print_warnings("stream", &report.warnings);
        invalid = stream_problem(&report);
        outputs.push(stream_output(&report));
        if let Some(error) = ingest.finish()? {
            invalid.get_or_insert(format!("the ingest reported: {error}"));
        }
    }
    let report = match run_command(&mut engine, "stop_recording", json!({}))? {
        CommandOutput::RecordingReport(report) => report,
        other => bail!("unexpected stop_recording output: {other:?}"),
    };
    print_warnings("recording", &report.warnings);
    outputs.insert(0, recording_output(&report));
    let render = report
        .pipeline
        .render
        .as_ref()
        .context("the recording report has no render statistics")?;
    let time = render.render_time.as_ref();
    let wall_s = wall.as_secs_f64();
    let cpu_s = cpu.as_secs_f64();
    Ok(Run {
        wall_s,
        cpu_s,
        cpu_pct: cpu_s / wall_s * 100.0,
        window: window(&first, &last),
        render_p95_ms: time.map(|time| time.p95_ms),
        render_p99_ms: time.map(|time| time.p99_ms),
        render_max_ms: time.map(|time| time.max_ms),
        health_polls,
        outputs,
        invalid,
    })
}

fn run_command(engine: &mut Engine, name: &str, params: Value) -> Result<CommandOutput> {
    let command = Command::decode(name, Some(&params)).map_err(|err| anyhow!("{name}: {err}"))?;
    engine.handle(command).map_err(|err| match &err.details {
        // Failed stops carry partial reports and warnings here.
        Some(details) => anyhow!("{name}: {err}; details: {details}"),
        None => anyhow!("{name}: {err}"),
    })
}

fn health(engine: &mut Engine) -> Result<HealthReport> {
    match run_command(engine, "get_health", json!({}))? {
        CommandOutput::Health(health) => Ok(*health),
        other => bail!("unexpected get_health output: {other:?}"),
    }
}

/// Counters between two health readings. The render mean over the window
/// follows from the running means and frame counts at both ends.
fn window(first: &HealthReport, last: &HealthReport) -> Window {
    let mut window = Window::default();
    if let (Some(before), Some(after)) = (&first.render, &last.render) {
        window.frames_rendered = after.frames_rendered.saturating_sub(before.frames_rendered);
        window.skipped_ticks = after.skipped_ticks.saturating_sub(before.skipped_ticks);
        window.late_frames = after.late_frames.saturating_sub(before.late_frames);
        let total = |frames: u64, time: Option<&command_api::LatencySummary>| {
            time.map_or(0.0, |time| time.mean_ms * frames as f64)
        };
        let spent = total(after.frames_rendered, after.render_time.as_ref())
            - total(before.frames_rendered, before.render_time.as_ref());
        window.render_mean_ms =
            (window.frames_rendered > 0).then(|| spent / window.frames_rendered as f64);
    }
    if let (Some(before), Some(after)) = (&first.mix, &last.mix) {
        window.mix_late_blocks = after.late_blocks.saturating_sub(before.late_blocks);
        window.mix_skipped_blocks = after.skipped_blocks.saturating_sub(before.skipped_blocks);
    }
    window
}

/// Why a stream run does not count: it did not stay connected.
fn stream_problem(report: &StreamingReport) -> Option<String> {
    let stream = &report.streaming;
    (stream.state != StreamState::Connected || stream.disconnects > 0).then(|| {
        format!(
            "the stream ended {} after {} disconnect(s)",
            stream.state.as_str(),
            stream.disconnects
        )
    })
}

fn print_warnings(output: &str, warnings: &[String]) {
    for warning in warnings {
        eprintln!("{output}: {warning}");
    }
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
        gap_samples: audio.gap_samples,
        discontinuities: audio.discontinuities,
        ..OutputRun::default()
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
        gap_samples: stream.audio.gap_samples,
        discontinuities: stream.audio.discontinuities,
        frames_dropped_offline: Some(stream.frames_dropped_offline),
        blocks_dropped_offline: Some(stream.blocks_dropped_offline),
        frames_undelivered: Some(stream.frames_undelivered),
        samples_undelivered: Some(stream.samples_undelivered),
        disconnects: Some(stream.disconnects),
        state: Some(stream.state.as_str().to_owned()),
    }
}

fn run_line(run: &Run) -> String {
    let (lost_frames, lost_blocks) = run.lost();
    let (start_frames, start_blocks) = run.start_drops();
    let mut line = format!(
        "cpu {:.1} %, render mean {} p95 {}, render skipped {} late {}, mix skipped {} late {}, \
         lost {lost_frames} frames / {lost_blocks} blocks, gaps {} samples, start drops \
         {start_frames}/{start_blocks} ({})",
        run.cpu_pct,
        optional_ms(run.window.render_mean_ms),
        optional_ms(run.render_p95_ms),
        run.window.skipped_ticks,
        run.window.late_frames,
        run.window.mix_skipped_blocks,
        run.window.mix_late_blocks,
        run.gap_samples(),
        run.encoders()
    );
    if let Some(invalid) = &run.invalid {
        line.push_str(&format!("; not counted: {invalid}"));
    }
    line
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

fn row(cells: [&str; 10]) -> String {
    format!(
        "{:<22} {:>18} {:>15} {:>15} {:>10} {:>10} {:>10} {:>6} {:>7}  {}\n",
        cells[0],
        cells[1],
        cells[2],
        cells[3],
        cells[4],
        cells[5],
        cells[6],
        cells[7],
        cells[8],
        cells[9]
    )
}

/// Medians of the valid runs with half their spread (`±`). The counter
/// columns show each column's highest value over the valid runs. `start`
/// is what streams dropped before their first video frame, by design.
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
    out.push_str(&row([
        "scenario",
        "CPU (% of 1 core)",
        "render mean",
        "render p95",
        "skip/late",
        "mix s/l",
        "lost f/b",
        "gaps",
        "start",
        "encoders",
    ]));
    let mut notes = Vec::new();
    for scenario in &results.scenarios {
        let valid: Vec<_> = scenario.valid().collect();
        let most = |value: &dyn Fn(&Run) -> u64| valid.iter().map(|run| value(run)).max();
        let pair = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (Some(a), Some(b)) => format!("{a}/{b}"),
            _ => "-".to_owned(),
        };
        let mut encoders: Vec<_> = valid.iter().map(|run| run.encoders()).collect();
        encoders.dedup();
        let gaps = most(&|run| run.gap_samples()).map_or_else(|| "-".to_owned(), |n| n.to_string());
        out.push_str(&row([
            &scenario.scenario,
            &summary_cell(scenario.cpu_pct, " %"),
            &summary_cell(scenario.render_mean_ms, " ms"),
            &summary_cell(scenario.render_p95_ms, " ms"),
            &pair(
                most(&|run| run.window.skipped_ticks),
                most(&|run| run.window.late_frames),
            ),
            &pair(
                most(&|run| run.window.mix_skipped_blocks),
                most(&|run| run.window.mix_late_blocks),
            ),
            &pair(most(&|run| run.lost().0), most(&|run| run.lost().1)),
            &gaps,
            &pair(
                most(&|run| run.start_drops().0),
                most(&|run| run.start_drops().1),
            ),
            &encoders.join(" / "),
        ]));
        for run in &scenario.runs {
            if let Some(invalid) = &run.invalid {
                notes.push(format!("{}: not counted: {invalid}", scenario.scenario));
            }
        }
        for error in &scenario.errors {
            notes.push(format!("{}: {error}", scenario.scenario));
        }
    }
    for note in notes {
        out.push_str(&format!("note: {note}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use command_api::{LatencySummary, LiveMixStats, LiveRenderStats};

    use super::*;

    fn reading(frames: u64, mean_ms: f64, skipped: u64, mix_skipped: u64) -> HealthReport {
        HealthReport {
            pipeline_running: true,
            render: Some(LiveRenderStats {
                frames_rendered: frames,
                skipped_ticks: skipped,
                render_time: Some(LatencySummary {
                    min_ms: 0.0,
                    mean_ms,
                    p95_ms: 0.0,
                    p99_ms: 0.0,
                    max_ms: 0.0,
                }),
                ..LiveRenderStats::default()
            }),
            mix: Some(LiveMixStats {
                skipped_blocks: mix_skipped,
                late_blocks: 1,
                ..LiveMixStats::default()
            }),
            recording: None,
            streaming: None,
            captures: Vec::new(),
        }
    }

    #[test]
    fn the_window_counts_only_its_own_frames() {
        // 100 cheap frames before the window (1 ms), then 300 at 2 ms.
        let first = reading(100, 1.0, 2, 1);
        let last = reading(400, 1.75, 5, 4);
        let counted = window(&first, &last);
        assert_eq!(counted.frames_rendered, 300);
        assert_eq!(counted.skipped_ticks, 3);
        assert_eq!(counted.mix_skipped_blocks, 3);
        assert_eq!(counted.mix_late_blocks, 0);
        let mean = counted.render_mean_ms.expect("mean");
        assert!((mean - 2.0).abs() < 1e-9, "{mean}");
        // No pipeline at one end: nothing to count.
        let idle = HealthReport {
            render: None,
            mix: None,
            ..reading(0, 0.0, 0, 0)
        };
        assert_eq!(window(&idle, &last), Window::default());
    }

    fn run(outputs: Vec<OutputRun>, mix_skipped: u64) -> Run {
        Run {
            wall_s: 1.0,
            cpu_s: 1.0,
            cpu_pct: 100.0,
            window: Window {
                mix_skipped_blocks: mix_skipped,
                ..Window::default()
            },
            render_p95_ms: None,
            render_p99_ms: None,
            render_max_ms: None,
            health_polls: 0,
            outputs,
            invalid: None,
        }
    }

    fn stream(disconnects: u32) -> OutputRun {
        OutputRun {
            output: "stream",
            frames_dropped: 4,
            blocks_dropped: 3,
            gap_samples: 10,
            frames_dropped_offline: Some(1),
            blocks_dropped_offline: Some(2),
            frames_undelivered: Some(5),
            samples_undelivered: Some(7),
            disconnects: Some(disconnects),
            ..OutputRun::default()
        }
    }

    #[test]
    fn start_drops_are_not_lost_but_outages_and_mixer_skips_are() {
        let recording = OutputRun {
            output: "recording",
            frames_dropped: 2,
            blocks_dropped: 1,
            gap_samples: 3,
            ..OutputRun::default()
        };
        let healthy = run(vec![recording.clone(), stream(0)], 6);
        // Frames: 2 + (4 - 1 start) + 5 undelivered; blocks: 1 + (3 - 2) + 6.
        assert_eq!(healthy.lost(), (10, 8));
        assert_eq!(healthy.start_drops(), (1, 2));
        assert_eq!(healthy.gap_samples(), 3 + 10 + 7);
        // After a disconnect, offline drops are lost media.
        let outage = run(vec![recording, stream(1)], 0);
        assert_eq!(outage.lost(), (11, 4));
        assert_eq!(outage.start_drops(), (0, 0));
    }

    #[test]
    fn scenario_names_are_the_command_line_names() {
        let names: Vec<_> = Scenario::value_variants()
            .iter()
            .map(|scenario| scenario.name())
            .collect();
        assert_eq!(
            names,
            [
                "record",
                "record-polled",
                "record-stream",
                "record-stream-polled"
            ]
        );
    }
}
