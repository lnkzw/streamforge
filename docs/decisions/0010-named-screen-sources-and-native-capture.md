# ADR 0010: Named screen sources; Windows and macOS screen capture (M9b)

Status: Accepted (M9b: Windows and macOS)

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
  - `window:<process id>:<handle in hex>`, valid while the window exists. Windows reuses handle values; with the process id in the id, a stored id whose handle now belongs to another process's window fails with `not_found` instead of capturing that window.
  - The id format is pure code and is unit-tested on every OS.
- **Unavailable capture:** `check_supported` first checks `available` and returns the backend's reason, so a client is not sent back and forth between "omit `source_id`" and "name a source".
- **Linux:** the portal backend rejects a `source_id` before any D-Bus call, so a named source added during a recording fails at once instead of blocking the engine thread.
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

### macOS backend

- **Crate:** `capture-macos` builds on `screencapturekit` =11.0.0 (MIT OR Apache-2.0), with the `macos_14_0` feature for the filter's pixel size.
  - The crate wraps ScreenCaptureKit behind a safe API. Its build compiles a Swift bridge, which needs Xcode 26 or newer: the bundled Metal bindings use the macOS 26 SDK.
  - Cargo does not pass a dependency's link arguments to the binaries that link it. The workspace's `.cargo/config.toml` therefore adds the rpath of the system Swift runtime (`/usr/lib/swift`) to every macOS binary.
  - The one `unsafe` call, reading a locked `CVPixelBuffer` as a slice, sits in `pixels.rs` with its invariants: a read-only lock held by the borrowed guard, and the sample buffer held while copying.
  - Rejected alternative: the `objc2-screen-capture-kit` bindings. They need no Swift toolchain, but would need far more `unsafe` code of our own (a protocol class, completion blocks, CoreMedia/CoreVideo calls). The trade-off is a crate with frequent major releases, pinned exactly and checked against CI on every update.
- **Ids:** `display:<CGDirectDisplayID>` and `window:<CGWindowID>`. Window numbers are not reused while the window server runs. Listed windows are those on layer 0 (normal app windows) with a title, named "title (application)".
- **Permission:** every query needs the screen-recording permission of the app that runs StreamForge.
  - Without it, listing and capturing fail with `permission_denied` and the remedy.
  - `get_capture_capabilities` does not ask, because asking can show the system prompt.
  - Listing can be allowed while capturing is not; clients handle both.
- **Frames:**
  - The stream is configured in pixels from `SCShareableContentInfo` (points × scale), so Retina displays are captured at full resolution.
  - BGRA at most 60 FPS (ScreenCaptureKit sends fewer frames while nothing changes), queue depth 5.
  - Complete frames go through the shared `FrameSink`; idle updates count as empty buffers.
- **End of capture:** a delegate records when ScreenCaptureKit stops a stream itself (window closed, display gone, permission revoked); the report says why.
- **Timeouts:** queries, `start_capture` and `stop_capture` run on a helper thread bounded by 5 s. A named source started during a recording therefore delays the engine thread by at most that bound; a picker would wait for a human.

### Shared delivery

`capture_api::FrameSink` holds what both native backends do alike:
- the pooled copy and publish
- the statistics and the negotiated format
- why the system ended the capture, and the first delivery errors
- the report

The backends only map frames and translate their system's callbacks. It is unit-tested on every OS: stride, pool exhaustion, report contents.

## Verification

On the `windows-latest` CI runner, Windows.Graphics.Capture is available (the CI step prints the capabilities):
- `capture-windows/tests/native.rs` lists the sources and captures the first monitor. The test checks that frames arrive at the listed size and that the report counts them. It also probes the monitor and checks structured errors: `invalid_params` without a source, `not_found` for unknown ids, `unsupported` for the metadata cursor where cursor modes exist.
- `streamforge-engine/tests/screen.rs` names that monitor in a scene and records it through the engine process. The scene capture reports frames, the recording encodes video, and the file exists.
- On the `macos-15-intel` runner the image allows screen recording. `capture sources` lists the 1920×1080 display. `capture-macos/tests/native.rs` captures and probes it, and the engine acceptance test records it from a scene. The tests also accept `permission_denied`, the outcome on Macs without the permission.
- On Linux the same engine test checks the structured `unsupported` error of `list_screen_sources`.

## Consequences

- Windows has native screen capture, and automated tests cover it on real hardware paths, not only by compiling.
- There is no system picker on Windows or macOS yet; clients choose from the list (the desktop UI shows it).
- Building on macOS needs Xcode 26 or newer, and the rpath setting must survive custom `RUSTFLAGS`.
- Each frame is copied twice on the way to the compositor: GPU to a staging texture inside `windows-capture`, which creates and maps a new staging texture for every frame, then into the pool. A reused staging texture, or GPU-side composition, would avoid that and is future work; at 1080p on the CI runner it keeps up.
- When a captured window closes, its capture ends. The compositor keeps the last frame, and the report says so only when the output stops (`the captured window was closed`). Live capture state in `get_status` is future work.
- Listed window sizes come from the window rectangle and are approximate (borders, display scaling); capture reports carry the real frame size.
- WGC delivers frames when content changes. A static screen yields few frames; the compositor keeps showing the newest one.
- The dependency adds `windows-capture` and its transitive crates (`windows`, `rayon`, `parking_lot`) on Windows only.
- Since M15 step 4b (ADR 0020), macOS 14 or newer has a system picker too (`SCContentSharingPicker`), with these rules; Windows still has none.
