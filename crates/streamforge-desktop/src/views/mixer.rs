//! The mixer dock, in two views (ADR 0017). The console has a strip per
//! channel, then one per aux bus, then the program bus set apart on the
//! right. A strip has its gain, a meter with peak hold and clip latch, a
//! fader, mute, its name and a line on its routing. The routing matrix is
//! the other view ([`super::routing`]).

use command_api::{BusState, ChannelState, MixerState};
use eframe::egui;

use crate::app::{DesktopApp, MixerView};
use crate::ui::theme::{TextRole, color, size};
use crate::ui::widgets::{self, METER_FLOOR_DB, StatusLevel};
use crate::workspace::Ballistics;

use super::{bus_short_name, commit, right_to_left};

/// The gain range of the faders, in dB.
const GAIN_RANGE: std::ops::RangeInclusive<f32> = METER_FLOOR_DB..=12.0;
const STRIP_WIDTH: f32 = 72.0;
const METER_WIDTH: f32 = 8.0;
/// Gain readout, mute row, name and note, with the spacing between them.
const STRIP_FIXED: f32 = 18.0 + 22.0 + 20.0 + 16.0 + 4.0 * size::SPACE_4;

/// What a strip shows.
struct Strip<'a> {
    id: &'a str,
    name: &'a str,
    gain_db: f32,
    muted: bool,
    note: Note,
}

/// The line under a strip's name: where a channel goes, or what a bus
/// feeds. The tooltip says it in full.
struct Note {
    text: String,
    warn: bool,
    tooltip: String,
}

impl Note {
    fn plain(text: impl Into<String>, tooltip: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            warn: false,
            tooltip: tooltip.into(),
        }
    }
}

/// What the operator changed on a strip this frame.
#[derive(Default)]
struct StripChange {
    gain: Option<f32>,
    mute: Option<bool>,
}

impl StripChange {
    fn any(&self) -> bool {
        self.gain.is_some() || self.mute.is_some()
    }
}

impl DesktopApp {
    pub(crate) fn mixer_dock(&mut self, ui: &mut egui::Ui) {
        let Some(mixer) = self.model.mixer.clone() else {
            widgets::section_header(ui, "Mixer", None);
            widgets::loading(ui, "Loading the mixer…");
            return;
        };
        let actions = widgets::section_header(ui, "Mixer", Some(mixer.channels.len()));
        let view = self.mixer_view;
        let mut switch = None;
        right_to_left(ui, actions, |ui| {
            let selected = match view {
                MixerView::Console => 0,
                MixerView::Routing => 1,
            };
            switch = widgets::tabs(ui, selected, &["Console", "Routing"]).map(|index| {
                if index == 0 {
                    MixerView::Console
                } else {
                    MixerView::Routing
                }
            });
            if !mixer.warnings.is_empty() {
                let text = match mixer.warnings.len() {
                    1 => "1 routing warning".to_owned(),
                    count => format!("{count} routing warnings"),
                };
                // The details are in the tooltip; a click shows the matrix.
                if widgets::status_button(ui, StatusLevel::Warn, &text, 200.0)
                    .on_hover_text(mixer.warnings.join("\n"))
                    .clicked()
                {
                    switch = Some(MixerView::Routing);
                }
            }
            if !mixer.running && view == MixerView::Console {
                ui.add(
                    egui::Label::new(
                        TextRole::Meta.text("Levels show while the preview or an output runs"),
                    )
                    .truncate(),
                );
            }
        });
        if let Some(view) = switch {
            self.mixer_view = view;
        }
        match self.mixer_view {
            MixerView::Console => self.console(ui, &mixer),
            MixerView::Routing => self.routing(ui, &mixer),
        }
    }

    /// Channels, then aux buses, then the program bus, in zones.
    fn console(&mut self, ui: &mut egui::Ui, mixer: &MixerState) {
        let height = (ui.available_height() - STRIP_FIXED).max(48.0);
        egui::ScrollArea::horizontal()
            .id_salt("console")
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
                            Strip {
                                id: &spec.id,
                                name: &spec.name,
                                gain_db: spec.gain_db,
                                muted: spec.muted,
                                note: channel_note(mixer, channel),
                            },
                            meter,
                            height,
                        );
                        if change.any() {
                            self.model
                                .update_channel(&spec.id, change.gain, change.mute);
                        }
                    }
                    if mixer.channels.is_empty() {
                        ui.vertical(|ui| {
                            ui.add_space(height / 2.0);
                            ui.label(TextRole::Meta.text("No channels"));
                        });
                    }
                    let Some((program, aux)) = mixer.buses.split_first() else {
                        return;
                    };
                    if !aux.is_empty() {
                        zone_divider(ui, color::LINE_SUBTLE);
                    }
                    for bus in aux {
                        let meter = self.meters.entry(bus.id.clone()).or_default();
                        let change = strip(
                            ui,
                            Strip {
                                id: &bus.id,
                                name: &bus.name,
                                gain_db: bus.gain_db,
                                muted: bus.muted,
                                note: bus_note(bus),
                            },
                            meter,
                            height,
                        );
                        if change.any() {
                            self.model
                                .update_bus(&bus.id, change.gain, change.mute, None);
                        }
                    }
                    // The program bus, set apart on the right.
                    ui.add_space(size::SPACE_8);
                    zone_divider(ui, color::LINE_STRONG);
                    let meter = self.meters.entry(program.id.clone()).or_default();
                    let change = strip(
                        ui,
                        Strip {
                            id: &program.id,
                            name: &program.name,
                            gain_db: program.gain_db,
                            muted: program.muted,
                            note: Note::plain("", ""),
                        },
                        meter,
                        height,
                    );
                    if change.any() {
                        self.model
                            .update_bus(&program.id, change.gain, change.mute, None);
                    }
                });
            });
    }
}

/// A vertical line between the console's zones.
fn zone_divider(ui: &mut egui::Ui, line: egui::Color32) {
    ui.add_space(size::SPACE_8);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(1.0, ui.available_height()), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, line);
    ui.add_space(size::SPACE_8);
}

/// Where a channel goes: the buses it sends to, marked when it reaches
/// the program bus more than once (allowed, but rarely meant).
fn channel_note(mixer: &MixerState, channel: &ChannelState) -> Note {
    let paths = mixer.program_paths(&channel.spec);
    if paths.len() > 1 {
        let ways: Vec<String> = paths.iter().map(ToString::to_string).collect();
        return Note {
            text: format!("PGM ×{}", paths.len()),
            warn: true,
            tooltip: format!(
                "Reaches the program bus {} times: {}. Turn off a send in Routing if that \
                 is not meant.",
                paths.len(),
                ways.join(", ")
            ),
        };
    }
    let targets: Vec<&BusState> = mixer
        .buses
        .iter()
        .zip(&channel.spec.sends)
        .filter(|(_, send)| send.enabled)
        .map(|(bus, _)| bus)
        .collect();
    if targets.is_empty() {
        return Note::plain("not routed", "It sends to no bus, so no output hears it.");
    }
    let short: Vec<&str> = targets.iter().map(|bus| bus_short_name(bus)).collect();
    let full: Vec<&str> = targets.iter().map(|bus| bus.name.as_str()).collect();
    Note::plain(
        format!("→ {}", short.join(" · ")),
        format!("Sends to {}", full.join(", ")),
    )
}

/// What an aux bus feeds: the program bus at a level, or nothing (a
/// separate mix, e.g. for the monitor).
fn bus_note(bus: &BusState) -> Note {
    match bus.assign_db {
        Some(level) => Note::plain(
            format!("→ PGM {}", gain_label(level)),
            format!("Feeds the program bus at {} dB", gain_label(level)),
        ),
        None => Note::plain(
            "separate",
            "A separate mix: it does not feed the program bus.",
        ),
    }
}

fn strip(ui: &mut egui::Ui, strip: Strip, meter: &mut Ballistics, height: f32) -> StripChange {
    let Strip {
        id,
        name,
        gain_db,
        muted,
        note,
    } = strip;
    let mut change = StripChange::default();
    // Ids by channel or bus, so a fader being dragged keeps its state when
    // strips come or go.
    ui.push_id(id, |ui| {
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
                    // The clip latch stays until the operator resets it: a
                    // click on the meter, or Space on it once Tab reaches
                    // it (only while it is latched).
                    if meter.clipped {
                        let latch = ui.interact(
                            meter_response.rect,
                            egui::Id::new(("clip latch", id)),
                            egui::Sense::click(),
                        );
                        latch.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                format!("Reset the clip of {name}"),
                            )
                        });
                        if latch.has_focus() {
                            widgets::focus_ring(ui.painter(), meter_response.rect, 0);
                        }
                        if latch.on_hover_text("Clipped: click to reset").clicked() {
                            meter.reset_clip();
                        }
                    }
                    let fader = widgets::fader(ui, height, &mut gain, GAIN_RANGE, name);
                    change.gain = commit(ui, &fader, gain);
                });
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
                ui.add(
                    egui::Label::new(TextRole::Label.text(name).color(color::TEXT_PRIMARY))
                        .truncate(),
                );
                let text = TextRole::Meta.text(&note.text);
                let note_label = ui.add(
                    egui::Label::new(if note.warn {
                        text.color(color::STATE_WARN)
                    } else {
                        text
                    })
                    .truncate(),
                );
                if !note.tooltip.is_empty() {
                    note_label.on_hover_text(note.tooltip);
                }
            },
        );
    });
    change
}

/// The gain as the strip shows it: `−6.0`, `0.0`, `+3.0`, `−∞` below the
/// fader's range.
pub(super) fn gain_label(gain_db: f32) -> String {
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
    use command_api::{ChannelInput, ChannelSpec, PROGRAM_BUS_ID, SendSpec};

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

    fn bus(id: &str, name: &str, assign_db: Option<f32>) -> BusState {
        BusState {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: if id == PROGRAM_BUS_ID {
                "program"
            } else {
                "aux"
            },
            gain_db: 0.0,
            muted: false,
            assign_db,
            peak_dbfs: None,
        }
    }

    fn mixer(sends: [bool; 3]) -> MixerState {
        let buses = vec![
            bus(PROGRAM_BUS_ID, "Program", None),
            bus("bus_voice", "Voice", Some(-3.0)),
            bus("bus_monitor", "Monitor", None),
        ];
        let sends = buses
            .iter()
            .zip(sends)
            .map(|(bus, enabled)| SendSpec {
                enabled,
                ..SendSpec::off(&bus.id)
            })
            .collect();
        MixerState {
            running: false,
            channels: vec![ChannelState {
                spec: ChannelSpec {
                    id: "ch_mic".to_owned(),
                    name: "Mic".to_owned(),
                    input: ChannelInput::Device { device: None },
                    gain_db: 0.0,
                    muted: false,
                    balance: 0.0,
                    sends,
                },
                peak_dbfs: None,
            }],
            buses,
            outputs: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn a_channel_note_names_its_buses_and_marks_double_routing() {
        let routed = mixer([true, false, true]);
        let note = channel_note(&routed, &routed.channels[0]);
        assert_eq!(note.text, "→ PGM · Monitor");
        assert!(!note.warn);

        let twice = mixer([true, true, false]);
        let note = channel_note(&twice, &twice.channels[0]);
        assert_eq!(note.text, "PGM ×2");
        assert!(note.warn);
        assert!(
            note.tooltip.contains("directly, through `Voice`"),
            "{}",
            note.tooltip
        );

        let silent = mixer([false, false, false]);
        assert_eq!(
            channel_note(&silent, &silent.channels[0]).text,
            "not routed"
        );
    }

    #[test]
    fn a_bus_note_says_what_it_feeds() {
        assert_eq!(
            bus_note(&bus("bus_voice", "Voice", Some(-3.0))).text,
            "→ PGM −3.0"
        );
        assert_eq!(bus_note(&bus("bus_mon", "Monitor", None)).text, "separate");
    }
}
