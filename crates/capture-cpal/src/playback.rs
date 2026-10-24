//! Audio playback for the monitor (ADR 0017) through cpal: WASAPI on
//! Windows, CoreAudio on macOS.
//!
//! As for capture, each playback owns a thread that builds the cpal stream
//! (not `Send` on every host), plays it and waits for the stop signal. The
//! cpal callback runs on the OS real-time audio thread and only fills the
//! device's buffer from the feed ([`PlaybackConverter`]); it does not lock,
//! wait or allocate.
//!
//! - **Device:** the named output, or the default output as it is when the
//!   playback starts. cpal's default device follows the system default,
//!   which would move the monitor to the speakers when headphones are
//!   unplugged, where the microphones pick it up; so the default is
//!   resolved to its device first. An output that goes away ends the
//!   playback, as on Linux.
//! - **Format:** WASAPI converts any rate and layout itself (cpal opens
//!   outputs with `AUTOCONVERTPCM`), so Windows asks for the feed's rate.
//!   CoreAudio would switch the device's rate for every app, so macOS
//!   plays at the device's current rate and the converter resamples.
//!   `f32` goes straight into the device's buffer when nothing needs
//!   converting; other sample formats go through a buffer allocated before
//!   the stream starts.
//! - **End:** a stream that reported an error and has not pulled since has
//!   ended ([`Liveness`]); the thread notices within [`POLL`] and drops the
//!   stream, which closes the feed.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use capture_api::{
    AudioOutput, CaptureError, CaptureErrorKind, CaptureState, PlaybackRequest, PlaybackStream,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use media_core::SampleReader;

use crate::backend::{capture_error, device_name, host, is_named};
use crate::convert::PlaybackConverter;
use crate::liveness::Liveness;

/// Frames converted per read of the feed, and per step for sample formats
/// other than `f32`. A pull this large is one read, so the feed sees its
/// true size: far more than a device period on either host (CoreAudio's
/// largest buffer is usually 4096 frames).
const SCRATCH_FRAMES: usize = 16_384;
/// How often the playback thread checks whether the stream has ended.
const POLL: Duration = Duration::from_millis(100);

pub(crate) fn outputs() -> Result<Vec<AudioOutput>, CaptureError> {
    let devices = host()
        .output_devices()
        .map_err(|err| capture_error(&err, "cannot list audio outputs"))?;
    Ok(devices
        .map(|device| AudioOutput {
            name: device_name(&device),
            description: device
                .description()
                .ok()
                .map(|description| description.name().to_owned()),
            media_class: "Audio/Sink".to_owned(),
        })
        .collect())
}

/// Whether `feed` can be played as requested; checked before anything
/// else, as it costs nothing.
fn check_request(request: &PlaybackRequest, feed: &SampleReader) -> Result<(), CaptureError> {
    if !(1..=2).contains(&request.channels) || request.rate == 0 {
        return Err(CaptureError::unsupported(format!(
            "playback supports 1 or 2 channels at a non-zero rate, got {} Hz / {}",
            request.rate, request.channels
        )));
    }
    if feed.channels() != request.channels as usize {
        return Err(CaptureError::new(
            CaptureErrorKind::InvalidRequest,
            format!(
                "the feed carries {} channel(s), the request {}",
                feed.channels(),
                request.channels
            ),
        ));
    }
    Ok(())
}

/// Play `feed` on the requested output and return once it plays. On any
/// error the feed is dropped.
pub(crate) fn start(
    request: &PlaybackRequest,
    feed: SampleReader,
) -> Result<Box<dyn PlaybackStream>, CaptureError> {
    check_request(request, &feed)?;
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (stop_tx, stop_rx) = mpsc::channel();
    let liveness = Arc::new(Liveness::new());
    let thread = thread::Builder::new()
        .name("audio-playback".to_owned())
        .spawn({
            let request = request.clone();
            let liveness = Arc::clone(&liveness);
            move || play(&request, feed, &liveness, &ready_tx, &stop_rx)
        })
        .map_err(|err| CaptureError::backend(format!("cannot start the playback thread: {err}")))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Box::new(CpalPlayback {
            label: match &request.output {
                Some(name) => format!("audio output `{name}`"),
                None => "the default audio output".to_owned(),
            },
            liveness,
            stop: Some(stop_tx),
            thread: Some(thread),
        })),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => Err(thread.join().ok().and_then(Result::err).unwrap_or_else(|| {
            CaptureError::backend("the audio playback thread ended during setup")
        })),
    }
}

/// The named output, or the device that is the default output now.
fn find_output(host: &cpal::Host, output: Option<&str>) -> Result<cpal::Device, CaptureError> {
    let devices: Vec<cpal::Device> = host
        .output_devices()
        .map_err(|err| capture_error(&err, "cannot list audio outputs"))?
        .collect();
    let position = match output {
        Some(wanted) => devices
            .iter()
            .position(|device| is_named(device, wanted))
            .ok_or_else(|| {
                CaptureError::new(
                    CaptureErrorKind::NotFound,
                    format!("audio output `{wanted}` not found; see list_audio_outputs"),
                )
                .with_detail("output", wanted)
                .with_detail(
                    "available",
                    devices.iter().map(device_name).collect::<Vec<_>>(),
                )
            })?,
        None => {
            let default = host.default_output_device().ok_or_else(|| {
                CaptureError::new(
                    CaptureErrorKind::NotFound,
                    "no default audio output is available; choose one (see list_audio_outputs)",
                )
            })?;
            let id = default
                .id()
                .map_err(|err| capture_error(&err, "cannot identify the default audio output"))?;
            devices
                .iter()
                .position(|device| device.id().is_ok_and(|candidate| candidate == id))
                .ok_or_else(|| {
                    CaptureError::new(
                        CaptureErrorKind::Unavailable,
                        "the default audio output is not among the listed outputs; choose one \
                         (see list_audio_outputs)",
                    )
                })?
        }
    };
    Ok(devices
        .into_iter()
        .nth(position)
        .expect("index from position"))
}

/// The rate to open the output at (see the module docs).
#[cfg(target_os = "windows")]
fn play_rate(feed_rate: u32, _device_rate: u32) -> u32 {
    feed_rate
}

/// The rate to open the output at (see the module docs).
#[cfg(target_os = "macos")]
fn play_rate(_feed_rate: u32, device_rate: u32) -> u32 {
    device_rate
}

/// The output config at [`play_rate`], preferring `f32` and stereo; else
/// the device's default config, which the converter adapts to.
fn choose_config(
    device: &cpal::Device,
    feed_rate: u32,
) -> Result<cpal::SupportedStreamConfig, CaptureError> {
    let default = device
        .default_output_config()
        .map_err(|err| capture_error(&err, "the output has no usable format"))?;
    let rate = play_rate(feed_rate, default.sample_rate());
    let usable = |format: SampleFormat| {
        matches!(
            format,
            SampleFormat::F32
                | SampleFormat::F64
                | SampleFormat::I16
                | SampleFormat::I32
                | SampleFormat::U16
                | SampleFormat::I8
                | SampleFormat::U8
        )
    };
    let best = device
        .supported_output_configs()
        .map_err(|err| capture_error(&err, "cannot read the output formats"))?
        .filter(|range| usable(range.sample_format()))
        .filter_map(|range| {
            let score = u8::from(range.sample_format() == SampleFormat::F32) * 2
                + u8::from(range.channels() == 2);
            range
                .try_with_sample_rate(rate)
                .map(|config| (score, config))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, config)| config);
    Ok(best.unwrap_or(default))
}

/// A sample format the device can be fed in; `f32` is written in place.
trait DeviceSample: SizedSample + FromSample<f32> + Send + 'static {
    fn as_f32(_data: &mut [Self]) -> Option<&mut [f32]> {
        None
    }
}

impl DeviceSample for f32 {
    fn as_f32(data: &mut [f32]) -> Option<&mut [f32]> {
        Some(data)
    }
}

impl DeviceSample for f64 {}
impl DeviceSample for i32 {}
impl DeviceSample for i16 {}
impl DeviceSample for u16 {}
impl DeviceSample for i8 {}
impl DeviceSample for u8 {}

/// State moved into the cpal data callback (see the module docs).
struct Render {
    converter: PlaybackConverter,
    feed: SampleReader,
    /// Device-layout `f32` for other sample formats (empty for `f32`).
    scratch: Vec<f32>,
    device_channels: usize,
    liveness: Arc<Liveness>,
}

impl Render {
    fn on_data<T: DeviceSample>(&mut self, data: &mut [T]) {
        if data.is_empty() {
            return;
        }
        self.liveness.data();
        if let Some(data) = T::as_f32(data) {
            self.converter.fill(data, &mut self.feed);
            return;
        }
        let step = self.scratch.len() / self.device_channels * self.device_channels;
        for chunk in data.chunks_mut(step) {
            let scratch = &mut self.scratch[..chunk.len()];
            self.converter.fill(scratch, &mut self.feed);
            for (sample, value) in chunk.iter_mut().zip(scratch.iter()) {
                *sample = T::from_sample(*value);
            }
        }
    }
}

fn build<T: DeviceSample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut render: Render,
    liveness: Arc<Liveness>,
) -> Result<cpal::Stream, cpal::Error> {
    device.build_output_stream::<T, _, _>(
        *config,
        move |data: &mut [T], _info| render.on_data(data),
        // Rare and not in the data path.
        move |err| liveness.error(&err.to_string()),
        None,
    )
}

fn play(
    request: &PlaybackRequest,
    feed: SampleReader,
    liveness: &Arc<Liveness>,
    ready: &SyncSender<Result<(), CaptureError>>,
    stop: &Receiver<()>,
) -> Result<(), CaptureError> {
    let setup = (|| {
        let device = find_output(&host(), request.output.as_deref())?;
        let supported = choose_config(&device, request.rate)?;
        let format = supported.sample_format();
        let config = supported.config();
        let device_channels = usize::from(config.channels.max(1));
        let render = Render {
            converter: PlaybackConverter::new(
                feed.channels(),
                request.rate,
                config.channels,
                config.sample_rate,
                SCRATCH_FRAMES,
            ),
            feed,
            scratch: if format == SampleFormat::F32 {
                Vec::new()
            } else {
                vec![0.0; SCRATCH_FRAMES * device_channels]
            },
            device_channels,
            liveness: Arc::clone(liveness),
        };
        let liveness = Arc::clone(liveness);
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, &config, render, liveness),
            SampleFormat::F64 => build::<f64>(&device, &config, render, liveness),
            SampleFormat::I16 => build::<i16>(&device, &config, render, liveness),
            SampleFormat::I32 => build::<i32>(&device, &config, render, liveness),
            SampleFormat::U16 => build::<u16>(&device, &config, render, liveness),
            SampleFormat::I8 => build::<i8>(&device, &config, render, liveness),
            SampleFormat::U8 => build::<u8>(&device, &config, render, liveness),
            other => {
                return Err(CaptureError::unsupported(format!(
                    "sample format {other:?} is not supported"
                )));
            }
        }
        .map_err(|err| capture_error(&err, "cannot open the audio output"))?;
        stream
            .play()
            .map_err(|err| capture_error(&err, "cannot start the audio output"))?;
        Ok(stream)
    })();
    let stream = match setup {
        Ok(stream) => {
            let _ = ready.send(Ok(()));
            stream
        }
        Err(error) => {
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };
    // Until `stop` (or the handle is dropped), or until the stream ends.
    while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(POLL) {
        if liveness.state() != CaptureState::Running {
            break;
        }
    }
    // Drops the feed, so the producer sees it closed.
    drop(stream);
    Ok(())
}

struct CpalPlayback {
    /// Names the output in the reason it ended for.
    label: String,
    liveness: Arc<Liveness>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<Result<(), CaptureError>>>,
}

impl CpalPlayback {
    /// Stop the stream (if it still plays) and wait for its thread.
    fn shutdown(&mut self) -> Result<(), CaptureError> {
        // Closing the channel wakes the thread, which drops the stream.
        self.stop.take();
        match self.thread.take() {
            Some(thread) => thread.join().unwrap_or_else(|_| {
                Err(CaptureError::backend("the audio playback thread panicked"))
            }),
            None => Ok(()),
        }
    }
}

impl PlaybackStream for CpalPlayback {
    fn state(&self) -> CaptureState {
        match self.liveness.state() {
            CaptureState::Ended { reason } => CaptureState::Ended {
                reason: format!("{} stopped playing: {reason}", self.label),
            },
            // The thread ends on its own only when the stream has ended.
            CaptureState::Running
                if self
                    .thread
                    .as_ref()
                    .is_some_and(|thread| thread.is_finished()) =>
            {
                CaptureState::Ended {
                    reason: "the audio playback thread panicked".to_owned(),
                }
            }
            state => state,
        }
    }

    fn stop(mut self: Box<Self>) -> Result<(), CaptureError> {
        // An end on its own is in `state` already; only a failure of this
        // stop is an error.
        let ended_before = self.state() != CaptureState::Running;
        let run = self.shutdown();
        if ended_before { Ok(()) } else { run }
    }
}

impl Drop for CpalPlayback {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
