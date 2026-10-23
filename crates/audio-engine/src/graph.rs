//! The audio graph (ADR 0017): channels with processing slots, mute, fader
//! and balance; sends to buses, tapped before or after the fader; aux buses
//! that can be assigned to the program bus; and the program bus.
//!
//! ```text
//! channel: input ─► slots ─► mute ─┬─► fader ─► balance ─┬─► meter
//!                                  pre-fader tap      post-fader tap
//!                                  └──── sends (on/off · level · tap) ──► buses
//! bus:     Σ sends ─► slots ─► mute ─► fader ─► meter ─► (aux) assign to program
//! ```
//!
//! Mute comes before both taps: a muted channel is silent in every bus.
//! Two tiers of buses (aux, program) keep the graph acyclic by
//! construction. The shape is fixed when the graph is built; every
//! parameter changes live, ramped over one block so nothing clicks.
//!
//! Pure and deterministic: identical input gives bit-identical output, and
//! [`AudioGraph::process`] does not allocate.

use std::f32::consts::FRAC_PI_2;
use std::fmt;

use crate::mixer::{GainRamp, MAX_GAIN_DB, MIN_GAIN_DB, Meter, MeterReading, db_to_linear};

/// Most channels in a graph.
pub const MAX_CHANNELS: usize = 8;
/// Most aux buses in a graph (the program bus comes on top).
pub const MAX_AUX_BUSES: usize = 8;
/// Processing slots per channel and per bus, for effects to come.
pub const PROCESSING_SLOTS: usize = 4;

/// A channel strip's settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelConfig {
    pub gain_db: f32,
    /// Silences the channel before both taps: in every bus.
    pub muted: bool,
    /// −1 (left only) … 0 (centre, unity on both sides) … 1 (right only).
    pub balance: f32,
}

impl ChannelConfig {
    pub const UNITY: Self = Self {
        gain_db: 0.0,
        muted: false,
        balance: 0.0,
    };
}

/// A bus's settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BusConfig {
    pub gain_db: f32,
    pub muted: bool,
    /// For an aux bus: the level it feeds the program bus with, or `None`
    /// when it is a separate mix. Ignored for the program bus.
    pub assign_db: Option<f32>,
}

impl BusConfig {
    pub const UNITY: Self = Self {
        gain_db: 0.0,
        muted: false,
        assign_db: None,
    };
}

/// Where a send takes its signal from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tap {
    /// After processing and mute, before fader and balance.
    PreFader,
    /// After fader and balance.
    PostFader,
}

/// One connection from a channel to a bus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SendConfig {
    pub enabled: bool,
    pub level_db: f32,
    pub tap: Tap,
}

impl SendConfig {
    pub const OFF: Self = Self {
        enabled: false,
        level_db: 0.0,
        tap: Tap::PostFader,
    };
    /// A new channel's send to the program bus.
    pub const PROGRAM: Self = Self {
        enabled: true,
        level_db: 0.0,
        tap: Tap::PostFader,
    };
}

/// A bus a send goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusRef {
    Program,
    Aux(usize),
}

impl BusRef {
    /// The bus's column in a send matrix with `aux_buses` aux buses: the
    /// program bus first, then the aux buses. `None` for an unknown bus.
    pub fn column(self, aux_buses: usize) -> Option<usize> {
        match self {
            Self::Program => Some(0),
            Self::Aux(aux) if aux < aux_buses => Some(aux + 1),
            Self::Aux(_) => None,
        }
    }

    /// The bus in `column` (see [`BusRef::column`]).
    pub fn from_column(column: usize) -> Self {
        match column {
            0 => Self::Program,
            column => Self::Aux(column - 1),
        }
    }
}

/// The sends of every channel to every bus: a channel × (program + aux)
/// matrix, so a graph's shape fixes it.
#[derive(Debug, Clone, PartialEq)]
pub struct SendMatrix {
    channels: usize,
    aux_buses: usize,
    sends: Vec<SendConfig>,
}

impl SendMatrix {
    /// Every channel sends to the program bus ([`SendConfig::PROGRAM`]) and
    /// to no aux bus: the graph sounds like a plain mixer.
    pub fn program_only(channels: usize, aux_buses: usize) -> Self {
        let columns = aux_buses + 1;
        let sends = (0..channels * columns)
            .map(|index| {
                if index % columns == 0 {
                    SendConfig::PROGRAM
                } else {
                    SendConfig::OFF
                }
            })
            .collect();
        Self {
            channels,
            aux_buses,
            sends,
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn aux_buses(&self) -> usize {
        self.aux_buses
    }

    fn index(&self, channel: usize, bus: BusRef) -> Option<usize> {
        let column = bus.column(self.aux_buses)?;
        (channel < self.channels).then(|| channel * (self.aux_buses + 1) + column)
    }

    pub fn get(&self, channel: usize, bus: BusRef) -> Option<SendConfig> {
        self.index(channel, bus).map(|index| self.sends[index])
    }

    /// Set one send; `false` for a channel or bus outside the matrix.
    pub fn set(&mut self, channel: usize, bus: BusRef, send: SendConfig) -> bool {
        match self.index(channel, bus) {
            Some(index) => {
                self.sends[index] = send;
                true
            }
            None => false,
        }
    }
}

/// Everything a graph is built from.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphConfig {
    pub channels: Vec<ChannelConfig>,
    pub aux_buses: Vec<BusConfig>,
    pub program: BusConfig,
    /// Must be `channels` × `aux_buses` in shape.
    pub sends: SendMatrix,
}

impl GraphConfig {
    /// `channels` sending to the program bus only, as a plain mixer.
    pub fn plain(channels: Vec<ChannelConfig>, program_gain_db: f32) -> Self {
        let sends = SendMatrix::program_only(channels.len(), 0);
        Self {
            channels,
            aux_buses: Vec::new(),
            program: BusConfig {
                gain_db: program_gain_db,
                ..BusConfig::UNITY
            },
            sends,
        }
    }
}

/// An effect in a processing slot. None exist yet (ADR 0017); the slots fix
/// where effects sit relative to mute, fader and the taps.
pub trait Processor: Send {
    /// Process one interleaved block in place. Must not block or allocate.
    fn process(&mut self, block: &mut [f32], channels: usize);
}

/// The processing slots of a channel or bus.
struct Slots([Option<Box<dyn Processor>>; PROCESSING_SLOTS]);

impl Slots {
    fn empty() -> Self {
        Self(std::array::from_fn(|_| None))
    }

    fn run(&mut self, block: &mut [f32], channels: usize) {
        for processor in self.0.iter_mut().flatten() {
            processor.process(block, channels);
        }
    }
}

impl fmt::Debug for Slots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let used = self.0.iter().filter(|slot| slot.is_some()).count();
        write!(f, "Slots({used} of {PROCESSING_SLOTS} used)")
    }
}

/// Gains of the left and right side for `balance`: unity at the centre;
/// moving it attenuates the other side along a cosine curve, to silence at
/// the end.
pub fn balance_gains(balance: f32) -> (f32, f32) {
    let attenuated = |amount: f32| {
        if amount >= 1.0 {
            0.0
        } else {
            (amount * FRAC_PI_2).cos()
        }
    };
    let balance = if balance.is_finite() {
        balance.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    if balance >= 0.0 {
        (attenuated(balance), 1.0)
    } else {
        (1.0, attenuated(-balance))
    }
}

/// Linear gain for `db`, within [`MIN_GAIN_DB`, `MAX_GAIN_DB`]. A
/// non-finite level is silence, so it cannot turn a bus into NaN; commands
/// validate levels before they get here.
fn gain(db: f32) -> f32 {
    if db.is_finite() {
        db_to_linear(db.clamp(MIN_GAIN_DB, MAX_GAIN_DB))
    } else {
        0.0
    }
}

fn switch(on: bool) -> f32 {
    if on { 1.0 } else { 0.0 }
}

fn send_gain(send: SendConfig) -> f32 {
    if send.enabled {
        gain(send.level_db)
    } else {
        0.0
    }
}

#[derive(Debug)]
struct Channel {
    config: ChannelConfig,
    mute: GainRamp,
    fader: GainRamp,
    left: GainRamp,
    right: GainRamp,
    meter: Meter,
    /// Largest absolute post-fader sample of the last block.
    block_peak: f32,
    slots: Slots,
}

impl Channel {
    fn new(config: ChannelConfig) -> Self {
        let (left, right) = balance_gains(config.balance);
        Self {
            config,
            mute: GainRamp::new(switch(!config.muted)),
            fader: GainRamp::new(gain(config.gain_db)),
            left: GainRamp::new(left),
            right: GainRamp::new(right),
            meter: Meter::default(),
            block_peak: 0.0,
            slots: Slots::empty(),
        }
    }

    fn set(&mut self, config: ChannelConfig) {
        self.config = config;
        self.mute.set_target(switch(!config.muted));
        self.fader.set_target(gain(config.gain_db));
        let (left, right) = balance_gains(config.balance);
        self.left.set_target(left);
        self.right.set_target(right);
    }
}

#[derive(Debug)]
struct SendState {
    config: SendConfig,
    level: GainRamp,
    /// The tap in use. A new tap takes over only after the level ramped to
    /// silence on the old one, so switching does not click.
    tap: Tap,
}

impl SendState {
    fn new(config: SendConfig) -> Self {
        Self {
            config,
            level: GainRamp::new(send_gain(config)),
            tap: config.tap,
        }
    }

    fn set(&mut self, config: SendConfig) {
        self.config = config;
        self.retarget();
    }

    /// Ramp to the configured level, or to silence while a tap change is
    /// pending.
    fn retarget(&mut self) {
        let target = if self.config.tap == self.tap {
            send_gain(self.config)
        } else {
            0.0
        };
        self.level.set_target(target);
    }

    /// After a block: switch a pending tap once the old one is silent.
    fn finish_block(&mut self) {
        self.level.finish_block();
        if self.config.tap != self.tap && self.level.is_silent() {
            self.tap = self.config.tap;
            self.retarget();
        }
    }
}

#[derive(Debug)]
struct Bus {
    config: BusConfig,
    mute: GainRamp,
    fader: GainRamp,
    assign: GainRamp,
    meter: Meter,
    block_peak: f32,
    slots: Slots,
    /// The bus's block: the sum of its sends, then its output.
    block: Vec<f32>,
}

impl Bus {
    fn new(config: BusConfig, block_len: usize) -> Self {
        Self {
            config,
            mute: GainRamp::new(switch(!config.muted)),
            fader: GainRamp::new(gain(config.gain_db)),
            assign: GainRamp::new(config.assign_db.map_or(0.0, gain)),
            meter: Meter::default(),
            block_peak: 0.0,
            slots: Slots::empty(),
            block: vec![0.0; block_len],
        }
    }

    fn set(&mut self, config: BusConfig) {
        self.config = config;
        self.mute.set_target(switch(!config.muted));
        self.fader.set_target(gain(config.gain_db));
        self.assign.set_target(config.assign_db.map_or(0.0, gain));
    }

    /// Slots, mute and fader on the summed block, in place; meter it.
    fn finish(&mut self, channels: usize) {
        let frames = self.block.len() / channels;
        self.slots.run(&mut self.block, channels);
        self.block_peak = 0.0;
        for (frame, samples) in self.block.chunks_exact_mut(channels).enumerate() {
            let gain = self.mute.at(frame, frames) * self.fader.at(frame, frames);
            for sample in samples {
                *sample *= gain;
                self.meter.observe(*sample);
                self.block_peak = self.block_peak.max(sample.abs());
            }
        }
        self.mute.finish_block();
        self.fader.finish_block();
    }
}

/// The audio graph of one run. See the module documentation.
#[derive(Debug)]
pub struct AudioGraph {
    /// Interleaved audio channels per frame (2: stereo).
    width: usize,
    channels: Vec<Channel>,
    aux: Vec<Bus>,
    program: Bus,
    /// Row-major: channel × (program + aux buses).
    sends: Vec<SendState>,
    pre: Vec<f32>,
    post: Vec<f32>,
    invalid_samples: u64,
}

impl AudioGraph {
    /// A graph for blocks of `block_frames` frames of `width` interleaved
    /// samples, built from `config`. Panics if the send matrix does not
    /// match the channels and buses, or a limit is exceeded; callers
    /// validate configurations before building.
    pub fn new(width: usize, block_frames: usize, config: &GraphConfig) -> Self {
        assert!(config.channels.len() <= MAX_CHANNELS, "too many channels");
        assert!(
            config.aux_buses.len() <= MAX_AUX_BUSES,
            "too many aux buses"
        );
        assert_eq!(
            (config.sends.channels(), config.sends.aux_buses()),
            (config.channels.len(), config.aux_buses.len()),
            "send matrix shape"
        );
        let width = width.max(1);
        let block_len = block_frames * width;
        Self {
            width,
            channels: config.channels.iter().copied().map(Channel::new).collect(),
            aux: config
                .aux_buses
                .iter()
                .map(|bus| Bus::new(*bus, block_len))
                .collect(),
            program: Bus::new(config.program, block_len),
            sends: config
                .sends
                .sends
                .iter()
                .copied()
                .map(SendState::new)
                .collect(),
            pre: vec![0.0; block_len],
            post: vec![0.0; block_len],
            invalid_samples: 0,
        }
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    pub fn aux_bus_count(&self) -> usize {
        self.aux.len()
    }

    /// Change channel `index` from the next block on; `false` if unknown.
    pub fn set_channel(&mut self, index: usize, config: ChannelConfig) -> bool {
        self.channels
            .get_mut(index)
            .map(|channel| channel.set(config))
            .is_some()
    }

    /// Change a bus from the next block on; `false` if unknown.
    pub fn set_bus(&mut self, bus: BusRef, config: BusConfig) -> bool {
        match bus {
            BusRef::Program => {
                self.program.set(config);
                true
            }
            BusRef::Aux(index) => self.aux.get_mut(index).map(|aux| aux.set(config)).is_some(),
        }
    }

    /// Change a send from the next block on; `false` if unknown.
    pub fn set_send(&mut self, channel: usize, bus: BusRef, config: SendConfig) -> bool {
        match bus.column(self.aux.len()) {
            Some(column) if channel < self.channels.len() => {
                self.sends[channel * (self.aux.len() + 1) + column].set(config);
                true
            }
            _ => false,
        }
    }

    /// Processing slot `slot` of channel `index`: after the input check,
    /// before mute and both taps. Swap a processor in with
    /// [`std::mem::replace`] and drop the old one off the audio thread.
    pub fn channel_slot_mut(
        &mut self,
        index: usize,
        slot: usize,
    ) -> Option<&mut Option<Box<dyn Processor>>> {
        self.channels
            .get_mut(index)
            .and_then(|channel| channel.slots.0.get_mut(slot))
    }

    /// Processing slot `slot` of a bus: after the sum of its sends, before
    /// its mute and fader.
    pub fn bus_slot_mut(
        &mut self,
        bus: BusRef,
        slot: usize,
    ) -> Option<&mut Option<Box<dyn Processor>>> {
        let bus = match bus {
            BusRef::Program => &mut self.program,
            BusRef::Aux(index) => self.aux.get_mut(index)?,
        };
        bus.slots.0.get_mut(slot)
    }

    pub fn channel_config(&self, index: usize) -> Option<ChannelConfig> {
        self.channels.get(index).map(|channel| channel.config)
    }

    pub fn bus_config(&self, bus: BusRef) -> Option<BusConfig> {
        match bus {
            BusRef::Program => Some(self.program.config),
            BusRef::Aux(index) => self.aux.get(index).map(|aux| aux.config),
        }
    }

    /// Process one block. `inputs[i]` is channel `i`'s input, interleaved
    /// and exactly one block long; a missing input is silence. Non-finite
    /// input samples become silence and are counted, so they cannot reach
    /// any bus; an input of another length is silence for the block, and
    /// all its samples are counted as invalid.
    pub fn process(&mut self, inputs: &[&[f32]]) {
        let width = self.width;
        let frames = self.pre.len() / width;
        let columns = self.aux.len() + 1;
        for bus in self
            .aux
            .iter_mut()
            .chain(std::iter::once(&mut self.program))
        {
            bus.block.fill(0.0);
        }
        for (index, channel) in self.channels.iter_mut().enumerate() {
            let input = inputs.get(index).copied().unwrap_or(&[]);
            if input.len() == self.pre.len() {
                for (sample, &value) in self.pre.iter_mut().zip(input) {
                    *sample = if value.is_finite() {
                        value
                    } else {
                        self.invalid_samples += 1;
                        0.0
                    };
                }
            } else {
                // Not one block: rejected as a whole, never cut or padded.
                self.pre.fill(0.0);
                self.invalid_samples += input.len() as u64;
            }
            channel.slots.run(&mut self.pre, width);
            channel.block_peak = 0.0;
            for (frame, (pre, post)) in self
                .pre
                .chunks_exact_mut(width)
                .zip(self.post.chunks_exact_mut(width))
                .enumerate()
            {
                let mute = channel.mute.at(frame, frames);
                let fader = channel.fader.at(frame, frames);
                let (left, right) = (
                    channel.left.at(frame, frames),
                    channel.right.at(frame, frames),
                );
                for (side, (pre, post)) in pre.iter_mut().zip(post.iter_mut()).enumerate() {
                    *pre *= mute;
                    // Balance acts on a stereo pair; other widths pass.
                    let balance = match (width, side) {
                        (2, 0) => left,
                        (2, _) => right,
                        _ => 1.0,
                    };
                    *post = *pre * fader * balance;
                    channel.meter.observe(*post);
                    channel.block_peak = channel.block_peak.max(post.abs());
                }
            }
            for ramp in [
                &mut channel.mute,
                &mut channel.fader,
                &mut channel.left,
                &mut channel.right,
            ] {
                ramp.finish_block();
            }
            let row = &mut self.sends[index * columns..(index + 1) * columns];
            for (column, send) in row.iter_mut().enumerate() {
                if !send.level.is_silent() {
                    let source = match send.tap {
                        Tap::PreFader => &self.pre,
                        Tap::PostFader => &self.post,
                    };
                    let bus = if column == 0 {
                        &mut self.program.block
                    } else {
                        &mut self.aux[column - 1].block
                    };
                    for (frame, (dst, src)) in bus
                        .chunks_exact_mut(width)
                        .zip(source.chunks_exact(width))
                        .enumerate()
                    {
                        let gain = send.level.at(frame, frames);
                        for (d, s) in dst.iter_mut().zip(src) {
                            *d += s * gain;
                        }
                    }
                }
                send.finish_block();
            }
        }
        for aux in &mut self.aux {
            aux.finish(width);
            if !aux.assign.is_silent() {
                for (frame, (dst, src)) in self
                    .program
                    .block
                    .chunks_exact_mut(width)
                    .zip(aux.block.chunks_exact(width))
                    .enumerate()
                {
                    let gain = aux.assign.at(frame, frames);
                    for (d, s) in dst.iter_mut().zip(src) {
                        *d += s * gain;
                    }
                }
            }
            aux.assign.finish_block();
        }
        self.program.finish(width);
    }

    /// The program bus's last block (not limited).
    pub fn program(&self) -> &[f32] {
        &self.program.block
    }

    /// Aux bus `index`'s last block (not limited).
    pub fn aux(&self, index: usize) -> Option<&[f32]> {
        self.aux.get(index).map(|aux| aux.block.as_slice())
    }

    /// Largest absolute post-fader sample of channel `index` in the last
    /// block (linear).
    pub fn channel_block_peak(&self, index: usize) -> Option<f32> {
        self.channels.get(index).map(|channel| channel.block_peak)
    }

    /// Largest absolute sample of a bus in the last block (linear).
    pub fn bus_block_peak(&self, bus: BusRef) -> Option<f32> {
        match bus {
            BusRef::Program => Some(self.program.block_peak),
            BusRef::Aux(index) => self.aux.get(index).map(|aux| aux.block_peak),
        }
    }

    /// Channel `index`'s post-fader level since the start.
    pub fn channel_meter(&self, index: usize) -> Option<MeterReading> {
        self.channels
            .get(index)
            .map(|channel| channel.meter.reading())
    }

    /// A bus's level since the start.
    pub fn bus_meter(&self, bus: BusRef) -> Option<MeterReading> {
        match bus {
            BusRef::Program => Some(self.program.meter.reading()),
            BusRef::Aux(index) => self.aux.get(index).map(|aux| aux.meter.reading()),
        }
    }

    /// Input samples replaced by silence so far: non-finite ones, and those
    /// of inputs that were not one block long.
    pub fn invalid_samples(&self) -> u64 {
        self.invalid_samples
    }
}

/// What [`limit_into`] changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limited {
    /// Samples beyond full scale, clamped to it.
    pub clipped: u64,
    /// Non-finite samples (an overflow in the graph, a processor's fault),
    /// replaced by silence.
    pub invalid: u64,
}

/// Copy `block` into `out` where a bus leaves the graph for an output:
/// hard-limited to `[-1, 1]`, with non-finite samples silenced, so an
/// output only ever receives valid samples. Counts both.
pub fn limit_into(block: &[f32], out: &mut [f32]) -> Limited {
    let mut limited = Limited::default();
    for (dst, &src) in out.iter_mut().zip(block) {
        *dst = if !src.is_finite() {
            limited.invalid += 1;
            0.0
        } else if src.abs() > 1.0 {
            limited.clipped += 1;
            src.clamp(-1.0, 1.0)
        } else {
            src
        };
    }
    limited
}

#[cfg(test)]
mod tests;
