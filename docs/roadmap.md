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
  - [ ] Window capture: pending. A run with `--target-kind window` delivered a monitor (2560×1440, 252 frames in 3 s, 84 FPS, 0 drops) because xdph's picker offers every type. The report now warns about such mismatches. Repeat the run and pick an entry in the picker's window tab.
- [x] Capture diagnostics report dimensions, timestamps (explicit time base), FPS, frame intervals and drops: sequence gaps, buffer starvation, empty and corrupted buffers, and the audio frame deficit.
- [x] Unsupported capabilities fail with a useful message:
  - `unsupported` with `details.available` for source types and cursor modes the portal does not offer.
  - `not_found` with alternatives for unknown inputs.
  - `unavailable` with a remedy when portal or PipeWire are missing.
  - Non-Linux builds report `unsupported` with the milestone that adds support.

Known limitations, deferred to later milestones:
- Probes block the engine thread; there is no continuous capture into a media graph yet (M3).
- No DMA-BUF import, restore tokens, multi-stream capture or cursor metadata.
- No desktop-audio (sink monitor) capture; only sources are listed. (Linux: since M11 step 5b; Windows and macOS: since step 5d.)
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
- Desktop audio (sink monitor) capture is not implemented. (Linux: since M11 step 5b; Windows and macOS: since step 5d.)
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
- Linux only; Windows and macOS return `unsupported` (M9). Lifted in M9a: recording works on all three (ADR 0009).
- Fixed settings: 1920×1080@60 reference scene, x264 `veryfast`/CRF 20, AAC 160 kbit/s. No hardware encoders, no settings in the command yet.
- Live-input drift resampling is still linear (ADR 0004).
- A failure during recording is visible through polling (`get_status.recording_failed`); there are no push events yet. This failure path has no automated test, because an encoder failure cannot be triggered deterministically.
- Live sources are stamped when mixed or composited, not when captured. Live audio is late by its jitter-buffer target (about 30 ms), and screen frames by up to one capture interval. Per-source latency compensation is deferred.
- `streamforge record` does not handle Ctrl-C; Matroska stays playable up to the last cluster but loses its index. Lifted in M13 step 5a: every program finalizes its outputs at Ctrl+C or SIGTERM.

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
- Linux only (M9); lifted in M9a, streaming works on all three (ADR 0009). `rtmps://` depends on the TLS support of the linked FFmpeg.
- The stream uses the canvas resolution (1080p60); no scaling, no hardware encoder, one stream at a time.
- Outputs share the sources of the first output; changing sources needs all outputs stopped (scene commands come with M7).
- FFmpeg's own log output is disabled once streaming is used (it can contain the stream path); a redacting log callback would keep diagnostics.
- Name resolution inside FFmpeg is neither bounded by the I/O timeout nor interruptible.
- Each output copies every frame and converts it separately (isolation over memory bandwidth).
- No push events for state changes; clients poll `get_status`.

## M7 — Desktop UI

- [x] Add scenes/sources, preview, audio mixer, recording/stream controls (`streamforge-desktop`, egui/eframe, ADR 0007):
  - engine-owned scenes with color bars, solid and screen sources, edited live
  - a 480×270 @ 30 FPS preview
  - mixer gain, mute and peak meters
  - record and RTMP controls with a password field for the key
- [x] UI calls the same command handlers as CLI/protocol adapters: an engine worker thread executes `Command::decode` + `CommandHandler::handle`; the UI model only builds commands.

Acceptance:
- [x] UI can create a scene, add a source, and start/stop a recording without direct business logic in UI widgets (`crates/streamforge-desktop/tests/acceptance.rs` drives the model the widgets call, against the real engine).

Known limitations, deferred:
- Scenes and settings are not persisted.
- Sources are placed numerically; no dragging on the preview, no undo.
- The UI has no audio-input picker yet (`add_channel` and `set_channel_input` exist in the API).
- The egui layer has no automated UI test; the model below it is tested.
- Requires Rust 1.95 or newer (eframe 0.36).

## M8 — Agent integration

- [x] Expose safe read-only status and diagnostics first: `streamforge-mcp` offers only read-only tools unless started with `--allow control` or `--allow sensitive` (ADR 0008).
- [x] Add MCP tools over the same command API (`rmcp`, stdio): every tool is a registry command with a schema generated from its decoder, called via `Command::decode` + policy + `CommandHandler`.
- [x] Require explicit authorization for sensitive actions and credentials:
  - Access levels are chosen by the person starting the adapter; denied commands are not listed and return `permission_denied`.
  - The JSON-lines server got `--allow` as well.
  - Streaming goes only to a target configured at startup, with the key from an environment variable.
- [x] Never expose stream keys in logs or default tool responses: the MCP tool takes no URL or key, redirects are refused, and results show `<stream key>`.

Acceptance:
- [x] Agent can inspect status and create a test scene (`crates/streamforge-mcp/tests/mcp.rs`, official MCP client against the real binary: `get_status`, then `create_scene` and `add_source` with color bars under `--allow control`).
- [x] Destructive and credential-related operations are permission-gated:
  - `delete_scene` and screen sources are denied below `sensitive`.
  - `start_streaming` exists only with a configured target and refuses a URL.
  - The key never reaches tool lists, results or server info (tested).

Known limitations, deferred:
- A long command (probes, a stream connecting, a screen picker) blocks other tool calls; MCP cancellation does not abort it.
- One streaming target per server; no per-call user confirmation through MCP elicitation yet.
- No frames over MCP (the preview is in-process only).

## M9 — Cross-platform backends

Delivered in steps (ADR 0009):

- [x] Windows capture/audio backend:
  - [x] Audio through `cpal` (WASAPI), converted to 48 kHz stereo (M9a).
  - [x] Screen capture through Windows.Graphics.Capture: monitors and windows named by `list_capture_targets`, verified on the CI runner (M9b, ADR 0010). A system picker is not offered yet.
- [x] macOS capture/audio backend:
  - [x] Audio through `cpal` (CoreAudio) (M9a).
  - [x] Screen capture through ScreenCaptureKit: displays and windows named by `list_capture_targets`, verified on the CI runner, whose image allows screen recording (M9b, ADR 0010). Without the permission, capture fails with a structured `permission_denied`.
- [x] Recording and streaming on Windows and macOS: FFmpeg linked on every OS (M9a).
- [x] Hardware encoding paths where beneficial (M9c, ADR 0011): `video_encoder` (`auto` by default) tries VAAPI or NVENC (Linux), NVENC, Quick Sync or AMF (Windows), VideoToolbox (macOS), then libx264. Verified with VAAPI on an AMD GPU (half the CPU time of libx264 for the reference recording); without a GPU on the CI runners, only the fallback and error paths are tested there.
- [x] Document capability matrix and run platform CI (`docs/platforms.md`): FFmpeg on all three runners; recording, streaming and acceptance tests run natively on each.

Acceptance:
- CI builds on all three operating systems.
- Capture/recording integration tests run on available native runners; limitations are explicit.

## M10 — Live health and reliability

From the architecture assessment (`docs/architecture/CURRENT_STATE_ASSESSMENT.md`, §9), one PR per step:

- [x] Steps 0–2: live counters in compositor, mixer and outputs, the `get_health` command, a truthful `get_status` (`media_engine_ready`, `pipeline_running`), CLI progress lines, stale documentation fixed (ADR 0012).
- [x] Step 3: health in the desktop UI: a health strip (outputs, render and mix trouble, ended captures), and each screen source's capture state, frame age and a restart button.
- [x] Step 4: capture stream state (`running`, or `ended` with a reason) from every backend, shown in `get_health` (`captures`). Stalls show through the frame age, not as a state. Verified on the Windows runner by closing a captured window (ADR 0013).
- [x] Step 5: `restart_source` without stopping outputs, for screen sources with a running capture, optionally on another `capture_target_id` (a closed window's id does not come back). Picker sources only while no output is open, which on Linux means every screen source (ADR 0013).
- [x] Step 6: versioned project file (save, load, validate; atomic write; no secrets).
  - [x] 6a: stable scene and source ids (`scene_…`, `src_…`); commands take an id or a name (ADR 0014).
  - [x] 6b: `save_project`, `load_project`, `validate_project` in a project directory set at start (`--project-dir`), `set_capture_target` for loaded screen sources, `streamforge project validate` (ADR 0015). No desktop UI for projects yet.
- [x] Step 7: measurement harness (`streamforge-bench`): render, drop and CPU figures for one and two outputs, with and without health polling; baseline in `docs/testing.md`, informational CI job on all three runners.

Acceptance:
- Failures of sources and outputs are visible while they run, through the same command surface for every client.
- Every step keeps the existing tests and adds tests for its behavior.

## M11 — Audio routing and output profiles

No release pressure: the audio model is designed and built before the protocol is frozen, so the API is designed once and the mixer UI shows the real model (`docs/architecture/ARCHITECTURE_VISION.md` §6; assessment §10.2, question 4). Inputs, channels, buses, sends, outputs and recording tracks are separate concepts in the model, the protocol and the UI; separate tracks alone are not a routing system.

- [x] ADR 0017: the audio graph and output profiles, reviewed with the project owner before code.
- [x] Stable channel ids; the master becomes the program bus (step 2a: `add_channel`, `remove_channel`, `update_channel`, `set_channel_input`, `update_bus`; project file version 2, version 1 migrated).
- [x] Buses (submixes with gain, mute, meters): up to 8 aux buses, each assigned to the program bus at a level or a separate mix (step 2b: `add_bus`, `remove_bus`, `update_bus`).
- [x] Sends from channels to buses, pre- or post-fader, with a level (step 2b: `set_send`; live, with a click-free tap change).
- [x] Output profiles: per output its mix, its recording tracks and its encoder settings (step 3a: mix and encoder settings, with a live mix switch; step 4: tracks).
- [x] Multitrack recording with a defined track mapping: track 1 is the mix, then up to 7 listed buses, each an AAC stream named after its bus (step 4).
- [ ] A monitor output to an audio device, separate from the program outputs.
  - [x] Step 5a: the monitor in the engine and the protocol (profile with bus, on/off and device; `set_monitor_device`, `list_audio_outputs`; lock-free feed with drift resampling; health and reports; feedback warning), tested with a fake device.
  - [x] Step 5b: PipeWire playback on Linux (a real-time output stream reading the lock-free feed), and each sink's monitor as an input with `loopback_of`, so desktop audio can be captured and the feedback warning works there. Tested natively: a live test plays into a null sink it creates, and the engine was checked end to end (`docs/capture-linux.md`).
  - [ ] Step 5c: playback on Windows and macOS through `cpal` (WASAPI converts on Windows; macOS keeps the device's rate and resamples in the callback; the default output is pinned when the monitor starts). Implemented and tested on CI; waiting for the tone test on real Windows and Mac hardware.
  - [ ] Step 5d: desktop audio on Windows and macOS: each output as an input `<output>.monitor` with `loopback_of` (WASAPI loopback, kept running by silence on the output; a CoreAudio process tap on macOS 14.6 or newer), so the feedback warning works there too. Outputs with more than two channels are not downmixed yet. Implemented and tested on CI (listing; capture on Windows if the runner has an output); waiting for the loopback test on real Windows and Mac hardware.
- [x] Processing slots in the graph for later effects (no effects yet): 4 per channel and bus, before mute and both taps on a channel, before mute and fader on a bus (step 1).

Acceptance:
- The graph has a fixed size per run: no allocation in the audio path, bounded queues, and drops or underruns counted as today.
- Every routing change is a typed command and is saved in the project file (with a schema migration).
- Tests cover summing, send semantics, per-output mixes and track mapping; the bench measures the cost of buses and sends.

## M12 — Desktop UI, second pass

From the project owner's brief: a professional production studio with its own identity. The "Control Room" direction is approved: `docs/ui/design-system.md`, `docs/ui/audit.md`, and ADR 0016 (egui stays, with an own design system).

- [x] Audit, design direction, design-system foundation (tokens, bundled IBM Plex fonts, painted icons, components) and headless renders of the real UI (`tests/render.rs`).
- [x] The production workspace as a vertical slice: program monitor with tally, transport, scenes, sources and inspector, outputs, health (step 2). The mixer dock shows channel strips and the program bus until the console replaces it. The preview is 480×270, so a large monitor shows it soft; a sharper preview is an engine change, not yet planned.
- [x] The mixer as a console (channels, buses, program) and a routing matrix (channels × buses, with each output's mix bus beside it), on the M11 model (step 3): sends with level and tap, aux bus assignment, buses added and removed while idle, each output's mix, the recording's tracks and the monitor switch, and the double-routing mark. Choosing devices for channels and the monitor comes with the audio sources (next step).
- [x] Projects, audio sources and output settings in the desktop (step 4, in three parts):
  - [x] 4a: projects. `--project-dir` (default: the working directory), a project menu in the top bar (open, save, save as) with an unsaved-changes mark that the window title repeats, an open dialog that lists the folder (`list_projects`, new in `command-api`, so agents have it too) and shows the engine's check of the chosen file, a save dialog that warns before replacing a file, and a question before opening a project or closing the window over unsaved changes (closing offers to save).
  - [x] 4b: audio sources. Channels added and removed, and their input chosen (`set_channel_input`), in the routing matrix: an input column with the engine's audio inputs (`list_audio_inputs`; desktop audio grouped apart, a device that is not listed now marked), a test tone, or the system's default input.
  - [x] 4c: output settings. The video encoder (auto, software, hardware) in the record and stream settings and the stream's bitrate, from and to the output profiles (`set_output_profile`); a monitor row in the outputs dock with its state and device, and a dialog that chooses the device from the engine's outputs (`list_audio_outputs`, `set_monitor_device`) while the pipeline is idle. A monitor that ended shows why there and in the health summary.
- [x] Rollout to the remaining views and keyboard operation as specified (step 5, in two parts):
  - [x] 5a: rollout. The last controls egui drew in its own style are design-system components: menu items (an icon column, the current choice's check, room for shortcuts) in the project menu and the scene and source context menus, dropdowns for audio devices and screen sources, segmented controls for a source's kind, a channel's input and the encoder, a slider with a number field for opacity, and a loading line. The health details have a dock's padding. Number and text fields stay egui's own, styled by the theme; so does the colour picker.
  - [x] 5b: keyboard operation as specified, plus Ctrl+O, Ctrl+S and Ctrl+Shift+S for projects (Cmd on macOS). Keys act only without a dialog, menu or text focus; menus and tooltips show them. Tab goes through the docks in reading order (the panels are laid out first, then drawn in that order), and stops only where a focus ring shows: the docks' edges are resized by the workspace's own handles, since egui's are Tab stops without a focus. Up and Down move in the focused scene and source lists, and no longer move the focus off a fader.

Acceptance:
- Every change is checked on renders of the running app at 1280×720, 1400×900 and 1920×1080, not only by building.
- New or changed commands go through `command-api`, so CLI, protocol and MCP get them too.

## M13 — Stable Linux release (0.1.0)

Linux first; Windows and macOS follow with tests on real machines.

- [x] Protocol review before the freeze: the breaking changes still wanted (e.g. `media_engine_ready`, the legacy `screen: true` output layer, the name `source_id` for capture targets). From 0.1.0 on, the versioning policy in `docs/protocol.md` applies.
  - [x] Step 1: review of the whole surface, decisions in ADR 0018 (2026-10-26).
  - [x] Step 2a: legacy, state and strictness: `media_engine_ready` and the output screen layer removed, one state per output in `get_status` and `get_health`, destructive flags, one file name rule on every system, repeated keys refused, `details.unexpected` for every command.
  - [x] Step 2b: names (capture targets, `<entity>_id`/`<entity>_name` references, explicit recording tracks, CLI flags).
  - [x] Step 2c: report shapes (`audio_tracks`, nested `pipeline`, embedded errors, units, fractional frame rates), then wire-shape tests on every result.
- [x] Soak test on the development machine, after the UI changes: an hour of recording and streaming with health polling; no unexplained drops, no growing memory, a file that decodes from start to end. Harness `streamforge-soak`; passed on 2026-10-27 with real sources and a stream to MediaMTX on the local network (`docs/testing.md`, Soak test).
- [x] Release workflow (`.github/workflows/release.yml`): Linux tarballs per distribution family, since FFmpeg and PipeWire are linked dynamically and their library versions differ (Ubuntu 24.04, Arch Linux). Each is smoke-tested on its own system; a tag creates a draft release.
- [x] User documentation: installation and runtime dependencies, a desktop and CLI quick start, MCP setup, known limitations, and the FFmpeg license note for binaries. The README is the project's front page; `docs/user-guide.md` is the guide (shipped in the packages with the rest of `docs/`), `docs/development.md` the developer part. The packages gained `install.sh` (into `~/.local` by default, `--uninstall`), an application-menu launcher and an app icon; the desktop window reports the launcher's app id.
- [x] Version 0.1.0: changelog, tag, published release. Published on 2026-10-27 (tag `v0.1.0` on c57f2f9, packages for Ubuntu 24.04 and Arch Linux).
  - [x] Step 5a: three gaps found while writing the user guide, closed before the tag. The desktop names each recording by its start time unless a name is typed, so a second take no longer fails on the existing file; closing the window while recording or live asks first; Ctrl+C or SIGTERM to the CLI, the engine process or the desktop stops and finalizes the outputs, as the MCP server already did (new crate `quit-signal`, now also used by the MCP server; a second signal quits at once; SIGHUP keeps its default, so `nohup` works).
  - [x] Step 5b: `CHANGELOG.md`, shipped in the packages; the release notes come from the version's section, checked on every release run; protocol v1 is stable from 0.1.0 on.
  - [x] Step 5c: the release check on the development machine. The Arch package from a manual release run installed with `./install.sh`; the launcher shows with its icon, the window's class is the app id, and a 10 min soak against the installed engine (synthetic sources, a stream read back from MediaMTX) passed. Two small findings were fixed before the tag: the installer's PATH note with a trailing slash, and `streamforge-desktop --version`. The package from the draft release installed and recorded as well.

Acceptance:
- The soak test passes, and CI on `main` stays green over several runs.
- The release installs from its tarball on the development machine and works there.

After 0.1.0, additive: effects in the processing slots (EQ, compressor, gate, ducking), per-output video compositions, production states and automation, plugins.

## M14 — Sources and layers

Cameras, images and layer control (ADR 0019). Everything is additive to protocol v1; the project file moves to schema 4.

- [x] Step 1: ADR 0019 with the project owner's decisions (2026-10-27).
- [x] Step 2: layers. `move_source` changes the drawing order, `visible` hides a source without stopping its capture, project file schema 4 migrates version 3; the desktop reorders by drag and keys and has an eye toggle.
- [x] Step 3: image source. PNG and JPEG by plain file name in the project folder (`list_images`), decoded by the new crate `frame-decode`, with size limits; a missing image does not block loading a project. `update_source` switches an image's file, `restart_source` reads it again; the desktop adds images from the project folder (with **Open folder**) and changes them in the inspector.
- [ ] Step 4: Linux camera.
  - [x] 4a: `CaptureBackend` split into `ScreenCapture`, `AudioDevices` and `CameraCapture`, without behaviour change.
  - [ ] 4b: cameras through PipeWire, in three parts (decided with the project owner on 2026-10-28):
    - [x] 4b1: the capture backend. `PipeWireCamera` lists camera nodes with their modes and captures them on their own thread; `frame-decode` converts YUY2, UYVY, NV12, I420 and MJPEG to BGRX; `list_cameras`, CLI `capture cameras`, `camera` in `get_capture_capabilities`; the conversion cost measured.
    - [ ] 4b2: the `camera` source kind in the engine (it starts with its scene, also on air, `restart_source`, a missing camera warns), health, the project file, a fake camera in the engine tests.
    - [ ] 4b3: the desktop's camera dialog and inspector.
- [ ] Step 5: Windows (Media Foundation, 5a) and macOS (AVFoundation, 5b) cameras; real cameras go to the manual hardware tests.

Acceptance:
- A scene with a camera, an image and a screen records and streams on the development machine (its camera is a test one: PipeWire's `videotestsrc` or a v4l2loopback device, since it has no webcam), with the layers reordered and one hidden on air.
- A project with all three kinds saves, loads and migrates from schema 3; a missing camera or image loads with a warning.
- The camera conversion and render cost are measured against the `docs/testing.md` baseline.

Not in M14: text, media file and browser sources, cropping and flipping, camera controls, an image file dialog or import.
