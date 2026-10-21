//! Whether a cpal stream still delivers, for `AudioStream::state`. Kept
//! apart from the backend so its tests run on every OS.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use capture_api::CaptureState;

/// How long a stream that reported an error may stay without data before
/// it counts as ended: many device periods (about 10 ms each).
const SILENCE_AFTER_ERROR: Duration = Duration::from_millis(500);

/// Whether a stream still delivers.
///
/// An error's kind does not tell: WASAPI's capture loop ends on any error
/// but keeps capturing after `StreamInvalidated` (a new default device), and
/// other hosts pause on some errors. What follows the error does: a stream
/// that reported one and has delivered no data since, for
/// [`SILENCE_AFTER_ERROR`], has ended. A stream that stops without an error
/// is not detected here (the mixer counts its underruns).
pub(crate) struct Liveness {
    start: Instant,
    /// Nanoseconds since `start`, plus one, of the last callback with data
    /// (0: none yet).
    last_data: AtomicU64,
    /// Nanoseconds since `start`, plus one, of the last error (0: none).
    last_error_at: AtomicU64,
    /// That error's message; written off the data path.
    last_error: Mutex<String>,
}

impl Liveness {
    pub(crate) fn new() -> Self {
        Self {
            start: Instant::now(),
            last_data: AtomicU64::new(0),
            last_error_at: AtomicU64::new(0),
            last_error: Mutex::new(String::new()),
        }
    }

    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos())
            .unwrap_or(u64::MAX)
            .saturating_add(1)
    }

    /// From the data callback: no lock, no allocation.
    pub(crate) fn data(&self) {
        self.last_data.store(self.now(), Ordering::Relaxed);
    }

    /// From the error callback. Reuses the message buffer; allocates only
    /// for a longer message.
    pub(crate) fn error(&self, message: &str) {
        {
            let mut last = self
                .last_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            last.clear();
            last.push_str(message);
        }
        self.last_error_at.store(self.now(), Ordering::Release);
    }

    /// Never waits: the message is skipped if the error callback holds it.
    pub(crate) fn state(&self) -> CaptureState {
        let error_at = self.last_error_at.load(Ordering::Acquire);
        if !has_ended(self.now(), self.last_data.load(Ordering::Relaxed), error_at) {
            return CaptureState::Running;
        }
        let reason = match self.last_error.try_lock() {
            Ok(message) => format!("{message}; no audio since"),
            Err(_) => "the stream reported an error and delivers no audio".to_owned(),
        };
        CaptureState::Ended { reason }
    }
}

/// See [`Liveness`]; times are nanoseconds plus one, 0 meaning never.
fn has_ended(now: u64, last_data: u64, last_error_at: u64) -> bool {
    let silence = u64::try_from(SILENCE_AFTER_ERROR.as_nanos()).unwrap_or(u64::MAX);
    last_error_at != 0 && last_data <= last_error_at && now.saturating_sub(last_error_at) >= silence
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn a_stream_ends_when_no_data_follows_an_error() {
        // No error: running, with or without data.
        assert!(!has_ended(10_000 * MS, 0, 0));
        assert!(!has_ended(10_000 * MS, 20 * MS, 0));
        // Data after the error (an xrun, a reroute): running.
        assert!(!has_ended(10_000 * MS, 120 * MS, 100 * MS));
        // No data since the error: running at first, ended after the grace.
        assert!(!has_ended(400 * MS, 90 * MS, 100 * MS));
        assert!(has_ended(600 * MS, 90 * MS, 100 * MS));
        // Never delivered, then an error.
        assert!(has_ended(600 * MS, 0, 100 * MS));
    }

    #[test]
    fn the_state_names_the_last_error() {
        let liveness = Liveness::new();
        liveness.data();
        assert_eq!(liveness.state(), CaptureState::Running);
        liveness.error("first");
        liveness.error("the device was removed");
        // Within the grace period.
        assert_eq!(liveness.state(), CaptureState::Running);
        liveness.last_error_at.store(1, Ordering::Release);
        liveness.last_data.store(1, Ordering::Relaxed);
        std::thread::sleep(SILENCE_AFTER_ERROR);
        assert_eq!(
            liveness.state(),
            CaptureState::Ended {
                reason: "the device was removed; no audio since".to_owned()
            }
        );
        liveness.data();
        assert_eq!(liveness.state(), CaptureState::Running);
    }
}
