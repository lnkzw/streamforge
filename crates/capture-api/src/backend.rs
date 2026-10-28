use std::sync::Arc;

use media_core::{AudioRing, FrameMailbox, SampleReader};

use crate::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioInput, AudioOutput,
    AudioStreamRequest, Camera, CameraCaptureCapability, CameraRequest, CaptureCapabilities,
    CaptureError, CaptureState, PlaybackRequest, ScreenCaptureCapability, ScreenCaptureRequest,
    ScreenSelection, ScreenSource, VideoCaptureReport, VideoSourceInfo,
};

/// A running audio capture delivering interleaved `f32` samples until stopped.
///
/// Dropping a stream without calling [`AudioStream::stop`] still stops it,
/// but discards the report.
pub trait AudioStream: Send {
    /// Samples in the requested rate and channel count; the ring is bounded
    /// and counts overruns when the consumer falls behind.
    fn samples(&self) -> Arc<AudioRing>;

    /// Whether the capture still delivers; never blocks. Required, so a
    /// stream (or a wrapper) cannot report a dead capture as running by
    /// forgetting it; a backend that cannot tell returns `Running`.
    fn state(&self) -> CaptureState;

    /// Stop capturing and return diagnostics for the whole session.
    fn stop(self: Box<Self>) -> Result<AudioCaptureReport, CaptureError>;
}

/// A running playback: the device pulls from its feed on its own thread
/// until stopped.
///
/// The device's callback must not lock, wait or allocate; it reads the
/// feed with [`SampleReader::read`], which plays silence when the feed runs
/// short and counts it. When the device ends on its own, the backend drops
/// the feed, so the producer sees it closed.
pub trait PlaybackStream: Send {
    /// Whether the device still plays; never blocks. Required, like
    /// [`AudioStream::state`].
    fn state(&self) -> CaptureState;

    /// Stop playing and release the device.
    fn stop(self: Box<Self>) -> Result<(), CaptureError>;
}

/// A running screen capture delivering frames until stopped.
///
/// Dropping a stream without calling [`VideoStream::stop`] still stops it
/// and releases system resources, but discards the report.
pub trait VideoStream: Send {
    /// Newest captured frame (Bgrx8 or premultiplied Bgra8); frames the
    /// consumer does not take in time are replaced and counted.
    fn frames(&self) -> Arc<FrameMailbox>;

    fn source(&self) -> &VideoSourceInfo;

    /// Whether the capture still delivers; never blocks. Required, like
    /// [`AudioStream::state`].
    fn state(&self) -> CaptureState;

    /// Stop capturing and return diagnostics for the whole session.
    fn stop(self: Box<Self>) -> Result<VideoCaptureReport, CaptureError>;
}

/// Screen capture from the running system.
///
/// Probes block for the requested duration plus any user interaction the
/// platform requires (e.g. a portal selection dialog); implementations bound
/// every wait on system services with a timeout.
pub trait ScreenCapture: Send {
    /// Query the screen capability from the running system. Never fails:
    /// unavailable capabilities are reported with a reason.
    fn capability(&mut self) -> ScreenCaptureCapability;

    /// Screen sources that can be captured by id, for backends whose
    /// capability has `target_list`.
    fn screen_sources(&mut self) -> Result<Vec<ScreenSource>, CaptureError>;

    fn probe_screen(
        &mut self,
        request: &ScreenCaptureRequest,
    ) -> Result<VideoCaptureReport, CaptureError>;

    /// Capture the selected screen source continuously (the named one, or
    /// the one the user picks).
    fn start_screen_stream(
        &mut self,
        selection: &ScreenSelection,
    ) -> Result<Box<dyn VideoStream>, CaptureError>;
}

/// Audio capture and playback from the running system.
///
/// Probes block for the requested duration plus any user interaction the
/// platform requires; implementations bound every wait on system services
/// with a timeout.
pub trait AudioDevices: Send {
    /// Query the audio capability from the running system. Never fails:
    /// unavailable capabilities are reported with a reason.
    fn capability(&mut self) -> AudioCaptureCapability;

    fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError>;

    /// Audio output devices, for backends that can play.
    fn audio_outputs(&mut self) -> Result<Vec<AudioOutput>, CaptureError>;

    fn probe_audio(
        &mut self,
        request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError>;

    /// Capture an audio input continuously in the requested format.
    fn start_audio_stream(
        &mut self,
        request: &AudioStreamRequest,
    ) -> Result<Box<dyn AudioStream>, CaptureError>;

    /// Play `feed` on an output in the requested format. On an error the
    /// feed is dropped, which closes it.
    fn start_playback(
        &mut self,
        request: &PlaybackRequest,
        feed: SampleReader,
    ) -> Result<Box<dyn PlaybackStream>, CaptureError>;
}

/// Camera capture from the running system; implementations bound every wait
/// on system services with a timeout.
pub trait CameraCapture: Send {
    /// Query the camera capability from the running system. Never fails:
    /// unavailable capabilities are reported with a reason.
    fn capability(&mut self) -> CameraCaptureCapability;
    fn cameras(&mut self) -> Result<Vec<Camera>, CaptureError>;
    /// A camera stream is a `VideoStream`, like a screen capture.
    fn start_camera_stream(
        &mut self,
        request: &CameraRequest,
    ) -> Result<Box<dyn VideoStream>, CaptureError>;
}

/// The capture parts of one system; the engine drives only these.
pub struct Capture {
    pub screen: Box<dyn ScreenCapture>,
    pub audio: Box<dyn AudioDevices>,
    pub camera: Box<dyn CameraCapture>,
}

impl Capture {
    /// Every part unsupported with `reason` (for engines and builds without capture).
    pub fn unsupported(reason: impl Into<String>) -> Self {
        let part = UnsupportedCapture::new(reason);
        Self {
            screen: Box::new(part.clone()),
            audio: Box::new(part.clone()),
            camera: Box::new(part),
        }
    }

    /// Like `UnsupportedCapture::for_current_platform()` for every part.
    pub fn for_unsupported_platform() -> Self {
        let part = UnsupportedCapture::for_current_platform();
        Self {
            screen: Box::new(part.clone()),
            audio: Box::new(part.clone()),
            camera: Box::new(part),
        }
    }

    /// The capabilities as `get_capture_capabilities` reports them.
    ///
    /// Screen and audio are queried in parallel, so a slow system service
    /// costs one query timeout, not two. Audio stays on the calling thread:
    /// cpal's WASAPI host keeps its device enumerator in the COM apartment
    /// of the first thread that uses it, which must outlive the query.
    /// `desktop` and `session_type` come from `XDG_CURRENT_DESKTOP` and
    /// `XDG_SESSION_TYPE` on every OS. Camera is not part of this report yet.
    pub fn capabilities(&mut self) -> CaptureCapabilities {
        let (screen, audio) = std::thread::scope(|scope| {
            let screen = scope.spawn(|| self.screen.capability());
            let audio = self.audio.capability();
            let screen = screen.join().unwrap_or_else(|_| {
                ScreenCaptureCapability::unavailable(None, "the screen capability query panicked")
            });
            (screen, audio)
        });
        CaptureCapabilities {
            platform: std::env::consts::OS.to_owned(),
            desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
            session_type: std::env::var("XDG_SESSION_TYPE").ok(),
            screen,
            audio,
        }
    }
}

/// Backend for builds without a capture implementation; every operation fails
/// with [`crate::CaptureErrorKind::Unsupported`] and the given reason.
#[derive(Debug, Clone)]
pub struct UnsupportedCapture {
    reason: String,
}

impl UnsupportedCapture {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// Reason naming the current OS, for builds without a capture backend.
    pub fn for_current_platform() -> Self {
        Self::new(format!(
            "capture is not implemented on {}; Linux, Windows and macOS are supported",
            std::env::consts::OS
        ))
    }

    fn error(&self) -> CaptureError {
        CaptureError::unsupported(self.reason.clone())
    }
}

impl ScreenCapture for UnsupportedCapture {
    fn capability(&mut self) -> ScreenCaptureCapability {
        ScreenCaptureCapability::unavailable(None, self.reason.clone())
    }

    fn screen_sources(&mut self) -> Result<Vec<ScreenSource>, CaptureError> {
        Err(self.error())
    }

    fn probe_screen(
        &mut self,
        _request: &ScreenCaptureRequest,
    ) -> Result<VideoCaptureReport, CaptureError> {
        Err(self.error())
    }

    fn start_screen_stream(
        &mut self,
        _selection: &ScreenSelection,
    ) -> Result<Box<dyn VideoStream>, CaptureError> {
        Err(self.error())
    }
}

impl AudioDevices for UnsupportedCapture {
    fn capability(&mut self) -> AudioCaptureCapability {
        AudioCaptureCapability::unavailable(None, self.reason.clone())
    }

    fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError> {
        Err(self.error())
    }

    fn audio_outputs(&mut self) -> Result<Vec<AudioOutput>, CaptureError> {
        Err(self.error())
    }

    fn probe_audio(
        &mut self,
        _request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError> {
        Err(self.error())
    }

    fn start_audio_stream(
        &mut self,
        _request: &AudioStreamRequest,
    ) -> Result<Box<dyn AudioStream>, CaptureError> {
        Err(self.error())
    }

    fn start_playback(
        &mut self,
        _request: &PlaybackRequest,
        _feed: SampleReader,
    ) -> Result<Box<dyn PlaybackStream>, CaptureError> {
        Err(self.error())
    }
}

impl CameraCapture for UnsupportedCapture {
    fn capability(&mut self) -> CameraCaptureCapability {
        CameraCaptureCapability::unavailable(None, self.reason.clone())
    }

    fn cameras(&mut self) -> Result<Vec<Camera>, CaptureError> {
        Err(self.error())
    }

    fn start_camera_stream(
        &mut self,
        _request: &CameraRequest,
    ) -> Result<Box<dyn VideoStream>, CaptureError> {
        Err(self.error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CaptureErrorKind, CursorMode, DEFAULT_PROBE_DURATION, VideoSourceKind};

    #[test]
    fn unsupported_backend_reports_reason_everywhere() {
        let mut backend = UnsupportedCapture::new("not here");
        let mut capture = Capture::unsupported("not here");
        let caps = capture.capabilities();
        assert_eq!(caps.platform, std::env::consts::OS);
        assert_eq!(caps.desktop, std::env::var("XDG_CURRENT_DESKTOP").ok());
        assert_eq!(caps.session_type, std::env::var("XDG_SESSION_TYPE").ok());
        assert!(!caps.screen.available);
        assert_eq!(caps.screen.unavailable_reason.as_deref(), Some("not here"));
        assert_eq!(
            caps.audio,
            AudioCaptureCapability::unavailable(None, "not here")
        );
        assert_eq!(ScreenCapture::capability(&mut backend), caps.screen);
        assert_eq!(AudioDevices::capability(&mut backend), caps.audio);
        assert_eq!(
            backend
                .probe_audio(&AudioCaptureRequest {
                    input: None,
                    duration: DEFAULT_PROBE_DURATION,
                })
                .unwrap_err()
                .message,
            "not here"
        );
        let selection = ScreenSelection {
            target_kinds: vec![VideoSourceKind::Monitor],
            cursor_mode: CursorMode::Embedded,
            capture_target_id: None,
        };
        let Err(err) = backend.start_screen_stream(&selection) else {
            panic!("screen started");
        };
        assert_eq!(err.message, "not here");
        let Err(err) = backend.start_audio_stream(&AudioStreamRequest {
            input: None,
            rate: 48_000,
            channels: 2,
        }) else {
            panic!("audio started");
        };
        assert_eq!(err.message, "not here");

        let err = backend
            .probe_screen(&ScreenCaptureRequest {
                selection: ScreenSelection {
                    target_kinds: vec![VideoSourceKind::Monitor],
                    cursor_mode: CursorMode::Embedded,
                    capture_target_id: None,
                },
                duration: DEFAULT_PROBE_DURATION,
            })
            .unwrap_err();
        assert_eq!(err.kind, CaptureErrorKind::Unsupported);
        assert_eq!(err.message, "not here");
        assert_eq!(
            backend.audio_inputs().unwrap_err().kind,
            CaptureErrorKind::Unsupported
        );
        assert_eq!(
            backend.screen_sources().unwrap_err().kind,
            CaptureErrorKind::Unsupported
        );
        assert_eq!(
            backend.audio_outputs().unwrap_err().kind,
            CaptureErrorKind::Unsupported
        );
        let (writer, reader) = media_core::sample_ring(2, 480);
        let request = PlaybackRequest {
            output: None,
            rate: 48_000,
            channels: 2,
        };
        let Err(err) = backend.start_playback(&request, reader) else {
            panic!("playback started");
        };
        assert_eq!(err.message, "not here");
        let camera = CameraCapture::capability(&mut backend);
        assert_eq!(
            camera,
            CameraCaptureCapability::unavailable(None, "not here")
        );
        assert_eq!(backend.cameras().unwrap_err().message, "not here");
        let Err(err) = backend.start_camera_stream(&CameraRequest {
            camera_id: "camera".into(),
            size: None,
            fps: None,
        }) else {
            panic!("camera started");
        };
        assert_eq!(err.message, "not here");
        // The refused feed is closed.
        assert!(writer.is_closed());
    }

    #[test]
    fn capture_takes_screen_and_audio_from_their_parts() {
        let mut capture = Capture {
            screen: Box::new(UnsupportedCapture::new("no screen here")),
            audio: Box::new(UnsupportedCapture::new("no audio here")),
            ..Capture::unsupported("no camera here")
        };
        let caps = capture.capabilities();
        assert_eq!(caps.platform, std::env::consts::OS);
        assert_eq!(caps.desktop, std::env::var("XDG_CURRENT_DESKTOP").ok());
        assert_eq!(caps.session_type, std::env::var("XDG_SESSION_TYPE").ok());
        assert_eq!(
            caps.screen.unavailable_reason.as_deref(),
            Some("no screen here")
        );
        assert_eq!(
            caps.audio.unavailable_reason.as_deref(),
            Some("no audio here")
        );
        assert_eq!(
            capture.audio.audio_inputs().unwrap_err().message,
            "no audio here"
        );
        assert_eq!(
            capture.screen.screen_sources().unwrap_err().message,
            "no screen here"
        );
    }

    struct TestAudio(Box<dyn FnMut() -> AudioCaptureCapability + Send>);

    impl AudioDevices for TestAudio {
        fn capability(&mut self) -> AudioCaptureCapability {
            (self.0)()
        }

        fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError> {
            Err(CaptureError::unsupported("unused"))
        }

        fn audio_outputs(&mut self) -> Result<Vec<AudioOutput>, CaptureError> {
            Err(CaptureError::unsupported("unused"))
        }

        fn probe_audio(
            &mut self,
            _request: &AudioCaptureRequest,
        ) -> Result<AudioCaptureReport, CaptureError> {
            Err(CaptureError::unsupported("unused"))
        }

        fn start_audio_stream(
            &mut self,
            _request: &AudioStreamRequest,
        ) -> Result<Box<dyn AudioStream>, CaptureError> {
            Err(CaptureError::unsupported("unused"))
        }

        fn start_playback(
            &mut self,
            _request: &PlaybackRequest,
            _feed: SampleReader,
        ) -> Result<Box<dyn PlaybackStream>, CaptureError> {
            Err(CaptureError::unsupported("unused"))
        }
    }

    #[test]
    fn panicking_screen_query_preserves_audio_capability() {
        let mut capture = Capture {
            screen: Box::new(TestScreen(Box::new(|| panic!("test screen panic")))),
            ..Capture::unsupported("audio reason")
        };
        let caps = capture.capabilities();
        assert_eq!(
            caps.screen,
            ScreenCaptureCapability::unavailable(None, "the screen capability query panicked")
        );
        assert_eq!(
            caps.audio,
            AudioCaptureCapability::unavailable(None, "audio reason")
        );
    }

    struct TestScreen(Box<dyn FnMut() -> ScreenCaptureCapability + Send>);

    impl ScreenCapture for TestScreen {
        fn capability(&mut self) -> ScreenCaptureCapability {
            (self.0)()
        }

        fn screen_sources(&mut self) -> Result<Vec<ScreenSource>, CaptureError> {
            UnsupportedCapture::new("unused").screen_sources()
        }

        fn probe_screen(
            &mut self,
            request: &ScreenCaptureRequest,
        ) -> Result<VideoCaptureReport, CaptureError> {
            UnsupportedCapture::new("unused").probe_screen(request)
        }

        fn start_screen_stream(
            &mut self,
            selection: &ScreenSelection,
        ) -> Result<Box<dyn VideoStream>, CaptureError> {
            UnsupportedCapture::new("unused").start_screen_stream(selection)
        }
    }

    #[test]
    fn capabilities_query_in_parallel_with_audio_on_the_calling_thread() {
        use std::sync::mpsc::sync_channel;
        use std::time::Duration;

        let caller = std::thread::current().id();
        let (audio_started, wait_audio) = sync_channel(1);
        let (screen_started, wait_screen) = sync_channel(1);
        let mut capture = Capture {
            screen: Box::new(TestScreen(Box::new(move || {
                assert_ne!(std::thread::current().id(), caller);
                screen_started.send(()).expect("screen started");
                wait_audio
                    .recv_timeout(Duration::from_secs(5))
                    .expect("audio runs alongside screen");
                ScreenCaptureCapability::unavailable(None, "test screen")
            }))),
            audio: Box::new(TestAudio(Box::new(move || {
                assert_eq!(std::thread::current().id(), caller);
                audio_started.send(()).expect("audio started");
                wait_screen
                    .recv_timeout(Duration::from_secs(5))
                    .expect("screen runs alongside audio");
                AudioCaptureCapability::unavailable(None, "test audio")
            }))),
            ..Capture::unsupported("no camera")
        };
        let caps = capture.capabilities();
        assert_eq!(
            caps.screen,
            ScreenCaptureCapability::unavailable(None, "test screen")
        );
        assert_eq!(
            caps.audio,
            AudioCaptureCapability::unavailable(None, "test audio")
        );
    }
}
