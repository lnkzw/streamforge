//! What the production workspace shows, computed from the model's state
//! without widgets: the scene selection, the transport states, the monitor
//! rectangle, meter ballistics, the health summary, the project's name
//! and files, recording names, what a channel plays, where and whether the monitor plays,
//! and what the keys do. The views draw these; keeping them here keeps
//! them unit-tested.

use std::time::Duration;

use chrono::Timelike;
use command_api::{
    AudioInput, AudioOutput, CaptureState, ChannelInput, HealthReport, MonitorProfile,
    OutputHealth, RecordingState, SceneList, SourceSpec, StreamState,
};
use eframe::egui::{Key, KeyboardShortcut, Modifiers, Rect, vec2};

use crate::model::{HealthItem, Level};
use crate::ui::icons::Icon;
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
        .unwrap_or(&scenes.active_scene_id)
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
pub fn transport_state<S: OutputPhase>(
    open: bool,
    starting: bool,
    failed: bool,
    health: Option<(&OutputHealth<S>, Duration)>,
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
    if output.state.failed() {
        TransportState::Failed
    } else if output.state.waiting() {
        TransportState::Starting
    } else {
        let age_ms = u64::try_from(age.as_millis()).unwrap_or(u64::MAX);
        TransportState::Running(output.elapsed_ms.saturating_add(age_ms) / 1000)
    }
}

/// What an output's state means for its transport.
pub trait OutputPhase: Copy {
    /// It ended, or cannot deliver.
    fn failed(self) -> bool;
    /// Open, but not live yet: a stream connecting or reconnecting.
    fn waiting(self) -> bool;
}

impl OutputPhase for RecordingState {
    fn failed(self) -> bool {
        self == Self::Failed
    }

    fn waiting(self) -> bool {
        false
    }
}

impl OutputPhase for StreamState {
    fn failed(self) -> bool {
        matches!(self, Self::Failed | Self::Stopped)
    }

    fn waiting(self) -> bool {
        matches!(self, Self::Connecting | Self::Reconnecting)
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

/// Automatic recording file names from the local start time, e.g.
/// `recording-2026-10-27_15-30-05.mkv`: takes sort by time, and the name is
/// valid on every system (no `:`). Since a file is never overwritten, a
/// further take within the same second gets `_2`, `_3` (sorting after the
/// first). After the clock went back (a correction, the end of daylight
/// saving time), takes count on from the latest name given rather than
/// reuse one.
#[derive(Debug, Default)]
pub struct RecordingNames {
    /// The latest name's start second and take.
    latest: Option<(chrono::NaiveDateTime, u32)>,
}

impl RecordingNames {
    /// The name for a recording starting `now`.
    pub fn next(&mut self, now: chrono::NaiveDateTime) -> String {
        let start = now.with_nanosecond(0).unwrap_or(now);
        let (stamp, take) = match self.latest {
            Some((latest, take)) if start <= latest => (latest, take.saturating_add(1)),
            _ => (start, 1),
        };
        self.latest = Some((stamp, take));
        let stamp = stamp.format("%Y-%m-%d_%H-%M-%S");
        match take {
            1 => format!("recording-{stamp}.mkv"),
            take => format!("recording-{stamp}_{take}.mkv"),
        }
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

/// How the routing matrix shows what a channel plays.
#[derive(Debug, Clone, PartialEq)]
pub struct InputLabel {
    pub icon: Icon,
    /// Short, for the matrix cell.
    pub text: String,
    /// All of it, for the tooltip.
    pub detail: String,
    /// A device the engine does not list now: the channel is silent while
    /// it is missing.
    pub missing: bool,
}

/// An audio input's name for people: its description, else its name.
pub fn input_name(input: &AudioInput) -> &str {
    input
        .description
        .as_deref()
        .filter(|description| !description.trim().is_empty())
        .unwrap_or(&input.name)
}

/// What `input` is, given the engine's audio inputs (`None` while they
/// are not known, so nothing counts as missing). Desktop audio is an input
/// that captures what an output plays (`loopback_of`).
pub fn input_label(input: &ChannelInput, inputs: Option<&[AudioInput]>) -> InputLabel {
    let label = |icon, text: &str, detail: String| InputLabel {
        icon,
        text: text.to_owned(),
        detail,
        missing: false,
    };
    let device = match input {
        ChannelInput::Tone {
            frequency_hz,
            level_dbfs,
        } => {
            let hz = if frequency_hz.fract() == 0.0 {
                format!("{frequency_hz:.0}")
            } else {
                format!("{frequency_hz:.1}")
            };
            let level = if *level_dbfs < 0.0 {
                format!("−{:.1}", -level_dbfs)
            } else {
                "0.0".to_owned()
            };
            return label(
                Icon::Tone,
                &format!("Tone {hz} Hz"),
                format!("A test tone, {hz} Hz at {level} dBFS"),
            );
        }
        ChannelInput::Device { device: None } => {
            return label(
                Icon::Mic,
                "Default input",
                "The system's default input".to_owned(),
            );
        }
        ChannelInput::Device {
            device: Some(device),
        } => device,
    };
    let Some(inputs) = inputs else {
        return label(Icon::Mic, device, device.clone());
    };
    let Some(input) = inputs.iter().find(|input| input.name == *device) else {
        return InputLabel {
            icon: Icon::Warning,
            text: device.clone(),
            detail: format!(
                "{device} is not among the audio inputs now; the channel is silent while it \
                 is missing"
            ),
            missing: true,
        };
    };
    let name = input_name(input);
    let named = if name == device {
        device.clone()
    } else {
        format!("{name} ({device})")
    };
    match &input.loopback_of {
        Some(output) => label(
            Icon::Speaker,
            name,
            format!("Desktop audio: what {output} plays; {named}"),
        ),
        None => label(Icon::Mic, name, named),
    }
}

/// The monitor's device, as the outputs dock and its dialog show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLabel {
    /// Short, for the dock.
    pub text: String,
    /// All of it, for the tooltip.
    pub detail: String,
    /// A device the engine does not list now: the monitor stays silent
    /// while it is missing.
    pub missing: bool,
}

/// An audio output's name for people: its description, else its name.
pub fn output_name(output: &AudioOutput) -> &str {
    output
        .description
        .as_deref()
        .filter(|description| !description.trim().is_empty())
        .unwrap_or(&output.name)
}

/// The monitor's `device` (`None`: the system default), given the
/// engine's audio outputs (`None` while they are not known, so nothing
/// counts as missing).
pub fn output_label(device: Option<&str>, outputs: Option<&[AudioOutput]>) -> OutputLabel {
    let label = |text: &str, detail: String| OutputLabel {
        text: text.to_owned(),
        detail,
        missing: false,
    };
    let Some(device) = device else {
        return label("Default output", "The system's default output".to_owned());
    };
    let Some(outputs) = outputs else {
        return label(device, device.to_owned());
    };
    let Some(output) = outputs.iter().find(|output| output.name == device) else {
        return OutputLabel {
            text: device.to_owned(),
            detail: format!(
                "{device} is not among the audio outputs now; the monitor is silent while it is \
                 missing"
            ),
            missing: true,
        };
    };
    let name = output_name(output);
    if name == device {
        label(device, device.to_owned())
    } else {
        label(name, format!("{name} ({device})"))
    }
}

/// Whether the monitor plays, for its row in the outputs dock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorState {
    Off,
    /// Switched on; it plays while the preview or an output runs.
    On,
    Playing,
    /// Switched on, but its device ended or did not open; the reason says
    /// why. Switching it on again opens the device anew.
    Ended(String),
}

/// The monitor's state from its profile (the switch, shown at once) and
/// the last health reading (`None` without a pipeline). A reading of a
/// stopped pipeline, or from before the monitor was switched on (again),
/// says nothing about now.
pub fn monitor_state(profile: &MonitorProfile, health: Option<&HealthReport>) -> MonitorState {
    if !profile.enabled {
        return MonitorState::Off;
    }
    let monitor = health
        .filter(|health| health.pipeline_running)
        .and_then(|health| health.monitor.as_ref())
        .filter(|monitor| monitor.enabled);
    match monitor.map(|monitor| &monitor.state) {
        Some(CaptureState::Ended { reason }) => MonitorState::Ended(reason.clone()),
        Some(CaptureState::Running) => MonitorState::Playing,
        None => MonitorState::On,
    }
}

/// A key of the workspace (`docs/ui/design-system.md`, Keyboard). The
/// workspace acts on one only while no dialog or menu is open and no text
/// field has focus; Space only while nothing has focus, since a focused
/// control takes Space as a click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    /// Space: start or stop the preview.
    Preview,
    /// Ctrl+R: start recording; stopping asks.
    Record,
    /// Ctrl+L: go live; stopping asks.
    Live,
    /// Ctrl+1 … Ctrl+9: put the scene at this index (from 0) on program.
    Program(usize),
    /// Delete: remove the selected source; asks first.
    RemoveSource,
    /// Ctrl+Up: move the selected source one layer up.
    RaiseSource,
    /// Ctrl+Down: move the selected source one layer down.
    LowerSource,
    /// Ctrl+O
    OpenProject,
    /// Ctrl+S
    SaveProject,
    /// Ctrl+Shift+S
    SaveProjectAs,
}

/// The scenes Ctrl and a digit put on program.
const PROGRAM_KEYS: [Key; 9] = [
    Key::Num1,
    Key::Num2,
    Key::Num3,
    Key::Num4,
    Key::Num5,
    Key::Num6,
    Key::Num7,
    Key::Num8,
    Key::Num9,
];

impl Shortcut {
    /// Every shortcut, for matching a key press against.
    pub const ALL: [Self; 18] = [
        Self::Preview,
        Self::Record,
        Self::Live,
        Self::RemoveSource,
        Self::RaiseSource,
        Self::LowerSource,
        Self::OpenProject,
        Self::SaveProject,
        Self::SaveProjectAs,
        Self::Program(0),
        Self::Program(1),
        Self::Program(2),
        Self::Program(3),
        Self::Program(4),
        Self::Program(5),
        Self::Program(6),
        Self::Program(7),
        Self::Program(8),
    ];

    /// What a key press asks for. Ctrl is Cmd on macOS; the modifiers must
    /// be exactly the shortcut's, so Ctrl+Shift+S is not also Ctrl+S.
    /// Backspace removes a source too: a Mac's delete key sends it.
    pub fn of(key: Key, modifiers: Modifiers) -> Option<Self> {
        if key == Key::Backspace && modifiers.is_none() {
            return Some(Self::RemoveSource);
        }
        Self::ALL.into_iter().find(|shortcut| {
            shortcut.keys().is_some_and(|keys| {
                keys.logical_key == key && modifiers.matches_exact(keys.modifiers)
            })
        })
    }

    /// The keys, for matching and for the hint (`Context::format_shortcut`
    /// names Ctrl as Cmd on macOS). `None` for a scene past the ninth.
    pub fn keys(self) -> Option<KeyboardShortcut> {
        let command = |key| Some(KeyboardShortcut::new(Modifiers::COMMAND, key));
        match self {
            Self::Preview => Some(KeyboardShortcut::new(Modifiers::NONE, Key::Space)),
            Self::Record => command(Key::R),
            Self::Live => command(Key::L),
            Self::Program(index) => PROGRAM_KEYS.get(index).copied().and_then(command),
            Self::RemoveSource => Some(KeyboardShortcut::new(Modifiers::NONE, Key::Delete)),
            Self::RaiseSource => command(Key::ArrowUp),
            Self::LowerSource => command(Key::ArrowDown),
            Self::OpenProject => command(Key::O),
            Self::SaveProject => command(Key::S),
            Self::SaveProjectAs => Some(KeyboardShortcut::new(
                Modifiers::COMMAND | Modifiers::SHIFT,
                Key::S,
            )),
        }
    }
}

/// Where a source at `index` in a drawing order of `len` layers (0 is
/// the bottom) moves one layer up (`raise`) or down; `None` at that end.
pub fn layer_step(index: usize, len: usize, raise: bool) -> Option<usize> {
    if raise {
        (index + 1 < len).then_some(index + 1)
    } else {
        index.checked_sub(1)
    }
}

/// The drawing-order index (0 is the bottom) of row `row` in a list of
/// `len` layers that shows the top layer first; also the other way round.
pub fn layer_of_row(row: usize, len: usize) -> usize {
    len.saturating_sub(row + 1)
}

/// The topmost visible source of `sources` (in drawing order) at canvas
/// point (`x`, `y`): what a click on the picture selects.
pub fn source_at(sources: &[SourceSpec], x: f32, y: f32) -> Option<&SourceSpec> {
    sources.iter().rev().find(|source| {
        let rect = &source.rect;
        source.visible
            && x >= rect.x
            && y >= rect.y
            && x < rect.x + rect.width
            && y < rect.y + rect.height
    })
}

/// Where a row dragged from row `from` lands when dropped in gap `gap`
/// (0 is above the first row, the row count below the last).
pub fn drop_row(from: usize, gap: usize) -> usize {
    if gap > from { gap - 1 } else { gap }
}

/// Where Up and Down (`steps`: down minus up presses) move in a list of
/// `len` rows from row `index`: they stop at its ends.
pub fn list_step(index: usize, len: usize, steps: isize) -> usize {
    index
        .saturating_add_signed(steps)
        .min(len.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use command_api::{LiveOutputStats, MonitorHealth, SceneSpec, VideoEncoderInfo};
    use eframe::egui::{self, pos2};

    use super::*;

    fn scenes(names: &[&str], active: &str) -> SceneList {
        let id = |name: &str| format!("scene_{}", name.to_lowercase().replace(' ', "_"));
        SceneList {
            canvas_width: 1920,
            canvas_height: 1080,
            active_scene_name: active.to_owned(),
            active_scene_id: id(active),
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

    fn output<S>(running: bool, state: S, elapsed_ms: u64) -> OutputHealth<S> {
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
            connection: None,
        }
    }

    #[test]
    fn the_transport_follows_commands_status_and_health() {
        let zero = Duration::ZERO;
        let no_reading = None::<(&OutputHealth<RecordingState>, Duration)>;
        assert_eq!(
            transport_state(false, false, false, no_reading),
            TransportState::Idle
        );
        assert_eq!(
            transport_state(false, true, false, no_reading),
            TransportState::Starting
        );
        // Open, before the first reading.
        assert_eq!(
            transport_state(true, false, false, no_reading),
            TransportState::Running(0)
        );
        // The timer runs on from the reading by its age.
        let recording = output(true, RecordingState::Running, 61_400);
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
        let stopped = output(false, RecordingState::Failed, 5_000);
        assert_eq!(
            transport_state(true, false, false, Some((&stopped, zero))),
            TransportState::Failed
        );
        // A stream waiting on the network pulses; connected runs.
        for state in [StreamState::Connecting, StreamState::Reconnecting] {
            let stream = output(true, state, 9_000);
            assert_eq!(
                transport_state(true, false, false, Some((&stream, zero))),
                TransportState::Starting
            );
        }
        let stream = output(true, StreamState::Connected, 9_000);
        assert_eq!(
            transport_state(true, false, false, Some((&stream, zero))),
            TransportState::Running(9)
        );
        let stream = output(true, StreamState::Failed, 9_000);
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
    fn recordings_are_named_by_their_start_time() {
        let at = |seconds, milli| {
            chrono::NaiveDate::from_ymd_opt(2026, 1, 7)
                .and_then(|day| day.and_hms_milli_opt(9, 5, seconds, milli))
                .expect("a valid time")
        };
        let mut names = RecordingNames::default();
        let mut next = |seconds, milli| names.next(at(seconds, milli));
        assert_eq!(next(3, 100), "recording-2026-01-07_09-05-03.mkv");
        assert_eq!(next(3, 900), "recording-2026-01-07_09-05-03_2.mkv");
        assert_eq!(next(3, 950), "recording-2026-01-07_09-05-03_3.mkv");
        assert_eq!(next(4, 0), "recording-2026-01-07_09-05-04.mkv");
        // The clock went back: no name comes twice.
        assert_eq!(next(3, 0), "recording-2026-01-07_09-05-04_2.mkv");
        assert_eq!(next(4, 500), "recording-2026-01-07_09-05-04_3.mkv");
        assert_eq!(next(9, 0), "recording-2026-01-07_09-05-09.mkv");
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

    #[test]
    fn a_channel_input_names_its_device_and_marks_desktop_audio_and_missing_ones() {
        let inputs = [
            AudioInput {
                name: "alsa_input.usb".to_owned(),
                description: Some("USB Mic".to_owned()),
                media_class: "Audio/Source".to_owned(),
                loopback_of: None,
            },
            AudioInput {
                name: "speakers.monitor".to_owned(),
                description: Some("Monitor of Speakers".to_owned()),
                media_class: "Audio/Source".to_owned(),
                loopback_of: Some("speakers".to_owned()),
            },
            AudioInput {
                name: "line".to_owned(),
                description: None,
                media_class: "Audio/Source".to_owned(),
                loopback_of: None,
            },
        ];
        let device = |name: Option<&str>| ChannelInput::Device {
            device: name.map(str::to_owned),
        };
        let label = |input: &ChannelInput| input_label(input, Some(&inputs));

        let usb = label(&device(Some("alsa_input.usb")));
        assert_eq!((usb.icon, usb.text.as_str()), (Icon::Mic, "USB Mic"));
        assert_eq!(usb.detail, "USB Mic (alsa_input.usb)");
        let line = label(&device(Some("line")));
        assert_eq!((line.text.as_str(), line.detail.as_str()), ("line", "line"));
        let desktop = label(&device(Some("speakers.monitor")));
        assert_eq!(
            (desktop.icon, desktop.text.as_str()),
            (Icon::Speaker, "Monitor of Speakers")
        );
        assert_eq!(
            desktop.detail,
            "Desktop audio: what speakers plays; Monitor of Speakers (speakers.monitor)"
        );
        assert_eq!(label(&device(None)).text, "Default input");

        let gone = label(&device(Some("headset")));
        assert!(gone.missing);
        assert_eq!((gone.icon, gone.text.as_str()), (Icon::Warning, "headset"));
        // Without a list, nothing counts as missing.
        let unknown = input_label(&device(Some("headset")), None);
        assert!(!unknown.missing);
        assert_eq!(unknown.icon, Icon::Mic);

        let tone = label(&ChannelInput::Tone {
            frequency_hz: 440.0,
            level_dbfs: -20.0,
        });
        assert_eq!((tone.icon, tone.text.as_str()), (Icon::Tone, "Tone 440 Hz"));
        assert_eq!(tone.detail, "A test tone, 440 Hz at −20.0 dBFS");
        let odd = label(&ChannelInput::Tone {
            frequency_hz: 261.63,
            level_dbfs: 0.0,
        });
        assert_eq!(odd.detail, "A test tone, 261.6 Hz at 0.0 dBFS");
    }

    #[test]
    fn the_monitor_device_reads_by_its_description_and_is_marked_when_missing() {
        let outputs = vec![
            AudioOutput {
                name: "alsa_output.usb".to_owned(),
                description: Some("USB Headset".to_owned()),
                media_class: "Audio/Sink".to_owned(),
            },
            AudioOutput {
                name: "line".to_owned(),
                description: Some(" ".to_owned()),
                media_class: "Audio/Sink".to_owned(),
            },
        ];
        let label = |device| output_label(device, Some(&outputs));
        let usb = label(Some("alsa_output.usb"));
        assert_eq!(
            (usb.text.as_str(), usb.detail.as_str(), usb.missing),
            ("USB Headset", "USB Headset (alsa_output.usb)", false)
        );
        assert_eq!(label(Some("line")).detail, "line");
        assert_eq!(label(None).text, "Default output");
        let gone = label(Some("headset"));
        assert!(gone.missing);
        assert_eq!(gone.text, "headset");
        assert!(!output_label(Some("headset"), None).missing);
    }

    #[test]
    fn the_monitor_state_follows_its_switch_and_the_run() {
        let profile = |enabled| MonitorProfile {
            mix: "bus_program".to_owned(),
            enabled,
            device: None,
        };
        let health = |state, enabled, pipeline_running| HealthReport {
            pipeline_running,
            render: None,
            mix: None,
            recording: None,
            streaming: None,
            monitor: Some(MonitorHealth {
                device: None,
                enabled,
                state,
                latency_ms: None,
                feed: None,
            }),
            captures: Vec::new(),
        };
        let running = health(CaptureState::Running, true, true);
        let ended = || CaptureState::Ended {
            reason: "could not start: no device".to_owned(),
        };
        let ended_on = health(ended(), true, true);
        assert_eq!(monitor_state(&profile(false), None), MonitorState::Off);
        assert_eq!(monitor_state(&profile(true), None), MonitorState::On);
        assert_eq!(
            monitor_state(&profile(true), Some(&running)),
            MonitorState::Playing
        );
        // The switch shows at once, before the next health reading.
        assert_eq!(
            monitor_state(&profile(false), Some(&running)),
            MonitorState::Off
        );
        assert_eq!(
            monitor_state(&profile(true), Some(&ended_on)),
            MonitorState::Ended("could not start: no device".to_owned())
        );
        // Off, an ended device does not matter until it is switched on.
        assert_eq!(
            monitor_state(&profile(false), Some(&ended_on)),
            MonitorState::Off
        );
        // Switched on again: the reading from while it was off is old.
        assert_eq!(
            monitor_state(&profile(true), Some(&health(ended(), false, true))),
            MonitorState::On
        );
        // A stopped pipeline plays nothing; it is reported on its own.
        assert_eq!(
            monitor_state(
                &profile(true),
                Some(&health(CaptureState::Running, true, false))
            ),
            MonitorState::On
        );
    }

    #[test]
    fn keys_map_to_shortcuts_with_exactly_their_modifiers() {
        let command = Modifiers::COMMAND;
        // On Linux and Windows, Ctrl sets both `ctrl` and `command`.
        let ctrl = Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::NONE
        };
        let mac_cmd = Modifiers {
            mac_cmd: true,
            command: true,
            ..Modifiers::NONE
        };
        assert_eq!(Shortcut::of(Key::R, ctrl), Some(Shortcut::Record));
        assert_eq!(Shortcut::of(Key::R, mac_cmd), Some(Shortcut::Record));
        assert_eq!(Shortcut::of(Key::L, command), Some(Shortcut::Live));
        assert_eq!(Shortcut::of(Key::O, command), Some(Shortcut::OpenProject));
        assert_eq!(Shortcut::of(Key::S, command), Some(Shortcut::SaveProject));
        assert_eq!(
            Shortcut::of(Key::S, command | Modifiers::SHIFT),
            Some(Shortcut::SaveProjectAs)
        );
        assert_eq!(Shortcut::of(Key::Num1, command), Some(Shortcut::Program(0)));
        assert_eq!(Shortcut::of(Key::Num9, command), Some(Shortcut::Program(8)));
        assert_eq!(
            Shortcut::of(Key::Space, Modifiers::NONE),
            Some(Shortcut::Preview)
        );
        assert_eq!(
            Shortcut::of(Key::Delete, Modifiers::NONE),
            Some(Shortcut::RemoveSource)
        );
        assert_eq!(
            Shortcut::of(Key::Backspace, Modifiers::NONE),
            Some(Shortcut::RemoveSource)
        );
        // Without Ctrl, with another modifier, or a key of its own: none.
        assert_eq!(Shortcut::of(Key::R, Modifiers::NONE), None);
        assert_eq!(Shortcut::of(Key::Num0, command), None);
        assert_eq!(Shortcut::of(Key::R, command | Modifiers::ALT), None);
        assert_eq!(Shortcut::of(Key::Space, Modifiers::SHIFT), None);
        assert_eq!(Shortcut::of(Key::Delete, command), None);
        // Ctrl on a Mac is not Cmd.
        let mac_ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        assert_eq!(Shortcut::of(Key::S, mac_ctrl), None);
    }

    #[test]
    fn each_shortcut_has_the_keys_that_ask_for_it() {
        for shortcut in Shortcut::ALL {
            let keys = shortcut.keys().expect("keys");
            assert_eq!(
                Shortcut::of(keys.logical_key, keys.modifiers),
                Some(shortcut)
            );
        }
        // Scenes past the ninth have no key.
        assert_eq!(Shortcut::Program(9).keys(), None);
        let hint = |shortcut: Shortcut| {
            shortcut
                .keys()
                .expect("keys")
                .format(&egui::ModifierNames::NAMES, false)
        };
        assert_eq!(hint(Shortcut::SaveProjectAs), "Ctrl+Shift+S");
        assert_eq!(hint(Shortcut::Program(2)), "Ctrl+3");
        assert_eq!(hint(Shortcut::RemoveSource), "Delete");
        assert_eq!(hint(Shortcut::RaiseSource), "Ctrl+Up");
    }

    #[test]
    fn layers_step_within_the_drawing_order_and_rows_map_to_layers() {
        let command = Modifiers::COMMAND;
        assert_eq!(
            Shortcut::of(Key::ArrowUp, command),
            Some(Shortcut::RaiseSource)
        );
        assert_eq!(
            Shortcut::of(Key::ArrowDown, command),
            Some(Shortcut::LowerSource)
        );
        // Plain arrows move the selection in a list instead.
        assert_eq!(Shortcut::of(Key::ArrowUp, Modifiers::NONE), None);
        assert_eq!(layer_step(0, 3, true), Some(1));
        assert_eq!(layer_step(2, 3, true), None);
        assert_eq!(layer_step(2, 3, false), Some(1));
        assert_eq!(layer_step(0, 3, false), None);
        assert_eq!(layer_step(0, 1, true), None);
        // The top row is the last layer.
        assert_eq!(layer_of_row(0, 3), 2);
        assert_eq!(layer_of_row(2, 3), 0);
        assert_eq!(layer_of_row(layer_of_row(1, 4), 4), 1);
        // A click on the picture finds the top visible layer there.
        let layer = |id: &str, x: f32, visible: bool| SourceSpec {
            id: id.to_owned(),
            name: id.to_owned(),
            kind: command_api::SourceKind::ColorBars,
            rect: command_api::SourceRect {
                x,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            opacity: 1.0,
            visible,
        };
        let sources = [layer("a", 0.0, true), layer("b", 50.0, true)];
        let hit = |sources: &[SourceSpec], x| source_at(sources, x, 10.0).map(|s| s.id.clone());
        assert_eq!(hit(&sources, 60.0).as_deref(), Some("b"));
        assert_eq!(hit(&sources, 10.0).as_deref(), Some("a"));
        assert_eq!(hit(&sources, 150.0), None);
        let hidden = [layer("a", 0.0, true), layer("b", 50.0, false)];
        assert_eq!(hit(&hidden, 60.0).as_deref(), Some("a"));
        // Dropped above itself or just below: no move.
        assert_eq!(drop_row(1, 1), 1);
        assert_eq!(drop_row(1, 2), 1);
        assert_eq!(drop_row(1, 0), 0);
        assert_eq!(drop_row(0, 3), 2);
    }

    #[test]
    fn up_and_down_stop_at_the_ends_of_a_list() {
        assert_eq!(list_step(1, 3, 1), 2);
        assert_eq!(list_step(1, 3, -1), 0);
        assert_eq!(list_step(0, 3, -1), 0);
        assert_eq!(list_step(2, 3, 1), 2);
        assert_eq!(list_step(0, 3, 5), 2);
        assert_eq!(list_step(0, 1, 1), 0);
    }
}
