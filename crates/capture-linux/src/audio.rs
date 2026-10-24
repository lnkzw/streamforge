//! Audio input capture: fixed-length probes and continuous streams.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use capture_api::{
    AudioCaptureReport, AudioCaptureRequest, AudioFormatInfo, AudioInput, AudioStats,
    AudioStatsAccumulator, AudioStream, AudioStreamRequest, CaptureEnd, CaptureError,
    CaptureErrorKind, CaptureState, StreamPosition,
};
use media_core::AudioRing;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamBox, StreamFlags};
use spa::param::ParamType;
use spa::pod::Pod;

use crate::formats;
use crate::pw::{BACKEND, Connection, RunUntil, StreamWatch, unlisted};

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

/// What a capture stream connects to: a source, or the monitor of a sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// The `node.name` to connect to.
    pub node: String,
    /// Capture what the sink `node` plays (`stream.capture.sink`).
    pub sink_monitor: bool,
}

/// Capture `target` (or the default input when `None`) until `until`. With
/// `delivery`, the stream requests the delivery format and pushes every
/// buffer into its ring.
pub(crate) fn capture(
    target: Option<&Target>,
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
        props.insert(*pw::keys::TARGET_OBJECT, target.node.as_str());
        if target.sink_monitor {
            props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
        }
        // A named input that goes away ends the stream; the session manager
        // must not move it to another source, which would record a
        // different microphone under this input's name. Nor may it fall
        // back to the default when the input is gone by the time the
        // stream connects (the name check uses a listing up to 2 s old).
        props.insert(*pw::keys::NODE_DONT_RECONNECT, "true");
        props.insert("node.dont-fallback", "true");
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
            if target.is_some_and(|target| target.sink_monitor) {
                "the sink played digital silence (nothing, or only silence, was playing)"
            } else {
                "the input delivered digital silence; check that the device is connected and \
                 unmuted"
            }
            .to_owned(),
        );
    }
    Ok(AudioRun {
        format: state.format,
        stats,
        warnings: state.warnings,
    })
}

/// What input `name` captures, or `NotFound` with the available names
/// unless it is in `inputs`.
pub(crate) fn resolve_input(name: &str, inputs: &[AudioInput]) -> Result<Target, CaptureError> {
    if let Some(input) = inputs.iter().find(|input| input.name == name) {
        return Ok(match &input.loopback_of {
            Some(sink) => Target {
                node: sink.clone(),
                sink_monitor: true,
            },
            None => Target {
                node: input.name.clone(),
                sink_monitor: false,
            },
        });
    }
    Err(unlisted(
        "input",
        name,
        inputs.iter().map(|input| &input.name),
    ))
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

/// `target` is the resolved `request.input` (see `LinuxCapture::check_input`).
pub(crate) fn probe(
    request: &AudioCaptureRequest,
    target: Option<Target>,
) -> Result<AudioCaptureReport, CaptureError> {
    let run = capture(target.as_ref(), RunUntil::Elapsed(request.duration), None)?;
    Ok(report(request.input.clone(), run))
}

pub(crate) fn start_stream(
    request: &AudioStreamRequest,
    target: Option<Target>,
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
    let ended = Arc::new(CaptureEnd::new());
    let thread = thread::Builder::new()
        .name("audio-capture".to_owned())
        .spawn({
            let ended = Arc::clone(&ended);
            move || {
                let run = capture(
                    target.as_ref(),
                    RunUntil::Stopped(stop_receiver),
                    Some(delivery),
                );
                // Before a stop, this is why the capture ended on its own.
                ended.end(|| match &run {
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
    ended: Arc<CaptureEnd>,
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
        // A thread that ends normally sets the reason first.
        if self
            .running
            .as_ref()
            .is_some_and(|(_, thread)| thread.is_finished())
        {
            self.ended
                .end(|| "the audio capture thread panicked".to_owned());
        }
        self.ended.state()
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
