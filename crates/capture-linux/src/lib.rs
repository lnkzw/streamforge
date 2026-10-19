//! Linux capture backend: XDG desktop portal ScreenCast + PipeWire.
//!
//! - Screen: the portal shows the compositor's picker and hands out a PipeWire
//!   node; frames arrive as shared-memory buffers with optional header metadata.
//! - Audio: PipeWire source nodes are enumerated through the registry and
//!   captured as interleaved F32 at their native rate and channel count.
//!
//! Operations run a PipeWire main loop on the calling thread for a bounded
//! time; callers must not invoke them from a UI thread. See
//! `docs/capture-linux.md` for system requirements (Hyprland in particular).
#![cfg(target_os = "linux")]

mod audio;
mod formats;
mod portal;
mod pw;
mod video;

use std::time::Duration;

use capture_api::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioInput, CaptureBackend,
    CaptureCapabilities, CaptureError, CaptureErrorKind, ScreenCaptureCapability,
    ScreenCaptureRequest, VideoCaptureReport, VideoSourceInfo, VideoSourceKind,
};
use serde_json::json;

/// Upper bound for each capability query. Screen and audio are queried in
/// parallel, so `capabilities()` (and the first `get_status`) waits at most
/// this long.
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
/// Upper bound for the user to pick a source in the portal dialog.
const SELECTION_TIMEOUT: Duration = Duration::from_secs(120);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

const SCREEN_BACKEND: &str = "xdg-desktop-portal+pipewire";

#[derive(Debug, Default)]
pub struct LinuxCapture {
    _private: (),
}

impl LinuxCapture {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CaptureBackend for LinuxCapture {
    fn capabilities(&mut self) -> CaptureCapabilities {
        let (portal, audio) = std::thread::scope(|scope| {
            let portal = scope.spawn(|| portal::query(QUERY_TIMEOUT));
            // PipeWire objects are !Send; they stay on this thread.
            let audio = pw::server_version(QUERY_TIMEOUT);
            let portal = portal.join().unwrap_or_else(|_| {
                Err(CaptureError::backend(
                    "the portal capability query panicked",
                ))
            });
            (portal, audio)
        });
        let screen = match portal {
            Ok(info) => ScreenCaptureCapability {
                backend: Some(SCREEN_BACKEND.to_owned()),
                available: !info.source_kinds.is_empty(),
                unavailable_reason: info
                    .source_kinds
                    .is_empty()
                    .then(|| "the ScreenCast portal advertises no source types".to_owned()),
                portal_version: Some(info.version),
                source_kinds: info.source_kinds,
                cursor_modes: info.cursor_modes,
            },
            Err(err) => ScreenCaptureCapability::unavailable(Some(SCREEN_BACKEND), err.message),
        };
        let audio = match audio {
            Ok(version) => AudioCaptureCapability {
                backend: Some(pw::BACKEND.to_owned()),
                available: true,
                unavailable_reason: None,
                server_version: Some(version),
            },
            Err(err) => AudioCaptureCapability::unavailable(Some(pw::BACKEND), err.message),
        };
        CaptureCapabilities {
            platform: std::env::consts::OS.to_owned(),
            desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
            session_type: std::env::var("XDG_SESSION_TYPE").ok(),
            screen,
            audio,
        }
    }

    fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError> {
        pw::audio_inputs(QUERY_TIMEOUT)
    }

    fn probe_screen(
        &mut self,
        request: &ScreenCaptureRequest,
    ) -> Result<VideoCaptureReport, CaptureError> {
        let info = portal::query(QUERY_TIMEOUT)?;
        request.check_supported(&ScreenCaptureCapability {
            backend: None,
            available: true,
            unavailable_reason: None,
            portal_version: Some(info.version),
            source_kinds: info.source_kinds,
            cursor_modes: info.cursor_modes,
        })?;

        let mut warnings = Vec::new();
        // ScreenCast v1 has no cursor modes; the request's cursor is ignored there.
        let cursor = if info.version >= 2 {
            Some(request.cursor)
        } else {
            warnings.push(format!(
                "portal interface version {} cannot select a cursor mode; `{}` was ignored",
                info.version,
                request.cursor.as_str()
            ));
            None
        };
        let cast = portal::start(
            &request.source_kinds,
            cursor,
            SELECTION_TIMEOUT,
            CLOSE_TIMEOUT,
        )?;
        let source = VideoSourceInfo {
            kind: cast.kind,
            width: cast.size.map(|(w, _)| w),
            height: cast.size.map(|(_, h)| h),
        };
        let run = cast
            .remote
            .try_clone()
            .map_err(|err| CaptureError::backend(format!("cannot duplicate PipeWire fd: {err}")))
            .and_then(|remote| video::capture(remote, cast.node_id, cast.size, request.duration));
        // Always end the portal session, even if capturing failed.
        let closed = cast.close(CLOSE_TIMEOUT);
        let mut run = match (run, closed) {
            (Ok(run), Ok(())) => run,
            (Ok(mut run), Err(close)) => {
                run.warnings.push(close.message);
                run
            }
            (Err(error), Ok(())) => return Err(error),
            (Err(error), Err(close)) => {
                return Err(error.with_detail("session_close_error", close.message));
            }
        };
        warnings.extend(source_kind_mismatch(&request.source_kinds, source.kind));
        warnings.append(&mut run.warnings);
        run.warnings = warnings;
        Ok(VideoCaptureReport {
            backend: SCREEN_BACKEND.to_owned(),
            source,
            format: run.format,
            buffer_memory: run.buffer_memory.map(str::to_owned),
            stats: run.stats,
            warnings: run.warnings,
        })
    }

    fn probe_audio(
        &mut self,
        request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError> {
        if let Some(name) = request.input.as_deref() {
            // Check first: PipeWire would silently fall back to the default
            // input for an unknown target.
            let inputs = pw::audio_inputs(QUERY_TIMEOUT)?;
            if !inputs.iter().any(|input| input.name == name) {
                return Err(CaptureError::new(
                    CaptureErrorKind::NotFound,
                    format!("audio input `{name}` not found; see list_audio_inputs"),
                )
                .with_details(json!({
                    "input": name,
                    "available": inputs.iter().map(|input| &input.name).collect::<Vec<_>>(),
                })));
            }
        }
        let run = audio::capture(request.input.as_deref(), request.duration)?;
        Ok(AudioCaptureReport {
            backend: pw::BACKEND.to_owned(),
            input: request.input.clone(),
            format: run.format,
            stats: run.stats,
            warnings: run.warnings,
        })
    }
}

/// Some portal pickers (e.g. xdph) offer every source type regardless of the
/// requested ones; make a mismatch visible instead of silently reporting it.
fn source_kind_mismatch(
    requested: &[VideoSourceKind],
    delivered: Option<VideoSourceKind>,
) -> Option<String> {
    let delivered = delivered?;
    if requested.contains(&delivered) {
        return None;
    }
    let requested: Vec<_> = requested.iter().map(|kind| kind.as_str()).collect();
    Some(format!(
        "requested source type(s) {} but the portal delivered a {} source; \
         the picker does not restrict the offered types",
        requested.join(", "),
        delivered.as_str()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatched_source_kind_is_reported() {
        let warning =
            source_kind_mismatch(&[VideoSourceKind::Window], Some(VideoSourceKind::Monitor))
                .expect("warning");
        assert!(
            warning.contains("window") && warning.contains("monitor"),
            "{warning}"
        );
    }

    #[test]
    fn matching_or_unknown_source_kind_is_silent() {
        let requested = [VideoSourceKind::Monitor, VideoSourceKind::Window];
        assert_eq!(
            source_kind_mismatch(&requested, Some(VideoSourceKind::Window)),
            None
        );
        assert_eq!(source_kind_mismatch(&requested, None), None);
    }
}
