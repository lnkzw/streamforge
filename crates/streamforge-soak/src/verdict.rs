//! The soak test's verdict (`docs/testing.md`, Soak test): each check passes,
//! fails or is skipped with a one-line reason. Any lost media fails the run;
//! the timeline's events say when it happened.

use serde::Serialize;

use crate::media::FileCheck;
use crate::process::ProcessReading;
use crate::receiver::Received;
use crate::reports::{RecordingStop, StreamingStop};
use crate::trend::{Trend, trend};
use crate::watch::Watch;

/// Resident memory may grow this much from the early to the late window:
/// two 1080p BGRA frames. A leak of about 100 bytes per frame exceeds it in
/// an hour.
pub const MEMORY_GROWTH_LIMIT_BYTES: f64 = 16.0 * MIB;
/// How far audio and video may start and end apart in the file (as in the
/// recording tests), and at the receiver (FLV's millisecond timestamps).
const FILE_AV_TOLERANCE_S: f64 = 0.05;
const STREAM_AV_TOLERANCE_S: f64 = 0.15;
/// A stream read back from a server starts at a key frame after the reader
/// joined: it may miss this much of the start.
const READ_BACK_JOIN_S: f64 = 10.0;
/// AAC decoders add priming and padding: up to two 1024-sample frames.
const AUDIO_SAMPLE_TOLERANCE: u64 = 2048;
const MIB: f64 = 1024.0 * 1024.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

/// Everything a run produced.
pub struct Outcome<'a> {
    pub seconds: f64,
    pub seconds_run: f64,
    /// Why the run stopped before its time, if it did.
    pub aborted: Option<String>,
    pub watch: &'a Watch,
    pub recording: Result<RecordingStop, String>,
    pub streaming: Result<StreamingStop, String>,
    /// `Ok(())` for a clean exit.
    pub engine_exit: Result<(), String>,
    /// Engine readings with their time since the outputs started.
    pub process: &'a [(f64, ProcessReading)],
    pub file: Result<FileCheck, String>,
    pub received: &'a Received,
}

pub fn judge(outcome: &Outcome) -> Vec<Check> {
    vec![
        outputs(outcome),
        recording_loss(outcome),
        stream_loss(outcome),
        pipeline_loss(outcome, "render", "render."),
        pipeline_loss(outcome, "mix", "mix."),
        captures(outcome),
        memory(outcome),
        handles(outcome),
        file(outcome),
        delivery(outcome),
    ]
}

pub fn passed(checks: &[Check]) -> bool {
    checks.iter().all(|check| check.status != Status::Fail)
}

fn check(name: &'static str, problems: Vec<String>, fine: String) -> Check {
    if problems.is_empty() {
        Check {
            name,
            status: Status::Pass,
            detail: fine,
        }
    } else {
        Check {
            name,
            status: Status::Fail,
            detail: problems.join("; "),
        }
    }
}

fn skip(name: &'static str, why: impl Into<String>) -> Check {
    Check {
        name,
        status: Status::Skip,
        detail: why.into(),
    }
}

fn outputs(outcome: &Outcome) -> Check {
    let mut problems = Vec::new();
    if let Some(why) = &outcome.aborted {
        problems.push(format!(
            "stopped after {:.0} of {:.0} s: {why}",
            outcome.seconds_run, outcome.seconds
        ));
    }
    let last = outcome.watch.last.as_ref();
    for (output, health, expected) in [
        (
            "recording",
            last.and_then(|health| health.recording.as_ref()),
            "running",
        ),
        (
            "stream",
            last.and_then(|health| health.streaming.as_ref()),
            "connected",
        ),
    ] {
        match health {
            Some(health) if health.state == expected && health.running => {}
            Some(health) => problems.push(format!("the {output} was {} at the end", health.state)),
            None => problems.push(format!("no {output} in the last health reading")),
        }
    }
    if let Err(error) = &outcome.recording {
        problems.push(error.clone());
    }
    match &outcome.streaming {
        Ok(stop) => {
            let stream = &stop.streaming;
            if stream.state != "connected"
                || stream.connects != 1
                || stream.disconnects != 0
                || stream.reconnect_attempts != 0
            {
                problems.push(format!(
                    "the stream ended {} after {} connect(s), {} disconnect(s), {} reconnect \
                     attempt(s){}",
                    stream.state,
                    stream.connects,
                    stream.disconnects,
                    stream.reconnect_attempts,
                    stream
                        .last_error
                        .as_ref()
                        .map(|error| format!(", last error: {error}"))
                        .unwrap_or_default()
                ));
            }
        }
        Err(error) => problems.push(error.clone()),
    }
    if let Err(error) = &outcome.engine_exit {
        problems.push(error.clone());
    }
    check(
        "outputs",
        problems,
        format!(
            "recording and stream ran {:.0} s; the stream connected once and never \
             reconnected; the engine exited cleanly",
            outcome.seconds_run
        ),
    )
}

fn recording_loss(outcome: &Outcome) -> Check {
    let Ok(stop) = &outcome.recording else {
        return skip("recording loss", "no recording report");
    };
    let recording = &stop.recording;
    let tracks = &recording.audio_tracks;
    let frames = recording.video.frames_dropped;
    let blocks: u64 = tracks.iter().map(|track| track.blocks_dropped).sum();
    let gaps: u64 = tracks.iter().map(|track| track.gap_samples).sum();
    let jumps: u64 = tracks.iter().map(|track| track.discontinuities).sum();
    let summary = format!(
        "{frames} frames and {blocks} audio blocks dropped, {gaps} gap samples, {jumps} \
         discontinuities ({} frames encoded, {} audio track(s))",
        recording.video.frames_encoded,
        tracks.len()
    );
    let lost = frames + blocks + gaps + jumps > 0;
    check(
        "recording loss",
        if lost {
            vec![summary.clone()]
        } else {
            Vec::new()
        },
        summary,
    )
}

fn stream_loss(outcome: &Outcome) -> Check {
    let Ok(stop) = &outcome.streaming else {
        return skip("stream loss", "no streaming report");
    };
    let stream = &stop.streaming;
    // Before its first video frame a stream drops what came earlier, by
    // design, so that both start together; after a disconnect, offline
    // drops are lost media.
    let (start_frames, start_blocks) = if stream.disconnects == 0 {
        (stream.frames_dropped_offline, stream.blocks_dropped_offline)
    } else {
        (0, 0)
    };
    let tracks = &stream.audio_tracks;
    let frames =
        stream.video.frames_dropped.saturating_sub(start_frames) + stream.frames_undelivered;
    let blocks = tracks
        .iter()
        .map(|track| track.blocks_dropped)
        .sum::<u64>()
        .saturating_sub(start_blocks);
    let gaps =
        tracks.iter().map(|track| track.gap_samples).sum::<u64>() + stream.samples_undelivered;
    let jumps: u64 = tracks.iter().map(|track| track.discontinuities).sum();
    let summary = format!(
        "{frames} frames and {blocks} audio blocks lost, {gaps} gap or undelivered samples, \
         {jumps} discontinuities ({} frames encoded; dropped at the start by design: \
         {start_frames} frames, {start_blocks} blocks)",
        stream.video.frames_encoded
    );
    let lost = frames + blocks + gaps + jumps > 0;
    check(
        "stream loss",
        if lost {
            vec![summary.clone()]
        } else {
            Vec::new()
        },
        summary,
    )
}

/// Render or mixer counters that grew while the outputs ran, with what
/// else the readings show.
fn pipeline_loss(outcome: &Outcome, name: &'static str, prefix: &str) -> Check {
    let watch = outcome.watch;
    let (Some(first), Some(last)) = (&watch.first, &watch.last) else {
        return skip(name, "no health readings");
    };
    let lost: Vec<_> = watch
        .pipeline_losses()
        .into_iter()
        .filter(|(counter, grown)| counter.starts_with(prefix) && *grown > 0)
        .map(|(counter, grown)| format!("{counter} +{grown}"))
        .collect();
    let info = if name == "render" {
        match (&first.render, &last.render) {
            (Some(first), Some(last)) => {
                let time = last.render_time.as_ref();
                format!(
                    "0 skipped ticks; {} late frames; render time over the pipeline: mean \
                     {:.2} ms, p99 {:.2} ms, max {:.2} ms",
                    last.late_frames.saturating_sub(first.late_frames),
                    time.map_or(0.0, |time| time.mean_ms),
                    time.map_or(0.0, |time| time.p99_ms),
                    time.map_or(0.0, |time| time.max_ms),
                )
            }
            _ => return skip(name, "no render statistics"),
        }
    } else {
        match (&first.mix, &last.mix) {
            (Some(first), Some(last)) => {
                let inputs: Vec<_> = last
                    .inputs
                    .iter()
                    .map(|input| {
                        let trimmed_before = first
                            .inputs
                            .iter()
                            .find(|before| before.name == input.name)
                            .map_or(0, |before| before.trimmed_frames);
                        let optional = |value: Option<f64>, unit: &str| {
                            value.map_or_else(
                                || "-".to_owned(),
                                |value| format!("{value:.0} {unit}"),
                            )
                        };
                        format!(
                            "{}: drift {}, {} frames trimmed, fill {}",
                            input.name,
                            optional(input.drift_ppm, "ppm"),
                            input.trimmed_frames.saturating_sub(trimmed_before),
                            optional(input.fill_ms, "ms")
                        )
                    })
                    .collect();
                format!(
                    "0 skipped blocks, no input underruns or overruns; {} late blocks; {}",
                    last.late_blocks.saturating_sub(first.late_blocks),
                    inputs.join(", ")
                )
            }
            _ => return skip(name, "no mixer statistics"),
        }
    };
    check(name, lost, info)
}

fn captures(outcome: &Outcome) -> Check {
    let Some(last) = &outcome.watch.last else {
        return skip("captures", "no health readings");
    };
    if last.captures.is_empty() {
        return skip("captures", "no captures (synthetic sources only)");
    }
    let ended: Vec<_> = last
        .captures
        .iter()
        .filter(|capture| capture.state != "running")
        .map(|capture| {
            format!(
                "{} {} {}{}",
                capture.kind,
                capture.name,
                capture.state,
                capture
                    .reason
                    .as_ref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            )
        })
        .collect();
    let names: Vec<_> = last
        .captures
        .iter()
        .map(|capture| format!("{} {}", capture.kind, capture.name))
        .collect();
    check(
        "captures",
        ended,
        format!("all running to the end: {}", names.join(", ")),
    )
}

fn readings(outcome: &Outcome, value: impl Fn(&ProcessReading) -> f64) -> Vec<(f64, f64)> {
    outcome
        .process
        .iter()
        .map(|(t, reading)| (*t, value(reading)))
        .collect()
}

/// Why a resource trend is not judged.
fn unjudged(outcome: &Outcome) -> String {
    if outcome.process.is_empty() {
        format!("not measured on {}", std::env::consts::OS)
    } else {
        format!(
            "run too short: needs {:.0} min (warm-up and two windows)",
            (crate::trend::WARM_UP_S + 2.0 * crate::trend::WINDOW_S) / 60.0
        )
    }
}

fn memory(outcome: &Outcome) -> Check {
    let Some(rss) = trend(&readings(outcome, |reading| reading.rss_bytes as f64)) else {
        return skip("memory", unjudged(outcome));
    };
    let summary = format!(
        "engine RSS median {:.1} MiB after the warm-up, {:.1} MiB at the end ({:+.1} MiB, limit \
         {:+.0}); slope {:+.2} MiB/h",
        rss.early / MIB,
        rss.late / MIB,
        (rss.late - rss.early) / MIB,
        MEMORY_GROWTH_LIMIT_BYTES / MIB,
        rss.slope_per_hour / MIB
    );
    let grew = rss.late - rss.early > MEMORY_GROWTH_LIMIT_BYTES;
    check(
        "memory",
        if grew {
            vec![summary.clone()]
        } else {
            Vec::new()
        },
        summary,
    )
}

/// Threads and file descriptors: the late window's highest count must not
/// exceed the early one's.
fn handles(outcome: &Outcome) -> Check {
    let threads = trend(&readings(outcome, |reading| reading.threads as f64));
    let fds = trend(&readings(outcome, |reading| reading.fds as f64));
    let (Some(threads), Some(fds)) = (threads, fds) else {
        return skip("threads and fds", unjudged(outcome));
    };
    let line = |name: &str, trend: &Trend| {
        format!(
            "{name} at most {:.0} after the warm-up, {:.0} at the end",
            trend.early_max, trend.late_max
        )
    };
    let summary = format!("{}; {}", line("threads", &threads), line("fds", &fds));
    let grew = threads.late_max > threads.early_max || fds.late_max > fds.early_max;
    check(
        "threads and fds",
        if grew {
            vec![summary.clone()]
        } else {
            Vec::new()
        },
        summary,
    )
}

fn file(outcome: &Outcome) -> Check {
    let file = match &outcome.file {
        Ok(file) => file,
        Err(error) => {
            return check("file", vec![error.clone()], String::new());
        }
    };
    let video = &file.video;
    let mut problems = Vec::new();
    if file.decode_errors > 0 {
        problems.push(format!(
            "{} decoder errors, the first: {}",
            file.decode_errors,
            file.first_error.as_deref().unwrap_or("-")
        ));
    }
    if video.out_of_order > 0 {
        problems.push(format!("{} frames out of order", video.out_of_order));
    }
    if video.gaps > 0 {
        problems.push(format!(
            "{} gaps between frames, the longest {:.3} s",
            video.gaps, video.longest_step_s
        ));
    }
    if let Ok(stop) = &outcome.recording {
        let report = &stop.recording;
        if video.frames != report.video.frames_encoded {
            problems.push(format!(
                "{} frames decoded, {} encoded",
                video.frames, report.video.frames_encoded
            ));
        }
        if file.audio.len() != report.audio_tracks.len() {
            problems.push(format!(
                "{} audio tracks in the file, {} recorded",
                file.audio.len(),
                report.audio_tracks.len()
            ));
        }
        for (index, (decoded, encoded)) in file.audio.iter().zip(&report.audio_tracks).enumerate() {
            if decoded.samples.abs_diff(encoded.samples_encoded) > AUDIO_SAMPLE_TOLERANCE {
                problems.push(format!(
                    "audio track {}: {} samples decoded, {} encoded",
                    index + 1,
                    decoded.samples,
                    encoded.samples_encoded
                ));
            }
        }
    }
    for (index, audio) in file.audio.iter().enumerate() {
        let (start, end) = (audio.start_s - video.start_s, audio.end_s - video.end_s);
        if start.abs() >= FILE_AV_TOLERANCE_S || end.abs() >= FILE_AV_TOLERANCE_S {
            problems.push(format!(
                "audio track {} starts {start:+.3} s and ends {end:+.3} s from the video",
                index + 1
            ));
        }
    }
    let drift = file
        .audio
        .iter()
        .map(|audio| (audio.end_s - video.end_s).abs())
        .fold(0.0, f64::max);
    check(
        "file",
        problems,
        format!(
            "decoded {} frames ({}x{}, {:.1} s) and {} audio track(s) without errors, gaps or \
             reordering; A/V end apart {:.3} s",
            video.frames,
            video.width,
            video.height,
            video.end_s - video.start_s,
            file.audio.len(),
            drift
        ),
    )
}

fn delivery(outcome: &Outcome) -> Check {
    let received = outcome.received;
    let mut problems = Vec::new();
    if let Some(error) = &received.error {
        problems.push(error.clone());
    }
    let (Some(video), Some(audio)) = (&received.video, &received.audio) else {
        problems.push("the receiver did not get both video and audio".to_owned());
        return check("stream delivery", problems, String::new());
    };
    if !received.first_video_is_key {
        problems.push("the first video packet was not a key frame".to_owned());
    }
    let span = video.end_s - video.first_s;
    if received.read_back {
        // The reader joined late, so packets are not counted against the
        // sender's: nothing may be missing between them, and they must
        // cover the run.
        if received.video_gaps > 0 {
            problems.push(format!(
                "{} gaps between video packets, the longest {:.3} s",
                received.video_gaps, received.longest_video_step_s
            ));
        }
        if span < outcome.seconds_run - READ_BACK_JOIN_S {
            problems.push(format!(
                "the read-back covers {span:.1} s of {:.1} s",
                outcome.seconds_run
            ));
        }
    } else if let Ok(stop) = &outcome.streaming {
        let stream = &stop.streaming;
        let sent = stream
            .video
            .frames_encoded
            .saturating_sub(stream.frames_undelivered);
        if video.packets != sent {
            problems.push(format!(
                "{} video packets arrived, {sent} were delivered",
                video.packets
            ));
        }
    }
    let end = audio.end_s - video.end_s;
    if end.abs() >= STREAM_AV_TOLERANCE_S {
        problems.push(format!("audio ends {end:+.3} s from the video"));
    }
    let source = if received.read_back {
        "read back from the server"
    } else {
        "at the local ingest"
    };
    check(
        "stream delivery",
        problems,
        format!(
            "{} video and {} audio packets {source} ({}x{}, {span:.1} s), no gaps; A/V end apart \
             {:.3} s",
            video.packets,
            audio.packets,
            received.width,
            received.height,
            end.abs()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{AudioCheck, VideoCheck};
    use crate::receiver::Track;
    use crate::reports::{
        AudioTrack, Capture, Health, Mix, OutputHealth, RecordingFile, Render, StreamReport,
        VideoTrack,
    };

    const FRAMES: u64 = 216_000;

    fn health(skipped_ticks: u64, capture: &str) -> Health {
        let output = |state: &str| OutputHealth {
            state: state.to_owned(),
            running: true,
            ..OutputHealth::default()
        };
        Health {
            render: Some(Render {
                skipped_ticks,
                ..Render::default()
            }),
            mix: Some(Mix::default()),
            recording: Some(output("running")),
            streaming: Some(output("connected")),
            captures: vec![Capture {
                kind: "screen".to_owned(),
                name: "Screen".to_owned(),
                state: capture.to_owned(),
                ..Capture::default()
            }],
        }
    }

    fn watch(end: Health) -> Watch {
        let mut watch = Watch::default();
        watch.observe(0.25, health(1, "running"));
        watch.observe(3600.0, end);
        watch
    }

    fn video() -> VideoTrack {
        VideoTrack {
            frames_encoded: FRAMES,
            ..VideoTrack::default()
        }
    }

    fn audio() -> Vec<AudioTrack> {
        vec![AudioTrack {
            samples_encoded: 172_800_000,
            ..AudioTrack::default()
        }]
    }

    fn recording() -> RecordingStop {
        RecordingStop {
            recording: RecordingFile {
                video: video(),
                audio_tracks: audio(),
            },
            warnings: Vec::new(),
        }
    }

    fn streaming() -> StreamingStop {
        StreamingStop {
            streaming: StreamReport {
                state: "connected".to_owned(),
                connects: 1,
                disconnects: 0,
                reconnect_attempts: 0,
                frames_dropped_offline: 0,
                blocks_dropped_offline: 1,
                frames_undelivered: 0,
                samples_undelivered: 0,
                last_error: None,
                video: video(),
                audio_tracks: vec![AudioTrack {
                    blocks_dropped: 1,
                    ..AudioTrack::default()
                }],
            },
            warnings: Vec::new(),
        }
    }

    fn file() -> FileCheck {
        FileCheck {
            video: VideoCheck {
                width: 1920,
                height: 1080,
                frames: FRAMES,
                start_s: 0.0,
                end_s: 3600.0,
                ..VideoCheck::default()
            },
            audio: vec![AudioCheck {
                samples: 172_800_000 + 1024,
                start_s: 0.0,
                end_s: 3600.01,
            }],
            ..FileCheck::default()
        }
    }

    fn received() -> Received {
        let track = |packets| Track {
            packets,
            first_s: 0.0,
            end_s: 3600.0,
        };
        Received {
            video: Some(track(FRAMES)),
            audio: Some(track(168_750)),
            width: 1920,
            height: 1080,
            first_video_is_key: true,
            ..Received::default()
        }
    }

    /// A reading every 10 s, the RSS given per time.
    fn process(rss_mib: impl Fn(f64) -> f64) -> Vec<(f64, ProcessReading)> {
        (0..=360)
            .map(|i| {
                let t = f64::from(i * 10);
                let reading = ProcessReading {
                    rss_bytes: (rss_mib(t) * MIB) as u64,
                    cpu_s: t,
                    threads: 48,
                    fds: 60,
                };
                (t, reading)
            })
            .collect()
    }

    fn outcome<'a>(
        watch: &'a Watch,
        process: &'a [(f64, ProcessReading)],
        received: &'a Received,
    ) -> Outcome<'a> {
        Outcome {
            seconds: 3600.0,
            seconds_run: 3600.0,
            aborted: None,
            watch,
            recording: Ok(recording()),
            streaming: Ok(streaming()),
            engine_exit: Ok(()),
            process,
            file: Ok(file()),
            received,
        }
    }

    fn failed(checks: &[Check]) -> Vec<&'static str> {
        checks
            .iter()
            .filter(|check| check.status == Status::Fail)
            .map(|check| check.name)
            .collect()
    }

    #[test]
    fn a_clean_hour_passes_and_start_drops_do_not_count() {
        let watch = watch(health(1, "running"));
        let process = process(|_| 400.0);
        let received = received();
        let checks = judge(&outcome(&watch, &process, &received));
        assert!(passed(&checks), "{checks:#?}");
        assert!(
            checks.iter().all(|check| check.status == Status::Pass),
            "{checks:#?}"
        );
        let stream = checks
            .iter()
            .find(|check| check.name == "stream loss")
            .unwrap();
        assert!(
            stream.detail.contains("0 frames and 0 audio blocks lost"),
            "{}",
            stream.detail
        );
    }

    #[test]
    fn every_kind_of_loss_fails_its_check() {
        // A skipped tick and an ended capture during the run.
        let watch = watch(health(2, "ended"));
        // 20 MiB more at the end than after the warm-up.
        let process = process(|t| 400.0 + 20.0 * t / 3600.0 * 3600.0 / 2700.0);
        let mut received = received();
        received.video.as_mut().unwrap().packets -= 1;
        let mut outcome = outcome(&watch, &process, &received);
        let mut recording = recording();
        recording.recording.audio_tracks[0].gap_samples = 480;
        outcome.recording = Ok(recording);
        let mut streaming = streaming();
        streaming.streaming.disconnects = 1;
        outcome.streaming = Ok(streaming);
        let mut file = file();
        file.video.gaps = 1;
        outcome.file = Ok(file);
        let checks = judge(&outcome);
        assert!(!passed(&checks));
        assert_eq!(
            failed(&checks),
            [
                "outputs",
                "recording loss",
                // After a disconnect, the offline block is lost media.
                "stream loss",
                "render",
                "captures",
                "memory",
                "file",
                "stream delivery"
            ]
        );
    }

    #[test]
    fn a_read_back_stream_is_judged_by_gaps_and_coverage_not_by_count() {
        let watch = watch(health(1, "running"));
        let process = process(|_| 400.0);
        let delivery = |received: &Received| {
            judge(&outcome(&watch, &process, received))
                .into_iter()
                .find(|check| check.name == "stream delivery")
                .unwrap()
        };
        // Joined 2 s late: fewer packets than sent, still whole.
        let mut late = received();
        late.read_back = true;
        let video = late.video.as_mut().unwrap();
        video.packets = FRAMES - 120;
        video.first_s = 2.0;
        assert_eq!(
            delivery(&late).status,
            Status::Pass,
            "{:?}",
            delivery(&late)
        );
        let mut gaps = late.clone();
        gaps.video_gaps = 2;
        gaps.longest_video_step_s = 0.5;
        assert_eq!(delivery(&gaps).status, Status::Fail);
        let mut short = late;
        short.video.as_mut().unwrap().first_s = 20.0;
        let check = delivery(&short);
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains("covers 3580.0 s of 3600.0 s"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn short_or_unmeasured_runs_skip_the_resource_checks() {
        let watch = watch(health(1, "running"));
        let received = received();
        let checks = judge(&outcome(&watch, &[], &received));
        let skipped: Vec<_> = checks
            .iter()
            .filter(|check| check.status == Status::Skip)
            .map(|check| check.name)
            .collect();
        assert_eq!(skipped, ["memory", "threads and fds"]);
        assert!(passed(&checks));
        let short = process(|_| 400.0)[..60].to_vec();
        let memory = judge(&outcome(&watch, &short, &received))
            .into_iter()
            .find(|check| check.name == "memory")
            .unwrap();
        assert_eq!(memory.status, Status::Skip);
        assert!(memory.detail.contains("too short"), "{}", memory.detail);
    }
}
