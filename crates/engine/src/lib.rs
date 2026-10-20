//! Engine controller: owns engine state and executes typed commands.
//!
//! Independent of UI and transport. CLI, the JSON-lines server, and future UI
//! and MCP adapters all drive the engine through [`CommandHandler`]. Platform
//! capture is injected as a [`CaptureBackend`] by the composition root.

mod composite;
mod mix;
mod outputs;
mod pipeline;

use outputs::{RecordingOutput, StreamingOutput};
use pipeline::Pipeline;

use std::path::PathBuf;

use capture_api::{
    CaptureBackend, CaptureCapabilities, CaptureError, CaptureErrorKind, UnsupportedCapture,
};
use command_api::{
    AudioInputList, Command, CommandError, CommandHandler, CommandList, CommandOutput,
    EngineStatus, ErrorCode,
};
use encoder_ffmpeg::StreamState;

pub struct Engine {
    /// Shared by the outputs; declared first so it is dropped (producers
    /// stopped) before the outputs finalize.
    pipeline: Option<Pipeline>,
    /// Open outputs; dropping the engine finalizes them.
    recording: Option<RecordingOutput>,
    streaming: Option<StreamingOutput>,
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
                // No media graph or output exists yet (M3+).
                media_engine_ready: false,
                capture_available: false,
                streaming_active: false,
                streaming_state: None,
                recording_active: false,
                recording_failed: false,
            },
            capture,
            output_dir: PathBuf::from("."),
            capture_capabilities: None,
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
            Command::ListCommands => Ok(CommandOutput::Commands(CommandList::all())),
            Command::GetCaptureCapabilities => Ok(CommandOutput::CaptureCapabilities(
                self.refresh_capabilities().clone(),
            )),
            Command::ListAudioInputs => self
                .capture
                .audio_inputs()
                .map(|inputs| CommandOutput::AudioInputs(AudioInputList { inputs }))
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
        }
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
    use std::cell::Cell;
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
            Ok(Box::new(FakeStream {
                mailbox,
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
        }));
        (engine, queries)
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
        assert!(!status.media_engine_ready);
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

    #[cfg(target_os = "linux")]
    fn status(engine: &mut Engine) -> EngineStatus {
        match engine.handle(Command::GetStatus).expect("status") {
            CommandOutput::Status(status) => status,
            other => panic!("unexpected output: {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    fn start(
        engine: &mut Engine,
        params: serde_json::Value,
    ) -> Result<CommandOutput, CommandError> {
        engine.handle(Command::decode("start_recording", Some(&params)).expect("decode"))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recording_lifecycle_produces_a_finalized_file() {
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
        // Both tracks end at the stop time, within a frame or a block, plus
        // any counted trailing drops on a slow machine.
        let video_end = recording.video.end_ms.expect("video");
        let audio_end = recording.audio.end_ms.expect("audio");
        let allowed = 30.0 + recording.video.frames_dropped as f64 * 1000.0 / 60.0;
        assert!(
            (video_end - audio_end).abs() < allowed,
            "{video_end} vs {audio_end}"
        );
        assert!(!status(&mut engine).recording_active);

        let stop_again = engine.handle(Command::StopRecording).unwrap_err();
        assert_eq!(stop_again.code, ErrorCode::InvalidState);
    }

    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn streams_the_pipeline_to_a_local_server() {
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

    #[cfg(target_os = "linux")]
    #[test]
    fn recording_continues_when_the_stream_fails() {
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
        // Every frame tick before the cut is encoded or counted.
        let ticks = (recording.audio.end_ms.expect("audio") * 60.0 / 1000.0).round() as u64;
        let skipped = report
            .pipeline
            .render
            .as_ref()
            .map_or(0, |r| r.skipped_ticks);
        let accounted = recording.video.frames_encoded + recording.video.frames_dropped + skipped;
        assert!(
            accounted.abs_diff(ticks) <= 1,
            "{accounted} vs {ticks}: {recording:?}"
        );
        let _ = server.join();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_output_directory_is_not_found() {
        let dir = TempDir::new("missing");
        let mut engine = engine().with_output_dir(dir.0.join("absent"));
        let err = start(&mut engine, json!({"file_name": "a.mkv"})).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound, "{err}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dropping_the_engine_finalizes_the_recording() {
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
