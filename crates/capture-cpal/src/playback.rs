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
//! - **Format:** WASAPI converts any rate itself (cpal opens outputs with
//!   `AUTOCONVERTPCM`), so Windows asks for the feed's rate, in the
//!   device's channel count (the only one cpal offers there).
//!   CoreAudio would switch the device's rate for every app, so macOS
//!   plays at the device's current rate and the converter resamples.
//!   `f32` goes straight into the device's buffer when nothing needs
//!   converting; other sample formats go through a buffer allocated before
//!   the stream starts.
//! - **End:** a stream that reported an error and has not pulled since has
//!   ended ([`Liveness`]); the thread notices within [`POLL`], keeps the
//!   reason and drops the stream, which closes the feed. On macOS that
//!   includes a change of the device's rate: cpal pauses the stream.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use capture_api::{
    AudioOutput, CaptureEnd, CaptureError, CaptureErrorKind, CaptureState, PlaybackRequest,
    PlaybackStream,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use media_core::SampleReader;

use crate::backend::{
    best_config, capture_error, description, device_name, host, is_named, may_end,
};
use crate::convert::PlaybackConverter;
use crate::liveness::Liveness;

/// Frames converted per read of the feed, and per step for sample formats
/// other than `f32`. A pull this large is one read, so the feed sees its
/// true size: far more than a device period on either host (CoreAudio's
/// largest buffer is usually 4096 frames).
const SCRATCH_FRAMES: usize = 16_384;
/// How often the playback thread checks whether the stream has ended.
const POLL: Duration = Duration::from_millis(100);
/// How long an output may take to open and pull for the first time (as on
/// Linux).
const START_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) fn outputs() -> Result<Vec<AudioOutput>, CaptureError> {
    let devices = host()
        .output_devices()
        .map_err(|err| capture_error(&err, "cannot list audio outputs"))?;
    Ok(devices
        .map(|device| AudioOutput {
            name: device_name(&device),
            description: description(&device),
            media_class: "Audio/Sink".to_owned(),
        })
        .collect())
}

/// Play `feed` on the requested output and return once the device pulls
/// from it; an output that does not within [`START_TIMEOUT`] is an error.
/// On any error the feed is dropped.
pub(crate) fn start(
    request: &PlaybackRequest,
    feed: SampleReader,
) -> Result<Box<dyn PlaybackStream>, CaptureError> {
    request.check(&feed)?;
    let deadline = Instant::now() + START_TIMEOUT;
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (stop_tx, stop_rx) = mpsc::channel();
    let liveness = Arc::new(Liveness::new());
    let ended = Arc::new(CaptureEnd::new());
    let thread = thread::Builder::new()
        .name("audio-playback".to_owned())
        .spawn({
            let request = request.clone();
            let (liveness, ended) = (Arc::clone(&liveness), Arc::clone(&ended));
            move || play(&request, feed, &liveness, &ended, &ready_tx, &stop_rx)
        })
        .map_err(|err| CaptureError::backend(format!("cannot start the playback thread: {err}")))?;
    let label = match ready_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(label)) => label,
        Ok(Err(error)) => {
            let _ = thread.join();
            return Err(error);
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Err(thread.join().ok().and_then(Result::err).unwrap_or_else(|| {
                CaptureError::backend("the audio playback thread ended during setup")
            }));
        }
        // The system hangs opening the device. The thread is left to finish
        // on its own: with the stop channel gone, it drops the stream (and
        // the feed) as soon as it has one.
        Err(RecvTimeoutError::Timeout) => {
            return Err(CaptureError::timeout(format!(
                "the output did not open within {} ms",
                START_TIMEOUT.as_millis()
            )));
        }
    };
    let mut playback = CpalPlayback {
        label,
        liveness,
        ended,
        stop: Some(stop_tx),
        thread: Some(thread),
    };
    // It plays once the device pulls.
    while !playback.liveness.has_data() {
        if let CaptureState::Ended { reason } = playback.state() {
            let _ = playback.shutdown();
            return Err(CaptureError::backend(reason));
        }
        if Instant::now() >= deadline {
            let _ = playback.shutdown();
            return Err(CaptureError::timeout(format!(
                "{} did not start playing within {} ms",
                playback.label,
                START_TIMEOUT.as_millis()
            )));
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(Box::new(playback))
}

/// Names the output in errors and end reasons: the default output by the
/// device it was when the playback started.
fn output_label(device: &cpal::Device, request: &PlaybackRequest) -> String {
    match &request.output {
        Some(name) => format!("audio output `{name}`"),
        None => {
            let name = description(device).unwrap_or_else(|| device_name(device));
            format!("the default audio output (`{name}`)")
        }
    }
}

fn stopped(label: &str, reason: &str) -> String {
    format!("{label} stopped playing: {reason}")
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
    let ranges = device
        .supported_output_configs()
        .map_err(|err| capture_error(&err, "cannot read the output formats"))?;
    Ok(best_config(ranges, play_rate(feed_rate, default.sample_rate())).unwrap_or(default))
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
    /// Device-layout `f32` for other sample formats (empty for `f32`); a
    /// whole number of device frames.
    scratch: Vec<f32>,
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
        for chunk in data.chunks_mut(self.scratch.len()) {
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
        // Rare and not in the data path; a glitch (which macOS reports on
        // the real-time thread) returns before anything is allocated.
        move |err| {
            if may_end(err.kind()) {
                liveness.error(&err.to_string());
            }
        },
        None,
    )
}

/// Run the output stream until stopped or until it ends; `ready` hears the
/// output's label once it plays, or why it did not open.
fn play(
    request: &PlaybackRequest,
    feed: SampleReader,
    liveness: &Arc<Liveness>,
    ended: &CaptureEnd,
    ready: &SyncSender<Result<String, CaptureError>>,
    stop: &Receiver<()>,
) -> Result<(), CaptureError> {
    let setup = (|| {
        let device = find_output(&host(), request.output.as_deref())?;
        let label = output_label(&device, request);
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
        Ok((stream, label))
    })();
    let (stream, label) = match setup {
        Ok((stream, label)) => {
            let _ = ready.send(Ok(label.clone()));
            (stream, label)
        }
        Err(error) => {
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };
    // Until `stop` (or the handle is dropped), or until the stream ends;
    // the end is kept, as a late pull would make the stream look alive.
    while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(POLL) {
        if let CaptureState::Ended { reason } = liveness.state() {
            ended.end(|| stopped(&label, &reason));
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
    /// Set when the thread saw the stream end.
    ended: Arc<CaptureEnd>,
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
        if let Some(reason) = self.ended.reason() {
            return CaptureState::Ended {
                reason: reason.to_owned(),
            };
        }
        match self.liveness.state() {
            CaptureState::Ended { reason } => CaptureState::Ended {
                reason: stopped(&self.label, &reason),
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
