//! The scenes dock. The program scene has the tally mark, the selected
//! scene (the one being edited) the accent bar: two marks, so editing a
//! scene that is not on air is visible. Putting a scene on program is its
//! own action.

use eframe::egui;

use crate::app::{Confirm, DesktopApp, Dialog};
use crate::ui::icons::Icon;
use crate::ui::theme::{TextRole, size};
use crate::ui::widgets::{self, Button, ButtonKind, ListRow};
use crate::workspace::selected_scene;

use super::right_to_left;

impl DesktopApp {
    pub(crate) fn scenes_dock(&mut self, ui: &mut egui::Ui) {
        let Some(scenes) = self.model.scenes.clone() else {
            widgets::section_header(ui, "Scenes", None);
            ui.spinner();
            return;
        };
        let actions = widgets::section_header(ui, "Scenes", Some(scenes.scenes.len()));
        right_to_left(ui, actions, |ui| {
            if widgets::icon_button(ui, Icon::Plus, "New scene").clicked() {
                self.new_scene.get_or_insert_with(String::new);
            }
        });

        let mut clicked = None;
        let mut take = None;
        let mut delete = None;
        // The list takes what the actions under it leave.
        let footer = size::CONTROL + size::SPACE_8 + 2.0 * ui.spacing().item_spacing.y;
        egui::ScrollArea::vertical()
            .max_height((ui.available_height() - footer).max(size::ROW))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let shown = selected_scene(self.selected_scene.as_deref(), None, &scenes);
                for scene in &scenes.scenes {
                    let meta = match scene.sources.len() {
                        1 => "1 source".to_owned(),
                        count => format!("{count} sources"),
                    };
                    let on_air = scene.id == scenes.active_id;
                    let row = ui.add(
                        ListRow::new(Icon::Scene, &scene.name)
                            .meta(&meta)
                            .selected(scene.id == shown)
                            .program(on_air),
                    );
                    if row.clicked() {
                        clicked = Some(scene.id.clone());
                    }
                    if row.double_clicked() && !on_air {
                        take = Some((scene.id.clone(), scene.name.clone()));
                    }
                    row.context_menu(|ui| {
                        if ui
                            .add_enabled(!on_air, egui::Button::new("Put on program"))
                            .clicked()
                        {
                            take = Some((scene.id.clone(), scene.name.clone()));
                        }
                        if ui
                            .add_enabled(!on_air, egui::Button::new("Delete…"))
                            .on_disabled_hover_text("The scene on program cannot be deleted")
                            .clicked()
                        {
                            delete = Some((scene.id.clone(), scene.name.clone()));
                        }
                    });
                }
                self.new_scene_field(ui);
            });

        let selected = selected_scene(self.selected_scene.as_deref(), clicked.as_deref(), &scenes);
        if clicked.is_some() && self.selected_scene.as_deref() != Some(&selected) {
            // The inspector belongs to the scene that was selected before.
            self.select_source(None);
        }
        self.selected_scene = Some(selected.clone());

        // Actions on the selected scene. They send the listed id with the
        // name: if the scene was replaced meanwhile, the engine refuses
        // instead of acting on the new one.
        let name = scenes
            .scenes
            .iter()
            .find(|scene| scene.id == selected)
            .map(|scene| scene.name.clone())
            .unwrap_or_default();
        let on_air = selected == scenes.active_id;
        ui.add_space(size::SPACE_8);
        ui.horizontal(|ui| {
            let put = ui
                .add(
                    Button::new(ButtonKind::Secondary, "Put on program")
                        .enabled(!on_air)
                        .min_width(ui.available_width() - size::CONTROL - size::SPACE_6),
                )
                .on_hover_text("Show the selected scene on program (double-click a scene)");
            if put.clicked() {
                take = Some((selected.clone(), name.clone()));
            }
            ui.add_enabled_ui(!on_air, |ui| {
                if widgets::icon_button(ui, Icon::Trash, "Delete the selected scene…").clicked() {
                    delete = Some((selected.clone(), name.clone()));
                }
            });
        });

        if let Some((id, name)) = take {
            self.model.activate_scene(&id, &name);
        }
        if let Some((id, name)) = delete {
            self.dialog = Some(Dialog::Confirm(Confirm::DeleteScene { id, name }));
        }
    }

    /// The field for a new scene's name: Enter creates it, Escape or
    /// leaving it empty cancels.
    fn new_scene_field(&mut self, ui: &mut egui::Ui) {
        let Some(name) = &mut self.new_scene else {
            return;
        };
        ui.add_space(size::SPACE_4);
        let field = ui.add(
            egui::TextEdit::singleline(name)
                .hint_text("Scene name, then Enter")
                .desired_width(f32::INFINITY),
        );
        if !field.has_focus() && !field.lost_focus() && name.is_empty() {
            field.request_focus();
        }
        if field.lost_focus() {
            let enter = ui.input(|input| input.key_pressed(egui::Key::Enter));
            let name = self.new_scene.take().unwrap_or_default();
            let name = name.trim();
            if enter && !name.is_empty() {
                self.model.create_scene(name);
            }
        }
        ui.label(TextRole::Meta.text("Enter creates the scene, Esc cancels."));
    }
}
