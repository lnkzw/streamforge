//! UI state and actions, free of any widget code.
//!
//! Every action becomes a command for the engine (the same names and
//! parameters the JSON-lines protocol uses), so validation and business
//! logic stay in the engine; widgets only call these methods and draw the
//! state that the responses fill in.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use command_api::{
    CommandError, CommandOutput, EngineStatus, MixerState, RecordingStarted, SceneList,
    StreamingStarted,
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
}

impl Default for SourceForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: SourceType::Solid,
            color: [40, 90, 200, 255],
            rect: None,
            opacity: 1.0,
        }
    }
}

/// What a pending request is for, so its response lands in the right place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Refresh {
    Status,
    Scenes,
    Mixer,
}

#[derive(Debug, Clone, Copy)]
enum Pending {
    Refresh(Refresh),
    Action,
}

pub struct Model {
    worker: EngineWorker,
    pending: HashMap<RequestId, Pending>,
    pub status: Option<EngineStatus>,
    pub scenes: Option<SceneList>,
    pub mixer: Option<MixerState>,
    pub preview_enabled: bool,
    pub recording: Option<RecordingStarted>,
    pub streaming: Option<StreamingStarted>,
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
            preview_enabled: false,
            recording: None,
            streaming: None,
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
        };
        self.submit(name, json!({}), Pending::Refresh(what));
    }

    /// Ask for status, scenes and mixer (skips what is already pending).
    pub fn refresh_all(&mut self) {
        self.refresh(Refresh::Status);
        self.refresh(Refresh::Scenes);
        self.refresh(Refresh::Mixer);
    }

    /// Status and mixer levels, for frequent polling.
    pub fn refresh_live(&mut self) {
        self.refresh(Refresh::Status);
        self.refresh(Refresh::Mixer);
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
            SourceType::Screen => "screen",
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

    pub fn set_source_opacity(&mut self, scene: &str, name: &str, opacity: f32) {
        self.act(
            "update_source",
            json!({ "scene": scene, "name": name, "opacity": opacity }),
        );
    }

    pub fn remove_source(&mut self, scene: &str, name: &str) {
        self.act("remove_source", json!({ "scene": scene, "name": name }));
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
            match response.result {
                Ok(output) => self.apply(output),
                Err(error) => self.failed(&response.name, &error),
            }
            // Actions change what status and mixer show.
            if matches!(pending, Some(Pending::Action)) {
                self.refresh_live();
            }
        }
        changed
    }

    fn apply(&mut self, output: CommandOutput) {
        match output {
            CommandOutput::Status(status) => self.status = Some(status),
            CommandOutput::SceneList(scenes) => self.scenes = Some(scenes),
            CommandOutput::MixerState(mixer) => self.mixer = Some(mixer),
            CommandOutput::PreviewState(preview) => {
                self.preview_enabled = preview.enabled;
                for warning in preview.warnings {
                    self.push(Level::Warning, warning);
                }
            }
            CommandOutput::RecordingStarted(started) => {
                self.push(Level::Info, format!("Recording to {}", started.path));
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
                self.push(Level::Info, format!("Streaming to {}", started.target));
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
