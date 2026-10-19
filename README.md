# StreamForge

An experimental, cross-platform, agent-friendly streaming application. The project aims to build its own real-time media graph and application workflow rather than wrapping OBS/libobs.

## Current status

This repository is an **architecture and workflow scaffold**, not a working streaming engine yet. In place so far:

- M0 (workspace) and M1 (command boundary): typed commands, a headless engine process, and a local JSON-lines control protocol.
- M2 (Linux capture spike): screen capture through the XDG portal and PipeWire, plus audio input capture. Both run as short diagnostic probes.
- M3 (first compositor): CPU compositor with scene graph, a 1080p/60 render loop on its own thread, and continuous screen capture as a live source.

Audio mixing, encoding, recording, and streaming are not implemented.

## Prerequisites

- Rust stable toolchain (`rustup`, `cargo`, `rustfmt`, `clippy`)
- Git
- Linux: PipeWire headers (≥ 0.3.44), `clang`/libclang and `pkg-config`, for `capture-linux`. Runtime needs PipeWire and xdg-desktop-portal with a ScreenCast backend. See `docs/capture-linux.md`.

Install Rust using the official instructions: <https://www.rust-lang.org/tools/install>

## Start locally

```bash
cargo build --workspace
cargo test --workspace
cargo run -p streamforge-cli -- --help
cargo run -p streamforge-cli -- status

# Headless engine: one JSON request per line on stdin, one response per line on stdout
printf '%s\n' '{"id":1,"command":"get_status"}' | cargo run -q -p streamforge-engine
```

The protocol is specified in `docs/protocol.md`.

Capture diagnostics (Linux; output is JSON):

```bash
cargo run -p streamforge-cli -- capture caps
cargo run -p streamforge-cli -- capture inputs
cargo run -p streamforge-cli -- capture screen --duration-ms 5000      # opens the system picker
cargo run -p streamforge-cli -- capture audio --input <name> --duration-ms 3000

# Compositor (1920x1080@60 test scene; --screen adds a live capture layer)
cargo run --release -p streamforge-cli -- compose --duration-ms 5000 --snapshot frame.ppm
```

The first version deliberately has no capture or streaming functionality. Do not expect it to replace OBS yet.

## Repository layout

- `crates/media-core`: platform-independent media types and time model
- `crates/capture-api`: platform-neutral capture interface and diagnostics
- `crates/capture-linux`: Linux capture backend (XDG portal + PipeWire)
- `crates/capture-platform`: selects the capture backend for the target OS
- `crates/video-compositor`: scene graph, CPU renderer and render loop
- `crates/command-api`: typed commands, registry, structured results/errors, `CommandHandler` trait
- `crates/engine`: engine controller implementing the command handlers
- `crates/control-protocol`: JSON-lines control protocol v1 (transport-agnostic)
- `crates/streamforge-engine`: headless engine process serving the protocol on stdio
- `crates/streamforge-cli`: CLI entry point (calls the same command handlers)
- `.github/workflows/ci.yml`: CI (fmt, clippy, tests on Linux/Windows/macOS)
- `docs/`: architecture, media model, control protocol, Linux capture, roadmap, testing strategy, decisions (ADRs)
- `AGENTS.md`: instructions for coding agents

## Development rules

Read `AGENTS.md` before making changes. Keep platform-specific APIs behind traits in `media-core` or dedicated backend crates. Do not add a dependency or implement a subsystem before documenting why it is needed.

## Design principles

1. Cross-platform APIs first; platform-specific backends behind explicit interfaces.
2. No OBS/libobs dependency in the media engine.
3. Audio and video have explicit timestamps and bounded queues.
4. The engine must be controllable headlessly through the same command API used by the UI and MCP server.
5. Every milestone must have an executable acceptance test.
