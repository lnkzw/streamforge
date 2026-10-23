//! The configured audio graph (ADR 0017): channels and aux buses with
//! stable ids, the sends between them, and the program bus.
//!
//! Adding or removing a channel or bus, and choosing what a channel plays,
//! need an idle pipeline; the engine checks that before calling in here.
//! Levels, mutes, balance, sends and assignments change live: the pipeline
//! applies them to its mixer by position, which cannot change while it
//! runs. Every channel holds one send per bus, in bus order (the program
//! bus first); adding and removing buses and channels keeps it so.

use audio_engine::{
    BusConfig, BusRef as GraphBus, ChannelConfig, LevelReading, MixerBus, SendConfig, SendMatrix,
    Tap,
};
use command_api::{
    AddBusRequest, AddChannelRequest, AudioGraphSpec, BUS_ID_PREFIX, BusRef, BusSpec, BusState,
    CHANNEL_ID_PREFIX, ChannelInput, ChannelRef, ChannelSpec, ChannelState, CommandError,
    DEFAULT_OUTPUT_TONE_DBFS, DEFAULT_TONE_HZ, ErrorCode, LiveInputSpec, MAX_AUX_BUSES,
    MAX_CHANNELS, MixerState, PROGRAM_BUS_ID, PROGRAM_BUS_NAME, ProgramBusSpec, SendSpec, SendTap,
    SetChannelInputRequest, SetSendRequest, ToneSpec, UpdateBusRequest, UpdateChannelRequest,
};

use crate::ids::new_id;
use crate::refs::{self, Kind, mismatch};

/// The built-in channel of a new engine: the default tone, so outputs
/// started without configuring audio have an audibly alive track.
pub(crate) const DEFAULT_CHANNEL_ID: &str = "ch_test_tone";

pub(crate) fn default_graph() -> AudioGraphSpec {
    AudioGraphSpec {
        channels: vec![ChannelSpec {
            id: DEFAULT_CHANNEL_ID.to_owned(),
            name: "Tone".to_owned(),
            input: ChannelInput::Tone {
                frequency_hz: DEFAULT_TONE_HZ,
                level_dbfs: DEFAULT_OUTPUT_TONE_DBFS,
            },
            gain_db: 0.0,
            muted: false,
            balance: 0.0,
            sends: vec![SendSpec::program()],
        }],
        buses: Vec::new(),
        program: ProgramBusSpec {
            gain_db: 0.0,
            muted: false,
        },
    }
}

/// An output's own sources as the graph of its run: live inputs, then
/// tones, as channels `Input 1`, `Tone 1`, … with ids from their position
/// (`ch_input_1`, `ch_tone_1`), the same in every run with the same sources.
/// They send to the program bus only.
pub(crate) fn from_sources(
    inputs: &[LiveInputSpec],
    tones: &[ToneSpec],
    master_gain_db: f32,
) -> AudioGraphSpec {
    let inputs = inputs.iter().enumerate().map(|(index, input)| ChannelSpec {
        id: format!("ch_input_{}", index + 1),
        name: format!("Input {}", index + 1),
        input: ChannelInput::Device {
            device: input.input.clone(),
        },
        gain_db: input.gain_db,
        muted: input.muted,
        balance: 0.0,
        sends: vec![SendSpec::program()],
    });
    let tones = tones.iter().enumerate().map(|(index, tone)| ChannelSpec {
        id: format!("ch_tone_{}", index + 1),
        name: format!("Tone {}", index + 1),
        input: ChannelInput::Tone {
            frequency_hz: tone.frequency_hz,
            level_dbfs: tone.level_dbfs,
        },
        gain_db: tone.gain_db,
        muted: tone.muted,
        balance: 0.0,
        sends: vec![SendSpec::program()],
    });
    AudioGraphSpec {
        channels: inputs.chain(tones).collect(),
        buses: Vec::new(),
        program: ProgramBusSpec {
            gain_db: master_gain_db,
            muted: false,
        },
    }
}

/// The position of `bus`'s send in a channel's `sends`.
fn send_index(bus: GraphBus) -> usize {
    match bus {
        GraphBus::Program => 0,
        GraphBus::Aux(index) => index + 1,
    }
}

/// The parameters that name a channel or bus: `id` and `name`, or e.g.
/// `set_send`'s `bus_id` and `bus`.
pub(crate) type Fields = (&'static str, &'static str);
const OWN: Fields = ("id", "name");

/// The position of the channel `channel` names (in the parameters
/// `fields`): by id first; a name given as well must match it.
pub(crate) fn channel_index(
    graph: &AudioGraphSpec,
    channel: &ChannelRef,
    (id_field, name_field): Fields,
) -> Result<usize, CommandError> {
    refs::position(
        &graph.channels,
        channel.id.as_deref(),
        channel.name.as_deref(),
        &Kind {
            what: "channel",
            id_field,
            name_field,
        },
        |channel| (&channel.id, &channel.name),
    )
}

/// The bus `bus` names (in the parameters `fields`): by id first; a name
/// given as well must match it.
pub(crate) fn bus_position(
    graph: &AudioGraphSpec,
    bus: &BusRef,
    (id_field, name_field): Fields,
) -> Result<GraphBus, CommandError> {
    let (id, name) = (bus.id.as_deref(), bus.name.as_deref());
    match (id, name) {
        (Some(PROGRAM_BUS_ID), Some(name)) if name != PROGRAM_BUS_NAME => Err(mismatch(format!(
            "bus `{PROGRAM_BUS_ID}` is named `{PROGRAM_BUS_NAME}`, not `{name}`"
        ))),
        (Some(PROGRAM_BUS_ID), _) | (None, Some(PROGRAM_BUS_NAME)) => Ok(GraphBus::Program),
        _ => refs::position(
            &graph.buses,
            id,
            name,
            &Kind {
                what: "bus",
                id_field,
                name_field,
            },
            |bus| (&bus.id, &bus.name),
        )
        .map(GraphBus::Aux),
    }
}

/// Add a channel at the end, sending as [`AudioGraphSpec::default_sends`];
/// returns its id.
pub(crate) fn add_channel(
    graph: &mut AudioGraphSpec,
    request: AddChannelRequest,
) -> Result<String, CommandError> {
    if graph
        .channels
        .iter()
        .any(|channel| channel.name == request.name)
    {
        return Err(CommandError::new(
            ErrorCode::InvalidParams,
            format!("a channel named `{}` already exists", request.name),
        ));
    }
    if graph.channels.len() >= MAX_CHANNELS {
        return Err(CommandError::new(
            ErrorCode::InvalidState,
            format!("at most {MAX_CHANNELS} channels; remove one first"),
        ));
    }
    let id = new_id(CHANNEL_ID_PREFIX, |id| {
        graph.channels.iter().any(|channel| channel.id == id)
    })?;
    let sends = graph.default_sends();
    graph.channels.push(ChannelSpec {
        id: id.clone(),
        name: request.name,
        input: request.input,
        gain_db: request.gain_db,
        muted: request.muted,
        balance: request.balance,
        sends,
    });
    debug_assert_eq!(graph.problems(), []);
    Ok(id)
}

pub(crate) fn remove_channel(
    graph: &mut AudioGraphSpec,
    channel: &ChannelRef,
) -> Result<ChannelSpec, CommandError> {
    let index = channel_index(graph, channel, OWN)?;
    let removed = graph.channels.remove(index);
    debug_assert_eq!(graph.problems(), []);
    Ok(removed)
}

/// Apply a fader, mute or balance change; returns the channel's position.
pub(crate) fn update_channel(
    graph: &mut AudioGraphSpec,
    request: &UpdateChannelRequest,
) -> Result<usize, CommandError> {
    let index = channel_index(graph, &request.channel, OWN)?;
    let channel = &mut graph.channels[index];
    channel.gain_db = request.gain_db.unwrap_or(channel.gain_db);
    channel.muted = request.muted.unwrap_or(channel.muted);
    channel.balance = request.balance.unwrap_or(channel.balance);
    Ok(index)
}

pub(crate) fn set_channel_input(
    graph: &mut AudioGraphSpec,
    request: SetChannelInputRequest,
) -> Result<(), CommandError> {
    let index = channel_index(graph, &request.channel, OWN)?;
    graph.channels[index].input = request.input;
    Ok(())
}

/// Add an aux bus at the end; every channel's send to it starts off.
/// Returns its id.
pub(crate) fn add_bus(
    graph: &mut AudioGraphSpec,
    request: AddBusRequest,
) -> Result<String, CommandError> {
    if request.name == PROGRAM_BUS_NAME || graph.buses.iter().any(|bus| bus.name == request.name) {
        return Err(CommandError::new(
            ErrorCode::InvalidParams,
            format!("a bus named `{}` already exists", request.name),
        ));
    }
    if graph.buses.len() >= MAX_AUX_BUSES {
        return Err(CommandError::new(
            ErrorCode::InvalidState,
            format!("at most {MAX_AUX_BUSES} aux buses; remove one first"),
        ));
    }
    let id = new_id(BUS_ID_PREFIX, |id| {
        id == PROGRAM_BUS_ID || graph.buses.iter().any(|bus| bus.id == id)
    })?;
    for channel in &mut graph.channels {
        channel.sends.push(SendSpec::off(&id));
    }
    graph.buses.push(BusSpec {
        id: id.clone(),
        name: request.name,
        gain_db: request.gain_db,
        muted: request.muted,
        assign_db: request.assign_db,
    });
    debug_assert_eq!(graph.problems(), []);
    Ok(id)
}

/// Remove an aux bus and every send to it.
pub(crate) fn remove_bus(
    graph: &mut AudioGraphSpec,
    bus: &BusRef,
) -> Result<BusSpec, CommandError> {
    let GraphBus::Aux(index) = bus_position(graph, bus, OWN)? else {
        return Err(CommandError::new(
            ErrorCode::InvalidParams,
            "the program bus cannot be removed: it is what the outputs take",
        ));
    };
    for channel in &mut graph.channels {
        channel.sends.remove(send_index(GraphBus::Aux(index)));
    }
    let removed = graph.buses.remove(index);
    debug_assert_eq!(graph.problems(), []);
    Ok(removed)
}

/// Apply a fader, mute or assignment change; returns the bus.
pub(crate) fn update_bus(
    graph: &mut AudioGraphSpec,
    request: &UpdateBusRequest,
) -> Result<GraphBus, CommandError> {
    let bus = bus_position(graph, &request.bus, OWN)?;
    match bus {
        GraphBus::Program => {
            if request.assign_db.is_some() {
                return Err(CommandError::new(
                    ErrorCode::InvalidParams,
                    "the program bus feeds no other bus; `assign_db` is for aux buses",
                ));
            }
            let program = &mut graph.program;
            program.gain_db = request.gain_db.unwrap_or(program.gain_db);
            program.muted = request.muted.unwrap_or(program.muted);
        }
        GraphBus::Aux(index) => {
            let aux = &mut graph.buses[index];
            aux.gain_db = request.gain_db.unwrap_or(aux.gain_db);
            aux.muted = request.muted.unwrap_or(aux.muted);
            aux.assign_db = request.assign_db.unwrap_or(aux.assign_db);
        }
    }
    Ok(bus)
}

/// Apply a send change; returns the channel's position and the bus.
pub(crate) fn set_send(
    graph: &mut AudioGraphSpec,
    request: &SetSendRequest,
) -> Result<(usize, GraphBus), CommandError> {
    let channel = channel_index(graph, &request.channel, ("channel_id", "channel"))?;
    let bus = bus_position(graph, &request.bus, ("bus_id", "bus"))?;
    let send = &mut graph.channels[channel].sends[send_index(bus)];
    send.enabled = request.enabled.unwrap_or(send.enabled);
    send.level_db = request.level_db.unwrap_or(send.level_db);
    send.tap = request.tap.unwrap_or(send.tap);
    Ok((channel, bus))
}

/// The send of channel `channel` to `bus`, for the mixer.
pub(crate) fn send_of(graph: &AudioGraphSpec, channel: usize, bus: GraphBus) -> SendConfig {
    send_config(&graph.channels[channel].sends[send_index(bus)])
}

/// A channel's settings for the mixer.
pub(crate) fn channel_config(channel: &ChannelSpec) -> ChannelConfig {
    ChannelConfig {
        gain_db: channel.gain_db,
        muted: channel.muted,
        balance: channel.balance,
    }
}

pub(crate) fn program_config(program: &ProgramBusSpec) -> BusConfig {
    BusConfig {
        gain_db: program.gain_db,
        muted: program.muted,
        assign_db: None,
    }
}

fn bus_config(bus: &BusSpec) -> BusConfig {
    BusConfig {
        gain_db: bus.gain_db,
        muted: bus.muted,
        assign_db: bus.assign_db,
    }
}

/// The mixer's config of `bus` in `graph`.
pub(crate) fn config_of(graph: &AudioGraphSpec, bus: GraphBus) -> BusConfig {
    match bus {
        GraphBus::Program => program_config(&graph.program),
        GraphBus::Aux(index) => bus_config(&graph.buses[index]),
    }
}

fn send_config(send: &SendSpec) -> SendConfig {
    SendConfig {
        enabled: send.enabled,
        level_db: send.level_db,
        tap: match send.tap {
            SendTap::PreFader => Tap::PreFader,
            SendTap::PostFader => Tap::PostFader,
        },
    }
}

/// The mixer's aux buses and send matrix for `graph`.
pub(crate) fn mixer_routing(graph: &AudioGraphSpec) -> (Vec<MixerBus>, SendMatrix) {
    let buses = graph
        .buses
        .iter()
        .map(|bus| MixerBus {
            name: bus.name.clone(),
            config: bus_config(bus),
        })
        .collect();
    let mut sends = SendMatrix::program_only(graph.channels.len(), graph.buses.len());
    for (index, channel) in graph.channels.iter().enumerate() {
        for (column, send) in channel.sends.iter().enumerate() {
            sends.set(index, GraphBus::from_column(column), send_config(send));
        }
    }
    (buses, sends)
}

/// The capture devices the graph's channels play, in channel order.
pub(crate) fn devices(
    graph: &AudioGraphSpec,
) -> impl Iterator<Item = (&ChannelSpec, &Option<String>)> {
    graph
        .channels
        .iter()
        .filter_map(|channel| match &channel.input {
            ChannelInput::Device { device } => Some((channel, device)),
            ChannelInput::Tone { .. } => None,
        })
}

/// Channels that reach the program bus more than once: directly, and
/// through each aux bus assigned to it. Allowed (ADR 0017), but it is
/// +6 dB at equal levels, and rarely meant. It is about the routing, so
/// mutes and levels, which change live, do not count.
fn double_routing(graph: &AudioGraphSpec) -> Vec<String> {
    graph
        .channels
        .iter()
        .filter_map(|channel| {
            let direct = channel.sends[0].enabled.then(|| "directly".to_owned());
            let through = graph
                .buses
                .iter()
                .zip(&channel.sends[1..])
                .filter(|(bus, send)| send.enabled && bus.assign_db.is_some())
                .map(|(bus, _)| format!("through `{}`", bus.name));
            let paths: Vec<String> = direct.into_iter().chain(through).collect();
            (paths.len() > 1).then(|| {
                format!(
                    "channel `{}` reaches the program bus {} times: {}; turn off a send if \
                     that is not meant",
                    channel.name,
                    paths.len(),
                    paths.join(", ")
                )
            })
        })
        .collect()
}

/// `get_mixer`'s view of `graph`, with the mixer's levels while it runs.
pub(crate) fn mixer_state(graph: &AudioGraphSpec, levels: Option<&LevelReading>) -> MixerState {
    let program = BusState {
        id: PROGRAM_BUS_ID.to_owned(),
        name: PROGRAM_BUS_NAME.to_owned(),
        kind: "program",
        gain_db: graph.program.gain_db,
        muted: graph.program.muted,
        assign_db: None,
        peak_dbfs: levels.and_then(|levels| levels.program),
    };
    let aux = graph.buses.iter().enumerate().map(|(index, bus)| BusState {
        id: bus.id.clone(),
        name: bus.name.clone(),
        kind: "aux",
        gain_db: bus.gain_db,
        muted: bus.muted,
        assign_db: bus.assign_db,
        peak_dbfs: levels.and_then(|levels| levels.aux_buses.get(index).copied().flatten()),
    });
    MixerState {
        running: levels.is_some(),
        channels: graph
            .channels
            .iter()
            .enumerate()
            .map(|(index, channel)| ChannelState {
                spec: channel.clone(),
                peak_dbfs: levels.and_then(|levels| levels.channels.get(index).copied().flatten()),
            })
            .collect(),
        buses: std::iter::once(program).chain(aux).collect(),
        warnings: double_routing(graph),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(name: &str) -> AddChannelRequest {
        AddChannelRequest {
            name: name.to_owned(),
            input: ChannelInput::Tone {
                frequency_hz: 440.0,
                level_dbfs: -12.0,
            },
            gain_db: 0.0,
            muted: false,
            balance: 0.0,
        }
    }

    fn bus(name: &str, assign_db: Option<f32>) -> AddBusRequest {
        AddBusRequest {
            name: name.to_owned(),
            gain_db: 0.0,
            muted: false,
            assign_db,
        }
    }

    fn named(name: &str) -> BusRef {
        BusRef {
            id: None,
            name: Some(name.to_owned()),
        }
    }

    fn send(channel: &str, bus: &str, enabled: bool) -> SetSendRequest {
        SetSendRequest {
            channel: ChannelRef::name(channel),
            bus: named(bus),
            enabled: Some(enabled),
            level_db: None,
            tap: None,
        }
    }

    #[test]
    fn the_built_in_graphs_keep_the_graph_rules() {
        assert_eq!(default_graph().problems(), []);
        let sources = from_sources(
            &[LiveInputSpec {
                input: None,
                gain_db: 0.0,
                muted: false,
            }],
            &[],
            0.0,
        );
        assert_eq!(sources.problems(), []);
        // A misordered send is what the check is for.
        let mut graph = default_graph();
        add_bus(&mut graph, bus("Voice", None)).expect("voice");
        graph.channels[0].sends.swap(0, 1);
        assert_eq!(graph.problems()[0].path, "channels[0].sends");
    }

    #[test]
    fn channels_are_added_with_fresh_ids_unique_names_and_a_limit() {
        let mut graph = default_graph();
        let id = add_channel(&mut graph, tone("Music")).expect("add");
        assert!(
            id.starts_with(CHANNEL_ID_PREFIX) && id != DEFAULT_CHANNEL_ID,
            "{id}"
        );
        let duplicate = add_channel(&mut graph, tone("Music")).expect_err("duplicate");
        assert_eq!(duplicate.code, ErrorCode::InvalidParams);
        for index in graph.channels.len()..MAX_CHANNELS {
            add_channel(&mut graph, tone(&format!("Tone {index}"))).expect("add");
        }
        let full = add_channel(&mut graph, tone("One too many")).expect_err("full");
        assert_eq!(full.code, ErrorCode::InvalidState);
    }

    #[test]
    fn channels_are_found_by_id_first_and_a_disagreeing_name_is_refused() {
        let mut graph = default_graph();
        let id = add_channel(&mut graph, tone("Music")).expect("add");
        assert_eq!(channel_index(&graph, &ChannelRef::id(&id), OWN), Ok(1));
        assert_eq!(channel_index(&graph, &ChannelRef::name("Tone"), OWN), Ok(0));
        let both = ChannelRef {
            id: Some(id.clone()),
            name: Some("Tone".to_owned()),
        };
        assert_eq!(
            channel_index(&graph, &both, OWN)
                .expect_err("mismatch")
                .code,
            ErrorCode::InvalidParams
        );
        assert_eq!(
            channel_index(&graph, &ChannelRef::id("ch_missing"), OWN)
                .expect_err("missing")
                .code,
            ErrorCode::NotFound
        );
        let removed = remove_channel(&mut graph, &ChannelRef::id(&id)).expect("remove");
        assert_eq!(removed.name, "Music");
        assert_eq!(graph.channels.len(), 1);
    }

    #[test]
    fn updates_change_only_what_they_name() {
        let mut graph = default_graph();
        let index = update_channel(
            &mut graph,
            &UpdateChannelRequest {
                channel: ChannelRef::name("Tone"),
                gain_db: None,
                muted: Some(true),
                balance: Some(-0.5),
            },
        )
        .expect("update");
        assert_eq!(index, 0);
        let channel = &graph.channels[0];
        assert_eq!(
            (channel.gain_db, channel.muted, channel.balance),
            (0.0, true, -0.5)
        );
        update_bus(
            &mut graph,
            &UpdateBusRequest {
                bus: BusRef::program(),
                gain_db: Some(-6.0),
                muted: None,
                assign_db: None,
            },
        )
        .expect("program");
        assert_eq!(
            graph.program,
            ProgramBusSpec {
                gain_db: -6.0,
                muted: false
            }
        );
        let other = update_bus(
            &mut graph,
            &UpdateBusRequest {
                bus: BusRef {
                    id: Some("bus_voice".to_owned()),
                    name: None,
                },
                gain_db: Some(0.0),
                muted: None,
                assign_db: None,
            },
        )
        .expect_err("no such bus");
        assert_eq!(other.code, ErrorCode::NotFound);
    }

    #[test]
    fn buses_keep_one_send_per_channel_in_bus_order() {
        let mut graph = default_graph();
        let voice = add_bus(&mut graph, bus("Voice", Some(0.0))).expect("voice");
        let monitor = add_bus(&mut graph, bus("Monitor", None)).expect("monitor");
        assert!(voice.starts_with(BUS_ID_PREFIX) && voice != PROGRAM_BUS_ID);
        add_channel(&mut graph, tone("Music")).expect("music");
        for channel in &graph.channels {
            let buses: Vec<(&str, bool)> = channel
                .sends
                .iter()
                .map(|send| (send.bus.as_str(), send.enabled))
                .collect();
            assert_eq!(
                buses,
                [
                    (PROGRAM_BUS_ID, true),
                    (voice.as_str(), false),
                    (monitor.as_str(), false)
                ]
            );
        }
        // Names are unique among the buses, Program included.
        for name in ["Voice", "Program"] {
            let taken = add_bus(&mut graph, bus(name, None)).expect_err(name);
            assert_eq!(taken.code, ErrorCode::InvalidParams);
        }
        let removed = remove_bus(&mut graph, &named("Voice")).expect("remove");
        assert_eq!(removed.id, voice);
        assert!(
            graph
                .channels
                .iter()
                .all(|channel| channel.sends.len() == 2 && channel.sends[1].bus == monitor)
        );
        let program = remove_bus(&mut graph, &BusRef::program()).expect_err("program");
        assert_eq!(program.code, ErrorCode::InvalidParams);
        for index in graph.buses.len()..MAX_AUX_BUSES {
            add_bus(&mut graph, bus(&format!("Bus {index}"), None)).expect("add");
        }
        let full = add_bus(&mut graph, bus("One too many", None)).expect_err("full");
        assert_eq!(full.code, ErrorCode::InvalidState);
    }

    #[test]
    fn sends_and_assignments_change_what_they_name() {
        let mut graph = default_graph();
        add_bus(&mut graph, bus("Voice", None)).expect("voice");
        let (channel, voice) = set_send(
            &mut graph,
            &SetSendRequest {
                level_db: Some(-6.0),
                tap: Some(SendTap::PreFader),
                ..send("Tone", "Voice", true)
            },
        )
        .expect("send");
        assert_eq!((channel, voice), (0, GraphBus::Aux(0)));
        assert_eq!(
            graph.channels[0].sends[1],
            SendSpec {
                bus: graph.buses[0].id.clone(),
                enabled: true,
                level_db: -6.0,
                tap: SendTap::PreFader,
            }
        );
        let assigned = update_bus(
            &mut graph,
            &UpdateBusRequest {
                bus: named("Voice"),
                gain_db: None,
                muted: None,
                assign_db: Some(Some(-3.0)),
            },
        )
        .expect("assign");
        assert_eq!(assigned, GraphBus::Aux(0));
        assert_eq!(graph.buses[0].assign_db, Some(-3.0));
        let program = update_bus(
            &mut graph,
            &UpdateBusRequest {
                bus: BusRef::program(),
                gain_db: None,
                muted: None,
                assign_db: Some(None),
            },
        )
        .expect_err("program has no assignment");
        assert_eq!(program.code, ErrorCode::InvalidParams);
        let mismatch = bus_position(
            &graph,
            &BusRef {
                id: Some(PROGRAM_BUS_ID.to_owned()),
                name: Some("Voice".to_owned()),
            },
            OWN,
        )
        .expect_err("mismatch");
        assert_eq!(mismatch.code, ErrorCode::InvalidParams);
        // The mixer gets the same routing.
        let (buses, sends) = mixer_routing(&graph);
        assert_eq!(buses[0].config.assign_db, Some(-3.0));
        assert_eq!(
            sends.get(0, GraphBus::Aux(0)),
            Some(SendConfig {
                enabled: true,
                level_db: -6.0,
                tap: Tap::PreFader,
            })
        );
        assert_eq!(sends.get(0, GraphBus::Program), Some(SendConfig::PROGRAM));
        assert_eq!(send_of(&graph, 0, GraphBus::Aux(0)).level_db, -6.0);
    }

    #[test]
    fn a_channel_reaching_program_twice_is_a_warning() {
        let mut graph = default_graph();
        add_bus(&mut graph, bus("Voice", Some(0.0))).expect("voice");
        add_bus(&mut graph, bus("Monitor", None)).expect("monitor");
        assert!(mixer_state(&graph, None).warnings.is_empty());
        // A separate mix does not reach program.
        set_send(&mut graph, &send("Tone", "Monitor", true)).expect("monitor");
        assert!(mixer_state(&graph, None).warnings.is_empty());
        set_send(&mut graph, &send("Tone", "Voice", true)).expect("voice");
        let warnings = mixer_state(&graph, None).warnings;
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("`Tone`") && warnings[0].contains("`Voice`"));
        // Through the submix only: fine.
        set_send(&mut graph, &send("Tone", "Program", false)).expect("program");
        assert!(mixer_state(&graph, None).warnings.is_empty());
        // Through two submixes: twice again, without the direct send.
        update_bus(
            &mut graph,
            &UpdateBusRequest {
                bus: named("Monitor"),
                gain_db: None,
                muted: Some(true),
                assign_db: Some(Some(0.0)),
            },
        )
        .expect("assign");
        let warnings = mixer_state(&graph, None).warnings;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("through `Voice`, through `Monitor`"),
            "{warnings:?}"
        );
        // A missing bus is named in the parameter it was given in.
        let missing = set_send(
            &mut graph,
            &SetSendRequest {
                bus: BusRef {
                    id: Some("bus_gone".to_owned()),
                    name: None,
                },
                ..send("Tone", "x", true)
            },
        )
        .expect_err("missing");
        assert_eq!(
            missing.details,
            Some(serde_json::json!({"bus_id": "bus_gone"}))
        );
    }

    #[test]
    fn an_outputs_own_sources_become_channels_with_ids_from_their_position() {
        let graph = from_sources(
            &[LiveInputSpec {
                input: Some("mic".to_owned()),
                gain_db: -3.0,
                muted: true,
            }],
            &[ToneSpec {
                frequency_hz: 440.0,
                level_dbfs: -12.0,
                gain_db: 0.0,
                muted: false,
            }],
            -1.0,
        );
        let channels: Vec<(&str, &str)> = graph
            .channels
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(
            channels,
            [("ch_input_1", "Input 1"), ("ch_tone_1", "Tone 1")]
        );
        assert_eq!(
            graph.channels[0].input,
            ChannelInput::Device {
                device: Some("mic".to_owned())
            }
        );
        assert!(graph.channels[0].muted);
        assert_eq!(graph.channels[0].sends, [SendSpec::program()]);
        assert_eq!(graph.program.gain_db, -1.0);
        assert_eq!(devices(&graph).count(), 1);
    }
}
