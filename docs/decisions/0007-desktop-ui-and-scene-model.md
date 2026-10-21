# ADR 0007: Desktop UI with egui and an engine-owned scene model

Status: Accepted (M7)

## Context

M7 asks for scenes and sources, a preview, an audio mixer and recording and stream controls in a desktop UI. The UI must call the same command handlers as the CLI and the protocol adapters, without business logic in widgets. Until M6, the pipeline rendered a fixed reference scene, could not change while running, and the mixer exposed neither live control nor levels.

## Decision

### UI toolkit

- **egui/eframe 0.36** (the project owner's choice): pure Rust, immediate mode, the same code on Linux, Windows and macOS. A frequently updated texture shows the preview cheaply.
- **Toolchain:** eframe 0.36 needs Rust 1.95; CI uses the current stable.
- **Rejected:**
  - Tauri: web toolchain, and a preview over IPC.
  - Slint: license conditions.
  - iced: API churn, and more work for a live texture.

### Layers in `streamforge-desktop`

- **`EngineWorker`:**
  - Builds the engine on its own thread and executes requests there via `Command::decode` and `CommandHandler::handle`, the same path as the JSON-lines server.
  - Capture backends need not be `Send`, so the engine never moves between threads.
  - Requests and responses use bounded channels. The UI thread only `try_send`s and `try_recv`s; when the queue is full it reports "busy".
  - A repaint notifier wakes the UI per response.
- **`Model`:** UI state plus actions. Every action is a command with the same names and JSON parameters the protocol uses; the responses fill in the state. It holds no widgets and no business logic, so the M7 acceptance test drives it without a display.
- **`DesktopApp` (egui):** draws the model and calls its actions; this layer is not unit-tested.
  - Panels: status with a preview toggle; scenes (create, activate, delete with confirmation); sources (add with kind, color and rectangle; opacity; remove); mixer (gain, mute, meters with UI-side decay); outputs (record file, RTMP URL plus a password field for the key, start and stop); activity log. Since M10 also a health strip below the outputs, and each screen source's capture state with a restart button (ADR 0013).
- **Stream key:** typed into a password field and handed to the engine on start only; the model never logs it.

### Scene model in the engine

- **Collection:** up to 16 scenes with up to 16 sources each, kept in memory. Source kinds:
  - `color_bars`
  - `solid` (RGBA)
  - `screen`: portal capture, fitted with its aspect ratio kept
- **Default scene:** `Test pattern` reproduces the M3 reference scene, so existing outputs and tests render the same image.
- **Commands:**
  - scenes: `list_scenes`, `create_scene`, `delete_scene` (needs `confirm: true`; the active scene cannot be deleted), `set_active_scene`
  - sources: `add_source`, `update_source`, `remove_source`
- **Live changes:**
  - A change to what the running pipeline renders is applied to the compositor. If that fails (e.g. the picker of a new screen source is cancelled), the change is undone and the error returned.
  - `Compositor::update` places a validated `SceneUpdate` in a single newest-wins slot. The render thread only try-locks it before a frame, so editing never blocks rendering and edits never queue up.
  - Sources whose id and input (the same static frame or mailbox) are unchanged keep their state.
  - The engine caches static frames by content so rebuilds reuse them.
  - **Screen captures:** a capture starts when its source is first rendered and keeps running while the source exists in any scene, so scene switches do not ask the picker again.
  - **Open outputs:** while a recording or stream is open, an edit that would start a new capture is refused. The picker waits for a human and would block the engine, so `stop_*` and `get_status` would queue behind it. With only the preview running, someone is at the UI, so it is allowed.
  - **Bounded reports:** stats of removed sources and capture reports are bounded (the latest 64 each, plus counts).

### Mixer control

- **Live changes:** `MixerControl` carries gain, mute and master changes in a preallocated slot that the mixer thread try-locks once per block (no allocation, no waiting).
- **Levels:** the mixer thread keeps the last 30 block peaks per strip in preallocated memory and publishes the window maximum (300 ms) as `AtomicU32` f32 bits. Reads change nothing, so `get_mixer` is read-only and concurrent readers see the same levels.
- **Commands:**
  - `get_mixer` and `set_mixer_strip`/`set_master_gain` work live and when idle.
  - `set_audio_sources` (sensitive) configures inputs and tones while the pipeline is idle.

### Preview

- **Commands versus media:** control goes through commands (`set_preview`, sensitive); frames take the media path. The pipeline's fan-out has a preview consumer that downscales every second frame by 4 (480×270 at 30 FPS) into a pooled buffer and publishes it to a newest-wins `FrameMailbox`. Embedding UIs read it through `Engine::preview_frames()`; over the JSON-lines protocol no frames are sent.
- **Downscaling:** four samples per output pixel, about 1 ms per frame on the compositor thread. Frames dropped for lack of a free buffer are counted and reported when the preview is turned off.
- **Lifecycle:** the pipeline runs while the preview or any output is on.

## Measurements and checks (development machine, 2026-10-20)

- Engine tests check live edits through preview pixels: a scene switch, an added box, an opacity change, and the rollback of a failing live change.
- Mixer levels match the tone (−20 dBFS ±0.2) and follow mute and unmute live.
- The model test creates a scene, adds a solid source, activates it and records a file through the worker thread.
- The release UI was started on the development machine (Wayland) and ran without errors.

## Consequences

- Business logic stays in the engine. The UI is a thin adapter, so a future MCP adapter (M8) gets the same scene commands.
- Scenes are not persisted; a restart starts from `Test pattern`.
- Commands that can start a screen capture (`add_source`, `set_active_scene`) and the irreversible `delete_scene` are `sensitive`; `update_source` and `remove_source` stay `control`.
- New screen sources cannot go live during a recording or stream; starting captures without blocking (asynchronously) is future work.
- The UI cannot drag sources on the preview yet (rectangles are entered numerically). There is no undo, no audio-input picker list in the UI (`set_audio_sources` exists), and no settings persistence.
- The preview is best effort: frames the UI does not pick up in time are replaced, not counted.
