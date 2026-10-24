//! The routing matrix (ADR 0017): rows are channels, columns are buses
//! (the program bus, then the aux buses), and each cell is a send with
//! its switch, level and tap. Under the channels come the aux buses'
//! assignment to the program bus, then each output's mix and the
//! recording's further tracks, in the same columns: outputs take buses,
//! never channels. Buses are added and removed here, while the pipeline
//! is idle.

use command_api::{
    BusState, MAX_AUX_BUSES, MAX_EXTRA_TRACKS, MAX_GAIN_DB, MIN_GAIN_DB, MixerState, OutputKind,
    OutputProfiles, PROGRAM_BUS_ID, SendSpec, SendTap,
};
use eframe::egui;

use crate::app::{Confirm, DesktopApp, Dialog};
use crate::ui::icons::Icon;
use crate::ui::theme::{TextRole, color, size};
use crate::ui::widgets;

use super::{commit, labelled};

/// Every column is at least this wide, so a cell keeps its place when its
/// send is switched on.
const COLUMN: f32 = 124.0;
const ROW: f32 = 20.0;
const ROW_GAP: f32 = size::SPACE_2;
/// Why the structure cannot change now (ADR 0017: structure while idle).
const IDLE_ONLY: &str = "Stop the preview and the outputs to add or remove buses";

/// What the operator changed on a send this frame.
#[derive(Default)]
struct SendChange {
    enabled: Option<bool>,
    level_db: Option<f32>,
    tap: Option<SendTap>,
}

impl SendChange {
    fn any(&self) -> bool {
        self.enabled.is_some() || self.level_db.is_some() || self.tap.is_some()
    }
}

impl DesktopApp {
    pub(crate) fn routing(&mut self, ui: &mut egui::Ui, mixer: &MixerState) {
        let idle = !mixer.running;
        let profiles = self.model.profiles.clone();
        let mut add_bus = false;
        let mut remove = None;
        // Where each group of rows begins, for the lines between them.
        let mut groups = Vec::new();
        egui::ScrollArea::both()
            .id_salt("routing")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Compact rows, so a few channels and the outputs fit the
                // dock's default height.
                ui.spacing_mut().interact_size.y = ROW;
                ui.spacing_mut().button_padding.y = size::SPACE_2;
                let grid = egui::Grid::new("routing matrix")
                    .num_columns(mixer.buses.len() + 2)
                    .min_col_width(COLUMN)
                    .min_row_height(ROW)
                    .spacing([size::SPACE_8, ROW_GAP])
                    .show(ui, |ui| {
                        ui.label(TextRole::Section.text("Channels"));
                        for bus in &mixer.buses {
                            if column_header(ui, bus, idle) {
                                remove = Some((bus.id.clone(), bus.name.clone()));
                            }
                        }
                        let room = mixer.buses.len() <= MAX_AUX_BUSES;
                        add_bus = ui
                            .add_enabled_ui(idle && room, |ui| {
                                widgets::icon_button(ui, Icon::Plus, "Add a bus")
                            })
                            .inner
                            .on_disabled_hover_text(if room {
                                IDLE_ONLY.to_owned()
                            } else {
                                format!("At most {MAX_AUX_BUSES} buses besides the program bus")
                            })
                            .clicked();
                        ui.end_row();

                        for channel in &mixer.channels {
                            let spec = &channel.spec;
                            let paths = mixer.program_paths(spec);
                            let header = row_header(ui, &spec.name);
                            if groups.is_empty() {
                                groups.push(header.rect.top());
                            }
                            if paths.len() > 1 {
                                let ways: Vec<String> =
                                    paths.iter().map(ToString::to_string).collect();
                                warning_mark(
                                    ui,
                                    header.rect,
                                    &format!(
                                        "Reaches the program bus {} times: {}",
                                        paths.len(),
                                        ways.join(", ")
                                    ),
                                );
                            }
                            for (bus, send) in mixer.buses.iter().zip(&spec.sends) {
                                // Ids by channel and bus, so a field being dragged keeps
                                // its state when other cells change.
                                let change = ui
                                    .push_id((&spec.id, &bus.id), |ui| {
                                        send_cell(ui, &spec.name, &bus.name, send)
                                    })
                                    .inner;
                                if change.any() {
                                    self.model.set_send(
                                        &spec.id,
                                        &bus.id,
                                        change.enabled,
                                        change.level_db,
                                        change.tap,
                                    );
                                }
                            }
                            ui.end_row();
                        }
                        if mixer.channels.is_empty() {
                            groups.push(row_header(ui, "No channels").rect.top());
                            ui.end_row();
                        }

                        groups.push(row_header(ui, "To program").rect.top());
                        for bus in &mixer.buses {
                            if bus.id == PROGRAM_BUS_ID {
                                ui.label("");
                            } else if let Some(assign_db) =
                                ui.push_id(&bus.id, |ui| assign_cell(ui, bus)).inner
                            {
                                self.model.update_bus(&bus.id, None, None, Some(assign_db));
                            }
                        }
                        ui.end_row();

                        if let Some(profiles) = &profiles {
                            groups.push(ui.cursor().top());
                            self.output_rows(ui, mixer, profiles);
                        }
                    });
                // Lines between the groups (the first one is the header's).
                let x = grid.response.rect.x_range();
                for &y in groups.iter() {
                    let y = (y - ROW_GAP / 2.0).round() - 0.5;
                    ui.painter()
                        .hline(x, y, egui::Stroke::new(size::STROKE, color::LINE_SUBTLE));
                }
            });
        if add_bus {
            self.dialog = Some(Dialog::AddBus {
                name: String::new(),
                assigned: false,
            });
        }
        if let Some((id, name)) = remove {
            self.dialog = Some(Dialog::Confirm(Confirm::RemoveBus { id, name }));
        }
    }

    /// Each output's mix as a choice in the bus columns; in the
    /// recording's row, each other bus can be a further track.
    fn output_rows(&mut self, ui: &mut egui::Ui, mixer: &MixerState, profiles: &OutputProfiles) {
        let running = |output: OutputKind| {
            mixer
                .outputs
                .iter()
                .any(|mix| mix.output == output && mix.running)
        };
        for (output, label, mix) in [
            (
                OutputKind::Recording,
                "Record",
                profiles.recording.mix.as_str(),
            ),
            (OutputKind::Stream, "Stream", profiles.stream.mix.as_str()),
            (
                OutputKind::Monitor,
                "Monitor",
                profiles.monitor.mix.as_str(),
            ),
        ] {
            let header = ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = size::SPACE_6;
                ui.label(TextRole::Body.text(label));
                if output == OutputKind::Monitor {
                    let mut on = profiles.monitor.enabled;
                    let device = profiles
                        .monitor
                        .device
                        .as_deref()
                        .unwrap_or("the system's default output");
                    if widgets::toggle(ui, &mut on, "Monitor on")
                        .on_hover_text(format!("Plays its mix on {device}"))
                        .changed()
                    {
                        self.model.set_monitor(on);
                    }
                }
                if running(output) {
                    let (dot, _) =
                        ui.allocate_exact_size(egui::vec2(8.0, ROW), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 3.5, color::TALLY_PROGRAM);
                }
            });
            let recording = output == OutputKind::Recording;
            if recording && self.model.recording.is_some() {
                header.response.on_hover_text(
                    "A running recording keeps its tracks; a change applies to the next one.",
                );
            }
            for bus in &mixer.buses {
                let selected = bus.id == mix;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = size::SPACE_6;
                    let choice =
                        widgets::radio(ui, selected, &format!("{label} takes {}", bus.name));
                    if choice.clicked() && !selected {
                        self.model.set_output_mix(output, &bus.id);
                    }
                    if recording {
                        self.track_chip(ui, bus, selected, &profiles.recording.tracks);
                    }
                });
            }
            ui.end_row();
        }
    }

    /// Whether a bus is one of the recording's further tracks, numbered
    /// after the mix (track 1) in the order they were added.
    fn track_chip(&mut self, ui: &mut egui::Ui, bus: &BusState, mix: bool, tracks: &[String]) {
        if mix {
            ui.label(TextRole::Numeric.text("T1").color(color::TEXT_TERTIARY))
                .on_hover_text("The mix is the recording's first track");
            return;
        }
        let index = tracks.iter().position(|track| *track == bus.id);
        let full = tracks.len() >= MAX_EXTRA_TRACKS;
        let (text, label) = match index {
            Some(index) => (
                format!("T{}", index + 2),
                format!(
                    "{} is track {}; click to stop recording it as a track",
                    bus.name,
                    index + 2
                ),
            ),
            None => (
                "track".to_owned(),
                format!("Record {} as a further track", bus.name),
            ),
        };
        let clicked = ui
            .add_enabled_ui(index.is_some() || !full, |ui| {
                widgets::chip(ui, &text, index.is_some(), &label)
            })
            .inner
            .on_disabled_hover_text(format!("At most {MAX_EXTRA_TRACKS} tracks besides the mix"))
            .clicked();
        if clicked {
            let mut tracks = tracks.to_vec();
            match index {
                Some(index) => {
                    tracks.remove(index);
                }
                None => tracks.push(bus.id.clone()),
            }
            self.model.set_recording_tracks(&tracks);
        }
    }
}

/// A bus's name over its column, with removal for aux buses. Returns
/// whether removal was asked for.
fn column_header(ui: &mut egui::Ui, bus: &BusState, idle: bool) -> bool {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = size::SPACE_4;
        ui.add(
            egui::Label::new(TextRole::Label.text(&bus.name).color(color::TEXT_PRIMARY)).truncate(),
        );
        if bus.id == PROGRAM_BUS_ID {
            return false;
        }
        ui.add_enabled_ui(idle, |ui| {
            widgets::icon_button(ui, Icon::Trash, &format!("Remove bus {}…", bus.name))
        })
        .inner
        .on_disabled_hover_text(IDLE_ONLY)
        .clicked()
    })
    .inner
}

fn row_header(ui: &mut egui::Ui, name: &str) -> egui::Response {
    ui.add(egui::Label::new(TextRole::Body.text(name)).truncate())
}

/// A warning icon after a row's name, with the reason as its tooltip.
fn warning_mark(ui: &mut egui::Ui, name: egui::Rect, reason: &str) {
    let rect = egui::Rect::from_center_size(
        egui::pos2(name.right() + size::SPACE_4 + 7.0, name.center().y),
        egui::Vec2::splat(14.0),
    );
    Icon::Warning.paint(ui.painter(), rect, color::STATE_WARN);
    ui.interact(
        rect,
        ui.id().with(("double routing", reason)),
        egui::Sense::hover(),
    )
    .on_hover_text(reason);
}

/// A send's level, or a bus's into the program bus, in dB, with the
/// engine's limits. A value set elsewhere stays as it is until edited.
fn level_field(value: &mut f32) -> egui::DragValue<'_> {
    egui::DragValue::new(value)
        .range(MIN_GAIN_DB..=MAX_GAIN_DB)
        .clamp_existing_to_range(false)
        .speed(0.1)
        .fixed_decimals(1)
        .update_while_editing(false)
}

/// A send: its switch, and while it is on, its level and tap.
fn send_cell(ui: &mut egui::Ui, channel: &str, bus: &str, send: &SendSpec) -> SendChange {
    let mut change = SendChange::default();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = size::SPACE_4;
        let mut on = send.enabled;
        if widgets::toggle(ui, &mut on, &format!("Send {channel} to {bus}")).changed() {
            change.enabled = Some(on);
        }
        if !send.enabled {
            return;
        }
        let mut level = send.level_db;
        let field = ui.add(level_field(&mut level));
        let field = labelled(ui, field, &format!("Level of {channel} to {bus} (dB)"));
        change.level_db = commit(ui, &field, level);
        let pre = send.tap == SendTap::PreFader;
        let (text, label) = if pre {
            (
                "pre",
                format!("{channel} to {bus}: before the fader; click for after it"),
            )
        } else {
            (
                "post",
                format!("{channel} to {bus}: after the fader; click for before it"),
            )
        };
        if widgets::chip(ui, text, pre, &label).clicked() {
            change.tap = Some(if pre {
                SendTap::PostFader
            } else {
                SendTap::PreFader
            });
        }
    });
    change
}

/// An aux bus's assignment: whether it feeds the program bus, and at what
/// level. Returns the new assignment (`Some(None)`: a separate mix).
fn assign_cell(ui: &mut egui::Ui, bus: &BusState) -> Option<Option<f32>> {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = size::SPACE_4;
        let mut on = bus.assign_db.is_some();
        let label = format!("Feed {} to the program bus", bus.name);
        if widgets::toggle(ui, &mut on, &label).changed() {
            return Some(on.then_some(0.0));
        }
        let mut level = bus.assign_db?;
        let field = ui.add(level_field(&mut level));
        let field = labelled(
            ui,
            field,
            &format!("Level of {} into the program bus (dB)", bus.name),
        );
        commit(ui, &field, level).map(Some)
    })
    .inner
}
