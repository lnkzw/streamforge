# ADR 0005: Recording with FFmpeg libraries via ffmpeg-next

Status: Accepted (M5)

## Context

M5 asks for H.264/AAC recording with FFmpeg libraries, a local container, files that are finalized on stop and on recoverable errors, and an automated check of duration, stream metadata and A/V drift. Three integration options were considered:

1. **Link the FFmpeg libraries through `ffmpeg-next`.** This matches the roadmap. Timestamps, errors and hardware encoders are under our control. The cost: FFmpeg development packages are needed at build time, and binaries inherit the license of the FFmpeg build.
2. **Run the `ffmpeg` CLI as a child process and feed it through pipes.** The build is trivial and the licenses stay separated. But we would have less control over timestamps and errors, and two pipes per platform.
3. **A pure Rust/BSD stack** (openh264 + AAC/Opus + a Rust muxer). The license is permissive, but there are no hardware encoders, quality and performance are lower, and there is more work of our own.

The project owner chose option 1.

## Decision

- **Crates:**
  - `encoder-api` holds settings, statistics and errors only, with no FFmpeg dependency, so `command-api` and adapters do not link FFmpeg.
  - `encoder-ffmpeg` implements `Recorder` on `ffmpeg-next` 9 (features `codec`, `format`, `software-scaling`).
- **Platforms:** FFmpeg is linked only on Linux for now; CI installs `libavcodec-dev`, `libavformat-dev`, `libavutil-dev` and `libswscale-dev`. On Windows and macOS, `Recorder::start` returns `Unsupported` (M9).
- **Formats:**
  - Video: libx264 (`preset=veryfast`, `crf=20`), falling back to any H.264 encoder. Input is BGRA, converted to YUV 4:2:0 with swscale.
  - Audio: FFmpeg's AAC at 160 kbit/s from interleaved `f32`, regrouped into 1024-sample frames. Gaps are filled with silence and counted; overlaps are dropped and counted.
- **Container:** Matroska (`.mkv`). A file cut short by a crash stays playable up to the last cluster; MP4 would be unreadable without its trailer. On stop and on encoder errors the encoders are flushed and the trailer is written.
- **Threading and back-pressure:** one `recorder` thread. Compositor and mixer feed it through output sinks, and nothing they do blocks:
  - The video tap copies each frame into one of 8 pooled buffers.
  - Audio blocks travel inline in a preallocated bounded channel of 256 items.
  - When the queue or pool is full, the item is dropped and counted (`frames_dropped`, `blocks_dropped`).
- **A/V sync:** both producers report each frame's or block's *nominal* time on the monotonic clock. The taps convert it to media time relative to one session epoch: microseconds for video, sample frames for audio. Video timestamps are exact frame ticks (CFR with gaps if ticks are skipped).
- **Commands:**
  - `start_recording` (`sensitive`) returns immediately; recording continues on its threads.
  - `stop_recording` (`control`) finalizes the file and returns the report.
  - Dropping the engine also finalizes the file.
  - `file_name` is a plain `.mkv` name in the engine's `--output-dir`. It is reserved with `create_new`, so an existing file is never overwritten.
  - Starting while recording, or stopping without a recording, fails with `invalid_state`.

## Licensing

The StreamForge sources remain MIT OR Apache-2.0. `ffmpeg-next` is WTFPL. FFmpeg itself is LGPL-2.1+, or GPL when it is built with `--enable-gpl`, which libx264 requires. A StreamForge binary linked against a GPL FFmpeg build (as on the development machine, `ffmpeg-obs` with GPLv3, and in typical distribution packages) is therefore distributed under the GPL. Anyone distributing binaries must comply with the license of the FFmpeg build they link. An LGPL-only build without libx264 also works: the recorder then falls back to another H.264 encoder if one is available.

## Measurements (development machine, 2026-10-20, release)

`streamforge record --duration-ms 3000` on the reference scene with the default tone:

- **Video:** libx264 1920×1080 at 60 FPS, 181 frames, 0 dropped.
- **Audio:** AAC 48 kHz stereo, 144 480 samples, 0 gaps.
- **ffprobe:** confirms h264 1920×1080 at 60/1 and aac 48 kHz with 2 channels; the container duration is 3.054 s.
- **A/V sync:** the ends differ by 3 ms. Audio starts 3.7 ms after video, which is the actual start offset of the two producer threads, recorded in the timestamps.

## Consequences

- Recording works end to end on Linux. The automated acceptance test runs the real engine process and checks the file:
  - streams and duration
  - end drift below 50 ms
  - decoded color bars
  - tone RMS
- Live-input drift resampling is linear (ADR 0004); recordings with live inputs carry its high-frequency modulation until a better resampler lands.
- Encoding uses the CPU (x264). VAAPI/NVENC/AMF are available in the linked FFmpeg and can be offered as encoder options later.
- Without a signal handler, Ctrl-C on the CLI ends the process without finalizing the file. Matroska keeps the file playable, but its index is missing. Use `--duration-ms`, or `stop_recording` over the protocol.
