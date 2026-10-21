//! The egui front end: draws the [`Model`] and calls its actions. Panels:
//! status and preview toggle (top), scenes (left), mixer (right), preview
//! and sources (center), outputs and activity log (bottom).

use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, RichText};

use command_api::{SceneList, SourceKind};

use crate::model::{Level, Model, SourceForm, SourceType};

/// Status and meters are polled this often.
const LIVE_REFRESH: Duration = Duration::from_millis(100);
/// Scenes are re-read this often (changes also arrive with each edit).
const SCENE_REFRESH: Duration = Duration::from_secs(2);
/// Meter fall-off per second, in dB.
const METER_DECAY_DB: f32 = 24.0;
const METER_FLOOR_DB: f32 = -60.0;

pub struct DesktopApp {
    model: Model,
    selected_scene: Option<String>,
    new_scene: String,
    source_form: SourceForm,
    record_file: String,
    stream_url: String,
    /// Typed into a password field; handed to the engine on start only.
    stream_key: String,
    confirm_delete: Option<String>,
    preview: Option<egui::TextureHandle>,
    rgba: Vec<u8>,
    meters: Vec<f32>,
    master_meter: f32,
    last_live: Instant,
    last_scenes: Instant,
    last_frame: Instant,
}

impl DesktopApp {
    pub fn new(model: Model) -> Self {
        let now = Instant::now();
        Self {
            model,
            selected_scene: None,
            new_scene: String::new(),
            source_form: SourceForm::default(),
            record_file: "recording.mkv".to_owned(),
            stream_url: String::new(),
            stream_key: String::new(),
            confirm_delete: None,
            preview: None,
            rgba: Vec::new(),
            meters: Vec::new(),
            master_meter: METER_FLOOR_DB,
            last_live: now,
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
        if now - self.last_scenes >= SCENE_REFRESH {
            self.model.refresh_all();
            self.last_scenes = now;
        }
        // Meters: jump up to new peaks, fall off slowly.
        let elapsed = (now - self.last_frame).as_secs_f32();
        self.last_frame = now;
        let decay = |value: &mut f32, peak: Option<f64>| {
            let peak = peak.map_or(METER_FLOOR_DB, |db| db as f32);
            *value = (*value - METER_DECAY_DB * elapsed)
                .max(peak)
                .max(METER_FLOOR_DB);
        };
        if let Some(mixer) = &self.model.mixer {
            self.meters.resize(mixer.strips.len(), METER_FLOOR_DB);
            for (meter, strip) in self.meters.iter_mut().zip(&mixer.strips) {
                decay(meter, strip.peak_dbfs);
            }
            decay(&mut self.master_meter, mixer.master_peak_dbfs);
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

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("StreamForge");
            ui.separator();
            let mut preview = self.model.preview_enabled;
            if ui.checkbox(&mut preview, "Preview").changed() {
                self.model.set_preview(preview);
            }
            ui.separator();
            if let Some(status) = &self.model.status {
                let recording = match (status.recording_active, status.recording_failed) {
                    (true, _) => RichText::new("● REC").color(Color32::RED),
                    (_, true) => RichText::new("REC failed").color(Color32::ORANGE),
                    _ => RichText::new("not recording").weak(),
                };
                ui.label(recording);
                let streaming = match status.streaming_state {
                    Some(state) => RichText::new(format!("stream: {state:?}")).color(
                        if status.streaming_active {
                            Color32::LIGHT_GREEN
                        } else {
                            Color32::ORANGE
                        },
                    ),
                    None => RichText::new("not streaming").weak(),
                };
                ui.label(streaming);
            }
            if self.model.busy() {
                ui.spinner();
            }
        });
    }

    fn scenes_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Scenes");
        let Some(scenes) = self.model.scenes.clone() else {
            ui.spinner();
            return;
        };
        let shown = selection(self.selected_scene.as_deref(), None, &scenes);
        let mut clicked = None;
        for scene in &scenes.scenes {
            let label = if scene.name == scenes.active {
                format!("▶ {}", scene.name)
            } else {
                scene.name.clone()
            };
            if ui.selectable_label(scene.name == shown, label).clicked() {
                clicked = Some(scene.name.clone());
            }
        }
        let selected = selection(self.selected_scene.as_deref(), clicked.as_deref(), &scenes);
        self.selected_scene = Some(selected.clone());
        ui.horizontal(|ui| {
            if ui
                .add_enabled(selected != scenes.active, egui::Button::new("Activate"))
                .clicked()
            {
                self.model.activate_scene(&selected);
            }
            if ui
                .add_enabled(selected != scenes.active, egui::Button::new("Delete…"))
                .clicked()
            {
                self.confirm_delete = Some(selected.clone());
            }
        });
        if let Some(name) = self.confirm_delete.clone() {
            ui.label(RichText::new(format!("Delete scene `{name}`?")).color(Color32::ORANGE));
            ui.horizontal(|ui| {
                if ui.button("Yes, delete").clicked() {
                    self.model.delete_scene_confirmed(&name);
                    self.confirm_delete = None;
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_delete = None;
                }
            });
        }
        ui.separator();
        ui.label("New scene");
        ui.text_edit_singleline(&mut self.new_scene);
        if ui
            .add_enabled(!self.new_scene.is_empty(), egui::Button::new("Create"))
            .clicked()
        {
            self.model.create_scene(&self.new_scene);
            self.new_scene.clear();
        }
    }

    fn mixer_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Mixer");
        let Some(mixer) = self.model.mixer.clone() else {
            ui.spinner();
            return;
        };
        if !mixer.running {
            ui.label(RichText::new("idle — levels show while previewing or live").weak());
        }
        for (strip, meter) in mixer.strips.iter().zip(&self.meters) {
            ui.group(|ui| {
                ui.label(&strip.name);
                meter_bar(ui, *meter);
                let mut gain = strip.gain_db;
                let mut muted = strip.muted;
                let gain_changed =
                    committed(&ui.add(egui::Slider::new(&mut gain, -60.0..=12.0).suffix(" dB")));
                let mute_changed = ui.checkbox(&mut muted, "Mute").changed();
                if gain_changed || mute_changed {
                    self.model.set_strip(strip.index, gain, muted);
                }
            });
        }
        ui.separator();
        ui.label("Master");
        meter_bar(ui, self.master_meter);
        let mut master = mixer.master_gain_db;
        if committed(&ui.add(egui::Slider::new(&mut master, -60.0..=12.0).suffix(" dB"))) {
            self.model.set_master_gain(master);
        }
    }

    fn center_panel(&mut self, ui: &mut egui::Ui) {
        // Preview, 16:9, as wide as the panel allows.
        let width = ui.available_width().min(960.0);
        let size = egui::vec2(width, width * 9.0 / 16.0);
        match (&self.preview, self.model.preview_enabled) {
            (Some(texture), true) => {
                ui.image((texture.id(), size));
            }
            _ => {
                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                ui.painter().rect_filled(rect, 4.0, Color32::from_gray(24));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Preview off",
                    egui::FontId::proportional(18.0),
                    Color32::GRAY,
                );
            }
        }
        ui.separator();
        let Some(scene_name) = self.selected_scene.clone() else {
            return;
        };
        let Some(scene) = self
            .model
            .scenes
            .as_ref()
            .and_then(|list| list.scenes.iter().find(|scene| scene.name == scene_name))
            .cloned()
        else {
            return;
        };
        ui.heading(format!("Sources of `{}`", scene.name));
        for source in scene.sources.iter().rev() {
            ui.horizontal(|ui| {
                let kind = match source.kind {
                    SourceKind::ColorBars => "color bars",
                    SourceKind::Solid { .. } => "solid",
                    SourceKind::Screen { .. } => "screen",
                };
                ui.label(format!("{} ({kind})", source.name));
                let mut opacity = source.opacity;
                if committed(&ui.add(egui::Slider::new(&mut opacity, 0.0..=1.0).text("opacity"))) {
                    self.model
                        .set_source_opacity(&scene.name, &source.name, opacity);
                }
                if ui.button("Remove").clicked() {
                    self.model.remove_source(&scene.name, &source.name);
                }
            });
        }
        ui.collapsing("Add source", |ui| {
            let form = &mut self.source_form;
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut form.name);
            });
            egui::ComboBox::from_label("Kind")
                .selected_text(match form.kind {
                    SourceType::ColorBars => "Color bars",
                    SourceType::Solid => "Solid color",
                    SourceType::Screen => "Screen capture",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut form.kind, SourceType::ColorBars, "Color bars");
                    ui.selectable_value(&mut form.kind, SourceType::Solid, "Solid color");
                    ui.selectable_value(&mut form.kind, SourceType::Screen, "Screen capture");
                });
            if form.kind == SourceType::Solid {
                ui.horizontal(|ui| {
                    ui.label("Color");
                    ui.color_edit_button_srgba_unmultiplied(&mut form.color);
                });
            }
            if form.kind == SourceType::Screen {
                screen_source_picker(ui, &mut self.model, form);
            }
            let mut whole = form.rect.is_none();
            if ui.checkbox(&mut whole, "Whole canvas").changed() {
                form.rect = (!whole).then_some([0.0, 0.0, 960.0, 540.0]);
            }
            if let Some(rect) = &mut form.rect {
                ui.horizontal(|ui| {
                    for (label, value) in ["x", "y", "w", "h"].iter().zip(rect.iter_mut()) {
                        ui.label(*label);
                        ui.add(egui::DragValue::new(value).range(-4096.0..=4096.0));
                    }
                });
            }
            ui.add(egui::Slider::new(&mut form.opacity, 0.0..=1.0).text("opacity"));
            if ui
                .add_enabled(!form.name.is_empty(), egui::Button::new("Add"))
                .clicked()
            {
                let form = form.clone();
                self.model.add_source(&scene.name, &form);
                self.source_form.name.clear();
            }
        });
    }

    fn outputs_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Record to");
            ui.add_enabled(
                self.model.recording.is_none(),
                egui::TextEdit::singleline(&mut self.record_file).desired_width(160.0),
            );
            if self.model.recording.is_none() {
                if ui.button("Start recording").clicked() {
                    self.model.start_recording(&self.record_file);
                }
            } else if ui.button("Stop recording").clicked() {
                self.model.stop_recording();
            }
            ui.separator();
            ui.label("RTMP URL");
            ui.add_enabled(
                self.model.streaming.is_none(),
                egui::TextEdit::singleline(&mut self.stream_url)
                    .hint_text("rtmp://host/app")
                    .desired_width(200.0),
            );
            ui.label("Key");
            ui.add_enabled(
                self.model.streaming.is_none(),
                egui::TextEdit::singleline(&mut self.stream_key)
                    .password(true)
                    .desired_width(120.0),
            );
            if self.model.streaming.is_none() {
                if ui
                    .add_enabled(!self.stream_url.is_empty(), egui::Button::new("Go live"))
                    .clicked()
                {
                    self.model
                        .start_streaming(&self.stream_url, &self.stream_key);
                }
            } else if ui.button("End stream").clicked() {
                self.model.stop_streaming();
            }
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(110.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for entry in &self.model.log {
                    let text = RichText::new(&entry.text);
                    ui.label(match entry.level {
                        Level::Info => text,
                        Level::Warning => text.color(Color32::ORANGE),
                        Level::Error => text.color(Color32::RED),
                    });
                }
            });
    }
}

/// The scene to show: a click wins, then the previous selection if it still
/// exists, then the active scene.
fn selection(previous: Option<&str>, clicked: Option<&str>, scenes: &SceneList) -> String {
    let exists = |name: &&str| scenes.scenes.iter().any(|scene| scene.name == *name);
    clicked
        .filter(exists)
        .or(previous.filter(exists))
        .unwrap_or(&scenes.active)
        .to_owned()
}

/// A slider edit is sent when a drag ends, or right away for keyboard and
/// typed input (which change the value without dragging). Mid-drag values
/// are not sent, so a drag is one command, not one per frame.
fn committed(response: &egui::Response) -> bool {
    response.drag_stopped() || (response.changed() && !response.dragged())
}

fn meter_bar(ui: &mut egui::Ui, db: f32) {
    let fraction = ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0);
    let color = if db > -3.0 {
        Color32::RED
    } else if db > -12.0 {
        Color32::YELLOW
    } else {
        Color32::GREEN
    };
    ui.add(
        egui::ProgressBar::new(fraction)
            .fill(color)
            .text(format!("{db:.0} dB")),
    );
}

impl eframe::App for DesktopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        let ctx = ui.ctx().clone();
        self.update_preview(&ctx);
        egui::Panel::top("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::bottom("outputs").show(ui, |ui| self.outputs_panel(ui));
        egui::Panel::left("scenes")
            .default_size(200.0)
            .show(ui, |ui| self.scenes_panel(ui));
        egui::Panel::right("mixer")
            .default_size(220.0)
            .show(ui, |ui| self.mixer_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.center_panel(ui));
        });
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

/// Choose a listed screen source, or leave it to the system picker.
fn screen_source_picker(ui: &mut egui::Ui, model: &mut Model, form: &mut SourceForm) {
    if ui.button("Find screen sources").clicked() {
        model.find_screen_sources();
    }
    // An id that is no longer listed (e.g. its window closed) is not sent.
    let listed = |id: &String| matches!(&model.screen_sources, Some(Ok(sources)) if sources.iter().any(|source| &source.id == id));
    if form.screen_source.as_ref().is_some_and(|id| !listed(id)) {
        form.screen_source = None;
    }
    const PICKER: &str = "System picker (if the platform has one)";
    match &model.screen_sources {
        Some(Ok(sources)) if !sources.is_empty() => {
            let selected = form
                .screen_source
                .as_ref()
                .and_then(|id| sources.iter().find(|source| &source.id == id))
                .map_or(PICKER, |source| source.name.as_str());
            egui::ComboBox::from_label("Source")
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut form.screen_source, None, PICKER);
                    for source in sources {
                        let label = format!("{} ({})", source.name, source.kind.as_str());
                        ui.selectable_value(
                            &mut form.screen_source,
                            Some(source.id.clone()),
                            label,
                        );
                    }
                });
        }
        Some(Ok(_)) => {
            ui.label(RichText::new("No screen sources found.").weak());
        }
        Some(Err(reason)) => {
            ui.label(RichText::new(reason.as_str()).weak());
        }
        None => {
            ui.label(
                RichText::new(
                    "Find screen sources to choose one; otherwise the system picker opens \
                     when the source starts, where the platform has one.",
                )
                .weak(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use command_api::SceneSpec;

    use super::*;

    fn scenes(names: &[&str], active: &str) -> SceneList {
        SceneList {
            canvas_width: 1920,
            canvas_height: 1080,
            active: active.to_owned(),
            scenes: names
                .iter()
                .map(|name| SceneSpec {
                    name: (*name).to_owned(),
                    sources: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_click_selects_and_the_selection_sticks() {
        let list = scenes(&["Test pattern", "Show"], "Test pattern");
        // Nothing chosen yet: the active scene.
        assert_eq!(selection(None, None, &list), "Test pattern");
        // A click selects, and the next frame keeps it.
        let clicked = selection(Some("Test pattern"), Some("Show"), &list);
        assert_eq!(clicked, "Show");
        assert_eq!(selection(Some(&clicked), None, &list), "Show");
        // A deleted selection falls back to the active scene.
        let fewer = scenes(&["Test pattern"], "Test pattern");
        assert_eq!(selection(Some("Show"), None, &fewer), "Test pattern");
    }
}
