//! The inspector: the selected source's position, size and opacity, a
//! screen source's capture with its restart, and an image source's file. A
//! dock in wide windows, a drawer over the workspace in narrow ones.

use command_api::{SourceKind, SourceSpec};
use eframe::egui;

use crate::app::{Confirm, DesktopApp, Dialog};
use crate::model::SourceHealth;
use crate::ui::icons::Icon;
use crate::ui::theme::{TextRole, color, size};
use crate::ui::widgets::{self, Button, ButtonKind, StatusLevel};
use crate::workspace::Shortcut;

use super::{commit, coordinate_range, kind_label, named, opacity_row, right_to_left, with_keys};

impl DesktopApp {
    pub(crate) fn inspector_dock(&mut self, ui: &mut egui::Ui) {
        widgets::section_header(ui, "Inspector", None);
        self.inspector(ui);
    }

    /// The inspector over the right edge of the program area, with a close
    /// button; Escape closes it too while nothing else takes it
    /// (`escape_free`).
    pub(crate) fn inspector_drawer(
        &mut self,
        ctx: &egui::Context,
        area: egui::Rect,
        escape_free: bool,
    ) {
        let width = 300.0_f32.min(area.width());
        let rect = egui::Rect::from_min_max(egui::pos2(area.right() - width, area.top()), area.max);
        if escape_free && ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.drawer_open = false;
            return;
        }
        egui::Area::new(egui::Id::new("inspector drawer"))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.min)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(color::BG_PANEL)
                    .stroke(egui::Stroke::new(size::STROKE, color::LINE_STRONG))
                    .shadow(ctx.global_style().visuals.popup_shadow)
                    .inner_margin(size::SPACE_8)
                    .show(ui, |ui| {
                        ui.set_min_size(rect.size() - egui::Vec2::splat(2.0 * size::SPACE_8));
                        ui.set_max_width(rect.width() - 2.0 * size::SPACE_8);
                        let actions = widgets::section_header(ui, "Inspector", None);
                        if right_to_left(ui, actions, |ui| {
                            widgets::icon_button(ui, Icon::Close, "Close the inspector (Esc)")
                                .clicked()
                        }) {
                            self.drawer_open = false;
                        }
                        self.inspector(ui);
                    });
            });
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        let Some(source) = self.selected_source_spec().cloned() else {
            widgets::empty_state(ui, Icon::Layers, "Select a source to edit it");
            return;
        };
        let (canvas_width, canvas_height) = self.canvas();

        let (icon, kind) = kind_label(&source.kind);
        ui.horizontal(|ui| {
            let (rect, _) =
                ui.allocate_exact_size(egui::Vec2::splat(size::ICON), egui::Sense::hover());
            icon.paint(ui.painter(), rect, color::TEXT_SECONDARY);
            ui.add(egui::Label::new(TextRole::Title.text(&source.name)).truncate());
        });
        ui.label(TextRole::Meta.text(format!("{kind} · {}", source.id)));
        ui.add_space(size::SPACE_8);

        widgets::section_header(ui, "Position and size", None);
        let mut rect = [
            source.rect.x,
            source.rect.y,
            source.rect.width,
            source.rect.height,
        ];
        let mut changed = false;
        egui::Grid::new(("transform", &source.id))
            .num_columns(4)
            .spacing([size::SPACE_8, size::SPACE_6])
            .show(ui, |ui| {
                let rows = [[("X", "X"), ("Y", "Y")], [("W", "Width"), ("H", "Height")]];
                for (row, labels) in rows.iter().enumerate() {
                    for (column, (label, name)) in labels.iter().enumerate() {
                        let index = row * 2 + column;
                        ui.label(TextRole::Label.text(*label));
                        let field = ui.add(
                            egui::DragValue::new(&mut rect[index])
                                .range(coordinate_range(row == 1))
                                // A value set elsewhere stays as it is
                                // until the operator edits it.
                                .clamp_existing_to_range(false)
                                .speed(1.0)
                                .max_decimals(1)
                                .update_while_editing(false),
                        );
                        // Named for assistive technology; the label beside
                        // it says the same on screen.
                        named(ui, &field, name);
                        if let Some(value) = commit(ui, &field, rect[index]) {
                            rect[index] = value;
                            changed = true;
                        }
                    }
                    ui.end_row();
                }
            });
        let canvas = [0.0, 0.0, canvas_width as f32, canvas_height as f32];
        if ui
            .add(Button::new(ButtonKind::Ghost, "Fill the canvas").enabled(rect != canvas))
            .clicked()
        {
            rect = canvas;
            changed = true;
        }
        if changed {
            self.model.update_source(&source.id, Some(rect), None);
        }

        ui.add_space(size::SPACE_8);
        widgets::section_header(ui, "Opacity", None);
        let mut percent = source.opacity * 100.0;
        let [slider, field] = opacity_row(ui, &mut percent);
        if let Some(percent) = commit(ui, &slider, percent).or(commit(ui, &field, percent)) {
            self.model
                .update_source(&source.id, None, Some((percent / 100.0).clamp(0.0, 1.0)));
        }

        if let SourceKind::Screen {
            capture_target_id, ..
        } = &source.kind
        {
            ui.add_space(size::SPACE_8);
            widgets::section_header(ui, "Capture", None);
            self.capture(ui, &source, capture_target_id.is_none());
        }
        if let SourceKind::Image { file } = &source.kind {
            ui.add_space(size::SPACE_8);
            widgets::section_header(ui, "Image", None);
            self.image_file(ui, &source.id, file);
        }

        ui.add_space(size::SPACE_16);
        if ui
            .add(Button::new(ButtonKind::Danger, "Remove source…").icon(Icon::Trash))
            .on_hover_ui(|ui| {
                let text = "Remove this source from the scene, after asking";
                ui.label(with_keys(ui.ctx(), text, Shortcut::RemoveSource));
            })
            .clicked()
        {
            self.dialog = Some(Dialog::Confirm(Confirm::RemoveSource {
                id: source.id.clone(),
                name: source.name.clone(),
            }));
        }
    }

    /// A screen source's capture state, and a restart while it has a
    /// capture (running or ended). Restarting ends a working capture, so
    /// that asks first. An ended named source can restart on another
    /// listed source: a closed window's id does not come back.
    fn capture(&mut self, ui: &mut egui::Ui, source: &SourceSpec, picker: bool) {
        let id = source.id.as_str();
        let ended = match self.model.source_health(id) {
            SourceHealth::NotCapturing => {
                widgets::status_line(
                    ui,
                    StatusLevel::Info,
                    "Not capturing: captures run while the scene is rendered.",
                );
                return;
            }
            SourceHealth::Capturing {
                last_frame_age_ms,
                frames_consumed,
            } => {
                // Screens may deliver only on change: an age, not an error.
                let seconds = last_frame_age_ms.unwrap_or(0) / 1000;
                let text = match (frames_consumed, seconds) {
                    (0, 0) => "Capturing, no frame yet".to_owned(),
                    (0, seconds) => format!("Capturing, no frame yet ({seconds} s)"),
                    (_, 0) => "Capturing".to_owned(),
                    (_, seconds) => format!("Capturing, last frame {seconds} s ago"),
                };
                widgets::status_line(ui, StatusLevel::Ok, &text);
                false
            }
            SourceHealth::Ended { reason } => {
                widgets::status_line(ui, StatusLevel::Error, &format!("Ended: {reason}"));
                true
            }
        };
        let target = if ended && !picker {
            self.restart_target(ui, id)
        } else {
            None
        };
        // The engine refuses a picker restart while outputs run; disabling
        // says why before a click. While a command is pending (e.g. a picker
        // is open), a second restart would end the capture just chosen.
        let outputs_open = self.model.recording.is_some() || self.model.streaming.is_some();
        let busy = self.model.busy();
        let restart = ui
            .add(
                Button::new(ButtonKind::Secondary, "Restart capture")
                    .icon(Icon::Restart)
                    .enabled(!busy && !(picker && outputs_open)),
            )
            .on_hover_text("Start the capture again; outputs keep running")
            .on_disabled_hover_text(if busy {
                "Waiting for the engine"
            } else {
                "A picker source asks again: restart it while nothing records or streams"
            });
        if restart.clicked() {
            if ended {
                self.restart_targets.remove(id);
                self.model.restart_source(id, target.as_deref());
            } else {
                self.dialog = Some(Dialog::Confirm(Confirm::RestartSource {
                    id: id.to_owned(),
                    name: source.name.clone(),
                }));
            }
        }
    }

    /// An image source's file: another one from the project folder, or the
    /// same one read again after it changed on disk.
    fn image_file(&mut self, ui: &mut egui::Ui, id: &str, file: &str) {
        if self.model.images.is_none() {
            self.model.list_images();
        }
        let mut chosen = file.to_owned();
        match &self.model.images {
            Some(Ok(list)) => {
                // A list cut off at its limit may leave the file out.
                let missing =
                    list.omitted == 0 && !list.files.iter().any(|info| info.file_name == file);
                let width = ui.available_width();
                widgets::dropdown(ui, file, "Image file", width, |ui| {
                    for info in &list.files {
                        widgets::choice(ui, &mut chosen, info.file_name.clone(), &info.file_name);
                    }
                })
                .on_hover_text("Image file");
                if missing {
                    widgets::status_line(
                        ui,
                        StatusLevel::Warn,
                        "Not in the project folder: what was read before (if anything) stays \
                         until the file is back and read again.",
                    );
                }
            }
            Some(Err(reason)) => {
                ui.label(TextRole::Body.text(file));
                widgets::status_line(ui, StatusLevel::Error, reason);
            }
            None => {
                ui.label(TextRole::Body.text(file));
            }
        }
        if chosen != file {
            self.model.set_source_file(id, &chosen);
        }
        let busy = self.model.busy();
        ui.horizontal(|ui| {
            if ui
                .add(
                    Button::new(ButtonKind::Secondary, "Read again")
                        .icon(Icon::Restart)
                        .enabled(!busy),
                )
                .on_hover_text(
                    "Read the file again after it changed; every source showing it updates",
                )
                .on_disabled_hover_text("Waiting for the engine")
                .clicked()
            {
                self.model.restart_source(id, None);
                self.model.list_images();
            }
            if ui
                .add(
                    Button::new(ButtonKind::Ghost, "Refresh list")
                        .enabled(!self.model.listing_images()),
                )
                .on_hover_text("List the project folder's images again")
                .clicked()
            {
                self.model.list_images();
            }
        });
    }

    /// For an ended named source: restart on the same id, or on another
    /// listed source.
    fn restart_target(&mut self, ui: &mut egui::Ui, id: &str) -> Option<String> {
        const SAME: &str = "The same source";
        let selected = self.restart_targets.entry(id.to_owned()).or_default();
        match &self.model.screen_sources {
            Some(Ok(sources)) if !sources.is_empty() => {
                // An id that is no longer listed is not sent.
                if selected
                    .as_ref()
                    .is_some_and(|chosen| !sources.iter().any(|source| &source.id == chosen))
                {
                    *selected = None;
                }
                let text = selected
                    .as_ref()
                    .and_then(|chosen| sources.iter().find(|source| &source.id == chosen))
                    .map_or(SAME, |source| source.name.as_str());
                let width = ui.available_width();
                widgets::dropdown(ui, text, "Restart on", width, |ui| {
                    widgets::choice(ui, selected, None, SAME);
                    for source in sources {
                        let label = format!("{} ({})", source.name, source.kind.as_str());
                        widgets::choice(ui, selected, Some(source.id.clone()), &label);
                    }
                })
                .on_hover_text("The source the capture restarts on");
            }
            _ => {
                if ui
                    .add(Button::new(ButtonKind::Ghost, "Find other sources"))
                    .on_hover_text("Choose another source to restart on")
                    .clicked()
                {
                    self.model.find_screen_sources();
                }
            }
        }
        selected.clone()
    }
}
