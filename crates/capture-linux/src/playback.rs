//! Audio playback for the monitor (ADR 0017): a PipeWire output stream that
//! pulls interleaved `f32` from the engine's feed.
//!
//! The stream runs with `RT_PROCESS`, so its process callback runs on
//! PipeWire's real-time data thread. It only reads the feed
//! ([`SampleReader::read`], which pads silence and counts it) into a buffer
//! allocated before the stream starts; it does not lock, wait or allocate,
//! and it captures only `Send` data. Lifecycle callbacks run on the
//! `audio-playback` thread's main loop.

use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use capture_api::{
    AudioOutput, CaptureEnd, CaptureError, CaptureErrorKind, CaptureState, PlaybackRequest,
    PlaybackStream,
};
use media_core::SampleReader;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamBox, StreamFlags, StreamState};
use serde_json::json;
use spa::pod::Pod;

use crate::QUERY_TIMEOUT;
use crate::formats;
use crate::pw::{Connection, RunUntil, StreamWatch};

const BYTES_PER_SAMPLE: usize = size_of::<f32>();
/// The device period asked for (`node.latency`), about 5 ms at 48 kHz.
/// PipeWire may run the graph with a larger one; the engine's feed sizes
/// its buffering by the pulls it sees.
const PERIOD_FRAMES: u32 = 256;
/// Frames converted per read of the feed: PipeWire's default largest
/// quantum, so a pull is one read, and the feed sees its true size.
const SCRATCH_FRAMES: usize = 8192;

/// `NotFound` with the available names unless `name` is in `outputs`.
pub(crate) fn verify_output(name: &str, outputs: &[AudioOutput]) -> Result<(), CaptureError> {
    if outputs.iter().any(|output| output.name == name) {
        return Ok(());
    }
    Err(CaptureError::new(
        CaptureErrorKind::NotFound,
        format!("audio output `{name}` not found; see list_audio_outputs"),
    )
    .with_details(json!({
        "output": name,
        "available": outputs.iter().map(|output| &output.name).collect::<Vec<_>>(),
    })))
}

/// Play `feed` on the requested output (named outputs are checked by the
/// caller) and return once it plays. A device that does not play within
/// [`QUERY_TIMEOUT`] is an error. On any error the feed is dropped.
pub(crate) fn start(
    request: &PlaybackRequest,
    feed: SampleReader,
) -> Result<Box<dyn PlaybackStream>, CaptureError> {
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
    let (stop, stop_receiver) = pw::channel::channel();
    let (started, playing) = mpsc::sync_channel(1);
    let ended = Arc::new(CaptureEnd::new());
    let thread = thread::Builder::new()
        .name("audio-playback".to_owned())
        .spawn({
            let request = request.clone();
            let ended = Arc::clone(&ended);
            move || {
                let run = play(&request, feed, stop_receiver, started);
                // Before a stop, this is why the playback ended on its own.
                ended.end(|| match &run {
                    Ok(()) => "the PipeWire stream ended (the output went away)".to_owned(),
                    Err(error) => error.message.clone(),
                });
                run
            }
        })
        .map_err(|err| CaptureError::backend(format!("cannot start the playback thread: {err}")))?;
    let mut playback = LinuxPlayback {
        ended,
        running: Some((stop, thread)),
    };
    match playing.recv_timeout(QUERY_TIMEOUT) {
        Ok(()) => Ok(Box::new(playback)),
        // Not playing: the thread's result says why.
        Err(_) => Err(playback.shutdown().err().unwrap_or_else(|| {
            CaptureError::timeout(format!(
                "the output did not start playing within {} ms",
                QUERY_TIMEOUT.as_millis()
            ))
        })),
    }
}

/// Run the output stream until stopped or until it ends; `started` hears
/// once it plays.
fn play(
    request: &PlaybackRequest,
    mut feed: SampleReader,
    stop: pw::channel::Receiver<()>,
    started: SyncSender<()>,
) -> Result<(), CaptureError> {
    let connection = Connection::connect()?;
    let start = Instant::now();
    let channels = feed.channels();

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Production",
    };
    props.insert(
        *pw::keys::NODE_LATENCY,
        format!("{PERIOD_FRAMES}/{}", request.rate),
    );
    if let Some(output) = &request.output {
        props.insert(*pw::keys::TARGET_OBJECT, output.as_str());
        // An output that goes away (headphones unplugged) ends the
        // playback; the session manager must not move the monitor to the
        // speakers, where the microphones would pick it up.
        props.insert(*pw::keys::NODE_DONT_RECONNECT, "true");
    }
    let stream = StreamBox::new(&connection.core, "streamforge-monitor", props)
        .map_err(|err| CaptureError::backend(format!("cannot create PipeWire stream: {err}")))?;
    let watch = StreamWatch::attach(&connection, &stream)?;

    let mut started = Some(started);
    let _state_listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_, _, _old, new| {
            if matches!(new, StreamState::Streaming)
                && let Some(started) = started.take()
            {
                // The starter may have given up waiting.
                let _ = started.send(());
            }
        })
        .register()
        .map_err(|err| {
            CaptureError::backend(format!("cannot register stream state listener: {err}"))
        })?;
    // Its own listener, so the data thread shares no callback state with
    // the main loop's. Allocated here: the callback must not.
    let mut scratch = vec![0.0_f32; SCRATCH_FRAMES * channels];
    let _process_listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            // Real-time path: no lock, wait or allocation.
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested();
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let frames = data
                .data()
                .map_or(0, |memory| fill(memory, requested, &mut feed, &mut scratch));
            let stride = channels * BYTES_PER_SAMPLE;
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            // Both fit: the buffer's memory is at most `u32::MAX` bytes.
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (frames * stride) as u32;
        })
        .register()
        .map_err(|err| CaptureError::backend(format!("cannot register stream listener: {err}")))?;

    let enum_format = formats::audio_enum_format(Some((request.rate, request.channels)))
        .map_err(CaptureError::backend)?;
    let enum_format = Pod::from_bytes(&enum_format)
        .ok_or_else(|| CaptureError::backend("invalid audio format pod"))?;
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut [enum_format],
        )
        .map_err(|err| CaptureError::backend(format!("cannot connect playback stream: {err}")))?;

    let outcome = watch.run(&connection, &stream, RunUntil::Stopped(stop), start)?;
    outcome.finish_playback(|message| playback_error(request.output.as_deref(), message))
}

/// Name the usual causes: WirePlumber reports a missing target as a core
/// error, e.g. when a named output was unplugged.
fn playback_error(output: Option<&str>, message: String) -> CaptureError {
    let not_found = |text: String| {
        CaptureError::new(CaptureErrorKind::NotFound, text)
            .with_detail("backend_message", message.clone())
    };
    match output {
        None if message.contains("no target node") => not_found(
            "no default audio output is available; choose one (see list_audio_outputs)".to_owned(),
        ),
        Some(name) if message.contains("target not found") => not_found(format!(
            "audio output `{name}` is gone; see list_audio_outputs"
        )),
        _ => CaptureError::backend(message),
    }
}

/// Fill `memory` (interleaved little-endian `f32`) from the feed: the
/// `requested` frames, or as many as fit when PipeWire does not say (0).
/// Returns the frames written. Never waits or allocates.
fn fill(memory: &mut [u8], requested: u64, feed: &mut SampleReader, scratch: &mut [f32]) -> usize {
    let channels = feed.channels();
    let stride = channels * BYTES_PER_SAMPLE;
    let fit = memory.len() / stride;
    let frames = match usize::try_from(requested) {
        Ok(0) | Err(_) => fit,
        Ok(requested) => requested.min(fit),
    };
    let step = scratch.len() / channels;
    for first in (0..frames).step_by(step) {
        let count = step.min(frames - first);
        let samples = &mut scratch[..count * channels];
        feed.read(samples);
        let bytes = &mut memory[first * stride..(first + count) * stride];
        for (bytes, sample) in bytes
            .as_chunks_mut::<BYTES_PER_SAMPLE>()
            .0
            .iter_mut()
            .zip(samples.iter())
        {
            *bytes = sample.to_le_bytes();
        }
    }
    frames
}

type Running = (
    pw::channel::Sender<()>,
    JoinHandle<Result<(), CaptureError>>,
);

struct LinuxPlayback {
    /// Set when the playback thread ends.
    ended: Arc<CaptureEnd>,
    running: Option<Running>,
}

impl LinuxPlayback {
    /// Stop the stream (if it still plays) and wait for its thread.
    fn shutdown(&mut self) -> Result<(), CaptureError> {
        let Some((stop, thread)) = self.running.take() else {
            return Ok(());
        };
        // Fails only if the thread already ended; joining reports why.
        let _ = stop.send(());
        thread
            .join()
            .unwrap_or_else(|_| Err(CaptureError::backend("the audio playback thread panicked")))
    }
}

impl PlaybackStream for LinuxPlayback {
    fn state(&self) -> CaptureState {
        // A thread that ends normally sets the reason first.
        if self
            .running
            .as_ref()
            .is_some_and(|(_, thread)| thread.is_finished())
        {
            self.ended
                .end(|| "the audio playback thread panicked".to_owned());
        }
        self.ended.state()
    }

    fn stop(mut self: Box<Self>) -> Result<(), CaptureError> {
        // An end on its own is in `state` already; only a failure of this
        // stop is an error.
        let ended_before = self.ended.reason().is_some();
        let run = self.shutdown();
        if ended_before { Ok(()) } else { run }
    }
}

impl Drop for LinuxPlayback {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use media_core::sample_ring;

    use super::*;

    fn decode(memory: &[u8]) -> Vec<f32> {
        memory
            .as_chunks::<BYTES_PER_SAMPLE>()
            .0
            .iter()
            .map(|bytes| f32::from_le_bytes(*bytes))
            .collect()
    }

    #[test]
    fn fills_the_requested_frames_then_silence() {
        let (mut writer, mut reader) = sample_ring(2, 64);
        writer.write(&[0.5, -0.5, 0.25, -0.25]);
        let mut memory = vec![0xff; 8 * 2 * BYTES_PER_SAMPLE];
        let mut scratch = vec![0.0; 16];
        let frames = fill(&mut memory, 3, &mut reader, &mut scratch);
        assert_eq!(frames, 3);
        assert_eq!(
            decode(&memory[..frames * 8]),
            [0.5, -0.5, 0.25, -0.25, 0.0, 0.0]
        );
        // Bytes past the frames are not touched.
        assert!(memory[frames * 8..].iter().all(|&byte| byte == 0xff));
        // The feed counts the silence (warm-up here: its first read).
        let stats = reader.stats();
        assert_eq!((stats.read_frames, stats.warmup_frames), (2, 1));
        assert_eq!(stats.max_read_frames, 3);
    }

    #[test]
    fn without_a_request_fills_what_fits() {
        let (_writer, mut reader) = sample_ring(1, 64);
        let mut memory = vec![0xff; 5 * BYTES_PER_SAMPLE + 3];
        let mut scratch = vec![0.0; 8];
        assert_eq!(fill(&mut memory, 0, &mut reader, &mut scratch), 5);
        assert_eq!(fill(&mut memory, 99, &mut reader, &mut scratch), 5);
        assert_eq!(decode(&memory[..20]), [0.0; 5]);
    }

    #[test]
    fn a_pull_larger_than_the_scratch_reads_in_steps() {
        let (mut writer, mut reader) = sample_ring(2, 64);
        let samples: Vec<f32> = (0..20).map(|i| i as f32).collect();
        writer.write(&samples);
        let mut memory = vec![0; 10 * 2 * BYTES_PER_SAMPLE];
        let mut scratch = vec![0.0; 3 * 2];
        assert_eq!(fill(&mut memory, 10, &mut reader, &mut scratch), 10);
        assert_eq!(decode(&memory), samples);
        let stats = reader.stats();
        assert_eq!((stats.underrun_frames, stats.warmup_frames), (0, 0));
    }

    #[test]
    fn unknown_outputs_are_not_found_with_the_names() {
        let outputs = [AudioOutput {
            name: "speakers".into(),
            description: None,
            media_class: "Audio/Sink".into(),
        }];
        verify_output("speakers", &outputs).expect("known");
        let err = verify_output("phones", &outputs).unwrap_err();
        assert_eq!(err.kind, CaptureErrorKind::NotFound);
        assert_eq!(
            err.details,
            Some(json!({"output": "phones", "available": ["speakers"]}))
        );
    }

    #[test]
    fn missing_targets_are_not_found() {
        let err = playback_error(
            Some("phones"),
            "PipeWire error on object 2: target not found (-2)".into(),
        );
        assert_eq!(err.kind, CaptureErrorKind::NotFound);
        assert_eq!(
            err.message,
            "audio output `phones` is gone; see list_audio_outputs"
        );
        assert!(err.details.is_some());
        let err = playback_error(None, "no target node available".into());
        assert_eq!(err.kind, CaptureErrorKind::NotFound);
        assert!(err.message.starts_with("no default audio output"));
        let err = playback_error(Some("phones"), "boom".into());
        assert_eq!(
            (err.kind, err.message.as_str()),
            (CaptureErrorKind::Backend, "boom")
        );
    }

    #[test]
    fn requests_the_feed_cannot_play_are_refused() {
        let (writer, reader) = sample_ring(2, 64);
        let request = PlaybackRequest {
            output: None,
            rate: 48_000,
            channels: 1,
        };
        let Err(err) = start(&request, reader) else {
            panic!("started");
        };
        assert_eq!(err.kind, CaptureErrorKind::InvalidRequest);
        // The refused feed is closed.
        assert!(writer.is_closed());
        let (_writer, reader) = sample_ring(2, 64);
        let request = PlaybackRequest {
            channels: 6,
            ..request
        };
        let Err(err) = start(&request, reader) else {
            panic!("started");
        };
        assert_eq!(err.kind, CaptureErrorKind::Unsupported);
    }
}
