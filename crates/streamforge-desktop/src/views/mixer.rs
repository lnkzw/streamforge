//! The mixer dock: a strip per channel and the program bus set apart on
//! the right. A strip has its gain, a meter with peak hold and clip latch,
//! a fader, mute and its name. Buses, sends and the routing matrix join in
//! the console (roadmap M12, next step).

use eframe::egui;

use crate::app::DesktopApp;
use crate::ui::theme::{TextRole, color, size};
use crate::ui::widgets::{self, METER_FLOOR_DB};
use crate::workspace::Ballistics;

use super::{committed, right_to_left};

/// The gain range of the faders, in dB.
const GAIN_RANGE: std::ops::RangeInclusive<f32> = METER_FLOOR_DB..=12.0;
const STRIP_WIDTH: f32 = 72.0;
const METER_WIDTH: f32 = 8.0;

/// What the operator changed on a strip this frame.
#[derive(Default)]
struct StripChange {
    gain: Option<f32>,
    mute: Option<bool>,
}

impl DesktopApp {
    pub(crate) fn mixer_dock(&mut self, ui: &mut egui::Ui) {
        let Some(mixer) = self.model.mixer.clone() else {
            widgets::section_header(ui, "Mixer", None);
            ui.spinner();
            return;
        };
        let actions = widgets::section_header(ui, "Mixer", Some(mixer.channels.len()));
        if !mixer.running {
            right_to_left(ui, actions, |ui| {
                ui.label(TextRole::Meta.text("Levels show while the preview or an output runs"));
            });
        }
        // Gain readout, meter and fader, mute, name.
        let fixed = 18.0 + 22.0 + 20.0 + 3.0 * size::SPACE_4;
        let height = (ui.available_height() - fixed).max(48.0);
        egui::ScrollArea::horizontal()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = size::SPACE_4;
                    ui.vertical(|ui| {
                        ui.add_space(18.0 + size::SPACE_4);
                        widgets::meter_scale(ui, height);
                    });
                    for channel in &mixer.channels {
                        let spec = &channel.spec;
                        let meter = self.meters.entry(spec.id.clone()).or_default();
                        let change = strip(
                            ui,
                            &spec.id,
                            &spec.name,
                            spec.gain_db,
                            Some(spec.muted),
                            meter,
                            height,
                        );
                        if change.gain.is_some() || change.mute.is_some() {
                            self.model.update_channel(
                                &spec.id,
                                change.gain.unwrap_or(spec.gain_db),
                                change.mute.unwrap_or(spec.muted),
                            );
                        }
                    }
                    if mixer.channels.is_empty() {
                        ui.vertical(|ui| {
                            ui.add_space(height / 2.0);
                            ui.label(TextRole::Meta.text("No channels"));
                        });
                    }
                    // The program bus, set apart on the right.
                    if let Some(program) = mixer.buses.first() {
                        ui.add_space(size::SPACE_12);
                        let (line, _) = ui.allocate_exact_size(
                            egui::vec2(1.0, ui.available_height()),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(line, 0.0, color::LINE_SUBTLE);
                        ui.add_space(size::SPACE_8);
                        let change = strip(
                            ui,
                            &program.id,
                            &program.name,
                            program.gain_db,
                            None,
                            &mut self.program_meter,
                            height,
                        );
                        if let Some(gain) = change.gain {
                            self.model.update_bus(&program.id, gain);
                        }
                    }
                });
            });
    }
}

/// One strip. `muted` is `None` for a strip without mute (the program).
fn strip(
    ui: &mut egui::Ui,
    id: &str,
    name: &str,
    gain_db: f32,
    muted: Option<bool>,
    meter: &mut Ballistics,
    height: f32,
) -> StripChange {
    let mut change = StripChange::default();
    ui.allocate_ui_with_layout(
        egui::vec2(STRIP_WIDTH, ui.available_height()),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            ui.set_width(STRIP_WIDTH);
            ui.spacing_mut().item_spacing.y = size::SPACE_4;
            let mut gain = gain_db;
            ui.label(
                TextRole::Numeric
                    .text(gain_label(gain))
                    .color(color::TEXT_SECONDARY),
            );
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = size::SPACE_6;
                let meter_response = widgets::meter(
                    ui,
                    egui::vec2(METER_WIDTH, height),
                    meter.level,
                    meter.peak,
                    meter.clipped,
                );
                // The clip latch stays until the operator resets it.
                let latch = ui.interact(
                    meter_response.rect,
                    egui::Id::new(("clip latch", id)),
                    egui::Sense::click(),
                );
                if meter.clipped && latch.on_hover_text("Clipped: click to reset").clicked() {
                    meter.reset_clip();
                }
                let fader = widgets::fader(ui, height, &mut gain, GAIN_RANGE, name);
                if committed(&fader) {
                    change.gain = Some(gain);
                }
            });
            match muted {
                Some(muted) => {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = size::SPACE_4;
                        let mut on = muted;
                        if widgets::toggle(ui, &mut on, &format!("Mute {name}")).changed() {
                            change.mute = Some(on);
                        }
                        let label = if muted { "Muted" } else { "Mute" };
                        let text = TextRole::Meta.text(label);
                        ui.label(if muted {
                            text.color(color::STATE_WARN)
                        } else {
                            text
                        });
                    });
                }
                None => {
                    ui.add_space(20.0);
                }
            }
            ui.add(
                egui::Label::new(TextRole::Label.text(name).color(color::TEXT_PRIMARY)).truncate(),
            );
        },
    );
    change
}

/// The gain as the strip shows it: `−6.0`, `0.0`, `+3.0`, `−∞` below the
/// fader's range.
fn gain_label(gain_db: f32) -> String {
    if gain_db <= METER_FLOOR_DB - 0.05 {
        "−∞".to_owned()
    } else if gain_db.abs() < 0.05 {
        "0.0".to_owned()
    } else if gain_db > 0.0 {
        format!("+{gain_db:.1}")
    } else {
        format!("−{:.1}", -gain_db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_reads_with_a_sign_and_a_true_minus() {
        assert_eq!(gain_label(0.0), "0.0");
        assert_eq!(gain_label(-0.01), "0.0");
        assert_eq!(gain_label(-6.0), "−6.0");
        assert_eq!(gain_label(3.25), "+3.2");
        assert_eq!(gain_label(-60.0), "−60.0");
        assert_eq!(gain_label(-96.0), "−∞");
    }
}
