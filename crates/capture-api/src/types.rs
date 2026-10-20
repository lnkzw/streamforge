use std::time::Duration;

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
    /// The user picks the source in a system dialog (no `source`).
    pub picker: bool,
    /// Sources can be listed ([`ScreenSource`]) and captured by id.
    pub source_list: bool,
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
            picker: false,
            source_list: false,
        }
    }
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
}

/// Which screen source to capture and how the cursor is captured: a
/// [`ScreenSource::id`], or (without one) whatever the user picks in the
/// system dialog among `source_kinds`.
///
/// `source_kinds` is non-empty and duplicate-free, and `source` is not
/// empty; `command_api` enforces this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSelection {
    pub source_kinds: Vec<VideoSourceKind>,
    pub cursor: CursorMode,
    pub source: Option<String>,
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
    /// mode at all (e.g. ScreenCast portal v1); the cursor request is then
    /// not checked and the backend reports that it was ignored.
    ///
    /// A named `source` needs a backend with a source list and defines its
    /// own kind, so `source_kinds` is not checked; without one the backend
    /// needs a picker.
    pub fn check_supported(
        &self,
        capability: &ScreenCaptureCapability,
    ) -> Result<(), CaptureError> {
        match (&self.source, capability.source_list, capability.picker) {
            (Some(_), false, _) => {
                return Err(CaptureError::unsupported(
                    "this screen capture backend cannot capture a named source; \
                     omit `source_id` to pick one in the system dialog",
                ));
            }
            (None, _, false) => {
                return Err(CaptureError::new(
                    CaptureErrorKind::InvalidRequest,
                    "this screen capture backend has no system picker; \
                     name a source (`source_id`) from list_screen_sources",
                ));
            }
            _ => {}
        }
        let missing: Vec<_> = self
            .source_kinds
            .iter()
            .filter(|kind| self.source.is_none() && !capability.source_kinds.contains(kind))
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
        if !capability.cursor_modes.is_empty() && !capability.cursor_modes.contains(&self.cursor) {
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

/// Capture an audio input continuously, converted by the platform to
/// `rate` Hz with `channels` interleaved channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStreamRequest {
    /// [`AudioInput::name`]; `None` uses the system default input.
    pub input: Option<String>,
    pub rate: u32,
    pub channels: u32,
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

    fn capability(kinds: &[VideoSourceKind], cursors: &[CursorMode]) -> ScreenCaptureCapability {
        ScreenCaptureCapability {
            backend: Some("test".into()),
            available: true,
            unavailable_reason: None,
            portal_version: Some(5),
            source_kinds: kinds.to_vec(),
            cursor_modes: cursors.to_vec(),
            picker: true,
            source_list: false,
        }
    }

    fn request(kinds: &[VideoSourceKind], cursor: CursorMode) -> ScreenSelection {
        ScreenSelection {
            source_kinds: kinds.to_vec(),
            cursor,
            source: None,
        }
    }

    #[test]
    fn named_sources_need_a_source_list_and_pickers_a_picker() {
        let picker_only = capability(&[VideoSourceKind::Monitor], &[]);
        let mut named = request(&[VideoSourceKind::Window], CursorMode::Embedded);
        named.source = Some("monitor:1".into());
        let err = named.check_supported(&picker_only).unwrap_err();
        assert_eq!(err.kind, CaptureErrorKind::Unsupported);
        assert!(err.message.contains("source_id"), "{}", err.message);

        let list_only = ScreenCaptureCapability {
            picker: false,
            source_list: true,
            ..picker_only
        };
        // The named source defines its kind; `source_kinds` is not checked.
        assert_eq!(named.check_supported(&list_only), Ok(()));
        let err = request(&[VideoSourceKind::Monitor], CursorMode::Embedded)
            .check_supported(&list_only)
            .unwrap_err();
        assert_eq!(err.kind, CaptureErrorKind::InvalidRequest);
        assert!(
            err.message.contains("list_screen_sources"),
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
}
