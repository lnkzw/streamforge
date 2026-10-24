# Platforms

What each operating system supports, how it is built, and how it is verified. Capabilities are discovered at runtime (`get_capture_capabilities`), so clients never have to assume a feature exists.

## Capability matrix

| Capability | Linux | Windows | macOS |
|------------|-------|---------|-------|
| Screen capture | XDG ScreenCast portal + PipeWire (monitor, window, virtual; picker only) | Windows.Graphics.Capture (monitors and windows by id from `list_screen_sources`; no picker yet) | ScreenCaptureKit (displays and windows by id from `list_screen_sources`; no picker yet; needs the screen-recording permission) |
| Audio input | PipeWire (format converted by PipeWire); each sink's monitor is an input too (desktop audio) | WASAPI through `cpal` (converted to 48 kHz stereo in the callback) | CoreAudio through `cpal` (same conversion) |
| Monitor playback (ADR 0017) | PipeWire output stream (sinks and duplex devices) | not yet (step 5c, `cpal`) | not yet (step 5c, `cpal`) |
| Compositor, mixer, preview | yes | yes | yes |
| Recording (H.264 + AAC in Matroska) | FFmpeg (system packages) | FFmpeg (prebuilt shared build) | FFmpeg (Homebrew) |
| Streaming (RTMP) | FFmpeg | FFmpeg | FFmpeg |
| Hardware encoders (`video_encoder: auto` tries them first, ADR 0011) | VAAPI (Intel, AMD; verified with an AMD RX 6800), NVENC | NVENC, Quick Sync, AMF | VideoToolbox |
| Desktop UI (egui) | yes | yes | yes |
| MCP server, JSON-lines engine, CLI | yes | yes | yes |

Notes:
- **Audio conversion (Windows, macOS):** WASAPI shared mode and CoreAudio deliver the device's own format.
  - The backend captures at the device's current rate (never switching it, which would affect other apps), preferring `f32` and stereo.
  - Unless that rate is 48 kHz, it resamples linearly, with the same quality note as the mixer's drift correction (ADR 0004).
  - Mono is duplicated to both channels; with more than two channels, the first two are used.
  - Each report warns about these conversions.
- **macOS microphone access:** it needs permission for the app that runs StreamForge (System Settings → Privacy & Security → Microphone). Without it, CoreAudio delivers silence, which the capture report flags.
- **Device names:** audio inputs are selected by the host's device id (as listed by `list_audio_inputs`) or by their display name.
- **Choosing a screen source:** `get_capture_capabilities` says how (ADR 0010). With `picker` (Linux) the user picks in the system dialog. With `source_list` (Windows) `list_screen_sources` returns ids such as `monitor:\\.\DISPLAY1` or `window:4242:0x2a0b6` (process id and window handle), and screen commands take one as `source_id`; the desktop UI shows that list.
- **macOS screen capture:** needs macOS 14 or newer and the screen-recording permission for the app that runs StreamForge (System Settings → Privacy & Security → Screen & System Audio Recording). Without it, listing and capturing fail with `permission_denied` and that remedy. `get_capture_capabilities` does not ask, because asking can show the system prompt. Display ids are `display:<CGDirectDisplayID>`, window ids `window:<CGWindowID>`.
- **Windows screen capture:** needs Windows 10 version 1903 or newer. Windows draws a border around captured content (its privacy indicator). Frames arrive when the content changes; a static screen delivers few. Window ids are valid while the window exists; a closed window ends its capture with a warning.

## Building

| | Linux | Windows | macOS |
|-|-------|---------|-------|
| Rust | stable ≥ 1.95 | stable ≥ 1.95 (MSVC) | stable ≥ 1.95 |
| FFmpeg | `libavcodec-dev libavformat-dev libavutil-dev libswscale-dev` (pkg-config) | a shared build with headers and import libraries; `FFMPEG_DIR` points to it, and its `bin` must be on `PATH` at runtime | `brew install ffmpeg pkgconf` |
| bindgen (FFmpeg bindings) | `libclang-dev` | LLVM (`LIBCLANG_PATH`) | Xcode command line tools |
| Capture | `libpipewire-0.3-dev` | — | Xcode 26 or newer (`screencapturekit` builds a Swift bridge against the macOS 26 SDK). Binaries need the rpath `/usr/lib/swift`, which `.cargo/config.toml` adds; a `RUSTFLAGS` variable replaces that setting, so add `-C link-arg=-Wl,-rpath,/usr/lib/swift` to it. |

CI uses the [BtbN FFmpeg builds](https://github.com/BtbN/FFmpeg-Builds) on Windows (`ffmpeg-n8.1-latest-win64-gpl-shared`). The archive is checked against the release's checksum file. Any FFmpeg 6.1 or newer with libx264 works. Binaries that link a GPL build fall under the GPL (ADR 0005).

## Verification

CI (`.github/workflows/ci.yml`) builds and tests every push and pull request on `ubuntu-latest`, `windows-latest` and `macos-15-intel`:

| Test area | Linux | Windows | macOS |
|-----------|-------|---------|-------|
| Unit tests, command and protocol tests | CI | CI | CI |
| Recording: decode-and-check of real files | CI | CI | CI |
| Streaming to an in-process RTMP ingest, reconnect, give-up | CI | CI | CI |
| Engine, desktop-model and MCP acceptance tests | CI | CI | CI |
| Audio input | manual (dev machine); CI runners have no session | CI: the runner has no input, so the structured `not_found` path; live capture still needs a manual check | CI: the runner's devices if any, else the structured error; microphone permission cannot be granted on runners |
| Hardware encoding | manual: VAAPI on the development machine (RX 6800, Mesa); NVENC untested. CI: no GPU; NVENC and VAAPI fail to open, and `auto` falls back to libx264 | CI: no GPU; NVENC, Quick Sync and AMF fail to open, fallback to libx264 | CI: VideoToolbox does not open on the VM, fallback to libx264 |
| Screen capture | manual (needs a human in the picker) | CI: lists sources, captures the runner's monitor (frames of its size), probes it, and records it from a scene through the engine process | CI: lists the runner's display, captures and probes it, and records it from a scene through the engine process (the runner image allows screen recording) |

At the first complete CI run (ADR 0009), every test passed on Windows. On macOS, every test passed except a throughput floor, which on macOS is now only a guard against total failure: the macOS VM skips some compositor ticks at 1080p60, and those skips are counted.
