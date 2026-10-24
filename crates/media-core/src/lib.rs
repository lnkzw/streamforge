//! Platform-independent media primitives.
//!
//! Keep this crate free of UI, capture, device, encoder, and OS-specific APIs.

pub mod audio;
pub mod metrics;
pub mod sample_ring;
pub mod video;

pub use audio::{AudioRing, AudioRingStats};
pub use metrics::{LatencyHistogram, LatencySummary, duration_millis};
pub use sample_ring::{SampleReader, SampleRingStats, SampleWriter, sample_ring};
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

    /// Convert to ticks of another time base, rounding to the nearest tick
    /// (halves round up), using checked integer arithmetic.
    pub fn rescale(self, time_base_num: u32, time_base_den: u32) -> Result<i64, MediaTimeError> {
        if time_base_num == 0 || time_base_den == 0 {
            return Err(MediaTimeError::InvalidTimeBase);
        }
        // ticks * num / den seconds = target * tb_num / tb_den seconds.
        let numerator = i128::from(self.ticks)
            .checked_mul(i128::from(self.time_base_num))
            .and_then(|v| v.checked_mul(i128::from(time_base_den)))
            .ok_or(MediaTimeError::Overflow)?;
        let denominator = i128::from(self.time_base_den) * i128::from(time_base_num);
        let quotient = numerator.div_euclid(denominator);
        let rounded = if numerator.rem_euclid(denominator) * 2 >= denominator {
            quotient + 1
        } else {
            quotient
        };
        i64::try_from(rounded).map_err(|_| MediaTimeError::Overflow)
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
    fn rescales_with_rounding_and_overflow_checks() {
        let ns = |ticks| MediaTimestamp::new(ticks, 1, 1_000_000_000).expect("valid");
        // 1 s in 1/60 and 1/48000.
        assert_eq!(ns(1_000_000_000).rescale(1, 60), Ok(60));
        assert_eq!(ns(1_000_000_000).rescale(1, 48_000), Ok(48_000));
        // 16.66 ms rounds to frame 1; 8.3 ms (just under half) to frame 0.
        assert_eq!(ns(16_660_000).rescale(1, 60), Ok(1));
        assert_eq!(ns(8_300_000).rescale(1, 60), Ok(0));
        // Exactly half a frame rounds up.
        assert_eq!(ns(10_000_000).rescale(1, 50), Ok(1));
        assert_eq!(ns(-10_000_000).rescale(1, 50), Ok(0));
        assert_eq!(ns(1).rescale(0, 1), Err(MediaTimeError::InvalidTimeBase));
        let huge = MediaTimestamp::new(i64::MAX, u32::MAX, 1).expect("valid");
        assert_eq!(huge.rescale(1, 1), Err(MediaTimeError::Overflow));
    }

    #[test]
    fn rejects_zero_time_base() {
        assert_eq!(
            MediaTimestamp::new(1, 1, 0),
            Err(MediaTimeError::InvalidTimeBase)
        );
    }
}
