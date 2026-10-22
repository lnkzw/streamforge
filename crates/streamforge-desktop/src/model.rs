//! UI state and actions, free of any widget code.
//!
//! Every action becomes a command for the engine (the same names and
//! parameters the JSON-lines protocol uses), so validation and business
//! logic stay in the engine; widgets only call these methods and draw the
//! state that the responses fill in.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use command_api::{
    CaptureKind, CaptureState, CommandError, CommandOutput, EngineStatus, HealthReport, MixerState,
    OutputHealth, RecordingStarted, SceneList, ScreenSource, StreamState, StreamingStarted,
    VideoEncoderInfo,
};
use media_core::FrameMailbox;
use serde_json::{Map, Value, json};

use crate::worker::{EngineWorker, RequestId};

/// Lines kept in the activity log.
const LOG_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub level: Level,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    ColorBars,
    Solid,
    Screen,
}

/// What a new source should be; filled in by the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceForm {
    pub name: String,
    pub kind: SourceType,
    /// Straight RGBA.
    pub color: [u8; 4],
    /// `None`: the whole canvas; otherwise x, y, width, height.
    pub rect: Option<[f32; 4]>,
    pub opacity: f32,
    /// Screen sources: an id from [`Model::screen_sources`]; `None` opens
    /// the system picker.
    pub screen_source: Option<String>,
}

impl Default for SourceForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: SourceType::Solid,
            color: [40, 90, 200, 255],
            rect: None,
            opacity: 1.0,
            screen_source: None,
        }
    }
}

/// What a pending request is for, so its response lands in the right place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Refresh {
    Status,
    Scenes,
    Mixer,
    Health,
}

#[derive(Debug, Clone, Copy)]
enum Pending {
    Refresh(Refresh),
    Action,
    /// `list_screen_sources`; failing is normal where there is only a picker.
    ScreenSources,
}

pub struct Model {
    worker: EngineWorker,
    pending: HashMap<RequestId, Pending>,
    pub status: Option<EngineStatus>,
    pub scenes: Option<SceneList>,
    pub mixer: Option<MixerState>,
    /// Live counters and capture states (`get_health`), while a pipeline
    /// may run.
    pub health: Option<HealthReport>,
    /// The health strip for `health`, built once per reading.
    pub health_items: Vec<HealthItem>,
    health_received: Option<Instant>,
    pub preview_enabled: bool,
    pub recording: Option<RecordingStarted>,
    pub streaming: Option<StreamingStarted>,
    /// Sources that can be captured by id, or why there are none (e.g. the
    /// platform only has a picker); `None` until asked for.
    pub screen_sources: Option<Result<Vec<ScreenSource>, String>>,
    pub log: VecDeque<LogEntry>,
}

impl Model {
    pub fn new(worker: EngineWorker) -> Self {
        let mut model = Self {
            worker,
            pending: HashMap::new(),
            status: None,
            scenes: None,
            mixer: None,
            health: None,
            health_items: Vec::new(),
            health_received: None,
            preview_enabled: false,
            recording: None,
            streaming: None,
            screen_sources: None,
            log: VecDeque::new(),
        };
        model.refresh_all();
        model
    }

    pub fn preview_frames(&self) -> &Arc<FrameMailbox> {
        self.worker.preview()
    }

    /// Whether any command is still waiting for the engine.
    pub fn busy(&self) -> bool {
        self.pending
            .values()
            .any(|pending| matches!(pending, Pending::Action))
    }

    fn submit(&mut self, name: &str, params: Value, pending: Pending) {
        match self.worker.submit(name, params) {
            Ok(id) => {
                self.pending.insert(id, pending);
            }
            Err(_) => self.push(
                Level::Error,
                format!("{name}: the engine is busy; try again"),
            ),
        }
    }

    fn act(&mut self, name: &str, params: Value) {
        self.submit(name, params, Pending::Action);
    }

    fn refresh(&mut self, what: Refresh) {
        // One request of each kind at a time.
        if self
            .pending
            .values()
            .any(|pending| matches!(pending, Pending::Refresh(kind) if *kind == what))
        {
            return;
        }
        let name = match what {
            Refresh::Status => "get_status",
            Refresh::Scenes => "list_scenes",
            Refresh::Mixer => "get_mixer",
            Refresh::Health => "get_health",
        };
        self.submit(name, json!({}), Pending::Refresh(what));
    }

    /// Ask for status, scenes, mixer and health (skips what is already
    /// pending).
    pub fn refresh_all(&mut self) {
        self.refresh(Refresh::Scenes);
        self.refresh_live();
        self.refresh_health();
    }

    /// Status and mixer levels, for frequent polling.
    pub fn refresh_live(&mut self) {
        self.refresh(Refresh::Status);
        self.refresh(Refresh::Mixer);
    }

    /// Health while a pipeline may run (the preview or an output is on,
    /// also when the pipeline stopped unexpectedly); without one it is
    /// empty, so it is cleared instead of asked for.
    pub fn refresh_health(&mut self) {
        let pipeline = self.preview_enabled
            || self.recording.is_some()
            || self.streaming.is_some()
            || self
                .status
                .as_ref()
                .is_some_and(|status| status.pipeline_running);
        if pipeline {
            self.refresh(Refresh::Health);
        } else if self.health.is_some() {
            self.set_health(None);
        }
    }

    fn set_health(&mut self, health: Option<HealthReport>) {
        self.health_items = health.as_ref().map(health_items).unwrap_or_default();
        self.health_received = health.as_ref().map(|_| Instant::now());
        self.health = health;
    }

    /// How old the health reading is; it ages while the engine is busy
    /// with a slow command (connecting a stream, a picker).
    pub fn health_age(&self) -> Option<Duration> {
        self.health_received.map(|received| received.elapsed())
    }

    /// The capture behind screen source `source_id`, from the last health
    /// reading.
    pub fn source_health(&self, source_id: &str) -> SourceHealth {
        self.health
            .as_ref()
            .map_or(SourceHealth::NotCapturing, |health| {
                source_health(health, source_id)
            })
    }

    /// Ask the engine which screen sources can be captured by id.
    pub fn find_screen_sources(&mut self) {
        if !self
            .pending
            .values()
            .any(|pending| matches!(pending, Pending::ScreenSources))
        {
            self.submit("list_screen_sources", json!({}), Pending::ScreenSources);
        }
    }

    pub fn create_scene(&mut self, name: &str) {
        self.act("create_scene", json!({ "name": name }));
    }

    /// Deleting is irreversible; the UI asks first and then calls this.
    pub fn delete_scene_confirmed(&mut self, name: &str) {
        self.act("delete_scene", json!({ "name": name, "confirm": true }));
    }

    pub fn activate_scene(&mut self, name: &str) {
        self.act("set_active_scene", json!({ "name": name }));
    }

    pub fn add_source(&mut self, scene: &str, form: &SourceForm) {
        let mut params = Map::new();
        params.insert("scene".into(), scene.into());
        params.insert("name".into(), form.name.clone().into());
        let kind = match form.kind {
            SourceType::ColorBars => "color_bars",
            SourceType::Solid => {
                let [r, g, b, a] = form.color;
                params.insert(
                    "color".into(),
                    format!("#{r:02x}{g:02x}{b:02x}{a:02x}").into(),
                );
                "solid"
            }
            SourceType::Screen => {
                if let Some(id) = &form.screen_source {
                    params.insert("source_id".into(), id.clone().into());
                }
                "screen"
            }
        };
        params.insert("kind".into(), kind.into());
        if let Some([x, y, width, height]) = form.rect {
            for (key, value) in [("x", x), ("y", y), ("width", width), ("height", height)] {
                params.insert(key.into(), value.into());
            }
        }
        params.insert("opacity".into(), form.opacity.into());
        self.act("add_source", Value::Object(params));
    }

    /// Sources are named by their stable `id` from `list_scenes`.
    pub fn set_source_opacity(&mut self, id: &str, opacity: f32) {
        self.act("update_source", json!({ "id": id, "opacity": opacity }));
    }

    pub fn remove_source(&mut self, id: &str) {
        self.act("remove_source", json!({ "id": id }));
    }

    /// Start a screen source's capture again, optionally on another source
    /// from [`Model::screen_sources`]; outputs keep running. It ends the
    /// running capture, so the UI asks first while that one still works.
    pub fn restart_source(&mut self, id: &str, source_id: Option<&str>) {
        let mut params = Map::new();
        params.insert("id".into(), id.into());
        if let Some(id) = source_id {
            params.insert("source_id".into(), id.into());
        }
        self.act("restart_source", Value::Object(params));
    }

    pub fn set_preview(&mut self, enabled: bool) {
        self.act("set_preview", json!({ "enabled": enabled }));
    }

    pub fn set_strip(&mut self, index: usize, gain_db: f32, muted: bool) {
        self.act(
            "set_mixer_strip",
            json!({ "index": index, "gain_db": gain_db, "mute": muted }),
        );
    }

    pub fn set_master_gain(&mut self, gain_db: f32) {
        self.act("set_master_gain", json!({ "gain_db": gain_db }));
    }

    pub fn start_recording(&mut self, file_name: &str) {
        self.act("start_recording", json!({ "file_name": file_name }));
    }

    pub fn stop_recording(&mut self) {
        self.act("stop_recording", json!({}));
    }

    /// The key goes to the engine only; it is never logged or kept here.
    pub fn start_streaming(&mut self, url: &str, stream_key: &str) {
        let mut params = Map::new();
        params.insert("url".into(), url.into());
        if !stream_key.is_empty() {
            params.insert("stream_key".into(), stream_key.into());
        }
        self.act("start_streaming", Value::Object(params));
    }

    pub fn stop_streaming(&mut self) {
        self.act("stop_streaming", json!({}));
    }

    fn push(&mut self, level: Level, text: String) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(LogEntry { level, text });
    }

    /// Apply the responses that arrived; returns whether anything changed.
    pub fn update(&mut self) -> bool {
        let responses = self.worker.poll();
        let changed = !responses.is_empty();
        for response in responses {
            let pending = self.pending.remove(&response.id);
            if matches!(pending, Some(Pending::ScreenSources)) {
                self.screen_sources = Some(match response.result {
                    Ok(CommandOutput::ScreenSources(list)) => Ok(list.sources),
                    Ok(other) => Err(format!("unexpected response: {other:?}")),
                    Err(error) => Err(error.message),
                });
                continue;
            }
            match response.result {
                Ok(output) => self.apply(output),
                Err(error) => self.failed(&response.name, &error),
            }
            // Actions change what status, mixer and health show.
            if matches!(pending, Some(Pending::Action)) {
                self.refresh_live();
                self.refresh_health();
            }
        }
        changed
    }

    fn apply(&mut self, output: CommandOutput) {
        match output {
            CommandOutput::Status(status) => self.status = Some(status),
            CommandOutput::SceneList(scenes) => self.scenes = Some(scenes),
            CommandOutput::MixerState(mixer) => self.mixer = Some(mixer),
            CommandOutput::Health(health) => self.set_health(Some(*health)),
            CommandOutput::PreviewState(preview) => {
                self.preview_enabled = preview.enabled;
                if let Some(dropped) = preview.frames_dropped.filter(|dropped| *dropped > 0) {
                    self.push(
                        Level::Warning,
                        format!("{dropped} preview frame(s) dropped (the UI fell behind)"),
                    );
                }
                for warning in preview.warnings {
                    self.push(Level::Warning, warning);
                }
            }
            CommandOutput::RecordingStarted(started) => {
                self.push(
                    Level::Info,
                    format!(
                        "Recording to {} ({})",
                        started.path,
                        encoder_label(&started.video_encoder)
                    ),
                );
                self.recording = Some(started);
            }
            CommandOutput::RecordingReport(report) => {
                self.recording = None;
                let video = &report.recording.video;
                self.push(
                    Level::Info,
                    format!(
                        "Recording saved: {} ({:.1} s, {} frames, {} dropped)",
                        report.recording.path,
                        report.recording.duration_ms / 1000.0,
                        video.frames_encoded,
                        video.frames_dropped
                    ),
                );
                for warning in report.warnings {
                    self.push(Level::Warning, warning);
                }
            }
            CommandOutput::StreamingStarted(started) => {
                self.push(
                    Level::Info,
                    format!(
                        "Streaming to {} ({})",
                        started.target,
                        encoder_label(&started.video_encoder)
                    ),
                );
                self.streaming = Some(started);
            }
            CommandOutput::StreamingReport(report) => {
                self.streaming = None;
                let stream = &report.streaming;
                self.push(
                    Level::Info,
                    format!(
                        "Stream ended ({:?}): {:.1} s connected, {} reconnect(s)",
                        stream.state,
                        stream.connected_ms / 1000.0,
                        stream.reconnect_attempts
                    ),
                );
                for warning in report.warnings {
                    self.push(Level::Warning, warning);
                }
            }
            other => self.push(Level::Info, format!("{other:?}")),
        }
    }

    fn failed(&mut self, name: &str, error: &CommandError) {
        // A failed stop still ends the output on the engine side.
        match name {
            "stop_recording" => self.recording = None,
            "stop_streaming" => self.streaming = None,
            _ => {}
        }
        self.push(Level::Error, format!("{name}: {}", error.message));
    }
}

/// What the sources list shows next to a screen source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceHealth {
    /// No capture runs for it: no pipeline, or its scene is not rendered.
    NotCapturing,
    /// `last_frame_age_ms`: since the last new frame, or since the start
    /// if none came yet (`frames_consumed: 0`). Screens may deliver only on
    /// change, so a long age alone is not an error.
    Capturing {
        last_frame_age_ms: Option<u64>,
        frames_consumed: u64,
    },
    /// The system ended the capture; `restart_source` starts it again.
    Ended { reason: String },
}

fn source_health(health: &HealthReport, source_id: &str) -> SourceHealth {
    // A stopped pipeline renders nothing; its last states are history.
    if !health.pipeline_running {
        return SourceHealth::NotCapturing;
    }
    let capture = health.captures.iter().find(|capture| {
        capture.kind == CaptureKind::Screen && capture.id.as_deref() == Some(source_id)
    });
    match capture.map(|capture| &capture.state) {
        None => SourceHealth::NotCapturing,
        Some(CaptureState::Ended { reason }) => SourceHealth::Ended {
            reason: reason.clone(),
        },
        Some(CaptureState::Running) => {
            let rendered = health.render.as_ref().and_then(|render| {
                render
                    .sources
                    .iter()
                    .find(|source| source.id.as_str() == source_id)
            });
            SourceHealth::Capturing {
                last_frame_age_ms: rendered.and_then(|source| source.last_frame_age_ms),
                frames_consumed: rendered.map_or(0, |source| source.frames_consumed),
            }
        }
    }
}

/// One entry of the health strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthItem {
    pub level: Level,
    pub text: String,
}

/// The health strip: open outputs, render and mix trouble, ended captures.
/// Counters are totals since the start; drops are warnings because media
/// was lost, late frames are not (they were still rendered).
pub fn health_items(health: &HealthReport) -> Vec<HealthItem> {
    let mut items = Vec::new();
    let mut push = |level, text| items.push(HealthItem { level, text });
    if health.render.is_some() && !health.pipeline_running {
        push(
            Level::Error,
            "the media pipeline stopped; turn the preview off and stop the outputs to see why"
                .to_owned(),
        );
    }
    if let Some(recording) = &health.recording {
        let (level, label) = if recording.running {
            (drop_level(recording), "REC")
        } else {
            (Level::Error, "REC failed")
        };
        push(level, format!("{label}: {}", output_counters(recording)));
    }
    if let Some(streaming) = &health.streaming {
        let state = streaming.state.map_or("failed", StreamState::as_str);
        let level = match (streaming.running, streaming.state) {
            (false, _) => Level::Error,
            (true, Some(StreamState::Connected)) => drop_level(streaming),
            (true, _) => Level::Warning,
        };
        push(
            level,
            format!(
                "stream {state}: {}, {} reconnect(s)",
                output_counters(streaming),
                streaming.counters.reconnect_attempts
            ),
        );
    }
    if let Some(render) = &health.render {
        let level = if render.skipped_ticks > 0 {
            Level::Warning
        } else {
            Level::Info
        };
        let mut text = format!(
            "render: {} late, {} skipped",
            render.late_frames, render.skipped_ticks
        );
        if let Some(time) = &render.render_time {
            text.push_str(&format!(", p95 {:.1} ms", time.p95_ms));
        }
        push(level, text);
    }
    if let Some(mix) = &health.mix {
        if mix.late_blocks > 0 || mix.skipped_blocks > 0 {
            let level = if mix.skipped_blocks > 0 {
                Level::Warning
            } else {
                Level::Info
            };
            push(
                level,
                format!(
                    "mix: {} late, {} skipped blocks",
                    mix.late_blocks, mix.skipped_blocks
                ),
            );
        }
        if mix.invalid_samples > 0 {
            push(
                Level::Warning,
                format!("mix: {} invalid samples replaced", mix.invalid_samples),
            );
        }
        // Audio a live input captured but the mixer did not take in time.
        for input in &mix.inputs {
            if let Some(lost) = input.overrun_frames.filter(|lost| *lost > 0) {
                push(
                    Level::Warning,
                    format!("input `{}`: {lost} frames lost (overrun)", input.name),
                );
            }
        }
    }
    for capture in &health.captures {
        if let CaptureState::Ended { reason } = &capture.state {
            push(
                Level::Error,
                format!(
                    "{} `{}` ended: {reason}",
                    capture.kind.as_str(),
                    capture.name
                ),
            );
        }
    }
    items
}

fn drop_level(output: &OutputHealth) -> Level {
    if output.counters.frames_dropped > 0 || output.counters.blocks_dropped > 0 {
        Level::Warning
    } else {
        Level::Info
    }
}

fn output_counters(output: &OutputHealth) -> String {
    let counters = &output.counters;
    let mut text = format!(
        "{} frames, {} dropped",
        counters.frames_encoded, counters.frames_dropped
    );
    if counters.blocks_dropped > 0 {
        text.push_str(&format!(
            ", {} audio blocks dropped",
            counters.blocks_dropped
        ));
    }
    text.push_str(&format!(", queue {}", counters.queue_depth));
    text
}

/// E.g. `h264_vaapi, hardware` or `libx264, software`.
fn encoder_label(info: &VideoEncoderInfo) -> String {
    let kind = if info.hardware {
        "hardware"
    } else {
        "software"
    };
    format!("{}, {kind}", info.encoder)
}

#[cfg(test)]
mod tests {
    use command_api::{
        CaptureHealth, LiveInputStats, LiveMixStats, LiveOutputStats, LiveRenderStats,
        LiveSourceStats, SourceId,
    };

    use super::*;

    fn output(running: bool, state: Option<StreamState>, dropped: u64) -> OutputHealth {
        OutputHealth {
            target: "out.mkv".to_owned(),
            elapsed_ms: 2000,
            running,
            state,
            video_encoder: VideoEncoderInfo {
                encoder: "libx264".to_owned(),
                hardware: false,
                skipped: Vec::new(),
            },
            counters: LiveOutputStats {
                frames_encoded: 120,
                frames_dropped: dropped,
                queue_depth: 1,
                ..LiveOutputStats::default()
            },
        }
    }

    fn screen(id: &str, state: CaptureState) -> CaptureHealth {
        CaptureHealth {
            kind: CaptureKind::Screen,
            id: Some(id.to_owned()),
            name: format!("screen {}", id.trim_start_matches("src_")),
            state,
        }
    }

    fn report() -> HealthReport {
        HealthReport {
            pipeline_running: true,
            render: Some(LiveRenderStats {
                sources: vec![LiveSourceStats {
                    id: SourceId::from("src_1"),
                    name: "screen 1".to_owned(),
                    kind: "live",
                    frames_consumed: 10,
                    stale_ticks: 0,
                    missing_ticks: 0,
                    last_frame_age_ms: Some(1500),
                }],
                ..LiveRenderStats::default()
            }),
            mix: Some(LiveMixStats::default()),
            recording: Some(output(true, None, 0)),
            streaming: None,
            captures: vec![
                screen("src_1", CaptureState::Running),
                screen(
                    "src_2",
                    CaptureState::Ended {
                        reason: "the captured window was closed".to_owned(),
                    },
                ),
            ],
        }
    }

    #[test]
    fn source_health_joins_capture_state_and_frame_age() {
        let health = report();
        assert_eq!(
            source_health(&health, "src_1"),
            SourceHealth::Capturing {
                last_frame_age_ms: Some(1500),
                frames_consumed: 10,
            }
        );
        assert_eq!(
            source_health(&health, "src_2"),
            SourceHealth::Ended {
                reason: "the captured window was closed".to_owned()
            }
        );
        assert_eq!(source_health(&health, "src_3"), SourceHealth::NotCapturing);
        // Running, but not rendered here: no frame count to show.
        let mut health = health;
        health.captures.push(screen("src_4", CaptureState::Running));
        assert_eq!(
            source_health(&health, "src_4"),
            SourceHealth::Capturing {
                last_frame_age_ms: None,
                frames_consumed: 0,
            }
        );
        // A stopped pipeline's last states are history.
        health.pipeline_running = false;
        assert_eq!(source_health(&health, "src_1"), SourceHealth::NotCapturing);
    }

    #[test]
    fn health_items_show_a_stopped_pipeline_and_lost_audio() {
        let mut health = report();
        health.pipeline_running = false;
        health.mix = Some(LiveMixStats {
            invalid_samples: 4,
            inputs: vec![LiveInputStats {
                name: "mic".to_owned(),
                kind: "live",
                warmup_frames: 0,
                underrun_frames: 0,
                trimmed_frames: 0,
                fill_ms: Some(20.0),
                drift_ppm: None,
                overrun_frames: Some(480),
            }],
            ..LiveMixStats::default()
        });
        let items = health_items(&health);
        assert_eq!(items[0].level, Level::Error, "{items:?}");
        assert!(items[0].text.starts_with("the media pipeline stopped"));
        let texts: Vec<_> = items.iter().map(|item| item.text.as_str()).collect();
        assert!(
            texts.contains(&"mix: 4 invalid samples replaced"),
            "{texts:?}"
        );
        assert!(
            texts.contains(&"input `mic`: 480 frames lost (overrun)"),
            "{texts:?}"
        );
    }

    #[test]
    fn health_items_flag_failures_drops_and_ended_captures() {
        let mut health = report();
        let items = health_items(&health);
        let texts: Vec<_> = items.iter().map(|item| item.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "REC: 120 frames, 0 dropped, queue 1",
                "render: 0 late, 0 skipped",
                "screen `screen 2` ended: the captured window was closed",
            ]
        );
        assert_eq!(items[0].level, Level::Info);
        assert_eq!(items[2].level, Level::Error);

        health.recording = Some(output(true, None, 3));
        health.streaming = Some(output(true, Some(StreamState::Reconnecting), 0));
        if let Some(render) = &mut health.render {
            render.skipped_ticks = 2;
        }
        let items = health_items(&health);
        assert_eq!(items[0].level, Level::Warning, "{items:?}");
        assert_eq!(
            items[1],
            HealthItem {
                level: Level::Warning,
                text: "stream reconnecting: 120 frames, 0 dropped, queue 1, 0 reconnect(s)"
                    .to_owned(),
            }
        );
        assert_eq!(items[2].level, Level::Warning);

        health.recording = Some(output(false, None, 0));
        health.streaming = Some(output(false, Some(StreamState::Failed), 0));
        let items = health_items(&health);
        assert_eq!(items[0].level, Level::Error);
        assert!(items[0].text.starts_with("REC failed"), "{items:?}");
        assert_eq!(items[1].level, Level::Error);
    }
}
