//! Audio engine: mixer core, test tones and the mixer thread.
//!
//! The mixer runs at a fixed 48 kHz stereo, interleaved `f32`, in 10 ms
//! blocks. Inputs arrive through bounded [`media_core::AudioRing`]s or are
//! generated (test tones). Independent of UI, capture and encoding.
//!
//! Policies (see `docs/media-model.md`):
//! - **Resampling:** inputs must already run at [`RATE`] with [`CHANNELS`]
//!   channels. Capture backends request that format and leave sample-rate and
//!   channel conversion to the platform (PipeWire's audioconvert on Linux).
//! - **Clock drift:** each live input is resampled by a tiny adaptive ratio
//!   (at most ±2000 ppm) that holds its buffer at a target fill, so drift
//!   causes neither gaps nor trims. Producer stalls are still padded with
//!   silence, and bursts far above target are trimmed; both are counted.
//! - **Clipping:** inputs are summed without intermediate clipping. The
//!   master bus is hard-limited to `[-1, 1]` after the master gain, and every
//!   limited sample is counted.

mod live;
pub mod mixer;
pub mod runner;
pub mod tone;

pub use live::MAX_CORRECTION;
pub use mixer::{
    GainRamp, MAX_GAIN_DB, MIN_GAIN_DB, Meter, MeterReading, Mixer, StripConfig, db_to_linear,
};
pub use runner::{
    BLOCK_FRAMES, BufferLatency, CHANNELS, InputSource, InputStats, MAX_INPUTS, MixStats,
    MixerConfig, MixerError, MixerInput, MixerThread, RATE,
};
pub use tone::Tone;
