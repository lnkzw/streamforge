//! Modal dialogs: confirmations for consequential actions, adding a
//! source or a bus, the output targets, and opening and saving projects.
//! One is open at a time; Escape or a click outside cancels.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use command_api::{StreamKey, StreamTarget};
use eframe::egui;

use crate::app::{Confirm, DesktopApp, Dialog};
use crate::model::{Model, SourceForm, SourceType};
use crate::ui::icons::Icon;
use crate::ui::theme::{TextRole, color, size};
use crate::ui::widgets::{self, Button, ButtonKind, ListRow, StatusLevel};
use crate::workspace;

use super::coordinate_range;

const DIALOG_WIDTH: f32 = 420.0;
/// Problems of a project file shown before the rest are counted.
const PROBLEMS_SHOWN: usize = 4;

/// What the operator did in a dialog this frame.
enum Outcome {
    Open,
    Cancel,
    Done,
}

impl DesktopApp {
    pub(crate) fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        // A light backdrop: the program stays readable behind a dialog.
        let modal = egui::Modal::new(egui::Id::new("dialog"))
            .backdrop_color(egui::Color32::from_black_alpha(64))
            .frame(
                egui::Frame::new()
                    .fill(color::BG_PANEL)
                    .stroke(egui::Stroke::new(size::STROKE, color::LINE_STRONG))
                    .corner_radius(size::RADIUS_POPUP)
                    .shadow(ctx.global_style().visuals.window_shadow)
                    .inner_margin(size::SPACE_16),
            )
            .show(ctx, |ui| {
                ui.set_width(DIALOG_WIDTH);
                ui.spacing_mut().item_spacing.y = size::SPACE_8;
                match &mut dialog {
                    Dialog::Confirm(confirm) => self.confirm(ui, confirm),
                    Dialog::AddSource {
                        scene_id,
                        scene_name,
                        form,
                    } => add_source(ui, &mut self.model, scene_id, scene_name, form),
                    Dialog::RecordSettings { file_name } => self.record_settings(ui, file_name),
                    Dialog::StreamSettings { url, key, go_live } => {
                        self.stream_settings(ui, url, key, *go_live)
                    }
                    Dialog::AddBus { name, assigned } => {
                        add_bus(ui, &mut self.model, name, assigned)
                    }
                    Dialog::OpenProject { selected } => self.open_project(ui, selected),
                    Dialog::SaveProject { file_name, quit } => {
                        self.save_project_dialog(ui, file_name, *quit)
                    }
                }
            });
        let keep = match modal.inner {
            Outcome::Open => !modal.should_close(),
            Outcome::Cancel | Outcome::Done => false,
        };
        // An action in the dialog may have opened another one.
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }

    fn confirm(&mut self, ui: &mut egui::Ui, confirm: &Confirm) -> Outcome {
        let lost = match &self.model.project {
            Some(project) => format!("Changes to “{project}” since it was saved are lost."),
            None => "The current setup was never saved; it is lost.".to_owned(),
        };
        let (title, text, action) = match confirm {
            Confirm::Quit => return self.confirm_quit(ui, &lost),
            Confirm::OpenProject { file_name } => {
                (format!("Open “{file_name}”?"), lost, "Discard and open")
            }
            Confirm::DeleteScene { name, .. } => (
                format!("Delete scene “{name}”?"),
                "Its sources are deleted with it. This cannot be undone.".to_owned(),
                "Delete scene",
            ),
            Confirm::RemoveSource { name, .. } => (
                format!("Remove source “{name}”?"),
                "It disappears from the scene, and from program if the scene is on air.".to_owned(),
                "Remove source",
            ),
            Confirm::RestartSource { name, .. } => (
                format!("Restart the capture of “{name}”?"),
                "The working capture ends and starts again; outputs keep running.".to_owned(),
                "Restart capture",
            ),
            Confirm::RemoveBus { name, .. } => (
                format!("Remove bus “{name}”?"),
                "Every send to it is removed with it.".to_owned(),
                "Remove bus",
            ),
            Confirm::StopRecording => (
                "Stop recording?".to_owned(),
                "The file is finalized; a new recording starts a new file.".to_owned(),
                "Stop recording",
            ),
            Confirm::StopStreaming => (
                "End the stream?".to_owned(),
                "Viewers lose the stream.".to_owned(),
                "End stream",
            ),
        };
        ui.label(TextRole::Title.text(title));
        ui.label(TextRole::Body.text(text).color(color::TEXT_SECONDARY));
        ui.add_space(size::SPACE_8);
        match buttons(ui, ButtonKind::Danger, action, true) {
            Some(true) => {
                match confirm {
                    Confirm::DeleteScene { id, name } => {
                        self.model.delete_scene_confirmed(id, name);
                    }
                    Confirm::RemoveSource { id, .. } => {
                        if self.selected_source.as_deref() == Some(id) {
                            self.select_source(None);
                        }
                        self.model.remove_source(id);
                    }
                    Confirm::RestartSource { id, .. } => self.model.restart_source(id, None),
                    Confirm::RemoveBus { id, name } => self.model.remove_bus_confirmed(id, name),
                    Confirm::OpenProject { file_name } => self.model.load_project(file_name),
                    Confirm::Quit => {}
                    Confirm::StopRecording => self.model.stop_recording(),
                    Confirm::StopStreaming => self.model.stop_streaming(),
                }
                Outcome::Done
            }
            Some(false) => Outcome::Cancel,
            None => Outcome::Open,
        }
    }

    /// Unsaved changes when the window closes: save them (to the current
    /// file, or one chosen now), discard them, or stay.
    fn confirm_quit(&mut self, ui: &mut egui::Ui, lost: &str) -> Outcome {
        ui.label(TextRole::Title.text("Quit with unsaved changes?"));
        ui.label(TextRole::Body.text(lost).color(color::TEXT_SECONDARY));
        ui.add_space(size::SPACE_8);
        let save = if self.model.project.is_some() {
            "Save and quit"
        } else {
            "Save as and quit…"
        };
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(Button::new(ButtonKind::Primary, save).min_width(96.0))
                .clicked()
            {
                // To the current file, or through the save dialog, which
                // replaces this one.
                self.save_project(true);
                return Outcome::Done;
            }
            if ui
                .add(Button::new(ButtonKind::Danger, "Quit without saving").min_width(96.0))
                .clicked()
            {
                self.quit(ui.ctx());
                return Outcome::Done;
            }
            if ui
                .add(Button::new(ButtonKind::Secondary, "Cancel").min_width(80.0))
                .clicked()
            {
                return Outcome::Cancel;
            }
            Outcome::Open
        })
        .inner
    }

    /// The project folder's files; a chosen one is checked by the engine,
    /// and only a valid one opens, while the pipeline is idle.
    fn open_project(&mut self, ui: &mut egui::Ui, selected: &mut Option<String>) -> Outcome {
        ui.label(TextRole::Title.text("Open a project"));
        self.project_folder_line(ui);
        let mut chosen = None;
        let mut open_now = false;
        match &self.model.projects {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(TextRole::Meta.text("Looking for project files…"));
                });
            }
            Some(Err(reason)) => {
                widgets::status_line(ui, StatusLevel::Error, reason);
            }
            Some(Ok(list)) if list.files.is_empty() => {
                widgets::empty_state(ui, Icon::File, "No project files in this folder yet.");
            }
            Some(Ok(list)) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                egui::ScrollArea::vertical()
                    .id_salt("project files")
                    .max_height(240.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for file in &list.files {
                            let size = workspace::file_size(file.bytes);
                            let meta = match file.modified_unix_ms {
                                Some(ms) => format!(
                                    "{size} · {}",
                                    workspace::written_ago(Duration::from_millis(ms), now)
                                ),
                                None => size,
                            };
                            let row = ui.add(
                                ListRow::new(Icon::File, &file.file_name)
                                    .meta(&meta)
                                    .selected(selected.as_deref() == Some(&file.file_name)),
                            );
                            if row.clicked() || row.double_clicked() {
                                chosen = Some(file.file_name.clone());
                                open_now = row.double_clicked();
                            }
                        }
                    });
                if list.omitted > 0 {
                    ui.label(
                        TextRole::Meta
                            .text(format!("{} more file(s) are not listed.", list.omitted)),
                    );
                }
            }
        }
        if let Some(file_name) = chosen
            && selected.as_deref() != Some(&file_name)
        {
            self.model.validate_project(&file_name);
            *selected = Some(file_name);
        }
        let valid = selected
            .as_deref()
            .is_some_and(|file_name| validation_lines(ui, &self.model, file_name));
        let idle = self
            .model
            .status
            .as_ref()
            .is_some_and(|status| !status.pipeline_running);
        if !idle {
            widgets::status_line(
                ui,
                StatusLevel::Warn,
                "Stop the preview and the outputs to open a project.",
            );
        }
        ui.add_space(size::SPACE_8);
        let ready = valid && idle;
        let open = match buttons(ui, ButtonKind::Primary, "Open", ready) {
            Some(true) => true,
            Some(false) => return Outcome::Cancel,
            None => open_now && ready,
        };
        match (open, selected.as_deref()) {
            (true, Some(file_name)) => {
                if self.model.unsaved {
                    self.dialog = Some(Dialog::Confirm(Confirm::OpenProject {
                        file_name: file_name.to_owned(),
                    }));
                } else {
                    self.model.load_project(file_name);
                }
                Outcome::Done
            }
            _ => Outcome::Open,
        }
    }

    /// A name for the project file; a name that exists is replaced only
    /// with this dialog's warning and its action saying so.
    fn save_project_dialog(
        &mut self,
        ui: &mut egui::Ui,
        file_name: &mut String,
        quit: bool,
    ) -> Outcome {
        ui.label(TextRole::Title.text("Save the project as"));
        self.project_folder_line(ui);
        ui.label(TextRole::Label.text("File name"));
        ui.add(
            egui::TextEdit::singleline(file_name)
                .hint_text("show.json")
                .desired_width(f32::INFINITY),
        );
        let file = workspace::project_file(file_name);
        let problem = !file.is_empty() && command_api::project_file_name(&file).is_err();
        let exists = matches!(&self.model.projects, Some(Ok(list))
            if list.files.iter().any(|listed| listed.file_name == file));
        if problem {
            widgets::status_line(
                ui,
                StatusLevel::Error,
                "Use a plain file name ending in .json, without folders and not a device \
                 name such as CON or NUL.",
            );
        } else if exists {
            widgets::status_line(
                ui,
                StatusLevel::Warn,
                &format!("“{file}” exists: saving replaces it."),
            );
        } else {
            ui.label(TextRole::Meta.text(
                "Scenes, audio and output settings are saved. Stream keys and what screen \
                 sources capture are not.",
            ));
        }
        ui.add_space(size::SPACE_8);
        // Whether the name exists is known once the folder was listed (a
        // listing that failed leaves it to the engine, which refuses).
        let ready = !file.is_empty() && !problem && self.model.projects.is_some();
        let (kind, action) = match (exists, quit) {
            (true, false) => (ButtonKind::Danger, "Replace"),
            (true, true) => (ButtonKind::Danger, "Replace and quit"),
            (false, false) => (ButtonKind::Primary, "Save"),
            (false, true) => (ButtonKind::Primary, "Save and quit"),
        };
        match buttons(ui, kind, action, ready) {
            Some(true) => {
                self.model.save_project(&file, exists);
                self.quit_after_save = quit;
                Outcome::Done
            }
            Some(false) => Outcome::Cancel,
            None => Outcome::Open,
        }
    }

    fn project_folder_line(&self, ui: &mut egui::Ui) {
        if let Some(folder) = &self.project_folder {
            ui.add(egui::Label::new(TextRole::Meta.text(format!("In {folder}"))).truncate())
                .on_hover_text(folder);
        }
    }

    fn record_settings(&mut self, ui: &mut egui::Ui, file_name: &mut String) -> Outcome {
        ui.label(TextRole::Title.text("Recording"));
        ui.label(TextRole::Label.text("File name"));
        ui.add(
            egui::TextEdit::singleline(file_name)
                .hint_text("recording.mkv")
                .desired_width(f32::INFINITY),
        );
        let note = if self.model.recording.is_some() {
            "Saved in the recordings directory. A change applies to the next recording."
        } else {
            "Saved in the recordings directory."
        };
        ui.label(TextRole::Meta.text(note));
        ui.add_space(size::SPACE_8);
        let name = file_name.trim();
        match buttons(ui, ButtonKind::Primary, "Done", !name.is_empty()) {
            Some(true) => {
                self.record_file = name.to_owned();
                Outcome::Done
            }
            Some(false) => Outcome::Cancel,
            None => Outcome::Open,
        }
    }

    /// The target is checked with the engine's rules as it is typed, so a
    /// mistake shows here and not as a failed start.
    fn stream_settings(
        &mut self,
        ui: &mut egui::Ui,
        url: &mut String,
        key: &mut String,
        go_live: bool,
    ) -> Outcome {
        ui.label(TextRole::Title.text("Stream"));
        ui.label(TextRole::Label.text("Server URL"));
        ui.add(
            egui::TextEdit::singleline(url)
                .hint_text("rtmp://host/app")
                .desired_width(f32::INFINITY),
        );
        ui.label(TextRole::Label.text("Stream key"));
        ui.add(
            egui::TextEdit::singleline(key)
                .password(true)
                .desired_width(f32::INFINITY),
        );
        let target = url.trim();
        let problem = if target.is_empty() {
            None
        } else {
            StreamTarget::new(target, StreamKey::new(key.as_str())).err()
        };
        match &problem {
            Some(problem) => {
                widgets::status_line(ui, StatusLevel::Error, problem);
            }
            None => {
                ui.label(TextRole::Meta.text(
                    "The key goes to the engine when the stream starts. It is not saved or shown.",
                ));
            }
        }
        ui.add_space(size::SPACE_8);
        let ready = problem.is_none() && (!target.is_empty() || !go_live);
        let action = if go_live { "Go live" } else { "Done" };
        match buttons(ui, ButtonKind::Primary, action, ready) {
            Some(true) => {
                self.stream_url = target.to_owned();
                self.stream_key = std::mem::take(key);
                if go_live {
                    self.model
                        .start_streaming(&self.stream_url, &self.stream_key);
                }
                Outcome::Done
            }
            Some(false) => Outcome::Cancel,
            None => Outcome::Open,
        }
    }
}

fn add_source(
    ui: &mut egui::Ui,
    model: &mut Model,
    scene_id: &str,
    scene_name: &str,
    form: &mut SourceForm,
) -> Outcome {
    ui.label(TextRole::Title.text(format!("Add a source to “{scene_name}”")));
    ui.label(TextRole::Label.text("Name"));
    ui.add(
        egui::TextEdit::singleline(&mut form.name)
            .hint_text("e.g. lower third")
            .desired_width(f32::INFINITY),
    );
    ui.label(TextRole::Label.text("Kind"));
    ui.horizontal(|ui| {
        for (kind, label) in [
            (SourceType::Solid, "Solid colour"),
            (SourceType::ColorBars, "Colour bars"),
            (SourceType::Screen, "Screen capture"),
        ] {
            ui.selectable_value(&mut form.kind, kind, label);
        }
    });
    match form.kind {
        SourceType::Solid => {
            ui.horizontal(|ui| {
                ui.label(TextRole::Label.text("Colour"));
                ui.color_edit_button_srgba_unmultiplied(&mut form.color);
            });
        }
        SourceType::Screen => screen_source_picker(ui, model, form),
        SourceType::ColorBars => {}
    }
    let mut whole = form.rect.is_none();
    ui.horizontal(|ui| {
        if widgets::toggle(ui, &mut whole, "Fill the canvas").changed() {
            form.rect = (!whole).then_some([0.0, 0.0, 960.0, 540.0]);
        }
        ui.label(TextRole::Body.text("Fill the canvas"));
    });
    if let Some(rect) = &mut form.rect {
        ui.horizontal(|ui| {
            for (index, (label, value)) in
                ["X", "Y", "W", "H"].iter().zip(rect.iter_mut()).enumerate()
            {
                ui.label(TextRole::Label.text(*label));
                ui.add(egui::DragValue::new(value).range(coordinate_range(index >= 2)));
            }
        });
    }
    ui.horizontal(|ui| {
        ui.label(TextRole::Label.text("Opacity"));
        let mut percent = form.opacity * 100.0;
        if ui
            .add(
                egui::Slider::new(&mut percent, 0.0..=100.0)
                    .suffix(" %")
                    .fixed_decimals(0),
            )
            .changed()
        {
            form.opacity = (percent / 100.0).clamp(0.0, 1.0);
        }
    });
    ui.add_space(size::SPACE_8);
    match buttons(
        ui,
        ButtonKind::Primary,
        "Add source",
        !form.name.trim().is_empty(),
    ) {
        Some(true) => {
            form.name = form.name.trim().to_owned();
            model.add_source(scene_id, form);
            Outcome::Done
        }
        Some(false) => Outcome::Cancel,
        None => Outcome::Open,
    }
}

fn add_bus(
    ui: &mut egui::Ui,
    model: &mut Model,
    name: &mut String,
    assigned: &mut bool,
) -> Outcome {
    ui.label(TextRole::Title.text("New bus"));
    ui.label(TextRole::Label.text("Name"));
    ui.add(
        egui::TextEdit::singleline(name)
            .hint_text("e.g. Voice")
            .desired_width(f32::INFINITY),
    );
    ui.horizontal(|ui| {
        widgets::toggle(ui, assigned, "Feed the program bus");
        ui.label(TextRole::Body.text("Feed the program bus"));
    });
    let note = if *assigned {
        "A submix: what channels send to it reaches program through it."
    } else {
        "A separate mix, e.g. for the monitor or a recording track."
    };
    ui.label(TextRole::Meta.text(note));
    ui.add_space(size::SPACE_8);
    match buttons(ui, ButtonKind::Primary, "Add bus", !name.trim().is_empty()) {
        Some(true) => {
            model.add_bus(name.trim(), *assigned);
            Outcome::Done
        }
        Some(false) => Outcome::Cancel,
        None => Outcome::Open,
    }
}

/// The engine's check of `file_name`: ready, converted on opening, or
/// its problems. Returns whether it can be opened.
fn validation_lines(ui: &mut egui::Ui, model: &Model, file_name: &str) -> bool {
    match &model.validation {
        Some((checked, Ok(validation))) if checked == file_name => {
            if !validation.valid {
                let count = validation.problems.len();
                widgets::status_line(
                    ui,
                    StatusLevel::Error,
                    &format!("{count} problem(s): this file cannot be opened."),
                );
                for problem in validation.problems.iter().take(PROBLEMS_SHOWN) {
                    let text = if problem.path.is_empty() {
                        problem.message.clone()
                    } else {
                        format!("{}: {}", problem.path, problem.message)
                    };
                    ui.add(egui::Label::new(TextRole::Meta.text(text)).truncate());
                }
                if count > PROBLEMS_SHOWN {
                    ui.label(TextRole::Meta.text(format!("and {} more", count - PROBLEMS_SHOWN)));
                }
                false
            } else if validation.migrates {
                widgets::status_line(
                    ui,
                    StatusLevel::Info,
                    "An older file version: it is converted when opened, and saving writes \
                     the current one.",
                );
                true
            } else {
                widgets::status_line(ui, StatusLevel::Ok, "A valid project.");
                true
            }
        }
        Some((checked, Err(reason))) if checked == file_name => {
            widgets::status_line(ui, StatusLevel::Error, reason);
            false
        }
        _ => {
            if model.validating(file_name) {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(TextRole::Meta.text("Checking the file…"));
                });
            }
            false
        }
    }
}

/// Choose a listed screen source, or leave it to the system picker.
fn screen_source_picker(ui: &mut egui::Ui, model: &mut Model, form: &mut SourceForm) {
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
            egui::ComboBox::from_id_salt("screen source")
                .selected_text(selected)
                .width(ui.available_width())
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
            ui.label(TextRole::Meta.text("No screen sources found."));
        }
        Some(Err(reason)) => {
            ui.label(TextRole::Meta.text(reason.as_str()));
        }
        None => {
            ui.label(TextRole::Meta.text(
                "Find screen sources to choose one; otherwise the system picker opens \
                 when the source starts, where the platform has one.",
            ));
        }
    }
    if ui
        .add(Button::new(ButtonKind::Ghost, "Find screen sources"))
        .clicked()
    {
        model.find_screen_sources();
    }
}

/// Cancel and the dialog's action, right-aligned. `Some(true)`: the
/// action; `Some(false)`: cancelled.
fn buttons(ui: &mut egui::Ui, kind: ButtonKind, action: &str, enabled: bool) -> Option<bool> {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if ui
            .add(Button::new(kind, action).enabled(enabled).min_width(96.0))
            .clicked()
        {
            return Some(true);
        }
        if ui
            .add(Button::new(ButtonKind::Secondary, "Cancel").min_width(80.0))
            .clicked()
        {
            return Some(false);
        }
        None
    })
    .inner
}
