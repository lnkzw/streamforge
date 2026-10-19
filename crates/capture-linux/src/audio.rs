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
use pw::stream::{StreamBox, StreamFlags, StreamState};
use spa::param::ParamType;
use spa::pod::Pod;

use crate::formats;
use crate::pw::Connection;

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
    channels: usize,
    reached_streaming: bool,
    error: Option<String>,
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
                        // The device disappeared while capturing.
                        main_loop.quit();
                    }
                    _ => {}
                }
            }
        })
        .param_changed({
            let state = Rc::clone(&state);
            move |_, _, id, param| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else {
                    return;
                };
                let mut state = state.borrow_mut();
                match formats::parse_audio_format(param) {
                    Ok(format) => {
                        state.stats.set_rate(format.rate);
                        state.channels = usize::try_from(format.channels).unwrap_or(0);
                        state.format = Some(format);
                    }
                    Err(message) => state.warnings.push(message),
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
                let position = stream.time().ok().and_then(|time| {
                    let rate = time.rate();
                    Some(StreamPosition {
                        ticks: i64::try_from(time.ticks()).ok()?,
                        time_base_num: rate.num,
                        time_base_den: rate.denom,
                    })
                });
                let frame_bytes = BYTES_PER_SAMPLE * state.channels;
                let datas = buffer.datas_mut();
                let samples = match datas.first_mut() {
                    Some(data) if frame_bytes > 0 => {
                        let chunk = data.chunk();
                        let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
                        data.data().map(|memory| {
                            let end = offset.saturating_add(size).min(memory.len());
                            let bytes = &memory[offset.min(end)..end];
                            &bytes[..bytes.len() - bytes.len() % frame_bytes]
                        })
                    }
                    _ => None,
                };
                let bytes = samples.unwrap_or(&[]);
                let frames = (bytes.len() / frame_bytes.max(1)) as u64;
                state.stats.observe_buffer(
                    arrival_ns,
                    frames,
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
        // WirePlumber reports this when no default source exists (e.g. the
        // configured default device is gone); point the caller at `input`.
        if target.is_none() && message.contains("no target node") {
            return Err(CaptureError::new(
                CaptureErrorKind::NotFound,
                "no default audio input is available; choose one with `input` \
                 (see list_audio_inputs)",
            )
            .with_details(serde_json::json!({ "backend_message": message })));
        }
        return Err(CaptureError::backend(message));
    }
    if !state.reached_streaming {
        return Err(CaptureError::timeout(format!(
            "the audio stream did not start within {} ms",
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
