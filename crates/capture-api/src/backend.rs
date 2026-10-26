use std::sync::Arc;

use media_core::{AudioRing, FrameMailbox, SampleReader};

use crate::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioInput, AudioOutput,
    AudioStreamRequest, CaptureCapabilities, CaptureError, CaptureState, PlaybackRequest,
    ScreenCaptureCapability, ScreenCaptureRequest, ScreenSelection, ScreenSource,
    VideoCaptureReport, VideoSourceInfo,
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

/// A platform capture implementation.
///
/// Probes block for the requested duration plus any user interaction the
/// platform requires (e.g. a portal selection dialog); implementations bound
/// every wait on system services with a timeout.
pub trait CaptureBackend {
    /// Query capabilities from the running system. Never fails: unavailable
    /// capabilities are reported with a reason.
    fn capabilities(&mut self) -> CaptureCapabilities;

    fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError>;

    /// Screen sources that can be captured by id, for backends whose
    /// capability has `target_list`; the default has none.
    fn screen_sources(&mut self) -> Result<Vec<ScreenSource>, CaptureError> {
        Err(CaptureError::unsupported(
            "this capture backend cannot list screen sources",
        ))
    }

    fn probe_screen(
        &mut self,
        request: &ScreenCaptureRequest,
    ) -> Result<VideoCaptureReport, CaptureError>;

    fn probe_audio(
        &mut self,
        request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError>;

    /// Capture the selected screen source continuously (the named one, or
    /// the one the user picks).
    fn start_screen_stream(
        &mut self,
        selection: &ScreenSelection,
    ) -> Result<Box<dyn VideoStream>, CaptureError>;

    /// Capture an audio input continuously in the requested format.
    fn start_audio_stream(
        &mut self,
        request: &AudioStreamRequest,
    ) -> Result<Box<dyn AudioStream>, CaptureError>;

    /// Audio output devices, for backends that can play; the default has
    /// none.
    fn audio_outputs(&mut self) -> Result<Vec<AudioOutput>, CaptureError> {
        Err(CaptureError::unsupported(
            "this backend cannot play audio yet",
        ))
    }

    /// Play `feed` on an output in the requested format. On an error the
    /// feed is dropped, which closes it.
    fn start_playback(
        &mut self,
        _request: &PlaybackRequest,
        _feed: SampleReader,
    ) -> Result<Box<dyn PlaybackStream>, CaptureError> {
        Err(CaptureError::unsupported(
            "this backend cannot play audio yet",
        ))
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

impl CaptureBackend for UnsupportedCapture {
    fn capabilities(&mut self) -> CaptureCapabilities {
        CaptureCapabilities {
            platform: std::env::consts::OS.to_owned(),
            desktop: None,
            session_type: None,
            screen: ScreenCaptureCapability::unavailable(None, self.reason.clone()),
            audio: AudioCaptureCapability::unavailable(None, self.reason.clone()),
        }
    }

    fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError> {
        Err(self.error())
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

    fn probe_audio(
        &mut self,
        _request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError> {
        Err(self.error())
    }

    fn start_screen_stream(
        &mut self,
        _selection: &ScreenSelection,
    ) -> Result<Box<dyn VideoStream>, CaptureError> {
        Err(self.error())
    }

    fn start_audio_stream(
        &mut self,
        _request: &AudioStreamRequest,
    ) -> Result<Box<dyn AudioStream>, CaptureError> {
        Err(self.error())
    }

    fn audio_outputs(&mut self) -> Result<Vec<AudioOutput>, CaptureError> {
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

/// Screen capture from one backend and audio capture from another, e.g.
/// native screen capture with `cpal` audio.
#[derive(Debug)]
pub struct SplitCapture<V, A> {
    pub video: V,
    pub audio: A,
}

impl<V: CaptureBackend, A: CaptureBackend> CaptureBackend for SplitCapture<V, A> {
    fn capabilities(&mut self) -> CaptureCapabilities {
        let video = self.video.capabilities();
        let audio = self.audio.capabilities();
        CaptureCapabilities {
            platform: video.platform,
            desktop: video.desktop.or(audio.desktop),
            session_type: video.session_type.or(audio.session_type),
            screen: video.screen,
            audio: audio.audio,
        }
    }

    fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError> {
        self.audio.audio_inputs()
    }

    fn screen_sources(&mut self) -> Result<Vec<ScreenSource>, CaptureError> {
        self.video.screen_sources()
    }

    fn probe_screen(
        &mut self,
        request: &ScreenCaptureRequest,
    ) -> Result<VideoCaptureReport, CaptureError> {
        self.video.probe_screen(request)
    }

    fn probe_audio(
        &mut self,
        request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError> {
        self.audio.probe_audio(request)
    }

    fn start_screen_stream(
        &mut self,
        selection: &ScreenSelection,
    ) -> Result<Box<dyn VideoStream>, CaptureError> {
        self.video.start_screen_stream(selection)
    }

    fn start_audio_stream(
        &mut self,
        request: &AudioStreamRequest,
    ) -> Result<Box<dyn AudioStream>, CaptureError> {
        self.audio.start_audio_stream(request)
    }

    fn audio_outputs(&mut self) -> Result<Vec<AudioOutput>, CaptureError> {
        self.audio.audio_outputs()
    }

    fn start_playback(
        &mut self,
        request: &PlaybackRequest,
        feed: SampleReader,
    ) -> Result<Box<dyn PlaybackStream>, CaptureError> {
        self.audio.start_playback(request, feed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CaptureErrorKind, CursorMode, DEFAULT_PROBE_DURATION, VideoSourceKind};

    #[test]
    fn unsupported_backend_reports_reason_everywhere() {
        let mut backend = UnsupportedCapture::new("not here");
        let caps = backend.capabilities();
        assert_eq!(caps.platform, std::env::consts::OS);
        assert!(!caps.screen.available);
        assert_eq!(caps.screen.unavailable_reason.as_deref(), Some("not here"));
        assert!(!caps.audio.available);

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
        // The refused feed is closed.
        assert!(writer.is_closed());
    }

    #[test]
    fn split_capture_takes_screen_and_audio_from_their_backends() {
        let mut split = SplitCapture {
            video: UnsupportedCapture::new("no screen here"),
            audio: UnsupportedCapture::new("no audio here"),
        };
        let caps = split.capabilities();
        assert_eq!(
            caps.screen.unavailable_reason.as_deref(),
            Some("no screen here")
        );
        assert_eq!(
            caps.audio.unavailable_reason.as_deref(),
            Some("no audio here")
        );
        assert_eq!(split.audio_inputs().unwrap_err().message, "no audio here");
        assert_eq!(
            split.screen_sources().unwrap_err().message,
            "no screen here"
        );
    }
}
