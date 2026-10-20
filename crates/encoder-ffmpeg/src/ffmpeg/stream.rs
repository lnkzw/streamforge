//! The `streamer` thread: publish FLV over RTMP, reconnecting as allowed.
//!
//! Each connection is a fresh FLV session with fresh encoders, so it starts
//! with a keyframe. Its timeline starts at its first video frame: audio
//! before that, and all media while disconnected, is dropped and counted
//! (`*_dropped_offline`). Every blocking network operation is bounded by
//! the configured I/O timeout, so neither a dead peer nor shutdown can stall
//! the thread for longer.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use ff::Dictionary;
use ffmpeg_next as ff;
use media_core::MediaTimestamp;

use super::{AudioTrack, Session, VideoTrack, init};
use crate::{
    AudioTrackStats, Item, Ready, RecorderError, RecorderErrorKind, Shared, StreamConfig,
    StreamState, StreamStats, VideoTrackStats,
};

const CONTAINER: &str = "flv";
/// A connection that stayed up this long restarts the reconnect count; a
/// server that accepts and drops at once still exhausts the attempts.
const STABLE_CONNECTION: Duration = Duration::from_secs(10);

pub(crate) fn run(
    config: StreamConfig,
    rx: Receiver<Item>,
    ready: Ready,
    shared: Arc<Shared>,
) -> Result<StreamStats, RecorderError> {
    let mut stream = Stream::new(&config, Arc::clone(&shared));
    // The first connection decides whether `Streamer::start` succeeds.
    let first = init(true).and_then(|()| stream.connect());
    let mut session = match first {
        Ok(session) => {
            let _ = ready.send(Ok(()));
            Some(session)
        }
        Err(error) => {
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };

    let mut attempt = 0;
    loop {
        if let Some(mut live) = session.take() {
            match stream.serve(&mut live, &rx) {
                Served::Stopped => {
                    stream.close(live);
                    return Ok(stream.finish(StreamState::Connected));
                }
                Served::Broken(error) => {
                    if stream.lose(live, error) >= STABLE_CONNECTION {
                        attempt = 0;
                    }
                }
            }
        }
        attempt += 1;
        if attempt > config.reconnect.max_attempts {
            stream.set_state(StreamState::Failed);
            shared.failed.store(true, Ordering::Release);
            stream.drain_until_stop(&rx);
            return Ok(stream.finish(StreamState::Failed));
        }
        if stream.wait(config.reconnect.delay(attempt), &rx) {
            return Ok(stream.finish(StreamState::Reconnecting));
        }
        stream.reconnect_attempts += 1;
        // A failed attempt is recorded in `last_error`; the loop retries.
        session = stream.connect().ok();
    }
}

enum Served {
    /// The owner asked to stop (or every producer is gone).
    Stopped,
    /// Writing or encoding failed; the connection is unusable.
    Broken(RecorderError),
}

struct Stream<'a> {
    config: &'a StreamConfig,
    shared: Arc<Shared>,
    display: String,
    connects: u32,
    disconnects: u32,
    reconnect_attempts: u32,
    connected: Duration,
    connected_since: Option<Instant>,
    last_error: Option<String>,
    video: Option<VideoTrackStats>,
    audio: Option<AudioTrackStats>,
    frames_offline: u64,
    blocks_offline: u64,
    frames_undelivered: u64,
    samples_undelivered: u64,
    /// Media time (ns) of the current connection's first video frame.
    origin_ns: Option<i64>,
}

impl<'a> Stream<'a> {
    fn new(config: &'a StreamConfig, shared: Arc<Shared>) -> Self {
        Self {
            config,
            shared,
            display: config.target.redacted(),
            connects: 0,
            disconnects: 0,
            reconnect_attempts: 0,
            connected: Duration::ZERO,
            connected_since: None,
            last_error: None,
            video: None,
            audio: None,
            frames_offline: 0,
            blocks_offline: 0,
            frames_undelivered: 0,
            samples_undelivered: 0,
            origin_ns: None,
        }
    }

    fn set_state(&self, state: StreamState) {
        self.shared.state.store(state.as_u8(), Ordering::Release);
    }

    fn connect(&mut self) -> Result<Session, RecorderError> {
        let mut options = Dictionary::new();
        // Bounds connect, handshake and every write (microseconds).
        options.set(
            "rw_timeout",
            &self.config.io_timeout.as_micros().to_string(),
        );
        let opened = Session::open(
            &self.config.target.connect_url(),
            CONTAINER,
            options,
            &self.display,
            RecorderErrorKind::Network,
            &self.config.video,
            &self.config.audio,
            Arc::clone(&self.shared),
        );
        match opened {
            Ok(session) => {
                self.connects += 1;
                self.connected_since = Some(Instant::now());
                self.origin_ns = None;
                self.set_state(StreamState::Connected);
                Ok(session)
            }
            Err(error) => {
                self.last_error = Some(error.message.clone());
                Err(error)
            }
        }
    }

    fn serve(&mut self, live: &mut Session, rx: &Receiver<Item>) -> Served {
        while let Ok(item) = rx.recv() {
            if matches!(item, Item::Stop) {
                return Served::Stopped;
            }
            if let Err(error) = self.write(live, item) {
                return Served::Broken(error);
            }
        }
        Served::Stopped
    }

    /// Shift `item` onto the connection's timeline and encode it.
    fn write(&mut self, live: &mut Session, item: Item) -> Result<(), RecorderError> {
        let end_ns = self.shared.end_ns.load(Ordering::Acquire);
        match item {
            Item::Video { frame, pts } => {
                let Some(ns) = nanos(pts) else {
                    live.video.frames_dropped += 1;
                    return Ok(());
                };
                let origin = *self.origin_ns.get_or_insert(ns);
                let pts = shifted(ns, origin);
                live.write(Item::Video { frame, pts }, end_ns.saturating_sub(origin))
            }
            Item::Audio { samples, len, pts } => {
                let Some(ns) = nanos(pts) else {
                    live.audio.blocks_dropped += 1;
                    return Ok(());
                };
                match self.origin_ns {
                    Some(origin) if ns >= origin => {
                        let pts = shifted(ns, origin);
                        live.write(
                            Item::Audio { samples, len, pts },
                            end_ns.saturating_sub(origin),
                        )
                    }
                    _ => {
                        self.blocks_offline += 1;
                        Ok(())
                    }
                }
            }
            Item::Stop => Ok(()),
        }
    }

    /// The connection broke: close it without waiting on the dead peer, keep
    /// its stats, and return how long it was up.
    fn lose(&mut self, live: Session, error: RecorderError) -> Duration {
        self.disconnects += 1;
        self.last_error = Some(error.message);
        self.set_state(StreamState::Reconnecting);
        live.abort();
        let (video, audio, _) = live.finish();
        self.absorb(&video, &audio)
    }

    /// Normal stop: flush, write the trailer, disconnect.
    fn close(&mut self, live: Session) {
        let (video, audio, finished) = live.finish();
        if let Err(error) = finished {
            self.last_error = Some(error.message);
        }
        self.absorb(&video, &audio);
    }

    /// Add a finished session's counters; returns how long it was connected.
    fn absorb(&mut self, video: &VideoTrack, audio: &AudioTrack) -> Duration {
        let lasted = self
            .connected_since
            .take()
            .map_or(Duration::ZERO, |since| since.elapsed());
        self.connected += lasted;
        // Encoded but not delivered: still inside the encoder or in the
        // failed write when the connection broke (zero after a clean close).
        self.frames_undelivered += video.undelivered();
        self.samples_undelivered += audio.undelivered();
        merge_video(&mut self.video, video.stats(0));
        merge_audio(&mut self.audio, audio.stats(0));
        lasted
    }

    /// Wait `delay` before the next attempt, counting what arrives. Returns
    /// `true` if the owner asked to stop meanwhile.
    fn wait(&mut self, delay: Duration, rx: &Receiver<Item>) -> bool {
        let deadline = Instant::now() + delay;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match rx.recv_timeout(remaining) {
                Ok(Item::Stop) | Err(RecvTimeoutError::Disconnected) => return true,
                Ok(Item::Video { .. }) => self.frames_offline += 1,
                Ok(Item::Audio { .. }) => self.blocks_offline += 1,
                Err(RecvTimeoutError::Timeout) => return false,
            }
        }
    }

    fn drain_until_stop(&mut self, rx: &Receiver<Item>) {
        for item in rx {
            match item {
                Item::Video { .. } => self.frames_offline += 1,
                Item::Audio { .. } => self.blocks_offline += 1,
                Item::Stop => break,
            }
        }
    }

    /// Final statistics; `state` is the state when the stop was requested.
    fn finish(self, state: StreamState) -> StreamStats {
        self.set_state(StreamState::Stopped);
        let settings = &self.config;
        let mut video = self.video.unwrap_or_else(|| VideoTrackStats {
            encoder: settings.video.encoder.clone(),
            width: settings.video.width,
            height: settings.video.height,
            fps: settings.video.fps,
            frames_encoded: 0,
            frames_dropped: 0,
            frames_after_end: 0,
            first_pts_ms: None,
            end_ms: None,
        });
        let mut audio = self.audio.unwrap_or_else(|| AudioTrackStats {
            encoder: "aac".to_owned(),
            rate: settings.audio.rate,
            channels: settings.audio.channels,
            bitrate: settings.audio.bitrate,
            samples_encoded: 0,
            blocks_dropped: 0,
            blocks_after_end: 0,
            gap_samples: 0,
            overlap_samples: 0,
            discontinuities: 0,
            padding_samples: 0,
            first_pts_ms: None,
            end_ms: None,
        });
        video.frames_dropped +=
            self.shared.video_dropped.load(Ordering::Relaxed) + self.frames_offline;
        audio.blocks_dropped +=
            self.shared.audio_dropped.load(Ordering::Relaxed) + self.blocks_offline;
        StreamStats {
            target: self.display,
            container: CONTAINER.to_owned(),
            state,
            connects: self.connects,
            disconnects: self.disconnects,
            reconnect_attempts: self.reconnect_attempts,
            connected_ms: self.connected.as_secs_f64() * 1000.0,
            last_error: self.last_error,
            video,
            audio,
            frames_dropped_offline: self.frames_offline,
            blocks_dropped_offline: self.blocks_offline,
            frames_undelivered: self.frames_undelivered,
            samples_undelivered: self.samples_undelivered,
        }
    }
}

fn nanos(pts: MediaTimestamp) -> Option<i64> {
    pts.to_nanoseconds()
        .ok()
        .and_then(|ns| i64::try_from(ns).ok())
}

fn shifted(ns: i64, origin: i64) -> MediaTimestamp {
    MediaTimestamp {
        ticks: ns.saturating_sub(origin),
        time_base_num: 1,
        time_base_den: 1_000_000_000,
    }
}

/// Sum counters over connections; timestamps are those of the last one.
fn merge_video(total: &mut Option<VideoTrackStats>, next: VideoTrackStats) {
    let Some(total) = total else {
        *total = Some(next);
        return;
    };
    total.encoder = next.encoder;
    total.frames_encoded += next.frames_encoded;
    total.frames_dropped += next.frames_dropped;
    total.frames_after_end += next.frames_after_end;
    total.first_pts_ms = next.first_pts_ms;
    total.end_ms = next.end_ms;
}

fn merge_audio(total: &mut Option<AudioTrackStats>, next: AudioTrackStats) {
    let Some(total) = total else {
        *total = Some(next);
        return;
    };
    total.samples_encoded += next.samples_encoded;
    total.blocks_dropped += next.blocks_dropped;
    total.blocks_after_end += next.blocks_after_end;
    total.gap_samples += next.gap_samples;
    total.overlap_samples += next.overlap_samples;
    total.discontinuities += next.discontinuities;
    total.padding_samples += next.padding_samples;
    total.first_pts_ms = next.first_pts_ms;
    total.end_ms = next.end_ms;
}
