# Testing Strategy

## Every change

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Test layers

- **Unit tests**: timestamp conversion, validation, scene transforms, audio math, queue policies.
- **Contract tests**: serialization and versioning of command/request/response types.
- **Integration tests**: headless command process, capture lifecycle, recording output.
- **Media tests**: inspect generated files with `ffprobe`; validate stream metadata, duration, timestamps, and A/V drift.
- **Performance tests**: FPS, frame latency, queue depth, dropped frames, audio underruns, CPU/GPU utilization.
- **Platform tests**: native capture tests on Linux/Wayland, Windows, and macOS.

## Continuous integration

`.github/workflows/ci.yml` runs on every push to `main` and on every pull request:

- `rustfmt` check on Linux.
- On `ubuntu-latest`, `windows-latest` and `macos-15-intel` (no fail-fast). The arm64 `macos-latest` runner was swapped out after jobs repeatedly went unscheduled for lack of capacity:
  - `clippy -D warnings`
  - build and tests with `--locked`
  - the M0 acceptance command (`streamforge-cli status --json`)
  - a stdio smoke test of `streamforge-engine`

The workflow uses the current Rust stable from `rustup` and logs the exact toolchain version. Its only third-party code is `actions/checkout`, pinned to a commit SHA. Jobs have timeouts and read-only repository permissions.

On Linux, CI installs `libpipewire-0.3-dev` and `libclang-dev` for `capture-linux`, and `libavcodec-dev`, `libavformat-dev`, `libavutil-dev` and `libswscale-dev` for `encoder-ffmpeg`. Ubuntu's FFmpeg includes libx264, so the recording tests run there too. The runners have no desktop session, so capture capability tests there exercise the "unavailable with reason" paths.

A newer stable toolchain can bring new clippy lints. Before pushing, you can run `cargo +<newer> clippy --workspace --all-targets -- -D warnings` locally.

## Where tests live

- `crates/command-api`: unit tests for registry consistency, parameter decoding and result serialization.
- `crates/control-protocol/src/*`: unit tests for bounded line reading and envelope encoding.
- `crates/control-protocol/tests/contract.rs`: wire-contract tests against a fake `CommandHandler`. These cover every error code, ID echoing, ordering and oversized lines.
- `crates/engine`: unit tests showing that every registered command is handled.
- `crates/capture-api`: unit tests for diagnostics math (FPS, intervals, sequence gaps, levels, frame deficit) and capability checks.
- `crates/capture-linux/src/*`: unit tests for SPA pod construction, format names and portal flag mapping. They run without a desktop session.
- `crates/capture-linux/tests/live.rs`: tests against the running session, `#[ignore]` by default. Run them with `cargo test -p capture-linux --test live -- --ignored --test-threads=1`; each one prints a skip reason when no PipeWire session exists. Screen capture needs a human in the picker, so it follows the manual procedure in `docs/capture-linux.md`.
- `crates/video-compositor`: pixel-exact renderer tests (alpha math, bilinear sampling, z-order, clipping, thread-count invariance), geometry, scene validation, the latency histogram, the render loop with static and live sources, PPM encoding.
- `crates/media-core`: frame geometry validation, pool bounds and recycling, mailbox replace counting, audio ring wrap-around, overrun and discard counting.
- `crates/audio-engine`: gain ramps, mute, clipping and NaN policy, meters, tone determinism, live-buffer warm-up/underrun/trim logic, and the mixer thread.
- `crates/audio-engine/tests/offline_mix.rs`: M4 offline acceptance:
  - two tones mix to their bit-exact sum, and the RMS matches theory
  - output is deterministic across runs
  - gain and mute are exact
  - clipping is counted
- `crates/encoder-ffmpeg/src/audio_fifo.rs`: audio timeline regrouping (gap fill, overlap trim, discontinuity restart).
- `crates/encoder-ffmpeg/tests/record.rs`: records a synthetic 2 s clip, then demuxes and decodes it. It checks:
  - stream metadata and color tags, duration and start timestamps
  - A/V end drift
  - decoded pixel colors and tone RMS
  - the end cut-off (`end_at`)
  - that a `pipe:` name becomes a file
  - that an unwritable path is an output error
- `crates/media-core`: `MediaTimestamp::rescale` rounding and overflow.
- `crates/engine` (Linux): the recording lifecycle, including:
  - `invalid_state` on a double start, on probes while recording, and on a stop without a recording
  - `not_found` for a missing output directory
  - no overwrite of existing files
  - finalization when the engine is dropped
- `crates/streamforge-engine/tests/common/mod.rs`: the shared process harness. Every wait is bounded and the child is killed on drop.
- `crates/streamforge-engine/tests/recording.rs`: M5 acceptance. It records 2 s through the real engine process over JSON lines, then decodes the file and checks:
  - codecs, size, frame rate, audio format and duration
  - A/V end drift below 50 ms
  - the color bars and the tone RMS
- `crates/streamforge-engine/tests/stdio_protocol.rs`: process integration tests. They spawn the real binary and talk JSON lines over pipes, with a 10 s timeout on every wait, and kill the child on failure.

## Performance measurements

- `cargo run --release -p video-compositor --example render_bench [frames]`: per-frame render time of the reference scene with 1 thread and the default thread count.
- `cargo run --release -p streamforge-cli -- --output-dir /tmp compose --duration-ms 5000 [--screen] [--snapshot out.ppm]`: the real render loop at 60 FPS, reporting late frames, skipped ticks and source drops.
- `cargo run --release -p streamforge-cli -- mix --input <name> --tone 1000@-12 --duration-ms 600000`: the M4 10-minute latency test; check `buffer.windows_ms` for growth.
- `cargo run --release -p streamforge-cli -- --output-dir /tmp record --file take.mkv --duration-ms 60000 [--screen] [--input <name>]`: a real recording; check `frames_dropped`, `blocks_dropped`, `gap_samples` and the compositor's late frames, then `ffprobe` the file.
- Record CPU model, OS, build profile and thread count with every result (see ADR 0003).

## Reliability requirements

- Tests must have timeouts for process and device operations.
- Tests must clean up temporary media files.
- Device-dependent tests should be marked and report a clear skip reason if no device/session is available.
- Never use only screenshots as proof of media correctness.
- Record benchmark hardware, OS, driver, build mode, resolution, frame rate, and encoder settings.
