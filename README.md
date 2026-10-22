# StreamForge

An experimental, cross-platform, agent-friendly streaming application. The project aims to build its own real-time media graph and application workflow rather than wrapping OBS/libobs.

## Current status

This repository is an **architecture and workflow scaffold**, not a working streaming engine yet. In place so far:

- M0 (workspace) and M1 (command boundary): typed commands, a headless engine process, and a local JSON-lines control protocol.
- M2 (Linux capture spike): screen capture through the XDG portal and PipeWire, plus audio input capture. Both run as short diagnostic probes.
- M3 (first compositor): CPU compositor with scene graph, a 1080p/60 render loop on its own thread, and continuous screen capture as a live source.
- M4 (audio mixer): a 48 kHz stereo mixer with gain, mute, meters and an explicit clipping and drift policy. It takes continuous audio capture and test tones.
- M5 (first recording, Linux): the composited scene and the audio mix are recorded to H.264 + AAC in Matroska through the FFmpeg libraries.
- M6 (live output, Linux): RTMP streaming with reconnects, running alongside a recording; the two fail independently.
- M7 (desktop UI): scenes and sources edited live, preview, mixer with meters, and recording/stream controls in an egui app on top of the command API.
- M8 (agent integration): an MCP server exposing the commands as tools, read-only by default. More access must be granted explicitly, and agents never see stream keys.

## Prerequisites

- Rust stable toolchain, 1.95 or newer (`rustup`, `cargo`, `rustfmt`, `clippy`; eframe needs 1.95)
- Git
- Linux: PipeWire headers (≥ 0.3.44), `clang`/libclang and `pkg-config`, for `capture-linux`. Runtime needs PipeWire and xdg-desktop-portal with a ScreenCast backend. See `docs/capture-linux.md`.
- FFmpeg libraries for `encoder-ffmpeg` (FFmpeg 6.1 or newer with libx264; 6.1, 8.1 and 9.0 are tested):
  - Linux: development packages, on Debian/Ubuntu `libavcodec-dev libavformat-dev libavutil-dev libswscale-dev`.
  - macOS: `brew install ffmpeg pkgconf`.
  - Windows: a shared build with headers and import libraries (e.g. BtbN's `win64-gpl-shared`). Set `FFMPEG_DIR` to it and put its `bin` on `PATH`; `LIBCLANG_PATH` must point to LLVM.
  - See `docs/platforms.md`, which also has the capability matrix per OS (screen capture is Linux-only so far; audio input works on all three).

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
cargo run -p streamforge-cli -- capture screen --duration-ms 5000      # opens the system picker (Linux)
cargo run -p streamforge-cli -- capture sources                         # monitors and windows by id (Windows, macOS)
cargo run -p streamforge-cli -- record --file take.mkv --video-encoder hardware  # GPU encoder or an error saying why
cargo run -p streamforge-cli -- capture screen --source-id 'monitor:\\.\DISPLAY1'  # no picker
cargo run -p streamforge-cli -- capture audio --input <name> --duration-ms 3000

# Audio mixer (live inputs and/or test tones; no audio is stored)
cargo run --release -p streamforge-cli -- mix --input <name>=-6 --tone 1000@-12 --duration-ms 10000

# Compositor (1920x1080@60 test scene; --screen adds a live capture layer)
cargo run --release -p streamforge-cli -- --output-dir /tmp compose --duration-ms 5000 --snapshot frame.ppm

# Recording (H.264 + AAC in Matroska; never overwrites an existing file)
cargo run --release -p streamforge-cli -- --output-dir /tmp record --file take.mkv --duration-ms 10000 [--screen] [--input <name>]

# MCP server for agents (stdio). Read-only unless you allow more; streaming
# only to the target you configure, with the key from an environment variable.
cargo build --release -p streamforge-mcp
claude mcp add streamforge -- "$PWD/target/release/streamforge-mcp" --allow control --output-dir /tmp
#   --allow sensitive --stream-url rtmp://live.example.com/app --stream-key-env STREAMFORGE_STREAM_KEY

# Desktop UI (scenes, preview, mixer, record and stream controls)
cargo run --release -p streamforge-desktop -- --output-dir /tmp

# Streaming over RTMP; the key comes from an environment variable, never from the command line
export STREAMFORGE_STREAM_KEY=…   # e.g. read it with `read -rs STREAMFORGE_STREAM_KEY`
cargo run --release -p streamforge-cli -- --output-dir /tmp stream --url rtmp://live.example.com/app --duration-ms 60000 [--record take.mkv]
```

StreamForge is not a replacement for OBS yet.

## Licensing

The StreamForge source code is licensed as stated in `Cargo.toml` and `LICENSE`. Builds link the system FFmpeg libraries dynamically, so a binary is also subject to the license of the FFmpeg build it links. With libx264 (`--enable-gpl`), as in most distribution packages, that is the GPL. Anyone distributing binaries must comply with it (ADR 0005).

## Repository layout

- `crates/media-core`: platform-independent media types and time model
- `crates/capture-api`: platform-neutral capture interface and diagnostics
- `crates/capture-linux`: Linux capture backend (XDG portal + PipeWire)
- `crates/capture-cpal`: audio capture on Windows and macOS (cpal)
- `crates/capture-windows`: screen capture on Windows (Windows.Graphics.Capture)
- `crates/capture-macos`: screen capture on macOS (ScreenCaptureKit)
- `crates/capture-platform`: selects the capture backend for the target OS
- `crates/video-compositor`: scene graph, CPU renderer and render loop
- `crates/audio-engine`: mixer, meters, test tones and mixer thread
- `crates/encoder-api`: recording and streaming settings, secret stream key, statistics and errors
- `crates/encoder-ffmpeg`: recorder (H.264 + AAC → Matroska) and streamer (→ FLV over RTMP) via FFmpeg
- `crates/rtmp-test-server`: in-process RTMP ingest for tests
- `crates/streamforge-desktop`: desktop UI (egui/eframe) over the command API
- `crates/streamforge-mcp`: MCP server for agents, with access levels
- `crates/command-api`: typed commands, registry, structured results/errors, `CommandHandler` trait
- `crates/project-file`: project file format (scenes, ids, audio setup), its checks and atomic writes
- `crates/engine`: engine controller implementing the command handlers
- `crates/control-protocol`: JSON-lines control protocol v1 (transport-agnostic)
- `crates/streamforge-engine`: headless engine process serving the protocol on stdio
- `crates/streamforge-cli`: CLI entry point (calls the same command handlers)
- `crates/streamforge-bench`: measurement harness for render, drop and CPU figures
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
