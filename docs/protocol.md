# Control Protocol (JSON lines, v1)

Local control channel for the headless engine (`streamforge-engine`). Implemented by `crates/control-protocol`; the engine process serves it on stdin/stdout. The format is versioned and intentionally small. It is a wire contract, not a serialization of internal Rust types. Contract tests in `crates/control-protocol` pin it down.

## Transport and framing

- The client starts `streamforge-engine` and talks to it over its stdin/stdout pipes.
- Each message is one UTF-8 JSON object on its own line, terminated by `\n`. The engine also accepts `\r\n`, and a final line without a terminator.
- Responses on stdout are always single-line JSON. The engine writes nothing else to stdout; diagnostics go to stderr.
- Lines containing only whitespace are ignored and get no response.
- A request line may be at most **65 536 bytes** (`MAX_REQUEST_BYTES`), not counting the `\n`. A longer line is discarded without being buffered and answered with `request_too_large`.
- The engine exits with status 0 when stdin closes. An I/O error on the pipes makes it exit with status 1 and print a message to stderr.

## Request

```json
{"v": 1, "id": "req-1", "command": "get_status", "params": {}}
```

| Field     | Required | Type                      | Meaning |
|-----------|----------|---------------------------|---------|
| `id`      | yes      | string, or integer in i64 range | Client-chosen correlation ID, echoed unchanged in the response. |
| `command` | yes      | string                    | Command name from the registry (`list_commands`). |
| `params`  | no       | object or `null`          | Command parameters. Absent, `null` and `{}` mean the same thing. |
| `v`       | no       | integer                   | Protocol version. Defaults to `1`. |

Any other top-level field is rejected with `invalid_request`, so a typo such as `parms` fails loudly. New fields require a new protocol version.

## Response

Success:

```json
{"v":1,"id":"req-1","ok":true,"result":{ ... }}
```

Failure:

```json
{"v":1,"id":"req-1","ok":false,"error":{"code":"unknown_command","message":"unknown command `foo`","details":{ ... }}}
```

- `v` is always the version of the protocol the engine speaks.
- `id` echoes the request ID. It is `null` only when the ID could not be determined: invalid JSON or UTF-8, a non-object request, a missing or malformed `id`, or an oversized line.
- Use `ok` to tell success from failure. Exactly one of `result` and `error` is present.
- `error.code` is stable and meant for matching. `error.message` is human-readable and may change. `error.details` is optional, structured context.

## Ordering

Requests are handled one at a time, and responses come back in request order. Clients may pipeline several requests, but should still correlate responses by `id`, because a later transport may answer out of order.

## Error codes

| Code                  | Layer    | Meaning |
|-----------------------|----------|---------|
| `parse_error`         | protocol | The line is not valid UTF-8 or not valid JSON. |
| `invalid_request`     | protocol | The JSON is valid but the envelope is wrong: not an object, `id` missing or of the wrong type, `command` missing or not a string, an unknown field, or `v` not an integer. |
| `request_too_large`   | protocol | The line exceeds `MAX_REQUEST_BYTES`. `details.max_bytes` gives the limit. |
| `unsupported_version` | protocol | `v` is not supported. `details.requested` and `details.supported` give the versions. |
| `unknown_command`     | command  | No such command. `details.available` lists the valid names. |
| `invalid_params`      | command  | `params` has the wrong shape or contains unexpected fields. `details.unexpected` lists unexpected names. |
| `invalid_state`       | command  | The command is not allowed in the current engine state. Reserved; no current command returns it. |
| `unsupported`         | command  | The platform or backend does not offer the requested capability, e.g. a cursor mode the portal lacks. `details.available` lists alternatives where known. |
| `unavailable`         | command  | A required system service (portal, PipeWire, D-Bus) is missing or not running. The message names a remedy. |
| `cancelled`           | command  | The user dismissed an interactive step, such as the screen selection dialog. |
| `permission_denied`   | command  | The system refused access. |
| `not_found`           | command  | A referenced device does not exist, e.g. an unknown audio `input`. `details.available` lists valid names. |
| `timeout`             | command  | A system service, or the user in a dialog, did not respond in time. |
| `backend_error`       | command  | A media backend failed. The message carries the backend's error. |
| `internal`            | command  | The engine failed while executing the command or encoding its result. |

A bad request never terminates the engine. After any error the engine keeps reading the next line.

## Commands (v1)

| Command                    | Params | Access      | Result |
|----------------------------|--------|-------------|--------|
| `get_status`               | none   | `read_only` | `{"application", "version", "media_engine_ready", "capture_available", "streaming_active", "streaming_state", "recording_active", "recording_failed"}` |
| `list_commands`            | none   | `read_only` | `{"commands": [{"name", "summary", "access"}]}` |
| `get_capture_capabilities` | none   | `read_only` | `{"platform", "desktop", "session_type", "screen": {...}, "audio": {...}}` |
| `list_audio_inputs`        | none   | `read_only` | `{"inputs": [{"name", "description", "media_class"}]}` |
| `probe_screen_capture`     | see below | `sensitive` | Video capture report |
| `probe_audio_capture`      | see below | `sensitive` | Audio capture report |
| `probe_composite`          | see below | `sensitive` | Composite report |
| `probe_audio_mix`          | see below | `sensitive` | Mix report |
| `start_recording`          | see below | `sensitive` | `{"path", "width", "height", "fps", "sample_rate", "channels"}` |
| `stop_recording`           | none   | `control`   | Recording report |
| `start_streaming`          | see below | `sensitive` | `{"target", "width", "height", "fps", "video_bitrate_kbps", "sample_rate", "channels"}` |
| `stop_streaming`           | none   | `control`   | Streaming report |

`access` is the permission class: `read_only`, `control` or `sensitive`. Adapters must gate `control` and `sensitive` commands behind explicit authorization. That enforcement arrives with M8. Both probes are `sensitive` because they capture private content. Screen capture goes through the portal picker, but some pickers can auto-select or remember a choice. Microphone capture has no system consent dialog at all.

`capture_available` in `get_status` is true if screen or audio capture was available at the last capability query. The engine runs that query on the first `get_status` and on every `get_capture_capabilities`.

### Capabilities

`screen` and `audio` each carry `backend`, `available` and `unavailable_reason`. The reason is a string when the capability is unavailable, otherwise `null`. In addition:

- `screen` has `portal_version`, `source_kinds` (subset of `monitor`, `window`, `virtual`) and `cursor_modes` (subset of `hidden`, `embedded`, `metadata`).
- `audio` has `server_version`.

### Capture probes

Probes capture briefly and return diagnostics. No media is stored or returned. A probe **blocks the engine** for its duration plus any user interaction: the screen probe opens the system's source picker and waits up to 120 s. Other requests on the same connection are answered afterwards.

`probe_screen_capture` params (all optional):

| Param          | Type | Default | Meaning |
|----------------|------|---------|---------|
| `duration_ms`  | integer 100..=60000 | 5000 | Capture time after the source is selected. |
| `source_types` | array of `"monitor"`, `"window"`, `"virtual"` | `["monitor","window"]` | Source types offered in the picker; duplicates are ignored. |
| `cursor`       | `"hidden"`, `"embedded"`, `"metadata"` | `"embedded"` | Cursor handling; must be one of `cursor_modes`. |

`probe_audio_capture` params (all optional):

| Param         | Type | Default | Meaning |
|---------------|------|---------|---------|
| `input`       | string | system default | `name` from `list_audio_inputs`. |
| `duration_ms` | integer 100..=60000 | 5000 | Capture time. |

Video report: `{"backend", "source": {"kind", "width", "height"}, "format": {"pixel_format", "width", "height", "framerate": {"num", "den"}}, "buffer_memory", "stats", "warnings"}`. The `stats` fields:

- **Counters:** `elapsed_ms`, `frames`, `empty_buffers`, `corrupted_buffers`.
- **Drop metrics:**
  - `dropped_frames`: gaps in the source sequence number.
  - `buffer_starvation`: process callbacks that found no buffer.
  - `frames_discarded`: frames received but not delivered to a consumer, e.g. because the buffer pool was exhausted. Only non-zero for continuous streams.
- **Rate:** `measured_fps`, `frame_interval` (`{"min_ms", "mean_ms", "max_ms"}`).
- **Timestamps:** `first_pts` and `last_pts`, as `{"ticks", "time_base_num", "time_base_den"}`.
- **Timestamp quality:** `non_monotonic_pts`, `frames_without_pts`.

A frame rate of `0/1` means variable (damage-driven) delivery.

Audio report: `{"backend", "input", "format": {"sample_format", "rate", "channels"}, "stats", "warnings"}`. The `stats` fields:

- **Counters:** `elapsed_ms`, `buffers`, `frames`, `empty_buffers`, `dropped_buffers` (data that could not be decoded, e.g. before a usable format was negotiated).
- **Drop metrics:**
  - `expected_frames` and `frame_deficit`: estimated from arrival times, accurate to about one buffer.
  - `buffer_starvation`.
- **Sample quality:** `invalid_samples`, `clipped_samples`.
- **Levels:** `peak_dbfs` and `rms_dbfs`; `null` means digital silence.
- **Stream positions:** `first_position` and `last_position`, in the backend's time base.

### Composite probe

`probe_composite` runs the compositor on a fixed 1920×1080 @ 60 FPS reference scene for `duration_ms` and reports render timing:
- color bars
- optionally a live screen layer, fitted into 1152×648 at 90 % opacity
- a half-transparent moving overlay

It is `sensitive`: with `screen: true` it captures the screen, and `snapshot_path` writes a file.

| Param           | Type | Default | Meaning |
|-----------------|------|---------|---------|
| `duration_ms`   | integer 100..=60000 | 5000 | Render time. |
| `screen`        | boolean | `false` | Add the live screen layer; opens the system picker. |
| `source_types`, `cursor` | as for `probe_screen_capture` | | Only allowed with `screen: true`. |
| `snapshot_path` | plain file name ending in `.ppm` | none | Write the last frame as binary PPM. The file is created in the engine's output directory: `--output-dir`, default the working directory. Directories, `..` and absolute paths are rejected with `invalid_params`. An existing file is never overwritten; the report records the error instead, and a partially written file is removed. |

Report: `{"render": {...}, "screen_capture": <video report>|null, "screen_capture_error": {"kind", "message", "details"}|null, "snapshot": {"path", "written", "error"}|null, "warnings"}`. The `render` object contains:

- **Setup:** `renderer`, `threads`, `width`, `height`, `target_fps`.
- **Totals:** `elapsed_ms`, `frames_rendered`, `achieved_fps`.
- **Deadline misses:**
  - `skipped_ticks`: ticks not rendered because rendering fell at least one frame behind.
  - `late_frames`: frames that finished after the next tick was due.
- **Timing:** `render_time` (`{"min_ms", "mean_ms", "p95_ms", "p99_ms", "max_ms"}`).
- **Sources:** a `sources` array. Each entry has `name`, `kind` (`static`/`live`), `frames_consumed`, `stale_ticks` and `missing_ticks`. Live sources also carry `mailbox` (`{"published", "replaced", "taken"}`), where `replaced` counts frames that were never composited.

### Audio mix probe

`probe_audio_mix` mixes live inputs and test tones at 48 kHz stereo for `duration_ms` and reports meters, buffering and timing. No audio is stored or returned. It is `sensitive` because it opens microphones.

| Param            | Type | Default | Meaning |
|------------------|------|---------|---------|
| `duration_ms`    | integer 100..=900000 | 5000 | Run time; up to 15 min for latency tests. |
| `inputs`         | array of `{"input": string|null, "gain_db": number, "mute": bool}` | `[]` | Live inputs; `input: null` is the system default. |
| `tones`          | array of `{"frequency_hz": number, "level_dbfs": number, "gain_db": number, "mute": bool}` | one 1 kHz tone at −12 dBFS if no source is given | Test tones; `level_dbfs` defaults to −12. |
| `master_gain_db` | number −96..=24 | 0 | Master gain. |

At most 8 sources in total. Gains must lie in −96..=24 dB, tone frequencies strictly between 0 and 24 000 Hz, and tone levels in −96..=0 dBFS.

Report: `{"mix": {...}, "captures": [{"input", "report", "error"}], "warnings"}`. The `mix` object contains:

- **Setup:** `rate`, `channels`, `block_frames`.
- **Timing:** `elapsed_ms`, `blocks_mixed`, `late_blocks`, `skipped_blocks`, `mix_time`.
- **Master:** `master` meter (`{"peak_dbfs", "rms_dbfs"}`), `clipped_samples`, `invalid_samples`.
- **Inputs:** an `inputs` array. Each entry has:
  - `name`, `kind` (`live`/`tone`), `gain_db`, `muted`
  - `meter`
  - `warmup_frames`, `underrun_frames`, `trimmed_frames`
  - `drift_ppm` (live inputs): the input clock's offset against the mixer clock, as tracked by the adaptive resampler
  - `ring` (`{"capacity_frames", "pushed_frames", "pulled_frames", "overrun_frames", "discarded_frames", "max_push_frames"}`)
  - `buffer` (`{"target_ms", "min_ms", "mean_ms", "max_ms", "windows_ms"}`), where `windows_ms` is the mean fill per 10 s window, oldest first

`warnings` lists non-fatal observations, e.g. "no frames received" on a static screen, or a delivered source kind that differs from `source_types`. When a probe fails, warnings gathered before the failure are in `error.details.warnings`. If the portal session could not be closed afterwards, `error.details.session_close_error` says why.

### Outputs: recording and streaming

Outputs share one **media pipeline**: the 1920×1080@60 reference scene (the `probe_composite` scene, with an optional live screen layer) and the audio mix.

- **Start and stop:** the first output to start brings the pipeline up with its source parameters. The pipeline stops when the last output stops.
- **Joining:** an output started while another runs joins the pipeline. It must not name sources (`invalid_state` otherwise).
- **Defaults:** without source parameters and without a running pipeline, the sources default to one 1 kHz tone at −20 dBFS.
- **Isolation:** each output has its own encoders, queue and thread. A failing stream never stops a recording, and vice versa.
- **Exclusive engine:** while any output is open, the probe commands fail with `invalid_state`. They would compete for devices and CPU and block the engine thread.

Source parameters (all optional; any of them makes the output bring its own sources):

| Param            | Type | Default | Meaning |
|------------------|------|---------|---------|
| `screen`, `source_types`, `cursor` | as in `probe_composite` | `false` | Add a live screen layer (opens the portal picker). |
| `inputs`         | as in `probe_audio_mix` | `[]` | Live audio inputs. |
| `tones`          | as in `probe_audio_mix` | one 1 kHz tone at −20 dBFS if there are no `inputs` | Test tones; `[]` means no tone. |
| `master_gain_db` | number −96..=24 | 0 | Master gain. |

#### Recording

`start_recording` records to H.264 + AAC in Matroska. It returns as soon as the file header is written; recording continues on its own threads until `stop_recording`. Closing the engine's stdin also stops and finalizes it. It is `sensitive` because it captures and stores private content.

| Param            | Type | Default | Meaning |
|------------------|------|---------|---------|
| `file_name`      | string | required | Plain file name ending in `.mkv`, created in the engine's `--output-dir`. Directories are rejected; an existing file is never overwritten (`invalid_params`). A missing output directory is `not_found`, an unwritable one `permission_denied`. |

Errors:
- `invalid_state`: a recording is open (`details.recording` names it), or `stop_recording` was called without one. If the recorder or the pipeline stopped on its own, `get_status` reports `recording_failed: true`; `stop_recording` then finalizes the file and returns the error.
- `unsupported`: no recorder on this platform (only Linux has one so far).
- `backend_error`: FFmpeg could not open the output or an encoder, or failed while encoding or finalizing.
- Capture errors are the same as for the probes.

If `start_recording` fails, the file it created is removed.

`stop_recording` sets one end time for both tracks, detaches the output, drains its queue, flushes the encoders and writes the trailer. Report: `{"recording", "pipeline_stopped", "render", "mix", "screen_capture", "screen_capture_error", "audio_captures", "warnings"}`.

- **`recording`:** `path`, `container`, `duration_ms`, `file_bytes`, plus:
  - `video` (`encoder`, `width`, `height`, `fps`, `frames_encoded`, `frames_dropped`, `frames_after_end`, `first_pts_ms`, `end_ms`)
  - `audio` (`encoder`, `rate`, `channels`, `bitrate`, `samples_encoded`, `blocks_dropped`, `blocks_after_end`, `gap_samples`, `overlap_samples`, `discontinuities`, `padding_samples`, `first_pts_ms`, `end_ms`)

  Both tracks are cut at the stop time; `*_after_end` counts what producers emitted while stopping.
- **`pipeline_stopped`:** whether this output was the last one, so the pipeline stopped with it. Only then are `render` (composite stats), `mix` (as in `probe_audio_mix`), the screen capture fields and `audio_captures` (as `captures` in `probe_audio_mix`) filled in. Otherwise they are `null` or empty.

If finalizing fails, the error's `details` carry the partial `recording` stats, the pipeline fields and `warnings`. The file is kept.

#### Streaming

`start_streaming` publishes over RTMP: H.264 with constant bit rate and a keyframe every 2 s, plus AAC, in FLV. It returns once the first connection is established. If that fails, the error is `unavailable` and nothing is retried. Later network or encoder errors trigger reconnects with exponential backoff. Each new connection uses fresh encoders and starts with a keyframe at timestamp 0. Every blocking network operation is bounded by a 5 s timeout. The command is `sensitive` because it publishes private content and handles a credential.

| Param                | Type | Default | Meaning |
|----------------------|------|---------|---------|
| `url`                | string | required | `rtmp://` or `rtmps://` server URL with host and application, without credentials (`user@` is rejected) and without the key. |
| `stream_key`         | string | none | Appended as the last path segment when connecting. No `/` or whitespace. **Secret:** never echoed in results, errors, logs or `Debug` output; reports show `<stream key>`. |
| `video_bitrate_kbps` | integer 300..=50000 | 6000 | Video bit rate (CBR, buffer of 2 s). |
| `reconnect_attempts` | integer 0..=100 | 5 | Attempts after a lost connection; `0` gives up right away. |
| `reconnect_delay_ms` | integer 100..=60000 | 1000 | Wait before the first attempt; doubles per attempt, up to 30 s. |

Targets are shown redacted: scheme, host and application. Deeper path segments become `***`, a query becomes `?***` and the key becomes `<stream key>`. Once streaming has been used, FFmpeg's own log output is turned off, because RTMP error messages can contain the stream path.

`get_status.streaming_state` is one of `connecting`, `connected`, `reconnecting` or `failed`, or `null` without a stream. `failed` means the reconnect attempts are exhausted, or the pipeline stopped. From then on, media is dropped and counted until `stop_streaming`. `streaming_active` is true while the state is `connecting`, `connected` or `reconnecting`.

Errors:
- `invalid_state`: a stream is already open, `stop_streaming` was called without one, or sources were given while the pipeline runs.
- `unavailable`: the server could not be reached, or the connection or publish handshake failed; `details.target` holds the redacted target.
- `unsupported`: no streaming on this platform (only Linux so far).

`stop_streaming` always returns a report, also for a failed stream: `{"streaming", "pipeline_stopped", "render", ..., "warnings"}` (pipeline fields as for recording). `streaming` contains:

- **Target and state:** `target` (redacted), `container` (`flv`), and `state` at the moment the stop was requested: `connected`, `reconnecting` or `failed`.
- **Connection history:** `connects`, `disconnects`, `reconnect_attempts`, `connected_ms`, and `last_error`, which never contains the key.
- **Tracks:** `video` and `audio` with totals over all connections; their `first_pts_ms` and `end_ms` refer to the last connection.
- **Offline drops:** `frames_dropped_offline` and `blocks_dropped_offline` count media that arrived while not connected. Audio received before a connection's first video frame counts here too. Both numbers are included in the tracks' `frames_dropped` and `blocks_dropped`.

## Example session

```console
$ printf '%s\n' '{"id":1,"command":"get_status"}' '{"id":2,"command":"nope"}' | cargo run -q -p streamforge-engine
{"v":1,"id":1,"ok":true,"result":{"application":"StreamForge","capture_available":true,"media_engine_ready":false,"recording_active":false,"recording_failed":false,"streaming_active":false,"streaming_state":null,"version":"0.1.0"}}
{"v":1,"id":2,"ok":false,"error":{"code":"unknown_command","message":"unknown command `nope`","details":{"available":["get_status","list_commands","get_capture_capabilities","list_audio_inputs","probe_screen_capture","probe_audio_capture"],"command":"nope"}}}
```

Key order inside `result` and `details` is not part of the contract. `capture_available` depends on the machine; this output is from a Linux desktop session.

## Versioning policy

- Adding a command, adding optional fields to a result, or adding an error code is backward compatible within v1. Clients must ignore unknown result fields and treat unknown error codes as generic failures.
- Removing or renaming a field, command or error code, changing a field's type, or accepting new request envelope fields requires v2.
- Update the contract tests in `crates/control-protocol` together with this document.
