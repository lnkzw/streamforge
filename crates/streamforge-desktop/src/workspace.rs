//! What the production workspace shows, computed from the model's state
//! without widgets: the scene selection, the transport states, the monitor
//! rectangle, meter ballistics, the health summary and the project's name
//! and files. The views draw these; keeping them here keeps them
//! unit-tested.

use std::time::Duration;

use command_api::{OutputHealth, SceneList, StreamState};
use eframe::egui::{Rect, vec2};

use crate::model::{HealthItem, Level};
use crate::ui::widgets::{METER_FLOOR_DB, StatusLevel, TransportState};

/// Below this window width the inspector is a drawer over the workspace
/// instead of a dock, so the program monitor keeps its width.
pub const DRAWER_BELOW: f32 = 1360.0;

/// The scene being edited, by id: a click wins, then the previous
/// selection if it still exists, then the program scene.
pub fn selected_scene(previous: Option<&str>, clicked: Option<&str>, scenes: &SceneList) -> String {
    let exists = |id: &&str| scenes.scenes.iter().any(|scene| scene.id == *id);
    clicked
        .filter(exists)
        .or(previous.filter(exists))
        .unwrap_or(&scenes.active_id)
        .to_owned()
}

/// The largest rectangle of the given aspect ratio centred in `outer`, on
/// whole pixels so the picture is not resampled across a seam.
pub fn fit(outer: Rect, width: u32, height: u32) -> Rect {
    if width == 0 || height == 0 || outer.width() <= 0.0 || outer.height() <= 0.0 {
        return Rect::from_center_size(outer.center(), vec2(0.0, 0.0));
    }
    let aspect = width as f32 / height as f32;
    let size = if outer.width() / outer.height() > aspect {
        vec2(outer.height() * aspect, outer.height())
    } else {
        vec2(outer.width(), outer.width() / aspect)
    }
    .floor();
    let min = (outer.center() - size / 2.0).round();
    Rect::from_min_size(min, size)
}

/// What the transport shows for one output.
///
/// - `open`: its start was answered and its stop not yet.
/// - `starting`: a start command waits for the engine.
/// - `failed`: the status says it stopped unexpectedly.
/// - `health`: the last reading for this output and its age; the timer
///   runs on from it between readings.
pub fn transport_state(
    open: bool,
    starting: bool,
    failed: bool,
    health: Option<(&OutputHealth, Duration)>,
) -> TransportState {
    if !open {
        return if starting {
            TransportState::Starting
        } else {
            TransportState::Idle
        };
    }
    if failed {
        return TransportState::Failed;
    }
    let Some((output, age)) = health else {
        // Started, and no reading for it yet.
        return TransportState::Running(0);
    };
    if !output.running {
        return TransportState::Failed;
    }
    match output.state {
        Some(StreamState::Connecting | StreamState::Reconnecting) => TransportState::Starting,
        Some(StreamState::Failed | StreamState::Stopped) => TransportState::Failed,
        Some(StreamState::Connected) | None => {
            let age_ms = u64::try_from(age.as_millis()).unwrap_or(u64::MAX);
            TransportState::Running(output.elapsed_ms.saturating_add(age_ms) / 1000)
        }
    }
}

/// How fast a meter falls, in dB per second.
pub const METER_FALL_DB_PER_S: f32 = 24.0;
/// How long the peak hold line stays before it falls.
pub const PEAK_HOLD: Duration = Duration::from_millis(1500);
/// A peak at or above this is a clip (full scale, with room for rounding).
pub const CLIP_DBFS: f32 = -0.1;

/// Meter ballistics for one strip: the bar jumps up to a new peak and
/// falls slowly; the peak hold line stays for [`PEAK_HOLD`] and then falls
/// the same way; the clip latch stays until the operator resets it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ballistics {
    pub level: f32,
    pub peak: f32,
    peak_held: f32,
    pub clipped: bool,
}

impl Default for Ballistics {
    fn default() -> Self {
        Self {
            level: METER_FLOOR_DB,
            peak: METER_FLOOR_DB,
            peak_held: 0.0,
            clipped: false,
        }
    }
}

impl Ballistics {
    /// Take a reading (`None`: silence or not running) after `dt` seconds.
    pub fn update(&mut self, reading_db: Option<f64>, dt: f32) {
        let reading = reading_db.map_or(METER_FLOOR_DB, |db| db as f32);
        let fall = METER_FALL_DB_PER_S * dt;
        self.level = (self.level - fall).max(reading).max(METER_FLOOR_DB);
        if reading >= self.peak {
            self.peak = reading;
            self.peak_held = 0.0;
        } else {
            self.peak_held += dt;
            if self.peak_held > PEAK_HOLD.as_secs_f32() {
                self.peak = (self.peak - fall).max(self.level);
            }
        }
        if reading >= CLIP_DBFS {
            self.clipped = true;
        }
    }

    pub fn reset_clip(&mut self) {
        self.clipped = false;
    }
}

/// The one indicator for health in the top bar: errors win, then
/// warnings (also a stale reading), then ok while something runs; idle
/// otherwise. `unseen_errors` are failed commands the operator has not
/// looked at yet.
pub fn health_level(items: &[HealthItem], stale: bool, unseen_errors: usize) -> StatusLevel {
    let worst = items
        .iter()
        .map(|item| item.level)
        .max_by_key(|level| match level {
            Level::Info => 0,
            Level::Warning => 1,
            Level::Error => 2,
        });
    if unseen_errors > 0 || worst == Some(Level::Error) {
        StatusLevel::Error
    } else if stale || worst == Some(Level::Warning) {
        StatusLevel::Warn
    } else if worst.is_some() {
        StatusLevel::Ok
    } else {
        StatusLevel::Info
    }
}

/// The project's name as the top bar and the window title show it.
pub fn project_name(project: Option<&str>) -> &str {
    project.unwrap_or("Untitled")
}

/// The window title: the project, marked while it has unsaved changes.
pub fn window_title(project: Option<&str>, unsaved: bool) -> String {
    let mark = if unsaved { " •" } else { "" };
    format!("{}{mark} — StreamForge", project_name(project))
}

/// The file name a typed project name stands for: trimmed (trailing dots
/// too, which Windows drops), with `.json` added unless it ends so
/// already, whatever other dots it has ("Show 2026.10"). The engine's rule
/// then checks it.
pub fn project_file(typed: &str) -> String {
    let typed = typed.trim().trim_end_matches('.');
    if typed.is_empty() || typed.to_ascii_lowercase().ends_with(".json") {
        typed.to_owned()
    } else {
        format!("{typed}.json")
    }
}

/// Whether two file names name the same file in the project folder: Windows
/// and macOS (by default) do not tell case apart.
pub fn same_file_name(a: &str, b: &str) -> bool {
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

/// A file size for a list: bytes, then kB and MB (powers of 1000), each
/// unit up to where its rounding would read 1000.
pub fn file_size(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..999_950 => format!("{:.1} kB", bytes as f64 / 1e3),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

/// How long ago a file was written, coarsely: a list needs "which one is
/// recent", not a timestamp. A time in the future (another clock) reads as
/// just now.
pub fn written_ago(modified: Duration, now: Duration) -> String {
    let seconds = now.saturating_sub(modified).as_secs();
    match seconds {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{} min ago", seconds / 60),
        3_600..86_400 => format!("{} h ago", seconds / 3_600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use command_api::{LiveOutputStats, SceneSpec, VideoEncoderInfo};
    use eframe::egui::pos2;

    use super::*;

    fn scenes(names: &[&str], active: &str) -> SceneList {
        let id = |name: &str| format!("scene_{}", name.to_lowercase().replace(' ', "_"));
        SceneList {
            canvas_width: 1920,
            canvas_height: 1080,
            active: active.to_owned(),
            active_id: id(active),
            scenes: names
                .iter()
                .map(|name| SceneSpec {
                    id: id(name),
                    name: (*name).to_owned(),
                    sources: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_click_selects_and_the_selection_sticks() {
        let list = scenes(&["Test pattern", "Show"], "Test pattern");
        // Nothing chosen yet: the program scene.
        assert_eq!(selected_scene(None, None, &list), "scene_test_pattern");
        // A click selects, and the next frame keeps it.
        let clicked = selected_scene(Some("scene_test_pattern"), Some("scene_show"), &list);
        assert_eq!(clicked, "scene_show");
        assert_eq!(selected_scene(Some(&clicked), None, &list), "scene_show");
        // A deleted selection falls back to the program scene.
        let fewer = scenes(&["Test pattern"], "Test pattern");
        assert_eq!(
            selected_scene(Some("scene_show"), None, &fewer),
            "scene_test_pattern"
        );
    }

    #[test]
    fn the_monitor_fits_by_width_or_height_on_whole_pixels() {
        // Wide space: height-bound, centred horizontally.
        let wide = Rect::from_min_size(pos2(10.0, 20.0), vec2(1000.0, 300.0));
        let fitted = fit(wide, 1920, 1080);
        assert_eq!(fitted.height(), 300.0);
        assert_eq!(fitted.width(), 533.0);
        assert_eq!(fitted.top(), 20.0);
        assert!((fitted.center().x - wide.center().x).abs() <= 0.5);
        // Tall space: width-bound.
        let tall = Rect::from_min_size(pos2(0.0, 0.0), vec2(640.0, 900.0));
        let fitted = fit(tall, 1920, 1080);
        assert_eq!((fitted.width(), fitted.height()), (640.0, 360.0));
        assert_eq!(fitted.min, fitted.min.round());
        // Nothing to fit into, or no canvas: an empty rectangle, no NaN.
        let empty = fit(
            Rect::from_min_size(pos2(5.0, 5.0), vec2(0.0, 0.0)),
            1920,
            1080,
        );
        assert_eq!(empty.size(), vec2(0.0, 0.0));
        assert_eq!(fit(wide, 0, 1080).size(), vec2(0.0, 0.0));
    }

    fn output(running: bool, state: Option<StreamState>, elapsed_ms: u64) -> OutputHealth {
        OutputHealth {
            target: "out".to_owned(),
            elapsed_ms,
            running,
            state,
            video_encoder: VideoEncoderInfo {
                encoder: "libx264".to_owned(),
                hardware: false,
                skipped: Vec::new(),
            },
            counters: LiveOutputStats::default(),
        }
    }

    #[test]
    fn the_transport_follows_commands_status_and_health() {
        let zero = Duration::ZERO;
        assert_eq!(
            transport_state(false, false, false, None),
            TransportState::Idle
        );
        assert_eq!(
            transport_state(false, true, false, None),
            TransportState::Starting
        );
        // Open, before the first reading.
        assert_eq!(
            transport_state(true, false, false, None),
            TransportState::Running(0)
        );
        // The timer runs on from the reading by its age.
        let recording = output(true, None, 61_400);
        assert_eq!(
            transport_state(
                true,
                false,
                false,
                Some((&recording, Duration::from_millis(700)))
            ),
            TransportState::Running(62)
        );
        // Failed by status or by health.
        assert_eq!(
            transport_state(true, false, true, Some((&recording, zero))),
            TransportState::Failed
        );
        let stopped = output(false, None, 5_000);
        assert_eq!(
            transport_state(true, false, false, Some((&stopped, zero))),
            TransportState::Failed
        );
        // A stream waiting on the network pulses; connected runs.
        for state in [StreamState::Connecting, StreamState::Reconnecting] {
            let stream = output(true, Some(state), 9_000);
            assert_eq!(
                transport_state(true, false, false, Some((&stream, zero))),
                TransportState::Starting
            );
        }
        let stream = output(true, Some(StreamState::Connected), 9_000);
        assert_eq!(
            transport_state(true, false, false, Some((&stream, zero))),
            TransportState::Running(9)
        );
        let stream = output(true, Some(StreamState::Failed), 9_000);
        assert_eq!(
            transport_state(true, false, false, Some((&stream, zero))),
            TransportState::Failed
        );
    }

    #[test]
    fn meters_jump_up_fall_slowly_and_hold_the_peak() {
        let mut meter = Ballistics::default();
        meter.update(Some(-12.0), 0.016);
        assert_eq!((meter.level, meter.peak), (-12.0, -12.0));
        // Half a second of silence: the bar falls 12 dB, the peak holds.
        meter.update(None, 0.5);
        assert_eq!(meter.level, -24.0);
        assert_eq!(meter.peak, -12.0);
        // After the hold time the peak falls too, never below the bar.
        meter.update(None, 1.1);
        assert!(meter.peak < -12.0 && meter.peak >= meter.level);
        // And everything rests at the floor.
        for _ in 0..10 {
            meter.update(None, 1.0);
        }
        assert_eq!((meter.level, meter.peak), (METER_FLOOR_DB, METER_FLOOR_DB));
        assert!(!meter.clipped);
    }

    #[test]
    fn a_clip_latches_until_reset() {
        let mut meter = Ballistics::default();
        meter.update(Some(-0.5), 0.016);
        assert!(!meter.clipped);
        meter.update(Some(0.0), 0.016);
        assert!(meter.clipped);
        meter.update(None, 5.0);
        assert!(meter.clipped, "the latch outlives the signal");
        meter.reset_clip();
        assert!(!meter.clipped);
    }

    #[test]
    fn health_shows_the_worst_state() {
        let item = |level| HealthItem {
            level,
            text: String::new(),
        };
        assert_eq!(health_level(&[], false, 0), StatusLevel::Info);
        assert_eq!(
            health_level(&[item(Level::Info)], false, 0),
            StatusLevel::Ok
        );
        assert_eq!(
            health_level(&[item(Level::Info)], true, 0),
            StatusLevel::Warn
        );
        assert_eq!(
            health_level(&[item(Level::Info), item(Level::Warning)], false, 0),
            StatusLevel::Warn
        );
        assert_eq!(
            health_level(&[item(Level::Error), item(Level::Warning)], false, 0),
            StatusLevel::Error
        );
        // A failed command the operator has not seen is an error too.
        assert_eq!(health_level(&[], false, 1), StatusLevel::Error);
    }

    #[test]
    fn the_project_reads_as_its_file_and_marks_unsaved_changes() {
        assert_eq!(window_title(None, false), "Untitled — StreamForge");
        assert_eq!(
            window_title(Some("show.json"), true),
            "show.json • — StreamForge"
        );
        assert_eq!(project_file("  show "), "show.json");
        assert_eq!(project_file("show.JSON"), "show.JSON");
        // Other dots are part of the name.
        assert_eq!(project_file("Show 2026.10"), "Show 2026.10.json");
        assert_eq!(project_file("show."), "show.json");
        assert_eq!(project_file(" "), "");
        assert!(same_file_name("show.json", "show.json"));
        assert_eq!(
            same_file_name("Show.json", "show.json"),
            cfg!(any(target_os = "windows", target_os = "macos"))
        );
    }

    #[test]
    fn file_lists_say_size_and_age_coarsely() {
        assert_eq!(file_size(999), "999 B");
        assert_eq!(file_size(12_345), "12.3 kB");
        assert_eq!(file_size(999_949), "999.9 kB");
        assert_eq!(file_size(999_960), "1.0 MB");
        assert_eq!(file_size(1_048_576), "1.0 MB");
        let now = Duration::from_secs(1_000_000);
        let ago = |seconds| written_ago(now - Duration::from_secs(seconds), now);
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(125), "2 min ago");
        assert_eq!(ago(7_200), "2 h ago");
        assert_eq!(ago(3 * 86_400), "3 d ago");
        assert_eq!(written_ago(now + Duration::from_secs(60), now), "just now");
    }
}
