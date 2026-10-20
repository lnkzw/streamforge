# StreamForge

An experimental, cross-platform, agent-friendly streaming application. The project aims to build its own real-time media graph and application workflow rather than wrapping OBS/libobs.

## Current status

This repository is an **architecture and workflow scaffold**, not a working streaming engine yet. In place so far:

- M0 (workspace) and M1 (command boundary): typed commands, a headless engine process, and a local JSON-lines control protocol.
- M2 (Linux capture spike): screen capture through the XDG portal and PipeWire, plus audio input capture. Both run as short diagnostic probes.
- M3 (first compositor): CPU compositor with scene graph, a 1080p/60 render loop on its own thread, and continuous screen capture as a live source.
- M4 (audio mixer): a 48 kHz stereo mixer with gain, mute, meters and an explicit clipping and drift policy. It takes continuous audio capture and test tones.
- M5 (first recording, Linux): the composited scene and the audio mix are recorded to H.264 + AAC in Matroska through the FFmpeg libraries.

Streaming and a desktop UI are not implemented.

## Prerequisites

- Rust stable toolchain (`rustup`, `cargo`, `rustfmt`, `clippy`)
- Git
- Linux: PipeWire headers (≥ 0.3.44), `clang`/libclang and `pkg-config`, for `capture-linux`. Runtime needs PipeWire and xdg-desktop-portal with a ScreenCast backend. See `docs/capture-linux.md`.
- Linux: FFmpeg development libraries (libavcodec, libavformat, libavutil, libswscale; FFmpeg 6.1 (CI, Ubuntu 24.04) and 9.0 are tested; libx264 is needed for H.264), for `encoder-ffmpeg`. On Debian/Ubuntu: `libavcodec-dev libavformat-dev libavutil-dev libswscale-dev`.

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

# Audio mixer (live inputs and/or test tones; no audio is stored)
cargo run --release -p streamforge-cli -- mix --input <name>=-6 --tone 1000@-12 --duration-ms 10000

# Compositor (1920x1080@60 test scene; --screen adds a live capture layer)
cargo run --release -p streamforge-cli -- --output-dir /tmp compose --duration-ms 5000 --snapshot frame.ppm

# Recording (H.264 + AAC in Matroska; never overwrites an existing file)
cargo run --release -p streamforge-cli -- --output-dir /tmp record --file take.mkv --duration-ms 10000 [--screen] [--input <name>]
```

StreamForge is not a replacement for OBS yet.

## Licensing

The StreamForge source code is licensed as stated in `Cargo.toml` and `LICENSE`. Builds link the system FFmpeg libraries dynamically, so a binary is also subject to the license of the FFmpeg build it links. With libx264 (`--enable-gpl`), as in most distribution packages, that is the GPL. Anyone distributing binaries must comply with it (ADR 0005).

## Repository layout

- `crates/media-core`: platform-independent media types and time model
- `crates/capture-api`: platform-neutral capture interface and diagnostics
- `crates/capture-linux`: Linux capture backend (XDG portal + PipeWire)
- `crates/capture-platform`: selects the capture backend for the target OS
- `crates/video-compositor`: scene graph, CPU renderer and render loop
- `crates/audio-engine`: mixer, meters, test tones and mixer thread
- `crates/encoder-api`: recording settings, statistics and errors
- `crates/encoder-ffmpeg`: recorder (H.264 + AAC → Matroska via FFmpeg)
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
