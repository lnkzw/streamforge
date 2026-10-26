//! XDG desktop portal ScreenCast access.
//!
//! The portal shows the compositor's source picker and hands out a PipeWire
//! file descriptor plus node ID for the selected source. All D-Bus calls are
//! bounded by timeouts.

use std::future::Future;
use std::os::fd::OwnedFd;
use std::time::Duration;

use ashpd::desktop::screencast::{
    CursorMode as PortalCursorMode, Screencast, SelectSourcesOptions, SourceType,
};
use ashpd::desktop::{PersistMode, ResponseError, Session};
use ashpd::enumflags2::BitFlags;
use ashpd::{Error as PortalApiError, PortalError};
use capture_api::{CaptureError, CaptureErrorKind, CursorMode, VideoSourceKind};

const HYPRLAND_HINT: &str = "install xdg-desktop-portal and a backend with ScreenCast support \
     (Hyprland: xdg-desktop-portal-hyprland; see docs/capture-linux.md)";

pub(crate) struct PortalInfo {
    pub version: u32,
    pub target_kinds: Vec<VideoSourceKind>,
    pub cursor_modes: Vec<CursorMode>,
}

/// Read ScreenCast portal properties without starting a session.
pub(crate) fn query(timeout: Duration) -> Result<PortalInfo, CaptureError> {
    block_on_timeout(timeout, "xdg-desktop-portal", async {
        let proxy = Screencast::new()
            .await
            .map_err(|err| map_error("cannot open the ScreenCast portal", err))?;
        let kinds = proxy
            .available_source_types()
            .await
            .map_err(|err| map_error("cannot read AvailableSourceTypes", err))?;
        let cursors = match proxy.available_cursor_modes().await {
            Ok(cursors) => cursors,
            // Interface versions below 2 have no cursor modes.
            Err(PortalApiError::RequiresVersion(_, _)) => BitFlags::empty(),
            Err(err) => return Err(map_error("cannot read AvailableCursorModes", err)),
        };
        Ok(PortalInfo {
            version: proxy.version(),
            target_kinds: target_kinds(kinds),
            cursor_modes: cursor_modes(cursors),
        })
    })
}

/// A started screen cast; close it with [`Cast::close`].
pub(crate) struct Cast {
    // Carries its own D-Bus proxy, so the ScreenCast proxy is not kept.
    session: Session<Screencast>,
    pub node_id: u32,
    pub kind: Option<VideoSourceKind>,
    pub size: Option<(u32, u32)>,
    pub remote: OwnedFd,
}

struct Selection {
    node_id: u32,
    kind: Option<VideoSourceKind>,
    size: Option<(u32, u32)>,
    remote: OwnedFd,
}

/// Create a session, let the user pick one source and open its PipeWire remote.
///
/// `cursor_mode` is `None` for portals that cannot select a cursor mode (v1).
/// The session is closed again on every failure after it was created,
/// including cancellation and `selection_timeout`; ashpd shares one D-Bus
/// connection per process, so an unclosed session would otherwise keep the
/// compositor sharing until the process exits.
pub(crate) fn start(
    kinds: &[VideoSourceKind],
    cursor_mode: Option<CursorMode>,
    selection_timeout: Duration,
    close_timeout: Duration,
) -> Result<Cast, CaptureError> {
    let sources = kinds
        .iter()
        .map(|kind| portal_source_type(*kind))
        .collect::<BitFlags<_>>();
    let (proxy, session) =
        block_on_timeout(close_timeout, "creating a screen cast session", async {
            let proxy = Screencast::new()
                .await
                .map_err(|err| map_error("cannot open the ScreenCast portal", err))?;
            let session = proxy
                .create_session(Default::default())
                .await
                .map_err(|err| map_error("cannot create a screen cast session", err))?;
            Ok((proxy, session))
        })?;
    let selection = block_on_timeout(
        selection_timeout,
        "the screen cast portal dialog",
        select(&proxy, &session, sources, cursor_mode),
    );
    match selection {
        Ok(selection) => Ok(Cast {
            session,
            node_id: selection.node_id,
            kind: selection.kind,
            size: selection.size,
            remote: selection.remote,
        }),
        Err(error) => Err(match close_session(&session, close_timeout) {
            Ok(()) => error,
            Err(close) => error.with_detail("session_close_error", close.message),
        }),
    }
}

async fn select(
    proxy: &Screencast,
    session: &Session<Screencast>,
    sources: BitFlags<SourceType>,
    cursor_mode: Option<CursorMode>,
) -> Result<Selection, CaptureError> {
    proxy
        .select_sources(
            session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor_mode.map(portal_cursor_mode))
                .set_sources(sources)
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await
        .and_then(|request| request.response())
        .map_err(|err| map_error("cannot select screen cast sources", err))?;
    let streams = proxy
        .start(session, None, Default::default())
        .await
        .and_then(|request| request.response())
        .map_err(|err| map_error("screen cast was not started", err))?;
    let stream = streams.streams().first().cloned().ok_or_else(|| {
        CaptureError::backend("the portal started a screen cast without any stream")
    })?;
    let remote = proxy
        .open_pipe_wire_remote(session, Default::default())
        .await
        .map_err(|err| map_error("cannot open the PipeWire remote", err))?;
    let size = stream
        .size()
        .and_then(|(w, h)| Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?)));
    Ok(Selection {
        node_id: stream.pipe_wire_node_id(),
        kind: stream.source_type().map(video_source_kind),
        size,
        remote,
    })
}

impl Cast {
    /// End the screen cast session so the compositor stops sharing.
    pub fn close(self, timeout: Duration) -> Result<(), CaptureError> {
        close_session(&self.session, timeout)
    }
}

fn close_session(session: &Session<Screencast>, timeout: Duration) -> Result<(), CaptureError> {
    block_on_timeout(timeout, "closing the screen cast session", async {
        session
            .close()
            .await
            .map_err(|err| map_error("cannot close the screen cast session", err))
    })
}

fn block_on_timeout<T>(
    timeout: Duration,
    what: &str,
    future: impl Future<Output = Result<T, CaptureError>>,
) -> Result<T, CaptureError> {
    let message = format!("{what} did not respond within {} s", timeout.as_secs());
    async_io::block_on(futures_lite::future::or(future, async move {
        async_io::Timer::after(timeout).await;
        Err(CaptureError::timeout(message))
    }))
}

fn map_error(context: &str, error: PortalApiError) -> CaptureError {
    match error {
        PortalApiError::Response(ResponseError::Cancelled)
        | PortalApiError::Portal(PortalError::Cancelled(_)) => CaptureError::new(
            CaptureErrorKind::Cancelled,
            format!("{context}: the selection dialog was cancelled"),
        ),
        PortalApiError::Portal(PortalError::NotAllowed(message)) => CaptureError::new(
            CaptureErrorKind::PermissionDenied,
            format!("{context}: access denied by the portal: {message}"),
        ),
        PortalApiError::PortalNotFound(interface) => CaptureError::unavailable(format!(
            "{context}: no portal implements {interface}; {HYPRLAND_HINT}"
        )),
        PortalApiError::RequiresVersion(required, available) => CaptureError::unsupported(format!(
            "{context}: needs portal interface version {required}, found {available}"
        )),
        PortalApiError::Zbus(err) => CaptureError::unavailable(format!(
            "{context}: D-Bus error: {err}; is a desktop session with a D-Bus session bus running? {HYPRLAND_HINT}"
        )),
        other => CaptureError::backend(format!("{context}: {other}")),
    }
}

fn target_kinds(flags: BitFlags<SourceType>) -> Vec<VideoSourceKind> {
    flags.iter().map(video_source_kind).collect()
}

fn cursor_modes(flags: BitFlags<PortalCursorMode>) -> Vec<CursorMode> {
    flags
        .iter()
        .map(|mode| match mode {
            PortalCursorMode::Hidden => CursorMode::Hidden,
            PortalCursorMode::Embedded => CursorMode::Embedded,
            PortalCursorMode::Metadata => CursorMode::Metadata,
        })
        .collect()
}

fn video_source_kind(source: SourceType) -> VideoSourceKind {
    match source {
        SourceType::Monitor => VideoSourceKind::Monitor,
        SourceType::Window => VideoSourceKind::Window,
        SourceType::Virtual => VideoSourceKind::Virtual,
    }
}

fn portal_source_type(kind: VideoSourceKind) -> SourceType {
    match kind {
        VideoSourceKind::Monitor => SourceType::Monitor,
        VideoSourceKind::Window => SourceType::Window,
        VideoSourceKind::Virtual => SourceType::Virtual,
    }
}

fn portal_cursor_mode(mode: CursorMode) -> PortalCursorMode {
    match mode {
        CursorMode::Hidden => PortalCursorMode::Hidden,
        CursorMode::Embedded => PortalCursorMode::Embedded,
        CursorMode::Metadata => PortalCursorMode::Metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_bitmask_seven_means_all_target_kinds() {
        let flags = BitFlags::<SourceType>::from_bits(7).expect("valid bits");
        assert_eq!(target_kinds(flags), VideoSourceKind::ALL.to_vec());
    }

    #[test]
    fn cursor_bitmask_three_means_hidden_and_embedded() {
        let flags = BitFlags::<PortalCursorMode>::from_bits(3).expect("valid bits");
        assert_eq!(
            cursor_modes(flags),
            vec![CursorMode::Hidden, CursorMode::Embedded]
        );
    }

    #[test]
    fn source_kind_mapping_round_trips() {
        for kind in VideoSourceKind::ALL {
            assert_eq!(video_source_kind(portal_source_type(kind)), kind);
        }
    }

    #[test]
    fn cancellation_maps_to_cancelled() {
        let err = map_error("x", PortalApiError::Response(ResponseError::Cancelled));
        assert_eq!(err.kind, CaptureErrorKind::Cancelled);
    }

    #[test]
    fn timeout_wins_over_a_pending_future() {
        let err = block_on_timeout(Duration::from_millis(20), "test", async {
            futures_lite::future::pending::<Result<(), CaptureError>>().await
        })
        .unwrap_err();
        assert_eq!(err.kind, CaptureErrorKind::Timeout);
    }
}
