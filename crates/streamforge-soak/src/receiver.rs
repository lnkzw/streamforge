//! What arrived at the other end of the stream: either the in-process
//! RTMP ingest, or a server elsewhere (e.g. MediaMTX on the local network)
//! that the harness reads the stream back from, as a player would.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ffmpeg_next as ff;
use rtmp_test_server::{ServerOptions, ServerReport, TestServer};
use serde::Serialize;

/// A read that gets nothing for this long ends (FFmpeg's `rw_timeout`).
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the reader tries to open the stream after it was published.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// After the stream stopped, the server ends the reader's session; if it
/// has not after this long, the reader is cancelled.
const FINISH_TIMEOUT: Duration = Duration::from_secs(15);
/// Samples per AAC frame, for the audio packets' expected step.
const AAC_FRAME_SAMPLES: f64 = 1024.0;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Received {
    /// Why nothing (or not everything) was received.
    pub error: Option<String>,
    pub video: Option<Track>,
    pub audio: Option<Track>,
    pub width: u32,
    pub height: u32,
    pub first_video_is_key: bool,
    /// Read back from a server: the reader joined after the stream started,
    /// so it counts no packets against the sender's.
    pub read_back: bool,
    /// Read back: seconds from the outputs' start until the first packet
    /// arrived.
    pub joined_after_s: Option<f64>,
    /// Read back: how the reading ended (the server closing the session
    /// after the stream stopped, a timeout, a cancel).
    pub ended_by: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Track {
    pub packets: u64,
    /// First and last-ending packet timestamps.
    pub first_s: f64,
    pub end_s: f64,
    /// Read back: steps between packets (in decode order) longer than one
    /// and a half packet durations, and the longest step.
    pub gaps: u64,
    pub longest_step_s: f64,
}

impl From<ServerReport> for Received {
    fn from(report: ServerReport) -> Self {
        let track = |track: rtmp_test_server::TrackReport| Track {
            packets: track.packets as u64,
            first_s: track.first_s,
            end_s: track.end_s,
            ..Track::default()
        };
        Self {
            error: report.error,
            video: report.video.map(track),
            audio: report.audio.map(track),
            width: report.width,
            height: report.height,
            first_video_is_key: report.first_video_is_key,
            ..Self::default()
        }
    }
}

/// The receiving end of a run's stream.
pub enum Receiver {
    Local(TestServer),
    ReadBack {
        reader: JoinHandle<Received>,
        cancel: Arc<AtomicBool>,
    },
}

impl Receiver {
    /// An in-process ingest and the URL to stream to; it waits up to
    /// `accept_timeout` for the stream.
    pub fn local(accept_timeout: Duration) -> (Self, String) {
        let server = TestServer::start(ServerOptions {
            accept_timeout,
            ..ServerOptions::default()
        });
        let url = server.url();
        (Self::Local(server), url)
    }

    /// Read `url` back from its server, once the stream publishes there.
    /// `fps` is the stream's frame rate, for finding gaps; `started` is
    /// when the outputs started.
    pub fn read_back(url: String, fps: f64, started: Instant) -> Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&cancel);
        let reader = thread::Builder::new()
            .name("stream-reader".to_owned())
            .spawn(move || read(&url, fps, started, &cancelled))?;
        Ok(Self::ReadBack { reader, cancel })
    }

    /// Wait until the stream ended at the receiver.
    pub fn finish(self) -> Received {
        match self {
            Self::Local(server) => server.join().into(),
            Self::ReadBack { reader, cancel } => {
                let deadline = Instant::now() + FINISH_TIMEOUT;
                while !reader.is_finished() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(50));
                }
                // FFmpeg's interrupt callback ends any blocking read.
                cancel.store(true, Ordering::Relaxed);
                reader.join().unwrap_or_else(|_| Received {
                    error: Some("the stream reader panicked".to_owned()),
                    read_back: true,
                    ..Received::default()
                })
            }
        }
    }
}

/// The URL a player reads, split into the server URL `start_streaming`
/// takes and its stream key, the last path segment:
/// `rtmp://host/live/soak` publishes to `rtmp://host/live` with key `soak`.
pub fn split_stream_url(url: &str) -> Result<(String, String)> {
    let Some(rest) = url
        .strip_prefix("rtmp://")
        .or_else(|| url.strip_prefix("rtmps://"))
    else {
        bail!("--url must start with rtmp:// or rtmps://: {url}");
    };
    let segments: Vec<_> = rest.split('/').collect();
    if segments.len() < 3 || segments.iter().any(|segment| segment.is_empty()) {
        bail!("--url needs a host, an application and a stream name, e.g. rtmp://host/live/soak");
    }
    let (server, key) = url.rsplit_once('/').unwrap_or_default();
    Ok((server.to_owned(), key.to_owned()))
}

/// Open `url` for reading. Right after the publisher connected, the server
/// may not offer the stream yet (MediaMTX refuses readers until the first
/// media arrived), so failures are retried for a while.
fn open(url: &str, cancel: &Arc<AtomicBool>) -> Result<ff::format::context::Input, ff::Error> {
    let deadline = Instant::now() + OPEN_TIMEOUT;
    loop {
        let mut options = ff::Dictionary::new();
        // In microseconds; without it a silent server blocks a read until
        // the cancel.
        options.set("rw_timeout", &READ_TIMEOUT.as_micros().to_string());
        // As the publisher does (encoder-ffmpeg's stream.rs): the URL holds
        // the stream key.
        if url
            .get(..8)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("rtmps://"))
        {
            options.set("tls_verify", "1");
        }
        let cancelled = Arc::clone(cancel);
        let interrupt = move || cancelled.load(Ordering::Relaxed);
        match ff::format::input_with_interrupt_and_dictionary(url, interrupt, options) {
            Ok(input) => return Ok(input),
            Err(error) if Instant::now() >= deadline || cancel.load(Ordering::Relaxed) => {
                return Err(error);
            }
            Err(_) => thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// Counts one track's packets and the steps between them.
struct Follow {
    kind: ff::media::Type,
    time_base: f64,
    /// Steps longer than this are gaps.
    step_limit: f64,
    last_dts: Option<i64>,
}

fn read(url: &str, fps: f64, started: Instant, cancel: &Arc<AtomicBool>) -> Received {
    let mut received = Received {
        read_back: true,
        ..Received::default()
    };
    let mut input = match open(url, cancel) {
        Ok(input) => input,
        Err(error) => {
            received.error = Some(format!("cannot read the stream back: {error}"));
            return received;
        }
    };
    let mut follows = Vec::new();
    for stream in input.streams() {
        let parameters = stream.parameters();
        let kind = parameters.medium();
        let context = ff::codec::context::Context::from_parameters(parameters);
        let step_limit = match kind {
            ff::media::Type::Video => {
                if let Ok(decoder) = context.and_then(|context| context.decoder().video()) {
                    received.width = decoder.width();
                    received.height = decoder.height();
                }
                1.5 / fps
            }
            ff::media::Type::Audio => {
                let rate = context
                    .and_then(|context| context.decoder().audio())
                    .map_or(48_000, |decoder| decoder.rate());
                1.5 * AAC_FRAME_SAMPLES / f64::from(rate.max(1))
            }
            _ => continue,
        };
        follows.push((
            stream.index(),
            Follow {
                kind,
                time_base: f64::from(stream.time_base()),
                step_limit,
                last_dts: None,
            },
        ));
    }
    let mut packet = ff::Packet::empty();
    // The stream ends with a read error: the server closed the session
    // after the stream stopped, or the read timed out or was cancelled.
    loop {
        if let Err(error) = packet.read(&mut input) {
            received.ended_by = Some(error.to_string());
            break;
        }
        let Some((_, follow)) = follows
            .iter_mut()
            .find(|(index, _)| *index == packet.stream())
        else {
            continue;
        };
        let Some(pts) = packet.pts() else { continue };
        received
            .joined_after_s
            .get_or_insert_with(|| started.elapsed().as_secs_f64());
        let time_base = follow.time_base;
        let (start, end) = (
            pts as f64 * time_base,
            (pts + packet.duration()) as f64 * time_base,
        );
        let track = if follow.kind == ff::media::Type::Video {
            if received.video.is_none() {
                received.first_video_is_key = packet.is_key();
            }
            &mut received.video
        } else {
            &mut received.audio
        };
        let track = track.get_or_insert(Track {
            first_s: start,
            end_s: end,
            ..Track::default()
        });
        track.packets += 1;
        track.first_s = track.first_s.min(start);
        track.end_s = track.end_s.max(end);
        if let Some(dts) = packet.dts() {
            if let Some(last) = follow.last_dts {
                let step = (dts - last) as f64 * time_base;
                track.longest_step_s = track.longest_step_s.max(step);
                if step > follow.step_limit {
                    track.gaps += 1;
                }
            }
            follow.last_dts = Some(dts);
        }
    }
    if received.video.is_none() && received.audio.is_none() {
        received.error = Some("the stream reader received nothing".to_owned());
    }
    received
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_url_splits_into_server_and_key() {
        let (server, key) = split_stream_url("rtmp://ramrod.local/live/soak").expect("valid");
        assert_eq!(
            (server.as_str(), key.as_str()),
            ("rtmp://ramrod.local/live", "soak")
        );
        let (server, key) = split_stream_url("rtmps://host:1936/app/x/key").expect("valid");
        assert_eq!(
            (server.as_str(), key.as_str()),
            ("rtmps://host:1936/app/x", "key")
        );
        for bad in [
            "http://host/live/soak",
            "rtmp://host/live",
            "rtmp://host/live/",
            "rtmp:///live/soak",
        ] {
            assert!(split_stream_url(bad).is_err(), "{bad}");
        }
    }
}
