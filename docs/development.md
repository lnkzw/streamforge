# Development

How to build, test and change StreamForge. For using it, see the [user guide](user-guide.md). Coding agents start with [`AGENTS.md`](../AGENTS.md).

## Prerequisites

- Rust stable, 1.95 or newer (`rustup`, `cargo`, `rustfmt`, `clippy`; eframe needs 1.95). Install it with the official instructions: <https://www.rust-lang.org/tools/install>.
- Git.
- FFmpeg libraries for `encoder-ffmpeg`: FFmpeg 6.1 or newer with libx264 (6.1, 8.1 and 9.0 are tested).
- Linux:
  - Debian and Ubuntu: `sudo apt install build-essential pkg-config libclang-dev libpipewire-0.3-dev libavcodec-dev libavformat-dev libavutil-dev libswscale-dev`
  - Arch Linux: `sudo pacman -S --needed base-devel clang pkgconf ffmpeg pipewire`
  - Runtime: PipeWire and xdg-desktop-portal with a ScreenCast backend ([`capture-linux.md`](capture-linux.md)).
- macOS: `brew install ffmpeg pkgconf`, and Xcode 26 or newer for ScreenCaptureKit.
- Windows: a shared FFmpeg build with headers and import libraries (for example BtbN's `win64-gpl-shared`). Set `FFMPEG_DIR` to it and put its `bin` on `PATH`; `LIBCLANG_PATH` must point to LLVM.

[`platforms.md`](platforms.md) has the details per system and the capability matrix.

## Build and test

```bash
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same on Linux, Windows and macOS for every pull request (`.github/workflows/ci.yml`). [`testing.md`](testing.md) describes the test layers, the measurement harness (`streamforge-bench`) and the soak test (`streamforge-soak`).

Run the programs from the workspace:

```bash
cargo run --release -p streamforge-desktop -- --output-dir /tmp --project-dir ~/shows
cargo run --release -p streamforge-cli -- status
printf '%s\n' '{"id":1,"command":"get_status"}' | cargo run -q -p streamforge-engine
cargo build --release -p streamforge-mcp   # then point an MCP client at target/release/streamforge-mcp
```

## Release packages

`packaging/linux/package.sh <distro>` builds the release binaries, checks them on the system it runs on, and packs `dist/streamforge-<version>-x86_64-linux-<distro>.tar.gz` with the launcher, icon, documentation and `install.sh`. The release workflow (`.github/workflows/release.yml`) runs it on Ubuntu 24.04 and Arch Linux; a pushed `v*` tag creates a draft release from both.

## Repository layout

- `crates/media-core`: platform-independent media types and time model
- `crates/capture-api`: platform-neutral capture interface and diagnostics
- `crates/capture-linux`: Linux capture backend (XDG portal + PipeWire)
- `crates/capture-cpal`: audio capture and playback on Windows and macOS (cpal)
- `crates/capture-windows`: screen capture on Windows (Windows.Graphics.Capture)
- `crates/capture-macos`: screen capture on macOS (ScreenCaptureKit)
- `crates/capture-platform`: selects the capture backend for the target OS
- `crates/video-compositor`: scene graph, CPU renderer and render loop
- `crates/audio-engine`: audio graph (channels, buses, sends), mixer thread, meters, test tones, monitor playback
- `crates/encoder-api`: recording and streaming settings, secret stream key, statistics and errors
- `crates/encoder-ffmpeg`: recorder (H.264 + AAC → Matroska) and streamer (→ FLV over RTMP) via FFmpeg
- `crates/rtmp-test-server`: in-process RTMP ingest for tests
- `crates/command-api`: typed commands, registry, structured results and errors, `CommandHandler` trait
- `crates/project-file`: project file format, its checks, migrations and atomic writes
- `crates/engine`: engine controller implementing the command handlers
- `crates/control-protocol`: JSON-lines control protocol v1 (transport-agnostic)
- `crates/streamforge-engine`: headless engine process serving the protocol on stdio
- `crates/streamforge-cli`: CLI (calls the same command handlers)
- `crates/streamforge-desktop`: desktop UI (egui/eframe) over the command API
- `crates/streamforge-mcp`: MCP server for agents, with access levels
- `crates/streamforge-bench`: measurement harness for render, drop and CPU figures
- `crates/streamforge-soak`: soak test harness (an engine process under load for an hour)
- `packaging/linux`: release packaging, launcher and installer
- `docs/`: user guide, architecture, media model, control protocol, platforms, roadmap, testing strategy, decisions (ADRs in `docs/decisions`), UI design system (`docs/ui`)

## Rules

Read [`AGENTS.md`](../AGENTS.md) before making changes. In short:

1. Cross-platform APIs first; platform-specific backends behind explicit interfaces.
2. No OBS/libobs dependency in the media engine.
3. Audio and video have explicit timestamps and bounded queues.
4. The engine is controllable headlessly through the same command API the desktop app and the MCP server use.
5. Every milestone has an executable acceptance test.
6. A dependency or a new subsystem is documented with its reason before it is added.

[`architecture.md`](architecture.md) describes the layers and the dependency rules, [`roadmap.md`](roadmap.md) the milestones, and `docs/decisions` the architecture decisions.
