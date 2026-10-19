use std::sync::Arc;

use media_core::FrameMailbox;

use crate::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioInput,
    CaptureCapabilities, CaptureError, ScreenCaptureCapability, ScreenCaptureRequest,
    ScreenSelection, VideoCaptureReport, VideoSourceInfo,
};

/// A running screen capture delivering frames until stopped.
///
/// Dropping a stream without calling [`VideoStream::stop`] still stops it
/// and releases system resources, but discards the report.
pub trait VideoStream: Send {
    /// Newest captured frame (Bgrx8 or premultiplied Bgra8); frames the
    /// consumer does not take in time are replaced and counted.
    fn frames(&self) -> Arc<FrameMailbox>;

    fn source(&self) -> &VideoSourceInfo;

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

    fn probe_screen(
        &mut self,
        request: &ScreenCaptureRequest,
    ) -> Result<VideoCaptureReport, CaptureError>;

    fn probe_audio(
        &mut self,
        request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError>;

    /// Let the user pick a screen source and capture it continuously.
    fn start_screen_stream(
        &mut self,
        selection: &ScreenSelection,
    ) -> Result<Box<dyn VideoStream>, CaptureError>;
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

    /// Reason naming the current OS and the milestone that adds support.
    pub fn for_current_platform() -> Self {
        Self::new(format!(
            "capture is not implemented on {} yet; Linux is supported, Windows and macOS backends are planned for M9",
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
                    source_kinds: vec![VideoSourceKind::Monitor],
                    cursor: CursorMode::Embedded,
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
    }
}
