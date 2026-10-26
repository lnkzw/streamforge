//! The egui front end: the production workspace of the design system
//! (`docs/ui/design-system.md`). It draws the [`Model`] and calls its
//! actions. The docks are drawn in [`crate::views`]; what they show is
//! computed in [`crate::workspace`].
//!
//! Top bar: transport and health. Left: scenes over the sources of the
//! selected scene. Centre: the program monitor. Right: the inspector of the
//! selected source (a drawer in narrow windows). Bottom: mixer and outputs.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use command_api::EncoderPreference;
use eframe::egui;

use crate::model::{InputForm, Model, SourceForm};
use crate::ui::theme::{self, color, size};
use crate::views;
use crate::workspace::{self, Ballistics, DRAWER_BELOW};

/// Status and meters are polled this often.
const LIVE_REFRESH: Duration = Duration::from_millis(100);
/// Health is polled this often while a pipeline runs: render and mix
/// counters refresh every 250 ms.
const HEALTH_REFRESH: Duration = Duration::from_millis(250);
/// A health reading older than this is marked as such.
pub(crate) const HEALTH_STALE: Duration = Duration::from_millis(1500);
/// Scenes are re-read this often (changes also arrive with each edit).
const SCENE_REFRESH: Duration = Duration::from_secs(2);

pub struct DesktopApp {
    pub(crate) model: Model,
    /// The scene being edited, by id. The program scene is the model's.
    pub(crate) selected_scene: Option<String>,
    /// The source in the inspector, by id.
    pub(crate) selected_source: Option<String>,
    /// The name typed for a new scene while its field is open.
    pub(crate) new_scene: Option<String>,
    pub(crate) dialog: Option<Dialog>,
    pub(crate) record_file: String,
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
    /// Meters by channel or bus id.
    pub(crate) meters: HashMap<String, Ballistics>,
    pub(crate) mixer_view: MixerView,
    /// Where project files are, as the project dialogs show it.
    pub(crate) project_folder: Option<String>,
    /// The operator chose to quit despite unsaved changes.
    quit_confirmed: bool,
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
    /// Close the window over unsaved changes.
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
            record_file: "recording.mkv".to_owned(),
            stream_url: String::new(),
            stream_key: String::new(),
            restart_targets: HashMap::new(),
            drawer_open: false,
            log_seen: 0,
            browser_split: 0.4,
            meters: HashMap::new(),
            mixer_view: MixerView::Console,
            project_folder: None,
            quit_confirmed: false,
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
        let drag = ui.interact(handle, ui.id().with("browser split"), egui::Sense::drag());
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

    /// The window can close unless the project has unsaved changes; then
    /// the operator chooses (save, discard or stay).
    fn guard_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.viewport().close_requested())
            && self.model.has_changes()
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
        let (Some(scenes), Some(id)) = (&self.model.scenes, &self.selected_source) else {
            return;
        };
        let exists = self
            .selected_scene
            .as_deref()
            .and_then(|scene| views::scene(scenes, scene))
            .is_some_and(|scene| views::source(scene, id).is_some());
        if !exists {
            self.select_source(None);
        }
    }

    /// Select a source for the inspector; in a narrow window this opens
    /// the drawer.
    pub(crate) fn select_source(&mut self, id: Option<String>) {
        self.drawer_open = id.is_some();
        self.selected_source = id;
    }
}

/// Draw a dock (`id` keeps its widgets apart from its siblings') into
/// `rect` with the dock's padding, clipped to it: what does not fit is
/// cut, so a dock never pushes its neighbours.
fn dock_in(ui: &mut egui::Ui, id: &str, rect: egui::Rect, add: impl FnOnce(&mut egui::Ui)) {
    let mut dock = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id)
            .max_rect(rect.shrink(size::SPACE_8))
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    dock.set_clip_rect(rect.intersect(ui.clip_rect()));
    add(&mut dock);
}

/// The frame of a dock: panel surface, 8 px padding.
pub(crate) fn dock_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(color::BG_PANEL)
        .inner_margin(size::SPACE_8)
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

        egui::Panel::top("top_bar")
            .exact_size(48.0)
            .frame(dock_frame().inner_margin(egui::Margin::symmetric(12, 8)))
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("console")
            .resizable(true)
            .default_size(232.0)
            .size_range(168.0..=420.0)
            .frame(egui::Frame::new())
            .show(ui, |ui| {
                egui::Panel::right("outputs")
                    .resizable(true)
                    .default_size(400.0)
                    .size_range(300.0..=560.0)
                    .frame(dock_frame())
                    .show(ui, |ui| self.outputs_dock(ui));
                egui::CentralPanel::default()
                    .frame(dock_frame())
                    .show(ui, |ui| self.mixer_dock(ui));
            });
        egui::Panel::left("browser")
            .resizable(true)
            .default_size(260.0)
            .size_range(200.0..=420.0)
            .frame(egui::Frame::new().fill(color::BG_PANEL))
            .show(ui, |ui| self.browser(ui));
        if !narrow {
            egui::Panel::right("inspector")
                .resizable(true)
                .default_size(300.0)
                .size_range(260.0..=440.0)
                .frame(dock_frame())
                .show(ui, |ui| self.inspector_dock(ui));
        }
        let program = egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(color::BG_APP)
                    .inner_margin(size::SPACE_12),
            )
            .show(ui, |ui| self.program_dock(ui));
        if narrow && self.drawer_open && self.selected_source.is_some() {
            let area = program.response.rect;
            let below_header = egui::Rect::from_min_max(
                egui::pos2(area.left(), program.inner + size::SPACE_6),
                area.max,
            );
            self.inspector_drawer(&ctx, below_header, escape_free);
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
