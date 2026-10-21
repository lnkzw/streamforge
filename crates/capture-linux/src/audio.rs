//! Audio input capture: fixed-length probes and continuous streams.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use capture_api::{
    AudioCaptureReport, AudioCaptureRequest, AudioFormatInfo, AudioInput, AudioStats,
    AudioStatsAccumulator, AudioStream, AudioStreamRequest, CaptureError, CaptureErrorKind,
    CaptureState, StreamPosition,
};
use media_core::AudioRing;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamBox, StreamFlags};
use serde_json::json;
use spa::param::ParamType;
use spa::pod::Pod;

use crate::formats;
use crate::pw::{BACKEND, Connection, RunUntil, StreamWatch};

const BYTES_PER_SAMPLE: usize = std::mem::size_of::<f32>();
/// Ring capacity of a continuous stream, in seconds of audio.
const STREAM_RING_SECONDS: usize = 1;

pub(crate) struct AudioRun {
    pub format: Option<AudioFormatInfo>,
    pub stats: AudioStats,
    pub warnings: Vec<String>,
}

/// Where captured samples go in streaming mode.
pub(crate) struct Delivery {
    pub ring: Arc<AudioRing>,
    pub rate: u32,
    pub channels: u32,
}

#[derive(Default)]
struct State {
    stats: AudioStatsAccumulator,
    format: Option<AudioFormatInfo>,
    /// Channel count of the negotiated format; 0 until a usable format exists.
    channels: usize,
    /// Negotiated format matches the delivery format.
    deliverable: bool,
    warnings: Vec<String>,
}

/// Capture `target` (a `node.name`, or the default input when `None`) until
/// `until`. With `delivery`, the stream requests the delivery format and
/// pushes every buffer into its ring.
pub(crate) fn capture(
    target: Option<&str>,
    until: RunUntil,
    delivery: Option<Delivery>,
) -> Result<AudioRun, CaptureError> {
    let connection = Connection::connect()?;
    let state = Rc::new(RefCell::new(State::default()));
    let start = Instant::now();
    let wanted = delivery.as_ref().map(|d| (d.rate, d.channels));

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Production",
    };
    if let Some(target) = target {
        props.insert(*pw::keys::TARGET_OBJECT, target);
    }
    let stream = StreamBox::new(&connection.core, "streamforge-audio", props)
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
                    state.deliverable = false;
                    return;
                };
                match formats::parse_audio_format(param) {
                    Ok(format) => {
                        state.stats.set_rate(format.rate);
                        state.channels = usize::try_from(format.channels).unwrap_or(0);
                        state.deliverable = wanted.is_none_or(|(rate, channels)| {
                            format.rate == rate && format.channels == channels
                        });
                        if !state.deliverable {
                            state.warnings.push(format!(
                                "PipeWire negotiated {} Hz / {} channel(s) instead of the \
                                 requested format; buffers are not delivered",
                                format.rate, format.channels
                            ));
                        }
                        state.format = Some(format);
                    }
                    Err(message) => {
                        state.channels = 0;
                        state.deliverable = false;
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
                let samples = || {
                    bytes
                        .as_chunks::<BYTES_PER_SAMPLE>()
                        .0
                        .iter()
                        .map(|sample| f32::from_le_bytes(*sample))
                };
                state.stats.observe_buffer(
                    arrival_ns,
                    (bytes.len() / frame_bytes) as u64,
                    samples(),
                    position,
                );
                if let Some(delivery) = &delivery
                    && !bytes.is_empty()
                {
                    if state.deliverable {
                        delivery.ring.push(samples());
                    } else {
                        state.stats.buffer_dropped();
                    }
                }
            }
        })
        .register()
        .map_err(|err| CaptureError::backend(format!("cannot register stream listener: {err}")))?;

    let enum_format = formats::audio_enum_format(wanted).map_err(CaptureError::backend)?;
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

    let outcome = watch.run(&connection, &stream, until, start)?;
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

/// `NotFound` with the available names unless `name` is in `inputs`.
pub(crate) fn verify_input(name: &str, inputs: &[AudioInput]) -> Result<(), CaptureError> {
    if inputs.iter().any(|input| input.name == name) {
        return Ok(());
    }
    Err(CaptureError::new(
        CaptureErrorKind::NotFound,
        format!("audio input `{name}` not found; see list_audio_inputs"),
    )
    .with_details(json!({
        "input": name,
        "available": inputs.iter().map(|input| &input.name).collect::<Vec<_>>(),
    })))
}

fn report(input: Option<String>, run: AudioRun) -> AudioCaptureReport {
    AudioCaptureReport {
        backend: BACKEND.to_owned(),
        input,
        format: run.format,
        stats: run.stats,
        warnings: run.warnings,
    }
}

/// Callers check named inputs first (see `LinuxCapture::check_input`).
pub(crate) fn probe(request: &AudioCaptureRequest) -> Result<AudioCaptureReport, CaptureError> {
    let run = capture(
        request.input.as_deref(),
        RunUntil::Elapsed(request.duration),
        None,
    )?;
    Ok(report(request.input.clone(), run))
}

pub(crate) fn start_stream(
    request: &AudioStreamRequest,
) -> Result<Box<dyn AudioStream>, CaptureError> {
    if !(1..=2).contains(&request.channels) || request.rate == 0 {
        return Err(CaptureError::unsupported(format!(
            "audio streams support 1 or 2 channels at a non-zero rate, got {} Hz / {}",
            request.rate, request.channels
        )));
    }
    let ring = Arc::new(AudioRing::new(
        request.channels,
        request.rate as usize * STREAM_RING_SECONDS,
    ));
    let delivery = Delivery {
        ring: Arc::clone(&ring),
        rate: request.rate,
        channels: request.channels,
    };
    let (stop, stop_receiver) = pw::channel::channel();
    let input = request.input.clone();
    let ended = Arc::new(OnceLock::new());
    let thread = thread::Builder::new()
        .name("audio-capture".to_owned())
        .spawn({
            let ended = Arc::clone(&ended);
            move || {
                let run = capture(
                    input.as_deref(),
                    RunUntil::Stopped(stop_receiver),
                    Some(delivery),
                );
                // Before a stop, this is why the capture ended on its own.
                let _ = ended.set(match &run {
                    Ok(_) => "the PipeWire stream ended (the input went away)".to_owned(),
                    Err(error) => error.message.clone(),
                });
                run
            }
        })
        .map_err(|err| CaptureError::backend(format!("cannot start the capture thread: {err}")))?;
    Ok(Box::new(LinuxAudioStream {
        input: request.input.clone(),
        ring,
        ended,
        running: Some((stop, thread)),
    }))
}

type Running = (
    pw::channel::Sender<()>,
    JoinHandle<Result<AudioRun, CaptureError>>,
);

struct LinuxAudioStream {
    input: Option<String>,
    ring: Arc<AudioRing>,
    /// Set when the capture thread ends.
    ended: Arc<OnceLock<String>>,
    running: Option<Running>,
}

impl LinuxAudioStream {
    fn shutdown(&mut self) -> Option<Result<AudioCaptureReport, CaptureError>> {
        let (stop, thread) = self.running.take()?;
        // Fails only if the thread already ended; joining reports why.
        let _ = stop.send(());
        let run = thread
            .join()
            .unwrap_or_else(|_| Err(CaptureError::backend("the audio capture thread panicked")));
        Some(run.map(|run| report(self.input.clone(), run)))
    }
}

impl AudioStream for LinuxAudioStream {
    fn samples(&self) -> Arc<AudioRing> {
        Arc::clone(&self.ring)
    }

    fn state(&self) -> CaptureState {
        match self.ended.get() {
            Some(reason) => CaptureState::Ended {
                reason: reason.clone(),
            },
            None => CaptureState::Running,
        }
    }

    fn stop(mut self: Box<Self>) -> Result<AudioCaptureReport, CaptureError> {
        self.shutdown()
            .unwrap_or_else(|| Err(CaptureError::backend("the stream was already stopped")))
    }
}

impl Drop for LinuxAudioStream {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
