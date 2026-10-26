//! The routing matrix (ADR 0017): rows are channels, columns are buses
//! (the program bus, then the aux buses), and each cell is a send with
//! its switch, level and tap. Each channel's input comes before the
//! buses. Under the channels come the aux buses' assignment to the program
//! bus, then each output's mix and the recording's further tracks, in the
//! same columns: outputs take buses, never channels. Channels and buses
//! are added and removed here, and inputs chosen, while the pipeline is
//! idle.

use command_api::{
    BusState, MAX_AUX_BUSES, MAX_CHANNELS, MAX_EXTRA_TRACKS, MAX_GAIN_DB, MIN_GAIN_DB, MixerState,
    OutputKind, OutputProfiles, PROGRAM_BUS_ID, SendSpec, SendTap,
};
use eframe::egui;

use crate::app::{Confirm, DesktopApp, Dialog};
use crate::model::InputForm;
use crate::ui::icons::Icon;
use crate::ui::theme::{TextRole, color, size};
use crate::ui::widgets;
use crate::workspace::{self, InputLabel};

use super::{commit, labelled};

/// Every column is at least this wide, so a cell keeps its place when its
/// send is switched on.
const COLUMN: f32 = 124.0;
/// Device names are long; the field cuts them and its tooltip has all.
const INPUT_COLUMN: f32 = 160.0;
const ROW: f32 = 20.0;
const ROW_GAP: f32 = size::SPACE_2;
/// Why the structure cannot change now (ADR 0017: structure while idle).
const IDLE_BUSES: &str = "Stop the preview and the outputs to add or remove buses";
const IDLE_CHANNELS: &str = "Stop the preview and the outputs to add or remove channels";
const IDLE_INPUT: &str = "Stop the preview and the outputs to choose another input";

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
        // Device names and desktop audio need the engine's list; it is
        // asked for once here, and again when an input is chosen.
        if self.model.audio_inputs.is_none() {
            self.model.find_audio_inputs();
        }
        let inputs = match &self.model.audio_inputs {
            Some(Ok(inputs)) => Some(inputs.as_slice()),
            _ => None,
        };
        let labels: Vec<InputLabel> = mixer
            .channels
            .iter()
            .map(|channel| workspace::input_label(&channel.spec.input, inputs))
            .collect();
        let mut add_channel = false;
        let mut choose_input = None;
        let mut remove_channel = None;
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
                    .num_columns(mixer.buses.len() + 3)
                    .min_col_width(COLUMN)
                    .min_row_height(ROW)
                    .spacing([size::SPACE_8, ROW_GAP])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = size::SPACE_4;
                            ui.label(TextRole::Section.text("Channels"));
                            let room = mixer.channels.len() < MAX_CHANNELS;
                            add_channel = ui
                                .add_enabled_ui(idle && room, |ui| {
                                    widgets::icon_button(ui, Icon::Plus, "Add a channel")
                                })
                                .inner
                                .on_disabled_hover_text(if room {
                                    IDLE_CHANNELS.to_owned()
                                } else {
                                    format!("At most {MAX_CHANNELS} channels")
                                })
                                .clicked();
                        });
                        ui.label(TextRole::Section.text("Input"));
                        for bus in &mixer.buses {
                            let blocked = if !idle {
                                Some(IDLE_BUSES.to_owned())
                            } else {
                                profiles
                                    .as_ref()
                                    .and_then(|profiles| in_use(profiles, &bus.id))
                            };
                            if column_header(ui, bus, blocked) {
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
                                IDLE_BUSES.to_owned()
                            } else {
                                format!("At most {MAX_AUX_BUSES} buses besides the program bus")
                            })
                            .clicked();
                        ui.end_row();

                        for (channel, label) in mixer.channels.iter().zip(&labels) {
                            let spec = &channel.spec;
                            let paths = mixer.program_paths(spec);
                            let warning = (paths.len() > 1).then(|| {
                                let ways: Vec<String> =
                                    paths.iter().map(ToString::to_string).collect();
                                format!(
                                    "Reaches the program bus {} times: {}",
                                    paths.len(),
                                    ways.join(", ")
                                )
                            });
                            let blocked = (!idle).then_some(IDLE_CHANNELS);
                            let (header, removed) = ui
                                .push_id(&spec.id, |ui| {
                                    channel_header(ui, &spec.name, warning.as_deref(), blocked)
                                })
                                .inner;
                            let chosen = ui
                                .push_id((&spec.id, "input"), |ui| {
                                    input_cell(ui, &spec.name, label, idle)
                                })
                                .inner;
                            if groups.is_empty() {
                                groups.push(header.top());
                            }
                            if removed {
                                remove_channel = Some((spec.id.clone(), spec.name.clone()));
                            }
                            if chosen {
                                choose_input = Some(Dialog::ChannelInput {
                                    id: spec.id.clone(),
                                    name: spec.name.clone(),
                                    current: spec.input.clone(),
                                    input: InputForm::new(&spec.input),
                                });
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
                        ui.label("");
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
        if add_channel {
            self.model.find_audio_inputs();
            self.dialog = Some(Dialog::AddChannel {
                name: String::new(),
                input: InputForm::default(),
            });
        }
        if let Some(dialog) = choose_input {
            self.model.find_audio_inputs();
            self.dialog = Some(dialog);
        }
        if let Some((id, name)) = remove_channel {
            self.dialog = Some(Dialog::Confirm(Confirm::RemoveChannel { id, name }));
        }
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
            ui.label("");
            let recording = output == OutputKind::Recording;
            // A running recording keeps its tracks until it stops: it shows
            // them, and they cannot change or become its mix meanwhile.
            let live_tracks = mixer
                .outputs
                .iter()
                .find(|mix| recording && mix.output == output && mix.running)
                .map(|mix| mix.tracks.as_slice());
            if live_tracks.is_some() {
                header
                    .response
                    .on_hover_text("The running recording keeps its tracks until it stops.");
            }
            for bus in &mixer.buses {
                let selected = bus.id == mix;
                let live_track = live_tracks.is_some_and(|tracks| tracks.contains(&bus.id));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = size::SPACE_6;
                    let choice = ui
                        .add_enabled_ui(!live_track, |ui| {
                            widgets::radio(ui, selected, &format!("{label} takes {}", bus.name))
                        })
                        .inner
                        .on_disabled_hover_text(format!(
                            "{} is a track of the running recording until it stops",
                            bus.name
                        ));
                    if choice.clicked() && !selected {
                        self.model.set_output_mix(output, &bus.id);
                    }
                    if recording {
                        let tracks = live_tracks.unwrap_or(&profiles.recording.tracks);
                        ui.add_enabled_ui(live_tracks.is_none(), |ui| {
                            self.track_chip(ui, bus, selected, tracks);
                        });
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
        let reason = if !ui.is_enabled() {
            "The running recording keeps its tracks; stop it to change them".to_owned()
        } else {
            format!("At most {MAX_EXTRA_TRACKS} tracks besides the mix")
        };
        let clicked = ui
            .add_enabled_ui(index.is_some() || !full, |ui| {
                widgets::chip(ui, &text, index.is_some(), &label)
            })
            .inner
            .on_disabled_hover_text(reason)
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

/// A bus's name over its column, with removal for aux buses; `blocked`
/// says why removal is not possible now. Returns whether removal was asked
/// for.
fn column_header(ui: &mut egui::Ui, bus: &BusState, blocked: Option<String>) -> bool {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = size::SPACE_4;
        ui.add(
            egui::Label::new(TextRole::Label.text(&bus.name).color(color::TEXT_PRIMARY)).truncate(),
        );
        if bus.id == PROGRAM_BUS_ID {
            return false;
        }
        let response = ui
            .add_enabled_ui(blocked.is_none(), |ui| {
                widgets::icon_button(ui, Icon::Trash, &format!("Remove bus {}…", bus.name))
            })
            .inner;
        match blocked {
            Some(reason) => {
                response.on_disabled_hover_text(reason);
                false
            }
            None => response.clicked(),
        }
    })
    .inner
}

/// Why a bus cannot be removed while output profiles use it (the engine
/// refuses, so a profile never names a missing bus), or `None`.
fn in_use(profiles: &OutputProfiles, bus_id: &str) -> Option<String> {
    let mut users = Vec::new();
    if profiles.recording.mix == bus_id {
        users.push("the recording's mix");
    }
    if profiles
        .recording
        .tracks
        .iter()
        .any(|track| track == bus_id)
    {
        users.push("a recording track");
    }
    if profiles.stream.mix == bus_id {
        users.push("the stream's mix");
    }
    if profiles.monitor.mix == bus_id {
        users.push("the monitor's mix");
    }
    (!users.is_empty()).then(|| {
        format!(
            "It is {}; choose another bus there before removing it",
            users.join(" and ")
        )
    })
}

fn row_header(ui: &mut egui::Ui, name: &str) -> egui::Response {
    ui.add(egui::Label::new(TextRole::Body.text(name)).truncate())
}

/// A channel's name in the first column, cut to fit, after a warning icon
/// (with the reason as its tooltip) when there is one, and its removal;
/// `blocked` says why removal is not possible now. Returns its rect and
/// whether removal was asked for.
fn channel_header(
    ui: &mut egui::Ui,
    name: &str,
    warning: Option<&str>,
    blocked: Option<&str>,
) -> (egui::Rect, bool) {
    let response = ui.allocate_ui_with_layout(
        egui::vec2(COLUMN, ROW),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            ui.set_width(COLUMN);
            ui.spacing_mut().item_spacing.x = size::SPACE_4;
            let remove = ui
                .add_enabled_ui(blocked.is_none(), |ui| {
                    widgets::icon_button(ui, Icon::Trash, &format!("Remove channel {name}…"))
                })
                .inner;
            let removed = match blocked {
                Some(reason) => {
                    remove.on_disabled_hover_text(reason);
                    false
                }
                None => remove.clicked(),
            };
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if let Some(reason) = warning {
                    let (rect, response) =
                        ui.allocate_exact_size(egui::Vec2::splat(14.0), egui::Sense::hover());
                    Icon::Warning.paint(ui.painter(), rect, color::STATE_WARN);
                    response.on_hover_text(reason);
                }
                ui.add(egui::Label::new(TextRole::Body.text(name)).truncate());
            });
            removed
        },
    );
    (response.response.rect, response.inner)
}

/// What a channel plays, as a field that opens the choice. Returns whether
/// it was clicked.
fn input_cell(ui: &mut egui::Ui, channel: &str, label: &InputLabel, idle: bool) -> bool {
    let tint = if label.missing {
        color::STATE_WARN
    } else {
        color::TEXT_SECONDARY
    };
    ui.add_enabled_ui(idle, |ui| {
        widgets::select_field(
            ui,
            label.icon,
            tint,
            &label.text,
            &format!("Input of {channel}"),
            INPUT_COLUMN,
        )
    })
    .inner
    .on_hover_text(format!("{}. Click to choose another input.", label.detail))
    .on_disabled_hover_text(format!("{}. {IDLE_INPUT}.", label.detail))
    .clicked()
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

#[cfg(test)]
mod tests {
    use command_api::{EncoderPreference, MonitorProfile, RecordingProfile, StreamProfile};

    use super::*;

    #[test]
    fn a_bus_in_a_profile_says_where_it_is_used() {
        let profiles = OutputProfiles {
            recording: RecordingProfile {
                mix: PROGRAM_BUS_ID.to_owned(),
                tracks: vec!["bus_voice".to_owned()],
                video_encoder: EncoderPreference::Auto,
            },
            stream: StreamProfile {
                mix: PROGRAM_BUS_ID.to_owned(),
                video_bitrate_kbps: 6_000,
                video_encoder: EncoderPreference::Auto,
            },
            monitor: MonitorProfile {
                mix: "bus_voice".to_owned(),
                enabled: false,
                device: None,
            },
        };
        assert_eq!(
            in_use(&profiles, "bus_voice").as_deref(),
            Some(
                "It is a recording track and the monitor's mix; choose another bus there \
                 before removing it"
            )
        );
        assert_eq!(in_use(&profiles, "bus_music"), None);
    }
}
