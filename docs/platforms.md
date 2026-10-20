# Platforms

What each operating system supports, how it is built, and how it is verified. Capabilities are discovered at runtime (`get_capture_capabilities`), so clients never have to assume a feature exists.

## Capability matrix

| Capability | Linux | Windows | macOS |
|------------|-------|---------|-------|
| Screen capture | XDG ScreenCast portal + PipeWire (monitor, window, virtual; picker) | not yet (M9b) | not yet (M9b) |
| Audio input | PipeWire (format converted by PipeWire) | WASAPI through `cpal` (converted to 48 kHz stereo in the callback) | CoreAudio through `cpal` (same conversion) |
| Compositor, mixer, preview | yes | yes | yes |
| Recording (H.264 + AAC in Matroska) | FFmpeg (system packages) | FFmpeg (prebuilt shared build) | FFmpeg (Homebrew) |
| Streaming (RTMP) | FFmpeg | FFmpeg | FFmpeg |
| Hardware encoders | not yet (M9c) | not yet (M9c) | not yet (M9c) |
| Desktop UI (egui) | yes | yes | yes |
| MCP server, JSON-lines engine, CLI | yes | yes | yes |

Notes:
- **Audio conversion (Windows, macOS):** WASAPI shared mode and CoreAudio deliver the device's own format.
  - The backend picks 48 kHz if the device offers it, preferring `f32` and stereo.
  - Otherwise it resamples linearly, with the same quality note as the mixer's drift correction (ADR 0004).
  - Mono is duplicated to both channels; with more than two channels, the first two are used.
  - Each report warns about these conversions.
- **macOS microphone access:** it needs permission for the app that runs StreamForge (System Settings → Privacy & Security → Microphone). Without it, CoreAudio delivers silence, which the capture report flags.
- **Device names:** audio inputs are selected by the host's device id (as listed by `list_audio_inputs`) or by their display name.

## Building

| | Linux | Windows | macOS |
|-|-------|---------|-------|
| Rust | stable ≥ 1.95 | stable ≥ 1.95 (MSVC) | stable ≥ 1.95 |
| FFmpeg | `libavcodec-dev libavformat-dev libavutil-dev libswscale-dev` (pkg-config) | a shared build with headers and import libraries; `FFMPEG_DIR` points to it, and its `bin` must be on `PATH` at runtime | `brew install ffmpeg pkgconf` |
| bindgen (FFmpeg bindings) | `libclang-dev` | LLVM (`LIBCLANG_PATH`) | Xcode command line tools |
| Capture | `libpipewire-0.3-dev` | — | — |

CI uses the [BtbN FFmpeg builds](https://github.com/BtbN/FFmpeg-Builds) on Windows (`ffmpeg-n8.1-latest-win64-gpl-shared`). The archive is checked against the release's checksum file. Any FFmpeg 6.1 or newer with libx264 works. Binaries that link a GPL build fall under the GPL (ADR 0005).

## Verification

CI (`.github/workflows/ci.yml`) builds and tests on `ubuntu-latest`, `windows-latest` and `macos-15-intel`:

| Test area | Linux | Windows | macOS |
|-----------|-------|---------|-------|
| Unit tests, command and protocol tests | CI | CI | CI |
| Recording: decode-and-check of real files | CI | CI | CI |
| Streaming to an in-process RTMP ingest, reconnect, give-up | CI | CI | CI |
| Engine, desktop-model and MCP acceptance tests | CI | CI | CI |
| Audio input | manual (dev machine); CI runners have no session | CI: the runner's devices if any, else the structured `not_found` path | CI: same; microphone permission cannot be granted on runners |
| Screen capture | manual (needs a human in the picker) | — | — |
