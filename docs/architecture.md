# Architecture

## Goals

- Build an independent, cross-platform streaming application.
- Keep the media engine independent of UI and agent transports.
- Make the same typed command surface available to CLI, desktop UI, JSON-RPC, and MCP adapters.
- Keep real-time paths bounded, observable, and testable.

## High-level layers

1. **Application clients**: desktop UI, CLI, the JSON-lines engine process and the MCP server.
2. **Command layer**: validates commands, checks state and permissions, dispatches to engine services.
3. **Engine controller**: owns lifecycle and coordinates media graph changes.
4. **Media pipeline**: captures, compositor and mixer feeding a fan-out to the outputs, with clocks, timestamps, bounded queues and live metrics (`get_health`, ADR 0012). It is a fixed pipeline, not a general node graph.
5. **Media services**:
   - Capture backends
   - Video compositor
   - Audio mixer and DSP
   - Encoders
   - Outputs / muxers
6. **Platform adapters**: Wayland portal + PipeWire, Windows capture/audio, macOS ScreenCaptureKit/CoreAudio/AVFoundation as appropriate.

## Dependency rules

- `media-core` defines platform-neutral types only.
- UI and transport adapters depend on the command API, not on media internals.
- Platform backends implement traits defined by a stable internal interface.
- The compositor and audio mixer must not depend on the UI.
- Do not add libobs as an engine dependency.

## Current crates

| Crate | Kind | Responsibility | Depends on |
|-------|------|----------------|------------|
| `media-core` | lib | Platform-neutral media types: time model, `VideoFrame`, `FramePool`, `FrameMailbox`, `AudioRing`, the lock-free `sample_ring` for playback devices (safe Rust: `AtomicU32` slots), latency histogram | — |
| `frame-decode` | lib | Encoded pictures into the engine's frames (ADR 0019): PNG and JPEG files into premultiplied BGRA `VideoFrame`s, with size limits checked from the header and JPEG EXIF orientation applied; header-only `probe` for listings; camera frames (YUY2, UYVY, NV12, I420, MJPEG) into a caller's BGRX buffer. Pure Rust, no `unsafe` | `media-core` (`png`, `zune-jpeg`) |
| `capture-api` | lib | Platform-neutral capture and playback interface: capabilities, requests, reports, `CaptureError`, `ScreenCapture`, `AudioDevices` and `CameraCapture` traits, grouped in `Capture` (audio playback for the monitor: `audio_outputs`, `start_playback`, `PlaybackStream`), allocation-free diagnostics accumulators, shared camera mode choice | `media-core` |
| `capture-linux` | lib | Linux `PortalScreen` (`ScreenCapture`): XDG ScreenCast portal + PipeWire screen capture; `PipeWireCamera` (`CameraCapture`): PipeWire camera nodes, converted by `frame-decode`; `PipeWireAudio` (`AudioDevices`): audio capture (sources and sink monitors), and PipeWire playback for the monitor (`docs/capture-linux.md`); empty on other OSes | `capture-api`, `frame-decode` (Linux only: `ashpd`, `pipewire`) |
| `capture-cpal` | lib | Windows and macOS `CpalCapture` (`AudioDevices`) (`cpal`: WASAPI, CoreAudio): capture converted to 48 kHz stereo, and the monitor's playback converted to the device, in the real-time callback without locks; only the converter builds on Linux | `capture-api`, `media-core` (Windows, macOS: `cpal`) |
| `capture-windows` | lib | Windows `WgcCapture` (`ScreenCapture`): Windows.Graphics.Capture of monitors and windows named by `list_capture_targets` (ADR 0010); `MfCamera` (`CameraCapture`): Media Foundation cameras converted to RGB32 by the system, Media Foundation loaded at run time, its `unsafe` calls in `mf.rs` (ADR 0019); only the id and camera formats build elsewhere | `capture-api`, `media-core` (Windows: `windows-capture`, `windows`) |
| `capture-macos` | lib | macOS `SckCapture` (`ScreenCapture`): ScreenCaptureKit capture of displays and windows named by `list_capture_targets` (ADR 0010); one isolated `unsafe` read of a locked pixel buffer; only the id format builds elsewhere | `capture-api`, `media-core` (macOS: `screencapturekit`) |
| `capture-platform` | lib | Builds `Capture` for the target OS (`cfg(target_os)`) from screen, audio and camera parts (an unsupported camera on macOS for now); the only crate that names platform backends (`docs/platforms.md`) | `capture-api`, `capture-linux` (Linux), `capture-cpal` (Windows, macOS), `capture-windows` (Windows), `capture-macos` (macOS) |
| `audio-engine` | lib | Audio routing graph (ADR 0017: channels, sends, aux and program buses, processing slots, gain ramps, meters, limit per output feed), test tones, mixer thread with bounded drift-absorbing input buffering, and the monitor's `PlaybackFeed` (drift resampling onto a device clock) | `media-core` |
| `video-compositor` | lib | Scene graph, CPU reference renderer, render loop on its own thread, test patterns, PPM snapshots (ADR 0003) | `media-core` |
| `encoder-api` | lib | Platform-neutral output vocabulary: recording and streaming settings, secret `StreamKey`, `StreamTarget` with redaction, reconnect policy, statistics, errors (no FFmpeg) | `media-core` (`Rational` frame rates) |
| `encoder-ffmpeg` | lib | Outputs on their own threads, each fed through a non-blocking bounded `OutputInput`: `Recorder` (H.264 + up to 8 named AAC tracks → Matroska, ADR 0005, ADR 0017) and `Streamer` (H.264 CBR + AAC → FLV over RTMP with reconnects, ADR 0006); hardware H.264 encoders where one opens, else libx264 (ADR 0011); on Linux, Windows and macOS | `encoder-api`, `media-core`, `ffmpeg-next` |
| `rtmp-test-server` | lib (test-only) | In-process RTMP ingest for tests (FFmpeg listen mode); never shipped | `ffmpeg-next` |
| `command-api` | lib | Typed commands, command registry, results, structured errors, `CommandHandler` trait | `capture-api`, `video-compositor`, `audio-engine`, `encoder-api` (report types) |
| `project-file` | lib | Project file v4 (ADR 0015, ADR 0017, ADR 0018, ADR 0019): JSON format, checks with JSON paths (sources and channels through the command decoders), migration of versions 1 to 3 with their frozen rules, atomic writes | `command-api` (`serde_path_to_error`) |
| `engine` | lib | Engine controller; owns engine state, implements `CommandHandler`, drives an injected `Capture` set and the media pipeline (compositor, mixer, fan-out) with its outputs; starts and keeps scene captures (screens, cameras); saves and loads projects; reads image sources' files from the project directory (one shared frame per file, 512 MiB in all) | `command-api`, `capture-api`, `video-compositor`, `audio-engine`, `encoder-ffmpeg`, `project-file`, `frame-decode` |
| `control-protocol` | lib | JSON-lines wire format v1 and a transport-agnostic serve loop (`docs/protocol.md`) | `command-api` |
| `quit-signal` | lib | Quit signals for the programs that own an engine: the first SIGINT or SIGTERM (Ctrl+C or Ctrl+Break on Windows) asks the program to finish, so its outputs are finalized; the second exits at once. SIGHUP keeps its default (`nohup` works) | — (`signal-hook` on Unix, `ctrlc` on Windows) |
| `streamforge-engine` | bin | Headless engine process; serves `control-protocol` on stdin/stdout, reading stdin on its own thread so that a quit signal ends the input like EOF | `engine`, `control-protocol`, `capture-platform`, `quit-signal` |
| `streamforge-cli` | bin | Human/script CLI; calls the in-process engine through `CommandHandler`; a quit signal ends `record` and `stream` early with their reports | `command-api`, `engine`, `capture-platform`, `quit-signal` |
| `streamforge-desktop` | bin + lib | Desktop UI (egui/eframe, ADR 0007): engine worker thread, a model that turns actions into commands, the production workspace (`views`, `workspace`) with projects opened and saved in `--project-dir`; the "Control Room" design system in `ui` (tokens, bundled IBM Plex fonts, painted icons, components; ADR 0016); a quit signal closes the window without asking | `command-api`, `engine`, `capture-platform`, `media-core`, `quit-signal` (`eframe`, `chrono` for local time) |
| `streamforge-bench` | bin | Measurement harness (M10 step 7): records and streams to a local ingest (a child process) for a fixed time, reports render, drop and CPU figures | `command-api`, `engine`, `rtmp-test-server` (`libc` / `windows-sys` for process CPU time) |
| `streamforge-soak` | bin | Soak test harness (M13): runs `streamforge-engine` as a child process over the protocol for a long recording and stream with desktop-like polling, samples the engine's memory, threads and descriptors (Linux `/proc`), and judges lost media, resource growth, the decoded file and the stream at a local ingest or read back from a server (`docs/testing.md`) | `rtmp-test-server` (`ffmpeg-next` to decode and read back, `libc` on Linux for the clock tick rate) |
| `streamforge-mcp` | bin + lib | MCP server on stdio (ADR 0008): tools from the registry with generated schemas, an access policy chosen at startup, streaming only to a configured target; a quit signal ends it like a closed session, finalizing outputs | `command-api`, `encoder-api`, `engine`, `capture-platform`, `quit-signal` (`rmcp`, `tokio`) |

`engine` and `control-protocol` do not depend on each other. Only the binaries combine them. A future socket transport, JSON-RPC or MCP adapter would be another crate that calls `CommandHandler`, next to `control-protocol`.

The binaries are the composition roots. Each injects `capture_platform::capture()` with `Engine::with_capture`, so CLI and protocol server always use the same capture parts. On OSes without a backend, `capture-platform` returns `capture_api::Capture::for_unsupported_platform()`, which reports a reason instead of failing to build. `engine` never depends on a platform crate.

## Command boundary

```text
adapter input ──decode──▶ Command ──CommandHandler::handle──▶ CommandOutput | CommandError ──encode──▶ adapter output
   (JSON line, CLI args, UI event)        (engine)                                      (JSON line, terminal, UI)
```

- `Command::decode(name, params)` is the only way to build a command from untyped input. It returns `unknown_command` or `invalid_params` and never panics.
- Each `CommandDescriptor` in the registry carries a stable wire name, a summary and a permission class (`read_only` / `control` / `sensitive`).
- Adapters must not compute results themselves; they translate input into a `Command` and output back to their medium.
- Wire formats map `command_api::ErrorCode` to their own codes explicitly, so internal renames do not silently change a protocol.

The headless process currently runs the engine and the protocol loop on the main thread and handles requests sequentially. Capture probes (M2) run their PipeWire loop on that same thread and block it for the probe duration; that is acceptable for diagnostics.

Since M3, real-time work has its own threads:
- **`screen-capture`:** PipeWire loop of a continuous screen stream. It converts frames into pooled buffers and publishes them to a `FrameMailbox`.
- **`compositor`:** fixed-rate render loop. It takes the newest frame of each live source and renders with scoped worker threads.
- **`audio-capture`:** PipeWire loop of a continuous audio stream. It pushes 48 kHz stereo `f32` into a bounded `AudioRing` (M4).
- **`audio-mixer`:** fixed 10 ms block clock. It pulls each ring with bounded buffering, mixes and meters, and does not allocate (M4).
- **`recorder`:** converts BGRA to YUV 4:2:0, encodes H.264 and AAC, and muxes Matroska (M5).
- **`streamer`:** the same conversion and encoding (CBR), muxed as FLV over RTMP. It reconnects with backoff, and every blocking network call is bounded (M6).
- **Feeding the outputs:** compositor and mixer feed outputs through output sinks (`FrameSink`, `BlockSink`) and a fan-out with one entry per attached output. The sinks never block:
  - each output's video tap copies into one of its own 8 pooled buffers
  - audio blocks travel inline in each output's bounded queue
  - whatever does not fit is dropped and counted for that output only

The engine thread only starts, waits for and stops these threads; it never touches frames.

Outputs (ADR 0006):
- **Lifecycle:** the media pipeline (captures, compositor, mixer, fan-out) starts with the first output and stops with the last. Outputs keep it running after their start command returns.
- **Stopping an output:** the engine sets the output's end time for both tracks, detaches it from the fan-out, then finalizes it. Producers keep serving the remaining outputs.
- **Exclusive commands:** while any output is open, commands that would compete for its devices or CPU, or block the engine thread (the probes), fail with `invalid_state`.
- **Drop order:** dropping the engine stops the pipeline first, then finalizes the outputs. See ADRs 0002 and 0003.

Scenes, mixer and preview (ADR 0007):
- **Scene state:** scenes are engine state, changed only through scene commands. The pipeline renders the active scene (default: the reference scene `Test pattern`).
- **Live scene edits:** an edit to the rendered scene reaches the running compositor through a newest-wins slot that the render thread only try-locks (`Compositor::update`); a failed live edit is undone.
- **Mixer control:** gain changes reach the running mixer the same way (`MixerControl`), and levels come back as atomics.
- **Preview:** the preview is a fan-out consumer that publishes downscaled frames to a mailbox (`Engine::preview_frames`). It runs on the media plane; control goes through commands.

Access control (ADR 0008):
- **Policy:** `command_api::AccessPolicy` allows commands up to a level chosen by whoever starts an adapter. `Command::required_access` refines the level per call; `add_source` needs only `control` for non-capturing sources.
- **JSON-lines server:** wraps the engine in `Gated` (default `sensitive`).
- **MCP server:** lists and accepts only allowed tools (default `read-only`). It gives agents no way to set a stream URL or key: streaming goes to a target configured at its start.
- **Threading:** `engine::EngineThread` runs the engine on its own thread for async adapters; calls go through `EngineHandle::call`.

The desktop UI is an adapter like the CLI:
- **Worker thread:** it builds the engine on its own thread and executes commands there via `Command::decode` and `CommandHandler::handle`. Channels are bounded, and the UI thread never blocks.
- **Model:** its model turns user actions into commands with the protocol's names and parameters. The widgets only call the model.
- **Projects:** the model tracks the project file last saved or opened and whether a command that changes what a file holds (scenes, audio, output profiles) succeeded since; the window title and the project menu show it, and opening a project or closing the window over unsaved changes asks first. Closing the window while the recording or the stream starts or runs asks too, since closing stops them.
- **Recording files:** without a typed name, each recording is named by its local start time (`recording-2026-10-27_15-30-05.mkv`, `_2` for a second take in the same second; after the clock went back, takes count on from the latest name), so the desktop's own takes never collide with the engine's no-overwrite rule. A file of that name from elsewhere (a second instance recording into the same folder) makes REC fail with a message.
- **Views:** the production workspace (`docs/ui/design-system.md`) is drawn by one module per dock in `views` (top bar with project menu, transport and health, scenes, sources, inspector, program monitor, mixer, outputs, dialogs). What they show (scene selection, transport state and timers, monitor fit, meter ballistics, the health summary) is computed in `workspace`, without widgets, and unit-tested there.

Windows camera lifecycle: a start is acknowledged after its first frame reaches the mailbox. Failure answers and stop completion follow native teardown: release the reader and source, drain queued samples, drop the receiver, then drop the Media Foundation/COM session. Start timeout, stop and Drop wait within a bound for that completion before joining; a driver call that never returns can leave a detached worker. Camera buffers expose individual pixel rows to the shared frame sink, which packs them without reading 2D-buffer padding. Screen capture keeps its mapped-frame publication API.

## Suggested future crates

Add only when a milestone needs them (the socket transport, if needed, extends `control-protocol` or becomes a sibling adapter crate):

- `capture-windows`
- `capture-macos`
- `output-rtmp`

## Cross-platform policy

The application architecture is cross-platform, but capture and device integration necessarily use OS-specific APIs. Each backend must document capability differences and provide runtime capability discovery. Never assume every OS supports identical capture modes.

## Real-time policy

- No unbounded queues in live paths.
- Keep device callbacks short and avoid blocking operations.
- Use a monotonic clock and explicit media time bases.
- Make frame-drop and audio-overrun behavior explicit and observable.
- Keep the UI thread separate from capture, mixing, rendering, and encoding.
