use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{AudioStats, CaptureError, VideoStats};

/// Shortest accepted probe duration.
pub const MIN_PROBE_DURATION: Duration = Duration::from_millis(100);
/// Longest accepted probe duration; keeps blocking probes bounded.
pub const MAX_PROBE_DURATION: Duration = Duration::from_secs(60);
pub const DEFAULT_PROBE_DURATION: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoSourceKind {
    Monitor,
    Window,
    /// A virtual output created for the capture (e.g. an extended desktop).
    Virtual,
}

impl VideoSourceKind {
    pub const ALL: [Self; 3] = [Self::Monitor, Self::Window, Self::Virtual];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::Window => "window",
            Self::Virtual => "virtual",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorMode {
    /// Cursor is not part of the stream.
    Hidden,
    /// Cursor is drawn into the frames.
    Embedded,
    /// Cursor is delivered as separate stream metadata.
    Metadata,
}

impl CursorMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::Embedded => "embedded",
            Self::Metadata => "metadata",
        }
    }
}

/// Exact rational number, e.g. a frame rate of 60000/1001.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rational {
    pub num: u32,
    pub den: u32,
}

/// Capture capabilities discovered at runtime, never assumed per OS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureCapabilities {
    /// `std::env::consts::OS` of the running build.
    pub platform: String,
    /// Desktop environment as reported by the session, e.g. `Hyprland`.
    pub desktop: Option<String>,
    /// Session type, e.g. `wayland` or `x11`.
    pub session_type: Option<String>,
    pub screen: ScreenCaptureCapability,
    pub audio: AudioCaptureCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScreenCaptureCapability {
    pub backend: Option<String>,
    pub available: bool,
    /// Why capture is unavailable, including a remedy where one is known.
    pub unavailable_reason: Option<String>,
    pub portal_version: Option<u32>,
    pub source_kinds: Vec<VideoSourceKind>,
    pub cursor_modes: Vec<CursorMode>,
}

impl ScreenCaptureCapability {
    pub fn unavailable(backend: Option<&str>, reason: impl Into<String>) -> Self {
        Self {
            backend: backend.map(str::to_owned),
            available: false,
            unavailable_reason: Some(reason.into()),
            portal_version: None,
            source_kinds: Vec::new(),
            cursor_modes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioCaptureCapability {
    pub backend: Option<String>,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    /// Version reported by the audio server, if any.
    pub server_version: Option<String>,
}

impl AudioCaptureCapability {
    pub fn unavailable(backend: Option<&str>, reason: impl Into<String>) -> Self {
        Self {
            backend: backend.map(str::to_owned),
            available: false,
            unavailable_reason: Some(reason.into()),
            server_version: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioInput {
    /// Stable identifier used to select this input (PipeWire `node.name`).
    pub name: String,
    pub description: Option<String>,
    /// Backend-specific class, e.g. `Audio/Source`.
    pub media_class: String,
}

/// Capture one user-selected screen source for `duration` and report diagnostics.
///
/// `source_kinds` is non-empty and duplicate-free; `duration` lies within
/// [`MIN_PROBE_DURATION`]..=[`MAX_PROBE_DURATION`]. `command_api` enforces this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenCaptureRequest {
    pub source_kinds: Vec<VideoSourceKind>,
    pub cursor: CursorMode,
    pub duration: Duration,
}

impl ScreenCaptureRequest {
    /// Reject source kinds or cursor modes the backend did not advertise.
    pub fn check_supported(
        &self,
        capability: &ScreenCaptureCapability,
    ) -> Result<(), CaptureError> {
        let missing: Vec<_> = self
            .source_kinds
            .iter()
            .filter(|kind| !capability.source_kinds.contains(kind))
            .map(|kind| kind.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(CaptureError::unsupported(format!(
                "screen capture backend does not offer source type(s): {}",
                missing.join(", ")
            ))
            .with_details(json!({
                "requested": self.source_kinds,
                "available": capability.source_kinds,
            })));
        }
        if !capability.cursor_modes.contains(&self.cursor) {
            return Err(CaptureError::unsupported(format!(
                "screen capture backend does not offer cursor mode `{}`",
                self.cursor.as_str()
            ))
            .with_details(json!({
                "requested": self.cursor,
                "available": capability.cursor_modes,
            })));
        }
        Ok(())
    }
}

/// Capture one audio input for `duration` and report diagnostics. No audio is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioCaptureRequest {
    /// [`AudioInput::name`] to capture; `None` uses the system default input.
    pub input: Option<String>,
    pub duration: Duration,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VideoSourceInfo {
    pub kind: Option<VideoSourceKind>,
    /// Size reported by the portal before format negotiation.
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VideoFormatInfo {
    /// Backend pixel format name, e.g. `BGRx`.
    pub pixel_format: String,
    pub width: u32,
    pub height: u32,
    /// Negotiated nominal frame rate; `0/1` means variable (damage-driven).
    pub framerate: Rational,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VideoCaptureReport {
    pub backend: String,
    pub source: VideoSourceInfo,
    pub format: Option<VideoFormatInfo>,
    /// Memory type of received buffers, e.g. `memfd` or `dmabuf`.
    pub buffer_memory: Option<String>,
    pub stats: VideoStats,
    /// Non-fatal problems observed during the probe.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioFormatInfo {
    pub sample_format: String,
    pub rate: u32,
    pub channels: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioCaptureReport {
    pub backend: String,
    /// Requested input; `None` means the system default.
    pub input: Option<String>,
    pub format: Option<AudioFormatInfo>,
    pub stats: AudioStats,
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CaptureErrorKind;

    fn capability(kinds: &[VideoSourceKind], cursors: &[CursorMode]) -> ScreenCaptureCapability {
        ScreenCaptureCapability {
            backend: Some("test".into()),
            available: true,
            unavailable_reason: None,
            portal_version: Some(5),
            source_kinds: kinds.to_vec(),
            cursor_modes: cursors.to_vec(),
        }
    }

    fn request(kinds: &[VideoSourceKind], cursor: CursorMode) -> ScreenCaptureRequest {
        ScreenCaptureRequest {
            source_kinds: kinds.to_vec(),
            cursor,
            duration: DEFAULT_PROBE_DURATION,
        }
    }

    #[test]
    fn supported_request_passes() {
        let caps = capability(
            &[VideoSourceKind::Monitor, VideoSourceKind::Window],
            &[CursorMode::Hidden, CursorMode::Embedded],
        );
        assert_eq!(
            request(&[VideoSourceKind::Window], CursorMode::Embedded).check_supported(&caps),
            Ok(())
        );
    }

    #[test]
    fn missing_source_kind_is_unsupported_with_alternatives() {
        let caps = capability(&[VideoSourceKind::Monitor], &[CursorMode::Embedded]);
        let err = request(
            &[VideoSourceKind::Monitor, VideoSourceKind::Window],
            CursorMode::Embedded,
        )
        .check_supported(&caps)
        .unwrap_err();
        assert_eq!(err.kind, CaptureErrorKind::Unsupported);
        assert!(err.message.contains("window"), "{}", err.message);
        assert_eq!(
            err.details.expect("details")["available"],
            json!(["monitor"])
        );
    }

    #[test]
    fn missing_cursor_mode_is_unsupported() {
        let caps = capability(&[VideoSourceKind::Monitor], &[CursorMode::Hidden]);
        let err = request(&[VideoSourceKind::Monitor], CursorMode::Metadata)
            .check_supported(&caps)
            .unwrap_err();
        assert_eq!(err.kind, CaptureErrorKind::Unsupported);
        assert!(err.message.contains("metadata"), "{}", err.message);
    }
}
