//! Measures the audio graph's cost per block (ADR 0017).
//!
//! ```text
//! cargo run --release -p audio-engine --example graph_bench [blocks]
//! ```
//!
//! Graphs: two channels into the program bus (today's default), and the
//! full graph of 8 channels with 8 aux buses, every send on, half of them
//! pre-fader, every aux bus assigned to program, balance off centre. A
//! block is 480 stereo frames (10 ms at 48 kHz).

use std::time::{Duration, Instant};

use audio_engine::{
    AudioGraph, BLOCK_FRAMES, BusConfig, BusRef, CHANNELS, ChannelConfig, GraphConfig,
    MAX_AUX_BUSES, MAX_CHANNELS, SendConfig, SendMatrix, Tap, Tone, limit_into,
};

const WIDTH: usize = CHANNELS as usize;

fn main() {
    let blocks: usize = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(10_000);
    run(
        "default 2 ch -> program",
        &GraphConfig::plain(vec![ChannelConfig::UNITY; 2], 0.0),
        blocks,
    );
    run("full 8 ch x 8 aux", &full(), blocks);
}

fn full() -> GraphConfig {
    let mut sends = SendMatrix::program_only(MAX_CHANNELS, MAX_AUX_BUSES);
    for channel in 0..MAX_CHANNELS {
        for aux in 0..MAX_AUX_BUSES {
            let tap = if (channel + aux) % 2 == 0 {
                Tap::PreFader
            } else {
                Tap::PostFader
            };
            sends.set(
                channel,
                BusRef::Aux(aux),
                SendConfig {
                    enabled: true,
                    level_db: -12.0,
                    tap,
                },
            );
        }
    }
    GraphConfig {
        channels: vec![
            ChannelConfig {
                gain_db: -3.0,
                muted: false,
                balance: 0.25,
            };
            MAX_CHANNELS
        ],
        aux_buses: vec![
            BusConfig {
                gain_db: -6.0,
                muted: false,
                assign_db: Some(-6.0),
            };
            MAX_AUX_BUSES
        ],
        program: BusConfig::UNITY,
        sends,
    }
}

fn run(label: &str, config: &GraphConfig, blocks: usize) {
    let block = BLOCK_FRAMES * WIDTH;
    let inputs: Vec<Vec<f32>> = (0..config.channels.len())
        .map(|index| {
            let mut samples = vec![0.0; block];
            Tone::new(220.0 * (index + 1) as f64, -24.0, audio_engine::RATE)
                .fill(&mut samples, WIDTH);
            samples
        })
        .collect();
    let views: Vec<&[f32]> = inputs.iter().map(Vec::as_slice).collect();
    let mut graph = AudioGraph::new(WIDTH, BLOCK_FRAMES, config);
    let mut out = vec![0.0; block];
    let mut times = Vec::with_capacity(blocks);
    let mut clipped = 0;
    for index in 0..blocks {
        // A parameter change every 10th block keeps the ramps busy.
        if index % 10 == 0 {
            graph.set_channel(
                0,
                ChannelConfig {
                    gain_db: -(((index / 10) % 12) as f32),
                    ..config.channels[0]
                },
            );
        }
        let start = Instant::now();
        graph.process(&views);
        clipped += limit_into(graph.program(), &mut out).clipped;
        times.push(start.elapsed());
    }
    times.sort_unstable();
    let total: Duration = times.iter().sum();
    let us = |duration: Duration| duration.as_secs_f64() * 1e6;
    let percentile = |p: f64| times[((times.len() - 1) as f64 * p) as usize];
    println!(
        "{label:<24} blocks={blocks} mean={:.1} us p99={:.1} us max={:.1} us clipped={clipped}  (block budget 10000 us)",
        us(total) / blocks as f64,
        us(percentile(0.99)),
        us(times[times.len() - 1]),
    );
}
