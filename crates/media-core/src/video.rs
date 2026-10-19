//! CPU video frames, buffer pooling and the latest-frame graph edge.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::MediaTimestamp;

/// Packed 8-bit-per-channel pixel layouts, named by memory byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat {
    /// B, G, R, A with premultiplied alpha.
    Bgra8,
    /// B, G, R, unused. Always opaque; the fourth byte is ignored.
    Bgrx8,
}

impl PixelFormat {
    pub const BYTES_PER_PIXEL: usize = 4;

    pub fn is_opaque(self) -> bool {
        matches!(self, Self::Bgrx8)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VideoFrameError {
    #[error("frame dimensions must be non-zero, got {width}x{height}")]
    EmptyFrame { width: u32, height: u32 },
    #[error("stride {stride} is smaller than one row ({row} bytes)")]
    StrideTooSmall { stride: usize, row: usize },
    #[error("buffer holds {len} bytes, frame needs {needed}")]
    BufferTooSmall { len: usize, needed: usize },
    #[error("frame size overflows usize")]
    Overflow,
}

/// One CPU-resident video frame. Row `y` starts at `y * stride`.
#[derive(Debug)]
pub struct VideoFrame {
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    pts: Option<MediaTimestamp>,
    data: FrameBuffer,
}

impl VideoFrame {
    pub fn new(
        width: u32,
        height: u32,
        stride: usize,
        format: PixelFormat,
        pts: Option<MediaTimestamp>,
        data: FrameBuffer,
    ) -> Result<Self, VideoFrameError> {
        let needed = Self::required_len(width, height, stride)?;
        if data.len() < needed {
            return Err(VideoFrameError::BufferTooSmall {
                len: data.len(),
                needed,
            });
        }
        Ok(Self {
            width,
            height,
            stride,
            format,
            pts,
            data,
        })
    }

    /// Zero-filled, tightly packed frame in its own allocation.
    pub fn zeroed(width: u32, height: u32, format: PixelFormat) -> Result<Self, VideoFrameError> {
        let stride = Self::packed_stride(width)?;
        let len = Self::required_len(width, height, stride)?;
        Self::new(
            width,
            height,
            stride,
            format,
            None,
            FrameBuffer::unpooled(vec![0; len]),
        )
    }

    /// Bytes per row without padding.
    pub fn packed_stride(width: u32) -> Result<usize, VideoFrameError> {
        usize::try_from(width)
            .ok()
            .and_then(|w| w.checked_mul(PixelFormat::BYTES_PER_PIXEL))
            .ok_or(VideoFrameError::Overflow)
    }

    /// Minimum buffer length for the given geometry.
    pub fn required_len(width: u32, height: u32, stride: usize) -> Result<usize, VideoFrameError> {
        if width == 0 || height == 0 {
            return Err(VideoFrameError::EmptyFrame { width, height });
        }
        let row = Self::packed_stride(width)?;
        if stride < row {
            return Err(VideoFrameError::StrideTooSmall { stride, row });
        }
        let height = usize::try_from(height).map_err(|_| VideoFrameError::Overflow)?;
        stride
            .checked_mul(height - 1)
            .and_then(|rows| rows.checked_add(row))
            .ok_or(VideoFrameError::Overflow)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn format(&self) -> PixelFormat {
        self.format
    }

    pub fn pts(&self) -> Option<MediaTimestamp> {
        self.pts
    }

    pub fn set_pts(&mut self, pts: Option<MediaTimestamp>) {
        self.pts = pts;
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Pixel bytes of row `y` (without stride padding).
    pub fn row(&self, y: u32) -> &[u8] {
        let start = y as usize * self.stride;
        &self.data[start..start + self.width as usize * PixelFormat::BYTES_PER_PIXEL]
    }

    /// The four bytes of pixel (`x`, `y`) in memory order.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let offset = y as usize * self.stride + x as usize * PixelFormat::BYTES_PER_PIXEL;
        [
            self.data[offset],
            self.data[offset + 1],
            self.data[offset + 2],
            self.data[offset + 3],
        ]
    }
}

/// Frame storage that returns itself to its [`FramePool`] when dropped.
#[derive(Debug)]
pub struct FrameBuffer {
    data: Vec<u8>,
    pool: Weak<PoolShared>,
}

impl FrameBuffer {
    /// Storage that is simply freed on drop.
    pub fn unpooled(data: Vec<u8>) -> Self {
        Self {
            data,
            pool: Weak::new(),
        }
    }
}

impl Deref for FrameBuffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.data
    }
}

impl DerefMut for FrameBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl Drop for FrameBuffer {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            let data = std::mem::take(&mut self.data);
            // A poisoned lock only means another holder panicked; the free
            // list itself is still a valid Vec.
            let mut free = pool.free.lock().unwrap_or_else(|e| e.into_inner());
            free.push(data);
        }
    }
}

#[derive(Debug)]
struct PoolShared {
    free: Mutex<Vec<Vec<u8>>>,
    max_buffers: usize,
    allocated: AtomicUsize,
}

/// Bounded set of reusable frame buffers.
///
/// After warm-up (`max_buffers` allocations) acquiring a buffer never
/// allocates unless the requested size grows (e.g. a resolution change).
#[derive(Debug, Clone)]
pub struct FramePool {
    shared: Arc<PoolShared>,
}

impl FramePool {
    pub fn new(max_buffers: usize) -> Self {
        Self {
            shared: Arc::new(PoolShared {
                free: Mutex::new(Vec::with_capacity(max_buffers)),
                max_buffers,
                allocated: AtomicUsize::new(0),
            }),
        }
    }

    /// A buffer of exactly `len` bytes, or `None` if all buffers are in use.
    /// The caller must count `None` as a dropped frame.
    pub fn acquire(&self, len: usize) -> Option<FrameBuffer> {
        let reused = self
            .shared
            .free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop();
        let mut data = match reused {
            Some(data) => data,
            None => {
                let allocated = self.shared.allocated.fetch_add(1, Ordering::Relaxed);
                if allocated >= self.shared.max_buffers {
                    self.shared.allocated.fetch_sub(1, Ordering::Relaxed);
                    return None;
                }
                Vec::new()
            }
        };
        // Only allocates on first use or when frames grow.
        data.resize(len, 0);
        Some(FrameBuffer {
            data,
            pool: Arc::downgrade(&self.shared),
        })
    }

    /// Buffers created so far (in use or free).
    pub fn allocated(&self) -> usize {
        self.shared.allocated.load(Ordering::Relaxed)
    }
}

/// Graph edge with capacity one where the newest frame wins.
///
/// Live video prefers fresh frames over completeness: a frame that is
/// replaced before the consumer takes it is dropped and counted in
/// [`MailboxStats::replaced`].
#[derive(Debug, Default)]
pub struct FrameMailbox {
    slot: Mutex<Option<VideoFrame>>,
    published: AtomicU64,
    replaced: AtomicU64,
    taken: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct MailboxStats {
    pub published: u64,
    /// Frames dropped because a newer one arrived before they were taken.
    pub replaced: u64,
    pub taken: u64,
}

impl FrameMailbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `frame`, dropping any frame that was not taken yet.
    /// Returns `true` if a frame was dropped.
    pub fn publish(&self, frame: VideoFrame) -> bool {
        let previous = self
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(frame);
        self.published.fetch_add(1, Ordering::Relaxed);
        let replaced = previous.is_some();
        if replaced {
            self.replaced.fetch_add(1, Ordering::Relaxed);
        }
        // `previous` (and its pooled buffer) is released after the lock.
        drop(previous);
        replaced
    }

    /// Take the newest frame, if one arrived since the last call.
    pub fn take(&self) -> Option<VideoFrame> {
        let frame = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        if frame.is_some() {
            self.taken.fetch_add(1, Ordering::Relaxed);
        }
        frame
    }

    pub fn stats(&self) -> MailboxStats {
        MailboxStats {
            published: self.published.load(Ordering::Relaxed),
            replaced: self.replaced.load(Ordering::Relaxed),
            taken: self.taken.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(pool: &FramePool, marker: u8) -> VideoFrame {
        let mut data = pool.acquire(16).expect("buffer");
        data[0] = marker;
        VideoFrame::new(2, 2, 8, PixelFormat::Bgrx8, None, data).expect("frame")
    }

    #[test]
    fn geometry_is_validated() {
        assert_eq!(VideoFrame::required_len(2, 2, 8), Ok(16));
        assert_eq!(VideoFrame::required_len(2, 2, 12), Ok(20));
        assert_eq!(
            VideoFrame::required_len(2, 2, 4),
            Err(VideoFrameError::StrideTooSmall { stride: 4, row: 8 })
        );
        assert!(matches!(
            VideoFrame::required_len(0, 2, 8),
            Err(VideoFrameError::EmptyFrame { .. })
        ));
        let err = VideoFrame::new(
            2,
            2,
            8,
            PixelFormat::Bgra8,
            None,
            FrameBuffer::unpooled(vec![0; 15]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            VideoFrameError::BufferTooSmall {
                len: 15,
                needed: 16
            }
        );
    }

    #[test]
    fn rows_and_pixels_respect_stride() {
        let mut data = vec![0u8; 20];
        data[12..16].copy_from_slice(&[1, 2, 3, 4]);
        let frame = VideoFrame::new(
            2,
            2,
            12,
            PixelFormat::Bgra8,
            None,
            FrameBuffer::unpooled(data),
        )
        .expect("frame");
        assert_eq!(frame.pixel(0, 1), [1, 2, 3, 4]);
        assert_eq!(frame.row(1).len(), 8);
    }

    #[test]
    fn pool_is_bounded_and_recycles() {
        let pool = FramePool::new(2);
        let a = pool.acquire(16).expect("first");
        let b = pool.acquire(16).expect("second");
        assert!(pool.acquire(16).is_none(), "pool must be bounded");
        drop(a);
        let c = pool.acquire(32).expect("recycled");
        assert_eq!(c.len(), 32);
        assert_eq!(pool.allocated(), 2);
        drop((b, c));
    }

    #[test]
    fn mailbox_keeps_newest_and_counts_replaced() {
        let pool = FramePool::new(4);
        let mailbox = FrameMailbox::new();
        assert!(!mailbox.publish(frame(&pool, 1)));
        assert!(mailbox.publish(frame(&pool, 2)));
        assert!(mailbox.publish(frame(&pool, 3)));
        let newest = mailbox.take().expect("frame");
        assert_eq!(newest.data()[0], 3);
        assert!(mailbox.take().is_none());
        assert_eq!(
            mailbox.stats(),
            MailboxStats {
                published: 3,
                replaced: 2,
                taken: 1
            }
        );
        // Replaced frames returned their buffers before the next acquire, so
        // three frames needed only two allocations.
        assert_eq!(pool.allocated(), 2);
        drop(newest);
        let held: Vec<_> = (0..2)
            .map(|_| pool.acquire(16).expect("returned buffer"))
            .collect();
        assert_eq!(held.len(), 2);
        assert_eq!(pool.allocated(), 2);
    }
}
