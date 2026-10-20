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

`.github/workflows/ci.yml` runs on every push to `main` and on every pull request. Windows minutes cost twice and macOS minutes ten times as much as Linux minutes, so a `plan` job picks the platforms:
- **Linux, Windows and macOS:** pushes to `main`, manual runs (`workflow_dispatch`), and pull requests labeled `cross-platform`.
- **Linux only:** every other pull request.

Label a pull request `cross-platform` when it touches platform code (capture, encoding, build), and before merging anything risky for Windows or macOS.

What runs:

- `rustfmt` check on Linux.
- On each picked platform (`ubuntu-latest`, `windows-latest`, `macos-15-intel`; no fail-fast). The arm64 `macos-latest` runner was swapped out after jobs repeatedly went unscheduled for lack of capacity:
  - `clippy -D warnings`
  - build and tests with `--locked`
  - the M0 acceptance command (`streamforge-cli status --json`)
  - a stdio smoke test of `streamforge-engine`

The workflow uses the current Rust stable from `rustup` and logs the exact toolchain version. Its only third-party code is `actions/checkout`, pinned to a commit SHA. Jobs have timeouts and read-only repository permissions.

CI installs FFmpeg on every OS, so the recording, streaming and output acceptance tests run natively on all three:
- **Linux:** `libpipewire-0.3-dev` and `libclang-dev` for `capture-linux`, and `libavcodec-dev`, `libavformat-dev`, `libavutil-dev` and `libswscale-dev` for `encoder-ffmpeg`.
- **macOS:** `brew install ffmpeg pkgconf`.
- **Windows:** BtbN's shared FFmpeg 8.1, with its SHA-256 checked against the release's checksum file.

The runners have no desktop session and no microphone. Capture tests there exercise the "unavailable with reason" and `not_found` paths, and a CI step prints each runner's capture capabilities (`docs/platforms.md`).

A newer stable toolchain can bring new clippy lints. Before pushing, you can run `cargo +<newer> clippy --workspace --all-targets -- -D warnings` locally.

Tests that run the 1080p60 pipeline and x264 encoders take turns within their test binary (a test-only lock). In parallel they overload small CI runners, the encoders fall seconds behind, and media is dropped. The drops are counted, but these tests assert that nothing is lost. Frame-accounting checks are one-sided: no frame lost silently, none invented. Compositor skip counts cover the whole pipeline run, not one output.

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
- `crates/encoder-api`: key redaction (`Debug`, redacted targets, validation messages), URL validation, reconnect backoff.
- `crates/encoder-ffmpeg/tests/stream.rs`: streaming against the in-process RTMP ingest (`crates/rtmp-test-server`):
  - streams arrive (codecs, size, keyframe first, A/V spans)
  - reconnecting to a second server on the same port restarts with a keyframe at 0
  - the output gives up after the attempts, with counted drops and a prompt stop
  - an unreachable server is a `Network` error without the key
  - a silent peer hits the I/O timeout instead of hanging
- `crates/engine` (Linux): streaming via the engine, and a recording that continues when the stream fails.
- `crates/streamforge-engine/tests/streaming.rs`: M6 acceptance through the real engine process. It streams to a local server, and a recording outlives a stream that fails for good; the recorded file is checked.
- `crates/video-compositor`: live scene updates (keep unchanged sources, retire removed ones, reject a canvas change) and the preview downscaler.
- `crates/audio-engine`: live gain/mute changes and peak levels of the running mixer.
- `crates/engine` (scenes):
  - scene rules (unique names, the active scene cannot be deleted)
  - live edits checked through preview pixels
  - rollback of a failing live change
  - mixer control with levels
  - a recording of a user scene
- `crates/command-api` (access): generated parameter schemas (objects, no `$ref`, enums, required fields), per-call access of `add_source`, `AccessPolicy` and `Gated`.
- `crates/engine` (`thread`): `EngineThread` serves calls from several threads; a stopped engine is an `internal` error.
- `crates/streamforge-mcp/tests/mcp.rs`: M8 acceptance. The official rmcp client drives the real `streamforge-mcp` binary:
  - read-only default (listing, denial, unknown tool)
  - creating a test scene under `--allow control`; screen sources and `delete_scene` denied
  - streaming only with a configured target, redirects refused, and the key absent from everything the agent sees (Linux, in-process RTMP ingest)
  - startup checks of the target
- `crates/streamforge-engine/tests/stdio_protocol.rs`: `--allow read-only` denies a control command.
- `crates/capture-cpal`: the format converter (channels, resampling, chunked output, state across callbacks), tested on every OS. `tests/native.rs` (Windows, macOS) checks capabilities, captures from the default input if there is one or expects a structured error if not, and checks unknown inputs and unsupported layouts.
- `crates/streamforge-desktop/tests/acceptance.rs`: M7 acceptance through the UI model (the layer the widgets call), against the real engine on its worker thread. It creates a scene, adds a source, activates it and records, and checks that engine errors reach the log and that preview frames and levels arrive. The egui layer itself is checked by starting the app.
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
- `STREAMFORGE_STREAM_KEY=… cargo run --release -p streamforge-cli -- --output-dir /tmp stream --url rtmp://127.0.0.1:1935/live --duration-ms 60000 [--record both.mkv]` against a local ingest (e.g. `ffmpeg -listen 1 -i rtmp://127.0.0.1:1935/live/x -c copy -f flv out.flv`): check the dropped frames of both outputs and the compositor's late frames.
- Record CPU model, OS, build profile and thread count with every result (see ADR 0003).

## Reliability requirements

- Tests must have timeouts for process and device operations.
- Tests must clean up temporary media files.
- Device-dependent tests should be marked and report a clear skip reason if no device/session is available.
- Never use only screenshots as proof of media correctness.
- Record benchmark hardware, OS, driver, build mode, resolution, frame rate, and encoder settings.
