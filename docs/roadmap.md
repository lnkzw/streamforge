# Roadmap and Acceptance Criteria

## M0 — Workspace foundation

- [x] Cargo workspace builds in principle
- [x] CLI returns structured status
- [x] Basic timestamp type and tests
- [x] CI runs format, clippy, and tests on Linux, Windows, macOS (`.github/workflows/ci.yml`). First green run on `ubuntu-latest`, `windows-latest` and `macos-15-intel` (x64) on 2026-10-19, run 39309549487. macOS arm64 was not covered: `macos-latest` jobs were not scheduled for lack of runner capacity.

Acceptance:
- `cargo build --workspace`
- `cargo test --workspace`
- `cargo run -p streamforge-cli -- status --json`

## M1 — Command boundary

- [x] Define a command registry and structured result/error types (`command-api`).
- [x] Add a headless engine process (`streamforge-engine`, stdio).
- [x] Add a local JSON-lines protocol with request IDs (`control-protocol`, `docs/protocol.md`).
- [x] Keep the transport replaceable; do not expose internal Rust types as an accidental permanent protocol (ADR 0002).

Acceptance:
- [x] Integration test sends `get_status` and receives a correlated JSON response (`crates/streamforge-engine/tests/stdio_protocol.rs`).
- [x] Invalid commands return structured errors and do not crash the process (same file; contract cases in `crates/control-protocol/tests/contract.rs`).

Known limitations, deferred to later milestones:
- Only `get_status` and `list_commands` exist; status fields are static until a media graph exists.
- stdio only: one controlling client per engine process. A socket/named-pipe transport for a shared engine is not implemented.
- Requests are handled sequentially on the main thread; no controller thread or bounded command queue yet.
- Permission classes are declared but not enforced (M8).
- Tests run on Linux only so far. Clippy passes locally for `x86_64-pc-windows-msvc` and `x86_64-pc-windows-gnu`, which proves compilation, not behavior. macOS is unchecked. CI covers all three once the repository is hosted.

## M2 — Capture spike

- [x] Implement Linux screen capture through XDG ScreenCast portal + PipeWire (`capture-linux`, `probe_screen_capture`).
- [x] Enumerate and capture audio input through PipeWire (`list_audio_inputs`, `probe_audio_capture`).
- [x] Record runtime capability information (`get_capture_capabilities`).
- [x] Document Hyprland-specific backend requirements (`docs/capture-linux.md`).

Acceptance:
- Manual test captures a selected screen/window:
  - [x] Monitor capture verified 2026-10-19 on Hyprland 0.56.2 / xdph 1.4.1: 2560×1440 BGRx, PTS present, 0 drops.
  - [x] Picker cancellation verified 2026-10-19: `cancelled`, exit code 1.
  - [ ] Window capture: pending. A run with `--source window` delivered a monitor (2560×1440, 252 frames in 3 s, 84 FPS, 0 drops) because xdph's picker offers every type. The report now warns about such mismatches. Repeat the run and pick an entry in the picker's window tab.
- [x] Capture diagnostics report dimensions, timestamps (explicit time base), FPS, frame intervals and drops: sequence gaps, buffer starvation, empty and corrupted buffers, and the audio frame deficit.
- [x] Unsupported capabilities fail with a useful message:
  - `unsupported` with `details.available` for source types and cursor modes the portal does not offer.
  - `not_found` with alternatives for unknown inputs.
  - `unavailable` with a remedy when portal or PipeWire are missing.
  - Non-Linux builds report `unsupported` with the milestone that adds support.

Known limitations, deferred to later milestones:
- Probes block the engine thread; there is no continuous capture into a media graph yet (M3).
- No DMA-BUF import, restore tokens, multi-stream capture or cursor metadata.
- No desktop-audio (sink monitor) capture; only sources are listed.
- Windows/macOS capture backends: M9.

## M3 — First video compositor

- [x] Composite two sources with position, scale and alpha (`video-compositor`: scene graph, CPU bilinear renderer, premultiplied "over"; ADR 0003).
- [x] Keep render loop independent from UI (dedicated compositor thread with a fixed frame clock; continuous screen capture on its own thread feeding a `FrameMailbox`).
- [x] Add deterministic tests for transform and scene graph logic. Tests are pixel-exact and cover geometry, alpha, bilinear sampling, z-order, thread-count invariance, the latency histogram, the mailbox and the pool.

Acceptance:
- Test pattern + captured source render correctly:
  - [x] Test pattern and moving overlay: PPM snapshot inspected, pixel values match the blend math.
  - [x] Live screen layer verified 2026-10-19 (`compose --screen`, release build):
    - render: 300/300 frames at 59.9993 FPS, 0 late, 0 skipped, render time 1.97 ms mean and 2.6 ms p99, incl. scaling the 2560×1440 source
    - capture: 423 frames at 84.6 FPS, 0 dropped or discarded
    - mailbox: 122 frames replaced, about 25/s, as expected for 85 → 60 FPS
    - snapshot shows the layer fitted into its box at 90 % opacity
- [x] Target 1080p/60 measured on the development machine (i5-13600KF, release build). Without screen: 300/300 frames at 59.9994 FPS, 0 late, render time 0.84 ms mean, 1.2 ms p99. Synthetic 3-layer benchmark incl. a scaled 1440p source: 1.71 ms mean with 8 threads, 6.50 ms with 1 thread. Details in ADR 0003.

Known limitations, deferred to later milestones:
- No GPU path or DMA-BUF import; screen frames are copied into pooled CPU buffers.
- No color management; blending happens on sRGB-encoded values.
- The scene is fixed inside `probe_composite`; editing scenes through commands is M7.
- The composite probe blocks the engine thread for its duration, like the capture probes.

## M4 — Audio mixer

- [x] Mix at least two inputs: up to 8 live inputs and test tones, on the `audio-mixer` thread (`audio-engine`, `probe_audio_mix`).
- [x] Add gain, mute and peak/RMS meters, with gain ramps and per-input and master meters.
- [x] Define clipping and resampling policy (`docs/media-model.md`): the master bus is hard-limited with counting; PipeWire does rate and channel conversion; clock drift is absorbed by bounded buffering, with trims and pads counted.

Acceptance:
- [x] Offline deterministic test tone mixing (`crates/audio-engine/tests/offline_mix.rs`):
  - the sum is bit-exact and the RMS matches theory
  - output is bit-identical across runs
  - gain, mute and clipping counts are exact
- [x] Live test shows no unbounded latency growth during a 10-minute run (2026-10-19, built-in input + 1 kHz tone, release, adaptive resampling):
  - 60 000/60 000 blocks, 0 late
  - buffer latency settled within about 50 s and stayed flat at 30.30–30.56 ms for 9 minutes
  - 0 underruns, 0 trims, 0 overruns
  - device drift tracked at about −40 ppm (ADR 0004; the earlier pad/trim run is recorded there for comparison)

Known limitations, deferred:
- Drift resampling uses linear interpolation, which slightly modulates high frequencies (up to about −2 dB at 10 kHz). Recordings in M5 should use a windowed-sinc or polyphase resampler (ADR 0004).
- The master bus uses a hard limit with counting, no limiter.
- The mixer output has no consumer yet; recording comes in M5.
- Desktop audio (sink monitor) capture is not implemented.
- The mix probe blocks the engine thread for its duration (up to 15 min).

## M5 — First recording

- [x] Encode video and audio using FFmpeg libraries: H.264 (libx264) and AAC through `ffmpeg-next` on the `recorder` thread (`encoder-ffmpeg`, ADR 0005).
- [x] Mux into a local container: Matroska, created only as a new file in the engine's output directory.
- [x] Finalize files correctly on stop and on recoverable errors: encoders are flushed and the trailer is written on `stop_recording`, on encoder errors and when the engine is dropped (stdin closed).

Acceptance:
- [x] Produce a playable recording with synchronized audio/video (2026-10-20, release, `streamforge record --duration-ms 3000`):
  - libx264 1920×1080 at 60 FPS: 181 frames, 0 dropped
  - AAC 48 kHz stereo: 0 gaps
  - ffprobe confirms both streams; the container lasts 3.054 s
  - the stream ends differ by 3 ms
- [x] Automated test checks duration, stream metadata, and A/V timestamp drift (`crates/streamforge-engine/tests/recording.rs`): it records through the real engine process, then demuxes and decodes the file and checks:
  - codecs, size, frame rate, sample rate and channels
  - duration
  - end drift below 50 ms
  - the color bars and the tone RMS

Known limitations, deferred:
- Linux only; Windows and macOS return `unsupported` (M9).
- Fixed settings: 1920×1080@60 reference scene, x264 `veryfast`/CRF 20, AAC 160 kbit/s. No hardware encoders, no settings in the command yet.
- Live-input drift resampling is still linear (ADR 0004).
- A failure during recording is visible through polling (`get_status.recording_failed`); there are no push events yet. This failure path has no automated test, because an encoder failure cannot be triggered deterministically.
- Live sources are stamped when mixed or composited, not when captured. Live audio is late by its jitter-buffer target (about 30 ms), and screen frames by up to one capture interval. Per-source latency compensation is deferred.
- `streamforge record` does not handle Ctrl-C; Matroska stays playable up to the last cluster but loses its index.

## M6 — Live output

- [x] Add RTMP output: H.264 (CBR, 2 s GOP) + AAC in FLV via FFmpeg's RTMP protocol on the `streamer` thread (`start_streaming`, `stop_streaming`, ADR 0006).
- [x] Handle reconnect, network errors, encoder failure and shutdown:
  - exponential-backoff reconnects with fresh encoders, keyframe first
  - connect, handshake and writes bounded (5 s); stop interrupts blocked I/O after a 2 s grace, so a dead or silent peer cannot stall the output or its shutdown
  - the reconnect count restarts only after a stable connection, so accept-and-drop servers still end in `failed`
  - a `failed` state after the attempts, with counted drops
  - the stream key handled as a secret
- [x] Keep recording and streaming as separate output consumers: one shared media pipeline fans out to outputs with their own copy pools, queues, encoders and threads.

Acceptance:
- [x] Stream to a local test server: an in-process FFmpeg RTMP ingest (`rtmp-test-server`). The tests check codecs, 1920×1080, a keyframe first and A/V spans; reconnect and give-up are covered too.
  - Manual check (2026-10-20, release): `streamforge stream --record` against `ffmpeg -listen 1` streamed 300 frames at 1080p60 / 6 Mbit/s with 0 dropped, while the recording got 317 frames with 0 dropped.
- [x] Recording can continue if streaming output fails, where the graph configuration allows it (`crates/streamforge-engine/tests/streaming.rs`):
  - the ingest drops the connection and never returns
  - the stream ends in `failed`, while `recording_active` stays true
  - the recorded file covers the whole run, with no audio drops and every video tick accounted for

Known limitations, deferred:
- Linux only (M9). `rtmps://` depends on the TLS support of the linked FFmpeg.
- The stream uses the canvas resolution (1080p60); no scaling, no hardware encoder, one stream at a time.
- Outputs share the sources of the first output; changing sources needs all outputs stopped (scene commands come with M7).
- FFmpeg's own log output is disabled once streaming is used (it can contain the stream path); a redacting log callback would keep diagnostics.
- Name resolution inside FFmpeg is neither bounded by the I/O timeout nor interruptible.
- Each output copies every frame and converts it separately (isolation over memory bandwidth).
- No push events for state changes; clients poll `get_status`.

## M7 — Desktop UI

- Add scenes/sources, preview, audio mixer, recording/stream controls.
- UI calls the same command handlers as CLI/protocol adapters.

Acceptance:
- UI can create a scene, add a source, and start/stop a recording without direct business logic in UI widgets.

## M8 — Agent integration

- Expose safe read-only status and diagnostics first.
- Add MCP tools over the same command API.
- Require explicit authorization for sensitive actions and credentials.
- Never expose stream keys in logs or default tool responses.

Acceptance:
- Agent can inspect status and create a test scene.
- Destructive and credential-related operations are permission-gated.

## M9 — Cross-platform backends

- Windows capture/audio backend.
- macOS capture/audio backend.
- Hardware encoding paths where beneficial.
- Document capability matrix and run platform CI.

Acceptance:
- CI builds on all three operating systems.
- Capture/recording integration tests run on available native runners; limitations are explicit.
