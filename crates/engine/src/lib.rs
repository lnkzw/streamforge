//! Engine controller: owns engine state and executes typed commands.
//!
//! Independent of UI and transport. The CLI, the JSON-lines server, the
//! desktop UI and the MCP server all drive the engine through
//! [`CommandHandler`]. Platform capture is injected as a [`Capture`]
//! by the composition root.

mod audio;
mod composite;
mod ids;
mod images;
mod mix;
mod outputs;
mod pipeline;
mod project;
mod refs;
mod scenes;
mod studio;
mod thread;

use outputs::{RecordingOutput, StreamingOutput};
use pipeline::Pipeline;
use scenes::SceneCollection;
pub use thread::{EngineHandle, EngineThread, PendingReply};

use std::path::PathBuf;
use std::sync::Arc;

use capture_api::{Capture, CaptureCapabilities};
use command_api::{
    AudioGraphSpec, AudioInputList, AudioOutputList, CameraList, CaptureTargetList, Command,
    CommandError, CommandHandler, CommandList, CommandOutput, EngineStatus, ErrorCode,
    OutputProfiles,
};
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
    /// The image sources' frames, by file name.
    images: images::ImageStore,
    /// The configured audio graph; every run mixes it (ADR 0017).
    audio: AudioGraphSpec,
    /// How each output starts: its mix bus and encoder settings.
    profiles: OutputProfiles,
    /// The preview keeps the pipeline running.
    preview_enabled: bool,
    /// Where preview frames go (see `Engine::preview_frames`).
    preview_frames: Arc<FrameMailbox>,
    status: EngineStatus,
    capture: Capture,
    /// Where commands may create files (snapshots); clients only name files.
    output_dir: PathBuf,
    /// Where project files live (`--project-dir`); without one, the project
    /// commands are unavailable.
    project_dir: Option<PathBuf>,
    /// Last capability query; `get_status` derives `capture_available` from it.
    capture_capabilities: Option<CaptureCapabilities>,
}

impl Engine {
    /// Engine without a capture backend; capture commands fail as unsupported.
    pub fn new() -> Self {
        Self::with_capture(Capture::unsupported(
            "no capture backend is configured for this engine instance",
        ))
    }

    pub fn with_capture(capture: Capture) -> Self {
        Self {
            status: EngineStatus {
                application: "StreamForge".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                pipeline_running: false,
                capture_available: false,
                recording_state: None,
                streaming_state: None,
                preview_active: false,
            },
            capture,
            output_dir: PathBuf::from("."),
            project_dir: None,
            capture_capabilities: None,
            scenes: SceneCollection::new(),
            images: images::ImageStore::default(),
            audio: audio::default_graph(),
            profiles: OutputProfiles::default(),
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

    /// Directory for project files and the images image sources show;
    /// clients only name files in it. Without one, the project commands,
    /// `list_images` and image sources are `invalid_state`.
    pub fn with_project_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.project_dir = Some(dir.into());
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
                self.status.recording_state = self.recording_state();
                self.status.streaming_state = self.streaming_state();
                self.status.preview_active = self.preview_enabled;
                self.status.pipeline_running = self.pipeline_running();
                Ok(CommandOutput::Status(self.status.clone()))
            }
            Command::GetHealth => Ok(CommandOutput::Health(Box::new(self.health()))),
            Command::ListCommands => Ok(CommandOutput::Commands(CommandList::all())),
            Command::GetCaptureCapabilities => Ok(CommandOutput::CaptureCapabilities(
                self.refresh_capabilities().clone(),
            )),
            Command::ListAudioInputs => self
                .capture
                .audio
                .audio_inputs()
                .map(|inputs| CommandOutput::AudioInputs(AudioInputList { inputs }))
                .map_err(CommandError::from),
            Command::ListAudioOutputs => self
                .capture
                .audio
                .audio_outputs()
                .map(|outputs| CommandOutput::AudioOutputs(AudioOutputList { outputs }))
                .map_err(CommandError::from),
            Command::ListCameras => self
                .capture
                .camera
                .cameras()
                .map(|cameras| CommandOutput::Cameras(CameraList { cameras }))
                .map_err(CommandError::from),
            Command::ListCaptureTargets => self
                .capture
                .screen
                .screen_sources()
                .map(|sources| {
                    CommandOutput::CaptureTargets(CaptureTargetList { targets: sources })
                })
                .map_err(CommandError::from),
            Command::ProbeScreenCapture(request) => {
                self.ensure_idle("probe_screen_capture")?;
                self.capture
                    .screen
                    .probe_screen(&request)
                    .map(CommandOutput::VideoCaptureReport)
                    .map_err(CommandError::from)
            }
            Command::ProbeAudioCapture(request) => {
                self.ensure_idle("probe_audio_capture")?;
                self.capture
                    .audio
                    .probe_audio(&request)
                    .map(CommandOutput::AudioCaptureReport)
                    .map_err(CommandError::from)
            }
            Command::ProbeComposite(request) => {
                self.ensure_idle("probe_composite")?;
                composite::probe(self.capture.screen.as_mut(), &self.output_dir, &request)
                    .map(|report| CommandOutput::CompositeReport(Box::new(report)))
            }
            Command::ProbeAudioMix(request) => {
                self.ensure_idle("probe_audio_mix")?;
                mix::probe(self.capture.audio.as_mut(), &request)
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
            Command::DeleteScene(scene) => self.delete_scene(&scene).map(CommandOutput::SceneList),
            Command::SetActiveScene(scene) => {
                self.set_active_scene(&scene).map(CommandOutput::SceneList)
            }
            Command::AddSource(request) => self.add_source(&request).map(CommandOutput::SceneList),
            Command::UpdateSource(request) => {
                self.update_source(&request).map(CommandOutput::SceneList)
            }
            Command::MoveSource(request) => {
                self.move_source(&request).map(CommandOutput::SceneList)
            }
            Command::RemoveSource(source) => {
                self.remove_source(&source).map(CommandOutput::SceneList)
            }
            Command::RestartSource {
                source,
                capture_target_id,
            } => self
                .restart_source(&source, capture_target_id.as_deref())
                .map(CommandOutput::SceneList),
            Command::GetMixer => Ok(CommandOutput::MixerState(self.mixer_state())),
            Command::AddChannel(request) => {
                self.add_channel(request).map(CommandOutput::MixerState)
            }
            Command::RemoveChannel(channel) => {
                self.remove_channel(&channel).map(CommandOutput::MixerState)
            }
            Command::UpdateChannel(request) => {
                self.update_channel(request).map(CommandOutput::MixerState)
            }
            Command::SetChannelInput(request) => self
                .set_channel_input(request)
                .map(CommandOutput::MixerState),
            Command::AddBus(request) => self.add_bus(request).map(CommandOutput::MixerState),
            Command::RemoveBus(bus) => self.remove_bus(&bus).map(CommandOutput::MixerState),
            Command::UpdateBus(request) => self.update_bus(request).map(CommandOutput::MixerState),
            Command::SetSend(request) => self.set_send(request).map(CommandOutput::MixerState),
            Command::GetOutputProfiles => Ok(CommandOutput::OutputProfiles(self.profiles.clone())),
            Command::SetOutputProfile(request) => self
                .set_output_profile(request)
                .map(CommandOutput::OutputProfileSet),
            Command::SetMonitorDevice { device } => self
                .set_monitor_device(device)
                .map(CommandOutput::OutputProfiles),
            Command::SetPreview { enabled } => self
                .set_preview(enabled)
                .map(|state| CommandOutput::PreviewState(Box::new(state))),
            Command::SetCaptureTarget {
                source,
                capture_target_id,
            } => self
                .set_capture_target(&source, &capture_target_id)
                .map(CommandOutput::SceneList),
            Command::ListProjects => self.list_projects().map(CommandOutput::ProjectFiles),
            Command::ListImages => self.list_images().map(CommandOutput::ImageFiles),
            Command::SaveProject(request) => {
                self.save_project(&request).map(CommandOutput::ProjectSaved)
            }
            Command::LoadProject { file_name } => self
                .load_project(&file_name)
                .map(|loaded| CommandOutput::ProjectLoaded(Box::new(loaded))),
            Command::ValidateProject { file_name } => self
                .validate_project(&file_name)
                .map(CommandOutput::ProjectValidation),
        }
    }
}

/// The code for a file the engine could not create in its output
/// directory: an existing file is the caller's choice (`invalid_params`),
/// a missing or unwritable directory is the system's.
pub(crate) fn file_error_code(error: &std::io::Error) -> ErrorCode {
    match error.kind() {
        std::io::ErrorKind::AlreadyExists => ErrorCode::InvalidParams,
        std::io::ErrorKind::NotFound => ErrorCode::NotFound,
        std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        _ => ErrorCode::BackendError,
    }
}

#[cfg(test)]
mod tests {
    mod images;
    mod monitor;
    use std::time::{Duration, Instant};

    use std::sync::{Arc, Mutex};

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use capture_api::{
        AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioDevices, AudioInput,
        AudioStatsAccumulator, AudioStream, AudioStreamRequest, CaptureError, CaptureErrorKind,
        ScreenCapture, ScreenCaptureCapability, ScreenCaptureRequest, ScreenSelection,
        VideoCaptureReport, VideoSourceInfo, VideoStatsAccumulator, VideoStream,
    };
    use command_api::{RecordingState, registry};
    use encoder_ffmpeg::StreamState;
    use media_core::{AudioRing, FrameBuffer, FrameMailbox, PixelFormat, Rational, VideoFrame};
    use serde_json::json;

    use super::*;

    /// Screen part with a working capability and observable stream lifecycle.
    struct FakeScreen {
        capability_queries: Arc<AtomicUsize>,
        screen_starts: Arc<AtomicUsize>,
        /// One end switch per started screen stream, in start order.
        screen_ends: Arc<Mutex<Vec<Arc<capture_api::CaptureEnd>>>>,
    }

    /// Audio part with an unavailable capability, but working fake devices.
    struct FakeAudio {
        /// What the fake speakers played, interleaved stereo.
        played: Arc<Mutex<Vec<f32>>>,
    }

    impl ScreenCapture for FakeScreen {
        fn capability(&mut self) -> ScreenCaptureCapability {
            self.capability_queries.fetch_add(1, Ordering::Relaxed);
            ScreenCaptureCapability {
                backend: Some("fake".into()),
                available: true,
                unavailable_reason: None,
                portal_version: Some(5),
                target_kinds: vec![capture_api::VideoSourceKind::Monitor],
                cursor_modes: vec![capture_api::CursorMode::Embedded],
                picker: true,
                target_list: false,
            }
        }

        fn screen_sources(&mut self) -> Result<Vec<capture_api::ScreenSource>, CaptureError> {
            Err(CaptureError::unsupported(
                "this capture backend cannot list screen sources",
            ))
        }

        fn probe_screen(
            &mut self,
            request: &ScreenCaptureRequest,
        ) -> Result<VideoCaptureReport, CaptureError> {
            Ok(VideoCaptureReport {
                backend: "fake".into(),
                source: VideoSourceInfo {
                    kind: request.selection.target_kinds.first().copied(),
                    width: Some(1920),
                    height: Some(1080),
                },
                format: None,
                buffer_memory: None,
                stats: VideoStatsAccumulator::new().finish(request.duration),
                warnings: Vec::new(),
            })
        }

        fn start_screen_stream(
            &mut self,
            selection: &ScreenSelection,
        ) -> Result<Box<dyn VideoStream>, CaptureError> {
            // A source that went away, e.g. a closed window.
            if selection.capture_target_id.as_deref() == Some("fake:gone") {
                return Err(CaptureError::new(
                    capture_api::CaptureErrorKind::NotFound,
                    "no such source",
                ));
            }
            self.screen_starts.fetch_add(1, Ordering::Relaxed);
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
            self.screen_ends
                .lock()
                .expect("screen ends")
                .push(Arc::clone(&ended));
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

    impl AudioDevices for FakeAudio {
        fn capability(&mut self) -> AudioCaptureCapability {
            AudioCaptureCapability::unavailable(Some("fake"), "no server")
        }

        fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError> {
            Ok(vec![
                AudioInput {
                    name: "mic".into(),
                    description: Some("Microphone".into()),
                    media_class: "Audio/Source".into(),
                    loopback_of: None,
                },
                AudioInput {
                    name: "speakers.monitor".into(),
                    description: Some("Monitor of Speakers".into()),
                    media_class: "Audio/Source".into(),
                    loopback_of: Some("speakers".into()),
                },
            ])
        }

        fn audio_outputs(&mut self) -> Result<Vec<capture_api::AudioOutput>, CaptureError> {
            Ok(vec![capture_api::AudioOutput {
                name: "speakers".into(),
                description: Some("Speakers".into()),
                media_class: "Audio/Sink".into(),
            }])
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
            if request.input.as_deref() == Some("denied") {
                return Err(CaptureError::new(
                    CaptureErrorKind::PermissionDenied,
                    "microphone access is not permitted",
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

        fn start_playback(
            &mut self,
            request: &capture_api::PlaybackRequest,
            feed: media_core::SampleReader,
        ) -> Result<Box<dyn capture_api::PlaybackStream>, CaptureError> {
            monitor::start_fake_playback(request, feed, &self.played)
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

    fn engine_with_counter() -> (Engine, Arc<AtomicUsize>) {
        let queries = Arc::new(AtomicUsize::new(0));
        let engine = Engine::with_capture(Capture {
            screen: Box::new(FakeScreen {
                capability_queries: Arc::clone(&queries),
                screen_starts: Arc::new(AtomicUsize::new(0)),
                screen_ends: Arc::default(),
            }),
            audio: Box::new(FakeAudio {
                played: Arc::default(),
            }),
            ..Capture::unsupported("no fake camera")
        });
        (engine, queries)
    }

    /// Engine with the fake backend, counting screen captures started.
    fn engine_with_screen_counter() -> (Engine, Arc<AtomicUsize>) {
        let (engine, starts, _) = engine_with_screen_streams();
        (engine, starts)
    }

    /// Like [`engine_with_screen_counter`], plus a switch per started screen
    /// stream that makes it report `ended`.
    #[allow(clippy::type_complexity)]
    fn engine_with_screen_streams() -> (
        Engine,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<Arc<capture_api::CaptureEnd>>>>,
    ) {
        let starts = Arc::new(AtomicUsize::new(0));
        let ends = Arc::new(Mutex::new(Vec::new()));
        let engine = Engine::with_capture(Capture {
            screen: Box::new(FakeScreen {
                capability_queries: Arc::new(AtomicUsize::new(0)),
                screen_starts: Arc::clone(&starts),
                screen_ends: Arc::clone(&ends),
            }),
            audio: Box::new(FakeAudio {
                played: Arc::default(),
            }),
            ..Capture::unsupported("no fake camera")
        });
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
        assert!(!status.pipeline_running);
        assert!(!status.capture_available);
        assert_eq!(status.recording_state, None);
        assert_eq!(status.streaming_state, None);
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
            let mut engine = engine.with_output_dir(&dir.0).with_project_dir(&dir.0);
            for descriptor in registry() {
                let params = match descriptor.name {
                    // Keep probes short; their defaults run for seconds.
                    name if name.starts_with("probe_") => Some(json!({"duration_ms": 100})),
                    "start_recording" => Some(json!({"file_name": format!("all-{index}.mkv")})),
                    "create_scene" => Some(json!({"name": "Scene"})),
                    "set_active_scene" => Some(json!({"scene_name": "Scene"})),
                    "delete_scene" => Some(json!({"scene_name": "Scene", "confirm": true})),
                    "add_source" => {
                        Some(json!({"scene_name": "Scene", "name": "bars", "kind": "color_bars"}))
                    }
                    "update_source" => {
                        Some(json!({"scene_name": "Scene", "source_name": "bars", "opacity": 0.5}))
                    }
                    "move_source" => {
                        Some(json!({"scene_name": "Scene", "source_name": "bars", "index": 0}))
                    }
                    "remove_source" | "restart_source" => {
                        Some(json!({"scene_name": "Scene", "source_name": "bars"}))
                    }
                    "add_channel" => Some(json!({"name": "Extra",
                        "input": {"kind": "tone", "frequency_hz": 440}})),
                    "remove_channel" => Some(json!({"channel_name": "Extra"})),
                    "update_channel" => Some(json!({"channel_name": "Tone", "mute": true})),
                    "set_channel_input" => Some(json!({"channel_name": "Tone",
                        "input": {"kind": "tone", "frequency_hz": 500}})),
                    "add_bus" => Some(json!({"name": "Voice", "assign_db": 0})),
                    "remove_bus" => Some(json!({"bus_name": "Voice"})),
                    "update_bus" => Some(json!({"bus_id": "bus_program", "gain_db": -3.0})),
                    "set_send" => Some(
                        json!({"channel_name": "Tone", "bus_name": "Program", "enabled": false}),
                    ),
                    "set_output_profile" => {
                        Some(json!({"output": "stream", "video_bitrate_kbps": 4000}))
                    }
                    "set_preview" => Some(json!({"enabled": false})),
                    "set_capture_target" => Some(
                        json!({"scene_name": "Scene", "source_name": "bars", "capture_target_id": "fake:1"}),
                    ),
                    "save_project" => Some(json!({"file_name": format!("all-{index}.json")})),
                    "load_project" | "validate_project" => {
                        Some(json!({"file_name": format!("all-{index}.json")}))
                    }
                    // Nothing listens there: a structured `unavailable` error.
                    "start_streaming" => Some(json!({
                        "url": format!("rtmp://127.0.0.1:{}/live", rtmp_test_server::free_port())
                    })),
                    _ => None,
                };
                let command = Command::decode(descriptor.name, params.as_ref()).expect("decode");
                // Unsupported capture is a structured error, not a panic.
                let _ = engine.handle(command);
            }
        }
    }

    /// Tests that run the media pipeline (1080p60 compositor, x264
    /// encoders) take turns: in parallel they overload small CI runners
    /// until encoders fall seconds behind and drop frames.
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
        try_run(engine, "start_recording", params)
    }

    #[test]
    fn recording_lifecycle_produces_a_finalized_file() {
        let _media = media_test_lock();
        let dir = TempDir::new("lifecycle");
        let mut engine = engine().with_output_dir(&dir.0);
        assert_eq!(status(&mut engine).recording_state, None);

        run(
            &mut engine,
            "set_channel_input",
            json!({"channel_name": "Tone", "input": {"kind": "device", "device": "mic"}}),
        );
        let started = start(&mut engine, json!({"file_name": "take.mkv"})).expect("start");
        let CommandOutput::RecordingStarted(started) = started else {
            panic!("unexpected output");
        };
        assert!(started.path.ends_with("take.mkv"));
        assert_eq!(
            (started.width, started.height, started.fps),
            (1920, 1080, Rational::integer(60))
        );
        assert_eq!(
            status(&mut engine).recording_state,
            Some(RecordingState::Running)
        );

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
        assert!(
            recording.audio_tracks[0].samples_encoded > 10_000,
            "{recording:?}"
        );
        assert!(recording.file_bytes.is_some_and(|bytes| bytes > 1_000));
        // The last output stopped the pipeline and reports it.
        assert!(report.pipeline.stopped);
        assert_eq!(report.pipeline.audio_captures.len(), 1);
        assert!(report.pipeline.render.is_some() && report.pipeline.mix.is_some());
        // Both tracks end at the stop time, within a frame or a block. On a
        // slow machine, counted trailing drops and skipped compositor ticks
        // end the video earlier; it never outlasts the audio.
        let video_end = recording.video.end_ms.expect("video");
        let audio_end = recording.audio_tracks[0].end_ms.expect("audio");
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
        assert_eq!(status(&mut engine).recording_state, None);

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
        assert_eq!(status(&mut engine).recording_state, None);
    }

    /// Start streaming to `url`, retrying while the test server is not
    /// listening yet.
    /// The test server listens once `TestServer::start` returns, so one
    /// attempt is enough, and its error is the one that matters.
    fn start_streaming(engine: &mut Engine, params: serde_json::Value) -> CommandOutput {
        let command = Command::decode("start_streaming", Some(&params)).expect("decode");
        engine
            .handle(command)
            .unwrap_or_else(|err| panic!("start_streaming: {err}"))
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
        assert_eq!(connected.streaming_state, Some(StreamState::Connected));
        std::thread::sleep(Duration::from_millis(1500));
        let report = match engine.handle(Command::StopStreaming).expect("stop") {
            CommandOutput::StreamingReport(report) => *report,
            other => panic!("unexpected output: {other:?}"),
        };
        assert_eq!(report.streaming.state, StreamState::Connected, "{report:?}");
        assert_eq!(report.streaming.connects, 1);
        assert!(report.pipeline.stopped);
        assert!(
            !serde_json::to_string(&report)
                .expect("json")
                .contains("SECRET")
        );
        let stopped = status(&mut engine);
        assert!(stopped.streaming_state.is_none());

        let arrived = server.join();
        assert_eq!((arrived.width, arrived.height), (1920, 1080));
        let video = arrived.video.expect("video");
        assert_eq!(video.codec, "h264");
        assert_eq!(arrived.audio.expect("audio").codec, "aac");
        // The stream ran for the 1.5 s sleep at least. On a slow runner the
        // encoder drops frames it cannot keep up with, and frames still in
        // the encoder when the stop flush runs out of time are never
        // delivered; trailing ones of either end the video early, and both
        // are counted. (Skipped compositor ticks in the middle leave gaps
        // instead.)
        // Half a second of video must arrive, whatever was dropped.
        let short = report.streaming.video.frames_dropped + report.streaming.frames_undelivered;
        let fps =
            f64::from(report.streaming.video.fps.num) / f64::from(report.streaming.video.fps.den);
        assert!(
            video.end_s - video.first_s + short as f64 / fps > 1.0
                && video.packets as f64 >= fps / 2.0,
            "{video:?}: {short} frames dropped or undelivered"
        );
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
        assert_eq!(status_now.streaming_state, Some(StreamState::Failed));
        assert_eq!(
            status_now.recording_state,
            Some(RecordingState::Running),
            "{status_now:?}"
        );
        assert!(status_now.pipeline_running);

        // Health tells the two outputs apart while they run: the stream
        // failed after its attempts, the recording keeps encoding.
        let first = health(&mut engine);
        let stream_health = first.streaming.as_ref().expect("stream health");
        assert_eq!(stream_health.state, StreamState::Failed);
        assert!(!stream_health.running);
        let connection = stream_health
            .connection
            .expect("stream connection counters");
        assert_eq!(connection.connects, 1);
        assert_eq!(connection.reconnect_attempts, 1);
        let recording_health = first.recording.as_ref().expect("recording health");
        assert_eq!(recording_health.state, RecordingState::Running);
        assert!(recording_health.running && recording_health.connection.is_none());
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
        assert!(!stream.pipeline.stopped);
        assert!(
            stream.warnings.iter().any(|w| w.contains("failed")),
            "{:?}",
            stream.warnings
        );
        assert_eq!(
            status(&mut engine).recording_state,
            Some(RecordingState::Running)
        );

        std::thread::sleep(Duration::from_millis(300));
        let report = match engine
            .handle(Command::StopRecording)
            .expect("stop recording")
        {
            CommandOutput::RecordingReport(report) => *report,
            other => panic!("unexpected output: {other:?}"),
        };
        assert!(report.pipeline.stopped);
        let recording = &report.recording;
        assert_eq!(recording.audio_tracks[0].blocks_dropped, 0, "{recording:?}");
        assert_eq!(recording.audio_tracks[0].gap_samples, 0, "{recording:?}");
        // No silent loss: every tick from the first frame to the cut is
        // encoded, dropped or skipped (skips cover the whole pipeline run,
        // so they can exceed the output's share), and no frame is invented.
        let first = recording.video.first_pts_ms.expect("video start");
        let end = recording.audio_tracks[0].end_ms.expect("audio");
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
        try_run(engine, name, params).unwrap_or_else(|err| panic!("{name}: {err}"))
    }

    fn try_run(
        engine: &mut Engine,
        name: &str,
        params: serde_json::Value,
    ) -> Result<CommandOutput, CommandError> {
        engine.handle(Command::decode(name, Some(&params)).expect("decode"))
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
            json!({"scene_name": "Show", "name": "red", "kind": "solid", "color": "#ff0000"}),
        );
        let CommandOutput::PreviewState(preview) =
            run(&mut engine, "set_preview", json!({"enabled": true}))
        else {
            panic!("unexpected output");
        };
        assert!(preview.enabled && preview.fps == Rational::integer(30));
        assert!(status(&mut engine).preview_active);
        // The default scene starts with color bars: white at the top left.
        wait_for_preview_pixel(&engine, 10, 10, [255, 255, 255, 255]);
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "Show"}),
        );
        wait_for_preview_pixel(&engine, 10, 10, [0, 0, 255, 255]);
        // A blue box over the top left quarter (canvas pixels).
        run(
            &mut engine,
            "add_source",
            json!({"scene_name": "Show", "name": "blue", "kind": "solid", "color": "#0000ff",
                   "x": 0, "y": 0, "width": 960, "height": 540}),
        );
        wait_for_preview_pixel(&engine, 10, 10, [255, 0, 0, 255]);
        wait_for_preview_pixel(&engine, 400, 200, [0, 0, 255, 255]);
        run(
            &mut engine,
            "update_source",
            json!({"scene_name": "Show", "source_name": "blue", "opacity": 0.0}),
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
    fn layer_order_and_visibility_apply_live_to_the_preview() {
        let _media = media_test_lock();
        let mut engine = Engine::new();
        run(&mut engine, "create_scene", json!({"name": "Show"}));
        for (name, color) in [("red", "#ff0000"), ("blue", "#0000ff")] {
            run(
                &mut engine,
                "add_source",
                json!({"scene_name": "Show", "name": name, "kind": "solid", "color": color}),
            );
        }
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "Show"}),
        );
        run(&mut engine, "set_preview", json!({"enabled": true}));
        // Blue was added last, so it is on top.
        wait_for_preview_pixel(&engine, 10, 10, [255, 0, 0, 255]);
        let CommandOutput::SceneList(list) = run(
            &mut engine,
            "move_source",
            json!({"scene_name": "Show", "source_name": "blue", "index": 0}),
        ) else {
            panic!("unexpected output");
        };
        let names: Vec<_> = list.scenes[1].sources.iter().map(|s| &s.name).collect();
        assert_eq!(names, ["blue", "red"]);
        wait_for_preview_pixel(&engine, 10, 10, [0, 0, 255, 255]);
        // Hiding the top layer shows the one under it.
        let CommandOutput::SceneList(list) = run(
            &mut engine,
            "update_source",
            json!({"scene_name": "Show", "source_name": "red", "visible": false}),
        ) else {
            panic!("unexpected output");
        };
        assert!(!list.scenes[1].sources[1].visible);
        wait_for_preview_pixel(&engine, 10, 10, [255, 0, 0, 255]);
        let past = try_run(
            &mut engine,
            "move_source",
            json!({"scene_name": "Show", "source_name": "red", "index": 2}),
        )
        .unwrap_err();
        assert_eq!(past.code, ErrorCode::InvalidParams);
        run(&mut engine, "set_preview", json!({"enabled": false}));
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
            Some(&json!({"scene_name": "Test pattern", "name": "desk", "kind": "screen"})),
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
            Some(&json!({"scene_name": "Test pattern", "confirm": true})),
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
        assert_eq!(idle.channels.len(), 1);
        assert_eq!(idle.channels[0].spec.id, audio::DEFAULT_CHANNEL_ID);
        assert_eq!(idle.channels[0].spec.input.kind(), "tone");
        assert_eq!(idle.buses[0].id, "bus_program");
        let muted = mixer(run(
            &mut engine,
            "update_channel",
            json!({"channel_id": audio::DEFAULT_CHANNEL_ID, "mute": true}),
        ));
        assert!(muted.channels[0].spec.muted);
        let missing = Command::decode(
            "update_channel",
            Some(&json!({"channel_name": "Mic", "mute": true})),
        )
        .expect("decode");
        assert_eq!(
            engine.handle(missing).unwrap_err().code,
            ErrorCode::NotFound
        );

        run(&mut engine, "set_preview", json!({"enabled": true}));
        let running = wait_mixer(&mut engine, |state| state.running);
        assert!(running.channels[0].spec.muted);
        assert_eq!(running.channels[0].peak_dbfs, None, "{running:?}");
        run(
            &mut engine,
            "update_channel",
            json!({"channel_name": "Tone", "mute": false}),
        );
        // The default tone peaks at -20 dBFS.
        // Past the unmute ramp, the window holds full-level blocks.
        let live = wait_mixer(&mut engine, |state| {
            state.channels[0].peak_dbfs.is_some_and(|peak| peak > -20.1)
        });
        let peak = live.channels[0].peak_dbfs.expect("tone level");
        assert!((peak + 20.0).abs() < 0.2, "{live:?}");
        // Reading levels is read-only: a second reader sees them too.
        assert!(
            mixer(run(&mut engine, "get_mixer", json!({}))).channels[0]
                .peak_dbfs
                .is_some()
        );
        let gain = mixer(run(
            &mut engine,
            "update_bus",
            json!({"bus_name": "Program", "gain_db": -6.0}),
        ));
        assert_eq!(gain.buses[0].gain_db, -6.0);
        // Hard left takes the right side away: the peak stays (left), and
        // the change is live.
        let panned = mixer(run(
            &mut engine,
            "update_channel",
            json!({"channel_name": "Tone", "balance": -1.0}),
        ));
        assert_eq!(panned.channels[0].spec.balance, -1.0);
        // Channels are fixed while running; faders are not.
        for (command, params) in [
            (
                "add_channel",
                json!({"name": "Mic", "input": {"kind": "device"}}),
            ),
            ("remove_channel", json!({"channel_name": "Tone"})),
            (
                "set_channel_input",
                json!({"channel_name": "Tone", "input": {"kind": "device"}}),
            ),
        ] {
            let structure = Command::decode(command, Some(&params)).expect("decode");
            assert_eq!(
                engine.handle(structure).unwrap_err().code,
                ErrorCode::InvalidState,
                "{command}"
            );
        }
        run(&mut engine, "set_preview", json!({"enabled": false}));
        // The live changes were kept in the configuration.
        let after = mixer(run(&mut engine, "get_mixer", json!({})));
        assert!(!after.running && !after.channels[0].spec.muted);
        assert_eq!(after.buses[0].gain_db, -6.0);
        assert_eq!(after.channels[0].spec.balance, -1.0);
        let added = mixer(run(
            &mut engine,
            "add_channel",
            json!({"name": "Music", "input": {"kind": "tone", "frequency_hz": 440},
                   "gain_db": -12.0}),
        ));
        assert_eq!(added.channels.len(), 2);
        assert!(added.channels[1].spec.id.starts_with("ch_"));
        let silent = mixer(run(
            &mut engine,
            "remove_channel",
            json!({"channel_name": "Tone"}),
        ));
        let silent = mixer(run(
            &mut engine,
            "remove_channel",
            json!({"channel_id": silent.channels[0].spec.id}),
        ));
        assert!(silent.channels.is_empty());
    }

    #[test]
    fn output_profiles_name_existing_buses_and_keep_them() {
        let mut engine = Engine::new();
        let CommandOutput::OutputProfiles(profiles) =
            run(&mut engine, "get_output_profiles", json!({}))
        else {
            panic!("unexpected output");
        };
        assert_eq!(profiles, command_api::OutputProfiles::default());
        // The handler keeps the bitrate to the stream, whoever builds the
        // request.
        let typed = Command::SetOutputProfile(command_api::SetOutputProfileRequest {
            output: command_api::OutputKind::Recording,
            mix: None,
            tracks: None,
            video_encoder: None,
            video_bitrate_kbps: Some(1_000),
            enabled: None,
        });
        assert_eq!(
            engine.handle(typed).unwrap_err().code,
            ErrorCode::InvalidParams
        );
        // And `enabled` and the encoder to the outputs they are for.
        for (output, encoder, enabled) in [
            (command_api::OutputKind::Stream, None, Some(true)),
            (
                command_api::OutputKind::Monitor,
                Some(command_api::EncoderPreference::Software),
                None,
            ),
        ] {
            let typed = Command::SetOutputProfile(command_api::SetOutputProfileRequest {
                output,
                mix: None,
                tracks: None,
                video_encoder: encoder,
                video_bitrate_kbps: None,
                enabled,
            });
            assert_eq!(
                engine.handle(typed).unwrap_err().code,
                ErrorCode::InvalidParams
            );
        }
        let unknown = Command::decode(
            "set_output_profile",
            Some(&json!({"output": "recording", "bus_name": "Voice"})),
        )
        .expect("decode");
        let error = engine.handle(unknown).unwrap_err();
        assert_eq!(error.code, ErrorCode::NotFound);
        assert_eq!(error.details, Some(json!({"bus_name": "Voice"})));
        run(&mut engine, "add_bus", json!({"name": "Voice"}));
        let CommandOutput::OutputProfileSet(set) = run(
            &mut engine,
            "set_output_profile",
            json!({"output": "recording", "bus_name": "Voice", "video_encoder": "hardware"}),
        ) else {
            panic!("unexpected output");
        };
        // Nothing runs: nothing applied now, nothing waits.
        assert!(set.applied_now.is_empty() && set.next_start.is_empty());
        let voice = set.profiles.recording.mix.clone();
        assert!(voice.starts_with("bus_") && voice != "bus_program");
        let state = mixer(run(&mut engine, "get_mixer", json!({})));
        assert_eq!(state.outputs[0].bus_id, voice);
        assert!(!state.outputs[0].running);
        // Tracks: existing buses, each once, not the mix again.
        for tracks in [
            json!([{"bus_name": "Voice"}]),
            json!([{"bus_name": "Program"}, {"bus_name": "Program"}]),
            json!([{"bus_name": "Gone"}]),
        ] {
            let command = Command::decode(
                "set_output_profile",
                Some(&json!({"output": "recording", "tracks": tracks})),
            )
            .expect("decode");
            let error = engine.handle(command).unwrap_err();
            assert!(
                matches!(error.code, ErrorCode::InvalidParams | ErrorCode::NotFound),
                "{tracks}: {error:?}"
            );
        }
        let CommandOutput::OutputProfileSet(set) = run(
            &mut engine,
            "set_output_profile",
            json!({"output": "recording", "tracks": [{"bus_name": "Program"}]}),
        ) else {
            panic!("unexpected output");
        };
        assert_eq!(set.profiles.recording.tracks, ["bus_program"]);
        // A refused request changes nothing, not even its valid parts.
        let before = run(&mut engine, "get_output_profiles", json!({}));
        let mixed = Command::decode(
            "set_output_profile",
            Some(&json!({"output": "recording", "bus_name": "Program", "tracks": [{"bus_name": "Gone"}]})),
        )
        .expect("decode");
        assert_eq!(engine.handle(mixed).unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(run(&mut engine, "get_output_profiles", json!({})), before);
        // A mix that is already a track is refused, as a file would be.
        let onto_track = Command::decode(
            "set_output_profile",
            Some(&json!({"output": "recording", "bus_name": "Program"})),
        )
        .expect("decode");
        let error = engine.handle(onto_track).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidParams);
        assert_eq!(error.details, Some(json!({"path": "recording.tracks[0]"})));
        assert_eq!(
            mixer(run(&mut engine, "get_mixer", json!({}))).outputs[0].tracks,
            ["bus_program"]
        );
        // The profile keeps its bus.
        let remove =
            Command::decode("remove_bus", Some(&json!({"bus_name": "Voice"}))).expect("decode");
        let error = engine.handle(remove).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidState);
        assert_eq!(error.details, Some(json!({"outputs": ["recording"]})));
        run(
            &mut engine,
            "set_output_profile",
            json!({"output": "recording", "bus_id": "bus_program", "tracks": []}),
        );
        run(&mut engine, "remove_bus", json!({"bus_name": "Voice"}));
    }

    #[test]
    fn aux_buses_and_sends_route_live_with_levels() {
        let _media = media_test_lock();
        let mut engine = Engine::new();
        // A separate monitor mix, fed before the fader; the fader only
        // lowers the program.
        run(
            &mut engine,
            "add_bus",
            json!({"name": "Monitor", "assign_db": null}),
        );
        run(
            &mut engine,
            "set_send",
            json!({"channel_name": "Tone", "bus_name": "Monitor", "enabled": true, "tap": "pre_fader"}),
        );
        run(
            &mut engine,
            "update_channel",
            json!({"channel_name": "Tone", "gain_db": -12.0}),
        );
        let near =
            |peak: Option<f64>, dbfs: f64| peak.is_some_and(|peak| (peak - dbfs).abs() < 0.3);
        run(&mut engine, "set_preview", json!({"enabled": true}));
        // The default tone peaks at -20 dBFS.
        let live = wait_mixer(&mut engine, |state| {
            state.running && near(state.buses[1].peak_dbfs, -20.0)
        });
        assert!(near(live.buses[1].peak_dbfs, -20.0), "{live:?}");
        assert!(near(live.buses[0].peak_dbfs, -32.0), "{live:?}");
        assert!(
            near(live.channels[0].peak_dbfs, -32.0),
            "post-fader: {live:?}"
        );
        assert!(live.warnings.is_empty(), "{live:?}");
        // Sends change live.
        run(
            &mut engine,
            "set_send",
            json!({"channel_name": "Tone", "bus_name": "Monitor", "level_db": -6.0}),
        );
        let lowered = wait_mixer(&mut engine, |state| near(state.buses[1].peak_dbfs, -26.0));
        assert!(near(lowered.buses[1].peak_dbfs, -26.0), "{lowered:?}");
        // Assigned to program, the monitor mix adds to it: the same sine at
        // -32 and -26 dBFS sums to about -22.5 dBFS, and the channel now
        // reaches program twice.
        let assigned = mixer(run(
            &mut engine,
            "update_bus",
            json!({"bus_name": "Monitor", "assign_db": 0.0}),
        ));
        assert_eq!(assigned.buses[1].assign_db, Some(0.0));
        assert_eq!(assigned.warnings.len(), 1, "{assigned:?}");
        let sum = 20.0 * (10f64.powf(-32.0 / 20.0) + 10f64.powf(-26.0 / 20.0)).log10();
        let summed = wait_mixer(&mut engine, |state| near(state.buses[0].peak_dbfs, sum));
        assert!(near(summed.buses[0].peak_dbfs, sum), "{sum} {summed:?}");
        // Buses are fixed while running.
        for (command, params) in [
            ("add_bus", json!({"name": "Voice"})),
            ("remove_bus", json!({"bus_name": "Monitor"})),
        ] {
            let structure = Command::decode(command, Some(&params)).expect("decode");
            assert_eq!(
                engine.handle(structure).unwrap_err().code,
                ErrorCode::InvalidState,
                "{command}"
            );
        }
        run(&mut engine, "set_preview", json!({"enabled": false}));
        // The live changes were kept; removing the bus removes its sends.
        let after = mixer(run(&mut engine, "get_mixer", json!({})));
        assert_eq!(after.buses[1].assign_db, Some(0.0));
        assert_eq!(after.channels[0].spec.sends[1].level_db, -6.0);
        let removed = mixer(run(
            &mut engine,
            "remove_bus",
            json!({"bus_name": "Monitor"}),
        ));
        assert_eq!(removed.buses.len(), 1);
        assert_eq!(removed.channels[0].spec.sends.len(), 1);
        assert!(removed.warnings.is_empty());
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
                json!({"scene_name": scene, "name": "desk", "kind": "screen"}),
            );
        }
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "Main"}),
        );
        run(&mut engine, "set_preview", json!({"enabled": true}));
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        // Each scene's source starts once; switching back reuses it.
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "BRB"}),
        );
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "Main"}),
        );
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "BRB"}),
        );
        assert_eq!(starts.load(Ordering::Relaxed), 2);
        // Removing a source from every scene ends its capture at once, also
        // when its scene is not the rendered one.
        let brb_desk = source_id(&mut engine, "BRB", "desk");
        run(
            &mut engine,
            "remove_source",
            json!({"scene_name": "Main", "source_name": "desk"}),
        );
        let ids = |engine: &mut Engine| -> Vec<String> {
            health(engine)
                .captures
                .into_iter()
                .map(|capture| capture.id)
                .collect()
        };
        assert_eq!(ids(&mut engine), std::slice::from_ref(&brb_desk));
        // So does deleting a scene whose capture is kept.
        run(&mut engine, "create_scene", json!({"name": "Spare"}));
        run(
            &mut engine,
            "add_source",
            json!({"scene_name": "Spare", "name": "cam", "kind": "screen"}),
        );
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "Spare"}),
        );
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "BRB"}),
        );
        assert_eq!(ids(&mut engine).len(), 2);
        run(
            &mut engine,
            "delete_scene",
            json!({"scene_name": "Spare", "confirm": true}),
        );
        assert_eq!(ids(&mut engine), [brb_desk]);
        let CommandOutput::PreviewState(preview) =
            run(&mut engine, "set_preview", json!({"enabled": false}))
        else {
            panic!("unexpected output");
        };
        let pipeline = preview.pipeline.expect("pipeline report");
        // Main's and Spare's captures retired when their sources went; BRB's
        // at the stop.
        assert_eq!(pipeline.scene_captures.len(), 3, "{pipeline:?}");
        assert!(preview.frames_published.is_some());
    }

    #[test]
    fn a_hidden_screen_source_keeps_its_capture() {
        let _media = media_test_lock();
        let (mut engine, starts) = engine_with_screen_counter();
        run(&mut engine, "create_scene", json!({"name": "Main"}));
        run(
            &mut engine,
            "add_source",
            json!({"scene_name": "Main", "name": "desk", "kind": "screen"}),
        );
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "Main"}),
        );
        run(&mut engine, "set_preview", json!({"enabled": true}));
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        for visible in [false, true, false] {
            run(
                &mut engine,
                "update_source",
                json!({"scene_name": "Main", "source_name": "desk", "visible": visible}),
            );
            assert_eq!(health(&mut engine).captures.len(), 1, "visible {visible}");
        }
        // Hidden, moved and shown again: still the first capture.
        run(
            &mut engine,
            "add_source",
            json!({"scene_name": "Main", "name": "bars", "kind": "color_bars"}),
        );
        run(
            &mut engine,
            "move_source",
            json!({"scene_name": "Main", "source_name": "desk", "index": 1}),
        );
        run(
            &mut engine,
            "update_source",
            json!({"scene_name": "Main", "source_name": "desk", "visible": true}),
        );
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        assert_eq!(health(&mut engine).captures.len(), 1);
        run(&mut engine, "set_preview", json!({"enabled": false}));
    }

    #[test]
    fn an_ended_screen_source_shows_in_health() {
        let _media = media_test_lock();
        let dir = TempDir::new("capture-state");
        let (engine, _starts, ends) = engine_with_screen_streams();
        let mut engine = engine.with_output_dir(&dir.0);
        let CommandOutput::SceneList(scenes) = run(
            &mut engine,
            "add_source",
            json!({"scene_name": "Test pattern", "name": "desk", "kind": "screen",
                   "capture_target_id": "fake:1"}),
        ) else {
            panic!("unexpected output");
        };
        let desk_id = scenes.scenes[0].sources[2].id.clone();
        start(&mut engine, json!({"file_name": "ended.mkv"})).expect("start");
        let desk = |health: &command_api::HealthReport| {
            health
                .captures
                .iter()
                .find(|capture| capture.name == "desk")
                .cloned()
                .expect("desk capture")
        };
        // Render counters are published every 250 ms.
        let deadline = Instant::now() + Duration::from_secs(2);
        let report = loop {
            let report = health(&mut engine);
            let rendered = report.render.as_ref().is_some_and(|render| {
                render
                    .sources
                    .iter()
                    .any(|source| source.id.as_str() == desk_id)
            });
            if rendered || Instant::now() >= deadline {
                break report;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let running = desk(&report);
        assert_eq!(running.kind, command_api::CaptureKind::Screen);
        // The same id as in list_scenes, in captures and render sources.
        assert_eq!(running.id, desk_id);
        let rendered = report.render.expect("render");
        assert!(
            rendered
                .sources
                .iter()
                .any(|source| source.id.as_str() == desk_id),
            "{rendered:?}"
        );
        assert_eq!(running.state, capture_api::CaptureState::Running);

        // The system ends the capture: health says so, with the reason, and
        // the recording keeps going.
        ends.lock().expect("screen ends")[0].end(|| "the captured window was closed".to_owned());
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
            try_run(
                engine,
                "restart_source",
                json!({"scene_name": scene, "source_name": name}),
            )
        };
        run(
            &mut engine,
            "add_source",
            json!({"scene_name": "Test pattern", "name": "desk", "kind": "screen",
                   "capture_target_id": "fake:1"}),
        );
        // Nothing captures yet.
        assert_eq!(
            restart(&mut engine, "Test pattern", "desk")
                .unwrap_err()
                .code,
            ErrorCode::InvalidState
        );
        start(&mut engine, json!({"file_name": "restart.mkv"})).expect("start");
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        let desk = |health: &command_api::HealthReport| {
            health
                .captures
                .iter()
                .find(|capture| capture.name == "desk")
                .cloned()
                .expect("desk capture")
        };
        ends.lock().expect("screen ends")[0].end(|| "the captured window was closed".to_owned());
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
        assert_eq!(starts.load(Ordering::Relaxed), 2);
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
            json!({"scene_name": "Other", "name": "cam", "kind": "screen", "capture_target_id": "fake:2"}),
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
            .filter(|outcome| outcome.name == "desk")
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
            try_run(
                engine,
                "restart_source",
                json!({"scene_name": "Test pattern", "source_name": "desk"}),
            )
        };
        run(&mut engine, "set_preview", json!({"enabled": true}));
        run(
            &mut engine,
            "add_source",
            json!({"scene_name": "Test pattern", "name": "desk", "kind": "screen"}),
        );
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        start(&mut engine, json!({"file_name": "picker.mkv"})).expect("start");
        let err = restart(&mut engine).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidState);
        assert!(err.message.contains("picker"), "{err}");
        assert_eq!(starts.load(Ordering::Relaxed), 1);
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
        assert_eq!(starts.load(Ordering::Relaxed), 2);
        run(&mut engine, "set_preview", json!({"enabled": false}));
    }

    fn scene_list(engine: &mut Engine) -> command_api::SceneList {
        match run(engine, "list_scenes", json!({})) {
            CommandOutput::SceneList(list) => list,
            other => panic!("unexpected output: {other:?}"),
        }
    }

    #[test]
    fn a_project_saves_and_loads_back_with_its_ids() {
        let dir = TempDir::new("project");
        let mut engine = engine().with_project_dir(&dir.0);
        run(&mut engine, "create_scene", json!({"name": "Show"}));
        run(
            &mut engine,
            "add_source",
            json!({"scene_name": "Show", "name": "red", "kind": "solid", "color": "#ff000080",
                   "x": 10, "y": 20, "width": 300, "height": 200, "opacity": 0.5}),
        );
        run(
            &mut engine,
            "add_source",
            json!({"scene_name": "Show", "name": "desk", "kind": "screen", "capture_target_id": "fake:1",
                   "cursor_mode": "hidden"}),
        );
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "Show"}),
        );
        run(
            &mut engine,
            "add_channel",
            json!({"name": "A", "input": {"kind": "tone", "frequency_hz": 440, "level_dbfs": -18},
                   "gain_db": -2, "mute": true, "balance": 0.25}),
        );
        run(
            &mut engine,
            "update_bus",
            json!({"bus_id": "bus_program", "gain_db": -4, "mute": true}),
        );
        run(
            &mut engine,
            "add_bus",
            json!({"name": "Voice", "gain_db": -1, "assign_db": -3}),
        );
        run(
            &mut engine,
            "set_send",
            json!({"channel_name": "A", "bus_name": "Voice", "enabled": true, "level_db": -6,
                   "tap": "pre_fader"}),
        );
        run(
            &mut engine,
            "set_output_profile",
            json!({"output": "stream", "bus_name": "Voice", "video_bitrate_kbps": 4500}),
        );
        let profiles_before = run(&mut engine, "get_output_profiles", json!({}));
        let before = scene_list(&mut engine);
        let mixer_before = mixer(run(&mut engine, "get_mixer", json!({})));
        let CommandOutput::ProjectSaved(saved) = run(
            &mut engine,
            "save_project",
            json!({"file_name": "show.json"}),
        ) else {
            panic!("unexpected output");
        };
        assert_eq!((saved.scenes, saved.replaced), (2, false));
        assert!(dir.0.join("show.json").is_file());

        // A fresh engine with the same directory gets everything back but
        // the screen source's target.
        let mut fresh = self::engine().with_project_dir(&dir.0);
        let CommandOutput::ProjectLoaded(loaded) = run(
            &mut fresh,
            "load_project",
            json!({"file_name": "show.json"}),
        ) else {
            panic!("unexpected output");
        };
        let mut expected = before.clone();
        let command_api::SourceKind::Screen {
            capture_target_id, ..
        } = &mut expected.scenes[1].sources[1].kind
        else {
            panic!("screen source");
        };
        *capture_target_id = None;
        assert_eq!(loaded.scenes, expected);
        assert_eq!(scene_list(&mut fresh), expected);
        assert_eq!(loaded.mixer, mixer_before);
        assert_eq!(
            run(&mut fresh, "get_output_profiles", json!({})),
            profiles_before
        );
        assert!(
            loaded.warnings[0].contains("set_capture_target"),
            "{:?}",
            loaded.warnings
        );
        // Ids survive, so commands by id work across the save.
        let desk = before.scenes[1].sources[1].id.clone();
        run(
            &mut fresh,
            "set_capture_target",
            json!({"source_id": desk, "capture_target_id": "fake:2"}),
        );
        let CommandOutput::ProjectValidation(validation) = run(
            &mut fresh,
            "validate_project",
            json!({"file_name": "show.json"}),
        ) else {
            panic!("unexpected output");
        };
        assert!(validation.valid && validation.problems.is_empty());
        assert_eq!(
            validation.schema_version,
            Some(project_file::SCHEMA_VERSION)
        );
        assert!(!validation.migrates);
    }

    #[test]
    fn a_version_1_project_is_migrated_on_load() {
        let dir = TempDir::new("project-v1");
        let v1 = json!({
            "schema_version": 1,
            "canvas": {"width": 1920, "height": 1080},
            "active_scene": "scene_test_pattern",
            "scenes": [{"id": "scene_test_pattern", "name": "Test pattern", "sources": []}],
            "audio": {
                "inputs": [{"input": "mic", "gain_db": -3, "mute": true}],
                "tones": [{"frequency_hz": 440, "level_dbfs": -18, "gain_db": 0, "mute": false}],
                "master_gain_db": -1
            }
        });
        std::fs::write(dir.0.join("old.json"), v1.to_string()).expect("write");
        let mut engine = engine().with_project_dir(&dir.0);
        let CommandOutput::ProjectValidation(validation) = run(
            &mut engine,
            "validate_project",
            json!({"file_name": "old.json"}),
        ) else {
            panic!("unexpected output");
        };
        assert!(validation.valid, "{validation:?}");
        assert_eq!(validation.schema_version, Some(1));
        assert!(validation.migrates);
        let CommandOutput::ProjectLoaded(loaded) = run(
            &mut engine,
            "load_project",
            json!({"file_name": "old.json"}),
        ) else {
            panic!("unexpected output");
        };
        // Every step since version 1 is named.
        let warning = &loaded.warnings[0];
        for change in ["migrated", "became channels", "`target_kinds`", "`visible`"] {
            assert!(warning.contains(change), "{change}: {warning}");
        }
        let channels: Vec<_> = loaded
            .mixer
            .channels
            .iter()
            .map(|channel| {
                (
                    channel.spec.name.as_str(),
                    channel.spec.input.kind(),
                    channel.spec.gain_db,
                    channel.spec.muted,
                )
            })
            .collect();
        assert_eq!(
            channels,
            [
                ("Input 1", "device", -3.0, true),
                ("Tone 1", "tone", 0.0, false)
            ]
        );
        assert_eq!(loaded.mixer.buses[0].gain_db, -1.0);
        // Saving writes the current version, which loads without a migration.
        run(
            &mut engine,
            "save_project",
            json!({"file_name": "old.json", "overwrite": true}),
        );
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.0.join("old.json")).expect("read"))
                .expect("json");
        assert_eq!(written["schema_version"], project_file::SCHEMA_VERSION);
        assert_eq!(written["audio"]["channels"][1]["name"], "Tone 1");
        let CommandOutput::ProjectLoaded(again) = run(
            &mut engine,
            "load_project",
            json!({"file_name": "old.json"}),
        ) else {
            panic!("unexpected output");
        };
        assert!(again.warnings.is_empty(), "{:?}", again.warnings);
        assert_eq!(again.mixer, loaded.mixer);
    }

    #[test]
    fn the_project_directory_lists_the_files_a_load_can_name() {
        let dir = TempDir::new("project-list");
        let mut engine = engine().with_project_dir(&dir.0);
        let CommandOutput::ProjectSaved(saved) = run(
            &mut engine,
            "save_project",
            json!({"file_name": "show.json"}),
        ) else {
            panic!("unexpected output");
        };
        std::fs::write(dir.0.join("notes.txt"), b"x").expect("txt");
        // Windows opens the device for this name, so it only exists as a
        // file elsewhere; no command would accept it.
        #[cfg(unix)]
        std::fs::write(dir.0.join("NUL.json"), b"{}").expect("device name");
        let CommandOutput::ProjectFiles(list) = run(&mut engine, "list_projects", json!({})) else {
            panic!("unexpected output");
        };
        let names: Vec<&str> = list
            .files
            .iter()
            .map(|file| file.file_name.as_str())
            .collect();
        assert_eq!(names, ["show.json"]);
        assert_eq!(list.files[0].bytes, saved.bytes);
        assert!(list.files[0].modified_unix_ms.is_some());
        assert_eq!(list.omitted, 0);

        // A bounded answer: the rest is counted.
        for index in 0..command_api::MAX_LISTED_PROJECTS {
            std::fs::write(dir.0.join(format!("extra-{index:03}.json")), b"{}").expect("extra");
        }
        let CommandOutput::ProjectFiles(list) = run(&mut engine, "list_projects", json!({})) else {
            panic!("unexpected output");
        };
        assert_eq!(list.files.len(), command_api::MAX_LISTED_PROJECTS);
        assert_eq!(list.omitted, 1);
        assert_eq!(list.files[0].file_name, "extra-000.json");
    }

    #[test]
    fn project_commands_need_a_directory_an_idle_pipeline_and_explicit_overwrite() {
        let _media = media_test_lock();
        let unconfigured = try_run(
            &mut engine(),
            "save_project",
            json!({"file_name": "a.json"}),
        )
        .unwrap_err();
        assert_eq!(unconfigured.code, ErrorCode::InvalidState);
        assert!(unconfigured.message.contains("--project-dir"));

        let unlisted = try_run(&mut self::engine(), "list_projects", json!({})).unwrap_err();
        assert_eq!(unlisted.code, ErrorCode::InvalidState);

        let dir = TempDir::new("project-rules");
        let mut engine = engine().with_project_dir(&dir.0);
        run(&mut engine, "save_project", json!({"file_name": "a.json"}));
        let exists =
            try_run(&mut engine, "save_project", json!({"file_name": "a.json"})).unwrap_err();
        assert_eq!(exists.code, ErrorCode::InvalidParams);
        assert!(exists.message.contains("overwrite"));
        let CommandOutput::ProjectSaved(replaced) = run(
            &mut engine,
            "save_project",
            json!({"file_name": "a.json", "overwrite": true}),
        ) else {
            panic!("unexpected output");
        };
        assert!(replaced.replaced);
        let missing =
            try_run(&mut engine, "load_project", json!({"file_name": "b.json"})).unwrap_err();
        assert_eq!(missing.code, ErrorCode::NotFound);
        let mut nowhere = self::engine().with_project_dir(dir.0.join("absent"));
        let no_dir =
            try_run(&mut nowhere, "save_project", json!({"file_name": "a.json"})).unwrap_err();
        assert_eq!(no_dir.code, ErrorCode::NotFound);
        assert!(
            no_dir.message.contains("does not exist"),
            "{}",
            no_dir.message
        );
        std::fs::create_dir(dir.0.join("folder.json")).expect("dir");
        let folder = try_run(
            &mut engine,
            "load_project",
            json!({"file_name": "folder.json"}),
        )
        .unwrap_err();
        assert_eq!(folder.code, ErrorCode::InvalidParams);

        // Saving works while the pipeline runs; loading does not.
        run(&mut engine, "set_preview", json!({"enabled": true}));
        run(
            &mut engine,
            "save_project",
            json!({"file_name": "a.json", "overwrite": true}),
        );
        let busy =
            try_run(&mut engine, "load_project", json!({"file_name": "a.json"})).unwrap_err();
        assert_eq!(busy.code, ErrorCode::InvalidState);
        run(&mut engine, "set_preview", json!({"enabled": false}));
        run(&mut engine, "load_project", json!({"file_name": "a.json"}));
    }

    #[test]
    fn an_invalid_project_changes_nothing_and_says_where() {
        let dir = TempDir::new("project-invalid");
        let mut engine = engine().with_project_dir(&dir.0);
        run(
            &mut engine,
            "save_project",
            json!({"file_name": "good.json"}),
        );
        let mut file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.0.join("good.json")).expect("read"))
                .expect("json");
        file["scenes"][0]["sources"][1]["opacity"] = json!(3);
        file["active_scene"] = json!("scene_gone");
        std::fs::write(dir.0.join("bad.json"), file.to_string()).expect("write");
        run(&mut engine, "create_scene", json!({"name": "Unsaved"}));
        let before = scene_list(&mut engine);

        let error = try_run(
            &mut engine,
            "load_project",
            json!({"file_name": "bad.json"}),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidParams);
        assert!(error.message.contains("2 problem(s)"), "{}", error.message);
        let details = error.details.expect("details");
        assert_eq!(details["problems"][0]["path"], "scenes[0].sources[1]");
        assert_eq!(details["problems"][1]["path"], "active_scene");
        assert_eq!(scene_list(&mut engine), before);

        let CommandOutput::ProjectValidation(validation) = run(
            &mut engine,
            "validate_project",
            json!({"file_name": "bad.json"}),
        ) else {
            panic!("unexpected output");
        };
        assert!(!validation.valid);
        assert_eq!(validation.problems.len(), 2);
    }

    #[test]
    fn set_capture_target_chooses_a_target_only_while_nothing_captures() {
        let _media = media_test_lock();
        let (engine, starts, _ends) = engine_with_screen_streams();
        let mut engine = engine;
        run(
            &mut engine,
            "add_source",
            json!({"scene_name": "Test pattern", "name": "desk", "kind": "screen",
                   "capture_target_id": "fake:1"}),
        );
        let desk = source_id(&mut engine, "Test pattern", "desk");
        let bars = source_id(&mut engine, "Test pattern", "color_bars");
        let not_screen = try_run(
            &mut engine,
            "set_capture_target",
            json!({"source_id": bars, "capture_target_id": "fake:2"}),
        )
        .unwrap_err();
        assert_eq!(not_screen.code, ErrorCode::InvalidParams);
        run(
            &mut engine,
            "set_capture_target",
            json!({"source_id": desk, "capture_target_id": "fake:2"}),
        );
        assert_eq!(
            screen_ids(&mut engine, "Test pattern"),
            [("desk".to_owned(), Some("fake:2".to_owned()))]
        );
        // A running capture keeps its target; restart_source switches it.
        run(&mut engine, "set_preview", json!({"enabled": true}));
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        let capturing = try_run(
            &mut engine,
            "set_capture_target",
            json!({"source_id": desk, "capture_target_id": "fake:3"}),
        )
        .unwrap_err();
        assert_eq!(capturing.code, ErrorCode::InvalidState);
        assert!(capturing.message.contains("restart_source"));
        assert_eq!(
            screen_ids(&mut engine, "Test pattern"),
            [("desk".to_owned(), Some("fake:2".to_owned()))]
        );
        run(&mut engine, "set_preview", json!({"enabled": false}));
    }

    /// The id `list_scenes` gives source `name` of `scene`.
    fn source_id(engine: &mut Engine, scene: &str, name: &str) -> String {
        let CommandOutput::SceneList(list) = run(engine, "list_scenes", json!({})) else {
            panic!("unexpected output");
        };
        list.scenes
            .into_iter()
            .find(|candidate| candidate.name == scene)
            .and_then(|scene| scene.sources.into_iter().find(|source| source.name == name))
            .map(|source| source.id)
            .expect("source")
    }

    /// Screen sources of `scene`: (name, capture_target_id).
    fn screen_ids(engine: &mut Engine, scene: &str) -> Vec<(String, Option<String>)> {
        let CommandOutput::SceneList(list) = run(engine, "list_scenes", json!({})) else {
            panic!("unexpected output");
        };
        let scene = list
            .scenes
            .into_iter()
            .find(|candidate| candidate.name == scene)
            .expect("scene");
        scene
            .sources
            .into_iter()
            .filter_map(|source| match source.kind {
                command_api::SourceKind::Screen {
                    capture_target_id, ..
                } => Some((source.name, capture_target_id)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_source_restarts_on_another_capture_target_id_and_a_failed_restart_changes_nothing() {
        let _media = media_test_lock();
        let dir = TempDir::new("retarget");
        let (engine, starts, ends) = engine_with_screen_streams();
        let mut engine = engine.with_output_dir(&dir.0);
        run(
            &mut engine,
            "add_source",
            json!({"scene_name": "Test pattern", "name": "desk", "kind": "screen",
                   "capture_target_id": "fake:1"}),
        );
        start(&mut engine, json!({"file_name": "retarget.mkv"})).expect("start");
        ends.lock().expect("screen ends")[0].end(|| "the captured window was closed".to_owned());

        // The window's id is gone: restarting on it fails, and nothing
        // changes (the scene keeps its id, the old capture stays listed).
        let err = try_run(
            &mut engine,
            "restart_source",
            json!({"scene_name": "Test pattern", "source_name": "desk", "capture_target_id": "fake:gone"}),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound, "{err}");
        assert_eq!(
            screen_ids(&mut engine, "Test pattern"),
            [("desk".to_owned(), Some("fake:1".to_owned()))]
        );
        assert!(health(&mut engine).captures.iter().any(|capture| {
            capture.name == "desk"
                && matches!(capture.state, capture_api::CaptureState::Ended { .. })
        }));

        // Another window: the scene keeps the new id.
        run(
            &mut engine,
            "restart_source",
            json!({"scene_name": "Test pattern", "source_name": "desk", "capture_target_id": "fake:2"}),
        );
        assert_eq!(starts.load(Ordering::Relaxed), 2);
        assert_eq!(
            screen_ids(&mut engine, "Test pattern"),
            [("desk".to_owned(), Some("fake:2".to_owned()))]
        );
        assert!(health(&mut engine).captures.iter().any(|capture| {
            capture.name == "desk" && capture.state == capture_api::CaptureState::Running
        }));
        engine.handle(Command::StopRecording).expect("stop");
    }

    #[test]
    fn a_kept_capture_of_another_scene_restarts() {
        let _media = media_test_lock();
        let (mut engine, starts, ends) = engine_with_screen_streams();
        for (scene, id) in [("Main", "fake:1"), ("BRB", "fake:2")] {
            run(&mut engine, "create_scene", json!({"name": scene}));
            run(
                &mut engine,
                "add_source",
                json!({"scene_name": scene, "name": "cam", "kind": "screen", "capture_target_id": id}),
            );
        }
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "Main"}),
        );
        run(&mut engine, "set_preview", json!({"enabled": true}));
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "BRB"}),
        );
        assert_eq!(starts.load(Ordering::Relaxed), 2);
        // Main's capture keeps running off air, and ends there.
        ends.lock().expect("screen ends")[0].end(|| "the captured window was closed".to_owned());
        // By id alone: source ids are unique across scenes.
        let cam = source_id(&mut engine, "Main", "cam");
        run(&mut engine, "restart_source", json!({"source_id": cam}));
        assert_eq!(starts.load(Ordering::Relaxed), 3);
        let captures = health(&mut engine).captures;
        assert_eq!(captures.len(), 2, "{captures:?}");
        assert!(
            captures
                .iter()
                .all(|capture| capture.state == capture_api::CaptureState::Running),
            "{captures:?}"
        );
        let CommandOutput::PreviewState(preview) =
            run(&mut engine, "set_preview", json!({"enabled": false}))
        else {
            panic!("unexpected output");
        };
        // The replaced capture and the two final ones.
        let pipeline = preview.pipeline.expect("pipeline report");
        assert_eq!(pipeline.scene_captures.len(), 3, "{pipeline:?}");
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
            Some(&json!({"scene_name": "Test pattern", "name": "desk", "kind": "screen"})),
        )
        .expect("decode");
        let err = engine.handle(add).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidState);
        assert!(err.message.contains("picker"), "{err}");
        assert_eq!(starts.load(Ordering::Relaxed), 0);
        // The refused edit was undone.
        let CommandOutput::SceneList(list) = run(&mut engine, "list_scenes", json!({})) else {
            panic!("unexpected output");
        };
        assert_eq!(list.scenes[0].sources.len(), 2);
        // Edits that need no capture still apply live.
        run(
            &mut engine,
            "update_source",
            json!({"scene_name": "Test pattern", "source_name": "overlay", "opacity": 0.5}),
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
            json!({"scene_name": "Green", "name": "fill", "kind": "solid", "color": "#00ff00"}),
        );
        run(
            &mut engine,
            "set_active_scene",
            json!({"scene_name": "Green"}),
        );
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
        assert_eq!(
            queries.load(Ordering::Relaxed),
            1,
            "get_status must reuse the cached query"
        );
        let output = engine
            .handle(Command::GetCaptureCapabilities)
            .expect("capabilities");
        let CommandOutput::CaptureCapabilities(caps) = output else {
            panic!("unexpected output: {output:?}");
        };
        assert_eq!(caps.platform, std::env::consts::OS);
        assert_eq!(caps.desktop, std::env::var("XDG_CURRENT_DESKTOP").ok());
        assert_eq!(caps.audio.unavailable_reason.as_deref(), Some("no server"));
        assert_eq!(
            queries.load(Ordering::Relaxed),
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
            (1920, 1080, Rational::integer(60))
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
        let params = json!({"duration_ms": 100, "snapshot_file_name": "snapshot.ppm"});

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
        assert!(stats.program.rms_dbfs.is_some());
        assert_eq!(report.captures.len(), 1);
        assert!(report.captures[0].report.is_some());
    }

    #[test]
    fn a_channel_whose_device_is_missing_is_silent_and_reported() {
        let _media = media_test_lock();
        let dir = TempDir::new("silent-device");
        let mut engine = engine().with_output_dir(&dir.0);
        let added = mixer(run(
            &mut engine,
            "add_channel",
            json!({"name": "Mic", "input": {"kind": "device", "device": "missing"}}),
        ));
        let mic = added.channels[1].spec.id.clone();
        // The recording starts; its outcome says the channel is silent.
        let CommandOutput::RecordingStarted(started) =
            start(&mut engine, json!({"file_name": "silent.mkv"})).expect("start")
        else {
            panic!("unexpected output");
        };
        assert_eq!(started.warnings.len(), 1, "{:?}", started.warnings);
        assert!(
            started.warnings[0].contains("`Mic` is silent")
                && started.warnings[0].contains("`missing` was not found"),
            "{:?}",
            started.warnings
        );
        let state = mixer(run(&mut engine, "get_mixer", json!({})));
        assert!(state.running);
        assert_eq!(state.warnings, started.warnings);
        let capture = health(&mut engine)
            .captures
            .into_iter()
            .find(|capture| capture.id == mic)
            .expect("the channel's capture");
        let capture_api::CaptureState::Ended { reason } = capture.state else {
            panic!("{capture:?}");
        };
        assert!(reason.starts_with("could not start"), "{reason}");
        // The tone still reaches the file: only the missing channel is silent.
        let deadline = Instant::now() + Duration::from_secs(10);
        while health(&mut engine)
            .recording
            .is_none_or(|recording| recording.counters.samples_encoded == 0)
        {
            assert!(Instant::now() < deadline, "no audio encoded");
            std::thread::sleep(Duration::from_millis(20));
        }
        let report = match engine.handle(Command::StopRecording).expect("stop") {
            CommandOutput::RecordingReport(report) => *report,
            other => panic!("unexpected output: {other:?}"),
        };
        let captures = &report.pipeline.audio_captures;
        assert_eq!(captures.len(), 1, "{captures:?}");
        assert_eq!(captures[0].input.as_deref(), Some("missing"));
        assert!(captures[0].error.is_some() && captures[0].report.is_none());
        assert!(
            report.recording.audio_tracks[0].samples_encoded > 0,
            "{report:?}"
        );
        let silent: Vec<_> = report
            .warnings
            .iter()
            .filter(|warning| warning.contains("no such input"))
            .collect();
        assert_eq!(silent.len(), 1, "{:?}", report.warnings);
        assert!(
            silent[0].contains("`Mic` was silent for the whole run"),
            "{silent:?}"
        );
    }

    #[test]
    fn other_device_errors_still_fail_the_start() {
        let _media = media_test_lock();
        let dir = TempDir::new("device-errors");
        let mut engine = engine().with_output_dir(&dir.0);
        // Not permitted: the user has to act, so the start says so.
        run(
            &mut engine,
            "add_channel",
            json!({"name": "Mic", "input": {"kind": "device", "device": "denied"}}),
        );
        let denied = start(&mut engine, json!({"file_name": "denied.mkv"})).unwrap_err();
        assert_eq!(denied.code, ErrorCode::PermissionDenied);
        assert!(!status(&mut engine).pipeline_running);
        assert!(!dir.0.join("denied.mkv").exists());
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
    #[test]
    fn missing_references_report_the_supplied_selector_field() {
        let mut engine = Engine::new();
        for (command, params, field, value) in [
            (
                "set_active_scene",
                json!({"scene_id": "scene_missing"}),
                "scene_id",
                "scene_missing",
            ),
            (
                "set_active_scene",
                json!({"scene_name": "Missing"}),
                "scene_name",
                "Missing",
            ),
            (
                "remove_source",
                json!({"source_id": "src_missing"}),
                "source_id",
                "src_missing",
            ),
            (
                "remove_source",
                json!({"scene_name": "Test pattern", "source_name": "Missing"}),
                "source_name",
                "Missing",
            ),
            (
                "remove_channel",
                json!({"channel_id": "ch_missing"}),
                "channel_id",
                "ch_missing",
            ),
            (
                "remove_channel",
                json!({"channel_name": "Missing"}),
                "channel_name",
                "Missing",
            ),
            (
                "remove_bus",
                json!({"bus_id": "bus_missing"}),
                "bus_id",
                "bus_missing",
            ),
            (
                "remove_bus",
                json!({"bus_name": "Missing"}),
                "bus_name",
                "Missing",
            ),
            (
                "set_send",
                json!({"channel_name": "Missing", "bus_name": "Program", "enabled": true}),
                "channel_name",
                "Missing",
            ),
            (
                "set_send",
                json!({"channel_name": "Tone", "bus_name": "Missing", "enabled": true}),
                "bus_name",
                "Missing",
            ),
            (
                "set_output_profile",
                json!({"output": "recording", "bus_id": "bus_missing"}),
                "bus_id",
                "bus_missing",
            ),
        ] {
            let error = try_run(&mut engine, command, params).unwrap_err();
            assert_eq!(error.code, ErrorCode::NotFound, "{command}: {error:?}");
            assert_eq!(error.details, Some(json!({field: value})), "{command}");
        }
        // A track's error names its entry: the mix takes the same fields.
        for (track, field, value) in [
            (json!({"bus_name": "Missing"}), "bus_name", "Missing"),
            (json!({"bus_id": "bus_missing"}), "bus_id", "bus_missing"),
        ] {
            let params =
                json!({"output": "recording", "tracks": [{"bus_id": "bus_program"}, track]});
            let error = try_run(&mut engine, "set_output_profile", params).unwrap_err();
            assert_eq!(error.code, ErrorCode::NotFound, "{error:?}");
            assert_eq!(error.details, Some(json!({field: value, "track_index": 1})));
            assert!(
                error.message.starts_with("`tracks[1]`"),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn source_selectors_and_track_references_must_agree() {
        let mut engine = Engine::new();
        run(
            &mut engine,
            "update_source",
            json!({"source_id": "src_color_bars", "scene_id": "scene_test_pattern", "scene_name": "Test pattern", "source_name": "color_bars", "x": 1}),
        );
        for params in [
            json!({"source_id": "src_color_bars", "source_name": "overlay", "x": 2}),
            json!({"source_id": "src_color_bars", "scene_name": "Missing", "x": 2}),
            json!({"source_id": "src_color_bars", "scene_id": "scene_test_pattern", "scene_name": "Missing", "x": 2}),
        ] {
            assert_eq!(
                try_run(&mut engine, "update_source", params)
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidParams
            );
        }
        run(&mut engine, "add_bus", json!({"name": "bus_program"}));
        let CommandOutput::OutputProfileSet(set) = run(
            &mut engine,
            "set_output_profile",
            json!({"output": "recording", "tracks": [{"bus_name": "bus_program"}]}),
        ) else {
            panic!("profiles");
        };
        let track = set.profiles.recording.tracks[0].clone();
        assert_ne!(track, "bus_program", "the id-looking name resolves by name");
        run(
            &mut engine,
            "set_output_profile",
            json!({"output": "recording", "tracks": [{"bus_id": track, "bus_name": "bus_program"}]}),
        );
        let before = run(&mut engine, "get_output_profiles", json!({}));
        let error = try_run(
            &mut engine,
            "set_output_profile",
            json!({"output": "recording", "tracks": [{"bus_id": track, "bus_name": "Program"}]}),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidParams);
        assert_eq!(run(&mut engine, "get_output_profiles", json!({})), before);
    }
}
