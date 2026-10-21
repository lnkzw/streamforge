//! VAAPI surfaces for `h264_vaapi` (Linux), the only candidate encoder
//! that needs its frames uploaded to the GPU first. ffmpeg-next 9 has no
//! hardware-context API, so this small module calls libavutil directly.
//!
//! Invariants:
//! - `device` and `frames` are owned `AVBufferRef`s, created here and
//!   released exactly once in `Drop` (also on every failure path of `new`).
//! - The encoder context gets its own reference to `frames`
//!   (`av_buffer_ref`), which libavcodec releases with the context.
//! - Uploaded frames come from `av_hwframe_get_buffer` on `frames`. Their
//!   `AVFrame` is owned by the returned `frame::Video` and freed with it;
//!   the surface goes back to the pool when the encoder releases it.
//! - The references are only used from the thread that owns the session.

use std::ptr;

use ff::ffi;
use ff::{encoder, frame};
use ffmpeg_next as ff;

/// Surfaces this many frames deep: the encoder's own references plus
/// headroom for the frame being uploaded.
const POOL_SIZE: i32 = 20;

pub(crate) struct Surfaces {
    device: *mut ffi::AVBufferRef,
    frames: *mut ffi::AVBufferRef,
}

// SAFETY: the buffer references are reference-counted atomically by
// libavutil; this type is moved into the session thread and used there only.
unsafe impl Send for Surfaces {}

fn error(context: &str, code: i32) -> String {
    format!("{context}: {}", ff::Error::from(code))
}

impl Surfaces {
    /// A VAAPI device (the default render node) with a pool of NV12
    /// surfaces of `width`×`height`.
    pub(crate) fn new(width: u32, height: u32) -> Result<Self, String> {
        let width = i32::try_from(width).map_err(|_| "width out of range".to_owned())?;
        let height = i32::try_from(height).map_err(|_| "height out of range".to_owned())?;
        let mut surfaces = Self {
            device: ptr::null_mut(),
            frames: ptr::null_mut(),
        };
        // SAFETY: `device` receives a new reference or stays null on error;
        // null device name and options select the default render node.
        let created = unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut surfaces.device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                ptr::null(),
                ptr::null_mut(),
                0,
            )
        };
        if created < 0 {
            return Err(error("no VAAPI device", created));
        }
        // SAFETY: `device` is a valid device reference (checked above).
        surfaces.frames = unsafe { ffi::av_hwframe_ctx_alloc(surfaces.device) };
        if surfaces.frames.is_null() {
            return Err("cannot allocate VAAPI frames".to_owned());
        }
        // SAFETY: `frames` was just allocated by `av_hwframe_ctx_alloc`, so
        // its data is an `AVHWFramesContext` that nothing else uses yet.
        let initialized = unsafe {
            let context = (*surfaces.frames).data.cast::<ffi::AVHWFramesContext>();
            (*context).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*context).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
            (*context).width = width;
            (*context).height = height;
            (*context).initial_pool_size = POOL_SIZE;
            ffi::av_hwframe_ctx_init(surfaces.frames)
        };
        if initialized < 0 {
            return Err(error("cannot create VAAPI surfaces", initialized));
        }
        Ok(surfaces)
    }

    /// Give the (not yet opened) encoder its surfaces.
    pub(crate) fn attach(&self, context: &mut encoder::video::Video) -> Result<(), String> {
        // SAFETY: `frames` is a valid, initialized frames reference; the new
        // reference is owned by the codec context from here on. The context
        // pointer is valid for the borrow of `context`.
        unsafe {
            let reference = ffi::av_buffer_ref(self.frames);
            if reference.is_null() {
                return Err("cannot reference the VAAPI surfaces".to_owned());
            }
            let codec = context.as_mut_ptr();
            ffi::av_buffer_unref(&mut (*codec).hw_frames_ctx);
            (*codec).hw_frames_ctx = reference;
        }
        Ok(())
    }

    /// Upload a software NV12 frame of the surfaces' size to a surface.
    pub(crate) fn upload(&self, software: &frame::Video) -> Result<frame::Video, String> {
        // SAFETY: `av_frame_alloc` returns an owned frame (or null), which
        // `frame::Video::wrap` takes over. `frames` is a valid frames
        // reference, and `software` is a valid NV12 frame for the call.
        unsafe {
            let raw = ffi::av_frame_alloc();
            if raw.is_null() {
                return Err("cannot allocate a frame".to_owned());
            }
            let mut surface = frame::Video::wrap(raw);
            let got = ffi::av_hwframe_get_buffer(self.frames, surface.as_mut_ptr(), 0);
            if got < 0 {
                return Err(error("no free VAAPI surface", got));
            }
            let copied = ffi::av_hwframe_transfer_data(surface.as_mut_ptr(), software.as_ptr(), 0);
            if copied < 0 {
                return Err(error("cannot upload to the GPU", copied));
            }
            Ok(surface)
        }
    }
}

impl Drop for Surfaces {
    fn drop(&mut self) {
        // SAFETY: both are owned references or null; `av_buffer_unref`
        // accepts null and sets the pointer to null.
        unsafe {
            ffi::av_buffer_unref(&mut self.frames);
            ffi::av_buffer_unref(&mut self.device);
        }
    }
}
