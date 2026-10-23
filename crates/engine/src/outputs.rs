//! Output commands: recording and streaming attach to the shared media
//! pipeline, which starts with the first output and stops with the last.
//!
//! Outputs are independent: each has its own encoders, queue and thread, so
//! a failing stream never stops a recording (and vice versa).

use std::fs::{self, OpenOptions};
use std::io::ErrorKind as IoErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use audio_engine::{CHANNELS, RATE};
use command_api::{
    CommandError, ErrorCode, HealthReport, MediaSources, OutputHealth, OutputKind, OutputProfiles,
    PipelineReport, RecordingReport, RecordingRequest, RecordingStarted, StreamingReport,
    StreamingRequest, StreamingStarted,
};
use encoder_ffmpeg::{
    AudioSettings, Recorder, RecorderConfig, RecorderError, RecorderErrorKind, RecordingStats,
    StreamConfig, StreamState, StreamStats, Streamer, VideoSettings,
};
use media_core::{MediaTimestamp, duration_millis};
use serde_json::{Map, Value, json};

use crate::Engine;
use crate::composite::{CANVAS_HEIGHT, CANVAS_WIDTH, FPS};
use crate::pipeline::{OutputId, Pipeline, since};

/// Bound for each blocking network operation of a stream (connect, write).
const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct RecordingOutput {
    recorder: Recorder,
    path: PathBuf,
    /// `path` as shown to clients, computed once at start.
    display: String,
    t0: Instant,
}

pub(crate) struct StreamingOutput {
    streamer: Streamer,
    t0: Instant,
}

impl Engine {
    pub(crate) fn pipeline_running(&self) -> bool {
        self.pipeline.as_ref().is_some_and(Pipeline::is_running)
    }

    /// Live counters of the pipeline and the open outputs (`get_health`).
    /// Reads atomics and snapshots only; never waits for a media thread.
    pub(crate) fn health(&self) -> HealthReport {
        let live = self.pipeline.as_ref().map(Pipeline::live);
        let elapsed_ms = |t0: Instant| duration_millis(t0.elapsed());
        let recording = self.recording.as_ref().map(|output| OutputHealth {
            target: output.display.clone(),
            elapsed_ms: elapsed_ms(output.t0),
            running: self.recording_running(),
            state: None,
            video_encoder: output.recorder.video_encoder().clone(),
            counters: output.recorder.live(),
        });
        let streaming = self.streaming.as_ref().map(|output| {
            let state = self.streaming_state();
            OutputHealth {
                target: output.streamer.target().to_owned(),
                elapsed_ms: elapsed_ms(output.t0),
                running: state.is_some_and(|state| state != StreamState::Failed),
                state,
                video_encoder: output.streamer.video_encoder().clone(),
                counters: output.streamer.live(),
            }
        });
        let (render, mix) = live.unzip();
        HealthReport {
            pipeline_running: self.pipeline_running(),
            render,
            mix,
            recording,
            streaming,
            captures: self
                .pipeline
                .as_ref()
                .map(Pipeline::captures)
                .unwrap_or_default(),
        }
    }

    /// The recording is open and all its workers run.
    pub(crate) fn recording_running(&self) -> bool {
        self.recording
            .as_ref()
            .is_some_and(|output| output.recorder.is_running())
            && self.pipeline_running()
    }

    /// State of the open stream; a dead pipeline counts as failed.
    pub(crate) fn streaming_state(&self) -> Option<StreamState> {
        let output = self.streaming.as_ref()?;
        Some(if self.pipeline_running() {
            output.streamer.state()
        } else {
            StreamState::Failed
        })
    }

    /// Commands that capture or render on their own need the devices and
    /// CPU the pipeline uses, and they would block the engine (and so
    /// `stop_*`) for their duration; they fail while outputs are open.
    pub(crate) fn ensure_idle(&self, command: &str) -> Result<(), CommandError> {
        if self.pipeline.is_none() {
            return Ok(());
        }
        let open: Vec<&str> = [
            self.recording.as_ref().map(|_| "recording"),
            self.streaming.as_ref().map(|_| "streaming"),
            self.preview_enabled.then_some("preview"),
        ]
        .into_iter()
        .flatten()
        .collect();
        Err(CommandError::new(
            ErrorCode::InvalidState,
            format!(
                "`{command}` is not available while outputs are open ({}); stop them first",
                open.join(", ")
            ),
        )
        .with_details(json!({ "open_outputs": open })))
    }

    /// Make sure the pipeline runs; returns whether this call started it.
    /// It renders the active scene and mixes the configured audio, unless
    /// `sources` (an output's own) replace the audio and add a screen layer.
    pub(crate) fn ensure_pipeline(
        &mut self,
        sources: Option<&MediaSources>,
    ) -> Result<bool, CommandError> {
        match &self.pipeline {
            Some(_) if sources.is_some() => Err(CommandError::new(
                ErrorCode::InvalidState,
                "the media pipeline already runs (for the preview or another output); omit the \
                 screen layer parameters (screen, source_id, source_types, cursor) to join it, \
                 or turn the preview off first",
            )),
            Some(pipeline) if !pipeline.is_running() => Err(CommandError::new(
                ErrorCode::InvalidState,
                "the media pipeline stopped unexpectedly; turn the preview off and stop the \
                 open outputs to collect their reports",
            )),
            Some(_) => Ok(false),
            None => {
                // Every run mixes the configured channels (ADR 0017).
                self.pipeline = Some(Pipeline::start(
                    self.capture.as_mut(),
                    self.scenes.active(),
                    self.audio.clone(),
                    sources.and_then(|sources| sources.screen.as_ref()),
                )?);
                Ok(true)
            }
        }
    }

    /// Stop the pipeline if no output or preview uses it anymore.
    pub(crate) fn release_pipeline(&mut self) -> (PipelineReport, Vec<String>) {
        if self.recording.is_some() || self.streaming.is_some() || self.preview_enabled {
            return (PipelineReport::default(), Vec::new());
        }
        match self.pipeline.take() {
            Some(pipeline) => pipeline.stop(),
            None => (PipelineReport::default(), Vec::new()),
        }
    }

    pub(crate) fn start_recording(
        &mut self,
        request: &RecordingRequest,
    ) -> Result<RecordingStarted, CommandError> {
        if let Some(open) = &self.recording {
            let message = if self.recording_running() {
                "a recording is already running; stop it first"
            } else {
                "the open recording stopped unexpectedly; call stop_recording to finalize it \
                 and read the error"
            };
            return Err(CommandError::new(ErrorCode::InvalidState, message)
                .with_details(json!({ "recording": open.path.display().to_string() })));
        }
        let path = reserve(&self.output_dir.join(&request.file_name))?;
        let started_pipeline = match self.ensure_pipeline(request.sources.as_ref()) {
            Ok(started) => started,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        };
        let Some((mix, tracks, names, mut warnings)) = self
            .pipeline
            .as_ref()
            .map(|pipeline| recording_tracks(pipeline, &self.profiles))
        else {
            let _ = fs::remove_file(&path);
            return Err(CommandError::new(
                ErrorCode::Internal,
                "media pipeline missing",
            ));
        };
        let recorder = Recorder::start(RecorderConfig {
            path: path.clone(),
            video: VideoSettings::h264(CANVAS_WIDTH, CANVAS_HEIGHT, FPS).with_encoders(
                request
                    .video_encoder
                    .unwrap_or(self.profiles.recording.video_encoder),
            ),
            audio: AudioSettings::aac(RATE, CHANNELS),
            audio_tracks: names,
        });
        let recorder = match recorder {
            Ok(recorder) => recorder,
            Err(error) => {
                // Only our just-created (empty or header-only) file is removed.
                let _ = fs::remove_file(&path);
                if started_pipeline {
                    let _ = self.release_pipeline();
                }
                return Err(output_error("recording", error));
            }
        };
        let Some(pipeline) = &self.pipeline else {
            return Err(CommandError::new(
                ErrorCode::Internal,
                "media pipeline missing",
            ));
        };
        let t0 = pipeline.attach(OutputId::Recording, recorder.input(), mix, tracks);
        warnings.extend(pipeline.audio_warnings());
        let started = RecordingStarted {
            path: display_path(&path),
            width: CANVAS_WIDTH,
            height: CANVAS_HEIGHT,
            fps: FPS,
            sample_rate: RATE,
            channels: CHANNELS,
            video_encoder: recorder.video_encoder().clone(),
            warnings,
        };
        self.recording = Some(RecordingOutput {
            recorder,
            display: display_path(&path),
            path,
            t0,
        });
        Ok(started)
    }

    pub(crate) fn stop_recording(&mut self) -> Result<RecordingReport, CommandError> {
        let RecordingOutput { recorder, t0, .. } = self
            .recording
            .take()
            .ok_or_else(|| CommandError::new(ErrorCode::InvalidState, "no recording is running"))?;
        // One end for both tracks: what the producers emit after it is cut
        // (and counted); what they owe before it is waited for.
        let end = now_since(t0);
        recorder.end_at(end);
        if let Some(pipeline) = &self.pipeline {
            pipeline.finish_output(OutputId::Recording, end);
        }
        let result = recorder.stop();
        let (pipeline, mut warnings) = self.release_pipeline();
        match result {
            Ok(recording) => {
                warnings.extend(recording_warnings(&recording));
                Ok(RecordingReport {
                    recording,
                    pipeline,
                    warnings,
                })
            }
            Err(error) => {
                let mut details = Map::new();
                if let Some(partial) = &error.partial {
                    details.insert("recording".into(), to_value(partial));
                }
                if let Value::Object(pipeline) = to_value(&pipeline) {
                    details.extend(pipeline);
                }
                details.insert("warnings".into(), to_value(&warnings));
                Err(output_error("recording", error).with_details(Value::Object(details)))
            }
        }
    }

    pub(crate) fn start_streaming(
        &mut self,
        request: &StreamingRequest,
    ) -> Result<StreamingStarted, CommandError> {
        if self.streaming.is_some() {
            return Err(CommandError::new(
                ErrorCode::InvalidState,
                "a stream is already open; stop it first",
            ));
        }
        let started_pipeline = self.ensure_pipeline(request.sources.as_ref())?;
        let video_bitrate_kbps = request
            .video_bitrate_kbps
            .unwrap_or(self.profiles.stream.video_bitrate_kbps);
        let streamer = Streamer::start(StreamConfig {
            target: request.target.clone(),
            video: VideoSettings::h264_live(CANVAS_WIDTH, CANVAS_HEIGHT, FPS, video_bitrate_kbps)
                .with_encoders(
                    request
                        .video_encoder
                        .unwrap_or(self.profiles.stream.video_encoder),
                ),
            audio: AudioSettings::aac_live(RATE, CHANNELS),
            reconnect: request.reconnect,
            io_timeout: STREAM_IO_TIMEOUT,
        });
        let streamer = match streamer {
            Ok(streamer) => streamer,
            Err(error) => {
                if started_pipeline {
                    let _ = self.release_pipeline();
                }
                return Err(output_error("streaming", error)
                    .with_details(json!({ "target": request.target.redacted() })));
            }
        };
        let Some(pipeline) = &self.pipeline else {
            return Err(CommandError::new(
                ErrorCode::Internal,
                "media pipeline missing",
            ));
        };
        let (mix, mut warnings) = mix_column(pipeline, &self.profiles, OutputKind::Stream);
        let t0 = pipeline.attach(OutputId::Streaming, streamer.input(), mix, Vec::new());
        warnings.extend(pipeline.audio_warnings());
        let started = StreamingStarted {
            target: streamer.target().to_owned(),
            width: CANVAS_WIDTH,
            height: CANVAS_HEIGHT,
            fps: FPS,
            video_bitrate_kbps,
            sample_rate: RATE,
            channels: CHANNELS,
            video_encoder: streamer.video_encoder().clone(),
            warnings,
        };
        self.streaming = Some(StreamingOutput { streamer, t0 });
        Ok(started)
    }

    pub(crate) fn stop_streaming(&mut self) -> Result<StreamingReport, CommandError> {
        let StreamingOutput { streamer, t0 } = self
            .streaming
            .take()
            .ok_or_else(|| CommandError::new(ErrorCode::InvalidState, "no stream is open"))?;
        let end = now_since(t0);
        streamer.end_at(end);
        if let Some(pipeline) = &self.pipeline {
            pipeline.finish_output(OutputId::Streaming, end);
        }
        let result = streamer.stop();
        let (pipeline, mut warnings) = self.release_pipeline();
        match result {
            Ok(streaming) => {
                warnings.extend(streaming_warnings(&streaming));
                Ok(StreamingReport {
                    streaming,
                    pipeline,
                    warnings,
                })
            }
            Err(error) => {
                // Keep what the pipeline reported even though the stream's
                // own statistics are lost.
                let mut details = Map::new();
                if let Value::Object(pipeline) = to_value(&pipeline) {
                    details.extend(pipeline);
                }
                details.insert("warnings".into(), to_value(&warnings));
                Err(output_error("streaming", error).with_details(Value::Object(details)))
            }
        }
    }
}

/// Reserve `path`; fails if it exists, so nothing is overwritten.
fn reserve(path: &Path) -> Result<PathBuf, CommandError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|_| path.to_owned())
        .map_err(|err| {
            let (code, hint) = match err.kind() {
                IoErrorKind::AlreadyExists => {
                    (ErrorCode::InvalidParams, "; choose another file_name")
                }
                IoErrorKind::NotFound => (
                    ErrorCode::NotFound,
                    "; the engine's output directory does not exist",
                ),
                IoErrorKind::PermissionDenied => (
                    ErrorCode::PermissionDenied,
                    "; the engine's output directory is not writable",
                ),
                _ => (ErrorCode::BackendError, ""),
            };
            CommandError::new(
                code,
                format!("cannot create {}: {err}{hint}", path.display()),
            )
            .with_details(json!({ "path": path.display().to_string() }))
        })
}

fn now_since(t0: Instant) -> MediaTimestamp {
    since(t0, Instant::now()).unwrap_or(MediaTimestamp {
        ticks: 0,
        time_base_num: 1,
        time_base_den: 1_000_000_000,
    })
}

fn recording_warnings(recording: &RecordingStats) -> Vec<String> {
    let mut warnings = Vec::new();
    if recording.video.frames_dropped > 0 || recording.audio.blocks_dropped > 0 {
        warnings.push(format!(
            "recorder dropped {} video frame(s) and {} audio block(s); the encoder fell behind",
            recording.video.frames_dropped, recording.audio.blocks_dropped
        ));
    }
    if recording.audio.gap_samples > 0 || recording.audio.discontinuities > 0 {
        warnings.push(format!(
            "audio timeline: {} sample(s) of silence inserted, {} discontinuities",
            recording.audio.gap_samples, recording.audio.discontinuities
        ));
    }
    warnings
}

fn streaming_warnings(stream: &StreamStats) -> Vec<String> {
    let mut warnings = Vec::new();
    let last_error = stream.last_error.as_deref().unwrap_or("unknown error");
    match stream.state {
        StreamState::Failed => warnings.push(format!(
            "the stream failed: {} reconnect attempt(s) exhausted; last error: {last_error}",
            stream.reconnect_attempts
        )),
        StreamState::Reconnecting => warnings.push(format!(
            "the stream was reconnecting when it was stopped; last error: {last_error}"
        )),
        _ => {}
    }
    if stream.disconnects > 0 {
        warnings.push(format!(
            "the connection was lost {} time(s); {} video frame(s) and {} audio block(s) were \
             dropped while offline",
            stream.disconnects, stream.frames_dropped_offline, stream.blocks_dropped_offline
        ));
    }
    if stream.frames_undelivered > 0 || stream.samples_undelivered > 0 {
        warnings.push(format!(
            "{} encoded video frame(s) and {} audio sample(s) never reached the server \
             (in flight when a connection broke)",
            stream.frames_undelivered, stream.samples_undelivered
        ));
    }
    // Producer-side drops (copy pool or queue full) cannot be told apart by
    // cause: the encoder, the network or a blocking reconnect may have
    // stalled the streamer.
    let queue_frames = stream
        .video
        .frames_dropped
        .saturating_sub(stream.frames_dropped_offline);
    let queue_blocks = stream
        .audio
        .blocks_dropped
        .saturating_sub(stream.blocks_dropped_offline);
    if queue_frames > 0 || queue_blocks > 0 {
        warnings.push(format!(
            "the stream's queue overflowed: {queue_frames} video frame(s) and {queue_blocks} \
             audio block(s) dropped while the streamer was busy (encoding, a slow network or \
             a reconnect)"
        ));
    }
    warnings
}

fn output_error(what: &str, error: RecorderError) -> CommandError {
    let code = match error.kind {
        RecorderErrorKind::Unsupported => ErrorCode::Unsupported,
        RecorderErrorKind::Network => ErrorCode::Unavailable,
        RecorderErrorKind::Output | RecorderErrorKind::Encoder => ErrorCode::BackendError,
        RecorderErrorKind::Internal => ErrorCode::Internal,
    };
    CommandError::new(code, format!("{what} failed: {}", error.message))
}

fn display_path(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_owned())
        .display()
        .to_string()
}

fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|err| Value::String(err.to_string()))
}

/// The recording's audio in `pipeline`'s run: the mix's column, the
/// further tracks' columns, every track's name (after its bus), and
/// warnings for buses the run does not have, which it leaves out. A bus is
/// recorded once, also where a missing mix falls back to the program bus.
pub(crate) fn recording_tracks(
    pipeline: &Pipeline,
    profiles: &OutputProfiles,
) -> (usize, Vec<usize>, Vec<String>, Vec<String>) {
    let (mix, mut warnings) = mix_column(pipeline, profiles, OutputKind::Recording);
    let mut columns = Vec::with_capacity(profiles.recording.tracks.len());
    for bus in &profiles.recording.tracks {
        match pipeline.mix_column(bus) {
            Some(column) if column != mix && !columns.contains(&column) => columns.push(column),
            Some(_) => warnings.push(format!(
                "the track bus `{bus}` is already the recording's mix in this run; the \
                 recording has it once"
            )),
            None => warnings.push(format!(
                "the track bus `{bus}` is not in this run; the recording leaves it out"
            )),
        }
    }
    let names = std::iter::once(mix)
        .chain(columns.iter().copied())
        .map(|column| pipeline.bus_name(column))
        .collect();
    (mix, columns, names, warnings)
}

/// The pipeline's id of an output that has a profile.
pub(crate) fn output_id(output: OutputKind) -> OutputId {
    match output {
        OutputKind::Recording => OutputId::Recording,
        OutputKind::Stream => OutputId::Streaming,
    }
}

/// The column of the bus `output`'s profile takes, in `pipeline`'s run;
/// the program bus, with a warning, if the run has no such bus.
fn mix_column(
    pipeline: &Pipeline,
    profiles: &OutputProfiles,
    output: OutputKind,
) -> (usize, Vec<String>) {
    let bus = profiles.mix(output);
    match pipeline.mix_column(bus) {
        Some(column) => (column, Vec::new()),
        // Profiles name configured buses, and a run mixes the configured
        // graph; this is only a safety net.
        None => (
            0,
            vec![format!(
                "the profile's mix `{bus}` is not in this run; it takes the program bus"
            )],
        ),
    }
}
