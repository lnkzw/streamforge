# ADR 0003: CPU reference compositor first

Status: Accepted (M3)

## Context

M3 needs a compositor that layers two or more sources with position, scale and alpha. The render loop must run independently of the UI, the logic must be testable deterministically, and 1080p/60 on the development machine has to be measured. ADR 0001 names wgpu as the rendering direction. `docs/media-model.md` asks for a reliable CPU path first, with optimization driven by profiling.

## Decision

- Build a **CPU reference renderer** (`video-compositor::CpuRenderer`):
  - premultiplied BGRA, composited with integer "over"
  - 8-bit fixed-point bilinear sampling; 1:1 copy for pixel-aligned layers
  - row bands split across scoped threads
  - identical output for any thread count
- Run the **render loop on its own thread** with a fixed monotonic frame clock. It never renders a burst to catch up; it skips ticks it fell behind on and counts them (`skipped_ticks`, `late_frames`).
- Feed live sources through `media_core::FrameMailbox`: capacity 1, newest frame wins, and every replaced frame is counted. Frame storage comes from a bounded `FramePool`, so steady-state capture and rendering do not allocate per frame.
- Compile the pixel crates (`video-compositor`, `media-core`, `capture-linux`) with `opt-level = 3` in the dev profile, so debug builds and tests stay usable.

## Measurements (development machine, 2026-10-19)

The host is an Intel i5-13600KF (20 hardware threads) running Arch Linux; the build is `--release`. The reference scene is:
- 1920×1080 color bars
- a 2560×1440 BGRx source fitted into 1152×648 at 90 % opacity
- a 160×160 half-transparent overlay at a sub-pixel position

`cargo run --release -p video-compositor --example render_bench`:

| Threads | Mean | p95 | p99 | Max |
|---------|------|-----|-----|-----|
| 1 | 6.50 ms | 6.75 ms | 7.70 ms | 9.53 ms |
| 8 | 1.71 ms | 2.05 ms | 2.15 ms | 2.27 ms |

`streamforge compose --duration-ms 5000` (bars + overlay, no screen):
- 300 frames at 59.9994 FPS
- 0 late frames and 0 skipped ticks
- render time 0.84 ms mean, 1.2 ms p99

The 60 FPS budget is 16.67 ms.

## Consequences

- 1080p60 composition has a large CPU margin for M3. GPU work (wgpu) waits until profiling with encoding (M5) or more and larger layers shows a need. It would sit behind the same `Scene`/`Layer` model.
- Capture frames are copied into pooled CPU buffers (memfd → `FramePool`), about 15 MB per 1440p frame. DMA-BUF import would remove that copy, but it needs a GPU path.
- No color management yet: values are treated as sRGB-encoded and blended in that encoding, not in linear light.
- `Animation` is a plain function pointer. Scene changes from commands (M7) will need a channel into the render thread.
