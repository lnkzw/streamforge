# Architecture

## Goals

- Build an independent, cross-platform streaming application.
- Keep the media engine independent of UI and agent transports.
- Make the same typed command surface available to CLI, desktop UI, JSON-RPC, and MCP adapters.
- Keep real-time paths bounded, observable, and testable.

## High-level layers

1. **Application clients**: desktop UI, CLI, future JSON-RPC and MCP adapters.
2. **Command layer**: validates commands, checks state and permissions, dispatches to engine services.
3. **Engine controller**: owns lifecycle and coordinates media graph changes.
4. **Media graph**: nodes, connections, clocks, timestamps, bounded queues, metrics.
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
| `media-core` | lib | Platform-neutral media types: time model, `VideoFrame`, `FramePool`, `FrameMailbox`, `AudioRing`, latency histogram | — |
| `capture-api` | lib | Platform-neutral capture interface: capabilities, requests, reports, `CaptureError`, `CaptureBackend` trait, allocation-free diagnostics accumulators | `media-core` |
| `capture-linux` | lib | Linux `CaptureBackend`: XDG ScreenCast portal + PipeWire (`docs/capture-linux.md`); empty on other OSes | `capture-api` (Linux only: `ashpd`, `pipewire`) |
| `capture-platform` | lib | Picks the `CaptureBackend` for the target OS (`cfg(target_os)`); the only crate that names platform backends | `capture-api`, `capture-linux` (Linux) |
| `audio-engine` | lib | Mixer core (gain ramps, mute, meters, clipping policy), test tones, mixer thread with bounded drift-absorbing input buffering | `media-core` |
| `video-compositor` | lib | Scene graph, CPU reference renderer, render loop on its own thread, test patterns, PPM snapshots (ADR 0003) | `media-core` |
| `command-api` | lib | Typed commands, command registry, results, structured errors, `CommandHandler` trait | `capture-api`, `video-compositor`, `audio-engine` (report types) |
| `engine` | lib | Engine controller; owns engine state, implements `CommandHandler`, drives an injected `CaptureBackend`, the compositor and the mixer | `command-api`, `capture-api`, `video-compositor`, `audio-engine` |
| `control-protocol` | lib | JSON-lines wire format v1 and a transport-agnostic serve loop (`docs/protocol.md`) | `command-api` |
| `streamforge-engine` | bin | Headless engine process; serves `control-protocol` on stdin/stdout | `engine`, `control-protocol`, `capture-platform` |
| `streamforge-cli` | bin | Human/script CLI; calls the in-process engine through `CommandHandler` | `command-api`, `engine`, `capture-platform` |

`engine` and `control-protocol` do not depend on each other. Only the binaries combine them. A future socket transport, JSON-RPC or MCP adapter would be another crate that calls `CommandHandler`, next to `control-protocol`.

The binaries are the composition roots. Each injects `capture_platform::backend()` with `Engine::with_capture`, so CLI and protocol server always use the same backend. On OSes without a backend, `capture-platform` returns `capture_api::UnsupportedCapture`, which reports a reason instead of failing to build. `engine` never depends on a platform crate.

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

The engine thread only starts, waits for and stops these threads; it never touches frames. Editing the scene while it runs, which the UI needs (M7), will go through a bounded command channel into the compositor thread. See ADRs 0002 and 0003.

## Suggested future crates

Add only when a milestone needs them (the socket transport, if needed, extends `control-protocol` or becomes a sibling adapter crate):

- `capture-windows`
- `capture-macos`
- `encoder-ffmpeg`
- `output-rtmp`
- `output-recording`
- `desktop-app`
- `mcp-server`

## Cross-platform policy

The application architecture is cross-platform, but capture and device integration necessarily use OS-specific APIs. Each backend must document capability differences and provide runtime capability discovery. Never assume every OS supports identical capture modes.

## Real-time policy

- No unbounded queues in live paths.
- Keep device callbacks short and avoid blocking operations.
- Use a monotonic clock and explicit media time bases.
- Make frame-drop and audio-overrun behavior explicit and observable.
- Keep the UI thread separate from capture, mixing, rendering, and encoding.
