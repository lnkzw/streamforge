//! Opening an FFmpeg output with an interrupt callback (the only `unsafe`
//! code of the outputs).
//!
//! ffmpeg-next 9 opens outputs without an `AVIOInterruptCB`, so blocked
//! network I/O can only end by its own timeout, and it leaks the format
//! context when `avio_open2` fails. [`open_output`] does both steps itself.
//!
//! Invariants:
//! - The callback's `opaque` is `Arc::as_ptr` of an [`Interrupt`] that the
//!   caller keeps alive until the returned `Output` has been dropped: FFmpeg
//!   calls the callback until the I/O context is closed, which happens in
//!   `Output`'s destructor. `Session` stores both and drops the output first.
//! - The callback only reads atomics; it cannot panic or block.
//! - On every failure path, everything allocated here is freed.

use std::ffi::{CString, c_int, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ff::ffi;
use ff::{Dictionary, format};
use ffmpeg_next as ff;

use crate::{Shared, monotonic_ns};

/// When blocked FFmpeg I/O of one output session must give up.
pub(crate) struct Interrupt {
    /// This session is being torn down (e.g. after a broken connection).
    abort: AtomicBool,
    /// The owner's stop deadline (`Shared::abort_after_ns`).
    shared: Arc<Shared>,
}

impl Interrupt {
    pub(crate) fn new(shared: Arc<Shared>) -> Arc<Self> {
        Arc::new(Self {
            abort: AtomicBool::new(false),
            shared,
        })
    }

    /// Make all further blocking I/O of this session fail at once.
    pub(crate) fn abort(&self) {
        self.abort.store(true, Ordering::Release);
    }

    fn fired(&self) -> bool {
        self.abort.load(Ordering::Acquire)
            || monotonic_ns() >= self.shared.abort_after_ns.load(Ordering::Acquire)
    }
}

unsafe extern "C" fn callback(opaque: *mut c_void) -> c_int {
    // SAFETY: `opaque` is `Arc::as_ptr` of an `Interrupt` that outlives the
    // output (module invariant), and `Interrupt` is `Sync`.
    let interrupt = unsafe { &*(opaque as *const Interrupt) };
    c_int::from(interrupt.fired())
}

/// Allocate an output context for `format`, install the interrupt callback
/// and open `url` for writing with protocol `options`.
pub(crate) fn open_output(
    url: &str,
    format: &str,
    options: Dictionary,
    interrupt: &Arc<Interrupt>,
) -> Result<format::context::Output, ff::Error> {
    let url = CString::new(url).map_err(|_| ff::Error::InvalidData)?;
    let format = CString::new(format).map_err(|_| ff::Error::InvalidData)?;
    // SAFETY: all pointers passed to FFmpeg are valid for the call: the C
    // strings live until the end of this function, `ctx` is checked before
    // use and freed on failure, and the callback's opaque pointer outlives
    // the returned output (module invariant).
    unsafe {
        let mut ctx: *mut ffi::AVFormatContext = ptr::null_mut();
        let ret = ffi::avformat_alloc_output_context2(
            &mut ctx,
            ptr::null(),
            format.as_ptr(),
            url.as_ptr(),
        );
        if ret < 0 || ctx.is_null() {
            return Err(ff::Error::from(ret.min(-1)));
        }
        (*ctx).interrupt_callback = ffi::AVIOInterruptCB {
            callback: Some(callback),
            opaque: Arc::as_ptr(interrupt) as *mut c_void,
        };
        let mut options = options.disown();
        let ret = ffi::avio_open2(
            &mut (*ctx).pb,
            url.as_ptr(),
            ffi::AVIO_FLAG_WRITE as c_int,
            &(*ctx).interrupt_callback,
            &mut options,
        );
        // Frees the options FFmpeg did not consume.
        drop(Dictionary::own(options));
        if ret < 0 {
            ffi::avformat_free_context(ctx);
            return Err(ff::Error::from(ret));
        }
        Ok(format::context::Output::wrap(ctx))
    }
}
