//! Screen capture stream: consume a portal-provided PipeWire node.

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use capture_api::{
    BufferKind, CaptureError, FrameObservation, VideoFormatInfo, VideoStats, VideoStatsAccumulator,
};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamBox, StreamFlags, StreamState};
use spa::buffer::ChunkFlags;
use spa::buffer::meta::{MetaHeader, MetaHeaderFlags};
use spa::param::ParamType;
use spa::pod::Pod;

use crate::formats;
use crate::pw::Connection;

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
    buffer_memory: Option<&'static str>,
    reached_streaming: bool,
    error: Option<String>,
    warnings: Vec<String>,
}

/// Capture `node_id` through `remote` for `duration`.
pub(crate) fn capture(
    remote: OwnedFd,
    node_id: u32,
    preferred_size: Option<(u32, u32)>,
    duration: Duration,
) -> Result<VideoRun, CaptureError> {
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

    let _core_listener = connection
        .core
        .add_listener_local()
        .error({
            let state = Rc::clone(&state);
            let main_loop = connection.main_loop.clone();
            move |id, _seq, res, message| {
                state.borrow_mut().error =
                    Some(format!("PipeWire error on object {id}: {message} ({res})"));
                main_loop.quit();
            }
        })
        .register();

    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let state = Rc::clone(&state);
            let main_loop = connection.main_loop.clone();
            move |_, _, _old, new| {
                let mut state = state.borrow_mut();
                match new {
                    StreamState::Streaming => state.reached_streaming = true,
                    StreamState::Error(message) => {
                        state.error = Some(format!("PipeWire stream error: {message}"));
                        main_loop.quit();
                    }
                    StreamState::Unconnected if state.reached_streaming => {
                        // The user stopped sharing or the source disappeared.
                        main_loop.quit();
                    }
                    _ => {}
                }
            }
        })
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
                    Ok(format) => state.format = Some(format),
                    Err(message) => {
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
                let datas = buffer.datas_mut();
                let (kind, memory) = match datas.first() {
                    None => (BufferKind::Empty, None),
                    Some(data) => {
                        let chunk = data.chunk();
                        let corrupted = chunk.flags().contains(ChunkFlags::CORRUPTED)
                            || header.is_some_and(|(_, _, flags)| {
                                flags.contains(MetaHeaderFlags::CORRUPTED)
                            });
                        let kind = if corrupted {
                            BufferKind::Corrupted
                        } else if chunk.size() == 0 {
                            BufferKind::Empty
                        } else {
                            BufferKind::Frame
                        };
                        (kind, Some(formats::memory_name(data.type_())))
                    }
                };
                if state.buffer_memory.is_none() && kind == BufferKind::Frame {
                    state.buffer_memory = memory;
                }
                state.stats.observe(FrameObservation {
                    arrival_ns,
                    // Negative PTS means "unknown" in SPA.
                    pts_ns: header.map(|(pts, _, _)| pts).filter(|pts| *pts >= 0),
                    seq: header.map(|(_, seq, _)| seq),
                    kind,
                });
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

    let ran_full_duration = connection.run_for(duration)?;
    let elapsed = start.elapsed();
    // Disconnecting emits a final state change; the loop is no longer running.
    let disconnect = stream.disconnect();

    let mut state = state.take();
    if let Err(err) = disconnect {
        state
            .warnings
            .push(format!("disconnecting the stream failed: {err}"));
    }
    if let Some(message) = state.error {
        return Err(CaptureError::backend(message));
    }
    if !state.reached_streaming {
        return Err(CaptureError::timeout(format!(
            "the screen stream did not start within {} ms; the portal backend may not support \
             shared-memory buffers for this source",
            duration.as_millis()
        )));
    }
    if !ran_full_duration {
        state
            .warnings
            .push("the stream ended before the probe duration elapsed".to_owned());
    }
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

fn request_header_meta(stream: &pw::stream::Stream) -> Result<(), String> {
    let bytes = formats::header_meta_param()?;
    let pod = Pod::from_bytes(&bytes).ok_or("invalid meta pod")?;
    stream
        .update_params(&mut [pod])
        .map_err(|err| err.to_string())
}
