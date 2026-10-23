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
- **Routing graph (ADR 0017):** each input is a channel (processing slots, mute, fader, balance) with sends to buses, tapped before or after the fader; mute comes before both taps. Aux buses (slots, mute, fader) can be assigned to the program bus, nothing else, so the graph is acyclic. Its shape is fixed for a run (at most 8 channels and 8 aux buses); parameters change live. The engine exposes channels, aux buses, sends and the program bus (`get_mixer`; M11 steps 2a and 2b); a new channel sends post-fader at 0 dB to the program bus only. `get_mixer` warns about a channel that reaches the program bus more than once (directly and through each assigned aux bus). Each output takes a bus as its mix (its profile's, step 3a), so a separate mix can feed the stream while the recording takes the program bus; a new mix fades in over one block.
- **Clipping policy:** channels and buses are summed in `f32` without intermediate clipping. A bus is hard-limited to [-1, 1] where it leaves the graph for an output; every limited sample is counted (`clipped_samples`). Non-finite input samples become silence at the channel input, before any bus, and are counted (`invalid_samples`); so are the samples of an input that is not exactly one block long. A non-finite sample that still arises inside the graph (an overflow from extreme gains, or a faulty processor later) is silenced where the bus leaves the graph and counted as invalid too, so an output only receives valid samples. A non-finite gain or send level is silence, and finite ones are held within −96 to +24 dB. There is no limiter yet.
- **Gain, mute and balance:** gain is set in dB from −96 to +24. Balance runs from −1 (left) to 1 (right): unity at the centre, the other side attenuated along a cosine to silence. Gain, mute, balance, send and assign changes ramp linearly over one block to avoid clicks; a send's tap change fades out on the old tap, then in on the new one. Meters measure each channel after fader and balance, and each bus after its fader, before the output limit, so a bus meter shows an overload the limit removes (peak and RMS in dBFS; `null` = digital silence).

## Synchronization

Audio and video use different production rates. Synchronize by timestamps, not by assuming one video frame corresponds to one audio block. Define behavior for late frames, missing audio, capture interruptions, and encoder backpressure.

Recording (M5, ADR 0005) puts both producers on one timeline:

- **Shared epoch:** the compositor reports each frame's nominal tick time and the mixer each block's nominal time on the monotonic clock. The recorder taps subtract one session epoch.
- **Video:** microseconds, rounded to the frame tick in the encoder time base 1/fps. Skipped ticks leave gaps; frames whose timestamp does not increase are dropped and counted.
- **Audio:** sample frames at 48 kHz (`round(t · 48000)`). The recorder regroups them into 1024-sample AAC frames:
  - gaps up to 1 s are filled with silence (`gap_samples`)
  - overlapping samples are dropped (`overlap_samples`)
  - larger jumps restart the timeline and are counted as `discontinuities`
- **Late frames and missing audio:** both keep their nominal timestamps, so a late frame does not shift later ones. A missing live input reads as silence in the mix (`underrun_frames`), so the audio track stays continuous.
- **Explicit time bases:** the taps hand the recorder `MediaTimestamp`s in nanoseconds. The recorder rescales them into 1/fps and 1/48000 with checked, rounded integer arithmetic (`MediaTimestamp::rescale`). A timestamp that cannot be converted drops its item and counts it.
- **Common end:** on stop, the engine sets one end time for the output. It then waits (at most 500 ms) until both producers have handed over everything stamped before it, because a compositor running late still owes frames. Only then does it detach the output.
  - Items stamped at or after the end are not recorded and are counted (`frames_after_end`, `blocks_after_end`).
  - Items still owed after the wait are counted as dropped.
  - Both tracks end within one frame or block of each other, and nothing before the end is lost silently.
- **Per-output epochs:** every output's media time 0 is the moment it was attached; frames and blocks from before that are not part of it. A stream additionally restarts its timeline at 0 with each connection, starting at the connection's first video frame (a keyframe).
- **Capture latency:** not compensated yet. Live audio is stamped when mixed, about one jitter-buffer target after capture; screen frames are stamped when composited.

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

Implemented output edges (one set per attached output, so outputs never starve each other):

| Edge | Capacity / policy | Metrics |
|------|-------------------|---------|
| Compositor → output's video copy pool | 8 frame buffers per output; with no free buffer the frame is dropped for that output | `video.frames_dropped` of that output |
| Fan-out → output queue | 256 items shared by video and audio (about 1 s), preallocated; when full the item is dropped | `video.frames_dropped`, `audio.blocks_dropped` |
| Audio timeline → AAC frames | gaps ≤ 1 s padded with silence, overlaps trimmed, larger jumps restart (the last frame before a restart and at the end is padded) | `gap_samples`, `overlap_samples`, `discontinuities`, `padding_samples` |
| Recorder after a failure | finalizes the file, then counts everything still queued or sent as dropped until stopped | `frames_dropped`, `blocks_dropped`; `get_status.recording_failed` |
| Streamer while disconnected (backoff, connect) or failed | everything received is dropped; audio before a connection's first video frame too | `frames_dropped_offline`, `blocks_dropped_offline` (included in the track totals); `get_status.streaming_state` |

Implemented control and preview edges (M7):

| Edge | Capacity / policy | Metrics |
|------|-------------------|---------|
| Engine → compositor scene update | 1 slot, newest wins; the render thread try-locks it before a frame | `RenderStats.scene_updates`; removed sources listed in `sources` |
| Engine → mixer gain/mute changes | 1 slot per strip plus master, newest wins; the mixer try-locks it once per block | — (changes are state, not media) |
| Mixer → levels | peak of the last 30 blocks (300 ms) per strip and master, written by the mixer thread into atomics; reads do not reset | `get_mixer` |
| Compositor → preview | every second frame, downscaled 4×, 3 pooled buffers, newest-wins mailbox; best effort (a viewer that falls behind sees fewer frames) | `frames_published`, `frames_dropped` (no free buffer) when the preview is turned off; replaced frames in the mailbox stats |
| Scene updates → retired stats | removed sources and ended captures are reported, bounded to the latest 64 each | `RenderStats.retired_sources`, `scene_captures_omitted` |
