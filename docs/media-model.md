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
