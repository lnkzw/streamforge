//! Screen capture sessions: portal selection plus a PipeWire stream, either
//! as a fixed-length probe or as a continuous stream on its own thread.

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use capture_api::{
    CaptureEnd, CaptureError, CaptureState, ScreenCaptureCapability, ScreenSelection,
    VideoCaptureReport, VideoSourceInfo, VideoSourceKind, VideoStream,
};
use media_core::{FrameMailbox, FramePool};
use pipewire as pw;

use crate::QUERY_TIMEOUT;
use crate::portal::{self, Cast};
use crate::pw::RunUntil;
use crate::video::{self, Delivery, VideoRun};

/// Upper bound for the user to pick a source in the portal dialog.
const SELECTION_TIMEOUT: Duration = Duration::from_secs(120);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
/// In flight in the capture callback, waiting in the mailbox, held by the
/// consumer, plus one spare.
const STREAM_BUFFERS: usize = 4;

pub(crate) const BACKEND: &str = "xdg-desktop-portal+pipewire";

/// The portal always asks the user and cannot capture a named source.
pub(crate) fn no_target_list() -> CaptureError {
    CaptureError::unsupported(
        "the ScreenCast portal cannot capture a named source (`capture_target_id`); \
         omit it and the user picks one in the system dialog",
    )
}

/// A portal session whose source the user has picked.
struct OpenCast {
    cast: Cast,
    source: VideoSourceInfo,
    warnings: Vec<String>,
}

fn open_cast(selection: &ScreenSelection) -> Result<OpenCast, CaptureError> {
    // Before any portal I/O: a named source can never work here, and the
    // caller may be the engine thread while an output runs.
    if selection.source.is_some() {
        return Err(no_target_list());
    }
    let info = portal::query(QUERY_TIMEOUT)?;
    selection.check_supported(&ScreenCaptureCapability {
        backend: None,
        available: true,
        unavailable_reason: None,
        portal_version: Some(info.version),
        target_kinds: info.target_kinds,
        cursor_modes: info.cursor_modes,
        // The portal always asks the user; it cannot capture a named source.
        picker: true,
        target_list: false,
    })?;
    let mut warnings = Vec::new();
    // ScreenCast v1 has no cursor modes; the requested cursor mode is ignored there.
    let cursor_mode = if info.version >= 2 {
        Some(selection.cursor_mode)
    } else {
        warnings.push(format!(
            "portal interface version {} cannot select a cursor mode; `{}` was ignored",
            info.version,
            selection.cursor_mode.as_str()
        ));
        None
    };
    let cast = portal::start(
        &selection.target_kinds,
        cursor_mode,
        SELECTION_TIMEOUT,
        CLOSE_TIMEOUT,
    )?;
    let source = VideoSourceInfo {
        kind: cast.kind,
        width: cast.size.map(|(w, _)| w),
        height: cast.size.map(|(_, h)| h),
    };
    warnings.extend(source_kind_mismatch(&selection.target_kinds, source.kind));
    Ok(OpenCast {
        cast,
        source,
        warnings,
    })
}

/// End the portal session (always) and assemble the report.
fn finish_cast(
    cast: Cast,
    source: VideoSourceInfo,
    mut warnings: Vec<String>,
    run: Result<VideoRun, CaptureError>,
) -> Result<VideoCaptureReport, CaptureError> {
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
    warnings.append(&mut run.warnings);
    Ok(VideoCaptureReport {
        backend: BACKEND.to_owned(),
        source,
        format: run.format,
        buffer_memory: run.buffer_memory.map(str::to_owned),
        stats: run.stats,
        warnings,
    })
}

fn duplicate_remote(cast: &Cast) -> Result<std::os::fd::OwnedFd, CaptureError> {
    cast.remote
        .try_clone()
        .map_err(|err| CaptureError::backend(format!("cannot duplicate PipeWire fd: {err}")))
}

/// Capture for `duration` on the calling thread, diagnostics only.
pub(crate) fn probe(
    selection: &ScreenSelection,
    duration: Duration,
) -> Result<VideoCaptureReport, CaptureError> {
    let open = open_cast(selection)?;
    let run = duplicate_remote(&open.cast).and_then(|remote| {
        video::capture(
            remote,
            open.cast.node_id,
            open.cast.size,
            RunUntil::Elapsed(duration),
            None,
        )
    });
    finish_cast(open.cast, open.source, open.warnings, run)
}

/// Start a continuous capture on a dedicated thread.
pub(crate) fn start_stream(
    selection: &ScreenSelection,
) -> Result<Box<dyn VideoStream>, CaptureError> {
    let open = open_cast(selection)?;
    let close_on_error = |cast: Cast, error: CaptureError| match cast.close(CLOSE_TIMEOUT) {
        Ok(()) => error,
        Err(close) => error.with_detail("session_close_error", close.message),
    };
    let remote = match duplicate_remote(&open.cast) {
        Ok(remote) => remote,
        Err(error) => return Err(close_on_error(open.cast, error)),
    };
    let mailbox = Arc::new(FrameMailbox::new());
    let delivery = Delivery {
        mailbox: Arc::clone(&mailbox),
        pool: FramePool::new(STREAM_BUFFERS),
    };
    let (stop, stop_receiver) = pw::channel::channel();
    let (node_id, size) = (open.cast.node_id, open.cast.size);
    let ended = Arc::new(CaptureEnd::new());
    let thread = thread::Builder::new()
        .name("screen-capture".to_owned())
        .spawn({
            let ended = Arc::clone(&ended);
            move || {
                let run = video::capture(
                    remote,
                    node_id,
                    size,
                    RunUntil::Stopped(stop_receiver),
                    Some(delivery),
                );
                // Before a stop, this is why the capture ended on its own.
                ended.end(|| match &run {
                    Ok(_) => "the PipeWire stream ended (the portal session closed or the \
                              shared source went away)"
                        .to_owned(),
                    Err(error) => error.message.clone(),
                });
                run
            }
        });
    let thread = match thread {
        Ok(thread) => thread,
        Err(err) => {
            let error = CaptureError::backend(format!("cannot start the capture thread: {err}"));
            return Err(close_on_error(open.cast, error));
        }
    };
    Ok(Box::new(LinuxScreenStream {
        source: open.source,
        mailbox,
        ended,
        running: Some(Running {
            stop,
            thread,
            cast: open.cast,
            warnings: open.warnings,
        }),
    }))
}

struct Running {
    stop: pw::channel::Sender<()>,
    thread: JoinHandle<Result<VideoRun, CaptureError>>,
    cast: Cast,
    warnings: Vec<String>,
}

pub(crate) struct LinuxScreenStream {
    source: VideoSourceInfo,
    mailbox: Arc<FrameMailbox>,
    /// Set when the capture thread ends.
    ended: Arc<CaptureEnd>,
    running: Option<Running>,
}

impl LinuxScreenStream {
    fn shutdown(&mut self) -> Option<Result<VideoCaptureReport, CaptureError>> {
        let running = self.running.take()?;
        // Fails only if the capture thread already ended (and dropped the
        // receiver); joining below reports why.
        let _ = running.stop.send(());
        let run = running
            .thread
            .join()
            .unwrap_or_else(|_| Err(CaptureError::backend("the screen capture thread panicked")));
        Some(finish_cast(
            running.cast,
            self.source.clone(),
            running.warnings,
            run,
        ))
    }
}

impl VideoStream for LinuxScreenStream {
    fn frames(&self) -> Arc<FrameMailbox> {
        Arc::clone(&self.mailbox)
    }

    fn source(&self) -> &VideoSourceInfo {
        &self.source
    }

    fn state(&self) -> CaptureState {
        let panicked = self
            .running
            .as_ref()
            .is_some_and(|running| running.thread.is_finished());
        // A thread that ends normally sets the reason first.
        if panicked {
            self.ended
                .end(|| "the screen capture thread panicked".to_owned());
        }
        self.ended.state()
    }

    fn stop(mut self: Box<Self>) -> Result<VideoCaptureReport, CaptureError> {
        self.shutdown()
            .unwrap_or_else(|| Err(CaptureError::backend("the stream was already stopped")))
    }
}

impl Drop for LinuxScreenStream {
    fn drop(&mut self) {
        // Stop sharing and join the thread even if the owner never called
        // `stop`; the report is not needed then.
        let _ = self.shutdown();
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
