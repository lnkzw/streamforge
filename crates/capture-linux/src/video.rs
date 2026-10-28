//! Video capture streams: a portal-provided screen-cast node, or a camera
//! node on the user's PipeWire daemon.

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::SyncSender;
use std::time::Instant;

use capture_api::{
    BufferKind, CaptureError, FrameObservation, VideoFormatInfo, VideoStats, VideoStatsAccumulator,
};
use frame_decode::{MjpegDecoder, Plane, Planes, YuvLayout};
use media_core::{FrameMailbox, FramePool, MediaTimestamp, VideoFrame};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamBox, StreamFlags, StreamState};
use spa::buffer::meta::{MetaHeader, MetaHeaderFlags};
use spa::buffer::{ChunkFlags, Data};
use spa::param::ParamType;
use spa::pod::Pod;

use crate::formats::{self, FrameLayout};
use crate::pw::{Connection, RunUntil, StreamWatch};

/// What a video stream connects to.
pub(crate) enum Target {
    /// A portal's screen-cast node, through the portal's PipeWire connection.
    Portal {
        remote: OwnedFd,
        node_id: u32,
        preferred_size: Option<(u32, u32)>,
    },
    /// A camera node, by name, on the user's PipeWire daemon, offered the
    /// `EnumFormat` pods in `formats` in order of preference.
    Camera {
        node_name: String,
        formats: Vec<Vec<u8>>,
    },
}

impl Target {
    fn what(&self) -> &'static str {
        match self {
            Self::Portal { .. } => "screen",
            Self::Camera { .. } => "camera",
        }
    }
}

/// Where captured frames go in streaming mode.
pub(crate) struct Delivery {
    pub mailbox: Arc<FrameMailbox>,
    /// Bounded; when empty, frames are discarded and counted.
    pub pool: FramePool,
}

pub(crate) struct VideoRun {
    pub format: Option<VideoFormatInfo>,
    pub buffer_memory: Option<&'static str>,
    pub stats: VideoStats,
    pub warnings: Vec<String>,
}

#[derive(Default)]
struct State {
    stats: VideoStatsAccumulator,
    format: Option<VideoFormatInfo>,
    /// Set when the negotiated format can be converted for delivery.
    layout: Option<FrameLayout>,
    /// Created when MJPEG is negotiated for delivery; reused for every frame.
    mjpeg: Option<MjpegDecoder>,
    buffer_memory: Option<&'static str>,
    /// Frames arrived with bottom-up rows (negative stride); not supported.
    saw_bottom_up: bool,
    warnings: Vec<String>,
}

/// Capture `target` until `until`. With `delivery`, every frame is
/// converted into a pooled buffer and published; without it only
/// diagnostics are gathered. `started` gets the negotiated format once the
/// stream reaches `Streaming`.
pub(crate) fn capture(
    target: Target,
    until: RunUntil,
    delivery: Option<Delivery>,
    started: Option<SyncSender<Option<VideoFormatInfo>>>,
) -> Result<VideoRun, CaptureError> {
    let delivering = delivery.is_some();
    let what = target.what();
    let (connection, stream_name, properties, node_id, enum_formats) = match target {
        Target::Portal {
            remote,
            node_id,
            preferred_size,
        } => (
            Connection::connect_fd(remote)?,
            "streamforge-screen-probe",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
            Some(node_id),
            vec![formats::video_enum_format(preferred_size).map_err(CaptureError::backend)?],
        ),
        Target::Camera { node_name, formats } => (
            Connection::connect()?,
            "streamforge-camera",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Camera",
                *pw::keys::TARGET_OBJECT => node_name,
                // An unplugged camera ends the capture instead of the
                // session manager moving it to another camera, and one gone
                // since the listing does not fall back to another one.
                *pw::keys::NODE_DONT_RECONNECT => "true",
                "node.dont-fallback" => "true",
            },
            None,
            formats,
        ),
    };
    let state = Rc::new(RefCell::new(State::default()));
    let start = Instant::now();

    let stream = StreamBox::new(&connection.core, stream_name, properties)
        .map_err(|err| CaptureError::backend(format!("cannot create PipeWire stream: {err}")))?;
    let watch = StreamWatch::attach(&connection, &stream)?;
    let _started = match started {
        Some(started) => Some(
            stream
                .add_local_listener_with_user_data(())
                .state_changed({
                    let state = Rc::clone(&state);
                    move |_, _, _old, new| {
                        if matches!(new, StreamState::Streaming) {
                            // Only the first message matters; the owner may be gone.
                            let _ = started.try_send(state.borrow().format.clone());
                        }
                    }
                })
                .register()
                .map_err(|err| {
                    CaptureError::backend(format!("cannot register stream state listener: {err}"))
                })?,
        ),
        None => None,
    };

    let _listener = stream
        .add_local_listener_with_user_data(())
        .param_changed({
            let state = Rc::clone(&state);
            move |stream, _, id, param| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else {
                    return;
                };
                // Format changes are rare and outside the per-frame path, so
                // allocating here is acceptable.
                let mut state = state.borrow_mut();
                match formats::parse_frame_format(param) {
                    Ok((format, layout)) => {
                        state.layout = layout;
                        if delivering && layout == Some(FrameLayout::Mjpeg) && state.mjpeg.is_none()
                        {
                            state.mjpeg = Some(MjpegDecoder::new());
                        }
                        if delivering && state.layout.is_none() {
                            state.warnings.push(format!(
                                "pixel format {} cannot be converted; frames are discarded",
                                format.pixel_format
                            ));
                        }
                        state.format = Some(format);
                    }
                    Err(message) => {
                        state.layout = None;
                        state.warnings.push(message);
                        return;
                    }
                }
                if let Err(message) = request_header_meta(stream) {
                    state.warnings.push(format!(
                        "frame timestamps unavailable, cannot request buffer headers: {message}"
                    ));
                }
            }
        })
        .process({
            let state = Rc::clone(&state);
            // Real-time path: no allocation, no blocking, O(1) per buffer.
            move |stream, _| {
                let arrival_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                let mut state = state.borrow_mut();
                let state = &mut *state;
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    state.stats.buffer_unavailable();
                    return;
                };
                let header = buffer
                    .find_meta::<MetaHeader>()
                    .map(|header| (header.pts(), header.seq(), header.flags()));
                // Negative PTS means "unknown" in SPA.
                let pts_ns = header.map(|(pts, _, _)| pts).filter(|pts| *pts >= 0);
                let datas = buffer.datas_mut();
                let Some(data) = datas.first() else {
                    state.stats.observe(FrameObservation {
                        arrival_ns,
                        pts_ns,
                        seq: header.map(|(_, seq, _)| seq),
                        kind: BufferKind::Empty,
                    });
                    return;
                };
                let chunk = data.chunk();
                let (size, stride) = (chunk.size(), chunk.stride());
                let corrupted = chunk.flags().contains(ChunkFlags::CORRUPTED)
                    || header
                        .is_some_and(|(_, _, flags)| flags.contains(MetaHeaderFlags::CORRUPTED));
                let kind = if corrupted {
                    BufferKind::Corrupted
                } else if size == 0 {
                    BufferKind::Empty
                } else {
                    BufferKind::Frame
                };
                if state.buffer_memory.is_none() && kind == BufferKind::Frame {
                    state.buffer_memory = Some(formats::memory_name(data.type_()));
                }
                state.stats.observe(FrameObservation {
                    arrival_ns,
                    pts_ns,
                    seq: header.map(|(_, seq, _)| seq),
                    kind,
                });
                if kind != BufferKind::Frame {
                    return;
                }
                let Some(delivery) = &delivery else {
                    return;
                };
                // SPA: a negative stride means rows are stored bottom-up.
                if stride < 0 {
                    state.saw_bottom_up = true;
                    state.stats.frame_discarded();
                    return;
                }
                let geometry = state.format.as_ref().map(|f| (f.width, f.height));
                let delivered = match (state.layout, geometry) {
                    (Some(layout), Some((width, height))) => deliver(
                        delivery,
                        datas,
                        width,
                        height,
                        layout,
                        &mut state.mjpeg,
                        pts_ns,
                    ),
                    _ => false,
                };
                if !delivered {
                    state.stats.frame_discarded();
                }
            }
        })
        .register()
        .map_err(|err| CaptureError::backend(format!("cannot register stream listener: {err}")))?;

    let mut enum_formats = enum_formats
        .iter()
        .map(|bytes| {
            Pod::from_bytes(bytes).ok_or_else(|| CaptureError::backend("invalid video format pod"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    stream
        .connect(
            spa::utils::Direction::Input,
            node_id,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut enum_formats,
        )
        .map_err(|err| CaptureError::backend(format!("cannot connect the {what} stream: {err}")))?;

    let outcome = watch.run(&connection, &stream, until, start)?;
    let mut state = state.take();
    let elapsed = outcome.finish(what, &mut state.warnings, CaptureError::backend)?;

    if state.saw_bottom_up {
        state.warnings.push(
            "frames with bottom-up row order (negative stride) are not supported and were discarded"
                .to_owned(),
        );
    }
    let stats = state.stats.finish(elapsed);
    if stats.frames == 0 {
        state.warnings.push(match what {
            "screen" => "no frames received; screen sources only send frames when content changes"
                .to_owned(),
            _ => format!("no frames received from the {what}"),
        });
    }
    if stats.frames > 0 && stats.frames_without_pts == stats.frames {
        state.warnings.push(
            "the source sent no presentation timestamps; first_pts/last_pts are unavailable"
                .to_owned(),
        );
    }
    Ok(VideoRun {
        format: state.format,
        buffer_memory: state.buffer_memory,
        stats,
        warnings: state.warnings,
    })
}

/// Convert one frame into a pooled buffer and publish it. Returns `false`
/// if it had to be discarded. Allocates only while the pool warms up or
/// when the resolution grows, and inside the MJPEG decoder (see
/// `frame-decode`).
fn deliver(
    delivery: &Delivery,
    datas: &mut [Data],
    width: u32,
    height: u32,
    layout: FrameLayout,
    mjpeg: &mut Option<MjpegDecoder>,
    pts_ns: Option<i64>,
) -> bool {
    let (w, h) = (width as usize, height as usize);
    let row = w * media_core::PixelFormat::BYTES_PER_PIXEL;
    let Some(mut buffer) = delivery.pool.acquire(row * h) else {
        return false;
    };
    if !convert(datas, width, height, layout, mjpeg, &mut buffer) {
        return false;
    }
    let pts = pts_ns.and_then(|ns| MediaTimestamp::new(ns, 1, 1_000_000_000).ok());
    match VideoFrame::new(width, height, row, layout.target_format(), pts, buffer) {
        Ok(frame) => {
            delivery.mailbox.publish(frame);
            true
        }
        Err(_) => false,
    }
}

/// The valid bytes of a data block's chunk, and its row stride (`None`
/// for unmapped memory or bottom-up rows). A short chunk fails conversion.
fn chunk_bytes(data: &mut Data) -> Option<(&[u8], usize)> {
    let chunk = data.chunk();
    let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
    let stride = usize::try_from(chunk.stride()).ok()?;
    let memory = data.data()?;
    let start = offset.min(memory.len());
    let end = start.saturating_add(size).min(memory.len());
    Some((&memory[start..end], stride))
}

/// Convert the frame in `datas` into the tightly packed `dst`. A zero
/// stride means "tightly packed". Does not allocate, except in the MJPEG
/// decoder.
fn convert(
    datas: &mut [Data],
    width: u32,
    height: u32,
    layout: FrameLayout,
    mjpeg: &mut Option<MjpegDecoder>,
    dst: &mut [u8],
) -> bool {
    let (w, h) = (width as usize, height as usize);
    let tight = |stride: usize, row: usize| if stride == 0 { row } else { stride };
    match layout {
        FrameLayout::Packed(layout) => {
            datas
                .first_mut()
                .and_then(chunk_bytes)
                .is_some_and(|(src, stride)| {
                    let stride = tight(stride, w * media_core::PixelFormat::BYTES_PER_PIXEL);
                    formats::convert_frame(src, stride, w, h, layout, dst)
                })
        }
        FrameLayout::Rgb24(layout) => {
            datas
                .first_mut()
                .and_then(chunk_bytes)
                .is_some_and(|(src, stride)| {
                    formats::convert_rgb24(src, tight(stride, w * 3), w, h, layout, dst)
                })
        }
        FrameLayout::Mjpeg => match (mjpeg, datas.first_mut().and_then(chunk_bytes)) {
            (Some(decoder), Some((src, _))) => decoder.decode_into(src, width, height, dst).is_ok(),
            _ => false,
        },
        FrameLayout::Yuv(yuv, color) => {
            let count = yuv.plane_count();
            let planes = if count > 1 && datas.len() >= count {
                // One data block per plane.
                let mut planes = [Plane {
                    data: &[],
                    stride: 0,
                }; 3];
                for (plane, data) in planes.iter_mut().zip(datas.iter_mut()).take(count) {
                    let Some((data, stride)) = chunk_bytes(data) else {
                        return false;
                    };
                    *plane = Plane { data, stride };
                }
                Planes::separate(yuv, &planes[..count])
            } else {
                // All planes one after the other in the first block.
                let Some((data, stride)) = datas.first_mut().and_then(chunk_bytes) else {
                    return false;
                };
                let luma_row = match yuv {
                    YuvLayout::Yuy2 | YuvLayout::Uyvy => w.div_ceil(2) * 4,
                    YuvLayout::Nv12 | YuvLayout::I420 => w,
                };
                Planes::contiguous(yuv, data, tight(stride, luma_row), height)
            };
            planes
                .and_then(|planes| {
                    frame_decode::convert_yuv(
                        yuv,
                        &planes,
                        width,
                        height,
                        color,
                        dst,
                        w * media_core::PixelFormat::BYTES_PER_PIXEL,
                    )
                })
                .is_ok()
        }
    }
}

fn request_header_meta(stream: &pw::stream::Stream) -> Result<(), String> {
    let bytes = formats::header_meta_param()?;
    let pod = Pod::from_bytes(&bytes).ok_or("invalid meta pod")?;
    stream
        .update_params(&mut [pod])
        .map_err(|err| err.to_string())
}
