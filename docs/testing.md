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
- On pushes to `main` and manual runs (not on pull requests), a `bench` job per platform, in parallel to the tests: a release build of `streamforge-bench`, run with 2 runs × 5 s per scenario. It prints the runner's figures; there are no thresholds, because shared runners vary. A failing measurement leaves a warning annotation and does not fail the job; a failing setup or build does. Both jobs share their setup through the local composite action `.github/actions/build-setup`.
- `release.yml` builds the Linux release packages (Ubuntu 24.04 on the runner, Arch Linux in a container) with `packaging/linux/package.sh`. The script checks that every linked library resolves and smoke-tests the command-line binaries (`--version`, `status`, a `get_status` request to the engine) before packing. Pull requests that change the packaging and manual runs build the packages as artifacts; a `v*` tag matching the workspace version creates a draft release.

The workflow uses the current Rust stable from `rustup` and logs the exact toolchain version. Its only third-party code is `actions/checkout`, pinned to a commit SHA. Jobs have timeouts and read-only repository permissions.

CI installs FFmpeg on every OS, so the recording, streaming and output acceptance tests run natively on all three:
- **Linux:** `libpipewire-0.3-dev` and `libclang-dev` for `capture-linux`, and `libavcodec-dev`, `libavformat-dev`, `libavutil-dev` and `libswscale-dev` for `encoder-ffmpeg`. The downloaded packages are cached per Ubuntu release and week (`actions/cache`), because the runners' mirror is at times too slow for the job timeout; apt fetches only packages that changed.
- **macOS:** `brew install ffmpeg pkgconf`.
- **Windows:** BtbN's shared FFmpeg 8.1, with its SHA-256 checked against the release's checksum file.

The runners have no desktop session and no microphone. Capture tests there exercise the "unavailable with reason" and `not_found` paths, and a CI step prints each runner's capture capabilities (`docs/platforms.md`).

A newer stable toolchain can bring new clippy lints. Before pushing, you can run `cargo +<newer> clippy --workspace --all-targets -- -D warnings` locally.

Tests that run the 1080p60 pipeline and x264 encoders take turns within their test binary (a test-only lock: `media_test_lock` in the engine crate, `media_lock` in the `streamforge-engine` tests, `MEDIA` in the MCP tests). In parallel they overload small CI runners, the encoders fall seconds behind, and media is dropped. Frame-accounting checks are one-sided: no frame lost silently, none invented. Compositor skip counts cover the whole pipeline run, not one output.

Even one at a time, shared runners do not always sustain 1080p60 with libx264. The CI bench shows skipped ticks and dropped frames on all three with two outputs, and the macOS VM at times encodes a third of the frames with one. So where streaming tests check the video's length, they allow for the frames counted as dropped or undelivered, which end the video early, but half a second of video must still arrive. Skipped ticks in the middle leave gaps instead, so they are not allowed for; a render stall right before the stop ends the video early without being counted, and only a short one (0.15 s) is tolerated. `streams_to_a_local_server` also bounds the length by the time from the start request to the stop reply, instead of a fixed duration. The recording acceptance test requires three quarters of the ticks encoded on Linux and Windows and a quarter on macOS, and no dropped or missing audio.

`TestServer::start` and `start_on` return once FFmpeg listens. FFmpeg first calls the input's interrupt callback while it waits for the connection, after binding and listening, and the callback tells the waiting thread. A port that was free when picked can be taken before FFmpeg binds it; the server then ends without listening, and `start` tries another port. Tests therefore start streaming once, without retries, so the first error is the one reported.

## Where tests live

- `crates/command-api`: unit tests for registry consistency, parameter decoding and result serialization (including the health report's JSON shape).
- Live health (ADR 0012):
  - `video-compositor` and `audio-engine` test that their snapshots follow the run and that a reader holding a snapshot never delays a frame or block.
  - `encoder-ffmpeg` tests that live output counters match the reports.
  - The engine's stream-failure test checks `get_health` while the stream has failed and the recording continues.
  - `streamforge-engine/tests/stdio_protocol.rs` checks `get_health` during a preview.
- Projects (ADR 0015): `project-file` unit tests cover the round trip, every check with its JSON path, the atomic write (a failure after the temporary file is complete keeps the old file) and the directory listing (`.json` files by name; other files, directories and broken links left out). Engine tests save and load across engines, refuse loading while the pipeline runs, leave everything as it was for an invalid file, and list only names the commands accept, at most 256 with the rest counted. The MCP test lists projects only with `--project-dir`. `stdio_protocol.rs` saves and loads through two engine processes with `--project-dir`; `streamforge-cli/tests/project.rs` runs `streamforge project validate`.
- Capture state and restart (ADR 0013): the engine's fake screen stream ends during a recording; `get_health` must show it `ended` with the reason while the recording runs on. `restart_source` must bring it back during the recording (new capture, frames keep coming, both captures reported), move it to another `source_id` (and change nothing when that start fails), restart a kept capture of a scene that is not rendered, refuse a picker source while an output is open, and refuse sources that do not capture. `FrameSink` and the cpal end rule (an error followed by no data) have unit tests, the latter on every OS; the native tests are listed below.
- `crates/control-protocol/src/*`: unit tests for bounded line reading and envelope encoding.
- `crates/control-protocol/tests/contract.rs`: wire-contract tests against a fake `CommandHandler`. These cover every error code, ID echoing, ordering and oversized lines.
- `crates/engine`: unit tests showing that every registered command is handled.
- `crates/capture-api`: unit tests for diagnostics math (FPS, intervals, sequence gaps, levels, frame deficit) and capability checks.
- `crates/capture-linux/src/*`: unit tests for SPA pod construction, format names, portal flag mapping, the audio node listing (sinks as outputs, their monitors as inputs with `loopback_of`), and playback: filling PipeWire buffers from the feed (the requested frames, then silence), unknown outputs, refused formats, and the error mapping. They run without a desktop session.
- `crates/capture-windows/src/ids.rs`: source id format (all OSes). `crates/capture-windows/tests/native.rs` (Windows): lists sources, captures a monitor and checks frame size, `running` state and the report, probes it, and checks structured errors for unknown ids and missing sources. It captures a PowerShell form it opens, kills its process, and expects the stream to report `ended` within 10 s.
- `crates/capture-macos/src/ids.rs`: source id format (all OSes). `crates/capture-macos/tests/native.rs` (macOS): lists sources and captures a display where screen recording is permitted (checking `running` state); otherwise checks `permission_denied` with its remedy. Checks structured errors for a missing source, the metadata cursor and malformed ids.
- `crates/streamforge-engine/tests/screen.rs`: records a monitor named by `list_screen_sources` from a scene, through the engine process. Where the backend lists no sources (Linux), or screen recording is not permitted (macOS CI), it checks the structured error instead.
- `crates/capture-linux/tests/live.rs`: tests against the running session, `#[ignore]` by default. Run them with `cargo test -p capture-linux --test live -- --ignored --test-threads=1`; each one prints a skip reason when no PipeWire session exists. They list inputs and outputs (every sink has its monitor), probe an input, refuse unknown inputs and outputs, and play a −20 dBFS tone into a null sink the test creates (so nothing is audible): the sink's monitor must capture it at that level, and removing the sink must end the playback, name the output in the reason and close its feed. A sink removed after the listing (so the cached name check passes) must fail with `not_found` instead of playing on the default sink. Screen capture needs a human in the picker, so it follows the manual procedure in `docs/capture-linux.md`.
- `crates/video-compositor`: pixel-exact renderer tests (alpha math, bilinear sampling, z-order, clipping, thread-count invariance), geometry, scene validation, the latency histogram, the render loop with static and live sources, PPM encoding.
- `crates/media-core`: frame geometry validation, pool bounds and recycling, mailbox replace counting, audio ring wrap-around, overrun and discard counting; the lock-free sample ring's order across the wrap, overrun, warm-up and underrun counting, and two threads passing 400 000 numbered frames in uneven chunks, each once and in order.
- `crates/audio-engine`: gain ramps, mute, clipping and NaN policy, meters, tone determinism, live-buffer warm-up/underrun/trim logic, and the mixer thread. The monitor's `PlaybackFeed` is simulated against devices drifting by up to ±500 ppm with quanta of 256 and 1024 frames for 120 s: no underruns, overruns or trims, the drift estimated, the fill near its target; a tone stays continuous; a mixer stall is counted and padded once; a device stall is trimmed, not overrun; off ramps out and keeps feeding.
- The monitor in the engine (`crates/engine/src/tests/monitor.rs`): a fake device pulls 256 frames at a time at exactly 48 kHz and records what it plays. It plays its bus beside a recording, switches bus and on/off live, and leaves the recording untouched; a device that does not open, or that ends during the run, only warns; a channel that captures the monitor device warns of feedback. Real playback devices come with the platform backends: PipeWire (step 5b) is tested natively on the development machine (see `capture-linux` below), cpal on Windows and macOS (step 5c) on CI and with a tone test on hardware (see `capture-cpal` below); CI runners usually have no audio devices.
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
- `crates/capture-cpal`: the format converters, tested on every OS: capture (channels, resampling, chunked output, state across callbacks) and playback (the feed read straight into a matching device in one read, channel mapping, resampling that reads exactly the feed's rate, interpolation, state across pulls and scratch steps). `tests/native.rs` (Windows, macOS) checks capabilities, captures from the default input if there is one or expects a structured error if not, and checks unknown inputs (a `.monitor` of an unknown output too) and unsupported layouts. It checks that each monitor input names a listed output (on Windows, every output has one) and, except on macOS, where the first process tap asks for a permission, captures a monitor while nothing plays if there is an output (the silence StreamForge plays keeps WASAPI's loopback delivering). For playback it lists the outputs, plays silence on the default output if there is one (the device must pull, and stopping closes the feed) or expects a structured error, and checks unknown outputs (`not_found` with the available names) and refused requests. `a_tone_plays_on_the_default_output_without_gaps` (`--ignored`, audible: −30 dBFS for 2 s) plays a tone on real hardware and expects no underrun; `the_default_outputs_monitor_captures_what_it_plays` (`--ignored`, audible: −30 dBFS for 1 s) captures the default output's monitor while it plays the tone there and expects a level: `cargo test -p capture-cpal --test native -- --ignored --nocapture`.
- `crates/streamforge-desktop/tests/acceptance.rs`: M7 acceptance through the UI model (the layer the widgets call), against the real engine on its worker thread. It creates a scene, adds a source, activates it and records, and checks that engine errors reach the log, that preview frames and levels arrive, and that health counts the recording while it runs. Unit tests in `model.rs` cover the health strip's entries and levels (outputs, a stopped pipeline, lost audio, ended captures, a monitor that ended while on) and each screen source's state (frame age, no frame yet, not rendered, stopped pipeline). Unit tests in `workspace.rs` cover what the workspace shows: the scene selection, the transport state from commands, status and health (with the timer running on between readings), the monitor fitted on whole pixels, meter ballistics (fall, peak hold, clip latch), the health summary, the monitor's device name (marked when the engine does not list it) and state (off, on, playing, ended), which shortcut a key press is (exactly its modifiers, Ctrl or Cmd; Backspace removes too) and where Up and Down move in a list. `tests/workspace.rs` drives the real UI with simulated input (`egui_kittest`, no rendering, so it runs on CI) against the real engine: selecting a source opens the inspector; stopping a recording asks first, cancelling keeps it, confirming writes the file; going live without a target asks for one; a project is saved, marked as changed and opened again over a confirmation; an existing name is replaced only through the save dialog's warning, and a name the engine would refuse cannot be saved; closing with unsaved changes asks, and can save first (the close and its cancellation are viewport commands of the frame), also over another dialog and with a change still on its way; a double-click opens a file once its check is back (that test steps 10 ms per input event, as kittest takes one frame per event); the stream's bitrate and encoder and the recording's encoder reach the engine; the monitor's device is chosen from the engine's outputs while idle, not while the preview runs, and a monitor switched on to a device that cannot play says why in the outputs dock. The keyboard is tested there too: Space switches the preview only while no control has focus; Ctrl+R and Ctrl+L start, and stopping by key asks (Escape cancels); Ctrl+1 … Ctrl+9 put a scene on program; Delete and Backspace ask before they remove the selected source; Ctrl+S saves (asking for a name the first time), Ctrl+Shift+S and Ctrl+O open their dialogs; no key acts behind a dialog or while a text field has focus; Up and Down move the selection in the focused scene and source lists and keep the focus there; Escape closes the inspector drawer; Tab goes through the docks in reading order and stops only on named controls; and a dock's edge resizes it when dragged. `tests/widgets.rs` checks the components the same way, among them that the arrow keys step a focused fader or slider without moving the focus. Its engine lists fake audio inputs and outputs (`tests/support`). The egui layer is also checked on renders: `tests/render.rs` drives the real model and engine and renders the app headless with wgpu at 1280×720, 1400×900 and 1920×1080, idle and live (a source selected, the inspector a drawer at 1280×720), one dialog, the project menu and the open and save dialogs, the stream settings, the monitor's device list and an ended monitor, plus a specimen of the design system. Rendering needs a GPU or a software adapter, so the tests are ignored by default; run them with `STREAMFORGE_UI_RENDER_DIR=<dir> cargo test -p streamforge-desktop --test render -- --ignored` and look at the PNG files.
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
  - `record-stream`: a recording and a stream to a local RTMP ingest, from one pipeline
  - `record-polled` and `record-stream-polled`: the same, polling `get_health` every 100 ms
- **Method:**
  - Each run starts a fresh engine on the reference scene with the default tone, through the same commands every client uses. No capture backend is involved.
  - One unmeasured warm-up run comes first. Each round runs every scenario, starting with a different one, so neither the machine's drift nor a fixed order favors one. Summaries are medians of the valid runs with half their spread (`±`).
  - **Measured window:** it starts 0.5 s after the outputs started and ends before they stop.
    - The process's CPU time (user plus system) covers the window.
    - Render and mix counters cover it too, from `get_health` readings at both ends. The window's render mean follows from the running means and frame counts.
    - Render p95, p99 and maximum cover the whole pipeline run, setup and flush included. They follow scheduling jitter and vary more than the mean.
  - **The ingest runs in a child process** (the same binary), so its receive and demux work is not measured.
  - **Lost media:** frames and audio blocks dropped or never delivered by any output, plus blocks the mixer skipped. `gaps` counts audio samples filled with silence or never delivered. `start` is what a stream dropped before its first video frame, by design so both start together (one or two audio blocks); it is not lost media.
  - **Invalid runs:** a stream that did not stay connected, or an ingest error, makes a run invalid. It is listed but left out of the summaries. A run that fails is listed, and the harness exits non-zero after printing what it measured.
  - The polled scenarios call `get_health` every 100 ms and serialize the result to JSON, as an adapter would.
- **Use:** compare numbers on one machine, before and after a change; record CPU model, OS and encoder with them. Measure on an idle machine: a game or a build running at the same time changes CPU and GPU (VAAPI) figures by more than the spread.

**Baseline** (2026-10-22, development machine: Linux, Intel Core i5-13600KF with 20 threads, AMD Radeon RX 6800 with Mesa VAAPI, release build, idle, 3 runs × 10 s). Run to run, CPU stayed within ±2 % and the render mean within ±2 %. Render p95 varied by ±1 to ±12 %: it covers the whole pipeline run, setup and flush included, so a few slow frames move it (one run at 2.0 ms instead of 1.65 ms).

| Scenario | Encoder | CPU (% of one core) | Render mean | Render p95 | Skipped / late | Lost |
|---|---|---|---|---|---|---|
| record | `h264_vaapi` | 44.7 | 1.53 ms | 1.70 ms | 0 / 0 | 0 |
| record-polled | `h264_vaapi` | 45.5 | 1.56 ms | 1.75 ms | 0 / 0 | 0 |
| record-stream | `h264_vaapi` × 2 | 74.4 | 2.14 ms | 2.30 ms | 0 / 0 | 0 |
| record-stream-polled | `h264_vaapi` × 2 | 75.9 | 2.15 ms | 2.45 ms | 0 / 0 | 0 |
| record | `libx264` | 78.2 | 1.54 ms | 1.65 ms | 0 / 0 | 0 |
| record-polled | `libx264` | 79.1 | 1.56 ms | 1.65 ms | 0 / 0 | 0 |
| record-stream | `libx264` × 2 | 153.8 | 2.16 ms | 2.45 ms | 0 / 0 | 0 |
| record-stream-polled | `libx264` × 2 | 152.2 | 2.13 ms | 2.45 ms | 0 / 0 | 0 |

11 of the 12 stream runs dropped one audio block before their first video frame (`start`, by design), the other none. One of the 26 runs, warm-up included, reported a late frame outside the measured window.

What the baseline shows:
- **Polling `get_health` every 100 ms costs less than the harness can resolve** (ADR 0012). The polled medians differ from the unpolled ones by +0.8, +1.4, +0.8 and −1.6 % of a core. The sign changes, and polling cannot make a run cheaper, so the differences are noise between scenarios, which is larger than the spread within one.
- **A second output costs about 30 % of a core with VAAPI and 76 % with libx264.** Each output converts BGRA frames on the CPU (and uploads them for VAAPI) on its own.
- **A second output also raises the render mean from about 1.54 to 2.15 ms**, although the scene is the same. The render time includes handing the frame to the outputs, and the engine's frame tap copies each 1920×1080 BGRA frame (8 MB) once per output on the render thread: about 0.6 ms per copy here. This is the copy and conversion work of the assessment (§6.1.1), and the baseline to measure it against.

### Other measurements

- `cargo run --release -p video-compositor --example render_bench [frames]`: per-frame render time of the reference scene with 1 thread and the default thread count.
- `cargo run --release -p audio-engine --example graph_bench [blocks]`: per-block cost of the audio graph (ADR 0017), for today's default (2 channels into program) and the full graph (8 channels, 8 aux buses, every send on, every aux bus assigned). On an i5-13600KF (Arch Linux, release build, 10 000 blocks): default 9.1 µs mean and 11.1 µs p99; full 88 µs mean and 94 µs p99, under 1 % of the 10 ms a block lasts.
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
