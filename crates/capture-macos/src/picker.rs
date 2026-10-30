//! The system picker, `SCContentSharingPicker` (macOS 14+, ADR 0020).
//!
//! A screen source without a capture target lets the user pick a display
//! or a window in the system's dialog, with the Linux picker's rules
//! (ADR 0010): the wait for a human is bounded, and a cancel is
//! `cancelled`. The picker runs on the main thread, so it needs a running
//! main run loop: the desktop's (AppKit's), or the one
//! [`crate::run_with_main_run_loop`] gives the other programs. Without one
//! (a test harness, a library caller) it is not offered, nor on the main
//! thread itself, which would wait for its own queue.
//!
//! The bridge keeps one picker session per process, so one pick runs at a
//! time. A pick that times out only deactivates the picker: the bridge's
//! `deactivate()` would also reset its count of the sessions that keep the
//! process a regular app, which the next pick's cleanup of the old one
//! then takes from the new one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use capture_api::{CaptureError, CaptureErrorKind, VideoSourceInfo, VideoSourceKind};
use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use screencapturekit::content_sharing_picker::{
    SCContentSharingPicker, SCContentSharingPickerConfiguration, SCContentSharingPickerMode,
    SCPickedSource, SCPickerOutcome,
};
use screencapturekit::prelude::SCContentFilter;
use screencapturekit::shareable_content::SCShareableContentInfo;

use crate::run_loop;

/// Longest wait for the user to pick a source, as on Linux (ADR 0010).
pub(crate) const PICK_TIMEOUT: Duration = Duration::from_secs(120);

/// Set while a pick runs.
static PICKING: AtomicBool = AtomicBool::new(false);

/// Whether the picker can be shown and waited for: macOS 14 or newer, off
/// the main thread, and a main run loop that runs what is sent to the
/// main queue.
pub(crate) fn available() -> bool {
    SCContentSharingPicker::is_available()
        && MainThreadMarker::new().is_none()
        && run_loop::main_run_loop_runs()
}

/// The one pick running, until dropped.
struct Picking;

impl Picking {
    fn start() -> Option<Self> {
        PICKING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then_some(Self)
    }
}

impl Drop for Picking {
    fn drop(&mut self) {
        PICKING.store(false, Ordering::Release);
    }
}

/// The modes for the requested kinds; none means either.
fn modes(kinds: &[VideoSourceKind]) -> Vec<SCContentSharingPickerMode> {
    let wanted = |kind| kinds.is_empty() || kinds.contains(&kind);
    let mut modes = Vec::new();
    if wanted(VideoSourceKind::Monitor) {
        modes.push(SCContentSharingPickerMode::SingleDisplay);
    }
    if wanted(VideoSourceKind::Window) {
        modes.push(SCContentSharingPickerMode::SingleWindow);
    }
    modes
}

/// Shows the picker and waits up to `timeout` for the user's choice: the
/// filter to capture and the source it describes (sizes in pixels where
/// ScreenCaptureKit reports them).
pub(crate) fn pick(
    kinds: &[VideoSourceKind],
    timeout: Duration,
) -> Result<(SCContentFilter, VideoSourceInfo), CaptureError> {
    let _picking = Picking::start().ok_or_else(|| {
        CaptureError::unavailable(
            "another screen source is being picked in the system picker; try again once it is done",
        )
    })?;
    let modes = modes(kinds);
    let (sender, receiver) = mpsc::sync_channel(1);
    // Set at a timeout. Held while the picker is sent to the main queue or
    // the timeout's deactivation is, so the deactivation comes after the
    // picker on that queue, or the picker is not sent at all; nothing
    // waits while it is held.
    let given_up = Arc::new(Mutex::new(false));
    let late = Arc::clone(&given_up);
    // Shown from the main thread, which the bridge requires, whether or
    // not the main run loop has started yet: the main queue runs inside it.
    DispatchQueue::main().exec_async(move || {
        let late = late.lock().unwrap_or_else(PoisonError::into_inner);
        if *late {
            return;
        }
        let mut configuration = match SCContentSharingPickerConfiguration::new() {
            Ok(configuration) => configuration,
            Err(err) => {
                let _ = sender.try_send(SCPickerOutcome::Error(err.to_string()));
                return;
            }
        };
        configuration.set_allowed_picker_modes(&modes);
        configuration.set_allows_changing_selected_content(false);
        // A late answer, after the wait ended, finds the receiver gone
        // and is dropped. The bridge presents on a later turn of the main
        // queue.
        SCContentSharingPicker::show(&configuration, move |outcome| {
            let _ = sender.try_send(outcome);
        });
        drop(late);
    });
    let result = match receiver.recv_timeout(timeout) {
        Ok(SCPickerOutcome::Picked(result)) => result,
        Ok(SCPickerOutcome::Cancelled) => {
            return Err(CaptureError::new(
                CaptureErrorKind::Cancelled,
                "the selection dialog was cancelled",
            ));
        }
        Ok(SCPickerOutcome::Error(message)) => {
            return Err(CaptureError::unavailable(format!(
                "the system picker could not be shown: {message}"
            )));
        }
        Err(RecvTimeoutError::Timeout) => {
            // Closes the dialog; nobody waits for its answer any more.
            let mut given_up = given_up.lock().unwrap_or_else(PoisonError::into_inner);
            *given_up = true;
            DispatchQueue::main().exec_async(|| {
                let _ = SCContentSharingPicker::set_active(false);
            });
            drop(given_up);
            return Err(CaptureError::timeout(format!(
                "nobody chose a screen source in the system picker within {} s",
                timeout.as_secs()
            )));
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Err(CaptureError::backend(
                "the system picker ended without an answer",
            ));
        }
    };
    let kind = match result.source() {
        SCPickedSource::Display(_) => Some(VideoSourceKind::Monitor),
        SCPickedSource::Window(_) => Some(VideoSourceKind::Window),
        SCPickedSource::Application(_) | SCPickedSource::Unknown => None,
    };
    let filter = result.filter();
    // Pixels (e.g. twice the points on a Retina display), as for a named
    // source; else what the picker reports.
    let (width, height) = SCShareableContentInfo::for_filter(&filter)
        .map(|info| info.pixel_size())
        .filter(|&(width, height)| width > 0 && height > 0)
        .unwrap_or_else(|| result.pixel_size());
    let positive = |value: u32| (value > 0).then_some(value);
    let info = VideoSourceInfo {
        kind,
        width: positive(width),
        height: positive(height),
    };
    Ok((filter, info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kinds_choose_the_modes() {
        use SCContentSharingPickerMode::{SingleDisplay, SingleWindow};
        assert_eq!(modes(&[VideoSourceKind::Monitor]), [SingleDisplay]);
        assert_eq!(modes(&[VideoSourceKind::Window]), [SingleWindow]);
        assert_eq!(
            modes(&[VideoSourceKind::Window, VideoSourceKind::Monitor]),
            [SingleDisplay, SingleWindow]
        );
        assert_eq!(modes(&[]), [SingleDisplay, SingleWindow]);
    }
}
