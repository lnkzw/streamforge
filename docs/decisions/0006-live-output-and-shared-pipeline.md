# ADR 0006: Live output over RTMP and a pipeline shared by independent outputs

Status: Accepted (M6)

## Context

M6 asks for RTMP output that handles reconnects, network errors, encoder failure and shutdown, and keeps recording and streaming as separate consumers. Acceptance requires streaming to a local test server, and a recording that keeps running when the stream fails.

In M5 a recording owned the compositor and mixer. Two outputs that run at the same time, and stop independently, need a different ownership model.

## Decision

### Pipeline and outputs

- **Shared pipeline:** the engine owns one media pipeline (captures, compositor, mixer) and a fan-out to attached outputs.
- **Lifecycle:** the first output starts the pipeline with its sources; later outputs join it and must not bring sources. The pipeline stops with the last output.
  - This replaces scene commands until M7. The command shape stays compatible with M5: `start_recording` without sources still works.
- **Separate resources per output:** each output has its own copy pool (8 frames), bounded queue, epoch (media time 0 at attach) and encoder thread.
  - A stalled stream (slow network) exhausts only its own pool and queue.
  - The recording, sharing the same producers, keeps every frame it can encode.
  - Two encoders cost CPU. Sharing encoded packets was rejected, because streaming needs CBR and a fixed GOP, while recording uses CRF.
- **Fan-out locking:**
  - The compositor and mixer threads lock their output list once per frame or block.
  - The engine thread takes that lock only to push or remove an output, never while waiting on anything.
  - The lists have reserved capacity, so the producers neither block on I/O nor allocate there.

### Streaming

- **Transport and container:** FFmpeg's native RTMP protocol with the FLV muxer, through the existing `ffmpeg-next` dependency.
- **Encoding:** H.264 via libx264 (`veryfast`, CBR with `nal-hrd=cbr`, a buffer of 2 s, keyframe exactly every 2 s with scene-cut keyframes off) plus AAC at 160 kbit/s.
- **Timeouts and interrupts:**
  - `rw_timeout` (5 s) bounds connect, handshake and every write. A test covers a silent TCP peer.
  - Outputs are opened through a small isolated FFI module (`ffmpeg/interrupt.rs`, the only `unsafe` code of the outputs) that installs an `AVIOInterruptCB`. ffmpeg-next 9 offers none for outputs, and its `output_as_with` leaks the context when opening fails; the module frees it.
  - **Stop:** `Streamer::stop` (and dropping it) gives a healthy connection 2 s (`STOP_GRACE`) to flush and close, then interrupts blocked I/O, so stopping is bounded even for a stalled peer or a reconnect hanging in its handshake (tested).
  - **Lost connection:** the broken session is aborted at once, so closing it does not wait for timeouts again.
  - **Not bounded:** name resolution (`getaddrinfo` inside FFmpeg) is neither covered by the timeout nor interruptible. A hanging resolver can delay a connect.
- **First connection:** `start_streaming` returns once it is established. If it fails, the error is `unavailable`, with no retry, so a wrong URL fails fast.
- **Reconnects:** after a write or encoder error, the output aborts the broken session and retries with exponential backoff.
  - The policy is `reconnect_attempts` (default 5), `reconnect_delay_ms` (default 1 s) and a 30 s maximum delay.
  - The attempt count restarts only after a connection stayed up for 10 s. A server that accepts and drops at once (e.g. an invalid key) therefore still ends in `failed`, instead of cycling forever (tested).
  - Every connection is a fresh FLV session with fresh encoders. It starts with a keyframe, and its timeline starts at its first video frame.
  - Media arriving while disconnected, and audio before a connection's first frame, is dropped and counted (`*_dropped_offline`).
  - Media that was encoded but never delivered because the connection broke is counted as `frames_undelivered` / `samples_undelivered`. That covers frames inside the encoder's lookahead and the failed write.
- **Failure:** after the last attempt the state is `failed`. The output then counts everything it receives as dropped until it is stopped. A streamer thread that ended on its own also reads as `failed`.
- **State and stop:** the state (`connecting`, `connected`, `reconnecting`, `failed`) is exposed through `get_status`. `stop_streaming` always reports, including for a failed stream. If even that fails, the error keeps the pipeline report.

### Stream key safeguards

- **Separate secret type:** the key is a separate parameter, held in `StreamKey`. `Debug` prints `<redacted>`, the type has no `Serialize`, and it is never part of errors or reports.
  - `StreamTarget`'s `Debug` shows only the redacted form: secrets embedded in the URL's path or query stay hidden too.
  - A wrongly typed `stream_key` is rejected without serde quoting its value.
  - The key is joined as the last path segment, before any query.
- **Exposure:** only `StreamTarget::connect_url` combines URL and key, and only for FFmpeg.
- **Redaction:** shown targets keep scheme, host and application and replace the rest.
- **Validation:** URLs with credentials (`user@`) are rejected. Validation messages never contain the key.
- **FFmpeg logs:** turned off for the rest of the process once streaming is used, because RTMP error messages can contain the stream path. The check and the switch happen under one lock, so a concurrent recorder start cannot turn logging back on. A redacting `av_log` callback would keep diagnostics and is future work.
- **CLI:** reads the key from an environment variable, never from argv, which other users and shell histories can see. With `--key-env NAME` the variable must be set; otherwise `STREAMFORGE_STREAM_KEY` is used if present.

## Testing

- **Test server:** `rtmp-test-server` (test-only, never shipped) runs FFmpeg's RTMP protocol in listen mode in-process. It reports codecs, size, the first-keyframe flag and packet time spans.
  - It can drop the connection after a set time to simulate a failing ingest.
  - FFmpeg's listener accepts any stream name, so the way keys are joined is covered by unit tests instead.
- **Encoder tests:**
  - streaming to the server
  - reconnecting to a second server on the same port, where the stream restarts with a keyframe at 0
  - giving up after the attempts, with a prompt stop and counted drops
  - a server that keeps accepting and dropping, which ends in `failed`
  - a bounded stop while a reconnect hangs on a silent peer (fails without the interrupt: 9.5 s instead of < 3 s)
  - an unreachable server, which yields a `Network` error without the key
  - a silent peer, which hits the timeout
- **Engine and process tests:**
  - streaming via the engine
  - "recording continues when the stream fails": recording and stream share the pipeline, the ingest fails for good, and the recording stays active. Its file covers the whole run with no audio drops, and every video tick is accounted for.

## Measurements (development machine, 2026-10-20, release)

`STREAMFORGE_STREAM_KEY=… streamforge stream --url rtmp://127.0.0.1:19352/live --duration-ms 5000 --record both.mkv` against `ffmpeg -listen 1` as the server:

- **Stream:** 300 frames at 1080p60 with 6 Mbit/s CBR, 0 dropped, 1 connection.
- **Recording at the same time:** 317 frames, 0 dropped, 0 audio gaps.
- **Compositor:** 0 late frames.
- **Server output:** 5.03 s of h264 1920×1080 + aac.
- **Key:** absent from stdout and stderr.

## Consequences

- Recording and streaming run together and fail independently. Each additional output costs one extra x264 encode at canvas resolution.
- Each output also gets its own copy of every frame (8 MB at 1080p, made on the compositor thread) and its own colour conversion. Sharing one copy would let a stalled stream starve the recording's buffers, so isolation wins over memory bandwidth for now; the measurement above showed 0 late frames with two outputs.
- Stream resolution equals the canvas (1080p60). Scaling for streams, hardware encoders and multiple simultaneous streams are future work.
- `rtmps://` depends on the TLS support of the linked FFmpeg build.
- Before the stream's first video frame, and between connections, audio is dropped (counted). A reconnect therefore loses up to the reconnect delay plus connect time of media for the viewer.
- Disabling FFmpeg's log output after streaming starts also hides FFmpeg diagnostics of the recorder. Our errors still carry FFmpeg's error text.
- Linux only, like recording (M9). Since ADR 0009, Windows and macOS too.
