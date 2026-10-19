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
| `media-core` | lib | Platform-neutral media types and time model | — |
| `command-api` | lib | Typed commands, command registry, results, structured errors, `CommandHandler` trait | — |
| `engine` | lib | Engine controller; owns engine state and implements `CommandHandler` | `command-api` |
| `control-protocol` | lib | JSON-lines wire format v1 and a transport-agnostic serve loop (`docs/protocol.md`) | `command-api` |
| `streamforge-engine` | bin | Headless engine process; serves `control-protocol` on stdin/stdout | `engine`, `control-protocol` |
| `streamforge-cli` | bin | Human/script CLI; calls the in-process engine through `CommandHandler` | `command-api`, `engine` |

`engine` and `control-protocol` do not depend on each other. Only the binaries combine them. A future socket transport, JSON-RPC or MCP adapter would be another crate that calls `CommandHandler`, next to `control-protocol`.

## Command boundary

```text
adapter input ──decode──▶ Command ──CommandHandler::handle──▶ CommandOutput | CommandError ──encode──▶ adapter output
   (JSON line, CLI args, UI event)        (engine)                                      (JSON line, terminal, UI)
```

- `Command::decode(name, params)` is the only way to build a command from untyped input. It returns `unknown_command` or `invalid_params` and never panics.
- Each `CommandDescriptor` in the registry carries a stable wire name, a summary and a permission class (`read_only` / `control` / `sensitive`).
- Adapters must not compute results themselves; they translate input into a `Command` and output back to their medium.
- Wire formats map `command_api::ErrorCode` to their own codes explicitly, so internal renames do not silently change a protocol.

The headless process currently runs the engine and the protocol loop on the main thread and handles requests sequentially. Once commands can block on devices or encoders, the engine controller moves to its own thread behind a bounded command queue. See ADR 0002.

## Suggested future crates

Add only when a milestone needs them (the socket transport, if needed, extends `control-protocol` or becomes a sibling adapter crate):

- `capture-api`
- `capture-linux`
- `capture-windows`
- `capture-macos`
- `video-compositor`
- `audio-engine`
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
