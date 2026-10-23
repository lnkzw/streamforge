//! M4 acceptance: deterministic offline mixing of test tones, through the
//! audio graph with its channels going straight to the program bus.

use audio_engine::{
    AudioGraph, BLOCK_FRAMES, BusRef, CHANNELS, ChannelConfig, GraphConfig, Meter, RATE, Tone,
    limit_into,
};

const CH: usize = CHANNELS as usize;
/// One second; both test frequencies complete whole cycles.
const BLOCKS: usize = RATE as usize / BLOCK_FRAMES;

fn strip(gain_db: f32, muted: bool) -> ChannelConfig {
    ChannelConfig {
        gain_db,
        muted,
        ..ChannelConfig::UNITY
    }
}

fn graph(channels: &[ChannelConfig], program_gain_db: f32) -> AudioGraph {
    AudioGraph::new(
        CH,
        BLOCK_FRAMES,
        &GraphConfig::plain(channels.to_vec(), program_gain_db),
    )
}

/// Render `blocks` blocks of each tone separately.
fn tones(specs: &[(f64, f64)], blocks: usize) -> Vec<Vec<f32>> {
    specs
        .iter()
        .map(|&(frequency, level)| {
            let mut tone = Tone::new(frequency, level, RATE);
            let mut samples = vec![0.0; blocks * BLOCK_FRAMES * CH];
            for block in samples.as_chunks_mut::<{ BLOCK_FRAMES * CH }>().0 {
                tone.fill(block, CH);
            }
            samples
        })
        .collect()
}

/// The program output, limited as it leaves the graph, and how many
/// samples the limit changed.
fn mix(graph: &mut AudioGraph, inputs: &[Vec<f32>]) -> (Vec<f32>, u64) {
    let block = BLOCK_FRAMES * CH;
    let mut out = vec![0.0; inputs[0].len()];
    let mut clipped = 0;
    for (index, chunk) in out.chunks_exact_mut(block).enumerate() {
        let views: Vec<&[f32]> = inputs
            .iter()
            .map(|input| &input[index * block..(index + 1) * block])
            .collect();
        graph.process(&views);
        clipped += limit_into(graph.program(), chunk).clipped;
    }
    (out, clipped)
}

fn rms_dbfs(samples: &[f32]) -> f64 {
    let mut meter = Meter::default();
    samples.iter().for_each(|s| meter.observe(*s));
    meter.reading().rms_dbfs.expect("non-silent")
}

#[test]
fn two_tones_mix_to_their_exact_sum() {
    let inputs = tones(&[(440.0, -6.0), (1000.0, -12.0)], BLOCKS);
    let mut graph = graph(&[strip(0.0, false), strip(0.0, false)], 0.0);
    let (out, clipped) = mix(&mut graph, &inputs);

    // Unity gains: bit-exact sum.
    for (i, sample) in out.iter().enumerate() {
        assert_eq!(*sample, inputs[0][i] + inputs[1][i], "sample {i}");
    }
    // Uncorrelated sines over whole cycles: RMS^2 = A^2/2 + B^2/2.
    let (a, b) = (10f64.powf(-6.0 / 20.0), 10f64.powf(-12.0 / 20.0));
    let expected = 10.0 * ((a * a + b * b) / 2.0).log10();
    assert!(
        (rms_dbfs(&out) - expected).abs() < 1e-3,
        "{}",
        rms_dbfs(&out)
    );
    let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
    assert!(f64::from(peak) <= a + b + 1e-6);
    assert_eq!(clipped, 0);
    assert!(
        (graph
            .channel_meter(0)
            .and_then(|m| m.peak_dbfs)
            .expect("peak")
            + 6.0)
            .abs()
            < 1e-3
    );
}

#[test]
fn mixing_is_deterministic() {
    let inputs = tones(&[(440.0, -6.0), (1000.0, -12.0)], BLOCKS);
    let config = [strip(-3.0, false), strip(2.5, false)];
    let (first, _) = mix(&mut graph(&config, -1.0), &inputs);
    let (second, _) = mix(&mut graph(&config, -1.0), &inputs);
    assert!(
        first
            .iter()
            .zip(&second)
            .all(|(a, b)| a.to_bits() == b.to_bits())
    );
}

#[test]
fn gain_halves_the_level_and_mute_removes_the_input() {
    let inputs = tones(&[(440.0, -6.0), (1000.0, -6.0)], BLOCKS);
    let half = -20.0 * 2f32.log10();
    let mut graph = graph(&[strip(0.0, true), strip(half, false)], 0.0);
    let (out, _) = mix(&mut graph, &inputs);
    // Input 0 is muted from the start; input 1 is exactly halved.
    let gain = audio_engine::db_to_linear(half);
    for (i, sample) in out.iter().enumerate() {
        assert_eq!(*sample, inputs[1][i] * gain, "sample {i}");
    }
    assert!((rms_dbfs(&out) - (-6.0 - 3.0103 - 6.0206)).abs() < 1e-2);
}

#[test]
fn full_scale_tones_are_limited_where_the_program_bus_leaves_the_graph() {
    let inputs = tones(&[(440.0, 0.0), (1000.0, 0.0)], BLOCKS);
    let mut graph = graph(&[strip(0.0, false), strip(0.0, false)], 0.0);
    let (out, clipped) = mix(&mut graph, &inputs);
    assert!(out.iter().all(|s| s.abs() <= 1.0));
    let over = inputs[0]
        .iter()
        .zip(&inputs[1])
        .filter(|(a, b)| (*a + *b).abs() > 1.0)
        .count() as u64;
    assert!(over > 0);
    assert_eq!(clipped, over);
    // The bus meter shows the overload the limit removed.
    let peak = graph
        .bus_meter(BusRef::Program)
        .and_then(|meter| meter.peak_dbfs)
        .expect("peak");
    assert!(peak > 0.0, "{peak}");
}
