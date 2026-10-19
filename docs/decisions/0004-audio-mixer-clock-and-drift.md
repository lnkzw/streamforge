# ADR 0004: Audio mixer clock, buffering and drift policy

Status: Accepted (M4)

## Context

M4 needs a mixer for at least two inputs, with gain, mute and meters, a defined clipping and resampling policy, deterministic offline tests, and a 10-minute live run that shows no unbounded latency growth.

Live inputs are produced on their device or graph clock (PipeWire). The mixer must also accept generated inputs (test tones) and later feed encoders, so it needs its own fixed block clock. Two clocks always drift apart, so the buffering between them must be bounded.

## Decision

- **Format:** fixed mixer format of 48 kHz, stereo, interleaved `f32`, in 10 ms blocks (480 frames). The mixer runs on its own `audio-mixer` thread with a monotonic block clock, and does not allocate after start-up.
- **Resampling:** delegated to the platform. Capture requests 48 kHz stereo, and PipeWire's audioconvert converts rate and channels. Inputs in any other format are not delivered and are counted.
- **Graph edge:** `media_core::AudioRing`, holding 1 s of audio; when full, the oldest frames are overwritten and counted.
- **Drift absorption:** bounded buffering per input. The target fill is the larger of two blocks and the largest producer push plus one block.
  - Warm-up until the target is reached.
  - Underruns are padded with silence; an input that runs dry warms up again.
  - Fill above target + 4 blocks is trimmed back to the target.
  - All three cases are counted. The report shows buffer latency per 10 s window.
- **Clipping:** inputs are summed in `f32` without intermediate clipping. The master bus is hard-limited to [-1, 1] after the master gain, and every limited sample is counted. There is no limiter.
- **Gain and mute:** changes ramp linearly over one block.

## Measurements (development machine, 2026-10-19, release build)

The 10-minute live run mixed the built-in analog input (PipeWire, quantum 1024, no microphone connected) with a 1 kHz tone at −12 dBFS:

| Metric | Value |
|--------|-------|
| Blocks mixed | 60 000 of 60 000; 0 late, 0 skipped |
| Mix time | 17 µs mean, 0.1 ms p99, 0.25 ms max |
| Master | −12.00 dBFS peak / −15.01 dBFS RMS; 0 clipped |
| Buffer latency per 10 s window | 31.3 ms at the start, falling slowly, stable at 20.3 ms for the last minutes; window range 20.3–31.3 ms |
| Overruns / trims | 0 / 0 |
| Underruns | 768 frames (16 ms) padded with silence in 10 min |
| Device clock vs mixer clock | device delivered 27.7 ms less than expected, ≈ 46 ppm slow |

Offline acceptance (`crates/audio-engine/tests/offline_mix.rs`):
- two tones mix to their bit-exact sum, and the RMS matches theory to 1e-3 dB
- output is bit-identical across runs
- gain and mute are exact
- clipping is counted exactly

## Consequences

- Latency is bounded by construction and stays stable over time, which is the M4 acceptance criterion.
- Drift is corrected with discontinuities: a slow device causes occasional short silence pads, a fast one causes trims. This is audible in recordings. The next step is adaptive resampling: adjust the input rate by ppm from the smoothed fill error. A cheaper step is sample slipping, which spreads single-frame insertions and deletions so the gaps are far less audible.
- Hard limiting is audible when inputs are hot; add a lookahead limiter once recordings exist (M5).
- The ring uses a short mutex section. Producers are PipeWire main-loop callbacks, not RT threads. Moving to `RT_PROCESS` callbacks requires a lock-free ring.
