use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU16, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ffmpeg_next as ff;

#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// RTMP application (first path segment).
    pub app: String,
    /// How long to wait for the publisher.
    pub accept_timeout: Duration,
    /// Drop the connection this long after the first packet (simulates a
    /// failing ingest); `None` reads until the publisher disconnects.
    pub close_after: Option<Duration>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            app: "live".to_owned(),
            accept_timeout: Duration::from_secs(15),
            close_after: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackReport {
    pub codec: String,
    pub packets: usize,
    /// First and last-ending packet timestamps, in seconds.
    pub first_s: f64,
    pub end_s: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServerReport {
    /// Why no publisher was received, if none was.
    pub error: Option<String>,
    pub video: Option<TrackReport>,
    pub audio: Option<TrackReport>,
    pub width: u32,
    pub height: u32,
    pub first_video_is_key: bool,
}

pub struct TestServer {
    port: u16,
    app: String,
    thread: JoinHandle<ServerReport>,
}

impl TestServer {
    /// Listen on a free local port.
    pub fn start(options: ServerOptions) -> Self {
        Self::start_on(free_port(), options)
    }

    /// Listen on `port` (e.g. again after a previous server on it ended).
    pub fn start_on(port: u16, options: ServerOptions) -> Self {
        let app = options.app.clone();
        let thread = thread::Builder::new()
            .name("rtmp-test-server".to_owned())
            .spawn(move || serve(port, &options))
            .expect("spawn test server");
        Self { port, app, thread }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The server already ended: it got no publisher (see
    /// [`ServerReport::error`]) or read one to the end.
    pub fn has_ended(&self) -> bool {
        self.thread.is_finished()
    }

    /// Server URL without a stream key: `rtmp://127.0.0.1:<port>/<app>`.
    pub fn url(&self) -> String {
        format!("rtmp://127.0.0.1:{}/{}", self.port, self.app)
    }

    /// Wait until the publisher disconnected (or the server gave up).
    pub fn join(self) -> ServerReport {
        self.thread.join().expect("test server panicked")
    }
}

/// A local port that is free right now, below the ranges that outgoing
/// connections draw their ports from (Linux 32768-60999, Windows and macOS
/// 49152-65535). FFmpeg binds the port only later, from the server thread;
/// a port from the OS's ephemeral range could meanwhile go to any outgoing
/// connection on the machine, and the server would never listen.
pub fn free_port() -> u16 {
    const FIRST: u16 = 20_000;
    const COUNT: u16 = 12_000;
    // Start at a random offset (std's hasher keys are random per process)
    // and step on, so parallel tests and processes rarely try the same port.
    static NEXT: AtomicU16 = AtomicU16::new(u16::MAX);
    let _ = NEXT.compare_exchange(
        u16::MAX,
        (RandomState::new().hash_one(std::process::id()) % u64::from(COUNT)) as u16,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    for _ in 0..COUNT {
        let offset = NEXT.fetch_add(1, Ordering::Relaxed) % COUNT;
        let port = FIRST + offset;
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!(
        "no free local port between {FIRST} and {}",
        FIRST + COUNT - 1
    );
}

fn serve(port: u16, options: &ServerOptions) -> ServerReport {
    if let Err(err) = ff::init() {
        return failed(format!("init: {err}"));
    }
    // FFmpeg's listener logs the published stream name (the key), even in
    // errors; stay silent (as the streamer does once used).
    ff::log::set_level(ff::log::Level::Quiet);
    let mut dictionary = ff::Dictionary::new();
    dictionary.set("listen", "1");
    dictionary.set(
        "timeout",
        &options.accept_timeout.as_secs().max(1).to_string(),
    );
    let url = format!("rtmp://127.0.0.1:{port}/{}/stream", options.app);
    let mut input = match ff::format::input_with_dictionary(&url, dictionary) {
        Ok(input) => input,
        Err(err) => return failed(format!("no publisher: {err}")),
    };

    let mut report = ServerReport::default();
    let mut tracks = Vec::new();
    for stream in input.streams() {
        let parameters = stream.parameters();
        let kind = parameters.medium();
        let codec = format!("{:?}", parameters.id()).to_lowercase();
        if kind == ff::media::Type::Video
            && let Ok(decoder) = ff::codec::context::Context::from_parameters(parameters)
                .and_then(|context| context.decoder().video())
        {
            report.width = decoder.width();
            report.height = decoder.height();
        }
        tracks.push((stream.index(), kind, codec, f64::from(stream.time_base())));
    }

    let started = Instant::now();
    let mut packet = ff::Packet::empty();
    let mut first_video = true;
    // Any read error (the publisher disconnected) ends the session.
    while packet.read(&mut input).is_ok() {
        if options
            .close_after
            .is_some_and(|after| started.elapsed() >= after)
        {
            break;
        }
        let Some((_, kind, codec, time_base)) =
            tracks.iter().find(|(index, ..)| *index == packet.stream())
        else {
            continue;
        };
        let Some(pts) = packet.pts() else { continue };
        let start = pts as f64 * time_base;
        let end = (pts + packet.duration()) as f64 * time_base;
        let track = match *kind {
            ff::media::Type::Video => {
                if first_video {
                    report.first_video_is_key = packet.is_key();
                    first_video = false;
                }
                &mut report.video
            }
            ff::media::Type::Audio => &mut report.audio,
            _ => continue,
        };
        let track = track.get_or_insert_with(|| TrackReport {
            codec: codec.clone(),
            packets: 0,
            first_s: start,
            end_s: end,
        });
        track.packets += 1;
        track.first_s = track.first_s.min(start);
        track.end_s = track.end_s.max(end);
    }
    // Dropping the input closes the connection.
    drop(input);
    report
}

fn failed(error: String) -> ServerReport {
    ServerReport {
        error: Some(error),
        ..ServerReport::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_ports_are_outside_the_ephemeral_ranges_and_differ() {
        let ports: Vec<_> = (0..4).map(|_| free_port()).collect();
        assert!(
            ports.iter().all(|port| (20_000..32_000).contains(port)),
            "{ports:?}"
        );
        assert!(ports.windows(2).all(|pair| pair[0] != pair[1]), "{ports:?}");
    }
}
