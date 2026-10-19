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

- Composite two sources with position, scale and alpha.
- Keep render loop independent from UI.
- Add deterministic tests for transform and scene graph logic.

Acceptance:
- Test pattern + captured source render correctly.
- Target: 1080p/60 FPS on the development machine, measured and reported rather than assumed.

## M4 — Audio mixer

- Mix at least two inputs.
- Add gain, mute and peak/RMS meters.
- Define clipping and resampling policy.

Acceptance:
- Offline deterministic test tone mixing.
- Live test shows no unbounded latency growth during a 10-minute run.

## M5 — First recording

- Encode video and audio using FFmpeg libraries.
- Mux into a local container.
- Finalize files correctly on stop and on recoverable errors.

Acceptance:
- Produce a playable recording with synchronized audio/video.
- Automated test checks duration, stream metadata, and A/V timestamp drift.

## M6 — Live output

- Add RTMP output.
- Handle reconnect, network errors, encoder failure and shutdown.
- Keep recording and streaming as separate output consumers.

Acceptance:
- Stream to a local test server.
- Recording can continue if streaming output fails, where the graph configuration allows it.

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
