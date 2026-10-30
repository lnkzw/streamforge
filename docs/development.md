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
- Windows: the MSVC build tools with the Windows SDK (the desktop embeds its icon through the SDK's resource compiler), and a shared FFmpeg build with headers and import libraries (for example BtbN's `win64-gpl-shared`; the package's own from `packaging/windows/build-ffmpeg.sh` builds the programs but lacks the demuxers and decoders the tests use). Set `FFMPEG_DIR` to it and put its `bin` on `PATH`; `LIBCLANG_PATH` must point to LLVM. The binaries link the C runtime statically (`.cargo/config.toml`).

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

`packaging/linux/package.sh <distro>` builds the release binaries, checks them on the system it runs on, and packs `dist/streamforge-<version>-x86_64-linux-<distro>.tar.gz` with the launcher, icon, documentation and `install.sh`. The release workflow (`.github/workflows/release.yml`) runs it on Ubuntu 24.04 and Arch Linux; a pushed `v*` tag creates a draft release from both. Its notes are the version's section of `CHANGELOG.md` (`packaging/release-notes.sh <version>`); every run checks that the workspace version has one, so add the section before tagging.

The Windows package (ADR 0020) takes two steps. `packaging/windows/build-ffmpeg.sh <dir>` builds the FFmpeg it ships on Linux with mingw-w64, from the pinned archives in `packaging/ffmpeg/sources.txt` (`packaging/ffmpeg/fetch-sources.sh` downloads and checks them). On Ubuntu 24.04 it needs `sudo apt install gcc mingw-w64 nasm cmake ninja-build make pkg-config curl xz-utils`; on another system, run it in an `ubuntu:24.04` container with those packages. Then, on Windows, `packaging/windows/package.ps1 -FFmpeg <dir>` (PowerShell 7) builds the binaries against it, checks what they import, smoke-tests them and `install.ps1`, and packs `dist/streamforge-<version>-x86_64-windows.zip`. The release workflow runs both and attaches the source archives to the draft release. It caches the FFmpeg build by its script and sources, which helps reruns of a pull request; a tag run builds it again from the same sources (caches made for a pull request are not shared with tags).

The macOS package (ADR 0020) builds on an arm64 Mac with Xcode 26 or newer and `brew install pkgconf`. `packaging/macos/build-ffmpeg.sh <dir>` builds its FFmpeg from the same archives, for macOS 14.2; `packaging/macos/package.sh <dir>` builds the binaries against it, puts them with the libraries into `StreamForge.app`, checks every Mach-O file (architecture, minimum macOS, linked libraries, run paths), signs the app ad hoc, runs `packaging/macos/smoke.sh` and packs `dist/streamforge-<version>-arm64-macos.zip`. The release workflow runs both on `macos-15`, with the same cache, and the smoke test again on `macos-14` with the downloaded package. The app's icon, `crates/streamforge-desktop/assets/icon/macos.icns`, is rendered from the SVG (the command is in the SVG).

Every package carries `LICENSE-RUST-CRATES.txt`, the licences of the Rust crates the programs are built from: `packaging/licenses/rust-crates.txt`, generated from `Cargo.lock` by `packaging/licenses/generate.py` (Python 3.9 or newer) with cargo-about 0.9.2 (`cargo install cargo-about --version 0.9.2 --locked`). After a change to `Cargo.lock`, including a version bump, run it and commit the result; CI runs it with `--check` and fails when the file is out of date. It also fails on a crate under a licence not listed in `packaging/licenses/about.toml` (check that the licence is compatible with version 3 of the GPL before adding it there), on anything cargo-about reports, such as a clarification in `about.toml` whose file changed (check the new file, then its checksum), and on a crate whose licence needs a copyright notice it does not ship: take the notice from the crate's repository at the commit it was published from (`.cargo_vcs_info.json`) into `packaging/licenses/notices/` (or, when there is none, a note saying so), whose files start with a `Crates:` and a `Source:` line. A notice there names exact versions, so a new version fails until its notice is checked again. `notices/` also holds notices for code or documents that crates took from others, as their source files say (libm, AccessKit and winit among them); the script does not search source files for them. They were audited for the versions in `Cargo.lock` before 0.3.0 (ADR 0020), so check the source headers of a new dependency, or of a new version of one, by hand, for example with `grep -rniE 'copyright|derived from|adapted from|ported from|based on' src` in its source.

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
- `crates/quit-signal`: Ctrl+C and SIGTERM handling, so the programs finalize their outputs
- `crates/streamforge-engine`: headless engine process serving the protocol on stdio
- `crates/streamforge-cli`: CLI (calls the same command handlers)
- `crates/streamforge-desktop`: desktop UI (egui/eframe) over the command API
- `crates/streamforge-mcp`: MCP server for agents, with access levels
- `crates/streamforge-bench`: measurement harness for render, drop and CPU figures
- `crates/streamforge-soak`: soak test harness (an engine process under load for an hour)
- `packaging/linux`: release packaging, launcher and installer
- `packaging/windows`: the Windows package, its FFmpeg build and `install.ps1`
- `packaging/macos`: the macOS package (`StreamForge.app`), its FFmpeg build, `Info.plist` and smoke test
- `packaging/ffmpeg`: the pinned FFmpeg sources of the Windows and macOS packages
- `packaging/licenses`: the licences of the Rust crates, which every package carries, and how they are generated
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
