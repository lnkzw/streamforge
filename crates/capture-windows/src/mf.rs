//! Media Foundation calls: the only `unsafe` code of the camera backend.
//!
//! Media Foundation is loaded at run time (`LoadLibraryExW` from System32)
//! instead of being linked: Windows N editions without the Media Feature
//! Pack, and Windows Server without its Media Foundation feature, lack the
//! DLLs, and a linked import would stop every StreamForge binary from
//! starting there. Without them the camera is unavailable with a reason.
//!
//! Invariants for everything here:
//! - Every call runs on a thread that entered the process's multithreaded
//!   apartment through [`Session::start`], and every Media Foundation
//!   object is released before that thread's [`Session`] drops (callers
//!   declare the session first, so it drops last).
//! - Interface pointers are owned by the `windows` crate's wrappers, which
//!   release them on drop; raw pointers from out parameters are wrapped
//!   with `from_raw` exactly once.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr;
use std::sync::OnceLock;
use std::sync::mpsc::SyncSender;

use capture_api::{CaptureError, CaptureErrorKind, ModeOffer};
use windows::Win32::Foundation::{
    E_ACCESSDENIED, ERROR_SHARING_VIOLATION, HMODULE, RPC_E_CHANGED_MODE,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFActivate, IMFAttributes, IMFMediaEvent, IMFMediaSource, IMFMediaType,
    IMFSample, IMFSourceReader, IMFSourceReaderCallback, IMFSourceReaderCallback_Impl,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_E_HW_MFT_FAILED_START_STREAMING,
    MF_E_NO_MORE_TYPES, MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED,
    MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED, MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ASYNC_CALLBACK,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_VERSION, MF2DBuffer_LockFlags_Read, MFMediaType_Video, MFSTARTUP_LITE, MFVideoFormat_RGB32,
    MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{GUID, HRESULT, Interface, PCSTR, PCWSTR, PWSTR, Ref, implement, s, w};

use crate::camera_formats::{
    self, CameraFormat, MAX_NATIVE_TYPES, RowSpan, locked_2d, locked_contiguous,
};

/// The source reader's first video stream, as its `u32` index.
pub(crate) const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const ALL_STREAMS: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;

// The exports used, with the signatures of the Windows SDK headers
// (mfapi.h, mfidl.h, mfreadwrite.h); interface pointers as raw pointers.
type StartupFn = unsafe extern "system" fn(u32, u32) -> HRESULT;
type ShutdownFn = unsafe extern "system" fn() -> HRESULT;
type GetStrideFn = unsafe extern "system" fn(u32, u32, *mut i32) -> HRESULT;
type CreateAttributesFn = unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT;
type CreateMediaTypeFn = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type EnumDeviceSourcesFn =
    unsafe extern "system" fn(*mut c_void, *mut *mut *mut c_void, *mut u32) -> HRESULT;
type CreateSourceReaderFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT;

/// What `GetProcAddress` returns for an export.
type RawProc = unsafe extern "system" fn() -> isize;

struct Api {
    startup: StartupFn,
    shutdown: ShutdownFn,
    get_stride: GetStrideFn,
    create_attributes: CreateAttributesFn,
    create_media_type: CreateMediaTypeFn,
    enum_device_sources: EnumDeviceSourcesFn,
    create_source_reader: CreateSourceReaderFn,
}

fn api() -> Result<&'static Api, CaptureError> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(load)
        .as_ref()
        .map_err(|reason| CaptureError::new(CaptureErrorKind::Unavailable, reason.clone()))
}

fn load() -> Result<Api, String> {
    let module = |name: PCWSTR, file: &str| {
        // SAFETY: loads a system DLL from System32 only; the module is
        // never freed, so pointers into it stay valid for the process.
        unsafe { LoadLibraryExW(name, None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.map_err(|err| {
            format!(
                "Media Foundation is not installed ({file}: {}); Windows N editions need the \
                 Media Feature Pack, Windows Server its Media Foundation feature",
                err.message()
            )
        })
    };
    let export = |module: HMODULE, name: PCSTR| {
        // SAFETY: `module` is loaded and `name` is a NUL-terminated literal.
        unsafe { GetProcAddress(module, name) }.ok_or_else(|| {
            // SAFETY: as above, a NUL-terminated literal.
            let name = unsafe { name.to_string() }.unwrap_or_default();
            format!("Media Foundation lacks `{name}`")
        })
    };
    let mfplat = module(w!("mfplat.dll"), "mfplat.dll")?;
    let mf = module(w!("mf.dll"), "mf.dll")?;
    let mfreadwrite = module(w!("mfreadwrite.dll"), "mfreadwrite.dll")?;
    // SAFETY: each export is cast to the signature its SDK header declares
    // (see the type aliases); all of them use the `system` ABI.
    unsafe {
        Ok(Api {
            startup: std::mem::transmute::<RawProc, StartupFn>(export(mfplat, s!("MFStartup"))?),
            shutdown: std::mem::transmute::<RawProc, ShutdownFn>(export(mfplat, s!("MFShutdown"))?),
            get_stride: std::mem::transmute::<RawProc, GetStrideFn>(export(
                mfplat,
                s!("MFGetStrideForBitmapInfoHeader"),
            )?),
            create_attributes: std::mem::transmute::<RawProc, CreateAttributesFn>(export(
                mfplat,
                s!("MFCreateAttributes"),
            )?),
            create_media_type: std::mem::transmute::<RawProc, CreateMediaTypeFn>(export(
                mfplat,
                s!("MFCreateMediaType"),
            )?),
            enum_device_sources: std::mem::transmute::<RawProc, EnumDeviceSourcesFn>(export(
                mf,
                s!("MFEnumDeviceSources"),
            )?),
            create_source_reader: std::mem::transmute::<RawProc, CreateSourceReaderFn>(export(
                mfreadwrite,
                s!("MFCreateSourceReaderFromMediaSource"),
            )?),
        })
    }
}

/// A Media Foundation error with what was being done, its system message
/// and code, and the kind a caller can act on.
pub(crate) fn error(context: impl std::fmt::Display, err: &windows::core::Error) -> CaptureError {
    let code = err.code();
    let (kind, hint) = if code == E_ACCESSDENIED {
        (
            CaptureErrorKind::PermissionDenied,
            Some("allow camera access for desktop apps in Settings > Privacy & security > Camera"),
        )
    } else if code == MF_E_HW_MFT_FAILED_START_STREAMING
        || code == MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED
        || code == ERROR_SHARING_VIOLATION.to_hresult()
    {
        (
            CaptureErrorKind::Unavailable,
            Some("another application may be using the camera"),
        )
    } else if code == MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED {
        (
            CaptureErrorKind::NotFound,
            Some("the camera was disconnected"),
        )
    } else {
        (CaptureErrorKind::Backend, None)
    };
    let mut message = format!("{context}: {} (HRESULT {:#010x})", err.message(), code.0);
    if let Some(hint) = hint {
        message = format!("{message}; {hint}");
    }
    CaptureError::new(kind, message)
}

fn backend(context: &str) -> impl Fn(windows::core::Error) -> CaptureError + '_ {
    move |err| error(context, &err)
}

/// COM (multithreaded apartment) and Media Foundation on this thread.
/// Not `Send`: it must drop on the thread that started it.
pub(crate) struct Session {
    api: &'static Api,
    com: bool,
    _thread: PhantomData<*const ()>,
}

impl Session {
    pub(crate) fn start() -> Result<Self, CaptureError> {
        let api = api()?;
        // SAFETY: paired with `CoUninitialize` in `Drop` on this thread
        // (the session is not `Send`) when it succeeded (`S_OK`/`S_FALSE`).
        let joined = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let com = joined.is_ok();
        // A thread already in a single-threaded apartment still has COM,
        // and Media Foundation works there; StreamForge's own camera
        // threads never are.
        if !com && joined != RPC_E_CHANGED_MODE {
            return Err(error("cannot initialise COM", &joined.into()));
        }
        // SAFETY: the SDK's version; paired with `MFShutdown` in `Drop`.
        let started = unsafe { (api.startup)(MF_VERSION, MFSTARTUP_LITE) };
        if let Err(err) = started.ok() {
            if com {
                // SAFETY: pairs the successful `CoInitializeEx` above.
                unsafe { CoUninitialize() };
            }
            return Err(error("cannot start Media Foundation", &err));
        }
        Ok(Self {
            api,
            com,
            _thread: PhantomData,
        })
    }

    fn attributes(&self, count: u32) -> Result<IMFAttributes, CaptureError> {
        let mut raw = ptr::null_mut();
        // SAFETY: a valid out pointer; on success it holds a new reference.
        unsafe { (self.api.create_attributes)(&mut raw, count) }
            .ok()
            .map_err(backend("cannot create Media Foundation attributes"))?;
        // SAFETY: the reference just returned, wrapped once.
        Ok(unsafe { IMFAttributes::from_raw(raw) })
    }

    fn media_type(&self) -> Result<IMFMediaType, CaptureError> {
        let mut raw = ptr::null_mut();
        // SAFETY: as in `attributes`.
        unsafe { (self.api.create_media_type)(&mut raw) }
            .ok()
            .map_err(backend("cannot create a media type"))?;
        // SAFETY: the reference just returned, wrapped once.
        Ok(unsafe { IMFMediaType::from_raw(raw) })
    }

    /// The video capture devices, in the system's order.
    pub(crate) fn cameras(&self) -> Result<Vec<Device>, CaptureError> {
        let attributes = self.attributes(1)?;
        // SAFETY: both GUIDs are constants that outlive the call.
        unsafe {
            attributes.SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
        }
        .map_err(backend("cannot list cameras"))?;
        let mut array: *mut *mut c_void = ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: valid attributes and out pointers; on success `array` is
        // a CoTaskMem array of `count` references, which we own.
        unsafe { (self.api.enum_device_sources)(attributes.as_raw(), &mut array, &mut count) }
            .ok()
            .map_err(backend("cannot list cameras"))?;
        let mut activates = Vec::with_capacity(count as usize);
        if !array.is_null() {
            for index in 0..count as usize {
                // SAFETY: `index < count`, inside the returned array.
                let raw = unsafe { *array.add(index) };
                if !raw.is_null() {
                    // SAFETY: each element is one reference, wrapped once.
                    activates.push(unsafe { IMFActivate::from_raw(raw) });
                }
            }
            // SAFETY: the array came from `MFEnumDeviceSources`; its
            // elements are owned by `activates` now.
            unsafe { CoTaskMemFree(Some(array.cast_const().cast())) };
        }
        let mut devices = Vec::with_capacity(activates.len());
        for activate in activates {
            let link = string(
                &activate,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            );
            let Ok(link) = link else {
                continue;
            };
            let name = string(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                .unwrap_or_else(|_| link.clone());
            devices.push(Device {
                activate,
                name,
                link,
            });
        }
        Ok(devices)
    }

    /// A source reader for `source`: synchronous for listing its modes, or
    /// asynchronous with `events` for capture. A capture reader converts to
    /// RGB32 with Media Foundation's advanced video processing (the video
    /// processor MFT, after a decoder for MJPEG); the basic one is meant
    /// for thumbnails, not real time.
    pub(crate) fn reader(
        &self,
        source: &IMFMediaSource,
        events: Option<SyncSender<ReaderEvent>>,
    ) -> Result<IMFSourceReader, CaptureError> {
        let attributes = self.attributes(3)?;
        let fail = backend("cannot create a source reader for the camera");
        if let Some(events) = events {
            let callback: IMFSourceReaderCallback = ReaderCallback { events }.into();
            // SAFETY: constant keys; the attributes keep a reference to the
            // callback, a live COM object.
            unsafe {
                attributes
                    .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
                    .map_err(&fail)?;
                attributes.SetUINT32(&MF_LOW_LATENCY, 1).map_err(&fail)?;
                attributes
                    .SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback)
                    .map_err(&fail)?;
            }
        }
        let mut raw = ptr::null_mut();
        // SAFETY: live source and attributes, a valid out pointer.
        unsafe { (self.api.create_source_reader)(source.as_raw(), attributes.as_raw(), &mut raw) }
            .ok()
            .map_err(&fail)?;
        // SAFETY: the reference just returned, wrapped once.
        Ok(unsafe { IMFSourceReader::from_raw(raw) })
    }

    /// Make the reader deliver `native` (one of its native types) as RGB32
    /// of the same size and rate, rows top-down.
    pub(crate) fn select(
        &self,
        reader: &IMFSourceReader,
        native: &IMFMediaType,
    ) -> Result<Layout, CaptureError> {
        let fail = backend("cannot select the camera mode");
        // SAFETY: a live reader and media types; constant keys.
        unsafe {
            reader
                .SetStreamSelection(ALL_STREAMS, false)
                .map_err(&fail)?;
            reader
                .SetStreamSelection(VIDEO_STREAM, true)
                .map_err(&fail)?;
            reader
                .SetCurrentMediaType(VIDEO_STREAM, None, native)
                .map_err(&fail)?;
        }
        let mut last_error = None;
        // Relax only the optional attributes, keeping a positive stride
        // even when the processor must infer the size and rate.
        for detail in [RgbDetail::Full, RgbDetail::SizeAndRate, RgbDetail::Subtype] {
            let rgb = self.rgb32(native, detail)?;
            // SAFETY: a live reader and media type.
            match unsafe { reader.SetCurrentMediaType(VIDEO_STREAM, None, &rgb) } {
                Ok(()) => return current_layout(reader),
                Err(err) => last_error = Some(err),
            }
        }
        Err(error(
            "the camera's format cannot be converted to RGB32",
            &last_error.expect("three output types tried"),
        ))
    }

    /// An RGB32 type with the native width's positive stride, optionally
    /// its size/rate and its aspect/interlacing attributes.
    fn rgb32(
        &self,
        native: &IMFMediaType,
        detail: RgbDetail,
    ) -> Result<IMFMediaType, CaptureError> {
        let media_type = self.media_type()?;
        let fail = backend("cannot describe RGB32 frames");
        // SAFETY: live media types; constant keys and GUIDs.
        unsafe {
            media_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(&fail)?;
            media_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(&fail)?;
            let size = native.GetUINT64(&MF_MT_FRAME_SIZE).map_err(&fail)?;
            let (width, _) = camera_formats::unpack_size(size);
            let stride = media_core::VideoFrame::packed_stride(width)
                .ok()
                .and_then(|stride| i32::try_from(stride).ok())
                .filter(|stride| *stride > 0)
                .ok_or_else(|| {
                    CaptureError::backend("the RGB32 stride does not fit in a positive LONG")
                })?;
            media_type
                .SetUINT32(&MF_MT_DEFAULT_STRIDE, stride as u32)
                .map_err(&fail)?;
            if matches!(detail, RgbDetail::Subtype) {
                return Ok(media_type);
            }
            media_type
                .SetUINT64(&MF_MT_FRAME_SIZE, size)
                .map_err(&fail)?;
            if let Ok(value) = native.GetUINT64(&MF_MT_FRAME_RATE) {
                media_type
                    .SetUINT64(&MF_MT_FRAME_RATE, value)
                    .map_err(&fail)?;
            }
            if matches!(detail, RgbDetail::Full) {
                if let Ok(value) = native.GetUINT64(&MF_MT_PIXEL_ASPECT_RATIO) {
                    media_type
                        .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, value)
                        .map_err(&fail)?;
                }
                let interlace = native
                    .GetUINT32(&MF_MT_INTERLACE_MODE)
                    .unwrap_or(MFVideoInterlace_Progressive.0 as u32);
                media_type
                    .SetUINT32(&MF_MT_INTERLACE_MODE, interlace)
                    .map_err(&fail)?;
            }
        }
        Ok(media_type)
    }
}

#[derive(Clone, Copy)]
enum RgbDetail {
    Full,
    SizeAndRate,
    Subtype,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: pairs the successful `MFStartup` and `CoInitializeEx` of
        // `start` on this thread; every Media Foundation object of this
        // thread is released by now (see the module docs).
        unsafe {
            let _ = (self.api.shutdown)();
            if self.com {
                CoUninitialize();
            }
        }
    }
}

fn string(attributes: &IMFAttributes, key: &GUID) -> windows::core::Result<String> {
    let mut text = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: valid out pointers; on success `text` is a CoTaskMem string.
    unsafe { attributes.GetAllocatedString(key, &mut text, &mut len) }?;
    // SAFETY: a NUL-terminated string from Media Foundation, freed once.
    let value = unsafe { text.to_string() };
    // SAFETY: the string from `GetAllocatedString`, not used after this.
    unsafe { CoTaskMemFree(Some(text.0.cast_const().cast())) };
    value.map_err(|_| windows::core::Error::from(windows::Win32::Foundation::E_UNEXPECTED))
}

/// A video capture device from [`Session::cameras`].
pub(crate) struct Device {
    activate: IMFActivate,
    pub name: String,
    /// The device's symbolic link: StreamForge's `camera_id` on Windows.
    pub link: String,
}

impl Device {
    /// Create the device's media source (it streams only once read from).
    pub(crate) fn open(&self) -> Result<Source, CaptureError> {
        // SAFETY: a live activation object.
        let source = unsafe { self.activate.ActivateObject::<IMFMediaSource>() }
            .map_err(|err| error(format_args!("cannot open camera `{}`", self.name), &err))?;
        Ok(Source(source))
    }
}

/// A camera's media source, shut down on drop (after its reader is gone).
pub(crate) struct Source(IMFMediaSource);

impl Source {
    pub(crate) fn get(&self) -> &IMFMediaSource {
        &self.0
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        // SAFETY: a live source; a second shutdown (the reader shuts its
        // source down when released) only returns `MF_E_SHUTDOWN`.
        let _ = unsafe { self.0.Shutdown() };
    }
}

/// The modes a reader's source offers: each accepted native type with its
/// index, at most [`MAX_NATIVE_TYPES`] read.
pub(crate) fn native_offers(
    reader: &IMFSourceReader,
) -> Result<Vec<(IMFMediaType, ModeOffer<CameraFormat>)>, CaptureError> {
    let mut offers = Vec::new();
    for index in 0..MAX_NATIVE_TYPES {
        // SAFETY: a live reader.
        let native = match unsafe { reader.GetNativeMediaType(VIDEO_STREAM, index) } {
            Ok(native) => native,
            Err(err) if err.code() == MF_E_NO_MORE_TYPES => break,
            Err(err) => return Err(error("cannot read the camera's modes", &err)),
        };
        // SAFETY: a live media type; constant keys.
        let (subtype, size, rate) = unsafe {
            (
                native.GetGUID(&MF_MT_SUBTYPE),
                native.GetUINT64(&MF_MT_FRAME_SIZE),
                native.GetUINT64(&MF_MT_FRAME_RATE).ok(),
            )
        };
        let (Ok(subtype), Ok(size)) = (subtype, size) else {
            continue;
        };
        if let Some(offer) = camera_formats::offer(subtype.to_u128(), size, rate) {
            offers.push((native, offer));
        }
    }
    Ok(offers)
}

/// The size and stride of the frames a reader delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    pub width: u32,
    pub height: u32,
    /// `MF_MT_DEFAULT_STRIDE`, negative for bottom-up rows.
    pub default_stride: i32,
}

pub(crate) fn current_layout(reader: &IMFSourceReader) -> Result<Layout, CaptureError> {
    // SAFETY: a live reader; the returned type remains owned here.
    let current = unsafe { reader.GetCurrentMediaType(VIDEO_STREAM) }
        .map_err(backend("cannot read the camera's output format"))?;
    layout_of(&current)
}

fn layout_of(current: &IMFMediaType) -> Result<Layout, CaptureError> {
    let fail = backend("cannot read the camera's output format");
    // SAFETY: a live media type; constant keys and a valid stride out pointer.
    unsafe {
        let (width, height) =
            camera_formats::unpack_size(current.GetUINT64(&MF_MT_FRAME_SIZE).map_err(&fail)?);
        let default_stride = match current.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
            // The attribute holds a signed value in a UINT32.
            Ok(stride) => stride as i32,
            Err(_) => {
                // Microsoft's default-stride procedure for contiguous
                // buffers: derive it from the subtype's first DWORD.
                let subtype = current.GetGUID(&MF_MT_SUBTYPE).map_err(&fail)?;
                let mut stride = 0;
                (api()?.get_stride)(subtype.data1, width, &mut stride)
                    .ok()
                    .map_err(&fail)?;
                stride
            }
        };
        Ok(Layout {
            width,
            height,
            default_stride,
        })
    }
}

/// Ask for the next sample; it arrives as a [`ReaderEvent::Sample`].
pub(crate) fn request_sample(reader: &IMFSourceReader) -> Result<(), CaptureError> {
    // SAFETY: a live asynchronous reader: every out parameter is null.
    unsafe { reader.ReadSample(VIDEO_STREAM, 0, None, None, None, None) }
        .map_err(backend("cannot read from the camera"))
}

/// Drop pending samples; [`ReaderEvent::Flushed`] follows.
pub(crate) fn flush(reader: &IMFSourceReader) -> Result<(), CaptureError> {
    // SAFETY: a live asynchronous reader.
    unsafe { reader.Flush(VIDEO_STREAM) }.map_err(backend("cannot flush the camera"))
}

/// Lock `sample`'s buffer as `layout` frames and hand its rows to `use_rows`.
pub(crate) fn with_rows<R>(
    sample: &Sample,
    layout: Layout,
    use_rows: impl FnOnce(&mut dyn Iterator<Item = &[u8]>) -> R,
) -> Result<R, String> {
    let (width, height) = (layout.width, layout.height);
    // SAFETY: a live sample; the buffer stays referenced while locked.
    let buffer = unsafe { sample.0.ConvertToContiguousBuffer() }
        .map_err(|err| format!("cannot read the sample's buffer: {}", err.message()))?;
    if let Ok(buffer) = buffer.cast::<IMF2DBuffer2>() {
        let (mut scanline0, mut pitch) = (ptr::null_mut(), 0i32);
        let (mut start, mut len) = (ptr::null_mut(), 0u32);
        // SAFETY: valid out pointers; unlocked below on every path.
        unsafe {
            buffer.Lock2DSize(
                MF2DBuffer_LockFlags_Read,
                &mut scanline0,
                &mut pitch,
                &mut start,
                &mut len,
            )
        }
        .map_err(|err| format!("cannot lock the frame: {}", err.message()))?;
        let rows = locked_2d(width, height, pitch).and_then(|span| {
            let lowest = span
                .lowest_row(scanline0 as usize, start as usize, len as usize)
                .ok_or_else(|| {
                    format!(
                        "a {width}x{height} frame with a pitch of {pitch} bytes does not fit \
                         its {len} byte buffer"
                    )
                })?;
            if scanline0.is_null() || start.is_null() {
                return Err("the camera locked a null frame buffer".to_owned());
            }
            debug_assert_eq!(scanline0 as usize - span.back, lowest);
            // SAFETY: `lowest_row` validated every pixel row inside this
            // locked buffer. Only pixel bytes are exposed, never padding.
            Ok(unsafe {
                use_locked_rows(
                    scanline0.cast_const().wrapping_sub(span.back),
                    span,
                    width,
                    height,
                    use_rows,
                )
            })
        });
        // SAFETY: pairs the successful lock above.
        let _ = unsafe { buffer.Unlock2D() };
        return rows;
    }
    let (mut data, mut len) = (ptr::null_mut(), 0u32);
    // SAFETY: valid out pointers; unlocked below on every path.
    unsafe { buffer.Lock(&mut data, None, Some(&mut len)) }
        .map_err(|err| format!("cannot lock the frame: {}", err.message()))?;
    let rows =
        locked_contiguous(width, height, layout.default_stride, len as usize).and_then(|span| {
            if data.is_null() {
                return Err("the camera locked a null frame buffer".to_owned());
            }
            // SAFETY: `locked_contiguous` validated every pixel row inside the
            // locked length; the buffer remains locked through the callback.
            Ok(unsafe { use_locked_rows(data.cast_const(), span, width, height, use_rows) })
        });
    // SAFETY: pairs the successful lock above.
    let _ = unsafe { buffer.Unlock() };
    rows
}

/// Expose the validated pixel rows of a buffer, top row first.
///
/// # Safety
/// `lowest` must point to the lowest row of a locked RGB32 buffer whose
/// geometry was validated as `span`; every row's pixels must be readable
/// and unchanged until `use_rows` returns. Padding need not be readable.
unsafe fn use_locked_rows<R>(
    lowest: *const u8,
    span: RowSpan,
    width: u32,
    height: u32,
    use_rows: impl FnOnce(&mut dyn Iterator<Item = &[u8]>) -> R,
) -> R {
    let row_len = media_core::VideoFrame::packed_stride(width).expect("validated row size");
    let mut rows = (0..height as usize).map(|y| {
        let index = if span.bottom_up {
            height as usize - 1 - y
        } else {
            y
        };
        // SAFETY: validated span bounds every row and its pixel length;
        // no slice covers padding. The caller holds the buffer lock.
        unsafe { std::slice::from_raw_parts(lowest.add(index * span.stride), row_len) }
    });
    use_rows(&mut rows)
}

/// A sample from the reader's callback.
pub(crate) struct Sample(IMFSample);

// SAFETY: Media Foundation's work-queue threads (where the callback runs)
// and the camera thread are all in the process's multithreaded apartment,
// where an interface pointer may be used from any thread without
// marshaling. Only one thread uses a sample at a time (it moves).
unsafe impl Send for Sample {}

/// What the asynchronous reader reports to the camera thread, plus the
/// stream handle's stop request.
pub(crate) enum ReaderEvent {
    Sample {
        status: HRESULT,
        flags: u32,
        /// In 100 ns units.
        timestamp: i64,
        sample: Option<Sample>,
    },
    Flushed,
    Stop,
}

#[implement(IMFSourceReaderCallback)]
struct ReaderCallback {
    /// Bounded: the camera thread asks for one sample at a time, so at
    /// most one sample, one flush and the stop request are queued.
    events: SyncSender<ReaderEvent>,
}

impl IMFSourceReaderCallback_Impl for ReaderCallback_Impl {
    fn OnReadSample(
        &self,
        status: HRESULT,
        _stream: u32,
        flags: u32,
        timestamp: i64,
        sample: Ref<IMFSample>,
    ) -> windows::core::Result<()> {
        // A send fails only once the camera thread is gone.
        let _ = self.events.send(ReaderEvent::Sample {
            status,
            flags,
            timestamp,
            sample: sample.cloned().map(Sample),
        });
        Ok(())
    }

    fn OnFlush(&self, _stream: u32) -> windows::core::Result<()> {
        let _ = self.events.send(ReaderEvent::Flushed);
        Ok(())
    }

    fn OnEvent(&self, _stream: u32, _event: Ref<IMFMediaEvent>) -> windows::core::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_rows_skip_padding_in_both_orientations() {
        let data: Vec<u8> = (0..32).collect();
        for pitch in [12, -12] {
            let span = locked_2d(2, 3, pitch).expect("span");
            // SAFETY: data holds all validated rows and stays unchanged
            // throughout the callback; this test needs no native buffer.
            unsafe {
                use_locked_rows(data.as_ptr(), span, 2, 3, |rows| {
                    let actual: Vec<_> = rows.map(<[u8]>::to_vec).collect();
                    let mut expected = vec![
                        data[..8].to_vec(),
                        data[12..20].to_vec(),
                        data[24..32].to_vec(),
                    ];
                    if pitch < 0 {
                        expected.reverse();
                    }
                    assert_eq!(actual, expected);
                });
            }
        }
    }

    #[test]
    fn rgb32_fallbacks_keep_positive_stride_and_missing_stride_is_resolved() {
        let session = match Session::start() {
            Ok(session) => session,
            Err(err) => {
                println!("Media Foundation unavailable: {err}");
                return;
            }
        };
        let native = session.media_type().expect("media type");
        let size = (640u64 << 32) | 480;
        let rate = (30u64 << 32) | 1;
        // SAFETY: owned media types, constant attribute keys.
        unsafe {
            native.SetUINT64(&MF_MT_FRAME_SIZE, size).unwrap();
            native.SetUINT64(&MF_MT_FRAME_RATE, rate).unwrap();
            native
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
                .unwrap();
            for detail in [RgbDetail::Full, RgbDetail::SizeAndRate, RgbDetail::Subtype] {
                let rgb = session.rgb32(&native, detail).expect("RGB32 type");
                assert_eq!(rgb.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap(), 2560);
                assert_eq!(rgb.GetGUID(&MF_MT_SUBTYPE).unwrap(), MFVideoFormat_RGB32);
                if matches!(detail, RgbDetail::Subtype) {
                    assert!(rgb.GetUINT64(&MF_MT_FRAME_SIZE).is_err());
                    assert!(rgb.GetUINT64(&MF_MT_FRAME_RATE).is_err());
                    // Simulate a negotiated subtype-only fallback that
                    // supplies a size but omits the requested stride.
                    rgb.SetUINT64(&MF_MT_FRAME_SIZE, size).unwrap();
                    rgb.DeleteItem(&MF_MT_DEFAULT_STRIDE).unwrap();
                    let layout = layout_of(&rgb).expect("resolved layout");
                    let mut expected = 0;
                    (session.api.get_stride)(MFVideoFormat_RGB32.data1, 640, &mut expected)
                        .ok()
                        .unwrap();
                    assert_ne!(expected, 0);
                    assert_eq!(layout.default_stride, expected);
                    rgb.SetUINT32(&MF_MT_DEFAULT_STRIDE, (-2560i32) as u32)
                        .unwrap();
                    assert_eq!(layout_of(&rgb).unwrap().default_stride, -2560);
                } else {
                    assert_eq!(rgb.GetUINT64(&MF_MT_FRAME_SIZE).unwrap(), size);
                    assert_eq!(rgb.GetUINT64(&MF_MT_FRAME_RATE).unwrap(), rate);
                    assert_eq!(
                        rgb.GetUINT64(&MF_MT_PIXEL_ASPECT_RATIO).is_ok(),
                        matches!(detail, RgbDetail::Full)
                    );
                }
            }
        }
    }

    #[test]
    fn errors_name_their_kind_and_remedy() {
        let denied = error("cannot open camera `Cam`", &E_ACCESSDENIED.into());
        assert_eq!(denied.kind, CaptureErrorKind::PermissionDenied);
        assert!(
            denied.message.starts_with("cannot open camera `Cam`: "),
            "{denied}"
        );
        assert!(denied.message.contains("0x80070005"), "{denied}");
        assert!(
            denied.message.contains("Privacy & security > Camera"),
            "{denied}"
        );
        let busy = error("x", &MF_E_HW_MFT_FAILED_START_STREAMING.into());
        assert_eq!(busy.kind, CaptureErrorKind::Unavailable);
        assert!(busy.message.contains("another application"), "{busy}");
        let gone = error("x", &MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED.into());
        assert_eq!(gone.kind, CaptureErrorKind::NotFound);
        let other = error("x", &MF_E_NO_MORE_TYPES.into());
        assert_eq!(other.kind, CaptureErrorKind::Backend);
    }
}
