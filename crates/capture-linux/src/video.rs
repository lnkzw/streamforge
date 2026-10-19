//! Screen capture stream: consume a portal-provided PipeWire node.

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use capture_api::{
    BufferKind, CaptureError, FrameObservation, VideoFormatInfo, VideoStats, VideoStatsAccumulator,
};
use media_core::{FrameMailbox, FramePool, MediaTimestamp, VideoFrame};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamBox, StreamFlags};
use spa::buffer::ChunkFlags;
use spa::buffer::meta::{MetaHeader, MetaHeaderFlags};
use spa::param::ParamType;
use spa::pod::Pod;

use crate::formats::{self, PixelLayout};
use crate::pw::{Connection, RunUntil, StreamWatch};

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
    layout: Option<PixelLayout>,
    buffer_memory: Option<&'static str>,
    warnings: Vec<String>,
}

/// Capture `node_id` through `remote` until `until`. With `delivery`, every
/// frame is converted into a pooled buffer and published; without it only
/// diagnostics are gathered.
pub(crate) fn capture(
    remote: OwnedFd,
    node_id: u32,
    preferred_size: Option<(u32, u32)>,
    until: RunUntil,
    delivery: Option<Delivery>,
) -> Result<VideoRun, CaptureError> {
    let delivering = delivery.is_some();
    let connection = Connection::connect_fd(remote)?;
    let state = Rc::new(RefCell::new(State::default()));
    let start = Instant::now();

    let stream = StreamBox::new(
        &connection.core,
        "streamforge-screen-probe",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|err| CaptureError::backend(format!("cannot create PipeWire stream: {err}")))?;
    let watch = StreamWatch::attach(&connection, &stream)?;

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
                match formats::parse_video_format(param) {
                    Ok((format, raw)) => {
                        state.layout = formats::pixel_layout(raw);
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
                let Some(data) = datas.first_mut() else {
                    state.stats.observe(FrameObservation {
                        arrival_ns,
                        pts_ns,
                        seq: header.map(|(_, seq, _)| seq),
                        kind: BufferKind::Empty,
                    });
                    return;
                };
                let chunk = data.chunk();
                let (offset, size, stride) =
                    (chunk.offset() as usize, chunk.size(), chunk.stride());
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
                let geometry = state.format.as_ref().map(|f| (f.width, f.height));
                let delivered = match (state.layout, geometry, data.data()) {
                    (Some(layout), Some((width, height)), Some(memory)) => deliver(
                        delivery,
                        &memory[offset.min(memory.len())..],
                        stride,
                        width,
                        height,
                        layout,
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

    let enum_format = formats::video_enum_format(preferred_size).map_err(CaptureError::backend)?;
    let enum_format = Pod::from_bytes(&enum_format)
        .ok_or_else(|| CaptureError::backend("invalid video format pod"))?;
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [enum_format],
        )
        .map_err(|err| {
            CaptureError::backend(format!("cannot connect to PipeWire node {node_id}: {err}"))
        })?;

    let outcome = watch.run(&connection, &stream, until, start)?;
    let mut state = state.take();
    let elapsed = outcome.finish("screen", &mut state.warnings, CaptureError::backend)?;

    let stats = state.stats.finish(elapsed);
    if stats.frames == 0 {
        state.warnings.push(
            "no frames received; screen sources only send frames when content changes".to_owned(),
        );
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
/// when the resolution grows.
fn deliver(
    delivery: &Delivery,
    source: &[u8],
    stride: i32,
    width: u32,
    height: u32,
    layout: PixelLayout,
    pts_ns: Option<i64>,
) -> bool {
    let (w, h) = (width as usize, height as usize);
    let row = w * media_core::PixelFormat::BYTES_PER_PIXEL;
    // A non-positive chunk stride means "tightly packed".
    let stride = usize::try_from(stride)
        .ok()
        .filter(|s| *s > 0)
        .unwrap_or(row);
    let Some(mut buffer) = delivery.pool.acquire(row * h) else {
        return false;
    };
    if !formats::convert_frame(source, stride, w, h, layout, &mut buffer) {
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

fn request_header_meta(stream: &pw::stream::Stream) -> Result<(), String> {
    let bytes = formats::header_meta_param()?;
    let pod = Pod::from_bytes(&bytes).ok_or("invalid meta pod")?;
    stream
        .update_params(&mut [pod])
        .map_err(|err| err.to_string())
}
