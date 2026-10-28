# ADR 0019: Sources and layers

- **Status:** Accepted (reviewed with the project owner on 2026-10-27)
- **Date:** 2026-10-27

## Context

Version 0.1.0 has three source kinds: color bars, a solid color, and screen or window capture. A scene draws its sources in the order they were added, and that order cannot change. Every source is always drawn. For a stream that is too little: most shows need a camera and a logo or an overlay image, and arranging layers is basic scene work.

The roadmap names cameras, images, text, media files and browser sources as the missing kinds. The project owner chose cameras, images and layer control for M14 (2026-10-27); text, media files and browser sources come later.

What already fits:

- **The compositor** takes two kinds of input: `SourceInput::Static(Arc<VideoFrame>)`, a frame that never changes, and `SourceInput::Live(Arc<FrameMailbox>)`, the newest frame of a producer. An image is a static input; a camera is a live one, like a screen capture.
- **The compositor's layer** already has `visible`; the command model does not expose it.
- **Scene captures:** the engine starts a screen source's capture when its scene is rendered and keeps it while the source exists. It reports it in `get_health.captures` and the pipeline report.
- **The assessment** (`docs/architecture/CURRENT_STATE_ASSESSMENT.md` §6.5.2) flagged that `CaptureBackend` mixes screen and audio in one trait, so "adding a source type grows every backend". A camera is that third source type.

Protocol v1 is stable since 0.1.0 (`docs/protocol.md`), so everything below is additive: new commands, new source kinds, new optional parameters, new result fields.

## Decision

### Layers

- **Order:** a new command `move_source {source, index}` moves a source to `index` in its scene's drawing order (0 is the bottom, the last index the top; the others shift). An index past the end is `invalid_params`. It applies live like `update_source` and returns the scene list. Access `control`, destructive like `update_source`. A drag in the desktop is one move.
- **Visibility:** every source gains `visible` (default `true`), set through `update_source` and returned in `list_scenes`. A hidden source is not drawn. Its capture keeps running, so showing it again is immediate. Access `control`. `add_source` takes it too (added in step 2), so a project file's source stays an `add_source` object.

### Image source

- **Kind `image`:** `{"kind": "image", "file": "logo.png"}`. `file` is a plain file name in the engine's project directory (`--project-dir`), under the same file name rule as projects (no folders, valid on every system). Without a project directory, image sources are `invalid_state`.
- **Why the project directory:** the project and its images move together, and a project file holds no paths. An agent can show only what the user put in that folder, so adding an image needs `control`, not `sensitive`. It captures nothing.
- **Listing:** a new command `list_images` (`read_only`) lists the image files in the project directory like `list_projects`: `{"files": [{"file_name", "bytes", "modified_unix_ms", "width", "height"}], "omitted"}`. The size comes from the file header (`null` if unreadable). At most 256 are listed.
- **Formats:** PNG and JPEG. JPEG's EXIF orientation is applied; straight alpha is premultiplied; 16-bit and grey images are converted to 8-bit BGRA. No animation (GIF, APNG), no SVG, no colour management (sRGB is assumed, ADR 0003).
- **Reading:** the file is decoded on the engine thread when the source is added, when a project loads, and on `restart_source`. The file is not watched. Identical files (name, modification time, length) share one frame.
- **Limits:** at most 8192 pixels per side and 16 megapixels per image (64 MiB as BGRA), and 512 MiB for all decoded images together. Beyond a limit, adding is `invalid_params`.
- **Errors:** a missing file is `not_found` and an undecodable one `invalid_params`. Loading a project whose image is missing or broken does not fail: the source loads, draws nothing, and the load reports a warning, so a moved logo does not block a show.
- **Placement:** without a rectangle, an image keeps its own size (scaled down to the canvas if larger) at the top left. With one, it is fitted into it, aspect kept, like a screen.
- **Changing the file** (added in step 3): `update_source` takes `file` for an image source, read like a new one; the rectangle stays. `restart_source` of an image source reads its file again, also while idle, and since a file has one frame, every source showing it updates. Both are additive to v1.

### Camera source

- **Kind `camera`:** `{"kind": "camera", "camera_id": "…", "width": 1280, "height": 720, "fps": {"num": 30, "den": 1}}`. `width` and `height` are optional and come together; `fps` is optional.
- **Listing:** a new command `list_cameras` (`read_only`, like `list_audio_inputs`) returns `{"cameras": [{"camera_id", "name", "modes": [{"width", "height", "fps"}]}]}`.
- **Ids:** `camera_id` is opaque and stable on one machine: the PipeWire `node.name` on Linux, the device's symbolic link on Windows, `AVCaptureDevice.uniqueID` on macOS. Unlike a screen's `capture_target_id`, it is saved in projects: it names a device, not private content.
- **Mode choice:**
  - Without a mode: the largest size up to the canvas (1920×1080) that runs at 30 fps or more, else the largest size; then the highest frame rate up to 60.
  - A requested mode that the camera lacks: the closest one, with a warning naming it.
  - Among formats of the chosen mode: packed RGB, then YUY2 or UYVY, then NV12, then I420, then MJPEG. The format used is reported, not chosen.
- **Access:** adding a camera source is `sensitive`, like a screen source: it captures. `restart_source` reopens a camera, also while recording or streaming, since there is no picker.
- **Lifecycle:**
  - A camera starts when its scene becomes active, also while recording or streaming.
  - It keeps running while its source exists, until the pipeline stops. Switching scenes back and forth does not reopen it. The camera's light stays on while the pipeline runs.
  - Removing the source stops it.
  - A camera that is not found shows nothing, and the command that started it reports a warning. Any other error (busy, permission denied) fails that command, as for screens.
  - An unplugged camera ends like a screen capture (`get_health.captures` with the reason) and its last frame stays, as for screens.
- **Health:** `get_health.captures` and the pipeline report gain the kind `camera` with the negotiated size, rate and format.
- **Placement:** without a rectangle, the whole canvas, fitted with its aspect kept.

### Capture traits

`CaptureBackend` is split into three traits in `capture-api`:

- `ScreenCapture`: capabilities, `screen_sources`, `probe_screen`, `start_screen_stream`.
- `AudioDevices`: capabilities, `audio_inputs`, `audio_outputs`, `probe_audio`, `start_audio_stream`, `start_playback`.
- `CameraCapture`: capability, `cameras`, `start_camera_stream`. A camera stream is a `VideoStream`.

The engine takes a set of the three (`Capture { screen, audio, camera }`). `capture-platform` builds it per system, and every part that a system lacks is an unsupported implementation with a reason. `SplitCapture` and the half-implemented methods that return `unsupported` go away. The split itself changes no behaviour and is its own step.

Implemented in step 4a: each trait answers one capability (`capability`), and the camera's joins `get_capture_capabilities` in step 4b. The traits are `Send`, so `Capture::capabilities` queries screen and audio in parallel, as the Linux backend did before: the screen on a scoped thread, audio on the calling one (cpal's WASAPI host keeps its device enumerator in the COM apartment of the first thread that uses it). `desktop` and `session_type` come from the environment on every system. The Linux parts are `PortalScreen` and `PipeWireAudio`.

### Cameras per system

- **Linux:** PipeWire directly, as the audio inputs already are. Cameras are the `Video/Source` nodes that WirePlumber creates for V4L2 and libcamera devices, told apart from screen-cast nodes by their device API and role. This covers libcamera (MIPI) cameras that V4L2 alone does not, and other applications can use the camera at the same time.
  - The capture runs on its own thread, like a screen capture.
  - PipeWire does not convert video formats on every supported system (Ubuntu 24.04 ships PipeWire 1.0), so StreamForge converts on that thread into pooled frames: YUY2, UYVY, NV12 and I420 to BGRX with the camera's colour matrix (BT.601 limited range when the format does not say), MJPEG through a JPEG decoder.
  - The camera portal (for sandboxed packages) can be added later on the same PipeWire path.
- **Windows:** Media Foundation's source reader in `capture-windows`, with its video processing on, so that the system delivers BGRA (MJPEG included).
- **macOS:** AVFoundation in `capture-macos`, with BGRA output requested from the system. The camera permission prompt belongs to the process (the terminal, or the app bundle with a usage description).

`unsafe` stays in small modules with documented invariants, as for the screen backends.

### New crate `frame-decode`

`frame-decode` turns encoded or YUV pictures into the engine's frames:

- PNG and JPEG files into a premultiplied BGRA `VideoFrame`.
- YUY2, UYVY, NV12 and I420 into a caller's BGRX buffer.
- MJPEG frames into a caller's BGRX buffer.

It depends on `media-core` and on pure-Rust decoders (planned: `png`, `zune-jpeg`; each is added in the step that needs it, with the reason in the PR). The engine uses it for images and `capture-linux` for cameras. On Windows and macOS the system converts.

A JPEG decoder may allocate inside a decode. Capture is not a real-time audio callback, but the frame path otherwise allocates nothing per frame after warm-up. The camera step measures this, and either reuses the decoder's buffers or documents the allocation (AGENTS.md).

### Project file

Schema 4 adds `visible` on every source and the kinds `image` (`file`) and `camera` (`camera_id`, optional mode). Version 3 migrates by setting `visible: true`. A 0.1.0 build refuses a schema 4 file as "newer than this build reads", which is its existing message. The kinds join schema 4 in the steps that add them (additive within the version, as in ADR 0017).

### Desktop

- **Sources dock:** reorder by drag and by keys, an eye toggle per row, a hidden row drawn dimmed.
- **Add-source dialog:** the kinds Image and Camera. Image lists `list_images` and offers to open the project folder in the file manager; there is no file dialog in M14. Camera lists `list_cameras` with a mode choice.
- **Inspector:** shows and changes an image's file, a camera and its mode.

## Alternatives considered

- **Camera methods with defaults on `CaptureBackend`:** the smallest change. Rejected: it keeps the half-implemented backends the assessment flagged, and a fourth kind (application audio) would grow them again.
- **`set_source_order` with the full list of ids:** atomic for any reordering, but every client must send the whole scene, and a drag is a single move anyway.
- **Images by absolute path, through a native file dialog:** familiar from other tools. Rejected for now: a project would hold machine paths and stop being portable, and an agent could show any image file on the machine, which would need `sensitive`. An import into the project folder can come later.
- **A `media/` subfolder:** tidier, but a second folder rule. Plain names next to the projects reuse the existing one.
- **V4L2 directly on Linux:** simpler, but it misses libcamera cameras and holds the device exclusively.
- **The camera portal first:** only needed for sandboxed packages, which StreamForge does not ship. It can come later on the same PipeWire path.
- **Starting every camera with the pipeline:** the first switch to a camera scene would be instant, but every camera in the project would light up even if never shown.
- **`nokhwa` for cameras on every system:** one dependency for three systems, but format handling and permissions would sit outside our tests and error model. Native code matches the screen backends.

## Validation (planned)

- **`frame-decode` unit tests:**
  - every YUV layout and MJPEG against reference pixels, including odd widths and padded strides
  - PNG with alpha (premultiplied), 16-bit, grey and palette images
  - every JPEG EXIF orientation
  - the limits, and truncated or corrupt files, which fail without a panic
- **Engine tests with the test backend's fake camera:**
  - a camera scene activated while recording starts its camera
  - removing the source stops it; `restart_source` reopens it
  - a camera that is not found renders nothing and warns; a busy one fails the command
  - health reports the kind `camera`
  - `move_source` and `visible` render the expected layer order
  - image sources inside and outside the limits; a project with a missing image loads with a warning
  - the access levels: image `control`, camera `sensitive`
- **Project file:** the version 3 to 4 migration, and a round trip with every new field.
- **Wire shapes:** the new commands and kinds in `command-api/tests/wire_shapes.rs`.
- **Linux live test (ignored by default):** a camera from PipeWire's `videotestsrc` node, or a v4l2loopback device fed by FFmpeg, captured for two seconds, delivering frames of the expected size. It runs on the development machine. CI runs it where a runner can host PipeWire; otherwise `docs/testing.md` says it is manual. A v4l2loopback device with `exclusive_caps` gets a PipeWire node only if a producer was attached when WirePlumber enumerated it.
- **Windows and macOS:** CI builds and covers the error paths, since the runners have no camera. Real cameras join the hardware test backlog.
- **Cost:** the conversion time per 1080p frame for YUY2, NV12 and MJPEG, and the render time with a camera and an image in the scene, against the `docs/testing.md` baseline.

## Implementation steps (one PR each)

1. This ADR and the roadmap.
2. **Layers:** `move_source`, `visible`, project file schema 4 with the migration; the desktop's reorder (drag, keys) and eye toggle.
3. **Image source:** `frame-decode` with PNG and JPEG, `list_images`, the `image` kind with its limits and load policy; the desktop dialog and inspector.
4. **Linux camera**, in two parts:
   - **4a:** the capture trait split, without behaviour change.
   - **4b:** `CameraCapture` on PipeWire, the YUV and MJPEG conversion in `frame-decode`, `list_cameras`, the `camera` kind and lifecycle, health; the desktop dialog and inspector.
5. **Windows and macOS cameras:**
   - **5a:** Media Foundation in `capture-windows`.
   - **5b:** AVFoundation in `capture-macos`.

## Decisions taken with the project owner

All on 2026-10-27, each the recommended option:

1. Split `CaptureBackend` into three traits, as its own step.
2. `move_source` with an index, not a full order list.
3. Visibility in M14; a hidden source keeps capturing.
4. Image files by plain name in the project folder, with `list_images`. No file dialog in M14.
5. Linux cameras through PipeWire directly.
6. A camera starts when its scene becomes active (also on air) and runs until the pipeline stops.
7. Native camera code on Windows (Media Foundation) and macOS (AVFoundation).

## Not in M14

Text, media file and browser sources; cropping, flipping and rotating a source; camera controls (exposure, focus, white balance); an image import or file dialog; per-output scenes.
