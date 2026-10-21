//! The only `unsafe` code of this crate: reading a locked pixel buffer.

use screencapturekit::cv::CVPixelBufferLockGuard;

/// The bytes of a locked, non-planar pixel buffer (`height * bytes_per_row`
/// bytes), or `None` for planar buffers or a missing base address.
pub(crate) fn locked_bytes<'a>(guard: &'a CVPixelBufferLockGuard<'_>) -> Option<&'a [u8]> {
    // SAFETY: `as_slice` requires the mapping to stay allocated, in place
    // and unmodified while the slice lives, and no manual unlock.
    // - The guard holds a read-only lock until it drops, and the slice
    //   borrows the guard, so the mapping outlives the slice; this crate
    //   never unlocks manually.
    // - ScreenCaptureKit hands the buffer to the output handler and reuses
    //   its surface only after every sample buffer referring to it has been
    //   released; the caller holds that sample buffer while it copies.
    // - Nothing in this process maps the buffer for writing.
    unsafe { guard.as_slice() }
}
