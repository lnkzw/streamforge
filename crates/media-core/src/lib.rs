//! Platform-independent media primitives.
//!
//! Keep this crate free of UI, capture, device, encoder, and OS-specific APIs.

pub mod audio;
pub mod metrics;
pub mod video;

pub use audio::{AudioRing, AudioRingStats};
pub use metrics::{LatencyHistogram, LatencySummary};
pub use video::{
    FrameBuffer, FrameMailbox, FramePool, MailboxStats, PixelFormat, VideoFrame, VideoFrameError,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Timestamp measured in an explicit rational time base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaTimestamp {
    pub ticks: i64,
    pub time_base_num: u32,
    pub time_base_den: u32,
}

impl MediaTimestamp {
    pub fn new(ticks: i64, time_base_num: u32, time_base_den: u32) -> Result<Self, MediaTimeError> {
        if time_base_num == 0 || time_base_den == 0 {
            return Err(MediaTimeError::InvalidTimeBase);
        }
        Ok(Self {
            ticks,
            time_base_num,
            time_base_den,
        })
    }

    /// Convert to nanoseconds, using checked integer arithmetic.
    pub fn to_nanoseconds(self) -> Result<i128, MediaTimeError> {
        let numerator = i128::from(self.ticks)
            .checked_mul(i128::from(self.time_base_num))
            .and_then(|v| v.checked_mul(1_000_000_000))
            .ok_or(MediaTimeError::Overflow)?;
        Ok(numerator / i128::from(self.time_base_den))
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MediaTimeError {
    #[error("time base numerator and denominator must be non-zero")]
    InvalidTimeBase,
    #[error("timestamp conversion overflowed")]
    Overflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_video_timestamp_to_nanoseconds() {
        let timestamp = MediaTimestamp::new(60, 1, 60).expect("valid time base");
        assert_eq!(
            timestamp.to_nanoseconds().expect("conversion"),
            1_000_000_000
        );
    }

    #[test]
    fn rejects_zero_time_base() {
        assert_eq!(
            MediaTimestamp::new(1, 1, 0),
            Err(MediaTimeError::InvalidTimeBase)
        );
    }
}
