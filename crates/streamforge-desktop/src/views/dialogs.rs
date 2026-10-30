//! Modal dialogs: confirmations for consequential actions, adding a
//! source, a channel or a bus, a channel's input, the output settings
//! (targets, video encoder and bitrate, the monitor's device), and opening
//! and saving projects.
//! One is open at a time; Escape or a click outside cancels.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use command_api::{
    EncoderPreference, MIN_GAIN_DB, OutputKind, StreamKey, StreamTarget, VIDEO_BITRATE_KBPS,
};
use eframe::egui;

use crate::app::{Confirm, DesktopApp, Dialog};
use crate::model::{InputForm, InputType, Level, Model, SourceForm, SourceType};
use crate::ui::icons::Icon;
use crate::ui::theme::{TextRole, color, size};
use crate::ui::widgets::{self, Button, ButtonKind, ListRow, StatusLevel};
use crate::workspace;

use super::{camera_mode_picker, coordinate_range, labelled, opacity_row, refresh_cameras};

const DIALOG_WIDTH: f32 = 420.0;
/// Problems of a project file shown before the rest are counted.
const PROBLEMS_SHOWN: usize = 4;
/// A test tone's frequency; the engine takes up to half the sample rate.
const TONE_HZ: std::ops::RangeInclusive<f64> = 20.0..=20_000.0;

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
                    } => add_source(
                        ui,
                        &mut self.model,
                        self.project_path.as_deref(),
                        scene_id,
                        scene_name,
                        form,
                    ),
                    Dialog::RecordSettings { file_name, encoder } => {
                        self.record_settings(ui, file_name, encoder)
                    }
                    Dialog::StreamSettings {
                        url,
                        key,
                        go_live,
                        encoder,
                        bitrate_kbps,
                    } => self.stream_settings(ui, url, key, *go_live, (encoder, bitrate_kbps)),
                    Dialog::MonitorSettings { device } => {
                        monitor_settings(ui, &mut self.model, device)
                    }
                    Dialog::AddChannel { name, input } => {
                        add_channel(ui, &mut self.model, name, input)
                    }
                    Dialog::ChannelInput { id, name, input } => {
                        channel_input(ui, &mut self.model, id, name, input)
                    }
                    Dialog::AddBus { name, assigned } => {
                        add_bus(ui, &mut self.model, name, assigned)
                    }
                    Dialog::OpenProject {
                        selected,
                        open_when_checked,
                    } => self.open_project(ui, selected, open_when_checked),
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
            Confirm::RemoveChannel { name, .. } => (
                format!("Remove channel “{name}”?"),
                "Its sends are removed with it.".to_owned(),
                "Remove channel",
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
                    Confirm::RemoveChannel { id, name } => {
                        self.model.remove_channel_confirmed(id, name);
                    }
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

    /// Unsaved changes or running outputs when the window closes: save the
    /// changes (to the current file, or one chosen now), discard them, stop
    /// the outputs, or stay. Closing stops the outputs either way.
    fn confirm_quit(&mut self, ui: &mut egui::Ui, lost: &str) -> Outcome {
        let unsaved = self.model.has_changes();
        let (recording, live) = self.outputs_on_air();
        let title = match (unsaved, recording, live) {
            (true, ..) => "Quit with unsaved changes?",
            (false, true, true) => "Quit while recording and live?",
            (false, true, false) => "Quit while recording?",
            (false, false, true) => "Quit while live?",
            // Both stopped while the question was open.
            (false, false, false) => "Quit StreamForge?",
        };
        let stops = match (recording, live) {
            (true, true) => Some("The recording stops and its file is finalized; the stream ends."),
            (true, false) => Some("The recording stops and its file is finalized."),
            (false, true) => Some("The stream ends; viewers lose it."),
            (false, false) => None,
        };
        ui.label(TextRole::Title.text(title));
        for text in stops.into_iter().chain(unsaved.then_some(lost)) {
            ui.label(TextRole::Body.text(text).color(color::TEXT_SECONDARY));
        }
        ui.add_space(size::SPACE_8);
        if !unsaved {
            let quit = if stops.is_some() {
                "Stop and quit"
            } else {
                "Quit"
            };
            return match buttons(ui, ButtonKind::Danger, quit, true) {
                Some(true) => {
                    self.quit(ui.ctx());
                    Outcome::Done
                }
                Some(false) => Outcome::Cancel,
                None => Outcome::Open,
            };
        }
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
    /// and only a valid one opens, while the pipeline is idle. A
    /// double-click opens it once the check is back.
    fn open_project(
        &mut self,
        ui: &mut egui::Ui,
        selected: &mut Option<String>,
        open_when_checked: &mut bool,
    ) -> Outcome {
        ui.label(TextRole::Title.text("Open a project"));
        self.project_folder_line(ui);
        let mut chosen = None;
        let mut open_now = false;
        match &self.model.projects {
            None => {
                widgets::loading(ui, "Looking for project files…");
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
        if let Some(file_name) = chosen {
            // A click checks again (the file may have changed), unless a
            // check is on its way.
            if !self.model.validating(&file_name) {
                self.model.validate_project(&file_name);
            }
            if open_now {
                *open_when_checked = true;
            } else if selected.as_deref() != Some(&file_name) {
                *open_when_checked = false;
            }
            *selected = Some(file_name);
        }
        // One status line from the start, so the centred dialog does not
        // move under the pointer when the check of a click shows (a
        // double-click's second click would miss the row).
        let listed = matches!(&self.model.projects, Some(Ok(list)) if !list.files.is_empty());
        let valid = match selected.as_deref() {
            Some(file_name) => validation_lines(ui, &self.model, file_name),
            None => {
                if listed {
                    widgets::status_line(
                        ui,
                        StatusLevel::Info,
                        "Choose a file: it is checked before it opens.",
                    );
                }
                false
            }
        };
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
        // A double-clicked file whose check came back: open it if it can,
        // otherwise forget the double-click (the reason shows).
        let checked = selected
            .as_deref()
            .is_some_and(|file_name| !self.model.validating(file_name));
        let open_checked = *open_when_checked && checked && ready;
        if checked {
            *open_when_checked = false;
        }
        let open = match buttons(ui, ButtonKind::Primary, "Open", ready) {
            Some(true) => true,
            Some(false) => return Outcome::Cancel,
            None => open_checked,
        };
        match (open, selected.as_deref()) {
            (true, Some(file_name)) => {
                if self.model.has_changes() {
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
        let problem =
            !file.is_empty() && command_api::project_file_name("save_project", &file).is_err();
        let exists = matches!(&self.model.projects, Some(Ok(list))
            if list.files.iter().any(|listed| workspace::same_file_name(&listed.file_name, &file)));
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

    fn record_settings(
        &mut self,
        ui: &mut egui::Ui,
        file_name: &mut String,
        encoder: &mut Option<EncoderPreference>,
    ) -> Outcome {
        let current = self
            .model
            .profiles
            .as_ref()
            .map(|profiles| profiles.recording.video_encoder);
        // From the profile once it is known, so an untouched dialog changes
        // nothing.
        if encoder.is_none() {
            *encoder = current;
        }
        ui.label(TextRole::Title.text("Recording"));
        ui.label(TextRole::Label.text("File name"));
        ui.add(
            egui::TextEdit::singleline(file_name)
                .hint_text("Automatic")
                .desired_width(f32::INFINITY),
        );
        ui.label(TextRole::Meta.text(
            "Saved in the recordings directory, never over another file. Left empty, \
             each recording is named by its start time, e.g. recording-2026-10-27_15-30-00.mkv.",
        ));
        match encoder {
            Some(encoder) => encoder_picker(ui, encoder),
            None => {
                ui.label(TextRole::Meta.text("Reading the output settings…"));
            }
        }
        if self.model.recording.is_some() {
            ui.label(TextRole::Meta.text("A change applies to the next recording."));
        }
        ui.add_space(size::SPACE_8);
        let name = file_name.trim();
        match buttons(ui, ButtonKind::Primary, "Done", true) {
            Some(true) => {
                self.record_file = name.to_owned();
                if let (Some(chosen), Some(current)) = (*encoder, current)
                    && chosen != current
                {
                    self.model
                        .set_video_settings(OutputKind::Recording, Some(chosen), None);
                }
                Outcome::Done
            }
            Some(false) => Outcome::Cancel,
            None => Outcome::Open,
        }
    }

    /// The target is checked with the engine's rules as it is typed, so a
    /// mistake shows here and not as a failed start. Changed video settings
    /// reach the engine before the stream starts.
    fn stream_settings(
        &mut self,
        ui: &mut egui::Ui,
        url: &mut String,
        key: &mut String,
        go_live: bool,
        (encoder, bitrate_kbps): (&mut Option<EncoderPreference>, &mut Option<u32>),
    ) -> Outcome {
        let current = self.model.profiles.as_ref().map(|profiles| {
            (
                profiles.stream.video_encoder,
                profiles.stream.video_bitrate_kbps,
            )
        });
        // From the profile once it is known, as for the recording.
        if let (None, Some((current_encoder, current_kbps))) = (*encoder, current) {
            *encoder = Some(current_encoder);
            *bitrate_kbps = Some(current_kbps);
        }
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
        match (encoder.as_mut(), bitrate_kbps.as_mut()) {
            (Some(encoder), Some(bitrate_kbps)) => {
                ui.horizontal(|ui| {
                    ui.label(TextRole::Label.text("Video bitrate"));
                    let field = ui.add(
                        egui::DragValue::new(bitrate_kbps)
                            .range(VIDEO_BITRATE_KBPS)
                            .speed(50.0)
                            .suffix(" kbps"),
                    );
                    labelled(ui, field, "Video bitrate (kbps)");
                });
                encoder_picker(ui, encoder);
            }
            _ => {
                ui.label(TextRole::Meta.text("Reading the output settings…"));
            }
        }
        if self.model.streaming.is_some() {
            ui.label(TextRole::Meta.text("Video settings apply to the next stream."));
        }
        ui.add_space(size::SPACE_8);
        let ready = problem.is_none() && (!target.is_empty() || !go_live);
        let action = if go_live { "Go live" } else { "Done" };
        match buttons(ui, ButtonKind::Primary, action, ready) {
            Some(true) => {
                self.stream_url = target.to_owned();
                self.stream_key = std::mem::take(key);
                let (encoder, bitrate) = match current {
                    Some((current_encoder, current_kbps)) => (
                        encoder.filter(|chosen| *chosen != current_encoder),
                        bitrate_kbps.filter(|chosen| *chosen != current_kbps),
                    ),
                    None => (None, None),
                };
                // The engine answers in order, so a start below takes these
                // settings; it does not start without them.
                let sent = (encoder.is_none() && bitrate.is_none())
                    || self
                        .model
                        .set_video_settings(OutputKind::Stream, encoder, bitrate);
                if go_live && sent {
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
    project_folder: Option<&Path>,
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
    widgets::segmented(
        ui,
        &mut form.kind,
        &[
            (SourceType::Solid, "Solid colour"),
            (SourceType::ColorBars, "Colour bars"),
            (SourceType::Screen, "Screen capture"),
            (SourceType::Image, "Image"),
            (SourceType::Camera, "Camera"),
        ],
    );
    match form.kind {
        SourceType::Solid => {
            ui.horizontal(|ui| {
                ui.label(TextRole::Label.text("Colour"));
                ui.color_edit_button_srgba_unmultiplied(&mut form.color);
            });
        }
        SourceType::Screen => screen_source_picker(ui, model, form),
        SourceType::Image => image_picker(ui, model, project_folder, form),
        SourceType::Camera => camera_picker(ui, model, form),
        SourceType::ColorBars => {}
    }
    // Without a rectangle an image keeps its own size; the rest fill the
    // canvas.
    let whole_label = if form.kind == SourceType::Image {
        "Own size"
    } else {
        "Fill the canvas"
    };
    let mut whole = form.rect.is_none();
    ui.horizontal(|ui| {
        if widgets::toggle(ui, &mut whole, whole_label).changed() {
            form.rect = (!whole).then_some([0.0, 0.0, 960.0, 540.0]);
        }
        ui.label(TextRole::Body.text(whole_label));
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
    ui.label(TextRole::Label.text("Opacity"));
    let mut percent = form.opacity * 100.0;
    if opacity_row(ui, &mut percent)
        .iter()
        .any(egui::Response::changed)
    {
        form.opacity = (percent / 100.0).clamp(0.0, 1.0);
    }
    ui.add_space(size::SPACE_8);
    let complete = match form.kind {
        SourceType::Image => form.image_file.is_some(),
        SourceType::Camera => form.camera_id.is_some(),
        _ => true,
    };
    match buttons(
        ui,
        ButtonKind::Primary,
        "Add source",
        !form.name.trim().is_empty() && complete,
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

fn add_channel(
    ui: &mut egui::Ui,
    model: &mut Model,
    name: &mut String,
    input: &mut InputForm,
) -> Outcome {
    ui.label(TextRole::Title.text("New channel"));
    ui.label(TextRole::Label.text("Name"));
    ui.add(
        egui::TextEdit::singleline(name)
            .hint_text("e.g. Host mic")
            .desired_width(f32::INFINITY),
    );
    input_form(ui, model, input);
    ui.label(TextRole::Meta.text("It sends to the program bus; route it in the matrix."));
    ui.add_space(size::SPACE_8);
    match buttons(
        ui,
        ButtonKind::Primary,
        "Add channel",
        !name.trim().is_empty(),
    ) {
        Some(true) => {
            model.add_channel(name.trim(), &input.input());
            Outcome::Done
        }
        Some(false) => Outcome::Cancel,
        None => Outcome::Open,
    }
}

/// Choose what a channel plays; what it plays now (another client may
/// have changed it meanwhile) needs no change, and a channel that is gone
/// cannot take one.
fn channel_input(
    ui: &mut egui::Ui,
    model: &mut Model,
    id: &str,
    name: &str,
    input: &mut InputForm,
) -> Outcome {
    ui.label(TextRole::Title.text(format!("Input of “{name}”")));
    input_form(ui, model, input);
    ui.add_space(size::SPACE_8);
    let chosen = input.input();
    let current = model.mixer.as_ref().and_then(|mixer| {
        mixer
            .channels
            .iter()
            .find(|channel| channel.spec.id == id)
            .map(|channel| &channel.spec.input)
    });
    let changed = current.is_some_and(|current| *current != chosen);
    match buttons(ui, ButtonKind::Primary, "Use input", changed) {
        Some(true) => {
            model.set_channel_input(id, &chosen);
            Outcome::Done
        }
        Some(false) => Outcome::Cancel,
        None => Outcome::Open,
    }
}

/// What a channel plays: a device from the engine's list (desktop audio
/// apart), the system's default input, or a test tone.
fn input_form(ui: &mut egui::Ui, model: &mut Model, form: &mut InputForm) {
    ui.label(TextRole::Label.text("Input"));
    widgets::segmented(
        ui,
        &mut form.kind,
        &[
            (InputType::Device, "Device"),
            (InputType::Tone, "Test tone"),
        ],
    );
    match form.kind {
        InputType::Device => device_picker(ui, model, form),
        InputType::Tone => {
            ui.horizontal(|ui| {
                ui.label(TextRole::Label.text("Frequency"));
                let field = ui.add(
                    egui::DragValue::new(&mut form.frequency_hz)
                        .range(TONE_HZ)
                        .clamp_existing_to_range(false)
                        .speed(1.0)
                        .fixed_decimals(0)
                        .suffix(" Hz"),
                );
                labelled(ui, field, "Tone frequency (Hz)");
                ui.label(TextRole::Label.text("Level"));
                let field = ui.add(
                    egui::DragValue::new(&mut form.level_dbfs)
                        .range(f64::from(MIN_GAIN_DB)..=0.0)
                        .speed(0.1)
                        .fixed_decimals(1)
                        .suffix(" dBFS"),
                );
                labelled(ui, field, "Tone level (dBFS)");
            });
        }
    }
}

/// Choose a listed audio input, or the system's default one. A device
/// that is not listed (unplugged) stays choosable under its name.
fn device_picker(ui: &mut egui::Ui, model: &mut Model, form: &mut InputForm) {
    const DEFAULT: &str = "System default input";
    // Without a list (still asked for, or failed), nothing counts as
    // missing.
    let listed = match &model.audio_inputs {
        Some(Ok(inputs)) => Some(inputs.as_slice()),
        _ => None,
    };
    let chosen = form.device.clone().map(|name| {
        let input = listed.and_then(|listed| listed.iter().find(|input| input.name == name));
        (name, input)
    });
    let unlisted = |name: &String| match listed {
        Some(_) => format!("{name} (not found)"),
        None => name.clone(),
    };
    let selected = match &chosen {
        None => DEFAULT.to_owned(),
        Some((_, Some(input))) => workspace::input_name(input).to_owned(),
        Some((name, None)) => unlisted(name),
    };
    let width = ui.available_width();
    widgets::dropdown(ui, &selected, "Audio input", width, |ui| {
        widgets::choice(ui, &mut form.device, None, DEFAULT);
        if let Some((name, None)) = &chosen {
            widgets::choice(ui, &mut form.device, Some(name.clone()), &unlisted(name));
        }
        for (heading, desktop) in [("Inputs", false), ("Desktop audio", true)] {
            let mut group = listed
                .unwrap_or_default()
                .iter()
                .filter(|input| input.loopback_of.is_some() == desktop)
                .peekable();
            if group.peek().is_none() {
                continue;
            }
            widgets::menu_heading(ui, heading);
            for input in group {
                widgets::choice(
                    ui,
                    &mut form.device,
                    Some(input.name.clone()),
                    workspace::input_name(input),
                )
                .on_hover_text(&input.name);
            }
        }
    })
    .on_hover_text("Audio input");
    // One line, always there: the dialog is centred and must not jump.
    let desktop = chosen
        .and_then(|(_, input)| input)
        .and_then(|input| input.loopback_of.as_deref());
    let note = match (&model.audio_inputs, desktop) {
        (None, _) => "Looking for audio inputs…".to_owned(),
        (Some(Err(reason)), _) => format!("The audio inputs could not be listed: {reason}"),
        (Some(Ok(inputs)), _) if inputs.is_empty() => "No audio inputs found.".to_owned(),
        (_, Some(_)) => "Desktop audio: what an output plays. A monitor on that output whose \
                         mix has this channel feeds back."
            .to_owned(),
        _ => "The device is captured while the preview or an output runs.".to_owned(),
    };
    ui.horizontal(|ui| {
        if ui
            .add(
                Button::new(ButtonKind::Ghost, "Refresh")
                    .icon(Icon::Restart)
                    .busy(model.finding_audio_inputs()),
            )
            .on_hover_text("List the audio inputs again")
            .clicked()
        {
            model.find_audio_inputs();
        }
        ui.add(egui::Label::new(TextRole::Meta.text(note)).wrap());
    });
}

/// Which H.264 encoders an output tries (ADR 0011), with what each choice
/// means; one note line, always there.
fn encoder_picker(ui: &mut egui::Ui, encoder: &mut EncoderPreference) {
    ui.label(TextRole::Label.text("Video encoder"));
    widgets::segmented(
        ui,
        encoder,
        &[
            (EncoderPreference::Auto, "Auto"),
            (EncoderPreference::Software, "Software"),
            (EncoderPreference::Hardware, "Hardware"),
        ],
    );
    let note = match encoder {
        EncoderPreference::Auto => "A hardware encoder if one opens on this machine, else x264.",
        EncoderPreference::Software => "x264 on the CPU: the same on every machine, more load.",
        EncoderPreference::Hardware => {
            "A hardware encoder only; the output does not start without one."
        }
    };
    ui.label(TextRole::Meta.text(note));
}

/// Choose the monitor's device while the pipeline is idle, as the engine
/// requires; the bus it plays is chosen in the routing matrix.
fn monitor_settings(ui: &mut egui::Ui, model: &mut Model, device: &mut Option<String>) -> Outcome {
    ui.label(TextRole::Title.text("Monitor"));
    let profile = model.profiles.as_ref().map(|profiles| &profiles.monitor);
    let bus = profile.and_then(|profile| model.bus_name(&profile.mix));
    ui.label(TextRole::Meta.text(format!(
        "Plays {} for listening; it is never recorded or streamed. Its mix is chosen in the \
         routing matrix.",
        bus.unwrap_or("its mix")
    )));
    let current = profile.map(|profile| profile.device.clone());
    let idle = model.pipeline_idle();
    ui.label(TextRole::Label.text("Output device"));
    ui.add_enabled_ui(idle, |ui| output_picker(ui, model, device, idle));
    ui.add_space(size::SPACE_8);
    let changed = current.is_some_and(|current| current != *device);
    match buttons(ui, ButtonKind::Primary, "Use device", idle && changed) {
        Some(true) => {
            model.set_monitor_device(device.as_deref());
            Outcome::Done
        }
        Some(false) => Outcome::Cancel,
        None => Outcome::Open,
    }
}

/// Choose a listed audio output, or the system's default one. A device
/// that is not listed (unplugged) stays choosable under its name.
fn output_picker(ui: &mut egui::Ui, model: &mut Model, device: &mut Option<String>, idle: bool) {
    const DEFAULT: &str = "System default output";
    let listed = model.listed_audio_outputs();
    let chosen = workspace::output_label(device.as_deref(), listed);
    let unlisted = format!("{} (not found)", chosen.text);
    let selected = match device {
        None => DEFAULT,
        Some(_) if chosen.missing => &unlisted,
        Some(_) => &chosen.text,
    };
    let width = ui.available_width();
    widgets::dropdown(ui, selected, "Audio output", width, |ui| {
        widgets::choice(ui, device, None, DEFAULT);
        if chosen.missing {
            let name = device.clone();
            widgets::choice(ui, device, name, &unlisted);
        }
        for output in listed.unwrap_or_default() {
            widgets::choice(
                ui,
                device,
                Some(output.name.clone()),
                workspace::output_name(output),
            )
            .on_hover_text(&output.name);
        }
    })
    .on_hover_text("Audio output");
    // One line, always there: the dialog is centred and must not jump.
    let note = match &model.audio_outputs {
        _ if !idle => "Stop the preview and the outputs to choose another device.".to_owned(),
        None => "Looking for audio outputs…".to_owned(),
        Some(Err(reason)) => format!("The audio outputs could not be listed: {reason}"),
        Some(Ok(outputs)) if outputs.is_empty() => "No audio outputs found.".to_owned(),
        Some(Ok(_)) => "The device opens when the monitor is on and the pipeline runs.".to_owned(),
    };
    ui.horizontal(|ui| {
        if ui
            .add(
                Button::new(ButtonKind::Ghost, "Refresh")
                    .icon(Icon::Restart)
                    .busy(model.finding_audio_outputs()),
            )
            .on_hover_text("List the audio outputs again")
            .clicked()
        {
            model.find_audio_outputs();
        }
        ui.add(egui::Label::new(TextRole::Meta.text(note)).wrap());
    });
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
                widgets::status_line(ui, StatusLevel::Info, "Checking the file…");
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
    // Linux and macOS have a system picker; Windows has none yet.
    const PICKER: &str = if cfg!(any(target_os = "linux", target_os = "macos")) {
        "Choose in the system dialog"
    } else {
        "System picker (if the platform has one)"
    };
    match &model.screen_sources {
        Some(Ok(sources)) if !sources.is_empty() => {
            let selected = form
                .screen_source
                .as_ref()
                .and_then(|id| sources.iter().find(|source| &source.id == id))
                .map_or(PICKER, |source| source.name.as_str());
            let width = ui.available_width();
            widgets::dropdown(ui, selected, "Screen source", width, |ui| {
                widgets::choice(ui, &mut form.screen_source, None, PICKER);
                for source in sources {
                    let label = format!("{} ({})", source.name, source.kind.as_str());
                    widgets::choice(ui, &mut form.screen_source, Some(source.id.clone()), &label);
                }
            })
            .on_hover_text("Screen source");
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

/// Choose an image file from the project folder. There is no file dialog
/// (ADR 0019): the folder opens in the file manager to put files there.
fn image_picker(
    ui: &mut egui::Ui,
    model: &mut Model,
    project_folder: Option<&Path>,
    form: &mut SourceForm,
) {
    if model.images.is_none() {
        model.list_images();
    }
    // A file that is no longer listed is not sent.
    let listed = |file: &String| matches!(&model.images, Some(Ok(list)) if list.files.iter().any(|info| &info.file_name == file));
    if form.image_file.as_ref().is_some_and(|file| !listed(file)) {
        form.image_file = None;
    }
    match &model.images {
        Some(Ok(list)) if !list.files.is_empty() => {
            const NONE: &str = "Choose an image";
            let before = form.image_file.clone();
            let selected = before.as_deref().unwrap_or(NONE);
            let width = ui.available_width();
            widgets::dropdown(ui, selected, "Image file", width, |ui| {
                for info in &list.files {
                    let size = match (info.width, info.height) {
                        (Some(width), Some(height)) => format!("{width}×{height}"),
                        _ => "cannot be read".to_owned(),
                    };
                    let label = format!("{} · {size}", info.file_name);
                    widgets::choice(
                        ui,
                        &mut form.image_file,
                        Some(info.file_name.clone()),
                        &label,
                    );
                }
            })
            .on_hover_text("Image file");
            if list.omitted > 0 {
                ui.label(TextRole::Meta.text(format!("{} more files not listed", list.omitted)));
            }
            // A new choice names an unnamed source after its file.
            if form.image_file != before
                && form.name.trim().is_empty()
                && let Some(stem) = form
                    .image_file
                    .as_deref()
                    .and_then(|file| file.rsplit_once('.'))
                    .map(|(stem, _)| stem)
                && command_api::is_valid_name(stem)
            {
                stem.clone_into(&mut form.name);
            }
        }
        Some(Ok(_)) => {
            ui.label(TextRole::Meta.text(
                "No PNG or JPEG files in the project folder yet: put them there and refresh.",
            ));
        }
        Some(Err(reason)) => {
            ui.label(TextRole::Meta.text(reason.as_str()));
        }
        None => {
            ui.label(TextRole::Meta.text("Listing the project folder…"));
        }
    }
    ui.horizontal(|ui| {
        if ui
            .add(Button::new(ButtonKind::Ghost, "Refresh").enabled(!model.listing_images()))
            .on_hover_text("List the project folder's images again")
            .clicked()
        {
            model.list_images();
        }
        if let Some(folder) = project_folder
            && ui
                .add(Button::new(ButtonKind::Ghost, "Open folder").icon(Icon::Folder))
                .on_hover_text(format!("Open {} in the file manager", folder.display()))
                .clicked()
            && let Err(error) = crate::folder::open(folder)
        {
            model.push_log(
                Level::Warning,
                format!(
                    "Cannot open the project folder {}: {error}",
                    folder.display()
                ),
            );
        }
    });
}

/// Choose a camera from `list_cameras`, and one of its modes or none (the
/// engine chooses).
fn camera_picker(ui: &mut egui::Ui, model: &mut Model, form: &mut SourceForm) {
    refresh_cameras(model);
    // A camera that a new list leaves out is not sent; a list that failed
    // says nothing about it.
    let gone = |id: &String| {
        model
            .listed_cameras()
            .is_some_and(|cameras| !cameras.iter().any(|camera| &camera.camera_id == id))
    };
    if form.camera_id.as_ref().is_some_and(gone) {
        form.camera_id = None;
        form.camera_mode = None;
    }
    match &model.cameras {
        Some(Ok(cameras)) if !cameras.is_empty() => {
            const NONE: &str = "Choose a camera";
            let before = form.camera_id.clone();
            let find = |id: Option<&str>| {
                id.and_then(|id| cameras.iter().find(|camera| camera.camera_id == id))
            };
            let selected = find(before.as_deref()).map_or(NONE, |camera| camera.name.as_str());
            let width = ui.available_width();
            widgets::dropdown(ui, selected, "Camera device", width, |ui| {
                for camera in cameras {
                    widgets::choice(
                        ui,
                        &mut form.camera_id,
                        Some(camera.camera_id.clone()),
                        &camera.name,
                    )
                    .on_hover_text(&camera.camera_id);
                }
            })
            .on_hover_text("Camera device");
            let camera = find(form.camera_id.as_deref());
            if form.camera_id != before {
                // A mode the new camera lacks would only be replaced.
                form.camera_mode = workspace::mode_for_camera(form.camera_mode.as_ref(), camera);
                // A new choice names an unnamed source after its camera.
                if let Some(camera) = camera
                    && form.name.trim().is_empty()
                    && command_api::is_valid_name(&camera.name)
                {
                    camera.name.clone_into(&mut form.name);
                }
            }
            if let Some(camera) = camera {
                camera_mode_picker(ui, &mut form.camera_mode, camera, width);
            }
        }
        Some(Ok(_)) => {
            ui.label(TextRole::Meta.text("No cameras found: plug one in and refresh."));
        }
        Some(Err(reason)) => {
            ui.label(TextRole::Meta.text(reason.as_str()));
        }
        None => {
            ui.label(TextRole::Meta.text("Looking for cameras…"));
        }
    }
    if ui
        .add(Button::new(ButtonKind::Ghost, "Refresh").enabled(!model.listing_cameras()))
        .on_hover_text("List the cameras again")
        .clicked()
    {
        model.list_cameras();
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
