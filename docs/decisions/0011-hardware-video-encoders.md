# ADR 0011: Hardware H.264 encoders with a software fallback (M9c)

Status: Accepted (M9c)

## Context

Every output encoded with libx264. At 1080p60 that is the largest CPU cost of the pipeline, and each additional output (a stream next to a recording) encodes again (ADR 0006). M9 asks for hardware encoding where it helps.

GPUs and media engines encode H.264 with little CPU, but which one exists, and whether its driver or runtime is installed, differs per machine, not just per OS. The linked FFmpeg builds already contain the encoders: system FFmpeg on Linux, BtbN on Windows, Homebrew on macOS. Only opening an encoder shows whether it works here.

## Decision

### Choosing an encoder

- **Preference:** `VideoSettings` has a preference (`EncoderPreference`) and a rate control (`RateControl`). Encoder names and x264 options no longer live in `encoder-api`; `encoder-ffmpeg` maps the preference and the rate to encoders and options.
  - `auto` (default): this OS's hardware candidates, then the software ones: libx264, OpenH264 (builds without the GPL), and finally whatever H.264 encoder the build has (e.g. `h264_mf` in an LGPL Windows build).
  - `software`: the software candidates only.
  - `hardware`: hardware candidates only; the output fails with `unsupported` if none opens.
- **Candidates, most common first:**

  | OS | Hardware candidates |
  |----|---------------------|
  | Linux | `h264_vaapi` (Intel, AMD), `h264_nvenc` |
  | Windows | `h264_nvenc`, `h264_qsv`, `h264_amf` |
  | macOS | `h264_videotoolbox` |

- **Trying them:** the first candidate that works is used.
  - "Works" means it opens and encodes a test frame: some drivers accept the session and reject the first frame. A throwaway instance encodes one frame and is flushed; then the real one is opened.
  - A missing GPU, driver or runtime fails quickly. FFmpeg's log is muted while hardware candidates are tried, because those failures are expected; no lock is held meanwhile, and the level comes back afterwards (also after a panic).
  - Each failure is kept with its reason (`skipped`).
  - The encoder that worked moves to the front of the list: a stream's reconnects try it first and still fall back to the others if it stops working (a GPU reset, a session limit).
- **Reporting:**
  - `start_recording` and `start_streaming` return `video_encoder: {encoder, hardware, skipped}`.
  - The final report's video track carries the same fields (`encoder`, `hardware`, `skipped`), from one `VideoEncoderInfo`, for the last session.
  - The desktop UI logs the encoder, and the CLI takes `--video-encoder`.

### Rate control per encoder

The same intent maps to each encoder's own options. The table in `encoders.rs` is unit-tested on every OS.

- **Quality (recordings):** libx264 CRF 20 (as before), VAAPI CQP 22, NVENC VBR with CQ 21, Quick Sync global quality 22, AMF CQP 20/22. VideoToolbox and OpenH264 have no constant-quality mode here; they use 16 Mbit/s.
- **Constant bit rate (streams):** for every encoder, `b` = `maxrate` = the requested rate, `bufsize` = twice that, and `g` = the keyframe interval (2 s). Then each encoder's CBR switch:
  - libx264: `nal-hrd=cbr`, no scene-cut keyframes
  - VAAPI: `rc_mode=CBR`
  - NVENC: `rc=cbr`, `tune=ll`, forced IDR
  - AMF: `rc=cbr`, low-latency usage
  - OpenH264: `rc_mode=bitrate`
  - VideoToolbox: average bit rate within `maxrate`, not strict CBR. Its `constant_bit_rate` exists on Apple silicon only and would make the encoder fail to open on Intel Macs. RTMP ingests accept this.

### Frames

- Encoders that take software frames get YUV 4:2:0, or NV12 when that is all they take (Quick Sync).
- `h264_vaapi` needs its frames in GPU surfaces. ffmpeg-next 9 has no hardware-context API, so a small module (`ffmpeg/vaapi.rs`, Linux only) calls libavutil directly:
  - it creates a VAAPI device and a pool of NV12 surfaces, and gives the encoder its reference
  - it uploads each converted frame (`av_hwframe_transfer_data`)
  - its invariants (ownership of every buffer reference, frame ownership, single thread) are documented in the module. Apart from `ffmpeg/interrupt.rs`, it is the only `unsafe` code of the outputs.
- The colour tags (BT.601 matrix, limited range, BT.709 primaries, sRGB transfer) are set on every encoder context, as before.
- An encoder that never received a frame is not flushed: FFmpeg 8's `h264_vaapi` crashes when flushed empty.
- `vaapi.rs` has its own tests. Where a device exists, they create surfaces, upload a frame and attach the surfaces to an encoder twice without leaking. Without one (CI), creation must fail with a clean error.

## Verification

- **Development machine (Linux, AMD RX 6800, Mesa VAAPI):**
  - `auto` picks `h264_vaapi`, and every recording, streaming and acceptance test runs on it and passes, including decoded colour bars, colour tags and A/V sync.
  - A 5 s reference recording took 2.0 s of user CPU time with VAAPI against 4.0 s with libx264, both with 300 frames encoded and none dropped.
  - FFmpeg's `h264_amf` fails to open there (no AMF runtime); it is not a Linux candidate.
- **CI runners have no GPU:**
  - The tests cover the fallback (`auto` → libx264 with every hardware candidate's reason), the `hardware` error and libx264 itself.
  - A CI step prints which encoder `auto` picked on each runner. At the first complete run, every runner fell back to libx264, and the reasons were:
    - Linux: no VAAPI device; NVENC "operation not permitted"
    - Windows: NVENC, Quick Sync and AMF fail to open
    - macOS: VideoToolbox fails to open on the VM
- **Untested:** NVENC, Quick Sync, AMF and VideoToolbox on real hardware. Their options follow FFmpeg's documentation; an option an encoder does not know is ignored by FFmpeg, and an encoder that fails to open falls back.

## Dependencies

`encoder-api` now depends on `schemars` (already a workspace dependency of `capture-api` and `command-api`): `EncoderPreference` derives `JsonSchema`, so the `video_encoder` parameter appears in the commands' parameter schemas and MCP tool definitions. No new crate enters the build.

## Consequences

- On machines with a working GPU encoder, recordings and streams cost much less CPU, and a second output no longer doubles the x264 load.
- `auto` makes results machine-dependent: the same command can produce a hardware-encoded file on one machine and an x264 file on another. Reports always say which, and `software` gives reproducible output.
- Hardware encoders differ in quality per bit; the quality settings are comparable, not identical.
- VAAPI is the only path with GPU uploads; NVENC, Quick Sync and AMF receive software frames and upload internally. Zero-copy capture-to-encoder paths (D3D11, IOSurface, DMA-BUF) are future work.
- Each frame is still converted from BGRA on the CPU (swscale) before encoding or upload.
