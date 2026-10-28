use std::sync::OnceLock;
use std::time::Duration;

pub use media_core::Rational;
use media_core::SampleReader;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{AudioStats, CaptureError, CaptureErrorKind, VideoStats};

/// Shortest accepted probe duration.
pub const MIN_PROBE_DURATION: Duration = Duration::from_millis(100);
/// Longest accepted probe duration; keeps blocking probes bounded.
pub const MAX_PROBE_DURATION: Duration = Duration::from_secs(60);
pub const DEFAULT_PROBE_DURATION: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
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
    pub target_kinds: Vec<VideoSourceKind>,
    pub cursor_modes: Vec<CursorMode>,
    /// The user picks the source in a system dialog (no `source`).
    pub picker: bool,
    /// Sources can be listed ([`ScreenSource`]) and captured by id.
    pub target_list: bool,
}

impl ScreenCaptureCapability {
    pub fn unavailable(backend: Option<&str>, reason: impl Into<String>) -> Self {
        Self {
            backend: backend.map(str::to_owned),
            available: false,
            unavailable_reason: Some(reason.into()),
            portal_version: None,
            target_kinds: Vec::new(),
            cursor_modes: Vec::new(),
            picker: false,
            target_list: false,
        }
    }
}

/// Whether cameras can be listed and captured, and through what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CameraCaptureCapability {
    pub backend: Option<String>,
    pub available: bool,
    pub unavailable_reason: Option<String>,
}

impl CameraCaptureCapability {
    pub fn unavailable(backend: Option<&str>, reason: impl Into<String>) -> Self {
        Self {
            backend: backend.map(str::to_owned),
            available: false,
            unavailable_reason: Some(reason.into()),
        }
    }
}

/// A camera that can be captured by its id (ADR 0019).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Camera {
    pub camera_id: String,
    pub name: String,
    pub modes: Vec<CameraMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CameraMode {
    pub width: u32,
    pub height: u32,
    pub fps: Rational,
}

/// What to capture from a camera; without a size or rate the backend chooses
/// (ADR 0019 "Mode choice").
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CameraRequest {
    pub camera_id: String,
    /// Width and height.
    pub size: Option<(u32, u32)>,
    pub fps: Option<Rational>,
}

/// A screen source that can be captured by its [`ScreenSource::id`]
/// without a picker. Names (e.g. window titles) can reveal private
/// content, so listing them needs the same access as capturing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScreenSource {
    /// Backend-specific id, valid while the source exists (e.g. `monitor:1`).
    pub id: String,
    pub kind: VideoSourceKind,
    /// Display name or window title.
    pub name: String,
    /// Size as the system reports it (macOS: points, not pixels). For
    /// windows this is approximate: the window rectangle can include
    /// invisible borders and differ from the captured frames with display
    /// scaling. Capture reports carry the real frame size.
    pub width: Option<u32>,
    pub height: Option<u32>,
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
    /// The [`AudioOutput::name`] whose playback this input captures (e.g.
    /// the monitor of a PipeWire sink), if the backend knows: playing the
    /// monitor there would feed back (ADR 0017).
    pub loopback_of: Option<String>,
}

/// An audio output device, e.g. for the monitor (ADR 0017).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioOutput {
    /// Stable identifier used to select this output (PipeWire `node.name`).
    pub name: String,
    pub description: Option<String>,
    /// Backend-specific class, e.g. `Audio/Sink`.
    pub media_class: String,
}

/// Play interleaved `f32` samples on an output, converted by the platform
/// from `rate` Hz with `channels` channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackRequest {
    /// [`AudioOutput::name`]; `None` uses the system default output.
    pub output: Option<String>,
    pub rate: u32,
    pub channels: u32,
}

impl PlaybackRequest {
    /// Whether `feed` can be played as requested: 1 or 2 channels at a
    /// non-zero rate (else `unsupported`), and the feed carries the
    /// requested channels (else `invalid_request`). Backends check this
    /// before anything else, as it costs nothing.
    pub fn check(&self, feed: &SampleReader) -> Result<(), CaptureError> {
        if !(1..=2).contains(&self.channels) || self.rate == 0 {
            return Err(CaptureError::unsupported(format!(
                "playback supports 1 or 2 channels at a non-zero rate, got {} Hz / {}",
                self.rate, self.channels
            )));
        }
        if feed.channels() != self.channels as usize {
            return Err(CaptureError::new(
                CaptureErrorKind::InvalidRequest,
                format!(
                    "the feed carries {} channel(s), the request {}",
                    feed.channels(),
                    self.channels
                ),
            ));
        }
        Ok(())
    }
}

/// Which capture target to capture and how the cursor is captured: a
/// [`ScreenSource::id`], or (without one) whatever the user picks in the
/// system dialog among `target_kinds`.
///
/// `target_kinds` is non-empty and duplicate-free, and `capture_target_id`
/// is not empty; `command_api` enforces this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSelection {
    pub target_kinds: Vec<VideoSourceKind>,
    pub cursor_mode: CursorMode,
    pub capture_target_id: Option<String>,
}

/// Capture one user-selected screen source for `duration` and report diagnostics.
///
/// `duration` lies within [`MIN_PROBE_DURATION`]..=[`MAX_PROBE_DURATION`];
/// `command_api` enforces this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenCaptureRequest {
    pub selection: ScreenSelection,
    pub duration: Duration,
}

impl ScreenSelection {
    /// Reject source kinds or cursor modes the backend did not advertise.
    /// An empty `cursor_modes` list means the backend cannot select a cursor
    /// mode at all (e.g. ScreenCast portal v1); the cursor mode request is then
    /// not checked and the backend reports that it was ignored.
    ///
    /// A named `source` needs a backend with a source list and defines its
    /// own kind, so `target_kinds` is not checked; without one the backend
    /// needs a picker.
    pub fn check_supported(
        &self,
        capability: &ScreenCaptureCapability,
    ) -> Result<(), CaptureError> {
        if !capability.available {
            return Err(CaptureError::unsupported(
                capability
                    .unavailable_reason
                    .clone()
                    .unwrap_or_else(|| "screen capture is not available".to_owned()),
            ));
        }
        match (
            &self.capture_target_id,
            capability.target_list,
            capability.picker,
        ) {
            (Some(_), false, _) => {
                return Err(CaptureError::unsupported(
                    "this screen capture backend cannot capture a named source; \
                     omit `capture_target_id` to pick one in the system dialog",
                ));
            }
            (None, _, false) => {
                return Err(CaptureError::new(
                    CaptureErrorKind::InvalidRequest,
                    "this screen capture backend has no system picker; \
                     name a source (`capture_target_id`) from list_capture_targets",
                ));
            }
            _ => {}
        }
        let missing: Vec<_> = self
            .target_kinds
            .iter()
            .filter(|kind| {
                self.capture_target_id.is_none() && !capability.target_kinds.contains(kind)
            })
            .map(|kind| kind.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(CaptureError::unsupported(format!(
                "screen capture backend does not offer target kind(s): {}",
                missing.join(", ")
            ))
            .with_details(json!({
                "requested": self.target_kinds,
                "available": capability.target_kinds,
            })));
        }
        if !capability.cursor_modes.is_empty()
            && !capability.cursor_modes.contains(&self.cursor_mode)
        {
            return Err(CaptureError::unsupported(format!(
                "screen capture backend does not offer cursor mode `{}`",
                self.cursor_mode.as_str()
            ))
            .with_details(json!({
                "requested": self.cursor_mode,
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

/// Capture an audio input continuously, converted by the platform to
/// `rate` Hz with `channels` interleaved channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStreamRequest {
    /// [`AudioInput::name`]; `None` uses the system default input.
    pub input: Option<String>,
    pub rate: u32,
    pub channels: u32,
}

/// Whether a running capture still delivers (see [`crate::VideoStream::state`]).
///
/// "Stalled" is deliberately not a state here: some systems deliver frames
/// only when the content changes, so a quiet source is not a failure.
/// Consumers see stalls from the frame age instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CaptureState {
    Running,
    /// The system ended the capture (a window closed, a device was removed,
    /// a permission was revoked); it delivers nothing more until restarted.
    Ended {
        reason: String,
    },
}

/// Why a capture ended on its own: set once, by whichever thread notices
/// first, and read without waiting. Streams build their
/// [`crate::VideoStream::state`] from it.
#[derive(Debug, Default)]
pub struct CaptureEnd(OnceLock<String>);

impl CaptureEnd {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record why the capture ended; the first reason is kept, and later
    /// calls do not run `reason`.
    pub fn end(&self, reason: impl FnOnce() -> String) {
        let _ = self.0.get_or_init(reason);
    }

    pub fn reason(&self) -> Option<&str> {
        self.0.get().map(String::as_str)
    }

    pub fn state(&self) -> CaptureState {
        match self.reason() {
            Some(reason) => CaptureState::Ended {
                reason: reason.to_owned(),
            },
            None => CaptureState::Running,
        }
    }
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
    pub sample_rate_hz: u32,
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

    fn capability(kinds: &[VideoSourceKind], cursors: &[CursorMode]) -> ScreenCaptureCapability {
        ScreenCaptureCapability {
            backend: Some("test".into()),
            available: true,
            unavailable_reason: None,
            portal_version: Some(5),
            target_kinds: kinds.to_vec(),
            cursor_modes: cursors.to_vec(),
            picker: true,
            target_list: false,
        }
    }

    fn request(kinds: &[VideoSourceKind], cursor_mode: CursorMode) -> ScreenSelection {
        ScreenSelection {
            target_kinds: kinds.to_vec(),
            cursor_mode,
            capture_target_id: None,
        }
    }

    #[test]
    fn unavailable_capture_reports_its_reason() {
        let unavailable = ScreenCaptureCapability::unavailable(None, "no desktop session");
        let err = request(&[VideoSourceKind::Monitor], CursorMode::Embedded)
            .check_supported(&unavailable)
            .unwrap_err();
        assert_eq!(err.kind, CaptureErrorKind::Unsupported);
        assert_eq!(err.message, "no desktop session");
    }

    #[test]
    fn named_sources_need_a_target_list_and_pickers_a_picker() {
        let picker_only = capability(&[VideoSourceKind::Monitor], &[]);
        let mut named = request(&[VideoSourceKind::Window], CursorMode::Embedded);
        named.capture_target_id = Some("monitor:1".into());
        let err = named.check_supported(&picker_only).unwrap_err();
        assert_eq!(err.kind, CaptureErrorKind::Unsupported);
        assert!(err.message.contains("capture_target_id"), "{}", err.message);

        let list_only = ScreenCaptureCapability {
            picker: false,
            target_list: true,
            ..picker_only
        };
        // The named source defines its kind; `target_kinds` is not checked.
        assert_eq!(named.check_supported(&list_only), Ok(()));
        let err = request(&[VideoSourceKind::Monitor], CursorMode::Embedded)
            .check_supported(&list_only)
            .unwrap_err();
        assert_eq!(err.kind, CaptureErrorKind::InvalidRequest);
        assert!(
            err.message.contains("list_capture_targets"),
            "{}",
            err.message
        );
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
    fn backend_without_cursor_selection_accepts_any_cursor_request() {
        let caps = capability(&[VideoSourceKind::Monitor], &[]);
        assert_eq!(
            request(&[VideoSourceKind::Monitor], CursorMode::Metadata).check_supported(&caps),
            Ok(())
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

    #[test]
    fn playback_requests_the_feed_cannot_play_are_refused() {
        let (_writer, stereo) = media_core::sample_ring(2, 64);
        let request = |rate, channels| PlaybackRequest {
            output: None,
            rate,
            channels,
        };
        request(48_000, 2).check(&stereo).expect("plays");
        let kind = |request: PlaybackRequest| request.check(&stereo).unwrap_err().kind;
        assert_eq!(kind(request(48_000, 6)), CaptureErrorKind::Unsupported);
        assert_eq!(kind(request(0, 2)), CaptureErrorKind::Unsupported);
        assert_eq!(kind(request(48_000, 1)), CaptureErrorKind::InvalidRequest);
    }
}
