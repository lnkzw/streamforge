# ADR 0009: Outputs and audio input on Windows and macOS (M9a)

Status: Accepted (M9a; screen capture and hardware encoders follow in M9b and M9c)

## Context

Until M8, only the platform-neutral parts ran everywhere: commands, protocol, compositor, mixer, UI and MCP. Recording, streaming and all capture were Linux-only, and Windows and macOS builds reported `unsupported`. M9 asks for Windows and macOS backends, hardware encoding where it helps, a capability matrix, and native CI tests ("capture/recording integration tests run on available native runners; limitations are explicit").

Native capture (Windows.Graphics.Capture or DXGI, ScreenCaptureKit) is large, platform-specific FFI work. It can only be verified on real machines and, partially, on CI runners. So M9 is split into steps, each with its own native CI verification:

- **M9a:** outputs and audio input on all three OSes.
- **M9b:** native screen capture.
- **M9c:** hardware encoders.

## Decision (M9a)

### Outputs everywhere

`encoder-ffmpeg`, `rtmp-test-server` and every output test now build and run on Linux, Windows and macOS; there are no `unsupported` stubs left. `ffmpeg-sys-next` finds FFmpeg differently on each OS:

| OS | Source of FFmpeg | How it is found |
|----|------------------|-----------------|
| Linux | distribution packages | pkg-config |
| macOS | Homebrew (`brew install ffmpeg pkgconf`) | pkg-config |
| Windows | a prebuilt shared build (CI: BtbN `ffmpeg-n8.1-latest-win64-gpl-shared`) | `FFMPEG_DIR` (headers, import libraries); DLLs from `PATH`; bindgen via `LIBCLANG_PATH` |

The Windows archive is checked in CI against the release's own `checksums.sha256` file. That proves integrity of the download, not independent provenance. Pinning a dated build would break when BtbN prunes old releases, so CI follows the stable 8.1 branch.

Windows paths work with the recorder's `file:` URL prefix. The test that a `pipe:` name stays a file is Unix-only, since `:` is not valid in Windows file names.

### Audio input on Windows and macOS through `cpal`

- **Backend:** `capture-cpal` uses `cpal` 0.18 (WASAPI, CoreAudio). `cpal` is a mature, widely used crate and gives one implementation for both platforms instead of two FFI layers. The dependency exists only on Windows and macOS; on Linux, PipeWire stays.
- **Threading:** each stream owns a thread that builds, plays and finally drops the `cpal` stream (not `Send` on every host). The data callback runs on the OS real-time audio thread (CoreAudio IOProc, WASAPI's audio thread), so it never allocates and never waits for a lock:
  - it converts the device format with `Converter` into a preallocated scratch buffer
  - it hands the result to the bounded `AudioRing` with `try_push`. While the mixer holds the ring's lock, frames wait, in order, in a preallocated pending buffer (one second) for the next callback; only if that fills are frames dropped, counted as ring overrun. Linux producers are not real-time threads and keep the blocking `push`.
  - it records statistics of the converted stereo stream with `try_lock` only; frames delivered while the statistics were busy (possible only at teardown) are counted and do not show up as a deficit
- **Conversion:** WASAPI shared mode and CoreAudio deliver the device's own format, which on Linux PipeWire would have converted.
  - The backend captures at the rate the device runs at now (its default configuration), preferring `f32` and stereo at that rate, and resamples linearly to 48 kHz if needed. Requesting 48 kHz instead would make CoreAudio switch the device's rate for every app, or fail while another app uses the device.
  - Mono is duplicated to both channels, and from multichannel input the first two channels are used.
  - Every conversion is reported as a warning, and digital silence gets a hint about macOS microphone permission.
- **Inputs and errors:** inputs are selected by the host's device id (or display name). `cpal` error kinds map to capture error codes: permission, not found, unavailable, unsupported, invalid request, backend.
- **Screen capture:** on these platforms it reports `unsupported`, with a reason, until M9b.

### Capability matrix and CI

- **Documentation:** `docs/platforms.md` documents capabilities, build requirements and verification per OS, and `get_capture_capabilities` reports them at runtime.
- **CI:**
  - Every push to `main` and every pull request runs on all three runners. (While the repository was private, pull requests ran on Linux only unless labeled `cross-platform`, to save minutes. It is public since 2026-10-21, standard runners are free, and Windows- or macOS-only code no longer merges untested.)
  - FFmpeg is installed on all three runners.
  - Recording (decode-checked files), streaming (in-process RTMP: connect, reconnect, give-up), the engine, desktop-model and MCP acceptance tests (including SIGTERM finalization) run natively on each.
  - The cpal tests run on Windows and macOS, and a step prints each runner's capture capabilities.

## Verification

At the last complete CI run before Actions minutes ran out:
- **Windows:** passed everything, including the FFmpeg tests, the cpal tests and all media acceptance tests.
- **macOS:** passed everything except the recording acceptance test's throughput floor. The 3-core Intel VM skips about a third of the 1080p60 compositor ticks; they are counted, and the accounting and content checks passed. On macOS the floor is now "at least a third" (a guard against total failure); throughput is a performance question, not a correctness one. Linux and Windows keep three quarters.

Runners have no microphone, so on Windows the `not_found` path was exercised. Live audio capture on Windows and macOS still needs a manual check on a real machine.

## Consequences

- Recording and streaming work on all three OSes; CI exercises them for real, not just by compiling.
- Audio quality on Windows and macOS depends on the device running at 48 kHz; otherwise (e.g. a 44.1 kHz microphone) the linear resampler softens high frequencies.
- Windows users need an FFmpeg shared build, and distributing binaries means shipping its DLLs under its license (GPL for builds with libx264).
- The Windows FFmpeg build follows BtbN's rolling 8.1 release, so CI may link a newer 8.1 build from one run to the next. Pinning a dated build with a hard-coded hash would also prove provenance, but breaks whenever BtbN prunes that release.
