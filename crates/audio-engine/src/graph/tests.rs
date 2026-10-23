use super::*;

/// Mono blocks keep the arithmetic readable; stereo tests say so.
fn mono(config: &GraphConfig, frames: usize) -> AudioGraph {
    AudioGraph::new(1, frames, config)
}

fn plain(channels: usize) -> GraphConfig {
    GraphConfig::plain(vec![ChannelConfig::UNITY; channels], 0.0)
}

/// One channel, an aux bus, and the given send to it.
fn with_aux(send: SendConfig, aux: BusConfig) -> GraphConfig {
    let mut sends = SendMatrix::program_only(1, 1);
    sends.set(0, BusRef::Aux(0), send);
    GraphConfig {
        channels: vec![ChannelConfig::UNITY],
        aux_buses: vec![aux],
        program: BusConfig::UNITY,
        sends,
    }
}

fn send(level_db: f32, tap: Tap) -> SendConfig {
    SendConfig {
        enabled: true,
        level_db,
        tap,
    }
}

#[test]
fn a_plain_graph_is_the_exact_sum() {
    let mut graph = AudioGraph::new(2, 2, &plain(2));
    let a = [0.25, -0.5, 0.125, 0.0];
    let b = [0.25, 0.25, -0.125, 0.5];
    graph.process(&[&a, &b]);
    assert_eq!(graph.program(), [0.5, -0.25, 0.0, 0.5]);
}

#[test]
fn missing_inputs_are_silence() {
    let mut graph = mono(&plain(2), 2);
    graph.process(&[&[0.5, 0.5]]);
    assert_eq!(graph.program(), [0.5, 0.5]);
    graph.process(&[]);
    assert_eq!(graph.program(), [0.0, 0.0]);
}

#[test]
fn non_finite_input_is_silenced_before_any_bus() {
    let mut graph = mono(&with_aux(send(0.0, Tap::PreFader), BusConfig::UNITY), 2);
    graph.process(&[&[f32::NAN, 0.5]]);
    assert_eq!(graph.program(), [0.0, 0.5]);
    assert_eq!(graph.aux(0).expect("aux"), [0.0, 0.5]);
    assert_eq!(graph.invalid_samples(), 1);
}

#[test]
fn mute_ramps_over_one_block_and_then_is_silent() {
    let mut graph = mono(&plain(1), 4);
    graph.set_channel(
        0,
        ChannelConfig {
            muted: true,
            ..ChannelConfig::UNITY
        },
    );
    let input = [1.0; 4];
    graph.process(&[&input]);
    assert_eq!(graph.program(), [0.75, 0.5, 0.25, 0.0]);
    graph.process(&[&input]);
    assert_eq!(graph.program(), [0.0; 4]);
}

#[test]
fn a_muted_channel_is_silent_in_every_bus_pre_fader_too() {
    let mut config = with_aux(send(0.0, Tap::PreFader), BusConfig::UNITY);
    config.channels[0].muted = true;
    let mut graph = mono(&config, 2);
    graph.process(&[&[0.5, 0.5]]);
    assert_eq!(graph.program(), [0.0, 0.0]);
    assert_eq!(graph.aux(0).expect("aux"), [0.0, 0.0]);
}

#[test]
fn the_fader_moves_post_fader_sends_and_not_pre_fader_ones() {
    let mut config = with_aux(send(0.0, Tap::PreFader), BusConfig::UNITY);
    config.channels[0].gain_db = -6.0;
    let mut graph = mono(&config, 2);
    graph.process(&[&[0.5, 0.5]]);
    let post = 0.5 * db_to_linear(-6.0);
    assert_eq!(graph.program(), [post, post], "program is post-fader");
    assert_eq!(graph.aux(0).expect("aux"), [0.5, 0.5], "pre-fader send");
}

#[test]
fn send_levels_and_switching_ramp() {
    let mut graph = mono(&with_aux(SendConfig::OFF, BusConfig::UNITY), 4);
    let input = [1.0; 4];
    graph.process(&[&input]);
    assert_eq!(graph.aux(0).expect("aux"), [0.0; 4], "off");
    graph.set_send(0, BusRef::Aux(0), send(0.0, Tap::PostFader));
    graph.process(&[&input]);
    assert_eq!(
        graph.aux(0).expect("aux"),
        [0.25, 0.5, 0.75, 1.0],
        "fades in"
    );
    graph.set_send(0, BusRef::Aux(0), send(-6.0, Tap::PostFader));
    graph.process(&[&input]);
    graph.process(&[&input]);
    let level = db_to_linear(-6.0);
    assert_eq!(graph.aux(0).expect("aux"), [level; 4]);
}

#[test]
fn a_tap_change_fades_out_then_in_without_a_jump() {
    let mut config = with_aux(send(0.0, Tap::PostFader), BusConfig::UNITY);
    config.channels[0].gain_db = -6.0;
    let mut graph = mono(&config, 4);
    let input = [1.0; 4];
    graph.process(&[&input]);
    let post = db_to_linear(-6.0);
    assert_eq!(graph.aux(0).expect("aux"), [post; 4]);
    graph.set_send(0, BusRef::Aux(0), send(0.0, Tap::PreFader));
    // Out on the old (post-fader) tap...
    graph.process(&[&input]);
    let out: Vec<f32> = [0.75, 0.5, 0.25, 0.0].iter().map(|g| g * post).collect();
    assert_eq!(graph.aux(0).expect("aux"), out.as_slice());
    // ...then in on the new (pre-fader) one.
    graph.process(&[&input]);
    assert_eq!(graph.aux(0).expect("aux"), [0.25, 0.5, 0.75, 1.0]);
    graph.process(&[&input]);
    assert_eq!(graph.aux(0).expect("aux"), [1.0; 4]);
}

#[test]
fn an_assigned_aux_bus_feeds_program_and_an_unassigned_one_does_not() {
    // Channel to program and to a pre-fader aux bus; the aux bus is a
    // separate mix at first.
    let mut config = with_aux(send(0.0, Tap::PreFader), BusConfig::UNITY);
    config.channels[0].gain_db = -6.0;
    let mut graph = mono(&config, 2);
    let input = [0.5, 0.5];
    graph.process(&[&input]);
    let post = 0.5 * db_to_linear(-6.0);
    assert_eq!(graph.program(), [post, post]);
    // Assigned at unity from the start of a graph: program gets both.
    config.aux_buses[0].assign_db = Some(0.0);
    let mut graph = mono(&config, 2);
    graph.process(&[&input]);
    assert_eq!(graph.program(), [post + 0.5, post + 0.5]);
}

#[test]
fn a_bus_fader_and_mute_shape_its_output() {
    let aux = BusConfig {
        gain_db: -6.0,
        ..BusConfig::UNITY
    };
    let mut graph = mono(&with_aux(send(0.0, Tap::PostFader), aux), 2);
    graph.process(&[&[1.0, 1.0]]);
    let level = db_to_linear(-6.0);
    assert_eq!(graph.aux(0).expect("aux"), [level, level]);
    graph.set_bus(BusRef::Aux(0), BusConfig { muted: true, ..aux });
    graph.process(&[&[1.0, 1.0]]);
    graph.process(&[&[1.0, 1.0]]);
    assert_eq!(graph.aux(0).expect("aux"), [0.0, 0.0]);
    // The channel still reaches program.
    assert_eq!(graph.program(), [1.0, 1.0]);
}

#[test]
fn balance_is_unity_at_the_centre_and_attenuates_the_other_side() {
    assert_eq!(balance_gains(0.0), (1.0, 1.0));
    assert_eq!(balance_gains(1.0), (0.0, 1.0));
    assert_eq!(balance_gains(-1.0), (1.0, 0.0));
    let (left, right) = balance_gains(0.5);
    assert!((left - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    assert_eq!(right, 1.0);
    assert_eq!(balance_gains(f32::NAN), (1.0, 1.0));
    assert_eq!(balance_gains(7.0), (0.0, 1.0));

    let mut config = plain(1);
    config.channels[0].balance = 1.0;
    let mut graph = AudioGraph::new(2, 2, &config);
    graph.process(&[&[0.5, 0.5, 0.5, 0.5]]);
    assert_eq!(graph.program(), [0.0, 0.5, 0.0, 0.5], "left silent");
}

#[test]
fn a_full_graph_runs_and_meters_every_part() {
    let channels = vec![ChannelConfig::UNITY; MAX_CHANNELS];
    let aux_buses = vec![
        BusConfig {
            assign_db: Some(-6.0),
            ..BusConfig::UNITY
        };
        MAX_AUX_BUSES
    ];
    let mut sends = SendMatrix::program_only(MAX_CHANNELS, MAX_AUX_BUSES);
    for channel in 0..MAX_CHANNELS {
        for aux in 0..MAX_AUX_BUSES {
            assert!(sends.set(channel, BusRef::Aux(aux), send(-12.0, Tap::PreFader)));
        }
    }
    assert!(!sends.set(MAX_CHANNELS, BusRef::Program, SendConfig::OFF));
    assert!(!sends.set(0, BusRef::Aux(MAX_AUX_BUSES), SendConfig::OFF));
    let config = GraphConfig {
        channels,
        aux_buses,
        program: BusConfig::UNITY,
        sends,
    };
    let mut graph = AudioGraph::new(2, 480, &config);
    let input = vec![0.01f32; 960];
    let inputs: Vec<&[f32]> = (0..MAX_CHANNELS).map(|_| input.as_slice()).collect();
    graph.process(&inputs);
    // Every aux bus: 8 channels at -12 dB.
    let aux = 8.0 * 0.01 * db_to_linear(-12.0);
    for index in 0..MAX_AUX_BUSES {
        let block = graph.aux(index).expect("aux");
        assert!((block[0] - aux).abs() < 1e-6, "{}", block[0]);
        assert!(
            graph
                .bus_meter(BusRef::Aux(index))
                .expect("meter")
                .peak_dbfs
                .is_some()
        );
    }
    // Program: 8 channels direct, plus 8 aux buses at -6 dB.
    let program = 8.0 * 0.01 + 8.0 * aux * db_to_linear(-6.0);
    assert!(
        (graph.program()[0] - program).abs() < 1e-5,
        "{}",
        graph.program()[0]
    );
    assert!(graph.channel_meter(7).expect("meter").peak_dbfs.is_some());
    assert!(graph.channel_block_peak(7).expect("peak") > 0.0);
}

#[test]
fn unknown_parts_are_refused() {
    let mut graph = mono(&plain(1), 2);
    assert!(!graph.set_channel(1, ChannelConfig::UNITY));
    assert!(!graph.set_bus(BusRef::Aux(0), BusConfig::UNITY));
    assert!(!graph.set_send(0, BusRef::Aux(0), SendConfig::OFF));
    assert!(!graph.set_send(1, BusRef::Program, SendConfig::OFF));
    assert!(graph.set_bus(BusRef::Program, BusConfig::UNITY));
}

#[test]
fn the_limit_is_applied_and_counted_where_a_bus_leaves_the_graph() {
    let mut graph = mono(&plain(2), 3);
    graph.process(&[&[0.75, -0.75, 0.25], &[0.5, -0.5, 0.25]]);
    // The bus itself is not limited: its meter shows the overload.
    assert_eq!(graph.program(), [1.25, -1.25, 0.5]);
    let mut out = [0.0; 3];
    assert_eq!(limit_into(graph.program(), &mut out), 2);
    assert_eq!(out, [1.0, -1.0, 0.5]);
}

/// Adds a constant: shows where in the chain a slot sits.
struct Offset(f32);

impl Processor for Offset {
    fn process(&mut self, block: &mut [f32], _channels: usize) {
        block.iter_mut().for_each(|sample| *sample += self.0);
    }
}

#[test]
fn a_channel_slot_runs_before_mute_and_both_taps() {
    let mut config = with_aux(send(0.0, Tap::PreFader), BusConfig::UNITY);
    config.channels[0].gain_db = -6.0;
    let mut graph = mono(&config, 2);
    *graph
        .channel_slot_mut(0, PROCESSING_SLOTS - 1)
        .expect("slot") = Some(Box::new(Offset(0.25)));
    assert!(graph.channel_slot_mut(0, PROCESSING_SLOTS).is_none());
    assert!(graph.channel_slot_mut(1, 0).is_none());
    graph.process(&[&[0.25, 0.25]]);
    assert_eq!(graph.aux(0).expect("aux"), [0.5, 0.5], "pre-fader tap");
    let post = 0.5 * db_to_linear(-6.0);
    assert_eq!(graph.program(), [post, post], "post-fader tap");
    // Mute comes after the slot: the offset does not get through either.
    graph.set_channel(
        0,
        ChannelConfig {
            muted: true,
            ..config.channels[0]
        },
    );
    graph.process(&[&[0.25, 0.25]]);
    graph.process(&[&[0.25, 0.25]]);
    assert_eq!(graph.aux(0).expect("aux"), [0.0, 0.0]);
    assert_eq!(graph.program(), [0.0, 0.0]);
}

#[test]
fn a_bus_slot_runs_on_the_sum_before_the_bus_fader() {
    let aux = BusConfig {
        gain_db: -6.0,
        assign_db: Some(0.0),
        muted: false,
    };
    let mut graph = mono(&with_aux(send(0.0, Tap::PreFader), aux), 2);
    *graph.bus_slot_mut(BusRef::Aux(0), 0).expect("slot") = Some(Box::new(Offset(0.5)));
    assert!(graph.bus_slot_mut(BusRef::Aux(1), 0).is_none());
    assert!(graph.bus_slot_mut(BusRef::Program, 0).is_some());
    graph.process(&[&[0.5, 0.5]]);
    let aux_out = (0.5 + 0.5) * db_to_linear(-6.0);
    assert_eq!(graph.aux(0).expect("aux"), [aux_out, aux_out]);
    // Program: the channel directly, plus the assigned aux bus.
    assert_eq!(graph.program(), [0.5 + aux_out, 0.5 + aux_out]);
}

#[test]
fn an_empty_graph_is_silent() {
    let mut graph = AudioGraph::new(2, 4, &plain(0));
    graph.process(&[&[1.0; 8]]);
    assert_eq!(graph.program(), [0.0; 8]);
    assert_eq!(
        graph.bus_meter(BusRef::Program).expect("meter").peak_dbfs,
        None
    );
}
