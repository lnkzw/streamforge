//! The sources dock: the selected scene's sources, top layer first.
//! Selecting one opens it in the inspector, the only place where its
//! properties are edited.

use command_api::SourceKind;
use eframe::egui;

use crate::app::{Confirm, DesktopApp, Dialog};
use crate::model::{SourceForm, SourceHealth};
use crate::ui::icons::Icon;
use crate::ui::theme::size;
use crate::ui::widgets::{self, Button, ButtonKind, ListRow, MenuItem};
use crate::workspace::Shortcut;

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
        let remove_keys = key_hint(ui.ctx(), Shortcut::RemoveSource).unwrap_or_default();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let mut rows = Vec::with_capacity(scene.sources.len());
                for source in scene.sources.iter().rev() {
                    let (icon, kind) = kind_label(&source.kind);
                    let meta = match &source.kind {
                        SourceKind::Screen { .. } => match self.model.source_health(&source.id) {
                            SourceHealth::NotCapturing => "screen",
                            SourceHealth::Capturing { .. } => "screen · live",
                            SourceHealth::Ended { .. } => "screen · ended",
                        },
                        _ => kind,
                    };
                    let row = ui.add(
                        ListRow::new(icon, &source.name)
                            .meta(meta)
                            .selected(self.selected_source.as_deref() == Some(&source.id)),
                    );
                    if row.clicked() {
                        select = Some(source.id.clone());
                    }
                    row.context_menu(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
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
            });
        if let Some(id) = select {
            self.select_source(Some(id));
        }
        if let Some((id, name)) = remove {
            self.dialog = Some(Dialog::Confirm(Confirm::RemoveSource { id, name }));
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
