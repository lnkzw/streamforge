//! Frame delivery and diagnostics shared by native screen backends whose
//! system API calls back with mapped frames (Windows.Graphics.Capture,
//! ScreenCaptureKit).
//!
//! The backend's callback maps a frame and hands its bytes to
//! [`FrameSink::publish`], which copies them into one of a few pooled
//! buffers and publishes the result to the stream's mailbox. When no pooled
//! buffer is free the frame is discarded and counted. The sink also keeps
//! the statistics, the negotiated format, why the system ended the capture,
//! and the first few delivery errors, and turns them into the report.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use media_core::{FrameMailbox, FramePool, MediaTimestamp, PixelFormat, VideoFrame};

use crate::{
    BufferKind, CaptureEnd, CaptureState, FrameObservation, Rational, VideoCaptureReport,
    VideoFormatInfo, VideoSourceInfo, VideoStatsAccumulator,
};

/// Frames in flight per stream (mailbox, compositor, one being filled).
const POOL_BUFFERS: usize = 4;
/// Delivery errors kept for the report.
const MAX_ERRORS: usize = 8;

/// One mapped frame: `height` rows of `stride` bytes, 4 bytes per pixel.
#[derive(Debug, Clone, Copy)]
pub struct MappedFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: &'a [u8],
}

#[derive(Default)]
struct State {
    stats: VideoStatsAccumulator,
    format: Option<VideoFormatInfo>,
    errors: Vec<String>,
}

/// See the module docs. Shared (`&self`) between the system's callback
/// thread and the stream handle.
pub struct FrameSink {
    mailbox: Arc<FrameMailbox>,
    pool: FramePool,
    deliver: bool,
    started: Instant,
    /// Backend pixel format name for the report, e.g. `BGRA8`.
    pixel_format: &'static str,
    /// Frames that reach the compositor (`Bgrx8`: captured alpha is not
    /// meaningful for a layer).
    format: PixelFormat,
    state: Mutex<State>,
    /// Outside `state`, so reading it never waits for a callback.
    end: CaptureEnd,
}

impl FrameSink {
    /// `deliver: false` (probes) only observes: frames are neither mapped
    /// nor copied.
    pub fn new(deliver: bool, pixel_format: &'static str) -> Self {
        Self {
            mailbox: Arc::new(FrameMailbox::new()),
            pool: FramePool::new(POOL_BUFFERS),
            deliver,
            started: Instant::now(),
            pixel_format,
            format: PixelFormat::Bgrx8,
            state: Mutex::new(State::default()),
            end: CaptureEnd::new(),
        }
    }

    pub fn mailbox(&self) -> Arc<FrameMailbox> {
        Arc::clone(&self.mailbox)
    }

    /// Whether frames should be mapped and passed to [`Self::publish`].
    pub fn delivers(&self) -> bool {
        self.deliver
    }

    /// Nanoseconds since the sink was created; the arrival time of a callback.
    pub fn elapsed_ns(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Copy a mapped frame into a pooled buffer and publish it. `Ok(false)`
    /// means no pooled buffer was free. Allocates only while the pool warms
    /// up or when the frame size grows.
    pub fn publish(&self, frame: MappedFrame<'_>, pts_ns: Option<i64>) -> Result<bool, String> {
        let needed = VideoFrame::required_len(frame.width, frame.height, frame.stride)
            .map_err(|err| err.to_string())?;
        if frame.data.len() < needed {
            return Err(format!(
                "the mapped frame holds {} bytes, {}x{} needs {needed}",
                frame.data.len(),
                frame.width,
                frame.height
            ));
        }
        let Some(mut pooled) = self.pool.acquire(needed) else {
            return Ok(false);
        };
        pooled.copy_from_slice(&frame.data[..needed]);
        let pts = pts_ns.and_then(|ns| MediaTimestamp::new(ns, 1, 1_000_000_000).ok());
        let frame = VideoFrame::new(
            frame.width,
            frame.height,
            frame.stride,
            self.format,
            pts,
            pooled,
        )
        .map_err(|err| err.to_string())?;
        self.mailbox.publish(frame);
        Ok(true)
    }

    /// Record a frame with image data. `delivered` is the outcome of
    /// [`Self::publish`] when the sink delivers, `None` when it only
    /// observes.
    pub fn observe_frame(
        &self,
        arrival_ns: u64,
        width: u32,
        height: u32,
        pts_ns: Option<i64>,
        delivered: Option<Result<bool, String>>,
    ) {
        let mut state = self.lock();
        state.stats.observe(FrameObservation {
            arrival_ns,
            pts_ns,
            seq: None,
            kind: BufferKind::Frame,
        });
        let changed = state
            .format
            .as_ref()
            .is_none_or(|format| (format.width, format.height) != (width, height));
        if changed {
            state.format = Some(VideoFormatInfo {
                pixel_format: self.pixel_format.to_owned(),
                width,
                height,
                // Frames arrive when the content changes.
                framerate: Rational { num: 0, den: 1 },
            });
        }
        match delivered {
            Some(Ok(false)) => state.stats.frame_discarded(),
            Some(Err(error)) => {
                state.stats.frame_discarded();
                if state.errors.len() < MAX_ERRORS {
                    state.errors.push(error);
                }
            }
            Some(Ok(true)) | None => {}
        }
    }

    /// Record an update without image data (e.g. nothing changed).
    pub fn observe_empty(&self, arrival_ns: u64) {
        self.lock().stats.observe(FrameObservation {
            arrival_ns,
            pts_ns: None,
            seq: None,
            kind: BufferKind::Empty,
        });
    }

    /// The system ended the capture (e.g. the window closed). The first
    /// reason is kept.
    pub fn end(&self, reason: impl Into<String>) {
        self.end.end(|| reason.into());
    }

    pub fn ended(&self) -> bool {
        self.end.reason().is_some()
    }

    /// The capture's state for [`crate::VideoStream::state`]; never waits.
    pub fn state(&self) -> CaptureState {
        self.end.state()
    }

    /// The report for a capture of `elapsed`, with the backend's own
    /// `warnings` first.
    pub fn report(
        &self,
        backend: &str,
        source: VideoSourceInfo,
        buffer_memory: &str,
        mut warnings: Vec<String>,
        elapsed: Duration,
    ) -> VideoCaptureReport {
        if let Some(reason) = self.end.reason() {
            warnings.push(format!("the capture ended early: {reason}"));
        }
        let state = self.lock();
        warnings.extend(
            state
                .errors
                .iter()
                .map(|error| format!("frame not delivered: {error}")),
        );
        VideoCaptureReport {
            backend: backend.to_owned(),
            source,
            format: state.format.clone(),
            buffer_memory: Some(buffer_memory.to_owned()),
            stats: state.stats.finish(elapsed),
            warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(data: &[u8], stride: usize) -> MappedFrame<'_> {
        MappedFrame {
            width: 2,
            height: 2,
            stride,
            data,
        }
    }

    #[test]
    fn published_frames_keep_their_stride_and_reach_the_mailbox() {
        let sink = FrameSink::new(true, "BGRA8");
        // Two rows of 2 pixels with 4 bytes of padding each.
        let data: Vec<u8> = (0..24).collect();
        assert_eq!(sink.publish(frame(&data, 12), Some(1_000)), Ok(true));
        let published = sink.mailbox().take().expect("frame");
        assert_eq!((published.width(), published.height()), (2, 2));
        assert_eq!(published.stride(), 12);
        assert_eq!(published.format(), PixelFormat::Bgrx8);
        assert_eq!(published.row(1)[..8], data[12..20]);
        assert_eq!(
            published.pts(),
            MediaTimestamp::new(1_000, 1, 1_000_000_000).ok()
        );
    }

    #[test]
    fn short_buffers_are_errors_and_a_full_pool_discards() {
        let sink = FrameSink::new(true, "BGRA8");
        let err = sink.publish(frame(&[0; 8], 8), None).unwrap_err();
        assert!(err.contains("needs 16"), "{err}");
        // Keep every pooled buffer busy: published frames hold theirs.
        let data = [0u8; 16];
        let mut held = Vec::new();
        for _ in 0..POOL_BUFFERS {
            assert_eq!(sink.publish(frame(&data, 8), None), Ok(true));
            held.push(sink.mailbox().take().expect("frame"));
        }
        assert_eq!(sink.publish(frame(&data, 8), None), Ok(false));
    }

    #[test]
    fn the_report_counts_outcomes_and_says_why_capture_ended() {
        let sink = FrameSink::new(true, "BGRA8");
        sink.observe_frame(0, 1920, 1080, Some(0), Some(Ok(true)));
        sink.observe_frame(10, 1920, 1080, Some(16), Some(Ok(false)));
        sink.observe_frame(20, 1280, 720, Some(33), Some(Err("lost".into())));
        sink.observe_empty(30);
        assert!(!sink.ended());
        assert_eq!(sink.state(), CaptureState::Running);
        sink.end("the window closed");
        sink.end("a later reason");
        assert!(sink.ended());
        assert_eq!(
            sink.state(),
            CaptureState::Ended {
                reason: "the window closed".to_owned()
            }
        );
        let report = sink.report(
            "test",
            VideoSourceInfo {
                kind: None,
                width: None,
                height: None,
            },
            "memory",
            vec!["first".into()],
            Duration::from_secs(1),
        );
        assert_eq!(report.stats.frames, 3);
        assert_eq!(report.stats.empty_buffers, 1);
        assert_eq!(report.stats.frames_discarded, 2);
        let format = report.format.expect("format");
        assert_eq!((format.width, format.height), (1280, 720));
        assert_eq!(
            report.warnings,
            [
                "first",
                "the capture ended early: the window closed",
                "frame not delivered: lost"
            ]
        );
    }
}
