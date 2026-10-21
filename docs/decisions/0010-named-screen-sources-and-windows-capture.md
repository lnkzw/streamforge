# ADR 0010: Named screen sources and Windows screen capture (M9b)

Status: Accepted (M9b, Windows; macOS ScreenCaptureKit follows)

## Context

Until M9a, screen capture existed only on Linux, and only through the XDG ScreenCast portal: a human picks the source in a system dialog every time. That model does not carry over to Windows and macOS:

- **Windows:** Windows.Graphics.Capture can capture any monitor or window by handle, without a dialog. Its system picker (`GraphicsCapturePicker`) needs a window to own it, and the `windows-capture` wrapper's picker spins in a busy loop with no timeout or cancellation.
- **macOS:** ScreenCaptureKit lists displays and windows once the app has screen-recording permission. Its picker (macOS 14+) needs an application run loop.
- **Testing:** a picker needs a human, so it cannot be tested on CI. Named sources can.

## Decision

### Named sources in the command API

- **Capabilities:** `ScreenCaptureCapability` gains `picker` (the user picks in a system dialog) and `source_list` (sources can be listed and captured by id). Linux has `picker`, Windows has `source_list`.
- **Listing:** `CaptureBackend::screen_sources` returns `ScreenSource {id, kind, name, width, height}`. Its default implementation is `unsupported`, so the Linux backend and test fakes need no change. The new command `list_screen_sources` exposes it.
- **Selection:** `ScreenSelection` gains `source: Option<String>`. Every screen command takes an optional `source_id`: the probe, `probe_composite`, `start_recording`, `start_streaming` and `add_source` of kind `screen`. Scene sources keep it, and the scene list shows it.
- **Validation:** `ScreenSelection::check_supported` checks the selection against the capability:
  - A named source needs `source_list`; without a name the backend needs `picker`. Otherwise the error says which of the two to use.
  - A named source defines its own kind, so `source_types` is not checked against it.
  - `command-api` rejects ids that are empty, longer than 256 characters or contain control characters.
- **Access:** `list_screen_sources` is `sensitive`, like capturing. Window titles can reveal private content, such as an open mail or a document name.
- **Live changes:** while an output runs, adding a picker source is still refused, because it would block the engine behind a human. Named sources start without waiting and are allowed.
- **Consent:** without a picker, nobody confirms a capture. The consent is the access level the person running the engine or MCP server grants (`sensitive`, ADR 0008). Windows still shows its border around captured content.

### Windows backend

- **Crate:** `capture-windows` builds on `windows-capture` =2.0.1 (MIT). The crate wraps the WinRT and D3D11 plumbing (capture thread, frame pool, staging copy) behind a safe API, so `capture-windows` has no `unsafe` code of its own. Writing it on the `windows` crate directly would mean a few hundred lines of COM/WinRT FFI.
- **Ids:**
  - `monitor:<GDI device name>`, e.g. `monitor:\\.\DISPLAY1`, stable while the monitor is connected; the enumeration index can change.
  - `window:<handle in hex>`, valid while the window exists.
  - The id format is pure code and is unit-tested on every OS.
- **Frames:**
  - The handler maps each frame (BGRA8), copies it with its row pitch into one of 4 pooled buffers and publishes it to the stream's mailbox.
  - When no buffer is free the frame is discarded and counted (`frames_discarded`).
  - Alpha of captured content is not meaningful for a layer, so frames are delivered as `Bgrx8` (opaque).
  - PTS come from the frame's SystemRelativeTime (QPC, 100 ns units).
  - Probes only observe, without mapping or copying.
- **Settings:**
  - The cursor is shown or hidden where the Windows build supports it; otherwise `cursor_modes` is empty and the request is ignored, with a warning.
  - Border, update interval and dirty regions stay at the system defaults; the border is the user's indicator that capture is running.
- **Composition:** `capture-platform` combines WGC screen capture with `cpal` audio through `capture_api::SplitCapture`, which takes each capability and command from its backend.
- **Clients:**
  - CLI: `capture sources`, and `--source-id` for the screen probe, `compose`, `record` and `stream`.
  - Desktop UI: offers the list (or the reason there is none) when a screen source is added.
  - MCP server: picks up the new command from the registry.

## Verification

On the `windows-latest` CI runner, Windows.Graphics.Capture is available (the CI step prints the capabilities):
- `capture-windows/tests/native.rs` lists the sources and captures the first monitor. The test checks that frames arrive at the listed size and that the report counts them. It also probes the monitor and checks structured errors: `invalid_params` without a source, `not_found` for unknown ids, `unsupported` for the metadata cursor where cursor modes exist.
- `streamforge-engine/tests/screen.rs` names that monitor in a scene and records it through the engine process. The scene capture reports frames, the recording encodes video, and the file exists.
- On Linux the same engine test checks the structured `unsupported` error of `list_screen_sources`.

## Consequences

- Windows has native screen capture, and automated tests cover it on real hardware paths, not only by compiling.
- There is no system picker on Windows yet; clients choose from the list (the desktop UI shows it).
- Each frame is copied twice on the way to the compositor: GPU to a staging texture inside `windows-capture`, then into the pool. GPU-side composition would avoid both and is future work.
- WGC delivers frames when content changes. A static screen yields few frames; the compositor keeps showing the newest one.
- The dependency adds `windows-capture` and its transitive crates (`windows`, `rayon`, `parking_lot`) on Windows only.
