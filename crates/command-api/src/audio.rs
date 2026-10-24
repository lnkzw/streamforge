//! Types of the audio commands (ADR 0017): channels and buses with stable
//! ids, the sends between them, and the mixer state.
//!
//! A channel is one input's strip: what it plays (a capture device or a
//! test tone), its fader, mute and balance, and one send to every bus. A
//! bus sums its sends; an aux bus is a submix that can be assigned to the
//! program bus, or a separate mix. The program bus is the main mix and each
//! output's default; an output can take any bus (its profile's mix).
//! Adding or removing a channel or bus, and choosing what a channel plays,
//! need an idle pipeline; levels, mutes, balance, sends and assignments
//! change live.
//!
//! Channels and buses have stable ids like scenes (ADR 0014), and commands
//! name them by id or by name.

use std::collections::HashSet;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{MAX_ID_LEN, OutputKind, is_valid_id};

/// The lowest gain, send or assign level a command takes, in dB.
pub const MIN_GAIN_DB: f32 = audio_engine::MIN_GAIN_DB;
/// The highest gain, send or assign level a command takes, in dB.
pub const MAX_GAIN_DB: f32 = audio_engine::MAX_GAIN_DB;
/// Most channels; the mixer's fixed capacity.
pub const MAX_CHANNELS: usize = audio_engine::MAX_CHANNELS;
/// Most aux buses (the program bus comes on top).
pub const MAX_AUX_BUSES: usize = audio_engine::MAX_AUX_BUSES;
/// Channel ids start with this; the rest is opaque.
pub const CHANNEL_ID_PREFIX: &str = "ch_";
/// Bus ids start with this; the rest is opaque.
pub const BUS_ID_PREFIX: &str = "bus_";
/// The program bus: the mix every output takes for now. It cannot be
/// removed.
pub const PROGRAM_BUS_ID: &str = "bus_program";
pub const PROGRAM_BUS_NAME: &str = "Program";

/// What a channel plays.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelInput {
    /// A capture device from `list_audio_inputs`; `None`: the system
    /// default input.
    Device { device: Option<String> },
    /// A generated sine.
    Tone { frequency_hz: f64, level_dbfs: f64 },
}

impl ChannelInput {
    /// `device` or `tone`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Device { .. } => "device",
            Self::Tone { .. } => "tone",
        }
    }
}

/// A configured channel. Serialized as in `get_mixer` (without the level)
/// and in a project file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChannelSpec {
    pub id: String,
    pub name: String,
    pub input: ChannelInput,
    pub gain_db: f32,
    #[serde(rename = "mute")]
    pub muted: bool,
    /// −1 (left only) … 0 (centre) … 1 (right only).
    pub balance: f32,
    /// One per bus, in bus order: the program bus, then the aux buses.
    pub sends: Vec<SendSpec>,
}

/// Where a send takes its signal from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SendTap {
    /// After the channel's mute, before its fader and balance: the fader
    /// does not move it (a monitor mix).
    PreFader,
    /// After fader and balance.
    PostFader,
}

/// A channel's connection to one bus.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SendSpec {
    /// The bus's id.
    pub bus: String,
    pub enabled: bool,
    pub level_db: f32,
    pub tap: SendTap,
}

impl SendSpec {
    /// A new channel's send to the program bus: on, post-fader, at 0 dB.
    pub fn program() -> Self {
        Self {
            bus: PROGRAM_BUS_ID.to_owned(),
            enabled: true,
            level_db: 0.0,
            tap: SendTap::PostFader,
        }
    }

    /// A send to an aux bus that was not set up: off, post-fader, at 0 dB.
    pub fn off(bus: &str) -> Self {
        Self {
            bus: bus.to_owned(),
            enabled: false,
            level_db: 0.0,
            tap: SendTap::PostFader,
        }
    }
}

/// A configured aux bus. Serialized as in `get_mixer` (without the level
/// and kind) and in a project file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BusSpec {
    pub id: String,
    pub name: String,
    pub gain_db: f32,
    #[serde(rename = "mute")]
    pub muted: bool,
    /// The level it feeds the program bus with; `null`: a separate mix
    /// (e.g. for the monitor).
    pub assign_db: Option<f32>,
}

/// The program bus's settings.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ProgramBusSpec {
    pub gain_db: f32,
    #[serde(rename = "mute")]
    pub muted: bool,
}

/// The configured audio: the channels and the aux buses, in mixer order,
/// and the program bus. Serialized as a project file's `audio`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioGraphSpec {
    pub channels: Vec<ChannelSpec>,
    pub buses: Vec<BusSpec>,
    pub program: ProgramBusSpec,
}

impl AudioGraphSpec {
    /// A new channel's sends in this graph: on to the program bus, off to
    /// every aux bus.
    pub fn default_sends(&self) -> Vec<SendSpec> {
        std::iter::once(SendSpec::program())
            .chain(self.buses.iter().map(|bus| SendSpec::off(&bus.id)))
            .collect()
    }

    /// The rules across the graph that no single command's parameters
    /// show: limits, ids (form, unique), names (unique; `Program` is the
    /// program bus's), and every channel's sends, one per bus in bus order.
    /// The engine's commands keep them; a project file is checked by them.
    pub fn problems(&self) -> Vec<GraphProblem> {
        let mut problems = Vec::new();
        let mut problem = |path: String, message: String| {
            problems.push(GraphProblem { path, message });
        };
        if self.channels.len() > MAX_CHANNELS {
            problem(
                "channels".to_owned(),
                format!(
                    "at most {MAX_CHANNELS} channels, got {}",
                    self.channels.len()
                ),
            );
        }
        if self.buses.len() > MAX_AUX_BUSES {
            problem(
                "buses".to_owned(),
                format!(
                    "at most {MAX_AUX_BUSES} aux buses, got {}",
                    self.buses.len()
                ),
            );
        }
        let mut ids = HashSet::new();
        let mut names = HashSet::from([PROGRAM_BUS_NAME]);
        for (index, bus) in self.buses.iter().enumerate() {
            if !is_valid_id(BUS_ID_PREFIX, &bus.id) || bus.id == PROGRAM_BUS_ID {
                problem(
                    format!("buses[{index}].id"),
                    format!("{} (and not `{PROGRAM_BUS_ID}`)", id_rule(BUS_ID_PREFIX)),
                );
            } else if !ids.insert(bus.id.as_str()) {
                problem(
                    format!("buses[{index}].id"),
                    format!("bus id `{}` is used twice", bus.id),
                );
            }
            if !names.insert(bus.name.as_str()) {
                problem(
                    format!("buses[{index}].name"),
                    format!(
                        "bus name `{}` is used twice (`{PROGRAM_BUS_NAME}` is the program bus's)",
                        bus.name
                    ),
                );
            }
        }
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        let order: Vec<&str> = std::iter::once(PROGRAM_BUS_ID)
            .chain(self.buses.iter().map(|bus| bus.id.as_str()))
            .collect();
        for (index, channel) in self.channels.iter().enumerate() {
            if !is_valid_id(CHANNEL_ID_PREFIX, &channel.id) {
                problem(format!("channels[{index}].id"), id_rule(CHANNEL_ID_PREFIX));
            } else if !ids.insert(channel.id.as_str()) {
                problem(
                    format!("channels[{index}].id"),
                    format!("channel id `{}` is used twice", channel.id),
                );
            }
            if !names.insert(channel.name.as_str()) {
                problem(
                    format!("channels[{index}].name"),
                    format!("channel name `{}` is used twice", channel.name),
                );
            }
            if !channel
                .sends
                .iter()
                .map(|send| send.bus.as_str())
                .eq(order.iter().copied())
            {
                problem(
                    format!("channels[{index}].sends"),
                    "one send per bus, in bus order: the program bus, then the aux buses"
                        .to_owned(),
                );
            }
        }
        problems
    }
}

/// A problem in an [`AudioGraphSpec`]: where, as a path below the graph
/// (e.g. `channels[1].name`), and what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphProblem {
    pub path: String,
    pub message: String,
}

/// What an id must look like.
pub fn id_rule(prefix: &str) -> String {
    format!("an id must be `{prefix}`, then `a-z`, `0-9` or `_` (at most {MAX_ID_LEN} bytes)")
}

/// How a command names a channel: by id, by name, or by both, which must
/// then name the same channel. The id is looked up first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChannelRef {
    pub id: Option<String>,
    pub name: Option<String>,
}

impl ChannelRef {
    pub fn id(id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            name: None,
        }
    }

    pub fn name(name: impl Into<String>) -> Self {
        Self {
            id: None,
            name: Some(name.into()),
        }
    }
}

/// How a command names a bus: by id or by name, as for channels.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BusRef {
    pub id: Option<String>,
    pub name: Option<String>,
}

impl BusRef {
    pub fn program() -> Self {
        Self {
            id: Some(PROGRAM_BUS_ID.to_owned()),
            name: None,
        }
    }
}

/// Add a channel. Validated by `Command::decode`: a valid name, a tone
/// below Nyquist at −96 to 0 dBFS, gain −96 to +24 dB, balance −1 to 1.
#[derive(Debug, Clone, PartialEq)]
pub struct AddChannelRequest {
    pub name: String,
    pub input: ChannelInput,
    pub gain_db: f32,
    pub muted: bool,
    pub balance: f32,
}

/// Change a channel's fader, mute or balance (live if running); at least
/// one is given.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateChannelRequest {
    pub channel: ChannelRef,
    pub gain_db: Option<f32>,
    pub muted: Option<bool>,
    pub balance: Option<f32>,
}

/// Choose what a channel plays (while the pipeline is idle).
#[derive(Debug, Clone, PartialEq)]
pub struct SetChannelInputRequest {
    pub channel: ChannelRef,
    pub input: ChannelInput,
}

/// Add an aux bus. Validated by `Command::decode`: a valid name, gain and
/// assign level −96 to +24 dB.
#[derive(Debug, Clone, PartialEq)]
pub struct AddBusRequest {
    pub name: String,
    pub gain_db: f32,
    pub muted: bool,
    /// `None`: a separate mix.
    pub assign_db: Option<f32>,
}

/// Change a bus's fader, mute or assignment (live if running); at least
/// one is given.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateBusRequest {
    pub bus: BusRef,
    pub gain_db: Option<f32>,
    pub muted: Option<bool>,
    /// An aux bus's assignment: `Some(None)` makes it a separate mix.
    pub assign_db: Option<Option<f32>>,
}

/// Change a channel's send to a bus (live if running); at least one of
/// `enabled`, `level_db` and `tap` is given.
#[derive(Debug, Clone, PartialEq)]
pub struct SetSendRequest {
    pub channel: ChannelRef,
    pub bus: BusRef,
    pub enabled: Option<bool>,
    pub level_db: Option<f32>,
    pub tap: Option<SendTap>,
}

/// A channel in `get_mixer`: its settings and its level.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChannelState {
    #[serde(flatten)]
    pub spec: ChannelSpec,
    /// Highest post-fader peak of the last 300 ms (dBFS; `null`: silence
    /// or not running).
    pub peak_dbfs: Option<f64>,
}

/// A bus in `get_mixer`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BusState {
    pub id: String,
    pub name: String,
    /// `program` or `aux`.
    pub kind: &'static str,
    pub gain_db: f32,
    #[serde(rename = "mute")]
    pub muted: bool,
    /// An aux bus's level into the program bus; `null`: a separate mix, or
    /// the program bus itself.
    pub assign_db: Option<f32>,
    /// Highest peak of the last 300 ms, before the output limit (dBFS; it
    /// can exceed 0 when the mix overloads; `null`: silence or not running).
    pub peak_dbfs: Option<f64>,
}

/// Result of `get_mixer` and the audio commands.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MixerState {
    /// Whether the pipeline (and so the mixer) runs; levels need it.
    pub running: bool,
    pub channels: Vec<ChannelState>,
    /// The program bus first, then the aux buses.
    pub buses: Vec<BusState>,
    /// The bus each output takes as its mix: while it runs, the one it
    /// takes now; otherwise its profile's.
    pub outputs: Vec<OutputMix>,
    /// Routing that is allowed but likely unintended, e.g. a channel that
    /// reaches the program bus twice.
    pub warnings: Vec<String>,
}

impl MixerState {
    /// The ways `channel` (one of [`MixerState::channels`]) reaches the
    /// program bus; see [`program_paths`].
    pub fn program_paths<'a>(&'a self, channel: &ChannelSpec) -> Vec<ProgramPath<'a>> {
        program_paths(
            &channel.sends,
            self.buses
                .iter()
                .filter(|bus| bus.id != PROGRAM_BUS_ID)
                .map(|bus| (bus.name.as_str(), bus.assign_db)),
        )
    }
}

/// One way a channel reaches the program bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramPath<'a> {
    /// Its own send to the program bus.
    Direct,
    /// Its send to an aux bus (by name) that is assigned to the program bus.
    Through(&'a str),
}

impl fmt::Display for ProgramPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct => f.write_str("directly"),
            Self::Through(bus) => write!(f, "through `{bus}`"),
        }
    }
}

/// The ways a channel with `sends` (one per bus in bus order, the program
/// bus first) reaches the program bus: directly, and through each aux bus
/// assigned to it. `aux` are the aux buses' names and assignments, in bus
/// order. More than one is double routing: allowed (ADR 0017), but +6 dB at
/// equal levels and rarely meant, so `get_mixer` warns and the desktop
/// console marks it. It is about the routing, so mutes and levels, which
/// change live, do not count.
pub fn program_paths<'a>(
    sends: &[SendSpec],
    aux: impl IntoIterator<Item = (&'a str, Option<f32>)>,
) -> Vec<ProgramPath<'a>> {
    let direct = sends
        .first()
        .is_some_and(|send| send.enabled)
        .then_some(ProgramPath::Direct);
    let through = aux
        .into_iter()
        .zip(sends.iter().skip(1))
        .filter(|((_, assign_db), send)| send.enabled && assign_db.is_some())
        .map(|((name, _), _)| ProgramPath::Through(name));
    direct.into_iter().chain(through).collect()
}

/// An output's mix in `get_mixer`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputMix {
    pub output: OutputKind,
    /// The bus's id.
    pub bus: String,
    /// The recording's further audio tracks (bus ids); fixed while it runs.
    pub tracks: Vec<String>,
    pub running: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(bus: &str, enabled: bool) -> SendSpec {
        SendSpec {
            enabled,
            ..SendSpec::off(bus)
        }
    }

    #[test]
    fn program_paths_count_the_direct_send_and_assigned_buses() {
        let aux = [("Voice", Some(-3.0)), ("Monitor", None)];
        let sends = [
            send(PROGRAM_BUS_ID, true),
            send("bus_voice", true),
            send("bus_monitor", true),
        ];
        let paths = program_paths(&sends, aux);
        // A separate mix (Monitor) does not reach program.
        assert_eq!(paths, [ProgramPath::Direct, ProgramPath::Through("Voice")]);
        assert_eq!(paths[1].to_string(), "through `Voice`");
        // Through the submix only: one path; no sends at all: none.
        let sends = [send(PROGRAM_BUS_ID, false), send("bus_voice", true)];
        assert_eq!(program_paths(&sends, aux), [ProgramPath::Through("Voice")]);
        assert!(program_paths(&[], aux).is_empty());
    }
}
