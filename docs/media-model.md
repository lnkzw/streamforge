# Media Model

## Time

Every video frame and audio block must carry a timestamp in an explicit rational time base. Use a monotonic clock for scheduling. Wall-clock time is for human-readable logs only.

## Video frame

The eventual frame type should include:

- presentation timestamp
- width and height
- pixel format / GPU texture format
- color space and range
- CPU buffer or GPU resource handle
- ownership/lifetime rules

Do not force all sources into CPU memory if a safe GPU import path exists. Start with a reliable CPU path where necessary, then optimize based on profiling.

Current CPU implementation (`media_core::VideoFrame`, M3):

- **Geometry:** width, height and stride in bytes. The buffer length is validated on construction.
- **Pixel formats:** `Bgra8` (premultiplied alpha) and `Bgrx8` (opaque; the fourth byte is ignored). Capture converts other packed RGB layouts on ingest and premultiplies straight alpha.
- **Timestamp:** optional PTS as `MediaTimestamp`; capture uses time base 1/1e9.
- **Color:** treated as sRGB-encoded, full range. No color management yet (ADR 0003).
- **Ownership:** a `FrameBuffer` either owns its `Vec<u8>` or comes from a bounded `FramePool`, and returns there when dropped. After warm-up, capture and rendering do not allocate per frame. A frame has exactly one owner at a time, which travels with it through the graph.

## Audio block

The eventual audio block type should include:

- sample timestamp
- sample rate
- channel layout
- sample format
- frame/sample count
- buffer ownership

Audio processing should use predictable block sizes and avoid allocations in the real-time callback path.

Current implementation (M4, `audio-engine`):

- **Mixer format:** 48 kHz, 2 channels, interleaved `f32`, in blocks of 480 frames (10 ms). Every input must deliver this format.
- **Graph edge:** `media_core::AudioRing`, a bounded ring of interleaved samples with a fixed capacity. When it is full, the oldest frames are overwritten and counted as `overrun_frames`. Pushing never allocates.
- **Resampling policy:** capture requests 48 kHz / stereo from the platform. On Linux, PipeWire's audioconvert does rate and channel conversion. If PipeWire negotiates any other format, the stream delivers nothing and warns; buffers are counted as dropped. The mixer itself never resamples.
- **Clock drift policy:** a producer (device/graph clock) and the mixer (monotonic clock) drift apart. Each live input is resampled by an adaptive ratio, so drift causes neither gaps nor trims:
  - The ratio is `1 + KP * (smoothed fill - target)`, clamped to ±2000 ppm.
  - Interpolation is linear and continuous across blocks.
  - The estimated clock offset is reported as `drift_ppm`.

  Target fill is the larger of two blocks and the largest recent producer push (current or previous 1 s window) plus one block, capped below the ring capacity. Explicit events remain only for real stalls and bursts:
  - **Warm-up:** the input stays silent until the target is reached (`warmup_frames`).
  - **Underrun:** when the producer stops, the rest of the block is padded with silence (`underrun_frames`) and the input warms up again.
  - **Trim:** fill far above target (target + 4 blocks + push peak) is trimmed back (`trimmed_frames`).

  Latency per input is therefore bounded, and the report shows its trend per 10 s window.
- **Clipping policy:** inputs are summed in `f32` without intermediate clipping. The master bus is hard-limited to [-1, 1] after the master gain; every limited sample is counted (`clipped_samples`). Non-finite samples become silence and are counted (`invalid_samples`). There is no limiter yet.
- **Gain and mute:** gain is set in dB from −96 to +24. Gain and mute changes ramp linearly over one block to avoid clicks. Meters measure each input after gain and the master after limiting (peak and RMS in dBFS; `null` = digital silence).

## Synchronization

Audio and video use different production rates. Synchronize by timestamps, not by assuming one video frame corresponds to one audio block. Define behavior for late frames, missing audio, capture interruptions, and encoder backpressure.

## Queue policy

Each edge in the media graph must have a bounded capacity and an explicit overload policy. Live video may drop stale frames to protect latency. Audio overrun handling must be deliberate and measurable. Every drop/overrun increments a metric.

Implemented video edges:

| Edge | Capacity / policy | Metrics |
|------|-------------------|---------|
| Capture callback → frame pool | 4 buffers; with no free buffer the frame is discarded | `VideoStats.frames_discarded` |
| Capture → compositor (`FrameMailbox`) | 1 frame, newest wins | `MailboxStats.replaced` (frames never composited) |
| Compositor clock | fixed rate; it skips ticks instead of bursting | `RenderStats.skipped_ticks`, `late_frames`; per source `stale_ticks` and `missing_ticks` |

Implemented audio edges:

| Edge | Capacity / policy | Metrics |
|------|-------------------|---------|
| Capture callback → `AudioRing` | 1 s; when full, the oldest frames are overwritten | `AudioRingStats.overrun_frames` |
| `AudioRing` → mixer | adaptive resampling to a bounded target fill (see above); warm-up, pad and trim only on stalls and bursts | `drift_ppm`, `warmup_frames`, `underrun_frames`, `trimmed_frames`, buffer latency min/mean/max and per-10 s trend |
| Mixer clock | fixed 10 ms blocks; it catches up to 20 blocks back to back, then skips | `late_blocks`, `skipped_blocks` |
