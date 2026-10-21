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

`.github/workflows/ci.yml` runs on every push to `main`, on every pull request and on manual runs (`workflow_dispatch`), always on Linux, Windows and macOS. The repository is public, so standard runners cost nothing. (While it was private, pull requests ran on Linux only unless labeled `cross-platform`; that saved minutes but let Windows- and macOS-only code merge untested.)

What runs:

- `rustfmt` check on Linux.
- On each platform (`ubuntu-latest`, `windows-latest`, `macos-15-intel`; no fail-fast). The arm64 `macos-latest` runner was swapped out after jobs repeatedly went unscheduled for lack of capacity:
  - `clippy -D warnings`
  - build and tests with `--locked`
  - the M0 acceptance command (`streamforge-cli status --json`)
  - a stdio smoke test of `streamforge-engine`
- A `bench` job per platform, in parallel to the tests: a release build of `streamforge-bench`, run with 2 runs × 5 s per scenario. It prints the runner's figures and never fails the workflow (no thresholds: shared runners vary). Both jobs share their setup through the local composite action `.github/actions/build-setup`.

The workflow uses the current Rust stable from `rustup` and logs the exact toolchain version. Its only third-party code is `actions/checkout`, pinned to a commit SHA. Jobs have timeouts and read-only repository permissions.

CI installs FFmpeg on every OS, so the recording, streaming and output acceptance tests run natively on all three:
- **Linux:** `libpipewire-0.3-dev` and `libclang-dev` for `capture-linux`, and `libavcodec-dev`, `libavformat-dev`, `libavutil-dev` and `libswscale-dev` for `encoder-ffmpeg`.
- **macOS:** `brew install ffmpeg pkgconf`.
- **Windows:** BtbN's shared FFmpeg 8.1, with its SHA-256 checked against the release's checksum file.

The runners have no desktop session and no microphone. Capture tests there exercise the "unavailable with reason" and `not_found` paths, and a CI step prints each runner's capture capabilities (`docs/platforms.md`).

A newer stable toolchain can bring new clippy lints. Before pushing, you can run `cargo +<newer> clippy --workspace --all-targets -- -D warnings` locally.

Tests that run the 1080p60 pipeline and x264 encoders take turns within their test binary (a test-only lock). In parallel they overload small CI runners, the encoders fall seconds behind, and media is dropped. The drops are counted, but these tests assert that nothing is lost. Frame-accounting checks are one-sided: no frame lost silently, none invented. Compositor skip counts cover the whole pipeline run, not one output.

## Where tests live

- `crates/command-api`: unit tests for registry consistency, parameter decoding and result serialization (including the health report's JSON shape).
- Live health (ADR 0012):
  - `video-compositor` and `audio-engine` test that their snapshots follow the run and that a reader holding a snapshot never delays a frame or block.
  - `encoder-ffmpeg` tests that live output counters match the reports.
  - The engine's stream-failure test checks `get_health` while the stream has failed and the recording continues.
  - `streamforge-engine/tests/stdio_protocol.rs` checks `get_health` during a preview.
- Capture state and restart (ADR 0013): the engine's fake screen stream ends during a recording; `get_health` must show it `ended` with the reason while the recording runs on. `restart_source` must bring it back during the recording (new capture, frames keep coming, both captures reported), move it to another `source_id` (and change nothing when that start fails), restart a kept capture of a scene that is not rendered, refuse a picker source while an output is open, and refuse sources that do not capture. `FrameSink` and the cpal end rule (an error followed by no data) have unit tests, the latter on every OS; the native tests are listed below.
- `crates/control-protocol/src/*`: unit tests for bounded line reading and envelope encoding.
- `crates/control-protocol/tests/contract.rs`: wire-contract tests against a fake `CommandHandler`. These cover every error code, ID echoing, ordering and oversized lines.
- `crates/engine`: unit tests showing that every registered command is handled.
- `crates/capture-api`: unit tests for diagnostics math (FPS, intervals, sequence gaps, levels, frame deficit) and capability checks.
- `crates/capture-linux/src/*`: unit tests for SPA pod construction, format names and portal flag mapping. They run without a desktop session.
- `crates/capture-windows/src/ids.rs`: source id format (all OSes). `crates/capture-windows/tests/native.rs` (Windows): lists sources, captures a monitor and checks frame size, `running` state and the report, probes it, and checks structured errors for unknown ids and missing sources. It captures a PowerShell form it opens, kills its process, and expects the stream to report `ended` within 10 s.
- `crates/capture-macos/src/ids.rs`: source id format (all OSes). `crates/capture-macos/tests/native.rs` (macOS): lists sources and captures a display where screen recording is permitted (checking `running` state); otherwise checks `permission_denied` with its remedy. Checks structured errors for a missing source, the metadata cursor and malformed ids.
- `crates/streamforge-engine/tests/screen.rs`: records a monitor named by `list_screen_sources` from a scene, through the engine process. Where the backend lists no sources (Linux), or screen recording is not permitted (macOS CI), it checks the structured error instead.
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
- `crates/encoder-ffmpeg/src/ffmpeg/encoders.rs`: candidate order per OS and preference, and the rate-control options of every encoder.
- `crates/encoder-ffmpeg/tests/record.rs`: `software` records with libx264; `hardware` uses a hardware encoder or fails with every candidate's reason; `auto` falls back and reports what it skipped. With a GPU (e.g. VAAPI on the development machine) every recording test runs on the hardware encoder; CI runners have none, so there they cover libx264 and the fallback.
- `crates/encoder-ffmpeg/tests/record.rs` also records a synthetic 2 s clip, then demuxes and decodes it. It checks:
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
- `crates/streamforge-desktop/tests/acceptance.rs`: M7 acceptance through the UI model (the layer the widgets call), against the real engine on its worker thread. It creates a scene, adds a source, activates it and records, and checks that engine errors reach the log, that preview frames and levels arrive, and that health counts the recording while it runs. Unit tests in `model.rs` cover the health strip's entries and levels (outputs, a stopped pipeline, lost audio, ended captures) and each screen source's state (frame age, no frame yet, not rendered, stopped pipeline). The egui layer itself is checked by starting the app.
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

### Measurement harness (`streamforge-bench`, M10 step 7)

`cargo run --release -p streamforge-bench -- [--seconds 10] [--runs 3] [--video-encoder auto|software|hardware] [--scenario …] [--json]`

- **Scenarios:**
  - `record`: one recording
  - `record-stream`: a recording and a stream to an in-process RTMP ingest, from one pipeline
  - `record-polled` and `record-stream-polled`: the same, polling `get_health` every 100 ms
- **Method:**
  - Each run starts a fresh engine on the reference scene with the default tone, through the same commands every client uses. No capture backend is involved.
  - Runs alternate between scenarios, so drift of the machine spreads over all of them. Summaries are medians with half the spread (`±`).
  - The CPU time is the whole process (user plus system), measured between the outputs' start and stop: steady state, without encoder probing or finalizing. The stream scenarios include the in-process ingest, which only demuxes.
  - The render mean is the stable measure of rendering work. p95 follows scheduling jitter and varies more.
  - `dropped` is media lost while connected. `offline` is what a stream dropped while not connected; this includes the audio before its first video frame, dropped by design so both start together (one or two blocks).
- **Use:** compare numbers on one machine, before and after a change; record CPU model, OS and encoder with them.

**Baseline** (2026-10-22, development machine: Linux, Intel Core i5-13600KF with 20 threads, AMD Radeon RX 6800 with Mesa VAAPI, release build, 3 runs × 10 s). Run to run, CPU stayed within ±1 %, render mean within ±2 % and render p95 within ±6 %.

| Scenario | Encoder | CPU (% of one core) | Render mean | Render p95 | Skipped / late | Lost |
|---|---|---|---|---|---|---|
| record | `h264_vaapi` | 44.7 | 1.53 ms | 1.75 ms | 0 / 0 | 0 |
| record-polled | `h264_vaapi` | 44.4 | 1.53 ms | 1.65 ms | 0 / 0 | 0 |
| record-stream | `h264_vaapi` × 2 | 75.7 | 2.11 ms | 2.40 ms | 0 / 0 | 0 |
| record-stream-polled | `h264_vaapi` × 2 | 75.6 | 2.12 ms | 2.40 ms | 0 / 0 | 0 |
| record | `libx264` | 78.0 | 1.55 ms | 1.70 ms | 0 / 0 | 0 |
| record-polled | `libx264` | 77.7 | 1.54 ms | 1.65 ms | 0 / 0 | 0 |
| record-stream | `libx264` × 2 | 153.0 | 2.14 ms | 2.45 ms | 0 / 0 | 0 |
| record-stream-polled | `libx264` × 2 | 151.5 | 2.11 ms | 2.40 ms | 0 / 0 | 0 |

What the baseline shows:
- **Polling `get_health` every 100 ms costs nothing measurable:** the polled and unpolled scenarios differ by less than their run-to-run spread (ADR 0012).
- **A second output costs about 31 % of a core with VAAPI and 75 % with libx264.** Each output converts BGRA frames on the CPU (and uploads them for VAAPI) on its own.
- **A second output also raises the render mean from 1.53 to 2.11 ms**, although the scene is the same. The render time includes handing the frame to the outputs, and the engine's frame tap copies each 1920×1080 BGRA frame (8 MB) once per output on the render thread: about 0.6 ms per copy here. This is the copy and conversion work of the assessment (§6.1.1), and the baseline to measure it against.

### Other measurements

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
