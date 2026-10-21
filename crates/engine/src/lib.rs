//! Engine controller: owns engine state and executes typed commands.
//!
//! Independent of UI and transport. The CLI, the JSON-lines server, the
//! desktop UI and the MCP server all drive the engine through
//! [`CommandHandler`]. Platform capture is injected as a [`CaptureBackend`]
//! by the composition root.

mod composite;
mod mix;
mod outputs;
mod pipeline;
mod scenes;
mod studio;
mod thread;

use outputs::{RecordingOutput, StreamingOutput};
use pipeline::Pipeline;
use scenes::SceneCollection;
pub use thread::{EngineHandle, EngineThread, PendingReply};

use std::path::PathBuf;
use std::sync::Arc;

use capture_api::{
    CaptureBackend, CaptureCapabilities, CaptureError, CaptureErrorKind, UnsupportedCapture,
};
use command_api::{
    AudioInputList, AudioSetup, Command, CommandError, CommandHandler, CommandList, CommandOutput,
    EngineStatus, ErrorCode, ScreenSourceList,
};
use encoder_ffmpeg::StreamState;
use media_core::FrameMailbox;

pub struct Engine {
    /// Shared by the outputs; declared first so it is dropped (producers
    /// stopped) before the outputs finalize.
    pipeline: Option<Pipeline>,
    /// Open outputs; dropping the engine finalizes them.
    recording: Option<RecordingOutput>,
    streaming: Option<StreamingOutput>,
    /// Scenes; the pipeline renders the active one.
    scenes: SceneCollection,
    /// Audio the pipeline mixes unless an output brings its own sources.
    audio: AudioSetup,
    /// The preview keeps the pipeline running.
    preview_enabled: bool,
    /// Where preview frames go (see `Engine::preview_frames`).
    preview_frames: Arc<FrameMailbox>,
    status: EngineStatus,
    capture: Box<dyn CaptureBackend>,
    /// Where commands may create files (snapshots); clients only name files.
    output_dir: PathBuf,
    /// Last capability query; `get_status` derives `capture_available` from it.
    capture_capabilities: Option<CaptureCapabilities>,
}

impl Engine {
    /// Engine without a capture backend; capture commands fail as unsupported.
    pub fn new() -> Self {
        Self::with_capture(Box::new(UnsupportedCapture::new(
            "no capture backend is configured for this engine instance",
        )))
    }

    pub fn with_capture(capture: Box<dyn CaptureBackend>) -> Self {
        Self {
            status: EngineStatus {
                application: "StreamForge".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                // Compositor, mixer and encoders are part of every build.
                media_engine_ready: true,
                pipeline_running: false,
                capture_available: false,
                streaming_active: false,
                streaming_state: None,
                recording_active: false,
                recording_failed: false,
                preview_active: false,
            },
            capture,
            output_dir: PathBuf::from("."),
            capture_capabilities: None,
            scenes: SceneCollection::new(),
            audio: default_audio(),
            preview_enabled: false,
            preview_frames: Arc::new(FrameMailbox::new()),
            pipeline: None,
            recording: None,
            streaming: None,
        }
    }

    /// Directory for files created by commands; defaults to the working directory.
    pub fn with_output_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.output_dir = dir.into();
        self
    }

    fn refresh_capabilities(&mut self) -> &CaptureCapabilities {
        let capabilities = self.capture.capabilities();
        self.status.capture_available =
            capabilities.screen.available || capabilities.audio.available;
        self.capture_capabilities.insert(capabilities)
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("status", &self.status)
            .field("capture_capabilities", &self.capture_capabilities)
            .finish_non_exhaustive()
    }
}

impl CommandHandler for Engine {
    fn handle(&mut self, command: Command) -> Result<CommandOutput, CommandError> {
        match command {
            Command::GetStatus => {
                if self.capture_capabilities.is_none() {
                    self.refresh_capabilities();
                }
                let recording = self.recording_running();
                self.status.recording_active = recording;
                self.status.recording_failed = self.recording.is_some() && !recording;
                let streaming = self.streaming_state();
                self.status.streaming_state = streaming;
                self.status.preview_active = self.preview_enabled;
                self.status.pipeline_running = self.pipeline_running();
                self.status.streaming_active = matches!(
                    streaming,
                    Some(
                        StreamState::Connecting
                            | StreamState::Connected
                            | StreamState::Reconnecting
                    )
                );
                Ok(CommandOutput::Status(self.status.clone()))
            }
            Command::GetHealth => Ok(CommandOutput::Health(Box::new(self.health()))),
            Command::ListCommands => Ok(CommandOutput::Commands(CommandList::all())),
            Command::GetCaptureCapabilities => Ok(CommandOutput::CaptureCapabilities(
                self.refresh_capabilities().clone(),
            )),
            Command::ListAudioInputs => self
                .capture
                .audio_inputs()
                .map(|inputs| CommandOutput::AudioInputs(AudioInputList { inputs }))
                .map_err(command_error),
            Command::ListScreenSources => self
                .capture
                .screen_sources()
                .map(|sources| CommandOutput::ScreenSources(ScreenSourceList { sources }))
                .map_err(command_error),
            Command::ProbeScreenCapture(request) => {
                self.ensure_idle("probe_screen_capture")?;
                self.capture
                    .probe_screen(&request)
                    .map(CommandOutput::VideoCaptureReport)
                    .map_err(command_error)
            }
            Command::ProbeAudioCapture(request) => {
                self.ensure_idle("probe_audio_capture")?;
                self.capture
                    .probe_audio(&request)
                    .map(CommandOutput::AudioCaptureReport)
                    .map_err(command_error)
            }
            Command::ProbeComposite(request) => {
                self.ensure_idle("probe_composite")?;
                composite::probe(self.capture.as_mut(), &self.output_dir, &request)
                    .map(|report| CommandOutput::CompositeReport(Box::new(report)))
            }
            Command::ProbeAudioMix(request) => {
                self.ensure_idle("probe_audio_mix")?;
                mix::probe(self.capture.as_mut(), &request)
                    .map(|report| CommandOutput::MixReport(Box::new(report)))
            }
            Command::StartRecording(request) => self
                .start_recording(&request)
                .map(CommandOutput::RecordingStarted),
            Command::StopRecording => self
                .stop_recording()
                .map(|report| CommandOutput::RecordingReport(Box::new(report))),
            Command::StartStreaming(request) => self
                .start_streaming(&request)
                .map(CommandOutput::StreamingStarted),
            Command::StopStreaming => self
                .stop_streaming()
                .map(|report| CommandOutput::StreamingReport(Box::new(report))),
            Command::ListScenes => Ok(CommandOutput::SceneList(self.list_scenes())),
            Command::CreateScene { name } => self.create_scene(&name).map(CommandOutput::SceneList),
            Command::DeleteScene { name } => self.delete_scene(&name).map(CommandOutput::SceneList),
            Command::SetActiveScene { name } => {
                self.set_active_scene(&name).map(CommandOutput::SceneList)
            }
            Command::AddSource(request) => self.add_source(&request).map(CommandOutput::SceneList),
            Command::UpdateSource(request) => {
                self.update_source(&request).map(CommandOutput::SceneList)
            }
            Command::RemoveSource { scene, name } => self
                .remove_source(&scene, &name)
                .map(CommandOutput::SceneList),
            Command::RestartSource { scene, name } => self
                .restart_source(&scene, &name)
                .map(CommandOutput::SceneList),
            Command::GetMixer => Ok(CommandOutput::MixerState(self.mixer_state())),
            Command::SetMixerStrip(change) => {
                self.set_mixer_strip(change).map(CommandOutput::MixerState)
            }
            Command::SetMasterGain { gain_db } => {
                Ok(CommandOutput::MixerState(self.set_master_gain(gain_db)))
            }
            Command::SetAudioSources(setup) => {
                self.set_audio_sources(setup).map(CommandOutput::MixerState)
            }
            Command::SetPreview { enabled } => self
                .set_preview(enabled)
                .map(|state| CommandOutput::PreviewState(Box::new(state))),
        }
    }
}

/// Audio before `set_audio_sources`: one quiet test tone (as outputs use
/// when started without sources).
fn default_audio() -> AudioSetup {
    let defaults = command_api::default_sources();
    AudioSetup {
        inputs: defaults.inputs,
        tones: defaults.tones,
        master_gain_db: defaults.master_gain_db,
    }
}

pub(crate) fn command_error(error: CaptureError) -> CommandError {
    let code = match error.kind {
        CaptureErrorKind::Unsupported => ErrorCode::Unsupported,
        CaptureErrorKind::Unavailable => ErrorCode::Unavailable,
        CaptureErrorKind::Cancelled => ErrorCode::Cancelled,
        CaptureErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        CaptureErrorKind::NotFound => ErrorCode::NotFound,
        CaptureErrorKind::Timeout => ErrorCode::Timeout,
        CaptureErrorKind::InvalidRequest => ErrorCode::InvalidParams,
        CaptureErrorKind::Backend => ErrorCode::BackendError,
    };
    CommandError {
        code,
        message: error.message,
        details: error.details,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::time::Duration;

    use std::sync::Arc;

    use std::sync::atomic::{AtomicBool, Ordering};

    use capture_api::{
        AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioInput,
        AudioStatsAccumulator, AudioStream, AudioStreamRequest, ScreenCaptureCapability,
        ScreenCaptureRequest, ScreenSelection, VideoCaptureReport, VideoSourceInfo,
        VideoStatsAccumulator, VideoStream,
    };
    use command_api::registry;
    use media_core::{AudioRing, FrameBuffer, FrameMailbox, PixelFormat, VideoFrame};
    use serde_json::json;

    use super::*;

    /// Backend with a working screen capability and a failing audio side.
    struct FakeCapture {
        capability_queries: Rc<Cell<usize>>,
        screen_starts: Rc<Cell<usize>>,
        /// One end switch per started screen stream, in start order.
        screen_ends: Rc<RefCell<Vec<Arc<capture_api::CaptureEnd>>>>,
    }

    impl CaptureBackend for FakeCapture {
        fn capabilities(&mut self) -> CaptureCapabilities {
            self.capability_queries
                .set(self.capability_queries.get() + 1);
            CaptureCapabilities {
                platform: "test".into(),
                desktop: Some("TestDesktop".into()),
                session_type: Some("wayland".into()),
                screen: ScreenCaptureCapability {
                    backend: Some("fake".into()),
                    available: true,
                    unavailable_reason: None,
                    portal_version: Some(5),
                    source_kinds: vec![capture_api::VideoSourceKind::Monitor],
                    cursor_modes: vec![capture_api::CursorMode::Embedded],
                    picker: true,
                    source_list: false,
                },
                audio: AudioCaptureCapability::unavailable(Some("fake"), "no server"),
            }
        }

        fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError> {
            Ok(vec![AudioInput {
                name: "mic".into(),
                description: Some("Microphone".into()),
                media_class: "Audio/Source".into(),
            }])
        }

        fn probe_screen(
            &mut self,
            request: &ScreenCaptureRequest,
        ) -> Result<VideoCaptureReport, CaptureError> {
            Ok(VideoCaptureReport {
                backend: "fake".into(),
                source: VideoSourceInfo {
                    kind: request.selection.source_kinds.first().copied(),
                    width: Some(1920),
                    height: Some(1080),
                },
                format: None,
                buffer_memory: None,
                stats: VideoStatsAccumulator::new().finish(request.duration),
                warnings: Vec::new(),
            })
        }

        fn probe_audio(
            &mut self,
            request: &AudioCaptureRequest,
        ) -> Result<AudioCaptureReport, CaptureError> {
            match request.input.as_deref() {
                Some("mic") | None => Ok(AudioCaptureReport {
                    backend: "fake".into(),
                    input: request.input.clone(),
                    format: None,
                    stats: AudioStatsAccumulator::new().finish(request.duration),
                    warnings: Vec::new(),
                }),
                Some(other) => Err(CaptureError::new(
                    CaptureErrorKind::NotFound,
                    format!("audio input `{other}` not found"),
                )
                .with_details(json!({"available": ["mic"]}))),
            }
        }

        fn start_audio_stream(
            &mut self,
            request: &AudioStreamRequest,
        ) -> Result<Box<dyn AudioStream>, CaptureError> {
            if request.input.as_deref() == Some("missing") {
                return Err(CaptureError::new(
                    CaptureErrorKind::NotFound,
                    "no such input",
                ));
            }
            let ring = Arc::new(AudioRing::new(request.channels, request.rate as usize));
            let stop = Arc::new(AtomicBool::new(false));
            let thread = std::thread::spawn({
                let (ring, stop) = (Arc::clone(&ring), Arc::clone(&stop));
                move || {
                    while !stop.load(Ordering::Acquire) {
                        ring.push(std::iter::repeat_n(0.25, 480 * 2));
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            });
            Ok(Box::new(FakeAudioStream {
                ring,
                stop,
                thread: Some(thread),
            }))
        }

        fn start_screen_stream(
            &mut self,
            _selection: &ScreenSelection,
        ) -> Result<Box<dyn VideoStream>, CaptureError> {
            self.screen_starts.set(self.screen_starts.get() + 1);
            let mailbox = Arc::new(FrameMailbox::new());
            // One solid red 32x18 frame.
            let pixels = [0u8, 0, 255, 0].repeat(32 * 18);
            mailbox.publish(
                VideoFrame::new(
                    32,
                    18,
                    32 * 4,
                    PixelFormat::Bgrx8,
                    None,
                    FrameBuffer::unpooled(pixels),
                )
                .expect("frame"),
            );
            let ended = Arc::new(capture_api::CaptureEnd::new());
            self.screen_ends.borrow_mut().push(Arc::clone(&ended));
            Ok(Box::new(FakeStream {
                mailbox,
                ended,
                source: VideoSourceInfo {
                    kind: Some(capture_api::VideoSourceKind::Monitor),
                    width: Some(32),
                    height: Some(18),
                },
            }))
        }
    }

    struct FakeStream {
        mailbox: Arc<FrameMailbox>,
        /// Set by a test to simulate the system ending the capture.
        ended: Arc<capture_api::CaptureEnd>,
        source: VideoSourceInfo,
    }

    /// Pushes 10 ms of constant 0.25 every 10 ms until stopped.
    struct FakeAudioStream {
        ring: Arc<AudioRing>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl AudioStream for FakeAudioStream {
        fn samples(&self) -> Arc<AudioRing> {
            Arc::clone(&self.ring)
        }

        fn state(&self) -> capture_api::CaptureState {
            capture_api::CaptureState::Running
        }

        fn stop(mut self: Box<Self>) -> Result<AudioCaptureReport, CaptureError> {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("producer");
            }
            Ok(AudioCaptureReport {
                backend: "fake".into(),
                input: Some("mic".into()),
                format: None,
                stats: AudioStatsAccumulator::new().finish(Duration::from_millis(1)),
                warnings: Vec::new(),
            })
        }
    }

    impl VideoStream for FakeStream {
        fn frames(&self) -> Arc<FrameMailbox> {
            Arc::clone(&self.mailbox)
        }

        fn source(&self) -> &VideoSourceInfo {
            &self.source
        }

        fn state(&self) -> capture_api::CaptureState {
            self.ended.state()
        }

        fn stop(self: Box<Self>) -> Result<VideoCaptureReport, CaptureError> {
            Ok(VideoCaptureReport {
                backend: "fake".into(),
                source: self.source.clone(),
                format: None,
                buffer_memory: None,
                stats: VideoStatsAccumulator::new().finish(Duration::from_millis(1)),
                warnings: Vec::new(),
            })
        }
    }

    fn engine_with_counter() -> (Engine, Rc<Cell<usize>>) {
        let queries = Rc::new(Cell::new(0));
        let engine = Engine::with_capture(Box::new(FakeCapture {
            capability_queries: Rc::clone(&queries),
            screen_starts: Rc::new(Cell::new(0)),
            screen_ends: Rc::default(),
        }));
        (engine, queries)
    }

    /// Engine with the fake backend, counting screen captures started.
    fn engine_with_screen_counter() -> (Engine, Rc<Cell<usize>>) {
        let (engine, starts, _) = engine_with_screen_streams();
        (engine, starts)
    }

    /// Like [`engine_with_screen_counter`], plus a switch per started screen
    /// stream that makes it report `ended`.
    #[allow(clippy::type_complexity)]
    fn engine_with_screen_streams() -> (
        Engine,
        Rc<Cell<usize>>,
        Rc<RefCell<Vec<Arc<capture_api::CaptureEnd>>>>,
    ) {
        let starts = Rc::new(Cell::new(0));
        let ends = Rc::new(RefCell::new(Vec::new()));
        let engine = Engine::with_capture(Box::new(FakeCapture {
            capability_queries: Rc::new(Cell::new(0)),
            screen_starts: Rc::clone(&starts),
            screen_ends: Rc::clone(&ends),
        }));
        (engine, starts, ends)
    }

    fn engine() -> Engine {
        engine_with_counter().0
    }

    #[test]
    fn get_status_reports_idle_scaffold() {
        let output = Engine::new().handle(Command::GetStatus).expect("status");
        let CommandOutput::Status(status) = output else {
            panic!("unexpected output: {output:?}");
        };
        assert_eq!(status.application, "StreamForge");
        assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
        assert!(status.media_engine_ready);
        assert!(!status.pipeline_running);
        assert!(!status.capture_available);
        assert!(!status.recording_active);
        assert!(!status.streaming_active);
    }

    #[test]
    fn status_reflects_capture_capabilities() {
        let output = engine().handle(Command::GetStatus).expect("status");
        let CommandOutput::Status(status) = output else {
            panic!("unexpected output: {output:?}");
        };
        assert!(status.capture_available);
    }

    #[test]
    fn list_commands_returns_full_registry() {
        let output = Engine::new()
            .handle(Command::ListCommands)
            .expect("commands");
        assert_eq!(
            output,
            CommandOutput::Commands(CommandList {
                commands: registry().to_vec()
            })
        );
    }

    #[test]
    fn every_registered_command_is_handled_without_panicking() {
        let dir = TempDir::new("all-commands");
        for (index, engine) in [Engine::new(), engine()].into_iter().enumerate() {
            let mut engine = engine.with_output_dir(&dir.0);
            for descriptor in registry() {
                let params = match descriptor.name {
                    // Keep probes short; their defaults run for seconds.
                    name if name.starts_with("probe_") => Some(json!({"duration_ms": 100})),
                    "start_recording" => Some(json!({"file_name": format!("all-{index}.mkv")})),
                    "create_scene" | "set_active_scene" => Some(json!({"name": "Scene"})),
                    "delete_scene" => Some(json!({"name": "Scene", "confirm": true})),
                    "add_source" => {
                        Some(json!({"scene": "Scene", "name": "bars", "kind": "color_bars"}))
                    }
                    "update_source" => {
                        Some(json!({"scene": "Scene", "name": "bars", "opacity": 0.5}))
                    }
                    "remove_source" | "restart_source" => {
                        Some(json!({"scene": "Scene", "name": "bars"}))
                    }
                    "set_mixer_strip" => Some(json!({"index": 0, "mute": true})),
                    "set_master_gain" => Some(json!({"gain_db": -3.0})),
                    "set_preview" => Some(json!({"enabled": false})),
                    // Nothing listens there: a structured `unavailable` error.
                    "start_streaming" => {
                        Some(json!({"url": format!("rtmp://127.0.0.1:{}/live", free_port())}))
                    }
                    _ => None,
                };
                let command = Command::decode(descriptor.name, params.as_ref()).expect("decode");
                // Unsupported capture is a structured error, not a panic.
                let _ = engine.handle(command);
            }
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free port")
            .port()
    }

    /// Tests that run the media pipeline (1080p60 compositor, x264
    /// encoders) take turns: in parallel they overload small CI runners
    /// until encoders fall seconds behind and drop media (counted, but the
    /// tests assert none is lost).
    fn media_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static MEDIA: std::sync::Mutex<()> = std::sync::Mutex::new(());
        MEDIA
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Temporary directory removed when the test ends.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("streamforge-engine-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn status(engine: &mut Engine) -> EngineStatus {
        match engine.handle(Command::GetStatus).expect("status") {
            CommandOutput::Status(status) => status,
            other => panic!("unexpected output: {other:?}"),
        }
    }

    fn health(engine: &mut Engine) -> command_api::HealthReport {
        match engine.handle(Command::GetHealth).expect("health") {
            CommandOutput::Health(health) => *health,
            other => panic!("unexpected output: {other:?}"),
        }
    }

    #[test]
    fn health_is_empty_while_idle() {
        let health = health(&mut Engine::new());
        assert!(!health.pipeline_running);
        assert!(health.render.is_none() && health.mix.is_none());
        assert!(health.recording.is_none() && health.streaming.is_none());
    }

    fn start(
        engine: &mut Engine,
        params: serde_json::Value,
    ) -> Result<CommandOutput, CommandError> {
        engine.handle(Command::decode("start_recording", Some(&params)).expect("decode"))
    }

    #[test]
    fn recording_lifecycle_produces_a_finalized_file() {
        let _media = media_test_lock();
        let dir = TempDir::new("lifecycle");
        let mut engine = engine().with_output_dir(&dir.0);
        assert!(!status(&mut engine).recording_active);

        let started = start(
            &mut engine,
            json!({"file_name": "take.mkv", "inputs": [{"input": "mic"}]}),
        )
        .expect("start");
        let CommandOutput::RecordingStarted(started) = started else {
            panic!("unexpected output");
        };
        assert!(started.path.ends_with("take.mkv"));
        assert_eq!(
            (started.width, started.height, started.fps),
            (1920, 1080, 60)
        );
        assert!(status(&mut engine).recording_active);

        let again = start(&mut engine, json!({"file_name": "other.mkv"})).unwrap_err();
        assert_eq!(again.code, ErrorCode::InvalidState);
        assert!(!dir.0.join("other.mkv").exists());
        // Probes would compete for devices and block stop_recording.
        let probe =
            Command::decode("probe_composite", Some(&json!({"duration_ms": 100}))).expect("decode");
        assert_eq!(
            engine.handle(probe).unwrap_err().code,
            ErrorCode::InvalidState
        );

        std::thread::sleep(Duration::from_millis(700));
        let report = match engine.handle(Command::StopRecording).expect("stop") {
            CommandOutput::RecordingReport(report) => *report,
            other => panic!("unexpected output: {other:?}"),
        };
        let recording = &report.recording;
        assert!(recording.video.frames_encoded > 10, "{recording:?}");
        assert!(recording.audio.samples_encoded > 10_000, "{recording:?}");
        assert!(recording.file_bytes.is_some_and(|bytes| bytes > 1_000));
        // The last output stopped the pipeline and reports it.
        assert!(report.pipeline.pipeline_stopped);
        assert_eq!(report.pipeline.audio_captures.len(), 1);
        assert!(report.pipeline.render.is_some() && report.pipeline.mix.is_some());
        // Both tracks end at the stop time, within a frame or a block. On a
        // slow machine, counted trailing drops and skipped compositor ticks
        // end the video earlier; it never outlasts the audio.
        let video_end = recording.video.end_ms.expect("video");
        let audio_end = recording.audio.end_ms.expect("audio");
        let skipped = report
            .pipeline
            .render
            .as_ref()
            .map_or(0, |render| render.skipped_ticks);
        let allowed_short =
            30.0 + (recording.video.frames_dropped + skipped) as f64 * 1000.0 / 60.0;
        assert!(
            audio_end - video_end < allowed_short && video_end - audio_end < 30.0,
            "video ends at {video_end} ms, audio at {audio_end} ms \
             ({} dropped, {skipped} skipped)",
            recording.video.frames_dropped
        );
        assert!(!status(&mut engine).recording_active);

        let stop_again = engine.handle(Command::StopRecording).unwrap_err();
        assert_eq!(stop_again.code, ErrorCode::InvalidState);
    }

    #[test]
    fn existing_files_are_never_overwritten() {
        let dir = TempDir::new("no-overwrite");
        std::fs::write(dir.0.join("keep.mkv"), b"precious").expect("write");
        let mut engine = engine().with_output_dir(&dir.0);
        let err = start(&mut engine, json!({"file_name": "keep.mkv"})).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert_eq!(
            std::fs::read(dir.0.join("keep.mkv")).expect("read"),
            b"precious"
        );
        assert!(!status(&mut engine).recording_active);
    }

    /// Start streaming to `url`, retrying while the test server is not
    /// listening yet.
    fn start_streaming(engine: &mut Engine, params: serde_json::Value) -> CommandOutput {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let command = Command::decode("start_streaming", Some(&params)).expect("decode");
            match engine.handle(command) {
                Ok(output) => return output,
                Err(err)
                    if err.code == ErrorCode::Unavailable
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) => panic!("start_streaming: {err}"),
            }
        }
    }

    #[test]
    fn streams_the_pipeline_to_a_local_server() {
        let _media = media_test_lock();
        let server = rtmp_test_server::TestServer::start(Default::default());
        let mut engine = engine();
        let started = start_streaming(
            &mut engine,
            json!({"url": server.url(), "stream_key": "k-SECRET", "video_bitrate_kbps": 2000}),
        );
        let CommandOutput::StreamingStarted(started) = started else {
            panic!("unexpected output");
        };
        assert!(
            started.target.ends_with("/live/<stream key>"),
            "{}",
            started.target
        );
        let connected = status(&mut engine);
        assert!(connected.streaming_active);
        assert_eq!(connected.streaming_state, Some(StreamState::Connected));
        std::thread::sleep(Duration::from_millis(1500));
        let report = match engine.handle(Command::StopStreaming).expect("stop") {
            CommandOutput::StreamingReport(report) => *report,
            other => panic!("unexpected output: {other:?}"),
        };
        assert_eq!(report.streaming.state, StreamState::Connected, "{report:?}");
        assert_eq!(report.streaming.connects, 1);
        assert!(report.pipeline.pipeline_stopped);
        assert!(
            !serde_json::to_string(&report)
                .expect("json")
                .contains("SECRET")
        );
        let stopped = status(&mut engine);
        assert!(!stopped.streaming_active && stopped.streaming_state.is_none());

        let arrived = server.join();
        assert_eq!((arrived.width, arrived.height), (1920, 1080));
        let video = arrived.video.expect("video");
        assert_eq!(video.codec, "h264");
        assert_eq!(arrived.audio.expect("audio").codec, "aac");
        assert!(video.end_s - video.first_s > 1.0, "{video:?}");
    }

    #[test]
    fn recording_continues_when_the_stream_fails() {
        let _media = media_test_lock();
        let dir = TempDir::new("survive");
        let server = rtmp_test_server::TestServer::start(rtmp_test_server::ServerOptions {
            close_after: Some(Duration::from_millis(800)),
            ..Default::default()
        });
        let mut engine = engine().with_output_dir(&dir.0);
        start(&mut engine, json!({"file_name": "keep.mkv"})).expect("start recording");
        // Outputs share the running pipeline; they cannot bring their own sources.
        let own_sources = Command::decode(
            "start_streaming",
            Some(&json!({"url": server.url(), "tones": []})),
        )
        .expect("decode");
        assert_eq!(
            engine.handle(own_sources).unwrap_err().code,
            ErrorCode::InvalidState
        );
        start_streaming(
            &mut engine,
            json!({"url": server.url(), "reconnect_attempts": 1, "reconnect_delay_ms": 100,
                   "video_bitrate_kbps": 2000}),
        );

        // The ingest drops the connection and never comes back.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while status(&mut engine).streaming_state != Some(StreamState::Failed) {
            assert!(std::time::Instant::now() < deadline, "stream never failed");
            std::thread::sleep(Duration::from_millis(50));
        }
        let status_now = status(&mut engine);
        assert!(!status_now.streaming_active);
        assert!(status_now.recording_active, "{status_now:?}");
        assert!(status_now.pipeline_running);

        // Health tells the two outputs apart while they run: the stream
        // failed after its attempts, the recording keeps encoding.
        let first = health(&mut engine);
        let stream_health = first.streaming.as_ref().expect("stream health");
        assert_eq!(stream_health.state, Some(StreamState::Failed));
        assert!(!stream_health.running);
        assert_eq!(stream_health.counters.connects, 1);
        assert_eq!(stream_health.counters.reconnect_attempts, 1);
        assert!(
            stream_health.target.ends_with("/live"),
            "{}",
            stream_health.target
        );
        let before = first.recording.as_ref().expect("recording health").counters;
        std::thread::sleep(Duration::from_millis(400));
        let later = health(&mut engine);
        let recording_health = later.recording.as_ref().expect("recording health");
        assert!(recording_health.running);
        assert!(recording_health.target.ends_with("keep.mkv"));
        assert!(
            recording_health.counters.frames_encoded > before.frames_encoded,
            "{before:?} -> {:?}",
            recording_health.counters
        );
        let stream_later = later.streaming.as_ref().expect("stream health");
        assert!(
            stream_later.counters.frames_dropped > stream_health.counters.frames_dropped,
            "a failed stream counts what it drops"
        );
        let render = later.render.as_ref().expect("render");
        assert!(render.frames_rendered > 0 && !render.sources.is_empty());
        assert!(later.mix.as_ref().expect("mix").blocks_mixed > 0);

        let stream = match engine.handle(Command::StopStreaming).expect("stop stream") {
            CommandOutput::StreamingReport(report) => *report,
            other => panic!("unexpected output: {other:?}"),
        };
        assert_eq!(stream.streaming.state, StreamState::Failed);
        assert_eq!(stream.streaming.disconnects, 1);
        // The recording still uses the pipeline.
        assert!(!stream.pipeline.pipeline_stopped);
        assert!(
            stream.warnings.iter().any(|w| w.contains("failed")),
            "{:?}",
            stream.warnings
        );
        assert!(status(&mut engine).recording_active);

        std::thread::sleep(Duration::from_millis(300));
        let report = match engine
            .handle(Command::StopRecording)
            .expect("stop recording")
        {
            CommandOutput::RecordingReport(report) => *report,
            other => panic!("unexpected output: {other:?}"),
        };
        assert!(report.pipeline.pipeline_stopped);
        let recording = &report.recording;
        assert_eq!(recording.audio.blocks_dropped, 0, "{recording:?}");
        assert_eq!(recording.audio.gap_samples, 0, "{recording:?}");
        // No silent loss: every tick from the first frame to the cut is
        // encoded, dropped or skipped (skips cover the whole pipeline run,
        // so they can exceed the output's share), and no frame is invented.
        let first = recording.video.first_pts_ms.expect("video start");
        let end = recording.audio.end_ms.expect("audio");
        let ticks = ((end - first) * 60.0 / 1000.0).round() as u64;
        let skipped = report
            .pipeline
            .render
            .as_ref()
            .map_or(0, |r| r.skipped_ticks);
        let accounted = recording.video.frames_encoded + recording.video.frames_dropped + skipped;
        assert!(
            accounted + 2 >= ticks,
            "{accounted} vs {ticks}: {recording:?}"
        );
        assert!(
            recording.video.frames_encoded + recording.video.frames_dropped <= ticks + 1,
            "{recording:?}"
        );
        let _ = server.join();
    }

    fn run(engine: &mut Engine, name: &str, params: serde_json::Value) -> CommandOutput {
        let command = Command::decode(name, Some(&params)).expect("decode");
        engine
            .handle(command)
            .unwrap_or_else(|err| panic!("{name}: {err}"))
    }

    /// Wait for a preview frame whose pixel at (x, y) is `bgra`.
    fn wait_for_preview_pixel(engine: &Engine, x: u32, y: u32, bgra: [u8; 4]) {
        let frames = engine.preview_frames();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut last = None;
        while std::time::Instant::now() < deadline {
            if let Some(frame) = frames.take() {
                assert_eq!((frame.width(), frame.height()), (480, 270));
                last = Some(frame.pixel(x, y));
                if last == Some(bgra) {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("preview pixel ({x}, {y}) is {last:?}, expected {bgra:?}");
    }

    #[test]
    fn scene_edits_apply_live_to_the_preview() {
        let _media = media_test_lock();
        let mut engine = Engine::new();
        run(&mut engine, "create_scene", json!({"name": "Show"}));
        run(
            &mut engine,
            "add_source",
            json!({"scene": "Show", "name": "red", "kind": "solid", "color": "#ff0000"}),
        );
        let CommandOutput::PreviewState(preview) =
            run(&mut engine, "set_preview", json!({"enabled": true}))
        else {
            panic!("unexpected output");
        };
        assert!(preview.enabled && preview.fps == 30);
        assert!(status(&mut engine).preview_active);
        // The default scene starts with color bars: white at the top left.
        wait_for_preview_pixel(&engine, 10, 10, [255, 255, 255, 255]);
        run(&mut engine, "set_active_scene", json!({"name": "Show"}));
        wait_for_preview_pixel(&engine, 10, 10, [0, 0, 255, 255]);
        // A blue box over the top left quarter (canvas pixels).
        run(
            &mut engine,
            "add_source",
            json!({"scene": "Show", "name": "blue", "kind": "solid", "color": "#0000ff",
                   "x": 0, "y": 0, "width": 960, "height": 540}),
        );
        wait_for_preview_pixel(&engine, 10, 10, [255, 0, 0, 255]);
        wait_for_preview_pixel(&engine, 400, 200, [0, 0, 255, 255]);
        run(
            &mut engine,
            "update_source",
            json!({"scene": "Show", "name": "blue", "opacity": 0.0}),
        );
        wait_for_preview_pixel(&engine, 10, 10, [0, 0, 255, 255]);
        // Probes would compete with the running pipeline.
        let probe =
            Command::decode("probe_composite", Some(&json!({"duration_ms": 100}))).expect("decode");
        let err = engine.handle(probe).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidState);
        assert!(err.message.contains("preview"), "{err}");
        run(&mut engine, "set_preview", json!({"enabled": false}));
        assert!(!status(&mut engine).preview_active);
        assert!(engine.preview_frames().take().is_none());
    }

    #[test]
    fn a_live_scene_change_that_fails_is_undone() {
        let _media = media_test_lock();
        // No capture backend: a screen source cannot start.
        let mut engine = Engine::new();
        run(&mut engine, "set_preview", json!({"enabled": true}));
        let before = match run(&mut engine, "list_scenes", json!({})) {
            CommandOutput::SceneList(list) => list,
            other => panic!("unexpected output: {other:?}"),
        };
        let add = Command::decode(
            "add_source",
            Some(&json!({"scene": "Test pattern", "name": "desk", "kind": "screen"})),
        )
        .expect("decode");
        assert_eq!(engine.handle(add).unwrap_err().code, ErrorCode::Unsupported);
        let after = match run(&mut engine, "list_scenes", json!({})) {
            CommandOutput::SceneList(list) => list,
            other => panic!("unexpected output: {other:?}"),
        };
        assert_eq!(before, after);
        // Deleting needs confirmation, and the active scene cannot go.
        let delete = Command::decode(
            "delete_scene",
            Some(&json!({"name": "Test pattern", "confirm": true})),
        )
        .expect("decode");
        assert_eq!(
            engine.handle(delete).unwrap_err().code,
            ErrorCode::InvalidState
        );
        run(&mut engine, "set_preview", json!({"enabled": false}));
    }

    /// Poll `get_mixer` until `done` holds (bounded).
    fn wait_mixer(
        engine: &mut Engine,
        done: impl Fn(&command_api::MixerState) -> bool,
    ) -> command_api::MixerState {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = mixer(run(engine, "get_mixer", json!({})));
            if done(&state) || std::time::Instant::now() >= deadline {
                return state;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn mixer(output: CommandOutput) -> command_api::MixerState {
        match output {
            CommandOutput::MixerState(state) => state,
            other => panic!("unexpected output: {other:?}"),
        }
    }

    #[test]
    fn mixer_gains_apply_live_and_levels_are_reported() {
        let _media = media_test_lock();
        let mut engine = Engine::new();
        let idle = mixer(run(&mut engine, "get_mixer", json!({})));
        assert!(!idle.running);
        assert_eq!(idle.strips.len(), 1);
        assert_eq!(idle.strips[0].kind, "tone");
        let muted = mixer(run(
            &mut engine,
            "set_mixer_strip",
            json!({"index": 0, "mute": true}),
        ));
        assert!(muted.strips[0].muted);
        let missing = Command::decode("set_mixer_strip", Some(&json!({"index": 3, "mute": true})))
            .expect("decode");
        assert_eq!(
            engine.handle(missing).unwrap_err().code,
            ErrorCode::NotFound
        );

        run(&mut engine, "set_preview", json!({"enabled": true}));
        let running = wait_mixer(&mut engine, |state| state.running);
        assert!(running.strips[0].muted);
        assert_eq!(running.strips[0].peak_dbfs, None, "{running:?}");
        run(
            &mut engine,
            "set_mixer_strip",
            json!({"index": 0, "mute": false}),
        );
        // The default tone peaks at -20 dBFS.
        // Past the unmute ramp, the window holds full-level blocks.
        let live = wait_mixer(&mut engine, |state| {
            state.strips[0].peak_dbfs.is_some_and(|peak| peak > -20.1)
        });
        let peak = live.strips[0].peak_dbfs.expect("tone level");
        assert!((peak + 20.0).abs() < 0.2, "{live:?}");
        // Reading levels is read-only: a second reader sees them too.
        assert!(
            mixer(run(&mut engine, "get_mixer", json!({}))).strips[0]
                .peak_dbfs
                .is_some()
        );
        let gain = mixer(run(
            &mut engine,
            "set_master_gain",
            json!({"gain_db": -6.0}),
        ));
        assert_eq!(gain.master_gain_db, -6.0);
        // Sources are fixed while running; gains are not.
        let setup =
            Command::decode("set_audio_sources", Some(&json!({"tones": []}))).expect("decode");
        assert_eq!(
            engine.handle(setup).unwrap_err().code,
            ErrorCode::InvalidState
        );
        run(&mut engine, "set_preview", json!({"enabled": false}));
        // The live changes were kept in the configuration.
        let after = mixer(run(&mut engine, "get_mixer", json!({})));
        assert!(!after.running && !after.strips[0].muted);
        assert_eq!(after.master_gain_db, -6.0);
        let silent = mixer(run(&mut engine, "set_audio_sources", json!({"tones": []})));
        assert!(silent.strips.is_empty());
    }

    #[test]
    fn screen_captures_survive_scene_switches() {
        let _media = media_test_lock();
        let (mut engine, starts) = engine_with_screen_counter();
        for scene in ["Main", "BRB"] {
            run(&mut engine, "create_scene", json!({"name": scene}));
            run(
                &mut engine,
                "add_source",
                json!({"scene": scene, "name": "desk", "kind": "screen"}),
            );
        }
        run(&mut engine, "set_active_scene", json!({"name": "Main"}));
        run(&mut engine, "set_preview", json!({"enabled": true}));
        assert_eq!(starts.get(), 1);
        // Each scene's source starts once; switching back reuses it.
        run(&mut engine, "set_active_scene", json!({"name": "BRB"}));
        run(&mut engine, "set_active_scene", json!({"name": "Main"}));
        run(&mut engine, "set_active_scene", json!({"name": "BRB"}));
        assert_eq!(starts.get(), 2);
        // Removing a source from every scene ends its capture.
        run(
            &mut engine,
            "remove_source",
            json!({"scene": "Main", "name": "desk"}),
        );
        let CommandOutput::PreviewState(preview) =
            run(&mut engine, "set_preview", json!({"enabled": false}))
        else {
            panic!("unexpected output");
        };
        let pipeline = preview.pipeline.expect("pipeline report");
        // One capture retired when `Main`'s source went, one at the stop.
        assert_eq!(pipeline.scene_captures.len(), 2, "{pipeline:?}");
        assert!(preview.frames_published.is_some());
    }

    #[test]
    fn an_ended_screen_source_shows_in_health() {
        let _media = media_test_lock();
        let dir = TempDir::new("capture-state");
        let (engine, _starts, ends) = engine_with_screen_streams();
        let mut engine = engine.with_output_dir(&dir.0);
        run(
            &mut engine,
            "add_source",
            json!({"scene": "Test pattern", "name": "desk", "kind": "screen",
                   "source_id": "fake:1"}),
        );
        start(&mut engine, json!({"file_name": "ended.mkv"})).expect("start");
        let desk = |health: &command_api::HealthReport| {
            health
                .captures
                .iter()
                .find(|capture| capture.name == "desk")
                .cloned()
                .expect("desk capture")
        };
        let running = desk(&health(&mut engine));
        assert_eq!(running.kind, command_api::CaptureKind::Screen);
        assert!(running.id.is_some());
        assert_eq!(running.state, capture_api::CaptureState::Running);

        // The system ends the capture: health says so, with the reason, and
        // the recording keeps going.
        ends.borrow()[0].end(|| "the captured window was closed".to_owned());
        let ended = health(&mut engine);
        assert_eq!(
            desk(&ended).state,
            capture_api::CaptureState::Ended {
                reason: "the captured window was closed".to_owned()
            }
        );
        assert!(ended.recording.expect("recording").running);
        engine.handle(Command::StopRecording).expect("stop");
        assert!(health(&mut engine).captures.is_empty());
    }

    #[test]
    fn an_ended_screen_source_restarts_while_recording() {
        let _media = media_test_lock();
        let dir = TempDir::new("restart");
        let (engine, starts, ends) = engine_with_screen_streams();
        let mut engine = engine.with_output_dir(&dir.0);
        let restart = |engine: &mut Engine, scene: &str, name: &str| {
            let command = Command::decode(
                "restart_source",
                Some(&json!({"scene": scene, "name": name})),
            )
            .expect("decode");
            engine.handle(command)
        };
        run(
            &mut engine,
            "add_source",
            json!({"scene": "Test pattern", "name": "desk", "kind": "screen",
                   "source_id": "fake:1"}),
        );
        // Nothing captures yet.
        assert_eq!(
            restart(&mut engine, "Test pattern", "desk")
                .unwrap_err()
                .code,
            ErrorCode::InvalidState
        );
        start(&mut engine, json!({"file_name": "restart.mkv"})).expect("start");
        assert_eq!(starts.get(), 1);
        let desk = |health: &command_api::HealthReport| {
            health
                .captures
                .iter()
                .find(|capture| capture.name == "desk")
                .cloned()
                .expect("desk capture")
        };
        ends.borrow()[0].end(|| "the captured window was closed".to_owned());
        assert!(matches!(
            desk(&health(&mut engine)).state,
            capture_api::CaptureState::Ended { .. }
        ));

        // Restarting starts a new capture while the recording keeps going.
        let before = health(&mut engine)
            .recording
            .expect("recording")
            .counters
            .frames_encoded;
        restart(&mut engine, "Test pattern", "desk").expect("restart");
        assert_eq!(starts.get(), 2);
        assert_eq!(
            desk(&health(&mut engine)).state,
            capture_api::CaptureState::Running
        );
        std::thread::sleep(Duration::from_millis(300));
        let after = health(&mut engine);
        let recording = after.recording.expect("recording");
        assert!(recording.running);
        assert!(recording.counters.frames_encoded > before, "{recording:?}");

        // Only screen sources of the active scene restart.
        assert_eq!(
            restart(&mut engine, "Test pattern", "color_bars")
                .unwrap_err()
                .code,
            ErrorCode::InvalidParams
        );
        run(&mut engine, "create_scene", json!({"name": "Other"}));
        run(
            &mut engine,
            "add_source",
            json!({"scene": "Other", "name": "cam", "kind": "screen", "source_id": "fake:2"}),
        );
        assert_eq!(
            restart(&mut engine, "Other", "cam").unwrap_err().code,
            ErrorCode::InvalidState
        );
        assert_eq!(
            restart(&mut engine, "Test pattern", "nope")
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );

        let report = match engine.handle(Command::StopRecording).expect("stop") {
            CommandOutput::RecordingReport(report) => *report,
            other => panic!("unexpected output: {other:?}"),
        };
        // The replaced capture and the final one are both reported.
        let desks = report
            .pipeline
            .scene_captures
            .iter()
            .filter(|outcome| outcome.source == "desk")
            .count();
        assert_eq!(desks, 2, "{:?}", report.pipeline.scene_captures);
        // The compositor took the new capture's frame.
        let render = report.pipeline.render.expect("render stats");
        assert_eq!(render.retired_sources, 1, "{render:?}");
        let desk = render
            .sources
            .iter()
            .find(|source| source.name == "desk")
            .expect("desk");
        assert_eq!(desk.frames_consumed, 1, "{desk:?}");
    }

    #[test]
    fn a_picker_source_restarts_only_while_no_output_is_open() {
        let _media = media_test_lock();
        let dir = TempDir::new("restart-picker");
        let (engine, starts, _ends) = engine_with_screen_streams();
        let mut engine = engine.with_output_dir(&dir.0);
        let restart = |engine: &mut Engine| {
            let command = Command::decode(
                "restart_source",
                Some(&json!({"scene": "Test pattern", "name": "desk"})),
            )
            .expect("decode");
            engine.handle(command)
        };
        run(&mut engine, "set_preview", json!({"enabled": true}));
        run(
            &mut engine,
            "add_source",
            json!({"scene": "Test pattern", "name": "desk", "kind": "screen"}),
        );
        assert_eq!(starts.get(), 1);
        start(&mut engine, json!({"file_name": "picker.mkv"})).expect("start");
        let err = restart(&mut engine).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidState);
        assert!(err.message.contains("picker"), "{err}");
        assert_eq!(starts.get(), 1);
        // The refused restart kept the running capture.
        let captures = health(&mut engine).captures;
        assert!(
            captures.iter().any(|capture| capture.name == "desk"
                && capture.state == capture_api::CaptureState::Running),
            "{captures:?}"
        );
        engine.handle(Command::StopRecording).expect("stop");
        // Only the preview runs: the picker may ask again.
        restart(&mut engine).expect("restart");
        assert_eq!(starts.get(), 2);
        run(&mut engine, "set_preview", json!({"enabled": false}));
    }

    #[test]
    fn open_outputs_never_wait_for_a_picker() {
        let _media = media_test_lock();
        let dir = TempDir::new("no-picker");
        let (engine, starts) = engine_with_screen_counter();
        let mut engine = engine.with_output_dir(&dir.0);
        start(&mut engine, json!({"file_name": "live.mkv"})).expect("start");
        let add = Command::decode(
            "add_source",
            Some(&json!({"scene": "Test pattern", "name": "desk", "kind": "screen"})),
        )
        .expect("decode");
        let err = engine.handle(add).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidState);
        assert!(err.message.contains("picker"), "{err}");
        assert_eq!(starts.get(), 0);
        // The refused edit was undone.
        let CommandOutput::SceneList(list) = run(&mut engine, "list_scenes", json!({})) else {
            panic!("unexpected output");
        };
        assert_eq!(list.scenes[0].sources.len(), 2);
        // Edits that need no capture still apply live.
        run(
            &mut engine,
            "update_source",
            json!({"scene": "Test pattern", "name": "overlay", "opacity": 0.5}),
        );
        engine.handle(Command::StopRecording).expect("stop");
    }

    #[test]
    fn a_recording_shows_the_active_scene() {
        let _media = media_test_lock();
        let dir = TempDir::new("scene");
        let mut engine = Engine::new().with_output_dir(&dir.0);
        run(&mut engine, "create_scene", json!({"name": "Green"}));
        run(
            &mut engine,
            "add_source",
            json!({"scene": "Green", "name": "fill", "kind": "solid", "color": "#00ff00"}),
        );
        run(&mut engine, "set_active_scene", json!({"name": "Green"}));
        start(&mut engine, json!({"file_name": "green.mkv"})).expect("start");
        std::thread::sleep(Duration::from_millis(400));
        let report = match engine.handle(Command::StopRecording).expect("stop") {
            CommandOutput::RecordingReport(report) => *report,
            other => panic!("unexpected output: {other:?}"),
        };
        assert!(report.recording.video.frames_encoded > 5);
        let render = report.pipeline.render.expect("render stats");
        let names: Vec<_> = render.sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["fill"]);
    }

    #[test]
    fn missing_output_directory_is_not_found() {
        let dir = TempDir::new("missing");
        let mut engine = engine().with_output_dir(dir.0.join("absent"));
        let err = start(&mut engine, json!({"file_name": "a.mkv"})).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound, "{err}");
    }

    #[test]
    fn dropping_the_engine_finalizes_the_recording() {
        let _media = media_test_lock();
        let dir = TempDir::new("drop");
        let mut engine = engine().with_output_dir(&dir.0);
        start(&mut engine, json!({"file_name": "dropped.mkv"})).expect("start");
        std::thread::sleep(Duration::from_millis(300));
        drop(engine);
        let bytes = std::fs::metadata(dir.0.join("dropped.mkv"))
            .expect("file")
            .len();
        assert!(bytes > 1_000, "{bytes}");
    }

    #[test]
    fn capabilities_are_requeried_on_request() {
        let (mut engine, queries) = engine_with_counter();
        engine.handle(Command::GetStatus).expect("status");
        engine.handle(Command::GetStatus).expect("status");
        assert_eq!(queries.get(), 1, "get_status must reuse the cached query");
        let output = engine
            .handle(Command::GetCaptureCapabilities)
            .expect("capabilities");
        let CommandOutput::CaptureCapabilities(caps) = output else {
            panic!("unexpected output: {output:?}");
        };
        assert_eq!(caps.desktop.as_deref(), Some("TestDesktop"));
        assert_eq!(caps.audio.unavailable_reason.as_deref(), Some("no server"));
        assert_eq!(
            queries.get(),
            2,
            "get_capture_capabilities must query again"
        );
    }

    #[test]
    fn missing_backend_maps_to_unsupported() {
        let err = Engine::new().handle(Command::ListAudioInputs).unwrap_err();
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert!(
            err.message.contains("no capture backend"),
            "{}",
            err.message
        );
    }

    #[test]
    fn capture_errors_keep_code_and_details() {
        let err = engine()
            .handle(Command::ProbeAudioCapture(AudioCaptureRequest {
                input: Some("nope".into()),
                duration: Duration::from_millis(100),
            }))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert_eq!(err.details, Some(json!({"available": ["mic"]})));
    }

    #[test]
    fn capture_error_kinds_map_to_distinct_codes() {
        let kinds = [
            (CaptureErrorKind::Unsupported, ErrorCode::Unsupported),
            (CaptureErrorKind::Unavailable, ErrorCode::Unavailable),
            (CaptureErrorKind::Cancelled, ErrorCode::Cancelled),
            (
                CaptureErrorKind::PermissionDenied,
                ErrorCode::PermissionDenied,
            ),
            (CaptureErrorKind::NotFound, ErrorCode::NotFound),
            (CaptureErrorKind::Timeout, ErrorCode::Timeout),
            (CaptureErrorKind::InvalidRequest, ErrorCode::InvalidParams),
            (CaptureErrorKind::Backend, ErrorCode::BackendError),
        ];
        for (kind, code) in kinds {
            assert_eq!(command_error(CaptureError::new(kind, "x")).code, code);
        }
    }

    #[test]
    fn probes_return_reports() {
        let mut engine = engine();
        let output = engine
            .handle(
                Command::decode("probe_screen_capture", Some(&json!({"duration_ms": 200})))
                    .expect("decode"),
            )
            .expect("probe");
        let CommandOutput::VideoCaptureReport(report) = output else {
            panic!("unexpected output: {output:?}");
        };
        assert_eq!(report.stats.elapsed_ms, 200);
        let output = engine.handle(Command::ListAudioInputs).expect("inputs");
        assert_eq!(
            serde_json::to_value(output).expect("json")["inputs"][0]["name"],
            "mic"
        );
    }

    fn composite(engine: &mut Engine, params: serde_json::Value) -> command_api::CompositeReport {
        let command = Command::decode("probe_composite", Some(&params)).expect("decode");
        match engine.handle(command).expect("composite") {
            CommandOutput::CompositeReport(report) => *report,
            other => panic!("unexpected output: {other:?}"),
        }
    }

    #[test]
    fn composite_probe_renders_the_test_scene() {
        let report = composite(&mut engine(), json!({"duration_ms": 200}));
        assert!(report.render.frames_rendered > 0, "{:?}", report.render);
        assert_eq!(
            (
                report.render.width,
                report.render.height,
                report.render.target_fps
            ),
            (1920, 1080, 60)
        );
        assert!(report.screen_capture.is_none());
        let names: Vec<_> = report
            .render
            .sources
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["color_bars", "overlay"]);
    }

    #[test]
    fn composite_probe_draws_the_live_screen_layer() {
        let report = composite(&mut engine(), json!({"duration_ms": 200, "screen": true}));
        let screen = report
            .render
            .sources
            .iter()
            .find(|s| s.name == "screen")
            .expect("screen source");
        assert_eq!(screen.kind, "live");
        assert_eq!(screen.frames_consumed, 1);
        assert_eq!(report.screen_capture.expect("report").backend, "fake");
    }

    #[test]
    fn composite_snapshot_is_written_once_and_never_overwritten() {
        let dir =
            std::env::temp_dir().join(format!("streamforge-engine-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut engine = engine().with_output_dir(&dir);
        let params = json!({"duration_ms": 100, "snapshot_path": "snapshot.ppm"});

        let first = composite(&mut engine, params.clone())
            .snapshot
            .expect("snapshot");
        assert!(first.written, "{first:?}");
        assert!(first.path.ends_with("snapshot.ppm"), "{}", first.path);
        let path = dir.join("snapshot.ppm");
        let bytes = std::fs::read(&path).expect("read snapshot");
        let header = b"P6\n1920 1080\n255\n";
        assert!(bytes.starts_with(header));
        assert_eq!(bytes.len(), header.len() + 1920 * 1080 * 3);

        let second = composite(&mut engine, params).snapshot.expect("snapshot");
        assert!(!second.written);
        assert!(second.error.is_some());
        assert_eq!(
            std::fs::read(&path).expect("still there").len(),
            bytes.len()
        );

        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    fn mix(engine: &mut Engine, params: serde_json::Value) -> command_api::MixReport {
        let command = Command::decode("probe_audio_mix", Some(&params)).expect("decode");
        match engine.handle(command).expect("mix") {
            CommandOutput::MixReport(report) => *report,
            other => panic!("unexpected output: {other:?}"),
        }
    }

    #[test]
    fn mix_probe_mixes_a_live_input_and_a_tone() {
        let report = mix(
            &mut engine(),
            json!({
                "duration_ms": 300,
                "inputs": [{"input": "mic", "gain_db": -6.0}],
                "tones": [{"frequency_hz": 1000.0, "level_dbfs": -12.0}],
            }),
        );
        let stats = &report.mix;
        assert!(stats.blocks_mixed >= 10, "{stats:?}");
        assert_eq!(stats.inputs.len(), 2);
        assert_eq!(stats.inputs[0].kind, "live");
        assert_eq!(stats.inputs[1].kind, "tone");
        let tone_peak = stats.inputs[1].meter.peak_dbfs.expect("tone peak");
        assert!((tone_peak + 12.0).abs() < 0.01, "{tone_peak}");
        assert!(stats.master.rms_dbfs.is_some());
        assert_eq!(report.captures.len(), 1);
        assert!(report.captures[0].report.is_some());
    }

    #[test]
    fn mix_probe_with_unknown_input_fails_before_mixing() {
        let command = Command::decode(
            "probe_audio_mix",
            Some(&json!({"inputs": [{"input": "missing"}], "duration_ms": 100})),
        )
        .expect("decode");
        let err = engine().handle(command).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }
}
