//! Audio input capture stream.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use capture_api::{
    AudioFormatInfo, AudioStats, AudioStatsAccumulator, CaptureError, CaptureErrorKind,
    StreamPosition,
};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamBox, StreamFlags};
use spa::param::ParamType;
use spa::pod::Pod;

use crate::formats;
use crate::pw::{Connection, RunUntil, StreamWatch};

const BYTES_PER_SAMPLE: usize = std::mem::size_of::<f32>();

pub(crate) struct AudioRun {
    pub format: Option<AudioFormatInfo>,
    pub stats: AudioStats,
    pub warnings: Vec<String>,
}

#[derive(Default)]
struct State {
    stats: AudioStatsAccumulator,
    format: Option<AudioFormatInfo>,
    /// Channel count of the negotiated format; 0 until a usable format exists.
    channels: usize,
    warnings: Vec<String>,
}

/// Capture `target` (a `node.name`, or the default input when `None`) for `duration`.
pub(crate) fn capture(target: Option<&str>, duration: Duration) -> Result<AudioRun, CaptureError> {
    let connection = Connection::connect()?;
    let state = Rc::new(RefCell::new(State::default()));
    let start = Instant::now();

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Production",
    };
    if let Some(target) = target {
        props.insert(*pw::keys::TARGET_OBJECT, target);
    }
    let stream = StreamBox::new(&connection.core, "streamforge-audio-probe", props)
        .map_err(|err| CaptureError::backend(format!("cannot create PipeWire stream: {err}")))?;
    let watch = StreamWatch::attach(&connection, &stream)?;

    let _listener = stream
        .add_local_listener_with_user_data(())
        .param_changed({
            let state = Rc::clone(&state);
            move |_, _, id, param| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let mut state = state.borrow_mut();
                let Some(param) = param else {
                    // Format cleared: stop decoding until a new one arrives.
                    state.channels = 0;
                    return;
                };
                match formats::parse_audio_format(param) {
                    Ok(format) => {
                        state.stats.set_rate(format.rate);
                        state.channels = usize::try_from(format.channels).unwrap_or(0);
                        state.format = Some(format);
                    }
                    Err(message) => {
                        state.channels = 0;
                        state.warnings.push(message);
                    }
                }
            }
        })
        .process({
            let state = Rc::clone(&state);
            // Real-time path: no allocation; samples are decoded in place.
            move |stream, _| {
                let arrival_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                let mut state = state.borrow_mut();
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    state.stats.buffer_unavailable();
                    return;
                };
                let frame_bytes = BYTES_PER_SAMPLE * state.channels;
                let datas = buffer.datas_mut();
                if frame_bytes == 0 {
                    // No usable format: data cannot be interpreted. Count it
                    // instead of letting it vanish as an "empty" buffer.
                    if datas.first().is_some_and(|data| data.chunk().size() > 0) {
                        state.stats.buffer_dropped();
                    } else {
                        state
                            .stats
                            .observe_buffer(arrival_ns, 0, std::iter::empty(), None);
                    }
                    return;
                }
                let position = stream.time().ok().and_then(|time| {
                    let rate = time.rate();
                    Some(StreamPosition {
                        ticks: i64::try_from(time.ticks()).ok()?,
                        time_base_num: rate.num,
                        time_base_den: rate.denom,
                    })
                });
                let bytes = datas
                    .first_mut()
                    .and_then(|data| {
                        let chunk = data.chunk();
                        let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
                        data.data().map(|memory| {
                            let end = offset.saturating_add(size).min(memory.len());
                            let bytes = &memory[offset.min(end)..end];
                            &bytes[..bytes.len() - bytes.len() % frame_bytes]
                        })
                    })
                    .unwrap_or(&[]);
                state.stats.observe_buffer(
                    arrival_ns,
                    (bytes.len() / frame_bytes) as u64,
                    bytes
                        .as_chunks::<BYTES_PER_SAMPLE>()
                        .0
                        .iter()
                        .map(|sample| f32::from_le_bytes(*sample)),
                    position,
                );
            }
        })
        .register()
        .map_err(|err| CaptureError::backend(format!("cannot register stream listener: {err}")))?;

    let enum_format = formats::audio_enum_format().map_err(CaptureError::backend)?;
    let enum_format = Pod::from_bytes(&enum_format)
        .ok_or_else(|| CaptureError::backend("invalid audio format pod"))?;
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [enum_format],
        )
        .map_err(|err| CaptureError::backend(format!("cannot connect audio stream: {err}")))?;

    let outcome = watch.run(&connection, &stream, RunUntil::Elapsed(duration), start)?;
    let mut state = state.take();
    let elapsed = outcome.finish("audio", &mut state.warnings, |message| {
        // WirePlumber reports this when no default source exists (e.g. the
        // configured default device is gone); point the caller at `input`.
        if target.is_none() && message.contains("no target node") {
            CaptureError::new(
                CaptureErrorKind::NotFound,
                "no default audio input is available; choose one with `input` \
                 (see list_audio_inputs)",
            )
            .with_detail("backend_message", message)
        } else {
            CaptureError::backend(message)
        }
    })?;

    let stats = state.stats.finish(elapsed);
    if stats.dropped_buffers > 0 {
        state.warnings.push(format!(
            "{} buffer(s) were dropped because no usable audio format was negotiated",
            stats.dropped_buffers
        ));
    }
    if stats.frames == 0 {
        state
            .warnings
            .push("no audio frames received from the input".to_owned());
    } else if stats.peak_dbfs.is_none() {
        state.warnings.push(
            "the input delivered digital silence; check that the device is connected and unmuted"
                .to_owned(),
        );
    }
    Ok(AudioRun {
        format: state.format,
        stats,
        warnings: state.warnings,
    })
}
