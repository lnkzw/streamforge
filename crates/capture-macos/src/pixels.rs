//! Reading locked pixel buffers: the only `unsafe` code of this crate
//! outside `avf`.

use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetDataSize, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferIsPlanar, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVReturnSuccess,
};
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

/// A camera frame's pixel buffer (Core Video, from AVFoundation), locked
/// for reading until dropped.
pub(crate) struct LockedPixels<'a> {
    buffer: &'a CVPixelBuffer,
}

impl<'a> LockedPixels<'a> {
    pub(crate) fn lock(buffer: &'a CVPixelBuffer) -> Result<Self, String> {
        // SAFETY: locking a live buffer; `Drop` unlocks it with the same
        // flags, once.
        let status =
            unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };
        if status != kCVReturnSuccess {
            return Err(format!("cannot lock the camera frame (CVReturn {status})"));
        }
        Ok(Self { buffer })
    }

    pub(crate) fn width(&self) -> usize {
        CVPixelBufferGetWidth(self.buffer)
    }

    pub(crate) fn height(&self) -> usize {
        CVPixelBufferGetHeight(self.buffer)
    }

    pub(crate) fn bytes_per_row(&self) -> usize {
        CVPixelBufferGetBytesPerRow(self.buffer)
    }

    /// The four-character code of the buffer's pixel format.
    pub(crate) fn pixel_format(&self) -> u32 {
        CVPixelBufferGetPixelFormatType(self.buffer)
    }

    /// The `height * bytes_per_row` bytes of a non-planar buffer, or `None`
    /// for a planar buffer, a missing base address, or a size that does
    /// not fit what Core Video says the buffer holds.
    pub(crate) fn bytes(&self) -> Option<&[u8]> {
        if CVPixelBufferIsPlanar(self.buffer) {
            return None;
        }
        let base = CVPixelBufferGetBaseAddress(self.buffer).cast::<u8>();
        if base.is_null() {
            return None;
        }
        let len = self.height().checked_mul(self.bytes_per_row())?;
        let size = CVPixelBufferGetDataSize(self.buffer);
        if size != 0 && size < len {
            return None;
        }
        // SAFETY: `from_raw_parts` needs `len` readable, initialised bytes
        // at `base` that nothing writes while the slice lives.
        // - The buffer is locked (read-only) from `lock` until `self`
        //   drops, and the slice borrows `self`; a locked non-planar
        //   buffer's base address points at `height` rows of
        //   `bytes_per_row` bytes (Core Video), and its data size, where
        //   stated, was checked to hold them.
        // - AVFoundation reuses a frame's memory only after every sample
        //   buffer referring to it has been released; the caller holds the
        //   sample buffer for the whole callback.
        // - Nothing in this process writes camera frames.
        Some(unsafe { std::slice::from_raw_parts(base, len) })
    }
}

impl Drop for LockedPixels<'_> {
    fn drop(&mut self) {
        // SAFETY: `lock` locked it read-only, and nothing else unlocks it.
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, CVPixelBufferLockFlags::ReadOnly) };
    }
}

/// A pixel buffer of `width`×`height` in `format` whose bytes count up from
/// 0, and its bytes per row (Core Video pads rows, so for BGRA the stride
/// exceeds `width * 4`). Only the first plane is filled.
#[cfg(test)]
pub(crate) fn test_buffer(
    width: usize,
    height: usize,
    format: u32,
) -> (objc2_core_foundation::CFRetained<CVPixelBuffer>, usize) {
    use std::ptr::NonNull;

    use objc2_core_video::{CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane};

    let mut raw = std::ptr::null_mut();
    // SAFETY: a valid out pointer; no allocator or attributes.
    let status =
        unsafe { CVPixelBufferCreate(None, width, height, format, None, NonNull::from(&mut raw)) };
    assert_eq!(status, kCVReturnSuccess);
    // SAFETY: CVPixelBufferCreate returned a +1 buffer.
    let buffer =
        unsafe { objc2_core_foundation::CFRetained::from_raw(NonNull::new(raw).expect("buffer")) };
    let planar = CVPixelBufferIsPlanar(&buffer);
    let stride = if planar {
        objc2_core_video::CVPixelBufferGetBytesPerRowOfPlane(&buffer, 0)
    } else {
        CVPixelBufferGetBytesPerRow(&buffer)
    };
    // SAFETY: the buffer is locked for writing while the `height` rows of
    // `stride` bytes of its first plane are filled, then unlocked.
    unsafe {
        let flags = CVPixelBufferLockFlags::empty();
        assert_eq!(
            CVPixelBufferLockBaseAddress(&buffer, flags),
            kCVReturnSuccess
        );
        let base = if planar {
            CVPixelBufferGetBaseAddressOfPlane(&buffer, 0)
        } else {
            CVPixelBufferGetBaseAddress(&buffer)
        }
        .cast::<u8>();
        for index in 0..height * stride {
            *base.add(index) = index as u8;
        }
        CVPixelBufferUnlockBaseAddress(&buffer, flags);
    }
    (buffer, stride)
}
