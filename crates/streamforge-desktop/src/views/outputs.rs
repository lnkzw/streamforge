//! The outputs dock: per output its state, its target and its numbers,
//! and a settings action for the target. The transport in the top bar
//! starts and stops them.

use command_api::{OutputHealth, StreamKey, StreamState, StreamTarget};
use eframe::egui;

use crate::app::DesktopApp;
use crate::ui::icons::Icon;
use crate::ui::theme::{TextRole, color, size};
use crate::ui::widgets::{self, BadgeKind, StatusLevel, TransportState};

impl DesktopApp {
    pub(crate) fn outputs_dock(&mut self, ui: &mut egui::Ui) {
        widgets::section_header(ui, "Outputs", Some(2));
        let health = self.model.health.as_ref();

        // Recording.
        let state = self.record_transport();
        let output = health.and_then(|health| health.recording.as_ref());
        // The file name; the full path is in the tooltip and the log.
        let target = match (&self.model.recording, output) {
            (Some(_), Some(output)) => output.target.clone(),
            (Some(started), None) => started.path.clone(),
            (None, _) => self.record_file.clone(),
        };
        let file_name = std::path::Path::new(&target)
            .file_name()
            .map_or(target.clone(), |name| name.to_string_lossy().into_owned());
        let badge = match state {
            TransportState::Idle => (BadgeKind::Neutral, "off"),
            TransportState::Starting => (BadgeKind::Warn, "starting"),
            TransportState::Running(_) => (BadgeKind::Tally, "rec"),
            TransportState::Failed => (BadgeKind::Error, "failed"),
        };
        let record_settings = output_row(
            ui,
            Icon::Record,
            "Record",
            badge,
            (&file_name, &target),
            "Recording settings",
        );
        output_numbers(
            ui,
            self.model.recording.is_some().then_some(output).flatten(),
        );
        ui.add_space(size::SPACE_6);
        widgets::divider(ui);
        ui.add_space(size::SPACE_6);

        // Stream.
        let state = self.live_transport();
        let output = health.and_then(|health| health.streaming.as_ref());
        let target = match (&self.model.streaming, output) {
            (Some(_), Some(output)) => output.target.clone(),
            (Some(started), None) => started.target.clone(),
            (None, _) => stream_target_label(&self.stream_url, &self.stream_key),
        };
        let network = output.and_then(|output| output.state);
        let badge = match (state, network) {
            (TransportState::Idle, _) => (BadgeKind::Neutral, "off"),
            (TransportState::Starting, Some(StreamState::Reconnecting)) => {
                (BadgeKind::Warn, "reconnecting")
            }
            (TransportState::Starting, _) => (BadgeKind::Warn, "connecting"),
            (TransportState::Running(_), _) => (BadgeKind::Tally, "live"),
            (TransportState::Failed, _) => (BadgeKind::Error, "failed"),
        };
        let stream_settings = output_row(
            ui,
            Icon::Live,
            "Stream",
            badge,
            (&target, &target),
            "Stream settings",
        );
        let streaming = self.model.streaming.is_some().then_some(output).flatten();
        output_numbers(ui, streaming);
        if let Some(output) = streaming
            && output.counters.reconnect_attempts > 0
        {
            widgets::status_line(
                ui,
                StatusLevel::Warn,
                &format!(
                    "{} reconnect attempt(s)",
                    output.counters.reconnect_attempts
                ),
            );
        }
        if record_settings {
            self.open_record_settings();
        }
        if stream_settings {
            self.open_stream_settings(false);
        }
    }
}

/// The stream target as the outputs dock shows it before going live:
/// redacted by the engine's rules, since a URL can carry a secret and the
/// screen may be on stream.
fn stream_target_label(url: &str, key: &str) -> String {
    let url = url.trim();
    if url.is_empty() {
        return "No target set".to_owned();
    }
    match StreamTarget::new(url, StreamKey::new(key)) {
        Ok(target) => target.redacted(),
        Err(_) => "Invalid URL: check the stream settings".to_owned(),
    }
}

/// An output's first line: icon, name, state badge, target, settings.
/// Returns whether the settings were asked for.
fn output_row(
    ui: &mut egui::Ui,
    icon: Icon,
    name: &str,
    (kind, state): (BadgeKind, &str),
    (target, full_target): (&str, &str),
    settings: &str,
) -> bool {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = size::SPACE_8;
        let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(size::ICON), egui::Sense::hover());
        let tint = if kind == BadgeKind::Tally {
            color::TALLY_PROGRAM
        } else {
            color::TEXT_TERTIARY
        };
        icon.paint(ui.painter(), rect, tint);
        ui.label(TextRole::Body.text(name));
        widgets::badge(ui, kind, state);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let clicked = widgets::icon_button(ui, Icon::Gear, settings).clicked();
            ui.add(
                egui::Label::new(TextRole::Meta.text(target).color(color::TEXT_SECONDARY))
                    .truncate(),
            )
            .on_hover_text(full_target);
            clicked
        })
        .inner
    })
    .inner
}

/// An output's numbers while it runs: frames, drops, queue and encoder;
/// drops show as a warning, since media was lost.
fn output_numbers(ui: &mut egui::Ui, output: Option<&OutputHealth>) {
    let Some(output) = output else {
        return;
    };
    let counters = &output.counters;
    let kind = if output.video_encoder.hardware {
        "hardware"
    } else {
        "software"
    };
    ui.label(
        TextRole::Numeric
            .text(format!(
                "{} frames · queue {} · {} ({kind})",
                counters.frames_encoded, counters.queue_depth, output.video_encoder.encoder
            ))
            .color(color::TEXT_TERTIARY),
    );
    if counters.frames_dropped > 0 || counters.blocks_dropped > 0 {
        widgets::status_line(
            ui,
            StatusLevel::Warn,
            &format!(
                "{} frames and {} audio blocks dropped",
                counters.frames_dropped, counters.blocks_dropped
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_target_never_shows_a_secret() {
        assert_eq!(stream_target_label("  ", ""), "No target set");
        assert_eq!(
            stream_target_label("rtmp://live.example.com/app", "secret"),
            "rtmp://live.example.com/app/<stream key>"
        );
        // A key pasted into the URL is hidden too.
        let label = stream_target_label("rtmp://live.example.com/app/secret?token=x", "");
        assert!(
            !label.contains("secret") && !label.contains("token"),
            "{label}"
        );
        assert!(stream_target_label("http://example.com", "").starts_with("Invalid URL"));
    }
}
