//! What arrived at the other end of the stream: either the in-process
//! RTMP ingest, or a server elsewhere (e.g. MediaMTX on the local network)
//! that the harness reads the stream back from, as a player would.

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
    /// so it has not seen the start and counts no packets against the
    /// sender's.
    pub read_back: bool,
    /// Read back: steps between video packets (in decode order) longer than
    /// one and a half frames, and the longest step.
    pub video_gaps: u64,
    pub longest_video_step_s: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Track {
    pub packets: u64,
    /// First and last-ending packet timestamps.
    pub first_s: f64,
    pub end_s: f64,
}

impl From<ServerReport> for Received {
    fn from(report: ServerReport) -> Self {
        let track = |track: rtmp_test_server::TrackReport| Track {
            packets: track.packets as u64,
            first_s: track.first_s,
            end_s: track.end_s,
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
    ReadBack(JoinHandle<Received>),
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
    /// `fps` is the stream's frame rate, for finding gaps.
    pub fn read_back(url: String, fps: f64) -> Result<Self> {
        let reader = thread::Builder::new()
            .name("stream-reader".to_owned())
            .spawn(move || read(&url, fps))?;
        Ok(Self::ReadBack(reader))
    }

    /// Wait until the stream ended at the receiver.
    pub fn finish(self) -> Received {
        match self {
            Self::Local(server) => server.join().into(),
            Self::ReadBack(reader) => reader.join().unwrap_or_else(|_| Received {
                error: Some("the stream reader panicked".to_owned()),
                read_back: true,
                ..Received::default()
            }),
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
fn open(url: &str) -> Result<ff::format::context::Input, ff::Error> {
    let deadline = Instant::now() + OPEN_TIMEOUT;
    loop {
        let mut options = ff::Dictionary::new();
        // In microseconds; without it a silent server blocks the read forever.
        options.set("rw_timeout", &READ_TIMEOUT.as_micros().to_string());
        match ff::format::input_with_dictionary(url, options) {
            Ok(input) => return Ok(input),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(250)),
        }
    }
}

fn read(url: &str, fps: f64) -> Received {
    let mut received = Received {
        read_back: true,
        ..Received::default()
    };
    let mut input = match open(url) {
        Ok(input) => input,
        Err(error) => {
            received.error = Some(format!("cannot read the stream back: {error}"));
            return received;
        }
    };
    let mut tracks = Vec::new();
    for stream in input.streams() {
        let parameters = stream.parameters();
        let kind = parameters.medium();
        if kind == ff::media::Type::Video
            && let Ok(decoder) = ff::codec::context::Context::from_parameters(parameters)
                .and_then(|context| context.decoder().video())
        {
            received.width = decoder.width();
            received.height = decoder.height();
        }
        tracks.push((stream.index(), kind, f64::from(stream.time_base())));
    }
    let step_limit = 1.5 / fps;
    let mut last_video_dts = None;
    let mut packet = ff::Packet::empty();
    // The stream ends with a read error: the publisher stopped and the
    // server closed the session (or the read timed out).
    while packet.read(&mut input).is_ok() {
        let Some((_, kind, time_base)) =
            tracks.iter().find(|(index, ..)| *index == packet.stream())
        else {
            continue;
        };
        let Some(pts) = packet.pts() else { continue };
        let (start, end) = (
            pts as f64 * time_base,
            (pts + packet.duration()) as f64 * time_base,
        );
        let track = match kind {
            ff::media::Type::Video => {
                if received.video.is_none() {
                    received.first_video_is_key = packet.is_key();
                }
                if let Some(dts) = packet.dts() {
                    if let Some(last) = last_video_dts {
                        let step = (dts - last) as f64 * time_base;
                        received.longest_video_step_s = received.longest_video_step_s.max(step);
                        if step > step_limit {
                            received.video_gaps += 1;
                        }
                    }
                    last_video_dts = Some(dts);
                }
                &mut received.video
            }
            ff::media::Type::Audio => &mut received.audio,
            _ => continue,
        };
        let track = track.get_or_insert(Track {
            packets: 0,
            first_s: start,
            end_s: end,
        });
        track.packets += 1;
        track.first_s = track.first_s.min(start);
        track.end_s = track.end_s.max(end);
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
