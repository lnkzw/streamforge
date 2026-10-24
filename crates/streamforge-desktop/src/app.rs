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

use eframe::egui;

use crate::model::{Model, SourceForm};
use crate::ui::theme::{self, color, size};
use crate::views;
use crate::workspace::{Ballistics, DRAWER_BELOW};

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
    /// Meters by channel id, and the program bus's.
    pub(crate) meters: HashMap<String, Ballistics>,
    pub(crate) program_meter: Ballistics,
    pub(crate) preview: Option<egui::TextureHandle>,
    rgba: Vec<u8>,
    last_live: Instant,
    last_health: Instant,
    last_scenes: Instant,
    last_frame: Instant,
}

/// The one modal dialog that can be open.
pub(crate) enum Dialog {
    AddSource {
        scene_id: String,
        scene_name: String,
        form: SourceForm,
    },
    /// Edits a draft; Done applies it, cancelling drops it.
    RecordSettings {
        file_name: String,
    },
    /// Edits a draft of the target. `go_live`: opened from the LIVE button
    /// without a target; its action starts the stream.
    StreamSettings {
        url: String,
        key: String,
        go_live: bool,
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
            program_meter: Ballistics::default(),
            preview: None,
            rgba: Vec::new(),
            last_live: now,
            last_health: now,
            last_scenes: now,
            last_frame: now,
        }
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
            self.meters
                .retain(|id, _| mixer.channels.iter().any(|channel| &channel.spec.id == id));
            for channel in &mixer.channels {
                self.meters
                    .entry(channel.spec.id.clone())
                    .or_default()
                    .update(channel.peak_dbfs, dt);
            }
            let program = mixer.buses.first().and_then(|program| program.peak_dbfs);
            self.program_meter.update(program, dt);
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

    pub(crate) fn open_record_settings(&mut self) {
        self.dialog = Some(Dialog::RecordSettings {
            file_name: self.record_file.clone(),
        });
    }

    pub(crate) fn open_stream_settings(&mut self, go_live: bool) {
        self.dialog = Some(Dialog::StreamSettings {
            url: self.stream_url.clone(),
            key: self.stream_key.clone(),
            go_live,
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
