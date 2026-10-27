//! The egui front end: the production workspace of the design system
//! (`docs/ui/design-system.md`). It draws the [`Model`] and calls its
//! actions. The docks are drawn in [`crate::views`]; what they show is
//! computed in [`crate::workspace`].
//!
//! Top bar: transport and health. Left: scenes over the sources of the
//! selected scene. Centre: the program monitor. Right: the inspector of the
//! selected source (a drawer in narrow windows). Bottom: mixer and outputs.

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use command_api::{EncoderPreference, SourceSpec};
use eframe::egui;

use crate::model::{InputForm, Model, SourceForm};
use crate::ui::theme::{self, color, size};
use crate::ui::widgets::{Transport, TransportState};
use crate::views;
use crate::workspace::{self, Ballistics, DRAWER_BELOW, Shortcut};

/// Status and meters are polled this often.
const LIVE_REFRESH: Duration = Duration::from_millis(100);
/// Health is polled this often while a pipeline runs: render and mix
/// counters refresh every 250 ms.
const HEALTH_REFRESH: Duration = Duration::from_millis(250);
/// A health reading older than this is marked as such.
pub(crate) const HEALTH_STALE: Duration = Duration::from_millis(1500);
/// Scenes are re-read this often (changes also arrive with each edit).
const SCENE_REFRESH: Duration = Duration::from_secs(2);

/// What the operator can resize the docks to, by dragging their edges.
const CONSOLE_HEIGHT: RangeInclusive<f32> = 168.0..=420.0;
const OUTPUTS_WIDTH: RangeInclusive<f32> = 300.0..=560.0;
const BROWSER_WIDTH: RangeInclusive<f32> = 200.0..=420.0;
const INSPECTOR_WIDTH: RangeInclusive<f32> = 260.0..=440.0;

pub struct DesktopApp {
    pub(crate) model: Model,
    /// The scene being edited, by id. The program scene is the model's.
    pub(crate) selected_scene: Option<String>,
    /// The source in the inspector, by id.
    pub(crate) selected_source: Option<String>,
    /// The name typed for a new scene while its field is open.
    pub(crate) new_scene: Option<String>,
    pub(crate) dialog: Option<Dialog>,
    /// The file name typed in the recording settings; empty (the default):
    /// each take is named by its start time.
    pub(crate) record_file: String,
    /// Names the takes without a typed name.
    recording_names: workspace::RecordingNames,
    pub(crate) stream_url: String,
    /// Typed into a password field; handed to the engine on start only.
    pub(crate) stream_key: String,
    /// Chosen source to restart an ended named source on (`None`: same).
    pub(crate) restart_targets: HashMap<String, Option<String>>,
    /// In narrow windows the inspector is a drawer; whether it is open.
    pub(crate) drawer_open: bool,
    /// Log entries the operator has seen (by opening the health details).
    pub(crate) log_seen: u64,
    /// Where the scenes dock ends and the sources dock begins, as a
    /// fraction of the left column.
    browser_split: f32,
    /// The docks' sizes, as their edges were dragged.
    docks: DockSizes,
    /// Meters by channel or bus id.
    pub(crate) meters: HashMap<String, Ballistics>,
    pub(crate) mixer_view: MixerView,
    /// Where project files are, as the project dialogs show it.
    pub(crate) project_folder: Option<String>,
    /// The operator chose to quit despite unsaved changes or running
    /// outputs.
    quit_confirmed: bool,
    /// Set by a signal handler: quit without asking.
    quit_signal: Option<Arc<AtomicBool>>,
    /// Quit once the save that was asked for succeeds.
    pub(crate) quit_after_save: bool,
    /// The window title last set.
    title: String,
    pub(crate) preview: Option<egui::TextureHandle>,
    rgba: Vec<u8>,
    last_live: Instant,
    last_health: Instant,
    last_scenes: Instant,
    last_frame: Instant,
}

/// The sizes of the docks the operator resizes, in points.
#[derive(Debug, Clone, Copy)]
struct DockSizes {
    console: f32,
    outputs: f32,
    browser: f32,
    inspector: f32,
}

/// What the mixer dock shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MixerView {
    /// The strips: faders, meters and mutes, for operating live.
    Console,
    /// The routing matrix: sends, assignments, output mixes, and buses
    /// added or removed.
    Routing,
}

/// The one modal dialog that can be open.
pub(crate) enum Dialog {
    AddSource {
        scene_id: String,
        scene_name: String,
        form: SourceForm,
    },
    /// Edits a draft; Done applies it, cancelling drops it. The video
    /// settings are `None` until the output profiles are known.
    RecordSettings {
        file_name: String,
        encoder: Option<EncoderPreference>,
    },
    /// Edits a draft of the target and the video settings (as for the
    /// recording). `go_live`: opened from the LIVE button without a
    /// target; its action starts the stream.
    StreamSettings {
        url: String,
        key: String,
        go_live: bool,
        encoder: Option<EncoderPreference>,
        bitrate_kbps: Option<u32>,
    },
    /// A draft of the monitor's device (`None`: the system default).
    MonitorSettings {
        device: Option<String>,
    },
    /// A new channel's name and what it plays.
    AddChannel {
        name: String,
        input: InputForm,
    },
    /// A draft of what channel `id` plays.
    ChannelInput {
        id: String,
        name: String,
        input: InputForm,
    },
    /// `assigned`: the new bus feeds the program bus (a submix);
    /// otherwise it is a separate mix.
    AddBus {
        name: String,
        assigned: bool,
    },
    /// The project directory's files; the chosen one is checked before it
    /// can be opened.
    OpenProject {
        selected: Option<String>,
        /// A double-click: open the file once its check is back.
        open_when_checked: bool,
    },
    /// A file name for the project; an existing file is replaced only
    /// with the dialog's own warning. `quit`: opened to save before
    /// quitting.
    SaveProject {
        file_name: String,
        quit: bool,
    },
    Confirm(Confirm),
}

/// A consequential action that asks first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Confirm {
    DeleteScene {
        id: String,
        name: String,
    },
    RemoveSource {
        id: String,
        name: String,
    },
    /// Restart a capture that still works.
    RestartSource {
        id: String,
        name: String,
    },
    RemoveChannel {
        id: String,
        name: String,
    },
    RemoveBus {
        id: String,
        name: String,
    },
    /// Open a project over unsaved changes.
    OpenProject {
        file_name: String,
    },
    /// Close the window over unsaved changes or running outputs.
    Quit,
    StopRecording,
    StopStreaming,
}

impl DesktopApp {
    /// Installs the design system on `ctx`; call it from the eframe
    /// creation closure, before the first frame.
    pub fn new(ctx: &egui::Context, model: Model) -> Self {
        theme::apply(ctx);
        let now = Instant::now();
        Self {
            model,
            selected_scene: None,
            selected_source: None,
            new_scene: None,
            dialog: None,
            record_file: String::new(),
            recording_names: workspace::RecordingNames::default(),
            stream_url: String::new(),
            stream_key: String::new(),
            restart_targets: HashMap::new(),
            drawer_open: false,
            log_seen: 0,
            browser_split: 0.4,
            docks: DockSizes {
                console: 232.0,
                outputs: 400.0,
                browser: 260.0,
                inspector: 300.0,
            },
            meters: HashMap::new(),
            mixer_view: MixerView::Console,
            project_folder: None,
            quit_confirmed: false,
            quit_signal: None,
            quit_after_save: false,
            title: String::new(),
            preview: None,
            rgba: Vec::new(),
            last_live: now,
            last_health: now,
            last_scenes: now,
            last_frame: now,
        }
    }

    /// Name the project folder in the project dialogs.
    pub fn with_project_folder(mut self, folder: impl Into<String>) -> Self {
        self.project_folder = Some(folder.into());
        self
    }

    /// Once `signal` is set (by a SIGTERM handler, say), the window closes
    /// without asking: running outputs stop and their files are finalized
    /// as at any close, unsaved project changes are lost.
    pub fn with_quit_signal(mut self, signal: Arc<AtomicBool>) -> Self {
        self.quit_signal = Some(signal);
        self
    }

    fn poll(&mut self) {
        self.model.update();
        let now = Instant::now();
        if now - self.last_live >= LIVE_REFRESH {
            self.model.refresh_live();
            self.last_live = now;
        }
        if now - self.last_health >= HEALTH_REFRESH {
            self.model.refresh_health();
            self.last_health = now;
        }
        if now - self.last_scenes >= SCENE_REFRESH {
            self.model.refresh_all();
            self.last_scenes = now;
        }
        let dt = (now - self.last_frame).as_secs_f32();
        self.last_frame = now;
        if let Some(mixer) = &self.model.mixer {
            self.meters.retain(|id, _| {
                mixer.channels.iter().any(|channel| &channel.spec.id == id)
                    || mixer.buses.iter().any(|bus| &bus.id == id)
            });
            let readings = mixer
                .channels
                .iter()
                .map(|channel| (&channel.spec.id, channel.peak_dbfs))
                .chain(mixer.buses.iter().map(|bus| (&bus.id, bus.peak_dbfs)));
            for (id, peak) in readings {
                self.meters.entry(id.clone()).or_default().update(peak, dt);
            }
        }
    }

    fn update_preview(&mut self, ctx: &egui::Context) {
        let Some(frame) = self.model.preview_frames().take() else {
            return;
        };
        let (width, height) = (frame.width() as usize, frame.height() as usize);
        // BGRA (opaque canvas, so premultiplied equals straight) to RGBA.
        self.rgba.clear();
        for y in 0..frame.height() {
            for bgra in frame.row(y).as_chunks::<4>().0 {
                self.rgba
                    .extend_from_slice(&[bgra[2], bgra[1], bgra[0], bgra[3]]);
            }
        }
        let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &self.rgba);
        match &mut self.preview {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.preview =
                    Some(ctx.load_texture("preview", image, egui::TextureOptions::LINEAR));
            }
        }
    }

    /// The left column: scenes over the sources of the selected scene,
    /// split by a divider that can be dragged.
    fn browser(&mut self, ui: &mut egui::Ui) {
        const MIN_DOCK: f32 = 120.0;
        let column = ui.max_rect();
        let split = (column.top() + column.height() * self.browser_split)
            .clamp(column.top() + MIN_DOCK, column.bottom() - MIN_DOCK)
            .round();
        let scenes = egui::Rect::from_min_max(column.min, egui::pos2(column.right(), split));
        let sources = egui::Rect::from_min_max(egui::pos2(column.left(), split + 1.0), column.max);
        dock_in(ui, "scenes", scenes, |ui| self.scenes_dock(ui));
        dock_in(ui, "sources", sources, |ui| self.sources_dock(ui));
        // The docks are children; the column still takes its whole width.
        ui.expand_to_include_rect(column);
        // The divider, registered last so it wins over the docks' edges.
        let handle = egui::Rect::from_center_size(
            egui::pos2(column.center().x, split),
            egui::vec2(column.width(), 7.0),
        );
        // Not focusable: Tab would stop on it without showing a focus.
        let drag = ui.interact(handle, ui.id().with("browser split"), egui::Sense::DRAG);
        if drag.dragged()
            && let Some(pointer) = drag.interact_pointer_pos()
        {
            self.browser_split = ((pointer.y - column.top()) / column.height()).clamp(0.15, 0.85);
        }
        let active = drag.hovered() || drag.dragged();
        if active {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        ui.painter().hline(
            column.x_range(),
            split + 0.5,
            egui::Stroke::new(
                size::STROKE,
                if active {
                    color::LINE_STRONG
                } else {
                    color::LINE_SUBTLE
                },
            ),
        );
    }

    pub(crate) fn open_projects(&mut self) {
        self.model.list_projects();
        self.model.validation = None;
        self.dialog = Some(Dialog::OpenProject {
            selected: None,
            open_when_checked: false,
        });
    }

    /// Save as: a name for the file, the current one to start with.
    pub(crate) fn save_project_as(&mut self, quit: bool) {
        self.model.list_projects();
        self.dialog = Some(Dialog::SaveProject {
            file_name: self.model.project.clone().unwrap_or_default(),
            quit,
        });
    }

    /// Save to the current file; without one, ask for a name.
    pub(crate) fn save_project(&mut self, quit: bool) {
        match self.model.project.clone() {
            Some(file_name) => {
                self.model.save_project(&file_name, true);
                self.quit_after_save = quit;
            }
            None => self.save_project_as(quit),
        }
    }

    /// Close the window: the confirmation or a save allowed it.
    pub(crate) fn quit(&mut self, ctx: &egui::Context) {
        self.quit_confirmed = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// The window can close unless the project has unsaved changes or an
    /// output runs; then the operator chooses (save, discard, stop or stay).
    fn guard_close(&mut self, ctx: &egui::Context) {
        if !self.quit_confirmed
            && self
                .quit_signal
                .as_ref()
                .is_some_and(|signal| signal.load(Ordering::Relaxed))
        {
            self.quit(ctx);
        }
        if ctx.input(|input| input.viewport().close_requested())
            && (self.model.has_changes() || self.outputs_on_air() != (false, false))
            && !self.quit_confirmed
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            // The question replaces another dialog (its draft is lost), but
            // not itself or the save it led to.
            let quitting = matches!(
                self.dialog,
                Some(Dialog::Confirm(Confirm::Quit) | Dialog::SaveProject { quit: true, .. })
            );
            if !quitting {
                self.dialog = Some(Dialog::Confirm(Confirm::Quit));
            }
        }
        // A save for quitting: close once it went through; a failed one
        // (in the log) keeps the window.
        if self.quit_after_save && !self.model.pending("save_project") {
            self.quit_after_save = false;
            if !self.model.unsaved {
                self.quit(ctx);
            }
        }
        let title = workspace::window_title(self.model.project.as_deref(), self.model.unsaved);
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    /// The video settings come from the output's profile, once known.
    pub(crate) fn open_record_settings(&mut self) {
        self.dialog = Some(Dialog::RecordSettings {
            file_name: self.record_file.clone(),
            encoder: None,
        });
    }

    pub(crate) fn open_stream_settings(&mut self, go_live: bool) {
        self.dialog = Some(Dialog::StreamSettings {
            url: self.stream_url.clone(),
            key: self.stream_key.clone(),
            go_live,
            encoder: None,
            bitrate_kbps: None,
        });
    }

    /// The device list is asked for again: devices come and go.
    pub(crate) fn open_monitor_settings(&mut self) {
        self.model.find_audio_outputs();
        self.dialog = Some(Dialog::MonitorSettings {
            device: self
                .model
                .profiles
                .as_ref()
                .and_then(|profiles| profiles.monitor.device.clone()),
        });
    }

    /// The canvas size from the scene list; the engine's default until
    /// the list arrives.
    pub(crate) fn canvas(&self) -> (u32, u32) {
        self.model.scenes.as_ref().map_or((1920, 1080), |scenes| {
            (scenes.canvas_width, scenes.canvas_height)
        })
    }

    /// Drop a selected source that no longer exists (e.g. another client
    /// removed it), so the inspector and its drawer do not stay on it.
    fn prune_selection(&mut self) {
        if self.model.scenes.is_some()
            && self.selected_source.is_some()
            && self.selected_source_spec().is_none()
        {
            self.select_source(None);
        }
    }

    /// The selected source, if it is still in the selected scene.
    pub(crate) fn selected_source_spec(&self) -> Option<&SourceSpec> {
        let scenes = self.model.scenes.as_ref()?;
        let scene = views::scene(scenes, self.selected_scene.as_deref()?)?;
        views::source(scene, self.selected_source.as_deref()?)
    }

    /// Select a source for the inspector; in a narrow window this opens
    /// the drawer.
    pub(crate) fn select_source(&mut self, id: Option<String>) {
        self.drawer_open = id.is_some();
        self.selected_source = id;
    }

    /// Whether the engine has yet to answer a start or stop of `output`;
    /// meanwhile its button and key do nothing.
    pub(crate) fn transport_waiting(&self, output: Transport) -> bool {
        let (start, stop) = match output {
            Transport::Record => ("start_recording", "stop_recording"),
            Transport::Live => ("start_streaming", "stop_streaming"),
        };
        self.model.pending(start) || self.model.pending(stop)
    }

    /// REC, by the button or its key: start; stopping asks; a failed
    /// recording only needs finalizing.
    pub(crate) fn record_action(&mut self) {
        if self.transport_waiting(Transport::Record) {
            return;
        }
        match self.record_transport() {
            TransportState::Idle => {
                let file_name = self.next_record_file();
                self.model.start_recording(&file_name);
            }
            TransportState::Failed => self.model.stop_recording(),
            TransportState::Starting | TransportState::Running(_) => {
                self.dialog = Some(Dialog::Confirm(Confirm::StopRecording));
            }
        }
    }

    /// Whether closing the window would stop the recording and the stream:
    /// each is starting or running (a failed one only needs finalizing).
    pub(crate) fn outputs_on_air(&self) -> (bool, bool) {
        let on_air = |state| matches!(state, TransportState::Starting | TransportState::Running(_));
        (
            on_air(self.record_transport()),
            on_air(self.live_transport()),
        )
    }

    /// The typed file name, or one from the local start time.
    fn next_record_file(&mut self) -> String {
        let typed = self.record_file.trim();
        if !typed.is_empty() {
            return typed.to_owned();
        }
        self.recording_names
            .next(chrono::Local::now().naive_local())
    }

    /// LIVE, as [`Self::record_action`]; without a target it asks for one
    /// and goes live from there.
    pub(crate) fn live_action(&mut self) {
        if self.transport_waiting(Transport::Live) {
            return;
        }
        match self.live_transport() {
            TransportState::Idle if self.stream_url.trim().is_empty() => {
                self.open_stream_settings(true);
            }
            TransportState::Idle => self
                .model
                .start_streaming(self.stream_url.trim(), &self.stream_key),
            TransportState::Failed => self.model.stop_streaming(),
            TransportState::Starting | TransportState::Running(_) => {
                self.dialog = Some(Dialog::Confirm(Confirm::StopStreaming));
            }
        }
    }

    /// The workspace's keys, taken out of this frame's input so no widget
    /// acts on them too. None while a dialog or menu is open, a text field
    /// has focus or the window waits for a save to close; Space also not
    /// while any control has focus (it clicks that control).
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some()
            || self.quit_after_save
            || egui::Popup::is_any_open(ctx)
            || ctx.text_edit_focused()
        {
            return;
        }
        let focused = ctx.memory(|memory| memory.focused().is_some());
        let pressed = ctx.input_mut(|input| {
            let mut pressed = Vec::new();
            input.events.retain(|event| {
                let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } = event
                else {
                    return true;
                };
                match Shortcut::of(*key, *modifiers) {
                    Some(Shortcut::Preview) if focused => true,
                    Some(shortcut) => {
                        pressed.push(shortcut);
                        false
                    }
                    None => true,
                }
            });
            pressed
        });
        for shortcut in pressed {
            // An action may have opened a dialog; the keys after it wait.
            if self.dialog.is_none() {
                self.shortcut(shortcut);
            }
        }
    }

    fn shortcut(&mut self, shortcut: Shortcut) {
        match shortcut {
            // The switch shows the engine's answer: a second press before
            // it would send the same state again.
            Shortcut::Preview if self.model.pending("set_preview") => {}
            Shortcut::Preview => self.model.set_preview(!self.model.preview_enabled),
            Shortcut::Record => self.record_action(),
            Shortcut::Live => self.live_action(),
            Shortcut::Program(index) => {
                let scene = self.model.scenes.as_ref().and_then(|scenes| {
                    scenes
                        .scenes
                        .get(index)
                        .filter(|scene| scene.id != scenes.active_scene_id)
                        .map(|scene| (scene.id.clone(), scene.name.clone()))
                });
                if let Some((id, name)) = scene {
                    self.model.activate_scene(&id, &name);
                }
            }
            Shortcut::RemoveSource => {
                if let Some(source) = self.selected_source_spec() {
                    self.dialog = Some(Dialog::Confirm(Confirm::RemoveSource {
                        id: source.id.clone(),
                        name: source.name.clone(),
                    }));
                }
            }
            Shortcut::OpenProject => self.open_projects(),
            Shortcut::SaveProject => self.save_project(false),
            Shortcut::SaveProjectAs => self.save_project_as(false),
        }
    }
}

/// Draw a dock (`id` keeps its widgets apart from its siblings') into
/// `rect` with the dock's padding, clipped to it: what does not fit is
/// cut, so a dock never pushes its neighbours.
fn dock_in(ui: &mut egui::Ui, id: &str, rect: egui::Rect, add: impl FnOnce(&mut egui::Ui)) {
    child_in(ui, id, rect.shrink(size::SPACE_8), rect, add);
}

/// A child `Ui` laid out top-down in `rect` and clipped to `clip`.
fn child_in<R>(
    ui: &mut egui::Ui,
    id: &str,
    rect: egui::Rect,
    clip: egui::Rect,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id)
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(clip.intersect(ui.clip_rect()));
    add(&mut child)
}

/// The frame of a dock: panel surface, 8 px padding.
pub(crate) fn dock_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(color::BG_PANEL)
        .inner_margin(size::SPACE_8)
}

/// A panel's body in the layout pass: it takes the whole room, and its
/// contents are drawn later with [`fill`].
fn room(ui: &mut egui::Ui) -> egui::Rect {
    ui.expand_to_include_rect(ui.max_rect());
    ui.max_rect()
}

/// Draw a dock's contents into the room its panel left for them,
/// clipped to the panel, as the panel itself would have.
fn fill<R>(
    ui: &mut egui::Ui,
    id: &str,
    panel: &egui::InnerResponse<egui::Rect>,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    child_in(ui, id, panel.inner, panel.response.rect, add)
}

/// The edge of a dock that is dragged to resize it.
#[derive(Clone, Copy)]
enum Edge {
    /// A dock on the left: its right edge.
    Right,
    /// A dock on the right: its left edge.
    Left,
    /// A dock at the bottom: its top edge.
    Top,
}

/// Resize a dock (`dock`: its rect) by dragging `edge`: `size` follows
/// the pointer within `range`. egui's own panel handles are Tab stops
/// that show no focus; this one only senses drags, so Tab passes it. It
/// is added after the docks' contents, so it wins over them at the edge.
fn resize_edge(
    ui: &egui::Ui,
    id: &str,
    dock: egui::Rect,
    edge: Edge,
    size: &mut f32,
    range: RangeInclusive<f32>,
) {
    let grab = ui.style().interaction.resize_grab_radius_side;
    // egui draws the separator on the dock's outermost pixel.
    let (handle, line, cursor) = match edge {
        Edge::Right => (
            egui::Rect::from_x_y_ranges(dock.right() - grab..=dock.right() + grab, dock.y_range()),
            dock.right() - 0.5,
            egui::CursorIcon::ResizeHorizontal,
        ),
        Edge::Left => (
            egui::Rect::from_x_y_ranges(dock.left() - grab..=dock.left() + grab, dock.y_range()),
            dock.left() + 0.5,
            egui::CursorIcon::ResizeHorizontal,
        ),
        Edge::Top => (
            egui::Rect::from_x_y_ranges(dock.x_range(), dock.top() - grab..=dock.top() + grab),
            dock.top() + 0.5,
            egui::CursorIcon::ResizeVertical,
        ),
    };
    let drag = ui.interact(handle, egui::Id::new(("resize", id)), egui::Sense::DRAG);
    if drag.dragged()
        && let Some(pointer) = drag.interact_pointer_pos()
    {
        let wanted = match edge {
            Edge::Right => pointer.x - dock.left(),
            Edge::Left => dock.right() - pointer.x,
            Edge::Top => dock.bottom() - pointer.y,
        };
        *size = wanted.round().clamp(*range.start(), *range.end());
    }
    if drag.hovered() || drag.dragged() {
        ui.ctx().set_cursor_icon(cursor);
        let stroke = egui::Stroke::new(size::STROKE, color::LINE_STRONG);
        match edge {
            Edge::Right | Edge::Left => ui.painter().vline(line, dock.y_range(), stroke),
            Edge::Top => ui.painter().hline(dock.x_range(), line, stroke),
        };
    }
}

impl eframe::App for DesktopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        self.prune_selection();
        let ctx = ui.ctx().clone();
        self.guard_close(&ctx);
        self.update_preview(&ctx);
        let narrow = ui.max_rect().width() < DRAWER_BELOW;
        // Taken before anything is drawn: a field or popup that has focus
        // now gets this frame's Escape, and the drawer must not take it too.
        let escape_free = self.dialog.is_none()
            && ctx.memory(|memory| memory.focused().is_none())
            && !egui::Popup::is_any_open(&ctx);

        self.shortcuts(&ctx);

        // The panels are laid out first and their contents drawn after, in
        // reading order: egui moves Tab focus in the order widgets are
        // added, and panels must be laid out from the outside in.
        let top_bar = egui::Panel::top("top_bar")
            .exact_size(48.0)
            .frame(dock_frame().inner_margin(egui::Margin::symmetric(12, 8)))
            .show(ui, room);
        let console = egui::Panel::bottom("console")
            // egui's resize handles are Tab stops; see `resize_edge`.
            .resizable(false)
            .exact_size(self.docks.console)
            .frame(egui::Frame::new())
            .show(ui, |ui| {
                let outputs = egui::Panel::right("outputs")
                    .resizable(false)
                    .exact_size(self.docks.outputs)
                    .frame(dock_frame())
                    .show(ui, room);
                let mixer = egui::CentralPanel::default()
                    .frame(dock_frame())
                    .show(ui, room);
                (outputs, mixer)
            });
        let browser = egui::Panel::left("browser")
            .resizable(false)
            .exact_size(self.docks.browser)
            .frame(egui::Frame::new().fill(color::BG_PANEL))
            .show(ui, room);
        let inspector = (!narrow).then(|| {
            egui::Panel::right("inspector")
                .resizable(false)
                .exact_size(self.docks.inspector)
                .frame(dock_frame())
                .show(ui, room)
        });
        let program = egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(color::BG_APP)
                    .inner_margin(size::SPACE_12),
            )
            .show(ui, room);

        fill(ui, "top bar", &top_bar, |ui| self.top_bar(ui));
        fill(ui, "browser", &browser, |ui| self.browser(ui));
        let header = fill(ui, "program", &program, |ui| self.program_dock(ui));
        if let Some(inspector) = &inspector {
            fill(ui, "inspector", inspector, |ui| self.inspector_dock(ui));
        } else if self.drawer_open && self.selected_source.is_some() {
            let area = program.response.rect;
            let below_header =
                egui::Rect::from_min_max(egui::pos2(area.left(), header + size::SPACE_6), area.max);
            self.inspector_drawer(&ctx, below_header, escape_free);
        }
        let (outputs, mixer) = &console.inner;
        fill(ui, "mixer", mixer, |ui| self.mixer_dock(ui));
        fill(ui, "outputs", outputs, |ui| self.outputs_dock(ui));

        let docks = &mut self.docks;
        resize_edge(
            ui,
            "console",
            console.response.rect,
            Edge::Top,
            &mut docks.console,
            CONSOLE_HEIGHT,
        );
        resize_edge(
            ui,
            "outputs",
            outputs.response.rect,
            Edge::Left,
            &mut docks.outputs,
            OUTPUTS_WIDTH,
        );
        resize_edge(
            ui,
            "browser",
            browser.response.rect,
            Edge::Right,
            &mut docks.browser,
            BROWSER_WIDTH,
        );
        if let Some(inspector) = &inspector {
            resize_edge(
                ui,
                "inspector",
                inspector.response.rect,
                Edge::Left,
                &mut docks.inspector,
                INSPECTOR_WIDTH,
            );
        }
        self.dialogs(&ctx);

        // Smooth preview and meters while something is live; otherwise just
        // often enough to pick up status changes (responses also repaint).
        let live = self.model.preview_enabled
            || self.model.recording.is_some()
            || self.model.streaming.is_some();
        ctx.request_repaint_after(if live {
            Duration::from_millis(33)
        } else {
            Duration::from_millis(250)
        });
    }
}
