//! The configured audio graph (ADR 0017): channels with stable ids and the
//! program bus.
//!
//! Adding or removing a channel and choosing what it plays need an idle
//! pipeline; the engine checks that before calling in here. Fader, mute and
//! balance change live: the pipeline applies them to its mixer by the
//! channel's position, which cannot change while it runs.

use audio_engine::{BusConfig, ChannelConfig, LevelReading};
use command_api::{
    AddChannelRequest, AudioGraphSpec, BusRef, BusState, CHANNEL_ID_PREFIX, ChannelInput,
    ChannelRef, ChannelSpec, ChannelState, CommandError, DEFAULT_OUTPUT_TONE_DBFS, DEFAULT_TONE_HZ,
    ErrorCode, LiveInputSpec, MAX_CHANNELS, MixerState, PROGRAM_BUS_ID, PROGRAM_BUS_NAME,
    ProgramBusSpec, SetChannelInputRequest, ToneSpec, UpdateBusRequest, UpdateChannelRequest,
};
use serde_json::json;

use crate::ids::new_id;
use crate::refs::{self, Kind};

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
        }],
        program: ProgramBusSpec {
            gain_db: 0.0,
            muted: false,
        },
    }
}

/// An output's own sources as the graph of its run: live inputs, then
/// tones, as channels `Input 1`, `Tone 1`, … with ids from their position
/// (`ch_input_1`, `ch_tone_1`), the same in every run with the same sources.
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
    });
    AudioGraphSpec {
        channels: inputs.chain(tones).collect(),
        program: ProgramBusSpec {
            gain_db: master_gain_db,
            muted: false,
        },
    }
}

/// The position of the channel `channel` names: by id first; a name given
/// as well must match it.
pub(crate) fn channel_index(
    graph: &AudioGraphSpec,
    channel: &ChannelRef,
) -> Result<usize, CommandError> {
    refs::position(
        &graph.channels,
        channel.id.as_deref(),
        channel.name.as_deref(),
        &Kind {
            what: "channel",
            id_field: "id",
        },
        |channel| (&channel.id, &channel.name),
    )
}

/// Add a channel at the end; returns its id.
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
    graph.channels.push(ChannelSpec {
        id: id.clone(),
        name: request.name,
        input: request.input,
        gain_db: request.gain_db,
        muted: request.muted,
        balance: request.balance,
    });
    Ok(id)
}

pub(crate) fn remove_channel(
    graph: &mut AudioGraphSpec,
    channel: &ChannelRef,
) -> Result<ChannelSpec, CommandError> {
    let index = channel_index(graph, channel)?;
    Ok(graph.channels.remove(index))
}

/// Apply a fader, mute or balance change; returns the channel's position.
pub(crate) fn update_channel(
    graph: &mut AudioGraphSpec,
    request: &UpdateChannelRequest,
) -> Result<usize, CommandError> {
    let index = channel_index(graph, &request.channel)?;
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
    let index = channel_index(graph, &request.channel)?;
    graph.channels[index].input = request.input;
    Ok(())
}

/// Apply a change to the bus `request` names; only the program bus exists
/// so far.
pub(crate) fn update_bus(
    graph: &mut AudioGraphSpec,
    request: &UpdateBusRequest,
) -> Result<(), CommandError> {
    check_program(&request.bus)?;
    let program = &mut graph.program;
    program.gain_db = request.gain_db.unwrap_or(program.gain_db);
    program.muted = request.muted.unwrap_or(program.muted);
    Ok(())
}

fn check_program(bus: &BusRef) -> Result<(), CommandError> {
    match (&bus.id, &bus.name) {
        (Some(id), _) if id != PROGRAM_BUS_ID => Err(CommandError::new(
            ErrorCode::NotFound,
            format!("no bus with id `{id}` (the program bus is `{PROGRAM_BUS_ID}`)"),
        )
        .with_details(json!({ "id": id }))),
        (Some(_), Some(name)) if name != PROGRAM_BUS_NAME => Err(CommandError::new(
            ErrorCode::InvalidParams,
            format!("bus `{PROGRAM_BUS_ID}` is named `{PROGRAM_BUS_NAME}`, not `{name}`"),
        )),
        (None, Some(name)) if name != PROGRAM_BUS_NAME => Err(CommandError::new(
            ErrorCode::NotFound,
            format!("no bus named `{name}` (the program bus is `{PROGRAM_BUS_NAME}`)"),
        )
        .with_details(json!({ "name": name }))),
        _ => Ok(()),
    }
}

/// A channel's settings for the mixer; every channel sends to program.
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

/// `get_mixer`'s view of `graph`, with the mixer's levels while it runs.
pub(crate) fn mixer_state(graph: &AudioGraphSpec, levels: Option<&LevelReading>) -> MixerState {
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
        buses: vec![BusState {
            id: PROGRAM_BUS_ID.to_owned(),
            name: PROGRAM_BUS_NAME.to_owned(),
            kind: "program",
            gain_db: graph.program.gain_db,
            muted: graph.program.muted,
            peak_dbfs: levels.and_then(|levels| levels.program),
        }],
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
        assert_eq!(channel_index(&graph, &ChannelRef::id(&id)), Ok(1));
        assert_eq!(channel_index(&graph, &ChannelRef::name("Tone")), Ok(0));
        let both = ChannelRef {
            id: Some(id.clone()),
            name: Some("Tone".to_owned()),
        };
        assert_eq!(
            channel_index(&graph, &both).expect_err("mismatch").code,
            ErrorCode::InvalidParams
        );
        assert_eq!(
            channel_index(&graph, &ChannelRef::id("ch_missing"))
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
            },
        )
        .expect_err("no such bus");
        assert_eq!(other.code, ErrorCode::NotFound);
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
        assert_eq!(graph.program.gain_db, -1.0);
        assert_eq!(devices(&graph).count(), 1);
    }
}
