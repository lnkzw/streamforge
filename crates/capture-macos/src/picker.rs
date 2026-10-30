//! The system picker, `SCContentSharingPicker` (macOS 14+, ADR 0020).
//!
//! A screen source without a capture target lets the user pick a display
//! or a window in the system's dialog, with the Linux picker's rules
//! (ADR 0010): the wait for a human is bounded, and a cancel is
//! `cancelled`. The picker runs on the main thread, so it needs a running
//! main run loop: the desktop's (AppKit's), or the one
//! [`crate::run_with_main_run_loop`] gives the other programs. Without one
//! (a test harness, a library caller) it is not offered.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use capture_api::{CaptureError, CaptureErrorKind, VideoSourceInfo, VideoSourceKind};
use dispatch2::DispatchQueue;
use screencapturekit::content_sharing_picker::{
    SCContentSharingPicker, SCContentSharingPickerConfiguration, SCContentSharingPickerMode,
    SCPickedSource, SCPickerOutcome,
};
use screencapturekit::prelude::SCContentFilter;
use screencapturekit::shareable_content::SCShareableContentInfo;

use crate::run_loop;

/// Longest wait for the user to pick a source, as on Linux (ADR 0010).
pub(crate) const PICK_TIMEOUT: Duration = Duration::from_secs(120);

/// Whether the picker can be shown: macOS 14 or newer, and a main run
/// loop that runs what is sent to the main queue.
pub(crate) fn available() -> bool {
    SCContentSharingPicker::is_available() && run_loop::main_run_loop_runs()
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
    let modes = modes(kinds);
    let (sender, receiver) = mpsc::sync_channel(1);
    // Shown from the main thread, which the bridge requires, whether or
    // not the main run loop has started yet: the main queue runs inside it.
    DispatchQueue::main().exec_async(move || {
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
        // and is dropped.
        SCContentSharingPicker::show(&configuration, move |outcome| {
            let _ = sender.try_send(outcome);
        });
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
            SCContentSharingPicker::deactivate();
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
