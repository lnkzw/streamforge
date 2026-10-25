//! The top bar: the application's mark, the project menu (open, save,
//! save as), the transport (REC and LIVE with running timers) and the
//! health summary, which opens the details and the activity log.

use command_api::StreamState;
use eframe::egui;

use crate::app::{Confirm, DesktopApp, Dialog, HEALTH_STALE};
use crate::model::Level;
use crate::ui::icons::Icon;
use crate::ui::theme::{TextRole, color, size};
use crate::ui::widgets::{
    self, StatusLevel, TRANSPORT_WIDTH, Transport, TransportButton, TransportState,
};
use crate::workspace::{self, health_level, transport_state};

/// The widest the health summary grows before its text is cut.
const HEALTH_WIDTH: f32 = 360.0;
/// The widest the project menu grows before the name is cut.
const PROJECT_WIDTH: f32 = 240.0;

impl DesktopApp {
    pub(crate) fn top_bar(&mut self, ui: &mut egui::Ui) {
        let bar = ui.max_rect();
        // The transport sits in the middle of the bar, whatever the sides
        // hold.
        let transport = egui::Rect::from_center_size(
            bar.center(),
            egui::vec2(2.0 * TRANSPORT_WIDTH + size::SPACE_8, size::CONTROL_LARGE),
        );
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = size::SPACE_8;
            let (mark, _) =
                ui.allocate_exact_size(egui::Vec2::splat(size::ICON), egui::Sense::hover());
            Icon::Scene.paint(ui.painter(), mark, color::TEXT_PRIMARY);
            ui.label(TextRole::Title.text("StreamForge"));
            let (divider, _) =
                ui.allocate_exact_size(egui::vec2(1.0, size::CONTROL), egui::Sense::hover());
            ui.painter().vline(
                divider.center().x,
                divider.y_range(),
                egui::Stroke::new(size::STROKE, color::LINE_SUBTLE),
            );
            let room = transport.left() - ui.cursor().left() - size::SPACE_16;
            self.project_menu(ui, room.min(PROJECT_WIDTH));
        });

        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(transport)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = size::SPACE_8;
                self.record_button(ui);
                self.live_button(ui);
            },
        );

        let health = egui::Rect::from_min_max(
            egui::pos2(transport.right() + size::SPACE_16, bar.top()),
            bar.max,
        );
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(health)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| self.health_summary(ui),
        );
    }

    /// The project's file (or "Untitled"), marked while it has unsaved
    /// changes; its menu opens, saves and saves as.
    fn project_menu(&mut self, ui: &mut egui::Ui, width: f32) {
        let name = workspace::project_name(self.model.project.as_deref()).to_owned();
        let unsaved = self.model.unsaved;
        let label = if unsaved {
            format!("Project {name}, unsaved changes")
        } else {
            format!("Project {name}")
        };
        let response = widgets::menu_button(ui, Icon::File, &name, unsaved, &label, width);
        let mut chosen = None;
        egui::Popup::menu(&response).width(220.0).show(|ui| {
            ui.set_min_width(200.0);
            for (item, text) in [
                (ProjectItem::Open, "Open project…"),
                (ProjectItem::Save, "Save"),
                (ProjectItem::SaveAs, "Save as…"),
            ] {
                if ui.button(TextRole::Body.text(text)).clicked() {
                    chosen = Some(item);
                }
            }
            if let Some(folder) = &self.project_folder {
                ui.separator();
                ui.add(egui::Label::new(TextRole::Meta.text(folder.as_str())).truncate())
                    .on_hover_text(folder);
            }
        });
        match chosen {
            Some(ProjectItem::Open) => self.open_projects(),
            Some(ProjectItem::Save) => self.save_project(false),
            Some(ProjectItem::SaveAs) => self.save_project_as(false),
            None => {}
        }
    }

    pub(crate) fn record_transport(&self) -> TransportState {
        let status = self.model.status.as_ref();
        self.transport(
            Transport::Record,
            self.model.recording.is_some(),
            status.is_some_and(|status| status.recording_failed),
        )
    }

    pub(crate) fn live_transport(&self) -> TransportState {
        let status = self.model.status.as_ref();
        self.transport(
            Transport::Live,
            self.model.streaming.is_some(),
            status.is_some_and(|status| status.streaming_state == Some(StreamState::Failed)),
        )
    }

    /// An output's transport state from whether it is open, its start
    /// command, the status's failure flag and its health reading.
    fn transport(&self, output: Transport, open: bool, failed: bool) -> TransportState {
        let model = &self.model;
        let (start, health) = match output {
            Transport::Record => (
                "start_recording",
                model.health.as_ref().and_then(|h| h.recording.as_ref()),
            ),
            Transport::Live => (
                "start_streaming",
                model.health.as_ref().and_then(|h| h.streaming.as_ref()),
            ),
        };
        let age = model.health_age().unwrap_or_default();
        transport_state(
            open,
            model.pending(start),
            failed,
            health.map(|output| (output, age)),
        )
    }

    /// On air: the output is open and has not failed. A start still
    /// waiting for the engine is not, so the tally never lies.
    pub(crate) fn on_air(&self, output: Transport) -> bool {
        match output {
            Transport::Record => {
                self.model.recording.is_some() && self.record_transport() != TransportState::Failed
            }
            Transport::Live => {
                self.model.streaming.is_some() && self.live_transport() != TransportState::Failed
            }
        }
    }

    fn record_button(&mut self, ui: &mut egui::Ui) {
        let state = self.record_transport();
        let waiting = self.model.pending("start_recording") || self.model.pending("stop_recording");
        let response = ui
            .add(TransportButton::new(Transport::Record, state).enabled(!waiting))
            .on_disabled_hover_text("Waiting for the engine");
        if response.clicked() {
            match state {
                TransportState::Idle => self.model.start_recording(&self.record_file),
                // A failed recording only needs finalizing.
                TransportState::Failed => self.model.stop_recording(),
                TransportState::Starting | TransportState::Running(_) => {
                    self.dialog = Some(Dialog::Confirm(Confirm::StopRecording));
                }
            }
        }
    }

    fn live_button(&mut self, ui: &mut egui::Ui) {
        let state = self.live_transport();
        let waiting = self.model.pending("start_streaming") || self.model.pending("stop_streaming");
        let response = ui
            .add(TransportButton::new(Transport::Live, state).enabled(!waiting))
            .on_disabled_hover_text("Waiting for the engine");
        if response.clicked() {
            match state {
                // Without a target, ask for one and go live from there.
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
    }

    /// Errors in the log since the operator last opened the details.
    fn unseen_errors(&self) -> Vec<&str> {
        let unseen = self.model.log_total().saturating_sub(self.log_seen);
        let unseen = usize::try_from(unseen).unwrap_or(usize::MAX);
        let mut errors: Vec<&str> = self
            .model
            .log
            .iter()
            .rev()
            .take(unseen)
            .filter(|entry| entry.level == Level::Error)
            .map(|entry| entry.text.as_str())
            .collect();
        errors.reverse();
        errors
    }

    /// One indicator that says the worst state and why; it opens the
    /// details.
    fn health_summary(&mut self, ui: &mut egui::Ui) {
        let stale = self.model.health_age().filter(|age| *age >= HEALTH_STALE);
        let unseen = self.unseen_errors();
        let level = health_level(&self.model.health_items, stale.is_some(), unseen.len());
        let first = |wanted: Level| {
            self.model
                .health_items
                .iter()
                .find(|item| item.level == wanted)
                .map(|item| item.text.clone())
        };
        let text = match level {
            StatusLevel::Error => unseen
                .last()
                .map(|text| (*text).to_owned())
                .or_else(|| first(Level::Error))
                .unwrap_or_default(),
            StatusLevel::Warn => first(Level::Warning).unwrap_or_else(|| {
                let seconds = stale.map_or(0, |age| age.as_secs());
                format!("Health from {seconds} s ago (the engine is busy)")
            }),
            StatusLevel::Ok => "All healthy".to_owned(),
            StatusLevel::Info => "Idle".to_owned(),
        };
        let width = ui.available_width().min(HEALTH_WIDTH);
        let response =
            widgets::status_button(ui, level, &text, width).on_hover_text("Health and activity");
        let popup = egui::Popup::from_toggle_button_response(&response)
            .width(440.0)
            .align(egui::RectAlign::BOTTOM_END)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| self.health_details(ui, stale));
        if popup.is_some() {
            self.log_seen = self.model.log_total();
        }
    }

    fn health_details(&self, ui: &mut egui::Ui, stale: Option<std::time::Duration>) {
        ui.spacing_mut().item_spacing.y = size::SPACE_4;
        widgets::section_header(ui, "Health", None);
        if let Some(age) = stale {
            widgets::status_line(
                ui,
                StatusLevel::Info,
                &format!("Health from {} s ago (the engine is busy)", age.as_secs()),
            );
        }
        if self.model.health_items.is_empty() {
            widgets::status_line(
                ui,
                StatusLevel::Info,
                "Nothing runs: health shows while the preview or an output is on.",
            );
        }
        for item in &self.model.health_items {
            widgets::status_line(ui, status(item.level), &item.text);
        }
        ui.add_space(size::SPACE_8);
        widgets::section_header(ui, "Activity", Some(self.model.log.len()));
        egui::ScrollArea::vertical()
            .max_height(260.0)
            .stick_to_bottom(true)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if self.model.log.is_empty() {
                    ui.label(TextRole::Meta.text("No activity yet."));
                }
                for entry in &self.model.log {
                    widgets::status_line(ui, status(entry.level), &entry.text);
                }
            });
    }
}

#[derive(Clone, Copy)]
enum ProjectItem {
    Open,
    Save,
    SaveAs,
}

/// How a log or health level reads as a status.
pub(crate) fn status(level: Level) -> StatusLevel {
    match level {
        Level::Info => StatusLevel::Info,
        Level::Warning => StatusLevel::Warn,
        Level::Error => StatusLevel::Error,
    }
}
