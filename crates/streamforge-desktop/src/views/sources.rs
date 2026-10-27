//! The sources dock: the selected scene's sources, top layer first.
//! Selecting one opens it in the inspector, where its properties are
//! edited; the dock changes only the layers: their order (drag, keys,
//! menu) and whether each is drawn (the eye).

use command_api::{SourceKind, SourceSpec};
use eframe::egui;

use crate::app::{Confirm, DesktopApp, Dialog};
use crate::model::{SourceForm, SourceHealth};
use crate::ui::icons::Icon;
use crate::ui::theme::{color, size};
use crate::ui::widgets::{self, Button, ButtonKind, ListRow, MenuItem};
use crate::workspace::{self, Shortcut};

use super::{key_hint, kind_label, list_keys, right_to_left, scene};

impl DesktopApp {
    pub(crate) fn sources_dock(&mut self, ui: &mut egui::Ui) {
        let scene = self
            .model
            .scenes
            .as_ref()
            .zip(self.selected_scene.as_deref())
            .and_then(|(scenes, id)| scene(scenes, id))
            .cloned();
        let Some(scene) = scene else {
            widgets::section_header(ui, "Sources", None);
            return;
        };
        let actions = widgets::section_header(ui, "Sources", Some(scene.sources.len()));
        let add = right_to_left(ui, actions, |ui| {
            widgets::icon_button(ui, Icon::Plus, "Add a source to this scene").clicked()
        });
        if scene.sources.is_empty() {
            widgets::empty_state(
                ui,
                Icon::Layers,
                &format!("“{}” has no sources yet", scene.name),
            );
            ui.vertical_centered(|ui| {
                ui.add_space(size::SPACE_8);
                if ui
                    .add(Button::new(ButtonKind::Secondary, "Add source").icon(Icon::Plus))
                    .clicked()
                {
                    self.open_add_source(&scene.id, &scene.name);
                }
            });
        }
        if add {
            self.open_add_source(&scene.id, &scene.name);
        }

        let mut select = None;
        let mut remove = None;
        let mut show = None;
        let mut step = None;
        let mut moved = None;
        let len = scene.sources.len();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let eye = ui.spacing().interact_size.y;
                let mut rows = Vec::with_capacity(len);
                for (row_index, source) in scene.sources.iter().rev().enumerate() {
                    let (icon, kind) = kind_label(&source.kind);
                    let meta = match &source.kind {
                        SourceKind::Screen { .. } => match self.model.source_health(&source.id) {
                            SourceHealth::NotCapturing => "screen",
                            SourceHealth::Capturing { .. } => "screen · live",
                            SourceHealth::Ended { .. } => "screen · ended",
                        },
                        _ => kind,
                    };
                    let layer = workspace::layer_of_row(row_index, len);
                    // Rows keep their widgets by source, so the focus moves
                    // with a source when the order changes. An explicit id:
                    // `push_id` would still mix in the row's position.
                    let row_id = egui::Id::new(("source row", &source.id));
                    let row = ui
                        .scope_builder(egui::UiBuilder::new().id(row_id), |ui| {
                            let row = ui.add(
                                ListRow::new(icon, &source.name)
                                    .meta(meta)
                                    .selected(self.selected_source.as_deref() == Some(&source.id))
                                    .dimmed(!source.visible)
                                    .trailing(eye)
                                    .draggable(true),
                            );
                            let (eye_icon, eye_label) = if source.visible {
                                (Icon::Preview, format!("Hide {}", source.name))
                            } else {
                                (Icon::Hidden, format!("Show {}", source.name))
                            };
                            let eye_rect = egui::Rect::from_center_size(
                                egui::pos2(
                                    row.rect.right() - size::SPACE_4 - eye / 2.0,
                                    row.rect.center().y,
                                ),
                                egui::Vec2::splat(eye),
                            );
                            // A child, so the eye does not move the cursor.
                            let mut cell = ui.new_child(egui::UiBuilder::new().max_rect(eye_rect));
                            if widgets::icon_button(&mut cell, eye_icon, &eye_label).clicked() {
                                show = Some((source.id.clone(), !source.visible));
                            }
                            row
                        })
                        .inner;
                    if row.clicked() {
                        select = Some(source.id.clone());
                    }
                    row.context_menu(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let hint = |shortcut| key_hint(ui.ctx(), shortcut).unwrap_or_default();
                        let (up_keys, down_keys, remove_keys) = (
                            hint(Shortcut::RaiseSource),
                            hint(Shortcut::LowerSource),
                            hint(Shortcut::RemoveSource),
                        );
                        for (label, keys, raise) in [
                            ("Move up", &up_keys, true),
                            ("Move down", &down_keys, false),
                        ] {
                            let to = workspace::layer_step(layer, len, raise);
                            if ui
                                .add(MenuItem::new(label).shortcut(keys).enabled(to.is_some()))
                                .clicked()
                                && let Some(to) = to
                            {
                                step = Some((source.id.clone(), to));
                            }
                        }
                        if ui
                            .add(
                                MenuItem::new("Remove…")
                                    .icon(Icon::Trash)
                                    .danger(true)
                                    .shortcut(&remove_keys),
                            )
                            .clicked()
                        {
                            remove = Some((source.id.clone(), source.name.clone()));
                        }
                    });
                    rows.push(row);
                }
                if let Some(index) = list_keys(ui, &rows)
                    && let Some(source) = scene.sources.iter().rev().nth(index)
                {
                    select = Some(source.id.clone());
                }
                moved = drag_rows(ui, &rows, &scene.sources);
            });
        if let Some(id) = select {
            self.select_source(Some(id));
        }
        if let Some((id, name)) = remove {
            self.dialog = Some(Dialog::Confirm(Confirm::RemoveSource { id, name }));
        }
        if let Some((id, visible)) = show {
            self.model.set_source_visible(&id, visible);
        }
        if let Some((id, index)) = step.or(moved)
            && !self.model.pending("move_source")
        {
            self.model.move_source(&id, index);
        }
    }

    /// Move the selected source one layer up (`raise`) or down. One move
    /// at a time: the next counts from the engine's answer.
    pub(crate) fn step_source(&mut self, raise: bool) {
        if self.model.pending("move_source") {
            return;
        }
        let Some(source) = self.selected_source_spec() else {
            return;
        };
        let id = source.id.clone();
        let Some(sources) = self
            .model
            .scenes
            .as_ref()
            .zip(self.selected_scene.as_deref())
            .and_then(|(scenes, scene_id)| scene(scenes, scene_id))
            .map(|scene| &scene.sources)
        else {
            return;
        };
        let Some(index) = sources.iter().position(|source| source.id == id) else {
            return;
        };
        if let Some(to) = workspace::layer_step(index, sources.len(), raise) {
            self.model.move_source(&id, to);
        }
    }

    fn open_add_source(&mut self, scene_id: &str, scene_name: &str) {
        self.dialog = Some(Dialog::AddSource {
            scene_id: scene_id.to_owned(),
            scene_name: scene_name.to_owned(),
            form: SourceForm::default(),
        });
    }
}

/// A drag of one of the `rows` (top layer first, of `sources` in drawing
/// order): while it lasts, a line where the row would go; when it ends
/// elsewhere, the source and its new drawing-order index.
fn drag_rows(
    ui: &egui::Ui,
    rows: &[egui::Response],
    sources: &[SourceSpec],
) -> Option<(String, usize)> {
    let from = rows
        .iter()
        .position(|row| row.dragged() || row.drag_stopped())?;
    let pointer = ui.ctx().pointer_latest_pos()?;
    // The gap the pointer is nearest to: before the first row whose middle
    // is below it.
    let gap = rows
        .iter()
        .filter(|row| row.rect.center().y < pointer.y)
        .count();
    let to = workspace::drop_row(from, gap);
    if rows[from].drag_stopped() {
        let source = sources.iter().rev().nth(from)?;
        return (to != from).then(|| (source.id.clone(), workspace::layer_of_row(to, rows.len())));
    }
    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    if to != from {
        let y = match gap {
            0 => rows[0].rect.top(),
            gap => rows[gap - 1].rect.bottom(),
        };
        let row = rows[from].rect;
        ui.painter().hline(
            row.x_range(),
            y,
            egui::Stroke::new(size::STROKE_HEAVY, color::ACCENT),
        );
    }
    None
}
