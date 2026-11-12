//! The preview consumer of the pipeline's fan-out.
//!
//! The render thread copies every second frame (30 FPS) into a pooled
//! canvas-sized buffer and hands it to the preview thread, which scales it
//! down to the size the viewer asked for and publishes it to a newest-wins
//! mailbox. Scaling a large preview takes a few milliseconds; on its own
//! thread it never delays the render, and so never the outputs. A preview
//! that falls behind drops its own frames, counted. A new size reaches the
//! running thread as a new scaler: the render thread's buffers stay.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use command_api::CommandError;
use media_core::{FrameMailbox, FramePool, VideoFrame};
use video_compositor::Downscaler;

use crate::composite::{CANVAS_HEIGHT, CANVAS_WIDTH};
use crate::pipeline::{internal, lock};

/// Preview frames are every second rendered frame (30 FPS).
pub(crate) const PREVIEW_EVERY: u64 = 2;
/// The preview's size when the viewer asks for none: a quarter of the
/// canvas in each dimension.
pub(crate) const DEFAULT_PREVIEW_SIZE: (u32, u32) = (CANVAS_WIDTH / 4, CANVAS_HEIGHT / 4);
/// Canvas copies: one waiting for the preview thread, one being scaled.
const COPIES: usize = 2;
/// Scaled frames: the newest in the mailbox, one the viewer draws, one
/// being written.
const PREVIEW_BUFFERS: usize = 3;

/// The preview frame size for a viewer that wants at most `width`×`height`:
/// the canvas's aspect ratio fitted inside, no larger than the canvas, at
/// least 1×1.
pub(crate) fn fit_preview_size(width: u32, height: u32) -> (u32, u32) {
    let (canvas_width, canvas_height) = (u64::from(CANVAS_WIDTH), u64::from(CANVAS_HEIGHT));
    let (width, height) = (
        u64::from(width).min(canvas_width),
        u64::from(height).min(canvas_height),
    );
    // Rounded to the nearest pixel; never above the limit, since the other
    // side limits at most as much.
    let (width, height) = if width * canvas_height <= height * canvas_width {
        (
            width,
            (width * canvas_height + canvas_width / 2) / canvas_width,
        )
    } else {
        (
            (height * canvas_width + canvas_height / 2) / canvas_height,
            height,
        )
    };
    (width.max(1) as u32, height.max(1) as u32)
}

/// The scaler from the canvas to a preview `size` that fits it (see
/// [`fit_preview_size`]); it allocates its tables, so build it off the
/// render thread.
pub(crate) fn preview_scaler(size: (u32, u32)) -> Result<Downscaler, CommandError> {
    Downscaler::new(CANVAS_WIDTH, CANVAS_HEIGHT, size.0, size.1)
        .map_err(|error| internal(format!("cannot size the preview: {error}")))
}

/// A running preview: what the render thread holds in the fan-out. Its
/// thread and buffers last as long as the preview; a new size only swaps
/// the scaler.
pub(crate) struct PreviewOut {
    frames: SyncSender<VideoFrame>,
    copies: FramePool,
    /// A scaler for a new size, taken by the preview thread before its
    /// next frame.
    resize: Arc<Mutex<Option<Downscaler>>>,
    /// No free copy, or the preview thread still busy with the last frame.
    dropped: u64,
    thread: JoinHandle<(u64, u64)>,
}

impl PreviewOut {
    /// Start the preview thread, publishing frames scaled by `scaler` to
    /// `mailbox`.
    pub(crate) fn start(
        mailbox: Arc<FrameMailbox>,
        scaler: Downscaler,
    ) -> Result<Self, CommandError> {
        let (frames, receive) = sync_channel(1);
        let resize = Arc::new(Mutex::new(None));
        let thread = thread::Builder::new()
            .name("preview".to_owned())
            .spawn({
                let resize = Arc::clone(&resize);
                move || run(&receive, scaler, &resize, &mailbox)
            })
            .map_err(|error| internal(format!("cannot start the preview thread: {error}")))?;
        Ok(Self {
            frames,
            copies: FramePool::new(COPIES),
            resize,
            dropped: 0,
            thread,
        })
    }

    /// Scale with `scaler` from the next frame on. Frames already scaled
    /// keep their size, but none of the old size follows one of the new.
    pub(crate) fn resize(&self, scaler: Downscaler) {
        *lock(&self.resize) = Some(scaler);
    }

    /// On the render thread: hand a copy of `frame` to the preview thread,
    /// or count it as dropped. Allocates only the first copies.
    pub(crate) fn offer(&mut self, frame: &VideoFrame) {
        let Some(mut buffer) = self.copies.acquire(frame.data().len()) else {
            self.dropped += 1;
            return;
        };
        buffer.copy_from_slice(frame.data());
        let copy = match VideoFrame::new(
            frame.width(),
            frame.height(),
            frame.stride(),
            frame.format(),
            frame.pts(),
            buffer,
        ) {
            Ok(copy) => copy,
            Err(_) => {
                self.dropped += 1;
                return;
            }
        };
        match self.frames.try_send(copy) {
            Ok(()) => {}
            // The preview thread still has the last frame waiting, or has
            // ended (it ends only when this is dropped).
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => self.dropped += 1,
        }
    }

    /// Stop the preview thread after the frames it has; returns the frames
    /// published and dropped. Call it without holding the fan-out's lock.
    pub(crate) fn stop(self) -> (u64, u64) {
        let Self {
            frames,
            dropped,
            thread,
            ..
        } = self;
        drop(frames);
        // A panic in the preview thread lost its counts; its frames still
        // stopped.
        let (published, dropped_late) = thread.join().unwrap_or_default();
        (published, dropped + dropped_late)
    }
}

/// The preview thread: scale each frame and publish it, until the sender
/// is dropped. Returns the frames published and dropped (no free buffer:
/// the viewer holds frames).
fn run(
    frames: &Receiver<VideoFrame>,
    mut scaler: Downscaler,
    resize: &Mutex<Option<Downscaler>>,
    mailbox: &FrameMailbox,
) -> (u64, u64) {
    // Buffers grow to a larger size once and are reused after.
    let pool = FramePool::new(PREVIEW_BUFFERS);
    let (mut published, mut dropped) = (0, 0);
    while let Ok(frame) = frames.recv() {
        if let Some(resized) = lock(resize).take() {
            scaler = resized;
        }
        // Only a frame of another size fails, and the canvas never changes.
        match pool
            .acquire(scaler.frame_len())
            .map(|buffer| scaler.scale(&frame, buffer))
        {
            Some(Ok(small)) => {
                mailbox.publish(small);
                published += 1;
            }
            _ => dropped += 1,
        }
    }
    (published, dropped)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use video_compositor::pattern;

    use super::*;

    #[test]
    fn sizes_fit_the_canvas_aspect_and_size() {
        assert_eq!(DEFAULT_PREVIEW_SIZE, (480, 270));
        // Exactly 16:9.
        assert_eq!(fit_preview_size(1280, 720), (1280, 720));
        // Wider or taller than 16:9: the other side limits.
        assert_eq!(fit_preview_size(2000, 720), (1280, 720));
        assert_eq!(fit_preview_size(1280, 2000), (1280, 720));
        // Rounded to the nearest pixel, within the limit.
        assert_eq!(fit_preview_size(1001, 1000), (1001, 563));
        assert_eq!(fit_preview_size(1000, 563), (1000, 563));
        // Never larger than the canvas, never empty.
        assert_eq!(fit_preview_size(3840, 2160), (1920, 1080));
        assert_eq!(fit_preview_size(u32::MAX, u32::MAX), (1920, 1080));
        assert_eq!(fit_preview_size(1, 1), (1, 1));
        assert_eq!(fit_preview_size(1, 1000), (1, 1));
    }

    #[test]
    fn frames_are_scaled_on_the_preview_thread_and_counted() {
        let mailbox = Arc::new(FrameMailbox::new());
        let scaler = preview_scaler((640, 360)).expect("scaler");
        let mut preview = PreviewOut::start(Arc::clone(&mailbox), scaler).expect("start");
        let canvas = pattern::solid(CANVAS_WIDTH, CANVAS_HEIGHT, [1, 2, 3, 255]).expect("canvas");
        let mut offered = 0;
        let small = next_frame(&mut preview, &mailbox, &canvas, &mut offered);
        assert_eq!((small.width(), small.height()), (640, 360));
        assert_eq!(small.pixel(639, 359), [1, 2, 3, 255]);
        drop(small);

        // A new size: once a frame has it, none of the old size follows.
        preview.resize(preview_scaler((320, 180)).expect("scaler"));
        let mut seen_new = 0;
        while seen_new < 5 {
            let frame = next_frame(&mut preview, &mailbox, &canvas, &mut offered);
            let size = (frame.width(), frame.height());
            if size == (320, 180) {
                seen_new += 1;
            } else {
                assert_eq!((seen_new, size), (0, (640, 360)), "old size after new");
            }
        }
        let (published, dropped) = preview.stop();
        assert!(published >= 6, "{published}");
        // Every offered frame was published or dropped, at both sizes.
        assert_eq!(published + dropped, offered);
    }

    /// Offer `canvas` until the preview publishes a frame (bounded).
    fn next_frame(
        preview: &mut PreviewOut,
        mailbox: &FrameMailbox,
        canvas: &VideoFrame,
        offered: &mut u64,
    ) -> VideoFrame {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            preview.offer(canvas);
            *offered += 1;
            if let Some(frame) = mailbox.take() {
                return frame;
            }
            assert!(Instant::now() < deadline, "no preview frame");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_busy_preview_thread_drops_and_counts() {
        let mailbox = Arc::new(FrameMailbox::new());
        let scaler = preview_scaler(DEFAULT_PREVIEW_SIZE).expect("scaler");
        let mut preview = PreviewOut::start(mailbox, scaler).expect("start");
        let canvas = pattern::solid(CANVAS_WIDTH, CANVAS_HEIGHT, [0, 0, 0, 255]).expect("canvas");
        // Back to back, faster than the thread scales: one copy waits, one is
        // being scaled, the rest are dropped.
        let offered = 50;
        for _ in 0..offered {
            preview.offer(&canvas);
        }
        let (published, dropped) = preview.stop();
        assert!(dropped > 0, "nothing dropped");
        assert_eq!(published + dropped, offered);
    }
}
