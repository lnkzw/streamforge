# ADR 0004: Audio mixer clock, buffering and drift policy

Status: Accepted (M4)

## Context

M4 needs a mixer for at least two inputs, with gain, mute and meters, a defined clipping and resampling policy, deterministic offline tests, and a 10-minute live run that shows no unbounded latency growth.

Live inputs are produced on their device or graph clock (PipeWire). The mixer must also accept generated inputs (test tones) and later feed encoders, so it needs its own fixed block clock. Two clocks always drift apart, so the buffering between them must be bounded.

## Decision

- **Format:** fixed mixer format of 48 kHz, stereo, interleaved `f32`, in 10 ms blocks (480 frames). The mixer runs on its own `audio-mixer` thread with a monotonic block clock, and does not allocate after start-up.
- **Resampling:** delegated to the platform. Capture requests 48 kHz stereo, and PipeWire's audioconvert converts rate and channels. Inputs in any other format are not delivered and are counted.
- **Graph edge:** `media_core::AudioRing`, holding 1 s of audio; when full, the oldest frames are overwritten and counted.
- **Drift compensation:** adaptive resampling per live input (`audio-engine/src/live.rs`):
  - **Fill:** frames in the ring plus the local read-ahead, smoothed with an exponential moving average (time constant about 2 s) so the producer's quantum sawtooth averages out.
  - **Controller:** proportional. The ratio is `1 + 1e-6 * (fill - target)`, clamped to ±2000 ppm. The loop time constant is about 21 s, and the steady-state error is about 1 frame per ppm of drift.
  - **Interpolation:** linear between consecutive source frames, continuous across blocks. At ratio 1 the input passes through unchanged, delayed by one frame.
  - **Target:** the larger of two blocks and the largest recent push (current or previous 1 s window) plus one block, capped below the ring capacity.
  - **Explicit events:** warm-up, silence padding with re-warm-up on producer stalls, and trimming of bursts above target + 4 blocks + push peak. All are counted. The report shows buffer latency per 10 s window and `drift_ppm`.
- **Clipping:** inputs are summed in `f32` without intermediate clipping. The master bus is hard-limited to [-1, 1] after the master gain, and every limited sample is counted. There is no limiter.
- **Gain and mute:** changes ramp linearly over one block.

## Measurements (development machine, 2026-10-19, release build)

Two 10-minute live runs mixed the built-in analog input (PipeWire, quantum 1024, no microphone connected) with a 1 kHz tone at −12 dBFS. The first run used the initial pad/trim policy; the second used the adaptive resampler that was adopted after review.

| Metric | Pad/trim only | Adaptive resampling (adopted) |
|--------|---------------|-------------------------------|
| Blocks mixed | 60 000; 0 late, 0 skipped | 60 000; 0 late, 0 skipped |
| Mix time | 17 µs mean, 0.25 ms max | 19 µs mean, 0.12 ms max |
| Master | −12.00 dBFS peak, 0 clipped | −12.00 dBFS peak, 0 clipped |
| Buffer latency per 10 s window | 31.3 ms falling to 20.3 ms (drained by drift) | 38.7 ms at start, settled within about 50 s, then **flat at 30.30–30.56 ms** for 9 minutes |
| Underruns | 768 frames (16 ms) of silence in short gaps | **0** |
| Overruns / trims | 0 / 0 | 0 / 0 |
| Drift | device ≈ 46 ppm slow (from the frame deficit) | `drift_ppm` −36.5 at the end; frame deficit ≈ 45 ppm |

Offline simulations (`live.rs` tests) cover:
- −46 ppm and +200 ppm producers over 5 minutes: no underruns or trims, drift estimated within ±5 / ±10 ppm, step error of an interpolated ramp below 0.01, fill held near the target
- a 300 ms producer stall, which causes one underrun and then recovery
- the target following recent pushes only, and an oversized push that cannot block warm-up

Offline acceptance (`crates/audio-engine/tests/offline_mix.rs`):
- two tones mix to their bit-exact sum, and the RMS matches theory to 1e-3 dB
- output is bit-identical across runs
- gain and mute are exact
- clipping is counted exactly

## Consequences

- Latency is bounded and flat over time, which is the M4 acceptance criterion. Drift no longer causes gaps or trims.
- Linear interpolation is a time-varying low-pass while the ratio differs from 1. As the fractional position sweeps (about every 0.5 s at 40 ppm), high frequencies are attenuated by up to cos(π f / 48 kHz) at mid-position: about −2 dB at 10 kHz and −12 dB at 20 kHz. That is acceptable for monitoring and for M4. Recordings (M5) should switch to a windowed-sinc or polyphase resampler.
- Very fast drift beyond ±2000 ppm (a broken clock) is outside the correction range and still ends in counted trims or underruns; the probe warns when the correction is near its limit.
- Hard limiting is audible when inputs are hot; add a lookahead limiter once recordings exist (M5).
- The ring uses a short mutex section. Producers are PipeWire main-loop callbacks, not RT threads. Moving to `RT_PROCESS` callbacks requires a lock-free ring.
