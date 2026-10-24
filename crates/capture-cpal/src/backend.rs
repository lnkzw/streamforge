//! The cpal capture backend (Windows, macOS); playback for the monitor is
//! in `playback`.
//!
//! Each audio stream owns a thread that builds the cpal stream (not `Send`
//! on every host), plays it and waits for the stop signal. The cpal
//! callback runs on the OS real-time audio thread. It converts into
//! preallocated buffers and hands the result to the bounded `AudioRing`
//! with `try_push`, so it never allocates and never waits for a lock:
//! - frames the ring cannot take yet (the mixer holds its lock) wait in a
//!   pending buffer for the next callback; only if that fills are frames
//!   dropped, counted as ring overrun;
//! - statistics are try-locked too; frames left out of them are counted.
//!
//! An input named `<output>.monitor` captures what the output plays
//! (desktop audio): cpal opens an input stream on the output device, which
//! is WASAPI loopback on Windows and a process tap of all apps on macOS
//! (14.6 or later, with the permission to record system audio). It records
//! in the output's current format (on Windows, its mix format), converted
//! like any input; so with more than two channels, only the first two.
//! - WASAPI's loopback delivers nothing while nothing plays, so the input
//!   would sit in warm-up and the mixer would count underruns. As OBS does,
//!   the capture plays silence on the output for as long as it runs.
//! - On macOS, cpal opens an output that is an input too (a duplex device,
//!   e.g. many USB interfaces) as that input, so such outputs have no
//!   monitor. WASAPI endpoints are never both.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use capture_api::{
    AudioCaptureCapability, AudioCaptureReport, AudioCaptureRequest, AudioFormatInfo, AudioInput,
    AudioOutput, AudioStatsAccumulator, AudioStream, AudioStreamRequest, CaptureBackend,
    CaptureCapabilities, CaptureError, CaptureErrorKind, CaptureState, PlaybackRequest,
    PlaybackStream, ScreenCaptureCapability, ScreenCaptureRequest, ScreenSelection,
    VideoCaptureReport, VideoStream,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use media_core::{AudioRing, SampleReader};

use crate::convert::Converter;
use crate::liveness::Liveness;
use crate::loopback;
use crate::playback;

/// Seconds of audio the ring holds (as on Linux).
const RING_SECONDS: usize = 1;

/// Audio capture through the platform's default cpal host.
#[derive(Debug, Default)]
pub struct CpalCapture;

impl CpalCapture {
    pub fn new() -> Self {
        Self
    }
}

pub(crate) fn host() -> cpal::Host {
    cpal::default_host()
}

fn backend_name(host: &cpal::Host) -> String {
    format!("cpal ({})", host.id().name())
}

fn screen_unsupported() -> String {
    format!(
        "screen capture on {} is not part of the cpal backend (capture-platform adds the native one); audio capture works",
        std::env::consts::OS
    )
}

impl CaptureBackend for CpalCapture {
    fn capabilities(&mut self) -> CaptureCapabilities {
        let host = host();
        let backend = backend_name(&host);
        let any_input = host
            .input_devices()
            .is_ok_and(|mut devices| devices.next().is_some())
            || host
                .output_devices()
                .is_ok_and(|mut devices| devices.any(|device| is_loopback_output(&device)));
        let audio = if any_input {
            AudioCaptureCapability {
                backend: Some(backend.clone()),
                available: true,
                unavailable_reason: None,
                server_version: None,
            }
        } else {
            AudioCaptureCapability::unavailable(
                Some(&backend),
                "no audio input found (no input device, and no output whose playback can be \
                 captured); connect a microphone or enable a device",
            )
        };
        CaptureCapabilities {
            platform: std::env::consts::OS.to_owned(),
            desktop: None,
            session_type: None,
            screen: ScreenCaptureCapability::unavailable(None, screen_unsupported()),
            audio,
        }
    }

    fn audio_inputs(&mut self) -> Result<Vec<AudioInput>, CaptureError> {
        let host = host();
        let inputs = host
            .input_devices()
            .map_err(|err| capture_error(&err, "cannot list audio inputs"))?
            .map(|device| AudioInput {
                name: device_name(&device),
                description: description(&device),
                media_class: "Audio/Source".to_owned(),
                loopback_of: None,
            })
            .collect();
        let outputs = loopback_outputs(&host)?;
        Ok(loopback::with_monitors(
            inputs,
            outputs
                .iter()
                .map(|device| (device_name(device), description(device))),
        ))
    }

    fn probe_screen(
        &mut self,
        _request: &ScreenCaptureRequest,
    ) -> Result<VideoCaptureReport, CaptureError> {
        Err(CaptureError::unsupported(screen_unsupported()))
    }

    fn probe_audio(
        &mut self,
        request: &AudioCaptureRequest,
    ) -> Result<AudioCaptureReport, CaptureError> {
        let stream = self.start_audio_stream(&AudioStreamRequest {
            input: request.input.clone(),
            rate: 48_000,
            channels: 2,
        })?;
        thread::sleep(request.duration);
        stream.stop()
    }

    fn start_screen_stream(
        &mut self,
        _selection: &ScreenSelection,
    ) -> Result<Box<dyn VideoStream>, CaptureError> {
        Err(CaptureError::unsupported(screen_unsupported()))
    }

    fn start_audio_stream(
        &mut self,
        request: &AudioStreamRequest,
    ) -> Result<Box<dyn AudioStream>, CaptureError> {
        if request.channels != 2 {
            return Err(CaptureError::new(
                CaptureErrorKind::InvalidRequest,
                format!(
                    "only stereo is supported, got {} channels",
                    request.channels
                ),
            ));
        }
        if request.rate == 0 {
            return Err(CaptureError::new(
                CaptureErrorKind::InvalidRequest,
                "the sample rate must be positive",
            ));
        }
        let ring = Arc::new(AudioRing::new(
            request.channels,
            request.rate as usize * RING_SECONDS,
        ));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = mpsc::channel();
        let liveness = Arc::new(Liveness::new());
        let thread = thread::Builder::new()
            .name("audio-capture".to_owned())
            .spawn({
                let request = request.clone();
                let ring = Arc::clone(&ring);
                let liveness = Arc::clone(&liveness);
                move || run(&request, ring, &liveness, &ready_tx, &stop_rx)
            })
            .map_err(|err| {
                CaptureError::backend(format!("cannot start the capture thread: {err}"))
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Box::new(CpalStream {
                ring,
                liveness,
                stop: Some(stop_tx),
                thread: Some(thread),
            })),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                Err(thread.join().ok().and_then(Result::err).unwrap_or_else(|| {
                    CaptureError::backend("the capture thread ended during setup")
                }))
            }
        }
    }

    fn audio_outputs(&mut self) -> Result<Vec<AudioOutput>, CaptureError> {
        playback::outputs()
    }

    fn start_playback(
        &mut self,
        request: &PlaybackRequest,
        feed: SampleReader,
    ) -> Result<Box<dyn PlaybackStream>, CaptureError> {
        playback::start(request, feed)
    }
}

struct CpalStream {
    ring: Arc<AudioRing>,
    liveness: Arc<Liveness>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<Result<AudioCaptureReport, CaptureError>>>,
}

impl AudioStream for CpalStream {
    fn samples(&self) -> Arc<AudioRing> {
        Arc::clone(&self.ring)
    }

    fn state(&self) -> CaptureState {
        self.liveness.state()
    }

    fn stop(mut self: Box<Self>) -> Result<AudioCaptureReport, CaptureError> {
        self.stop.take();
        match self.thread.take() {
            Some(thread) => thread
                .join()
                .unwrap_or_else(|_| Err(CaptureError::backend("the capture thread panicked"))),
            None => Err(CaptureError::backend("the stream was already stopped")),
        }
    }
}

impl Drop for CpalStream {
    fn drop(&mut self) {
        // Closing the channel wakes the thread, which drops the cpal stream.
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Stable selection name: the host's device id, else the device name.
pub(crate) fn device_name(device: &cpal::Device) -> String {
    device
        .id()
        .map(|id| id.id().to_owned())
        .or_else(|_| device.description().map(|d| d.name().to_owned()))
        .unwrap_or_else(|_| device.to_string())
}

/// The device's description, for listings.
pub(crate) fn description(device: &cpal::Device) -> Option<String> {
    device
        .description()
        .ok()
        .map(|description| description.name().to_owned())
}

/// Whether `wanted` selects `device`: its [`device_name`] or its
/// description's name.
pub(crate) fn is_named(device: &cpal::Device, wanted: &str) -> bool {
    device_name(device) == wanted
        || device
            .description()
            .is_ok_and(|description| description.name() == wanted)
}

/// Whether an output's playback can be captured: unless it is an input
/// too (see the module docs).
fn is_loopback_output(device: &cpal::Device) -> bool {
    !device
        .supported_input_configs()
        .is_ok_and(|mut configs| configs.next().is_some())
}

/// The outputs whose playback can be captured.
fn loopback_outputs(host: &cpal::Host) -> Result<Vec<cpal::Device>, CaptureError> {
    Ok(host
        .output_devices()
        .map_err(|err| capture_error(&err, "cannot list audio outputs"))?
        .filter(is_loopback_output)
        .collect())
}

/// What a capture records: an input device, or what an output plays.
struct Source {
    device: cpal::Device,
    loopback: bool,
}

/// The default input, or the one `input` names, in the order that keeps
/// the listing's names unique (see [`loopback::with_monitors`]): an input's
/// listed name, a monitor's listed name, an input's description. A monitor
/// is selected by its listed name only, which is what the engine compares
/// with the monitor's device to warn of feedback.
fn find_source(host: &cpal::Host, input: Option<&str>) -> Result<Source, CaptureError> {
    let Some(wanted) = input else {
        let device = host.default_input_device().ok_or_else(|| {
            CaptureError::new(
                CaptureErrorKind::NotFound,
                "there is no default audio input; name one from list_audio_inputs",
            )
        })?;
        return Ok(Source {
            device,
            loopback: false,
        });
    };
    let devices = host
        .input_devices()
        .map_err(|err| capture_error(&err, "cannot list audio inputs"))?;
    let mut inputs: Vec<cpal::Device> = Vec::new();
    for device in devices {
        if device_name(&device) == wanted {
            return Ok(Source {
                device,
                loopback: false,
            });
        }
        inputs.push(device);
    }
    let mut outputs = loopback_outputs(host)?;
    if let Some(output) = loopback::monitored(wanted)
        && let Some(index) = outputs
            .iter()
            .position(|device| device_name(device) == output)
    {
        return Ok(Source {
            device: outputs.swap_remove(index),
            loopback: true,
        });
    }
    if let Some(index) = inputs.iter().position(|device| is_named(device, wanted)) {
        return Ok(Source {
            device: inputs.swap_remove(index),
            loopback: false,
        });
    }
    let available: Vec<String> = inputs
        .iter()
        .map(device_name)
        .chain(
            outputs
                .iter()
                .map(|device| loopback::monitor_name(&device_name(device))),
        )
        .collect();
    Err(CaptureError::new(
        CaptureErrorKind::NotFound,
        format!("no audio input named `{wanted}`"),
    )
    .with_detail("available", available))
}

/// The device config to capture with: at the rate the device runs at now
/// (its default config), preferring f32 and stereo at that rate. Asking
/// for another rate would make CoreAudio switch the device's rate for every
/// app, or fail while another app uses it; the converter resamples instead.
/// A loopback records in its output's current format as it is: WASAPI
/// opens it without conversion, so only the mix format works there.
fn choose_config(source: &Source) -> Result<cpal::SupportedStreamConfig, CaptureError> {
    let device = &source.device;
    if source.loopback {
        return device
            .default_output_config()
            .map_err(|err| capture_error(&err, "the output has no usable format"));
    }
    let default = device
        .default_input_config()
        .map_err(|err| capture_error(&err, "the input has no usable format"))?;
    let ranges = device
        .supported_input_configs()
        .map_err(|err| capture_error(&err, "cannot read the input formats"))?;
    Ok(best_config(ranges, default.sample_rate()).unwrap_or(default))
}

/// The best of `ranges` at `rate` in a sample format this backend
/// converts (capture and playback), preferring `f32`, then stereo.
pub(crate) fn best_config(
    ranges: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    rate: u32,
) -> Option<cpal::SupportedStreamConfig> {
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
    ranges
        .filter(|range| usable(range.sample_format()))
        .filter_map(|range| {
            let score = u8::from(range.sample_format() == SampleFormat::F32) * 2
                + u8::from(range.channels() == 2);
            range
                .try_with_sample_rate(rate)
                .map(|config| (score, config))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, config)| config)
}

/// State moved into the cpal data callback (see the module docs).
struct Callback {
    converter: Converter,
    /// Converted stereo output of one device buffer; flushed when it fills.
    scratch: Vec<f32>,
    sink: Sink,
    start: Instant,
    liveness: Arc<Liveness>,
}

/// Where converted frames go: the ring and the statistics.
struct Sink {
    ring: Arc<AudioRing>,
    /// Frames waiting for the ring, in order: `pending[..pending_len]`.
    pending: Vec<f32>,
    pending_len: usize,
    /// Levels and timing of the converted stereo stream.
    stats: Arc<Mutex<AudioStatsAccumulator>>,
    /// Frames delivered while `stats` was busy (only possible at teardown).
    unobserved_frames: Arc<AtomicU64>,
}

impl Callback {
    fn on_data<T>(&mut self, data: &[T])
    where
        T: Sample,
        f32: FromSample<T>,
    {
        let arrival = u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if data.is_empty() {
            self.sink.observe(arrival, &[]);
            return;
        }
        self.liveness.data();
        let sink = &mut self.sink;
        self.converter.convert(
            data.iter().map(|sample| sample.to_sample::<f32>()),
            &mut self.scratch,
            |part| {
                sink.observe(arrival, part);
                sink.deliver(part);
            },
        );
    }
}

impl Sink {
    /// Statistics of converted stereo frames; never waits.
    fn observe(&self, arrival: u64, part: &[f32]) {
        let frames = (part.len() / 2) as u64;
        let mut stats = match self.stats.try_lock() {
            Ok(stats) => stats,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                self.unobserved_frames.fetch_add(frames, Ordering::Relaxed);
                return;
            }
        };
        stats.observe_buffer(arrival, frames, part.iter().copied(), None);
    }

    /// Hand frames to the ring without waiting, keeping their order.
    fn deliver(&mut self, part: &[f32]) {
        if self.pending_len == 0 && self.ring.try_push(part).is_some() {
            return;
        }
        let room = self.pending.len() - self.pending_len;
        let kept = part.len().min(room) / 2 * 2;
        self.pending[self.pending_len..self.pending_len + kept].copy_from_slice(&part[..kept]);
        self.pending_len += kept;
        let lost = (part.len() - kept) / 2;
        if lost > 0 {
            self.ring.count_overrun(lost);
        }
        if self
            .ring
            .try_push(&self.pending[..self.pending_len])
            .is_some()
        {
            self.pending_len = 0;
        }
    }
}

impl Drop for Sink {
    /// Teardown, not the real-time path: deliver what is still pending.
    fn drop(&mut self) {
        if self.pending_len > 0 {
            self.ring
                .push(self.pending[..self.pending_len].iter().copied());
        }
    }
}

/// A stream's error callback. Errors are rare and not in the data path;
/// it keeps the first few messages, each after `prefix`.
fn on_error(
    prefix: &'static str,
    errors: &Arc<Mutex<Vec<String>>>,
    liveness: &Arc<Liveness>,
) -> impl FnMut(cpal::Error) + Send + 'static {
    let (errors, liveness) = (Arc::clone(errors), Arc::clone(liveness));
    move |err| {
        let message = format!("{prefix}{err}");
        liveness.error(&message);
        let mut errors = errors.lock().unwrap_or_else(PoisonError::into_inner);
        if errors.len() < 8 {
            errors.push(message);
        }
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut callback: Callback,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device.build_input_stream::<T, _, _>(
        *config,
        move |data: &[T], _info| callback.on_data(data),
        on_error,
        None,
    )
}

/// Silence played on a loopback's output while it is captured, on Windows
/// (see the module docs); `None` elsewhere. WASAPI opens outputs with
/// `AUTOCONVERTPCM`, so `f32` works in any mix format. Its errors count as
/// the capture's: without it, the loopback may stall.
fn keep_playing(
    source: &Source,
    config: &cpal::StreamConfig,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Option<cpal::Stream>, CaptureError> {
    if !source.loopback || cfg!(not(target_os = "windows")) {
        return Ok(None);
    }
    let stream = source
        .device
        .build_output_stream::<f32, _, _>(
            *config,
            |data: &mut [f32], _info| data.fill(0.0),
            on_error,
            None,
        )
        .map_err(|err| capture_error(&err, "cannot play silence on the output for its loopback"))?;
    stream
        .play()
        .map_err(|err| capture_error(&err, "cannot play silence on the output for its loopback"))?;
    Ok(Some(stream))
}

/// The context of an error opening a loopback.
const LOOPBACK_OPEN: &str = if cfg!(target_os = "macos") {
    "cannot capture what the output plays (this needs macOS 14.6 or later)"
} else {
    "cannot capture what the output plays"
};

fn run(
    request: &AudioStreamRequest,
    ring: Arc<AudioRing>,
    liveness: &Arc<Liveness>,
    ready: &SyncSender<Result<(), CaptureError>>,
    stop: &Receiver<()>,
) -> Result<AudioCaptureReport, CaptureError> {
    let host = host();
    let stats = Arc::new(Mutex::new(AudioStatsAccumulator::new()));
    let unobserved_frames = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(Mutex::new(Vec::new()));
    let setup = (|| {
        let source = find_source(&host, request.input.as_deref())?;
        let supported = choose_config(&source)?;
        let format = supported.sample_format();
        let config = supported.config();
        // The statistics describe the converted stream.
        stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set_rate(request.rate);
        // One second of stereo output each: far more than one device
        // buffer, so neither fills in normal operation.
        let second = request.rate as usize * 2;
        let callback = Callback {
            converter: Converter::new(config.channels, config.sample_rate, request.rate),
            scratch: vec![0.0; second],
            sink: Sink {
                ring,
                pending: vec![0.0; second],
                pending_len: 0,
                stats: Arc::clone(&stats),
                unobserved_frames: Arc::clone(&unobserved_frames),
            },
            start: Instant::now(),
            liveness: Arc::clone(liveness),
        };
        // Started first, so the loopback has data from its first period.
        let silence = keep_playing(
            &source,
            &config,
            on_error("silence on the output: ", &errors, liveness),
        )?;
        let device = &source.device;
        let on_error = on_error("", &errors, liveness);
        let stream = match format {
            SampleFormat::F32 => build::<f32>(device, &config, callback, on_error),
            SampleFormat::F64 => build::<f64>(device, &config, callback, on_error),
            SampleFormat::I16 => build::<i16>(device, &config, callback, on_error),
            SampleFormat::I32 => build::<i32>(device, &config, callback, on_error),
            SampleFormat::U16 => build::<u16>(device, &config, callback, on_error),
            SampleFormat::I8 => build::<i8>(device, &config, callback, on_error),
            SampleFormat::U8 => build::<u8>(device, &config, callback, on_error),
            other => {
                return Err(CaptureError::unsupported(format!(
                    "sample format {other:?} is not supported"
                )));
            }
        }
        .map_err(|err| {
            let context = if source.loopback {
                LOOPBACK_OPEN
            } else {
                "cannot open the audio input"
            };
            capture_error(&err, context)
        })?;
        stream
            .play()
            .map_err(|err| capture_error(&err, "cannot start the audio input"))?;
        Ok((stream, silence, source.loopback, format, config))
    })();
    let (stream, silence, loopback, format, config) = match setup {
        Ok(setup) => {
            let _ = ready.send(Ok(()));
            setup
        }
        Err(error) => {
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };
    let started = Instant::now();
    // Until `stop` (or the stream handle is dropped).
    let _ = stop.recv();
    drop(stream);
    drop(silence);
    let mut stats = stats
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .finish(started.elapsed());
    let unobserved = unobserved_frames.load(Ordering::Relaxed);
    if unobserved > 0 {
        // Delivered, so not a deficit; only their levels are missing.
        stats.frames += unobserved;
        stats.frame_deficit = stats
            .expected_frames
            .map(|expected| expected.saturating_sub(stats.frames));
    }

    let mut warnings: Vec<String> = errors
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|message| format!("stream error: {message}"))
        .collect();
    if config.sample_rate != request.rate {
        warnings.push(format!(
            "the input runs at {} Hz; resampled linearly to {} Hz",
            config.sample_rate, request.rate
        ));
    }
    if config.channels > 2 {
        warnings.push(format!(
            "the input has {} channels; only the first two are used",
            config.channels
        ));
    }
    if unobserved > 0 {
        warnings.push(format!(
            "{unobserved} frame(s) delivered without level statistics (stopped mid-buffer)"
        ));
    }
    if stats.frames > 0 && stats.peak_dbfs.is_none() {
        warnings.push(
            if loopback {
                "the loopback delivered only digital silence: nothing played on the output; on \
                 macOS, also allow the app that runs StreamForge to record system audio (System \
                 Settings, Privacy & Security, Screen & System Audio Recording)"
            } else {
                "the input delivered only digital silence; on macOS, allow microphone access \
                 (System Settings, Privacy & Security, Microphone) for the app that runs StreamForge"
            }
            .to_owned(),
        );
    }
    Ok(AudioCaptureReport {
        backend: backend_name(&host),
        input: request.input.clone(),
        format: Some(AudioFormatInfo {
            sample_format: format!("{format:?}"),
            rate: config.sample_rate,
            channels: u32::from(config.channels),
        }),
        stats,
        warnings,
    })
}

pub(crate) fn capture_error(error: &cpal::Error, context: &str) -> CaptureError {
    use cpal::ErrorKind;
    let kind = match error.kind() {
        ErrorKind::PermissionDenied => CaptureErrorKind::PermissionDenied,
        ErrorKind::DeviceNotAvailable => CaptureErrorKind::NotFound,
        ErrorKind::DeviceBusy | ErrorKind::HostUnavailable | ErrorKind::ResourceExhausted => {
            CaptureErrorKind::Unavailable
        }
        ErrorKind::UnsupportedConfig | ErrorKind::UnsupportedOperation => {
            CaptureErrorKind::Unsupported
        }
        ErrorKind::InvalidInput => CaptureErrorKind::InvalidRequest,
        _ => CaptureErrorKind::Backend,
    };
    CaptureError::new(kind, format!("{context}: {error}"))
}
