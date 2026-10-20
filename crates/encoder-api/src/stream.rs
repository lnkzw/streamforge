//! Live streaming vocabulary: target, secret key, reconnect policy, stats.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{AudioSettings, AudioTrackStats, VideoSettings, VideoTrackStats};

const MAX_URL_LEN: usize = 2048;
const MAX_KEY_LEN: usize = 1024;

/// Secret stream key. It is never printed, serialized or put into errors;
/// only [`StreamTarget::connect_url`] reveals it, for the network layer.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct StreamKey(String);

impl StreamKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for StreamKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("StreamKey(<empty>)")
        } else {
            f.write_str("StreamKey(<redacted>)")
        }
    }
}

/// A validated RTMP(S) server URL plus its separate stream key. `Debug`
/// shows only the redacted form: URLs can embed secrets in deeper path
/// segments or the query.
#[derive(Clone, PartialEq, Eq)]
pub struct StreamTarget {
    url: String,
    key: StreamKey,
}

impl StreamTarget {
    /// Validate `url` (`rtmp://` or `rtmps://`, a host, no credentials, no
    /// whitespace) and `key` (no `/`, no whitespace). Error messages never
    /// contain the key.
    pub fn new(url: &str, key: StreamKey) -> Result<Self, String> {
        let lower = url.to_ascii_lowercase();
        let rest = ["rtmp://", "rtmps://"]
            .iter()
            .find_map(|scheme| lower.starts_with(scheme).then(|| &url[scheme.len()..]))
            .ok_or("`url` must start with rtmp:// or rtmps://")?;
        if url.len() > MAX_URL_LEN {
            return Err(format!("`url` is longer than {MAX_URL_LEN} bytes"));
        }
        if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("`url` must not contain whitespace or control characters".to_owned());
        }
        let authority = rest.split(['/', '?']).next().unwrap_or_default();
        if authority.is_empty() {
            return Err("`url` has no host".to_owned());
        }
        if authority.contains('@') {
            return Err(
                "`url` must not contain credentials; put the secret in `stream_key`".to_owned(),
            );
        }
        if key.0.len() > MAX_KEY_LEN {
            return Err(format!("`stream_key` is longer than {MAX_KEY_LEN} bytes"));
        }
        if key
            .0
            .chars()
            .any(|c| c == '/' || c.is_whitespace() || c.is_control())
        {
            return Err(
                "`stream_key` must not contain `/`, whitespace or control characters".to_owned(),
            );
        }
        Ok(Self {
            url: url.to_owned(),
            key,
        })
    }

    /// The URL to connect to, including the secret key as the last path
    /// segment (before any query). Hand it only to the network layer; never
    /// log or return it.
    pub fn connect_url(&self) -> String {
        if self.key.is_empty() {
            return self.url.clone();
        }
        let (path, query) = match self.url.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (self.url.as_str(), None),
        };
        let mut url = format!("{}/{}", path.trim_end_matches('/'), self.key.0);
        if let Some(query) = query {
            url.push('?');
            url.push_str(query);
        }
        url
    }

    /// Safe to show: scheme, host and the first path segment (the RTMP
    /// application); deeper segments, queries and the key are replaced.
    pub fn redacted(&self) -> String {
        let scheme_end = self.url.find("://").map_or(0, |i| i + 3);
        let (scheme, rest) = self.url.split_at(scheme_end);
        let (without_query, has_query) = match rest.split_once('?') {
            Some((before, _)) => (before, true),
            None => (rest, false),
        };
        let mut segments = without_query.trim_end_matches('/').split('/');
        let mut shown = format!("{scheme}{}", segments.next().unwrap_or_default());
        if let Some(app) = segments.next() {
            shown.push('/');
            shown.push_str(app);
        }
        if segments.next().is_some() {
            shown.push_str("/***");
        }
        if has_query {
            shown.push_str("?***");
        }
        if !self.key.is_empty() {
            shown.push_str("/<stream key>");
        }
        shown
    }
}

impl fmt::Debug for StreamTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StreamTarget")
            .field(&self.redacted())
            .finish()
    }
}

/// When and how often a dropped connection is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Attempts after a connection was lost; 0 disables reconnecting. The
    /// count restarts only after a connection stayed up for a while (see the
    /// streamer), so a server that accepts and drops at once still ends in
    /// [`StreamState::Failed`].
    pub max_attempts: u32,
    /// Wait before the first attempt; doubles per attempt.
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl ReconnectPolicy {
    /// Wait before attempt `attempt` (1-based).
    pub fn delay(&self, attempt: u32) -> Duration {
        let factor = 1u32
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.initial_delay
            .saturating_mul(factor)
            .min(self.max_delay)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamConfig {
    pub target: StreamTarget,
    pub video: VideoSettings,
    pub audio: AudioSettings,
    pub reconnect: ReconnectPolicy,
    /// Bound for each blocking network operation (connect, write), so a dead
    /// peer cannot stall the output or its shutdown.
    pub io_timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamState {
    Connecting,
    Connected,
    /// The connection was lost; waiting for or attempting a reconnect.
    Reconnecting,
    /// Reconnect attempts are exhausted; media is dropped (and counted)
    /// until the output is stopped.
    Failed,
    Stopped,
}

impl StreamState {
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Connecting => 0,
            Self::Connected => 1,
            Self::Reconnecting => 2,
            Self::Failed => 3,
            Self::Stopped => 4,
        }
    }

    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Connecting,
            1 => Self::Connected,
            2 => Self::Reconnecting,
            3 => Self::Failed,
            _ => Self::Stopped,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamStats {
    /// Redacted target (see [`StreamTarget::redacted`]).
    pub target: String,
    pub container: String,
    /// State when the output was stopped.
    pub state: StreamState,
    pub connects: u32,
    pub disconnects: u32,
    pub reconnect_attempts: u32,
    /// Time spent connected.
    pub connected_ms: f64,
    /// Most recent network or encoder error (never contains the key).
    pub last_error: Option<String>,
    /// Totals over all connections.
    pub video: VideoTrackStats,
    pub audio: AudioTrackStats,
    /// Media the streamer discarded because it was not connected (backoff,
    /// failed), including audio before a connection's first video frame.
    /// Included in the track `frames_dropped` / `blocks_dropped`, which also
    /// count producer-side drops (copy pool or queue full).
    pub frames_dropped_offline: u64,
    pub blocks_dropped_offline: u64,
    /// Media that was encoded but never delivered because the connection
    /// broke: frames still inside the encoder or in the failed write.
    /// Audio is counted in whole encoder frames. Included in
    /// `frames_encoded` / `samples_encoded`.
    pub frames_undelivered: u64,
    pub samples_undelivered: u64,
}

impl VideoSettings {
    /// libx264 for live streaming: `preset=veryfast`, constant bit rate
    /// (`nal-hrd=cbr`, buffer of two seconds' worth) and a keyframe exactly
    /// every two seconds (scene-cut keyframes off), as common RTMP ingests
    /// expect.
    pub fn h264_live(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Self {
        let rate = format!("{bitrate_kbps}k");
        Self {
            width,
            height,
            fps,
            encoder: "libx264".to_owned(),
            options: vec![
                ("preset".to_owned(), "veryfast".to_owned()),
                ("b".to_owned(), rate.clone()),
                ("maxrate".to_owned(), rate),
                (
                    "bufsize".to_owned(),
                    format!("{}k", u64::from(bitrate_kbps) * 2),
                ),
                ("nal-hrd".to_owned(), "cbr".to_owned()),
                ("g".to_owned(), (u64::from(fps) * 2).to_string()),
                ("keyint_min".to_owned(), (u64::from(fps) * 2).to_string()),
                ("sc_threshold".to_owned(), "0".to_owned()),
            ],
        }
    }
}

impl AudioSettings {
    /// Same AAC settings as for recording; RTMP ingests accept 48 kHz AAC.
    pub fn aac_live(rate: u32, channels: u32) -> Self {
        Self::aac(rate, channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str, key: &str) -> Result<StreamTarget, String> {
        StreamTarget::new(url, StreamKey::new(key))
    }

    #[test]
    fn keys_never_show_in_debug_or_redacted_form() {
        let target = target("rtmp://live.example.com/app", "sk_live_SECRET").expect("valid");
        assert!(!format!("{target:?}").contains("SECRET"));
        assert!(!target.redacted().contains("SECRET"));
        assert_eq!(
            target.redacted(),
            "rtmp://live.example.com/app/<stream key>"
        );
        assert_eq!(
            target.connect_url(),
            "rtmp://live.example.com/app/sk_live_SECRET"
        );
    }

    #[test]
    fn redaction_hides_deeper_paths_and_queries() {
        let t = target("rtmp://host:1935/live2/embedded-SECRET?token=x", "").expect("valid");
        assert_eq!(t.redacted(), "rtmp://host:1935/live2/***?***");
        assert_eq!(
            t.connect_url(),
            "rtmp://host:1935/live2/embedded-SECRET?token=x"
        );
        // Debug shows only the redacted form.
        assert!(!format!("{t:?}").contains("SECRET"), "{t:?}");
        // The key goes before the query.
        let with_query = target("rtmp://host/app?auth=abc", "k").expect("valid");
        assert_eq!(with_query.connect_url(), "rtmp://host/app/k?auth=abc");
        let bare = target("rtmps://host", "").expect("valid");
        assert_eq!(bare.redacted(), "rtmps://host");
    }

    #[test]
    fn invalid_targets_are_rejected_without_echoing_the_key() {
        for (url, key) in [
            ("http://host/app", "k"),
            ("rtmp://", "k"),
            ("rtmp:///app", "k"),
            ("rtmp://user:pass@host/app", "k"),
            ("rtmp://host/a pp", "k"),
            ("rtmp://host/app", "bad/SECRET"),
            ("rtmp://host/app", "bad SECRET"),
        ] {
            let err = target(url, key).expect_err(url);
            assert!(!err.contains("SECRET"), "{err}");
        }
        assert!(target("RTMP://Host/App", "k").is_ok());
    }

    #[test]
    fn reconnect_delay_doubles_up_to_the_limit() {
        let policy = ReconnectPolicy::default();
        assert_eq!(policy.delay(1), Duration::from_secs(1));
        assert_eq!(policy.delay(2), Duration::from_secs(2));
        assert_eq!(policy.delay(5), Duration::from_secs(16));
        assert_eq!(policy.delay(6), Duration::from_secs(30));
        assert_eq!(policy.delay(64), Duration::from_secs(30));
    }

    #[test]
    fn state_round_trips_through_u8() {
        for state in [
            StreamState::Connecting,
            StreamState::Connected,
            StreamState::Reconnecting,
            StreamState::Failed,
            StreamState::Stopped,
        ] {
            assert_eq!(StreamState::from_u8(state.as_u8()), state);
        }
    }
}
