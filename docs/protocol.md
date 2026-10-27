# Control Protocol (JSON lines, v1)

Local control channel for the headless engine (`streamforge-engine`). Implemented by `crates/control-protocol`; the engine process serves it on stdin/stdout. The format is versioned and intentionally small. It is a wire contract, not a serialization of internal Rust types. Contract tests in `crates/control-protocol` pin it down.

## Transport and framing

- The client starts `streamforge-engine` and talks to it over its stdin/stdout pipes.
- Each message is one UTF-8 JSON object on its own line, terminated by `\n`. The engine also accepts `\r\n`, and a final line without a terminator.
- Responses on stdout are always single-line JSON. The engine writes nothing else to stdout; diagnostics go to stderr.
- Lines containing only whitespace are ignored and get no response.
- A request line may be at most **65 536 bytes** (`MAX_REQUEST_BYTES`), not counting the `\n`. A longer line is discarded without being buffered and answered with `request_too_large`.
- The engine exits with status 0 when stdin closes, and also at SIGINT (Ctrl+C) or SIGTERM (Ctrl+C or Ctrl+Break on Windows): it finishes the request in progress, may answer requests already sent, and leaves the rest unanswered. Either way, open recordings and streams are stopped and finalized first. It stops at its next read of a request: while a client leaves the responses unread and the pipe is full, the engine waits to write an answer and finalizes only once that is written. A second signal exits at once with status 130, leaving them unfinished. SIGHUP keeps its default. An I/O error on the pipes makes it exit with status 1 and print a message to stderr.

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

Any other top-level field is rejected with `invalid_request`, so a typo such as `parms` fails loudly. New fields require a new protocol version; `v` is checked first, so a request of another version gets `unsupported_version` even when it has fields this version does not know.

A key may occur only once in an object. A repeated key is `invalid_request` at the top level (`details.duplicate` names it; a repeated `id` is answered with `id: null`) and `invalid_params` inside `params` (`details.duplicate` is its path there, e.g. `tones[0].gain_db`), after an unknown command, which is still `unknown_command`. This is the JSON-lines transport's rule: the MCP server gets its tool arguments already parsed by its MCP library, which keeps the last of repeated keys.

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
- Errors inside a result (a capture's `error`, `screen_capture_error`, a snapshot's `error`) have the same shape, `{"code", "message", "details"}`, with the codes below; `details` is `null` without context.

## Ordering

Requests are handled one at a time, and responses come back in request order. Clients may pipeline several requests, but should still correlate responses by `id`, because a later transport may answer out of order.

## Error codes

| Code                  | Layer    | Meaning |
|-----------------------|----------|---------|
| `parse_error`         | protocol | The line is not valid UTF-8 or not valid JSON. |
| `invalid_request`     | protocol | The JSON is valid but the envelope is wrong: not an object, `id` missing or of the wrong type, `command` missing or not a string, an unknown or repeated field, or `v` not an integer. |
| `request_too_large`   | protocol | The line exceeds `MAX_REQUEST_BYTES`. `details.max_bytes` gives the limit. |
| `unsupported_version` | protocol | `v` is not supported. `details.requested` and `details.supported` give the versions. |
| `unknown_command`     | command  | No such command. `details.available` lists the valid names. |
| `invalid_params`      | command  | `params` has the wrong shape, a wrong value, or unexpected or repeated fields. `details.command` names the command, `details.unexpected` lists unknown names among its parameters (an unknown field inside one, e.g. in `input`, is named in the message), `details.duplicate` a repeated key. |
| `invalid_state`       | command  | The command is not allowed in the current engine state, e.g. a structure change while the pipeline runs, or a second recording. |
| `unsupported`         | command  | The platform or backend does not offer the requested capability, e.g. a cursor mode the portal lacks. `details.available` lists alternatives where known. |
| `unavailable`         | command  | A required system service (portal, PipeWire, D-Bus) is missing or not running. The message names a remedy. |
| `cancelled`           | command  | The user dismissed an interactive step, such as the screen selection dialog. |
| `permission_denied`   | command  | The system refused access. |
| `not_found`           | command  | A referenced device, scene, source, channel, bus or file does not exist, e.g. an unknown audio `input`. `details.available` lists valid names where known. |
| `timeout`             | command  | A system service, or the user in a dialog, did not respond in time. |
| `backend_error`       | command  | A media backend failed. The message carries the backend's error. |
| `internal`            | command  | The engine failed while executing the command or encoding its result. |

A bad request never terminates the engine. After any error the engine keeps reading the next line.

## Commands (v1)

| Command                    | Params | Access      | Result |
|----------------------------|--------|-------------|--------|
| `get_status`               | none   | `read_only` | `{"application", "version", "pipeline_running", "capture_available", "recording_state", "streaming_state", "preview_active"}` |
| `get_health`               | none   | `read_only` | Live counters of the pipeline and outputs (see below) |
| `list_commands`            | none   | `read_only` | `{"commands": [{"name", "summary", "access", "destructive", "open_world"}]}` |
| `get_capture_capabilities` | none   | `read_only` | `{"platform", "desktop", "session_type", "screen": {...}, "audio": {...}}` |
| `list_audio_inputs`        | none   | `read_only` | `{"inputs": [{"name", "description", "media_class", "loopback_of"}]}` |
| `list_audio_outputs`       | none   | `read_only` | `{"outputs": [{"name", "description", "media_class"}]}` |
| `list_capture_targets`      | none   | `sensitive` | `{"targets": [{"id", "kind", "name", "width", "height"}]}` |
| `probe_screen_capture`     | see below | `sensitive` | Video capture report |
| `probe_audio_capture`      | see below | `sensitive` | Audio capture report |
| `probe_composite`          | see below | `sensitive` | Composite report |
| `probe_audio_mix`          | see below | `sensitive` | Mix report |
| `start_recording`          | see below | `sensitive` | `{"path", "width", "height", "fps", "sample_rate_hz", "channels", "video_encoder", "warnings"}` |
| `stop_recording`           | none   | `control`, destructive | Recording report |
| `start_streaming`          | see below | `sensitive` | `{"target", "width", "height", "fps", "video_bitrate_kbps", "sample_rate_hz", "channels", "video_encoder", "warnings"}` |
| `stop_streaming`           | none   | `control`, destructive | Streaming report |
| `list_scenes`              | none   | `read_only` | Scene list |
| `create_scene`             | see below | `control` | Scene list |
| `delete_scene`             | see below | `sensitive`, destructive | Scene list |
| `set_active_scene`         | see below | `sensitive`, destructive | Scene list |
| `add_source`               | see below | `sensitive` | Scene list |
| `update_source`            | see below | `control`, destructive | Scene list |
| `remove_source`            | see below | `control`, destructive | Scene list |
| `restart_source`           | see below | `sensitive`, destructive | Scene list |
| `set_capture_target`        | see below | `sensitive`, destructive | Scene list |
| `get_mixer`                | none   | `read_only` | Mixer state |
| `add_channel`              | see below | `sensitive` | Mixer state |
| `remove_channel`           | see below | `control`, destructive | Mixer state |
| `update_channel`           | see below | `control`, destructive | Mixer state |
| `set_channel_input`        | see below | `sensitive`, destructive | Mixer state |
| `add_bus`                  | see below | `control` | Mixer state |
| `remove_bus`               | see below | `control`, destructive | Mixer state |
| `update_bus`               | see below | `control`, destructive | Mixer state |
| `set_send`                 | see below | `control`, destructive | Mixer state |
| `get_output_profiles`      | none   | `read_only` | `{"recording": {"mix", "tracks", "video_encoder"}, "stream": {"mix", "video_bitrate_kbps", "video_encoder"}, "monitor": {"mix", "enabled", "device"}}` |
| `set_output_profile`       | see below | `control`, destructive | The profiles plus `{"applied_now", "next_start", "warnings"}` |
| `set_monitor_device`       | see below | `sensitive`, destructive | The profiles |
| `set_preview`              | see below | `sensitive`, destructive | `{"enabled", "width", "height", "fps", "frames_published", "frames_dropped", "pipeline", "warnings"}` |
| `list_projects`            | none   | `read_only` | `{"files": [{"file_name", "bytes", "modified_unix_ms"}], "omitted"}` |
| `save_project`             | see below | `sensitive`, destructive | `{"file_name", "path", "replaced", "scenes", "bytes"}` |
| `load_project`             | see below | `sensitive`, destructive | Scene list plus `{"file_name", "mixer", "warnings"}` |
| `validate_project`         | see below | `read_only` | `{"file_name", "valid", "schema_version", "migrates", "problems": [{"path", "message"}]}` |

Units are in the names (`_ms`, `_hz`, `_bps`, `_kbps`, `_db`, `_dbfs`). A nominal frame rate (`fps`, `target_fps`, a capture's `framerate`) is a fraction `{"num", "den"}`, e.g. `{"num": 60, "den": 1}`; measured rates (`achieved_fps`, `measured_fps`) are numbers.

`access` is the permission class: `read_only`, `control` or `sensitive`; levels are ordered in that sequence. "destructive" marks a command that may overwrite, remove or end something (a scene, a gain, a running output), as opposed to only adding or inspecting; it is `list_commands`' `destructive` and MCP's destructive hint. Adapters gate commands with an `AccessPolicy` chosen by whoever starts them (ADR 0008):
- **Default level:** `streamforge-engine --allow read-only|control|sensitive` defaults to `sensitive`, since its client is whoever controls stdin. The MCP server (`streamforge-mcp`) defaults to `read-only`.
- **Denied commands:** a command above the allowed level fails with `permission_denied` and `details: {"command", "required", "allowed"}`. Parameters are decoded first, so `required` is what that call needs.
- **`add_source`:** needs only `control` for color bars and solid sources; screen sources need `sensitive`.

Both probes are `sensitive` because they capture private content. Screen capture through a picker asks the user, but some pickers can auto-select or remember a choice; a named source (`capture_target_id`) is captured without asking. Microphone capture has no system consent dialog at all. `list_capture_targets` is `sensitive` too: window titles can reveal private content (an open mail, a document name).

`capture_available` in `get_status` is true if screen or audio capture was available at the last capability query. The engine runs that query on the first `get_status` and on every `get_capture_capabilities`. `pipeline_running` is true while a media pipeline runs for outputs or the preview. Each output has one state, `null` while it is not open:
- `recording_state`: `running`, or `failed` when the recording stopped unexpectedly (encoder, compositor or mixer failure); `stop_recording` then finalizes the file and reports the error.
- `streaming_state`: see Streaming.

### File names

Commands that create or read a file (`start_recording`, `probe_composite`'s snapshot, the project commands) take a plain file name, never a path, and the engine places it in its own directory. The rule is the same on every system, so a name means the same file everywhere:
- It ends in the command's extension (`.mkv`, `.ppm`, `.json`, any case) with at least one character before it.
- It contains no `/`, `\`, `:`, `<`, `>`, `"`, `|`, `?`, `*` and no control characters.
- Its part before the first dot, without trailing spaces, is not a Windows device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, any case): `con.backup.json` is refused too.

### Health

`get_health` returns live counters while things run (ADR 0012): what the final reports count, before the stop. It is cheap and never waits for a media thread, so clients can poll it, e.g. once a second. Counters are totals since the pipeline or the output started; rates come from two readings.

```json
{"pipeline_running": true,
 "render": {"elapsed_ms", "frames_rendered", "skipped_ticks", "late_frames", "render_time": {"min_ms", "mean_ms", "p95_ms", "p99_ms", "max_ms"},
            "sources": [{"id", "name", "kind", "frames_consumed", "stale_ticks", "missing_ticks", "last_frame_age_ms"}]},
 "mix": {"elapsed_ms", "blocks_mixed", "late_blocks", "skipped_blocks", "clipped_samples", "invalid_samples", "mix_time",
         "inputs": [{"name", "kind", "warmup_frames", "underrun_frames", "trimmed_frames", "fill_ms", "drift_ppm", "overrun_frames"}],
         "aux_buses": [{"name", "clipped_samples", "invalid_samples"}]},
 "recording": {"target", "elapsed_ms", "running", "state": "running", "video_encoder": {"encoder", "hardware", "skipped"},
               "frames_encoded", "frames_dropped", "samples_encoded", "blocks_dropped", "queue_depth"},
 "streaming": {... the same fields, "state" is the stream state, and "connects", "reconnect_attempts"},
 "monitor": {"device", "enabled", "state": "running", "latency_ms",
             "feed": {"fill_frames", "pull_frames", "target_frames", "drift_ppm", "padded_frames", "trimmed_frames",
                      "ring": {"capacity_frames", "written_frames", "read_frames", "warmup_frames", "underrun_frames", "overrun_frames", "max_read_frames"}}},
 "captures": [{"kind": "screen", "id": "src_k3j9x2m4q8w1", "name": "desk", "state": "running"},
              {"kind": "audio", "id": "ch_4k2m9x0q1w7e", "name": "Mic", "state": "ended", "reason": "..."}]}
```

- **Without a pipeline:** `render` and `mix` are `null`, `captures` is empty, and `recording` and `streaming` are `null` while no output is open.
- **Freshness:** `render` and `mix` are refreshed every 250 ms; a refresh that finds a reader copying the snapshot is retried on the next frame or block. Output counters are refreshed after every item.
- **Ended captures:** `captures` lists every running capture (ADR 0013): the scenes' screen sources (with their `id`, as in `list_scenes` and `render.sources`) and the channels that capture a device (with the channel's `id` and name, as in `get_mixer` and `mix.inputs`). `state` is `ended`, with the system's `reason`, once the capture delivers nothing more: a closed window, an ended portal session, a removed device, a revoked permission. Outputs keep running, and the source shows its last frame.
- **Stalled sources:** `sources[].last_frame_age_ms` is the time since a live source last delivered a new frame, or since it was added if it never did (`null` for static sources). Screen captures may deliver only when the content changes, so a long age alone is not an error; an ended capture's age keeps growing too. `frames_consumed: 0` means it never delivered.
- **Silent inputs:** a live audio input whose `underrun_frames` grow while `fill_ms` stays near 0 has stopped delivering.
- **Drops:** `frames_dropped` and `blocks_dropped` cover every cause the stop report counts: producer side (copy pool or queue full), encoder side, and, for streams, media while offline.
- **Backpressure:** `queue_depth` is the number of items waiting for the output thread. A growing queue means the encoder or the network cannot keep up.
- **Running:** `running` is false once the output or the pipeline it needs has failed; the stop report says why. `state` is the output's state as in `get_status` (`running` or `failed` for the recording).
- **Monitor:** `monitor` is `null` until the monitor is switched on in a run (see Monitor). Its `state` is `ended` with a `reason` when the device did not open (`could not start: …`, then `feed` and `latency_ms` are `null`) or ended during the run. `feed.ring.underrun_frames` grows when the device ran short, and `overrun_frames` and `trimmed_frames` when the mixer was ahead; `latency_ms` is the buffered audio plus the device's largest recent pull (`pull_frames`, the current or previous second).
- **Secrets:** `target` is the file path or the redacted stream target; it never holds the key.

### Capabilities

`screen` and `audio` each carry `backend`, `available` and `unavailable_reason`. The reason is a string when the capability is unavailable, otherwise `null`. In addition:

- `screen` has `portal_version`, `target_kinds` (subset of `monitor`, `window`, `virtual`) and `cursor_modes` (subset of `hidden`, `embedded`, `metadata`). An empty `cursor_modes` means the backend cannot select a cursor mode; the request is then ignored, with a warning.
- `screen` also says how a source is chosen:
  - `picker`: without `capture_target_id`, the user picks in a system dialog (Linux: the ScreenCast portal).
  - `target_list`: `list_capture_targets` lists monitors and windows, and screen commands capture one by its `capture_target_id` without a dialog (Windows).
- `audio` has `server_version`.

### Capture probes

Probes capture briefly and return diagnostics. No media is stored or returned. A probe **blocks the engine** for its duration plus any user interaction: without `capture_target_id`, the screen probe opens the system's source picker and waits up to 120 s. Other requests on the same connection are answered afterwards.

`probe_screen_capture` params (all optional):

| Param          | Type | Default | Meaning |
|----------------|------|---------|---------|
| `duration_ms`  | integer 100..=60000 | 5000 | Capture time after the source is selected. |
| `capture_target_id`    | string (1 to 256 characters) | none | An `id` from `list_capture_targets`: capture that source without a picker. Needs `target_list`; without it, the backend needs `picker`, else the call fails with `invalid_params`. Unknown or closed sources fail with `not_found`. |
| `target_kinds` | array of `"monitor"`, `"window"`, `"virtual"` | `["monitor","window"]` | Target kinds offered in the picker; duplicates are ignored. Not checked with `capture_target_id` (the source has its own kind). |
| `cursor_mode`       | `"hidden"`, `"embedded"`, `"metadata"` | `"embedded"` | Cursor handling; must be one of `cursor_modes`. |

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

Audio report: `{"backend", "input", "format": {"sample_format", "sample_rate_hz", "channels"}, "stats", "warnings"}`. The `stats` fields:

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

It is `sensitive`: with `screen: true` it captures the screen, and `snapshot_file_name` writes a file.

| Param           | Type | Default | Meaning |
|-----------------|------|---------|---------|
| `duration_ms`   | integer 100..=60000 | 5000 | Render time. |
| `screen`        | boolean | `false` | Add the live screen layer: the `capture_target_id` target, else the one picked in the system dialog. |
| `capture_target_id`, `target_kinds`, `cursor_mode` | as for `probe_screen_capture` | | Only allowed with `screen: true`. |
| `snapshot_file_name` | plain file name ending in `.ppm` | none | Write the last frame as binary PPM. The file is created in the engine's output directory: `--output-dir`, default the working directory. It must be a plain file name (see File names), else `invalid_params`. An existing file is never overwritten; the report records the error instead, and a partially written file is removed. |

Report: `{"render": {...}, "screen_capture": <video report>|null, "screen_capture_error": {"code", "message", "details"}|null, "snapshot": {"path", "written", "error": {"code", "message", "details"}|null}|null, "warnings"}`. A snapshot that was not written has an `error`: `invalid_params` for an existing file, `not_found` or `permission_denied` for the output directory, `backend_error` for a failed write. The `render` object contains:

- **Setup:** `renderer`, `threads`, `width`, `height`, `target_fps` (`{"num", "den"}`).
- **Totals:** `elapsed_ms`, `frames_rendered`, `achieved_fps`; `scene_updates` (scene changes applied while running) and `retired_sources` (sources removed by them).
- **Deadline misses:**
  - `skipped_ticks`: ticks not rendered because rendering fell at least one frame behind.
  - `late_frames`: frames that finished after the next tick was due.
- **Timing:** `render_time` (`{"min_ms", "mean_ms", "p95_ms", "p99_ms", "max_ms"}`).
- **Sources:** a `sources` array. Each entry has `id` (in this reference scene `bars`, `screen` or `overlay`; in pipeline reports the scene source's id), `name`, `kind` (`static`/`live`), `frames_consumed`, `stale_ticks` and `missing_ticks`. Live sources also carry `mailbox` (`{"published", "replaced", "taken"}`), where `replaced` counts frames that were never composited.

### Audio mix probe

`probe_audio_mix` mixes live inputs and test tones at 48 kHz stereo for `duration_ms` and reports meters, buffering and timing. No audio is stored or returned. It is `sensitive` because it opens microphones.

| Param            | Type | Default | Meaning |
|------------------|------|---------|---------|
| `duration_ms`    | integer 100..=900000 | 5000 | Run time; up to 15 min for latency tests. |
| `inputs`         | array of `{"input": string|null, "gain_db": number, "mute": bool}` | `[]` | Live inputs; `input: null` is the system default. |
| `tones`          | array of `{"frequency_hz": number, "level_dbfs": number, "gain_db": number, "mute": bool}` | one 1 kHz tone at −12 dBFS if no source is given | Test tones; `level_dbfs` defaults to −12. |
| `program_gain_db` | number −96..=24 | 0 | Program bus gain. |

At most 8 sources in total. Gains must lie in −96..=24 dB, tone frequencies strictly between 0 and 24 000 Hz, and tone levels in −96..=0 dBFS.

Report: `{"mix": {...}, "captures": [{"input", "report", "error"}], "warnings"}`, where `error` is `{"code", "message", "details"}` or `null`. The `mix` object contains:

- **Setup:** `sample_rate_hz`, `channels`, `block_frames`.
- **Timing:** `elapsed_ms`, `blocks_mixed`, `late_blocks`, `skipped_blocks`, `mix_time`.
- **Program:** the `program` meter (`{"peak_dbfs", "rms_dbfs"}`, before the limit), `clipped_samples`, `invalid_samples`: the program bus.
- **Aux buses:** an `aux_buses` array, each `{"name", "meter", "clipped_samples", "invalid_samples"}`: every bus is limited and counted where it leaves the graph, also when no output takes it.
- **Inputs:** an `inputs` array. Each entry has:
  - `name`, `kind` (`live`, `tone`, or `silent` for a channel whose device was not found), `gain_db`, `mute`
  - `meter`
  - `warmup_frames`, `underrun_frames`, `trimmed_frames`
  - `drift_ppm` (live inputs): the input clock's offset against the mixer clock, as tracked by the adaptive resampler
  - `ring` (`{"capacity_frames", "pushed_frames", "pulled_frames", "overrun_frames", "discarded_frames", "max_push_frames"}`)
  - `buffer` (`{"target_ms", "min_ms", "mean_ms", "max_ms", "windows_ms"}`), where `windows_ms` is the mean fill per 10 s window, oldest first

`warnings` lists non-fatal observations, e.g. "no frames received" on a static screen, or a delivered source kind that differs from `target_kinds`. When a probe fails, warnings gathered before the failure are in `error.details.warnings`. If the portal session could not be closed afterwards, `error.details.session_close_error` says why.

### Outputs: recording and streaming

Outputs share one **media pipeline**: the active scene (see below; by default the `probe_composite` reference scene) at 1920×1080@60, and the audio mix. The preview uses the same pipeline.

- **Start and stop:** the first output to start brings the pipeline up with the active scene; screen capture comes from the scene's screen sources. The pipeline stops when the last output and the preview stop.
- **Output profiles (ADR 0017):** each output has a profile: the bus it takes as its mix, and its encoder settings (`recording`: `mix`, `tracks`, `video_encoder`; `stream`: `mix`, `video_bitrate_kbps`, `video_encoder`; `monitor`: `mix`, `enabled`, `device`, see Monitor). By default all take the program bus, with `auto` encoders and 6000 kbit/s, and the monitor is off. `start_recording` and `start_streaming` use it; their `video_encoder` and `video_bitrate_kbps` override it for that run. The stream's target is not part of the profile: the URL stays a `start_streaming` parameter, and the key is never stored.
  - **`set_output_profile {output, bus_id | bus_name, tracks, video_encoder, video_bitrate_kbps, enabled}`** (at least one; `tracks` only for `recording`, the bitrate only for `stream`, the encoder not for `monitor`, `enabled` only for `monitor`): a new mix applies to a running output now, fading over one block; tracks and encoder settings wait for its next start. A running recording's mix cannot become one of the tracks it records (`invalid_state`, `details.bus`), since its tracks are fixed until it stops. The result lists the profiles, which fields applied now (`applied_now`) or wait (`next_start`), and `warnings`, e.g. a monitor device that did not open.
  - **Recording tracks:** the mix is audio track 1; `tracks` lists up to 7 more bus references (`[{"bus_id": "bus_…"} | {"bus_name": "Music"}]`); each entry requires at least one field, both must agree, and unknown fields are refused. Profiles still store and report bus ids (`tracks: ["bus_…"]`). Each bus is recorded as its own AAC stream in the Matroska file and named after its bus (the stream's `title`). A bus is in the list once and not as the mix too. The list is fixed when the recording starts; switching the mix live changes what track 1 carries, not its name. All tracks of a block travel as one queue item, so they share the queue's depth and are dropped together. The report's `recording.audio_tracks` lists every track in file order, the mix first, each with its `name`.
  - `remove_bus` is `invalid_state` while a profile takes the bus as its mix or a track (`details.outputs`).
- **A missing device** does not keep the pipeline from starting (ADR 0017): when a configured channel's device is not found, the channel is silent for the run, and that is reported everywhere. The `warnings` of `start_recording`, `start_streaming` and `set_preview` say so, `get_mixer` repeats it while the run lasts, `get_health` lists the channel's capture as `ended` with `reason: "could not start: …"`, and the stop report has the device's error in `pipeline.audio_captures` and a warning that names the channel. Any other capture error (not permitted, no audio server, …) still fails the start, since the user has to act. A device that disappears during a run ends its capture the same way (`get_health`), and the channel goes silent; the outputs keep running. Nothing reconnects on its own: connect the device, or stop the outputs and preview and choose another input with `set_channel_input`. `probe_audio_mix` tests devices, so there a missing device is `not_found`.
- **Joining:** an output started while another output or the preview runs joins the pipeline.
- **Audio:** every run mixes the configured channels and buses (see Mixer; initially one channel, `Tone`, a 1 kHz tone at −20 dBFS), so the profiles' buses are always there. The outputs have no audio parameters of their own (ADR 0017): set the channels up with the mixer commands, or load a project, before the start.
- **Isolation:** each output has its own encoders, queue and thread. A failing stream never stops a recording, and vice versa.
- **Exclusive engine:** while the pipeline runs (for an output or the preview), the probe commands fail with `invalid_state`. They would compete for devices and CPU and block the engine thread.

#### Recording

`start_recording` records to H.264 + AAC in Matroska. It returns as soon as the file header is written; recording continues on its own threads until `stop_recording`. Closing the engine's stdin, or a quit signal (see above), also stops and finalizes it. It is `sensitive` because it captures and stores private content.

| Param            | Type | Default | Meaning |
|------------------|------|---------|---------|
| `file_name`      | string | required | Plain file name ending in `.mkv`, created in the engine's `--output-dir` (see File names); an existing file is never overwritten (`invalid_params`). A missing output directory is `not_found`, an unwritable one `permission_denied`. |
| `video_encoder`  | `"auto"`, `"software"`, `"hardware"` | the recording profile's (`"auto"` by default) | H.264 encoder (ADR 0011). `auto` tries this platform's hardware encoders first and falls back to software. `software` tries libx264, then OpenH264, then the build's default H.264 encoder (for FFmpeg builds without the GPL). `hardware` fails with `unsupported` (listing each candidate and why it did not open) when no hardware encoder opens. |

The result's `video_encoder` is `{"encoder", "hardware", "skipped": [{"encoder", "reason"}]}`: the encoder in use and the candidates tried before it.

Errors:
- `invalid_state`: a recording is open (`details.recording` names it), or `stop_recording` was called without one. If the recorder or the pipeline stopped on its own, `get_status` reports `recording_state: "failed"`; `stop_recording` then finalizes the file. It returns the recorder's error if the recorder failed; if only the pipeline failed, it returns the report, whose pipeline fields and `warnings` say what failed.
- `unsupported`: no recorder on this platform (Linux, Windows and macOS have one; `docs/platforms.md`), or no H.264 encoder of the requested kind opened (the message lists why for each).
- `backend_error`: FFmpeg could not open the output or an encoder, or failed while encoding or finalizing.
- Capture errors are the same as for the probes, except that a configured channel's missing device leaves that channel silent instead (see above).

If `start_recording` fails, the file it created is removed.

`stop_recording` sets one end time for every track, detaches the output, drains its queue, flushes the encoders and writes the trailer. Report: `{"recording", "pipeline", "warnings"}`.

- **`recording`:** `path`, `container`, `duration_ms`, `file_bytes`, plus:
  - `video` (`encoder`, `hardware`, `skipped`, `width`, `height`, `fps`, `frames_encoded`, `frames_dropped`, `frames_after_end`, `first_pts_ms`, `end_ms`)
  - `audio_tracks`: every audio track in file order, the mix first (see Recording tracks), each `{"name", "encoder", "sample_rate_hz", "channels", "bitrate_bps", "samples_encoded", "blocks_dropped", "blocks_after_end", "gap_samples", "overlap_samples", "discontinuities", "padding_samples", "first_pts_ms", "end_ms"}`

  Every track is cut at the stop time; `*_after_end` counts what producers emitted while stopping.
- **`pipeline`:** `{"stopped", "render", "mix", "audio_captures", "scene_captures", "scene_captures_omitted", "monitor"}`. `stopped` says whether this output was the last one, so the pipeline stopped with it. Only then are `render` (composite stats), `mix` (as in `probe_audio_mix`), `audio_captures` (as `captures` in `probe_audio_mix`), the scene captures and `monitor` filled in. Otherwise they are `null` or empty.

If finalizing fails, the error's `details` are `{"recording", "pipeline", "warnings"}`: the partial `recording` stats (left out when the recorder kept none), the `pipeline` as in the report, and `warnings`. The file is kept.

#### Streaming

`start_streaming` publishes over RTMP: H.264 with constant bit rate and a keyframe exactly every 2 s, plus AAC, in FLV. It is `sensitive` because it publishes private content and handles a credential.

- **First connection:** the command returns once it is established. If that fails, the error is `unavailable` and nothing is retried.
- **Reconnects:** later network or encoder errors trigger reconnects with exponential backoff. Each new connection uses fresh encoders and starts with a keyframe at timestamp 0. The attempt count restarts only after a connection stayed up for 10 s, so a server that keeps accepting and dropping ends in `failed`.
- **Timeouts:** connect, handshake and writes are bounded by a 5 s timeout. `stop_streaming` gives a healthy connection 2 s to flush, then interrupts blocked I/O. Name resolution is not bounded.

| Param                | Type | Default | Meaning |
|----------------------|------|---------|---------|
| `url`                | string | required | `rtmp://` or `rtmps://` server URL with host and application, without credentials (`user@` is rejected) and without the key. |
| `stream_key`         | string | none | Appended as the last path segment when connecting. No `/` or whitespace. **Secret:** never echoed in results, errors, logs or `Debug` output; reports show `<stream key>`. |
| `video_bitrate_kbps` | integer 300..=50000 | the stream profile's (6000 by default) | Video bit rate (CBR, buffer of 2 s). |
| `video_encoder`      | as for `start_recording` | the stream profile's (`"auto"` by default) | Reconnects try the encoder that opened first, then the other candidates. |
| `reconnect_attempts` | integer 0..=100 | 5 | Attempts after a lost connection; `0` gives up right away. The count restarts after a connection stayed up for 10 s. |
| `reconnect_delay_ms` | integer 100..=60000 | 1000 | Wait before the first attempt; doubles per attempt, up to 30 s or this delay, whichever is larger. |

Targets are shown redacted: scheme, host and application. Deeper path segments become `***`, a query becomes `?***` and the key becomes `<stream key>`. Once streaming has been used, FFmpeg's own log output is turned off, because RTMP error messages can contain the stream path.

`get_status.streaming_state` is one of `connecting`, `connected`, `reconnecting` or `failed`, or `null` without a stream. `failed` means the reconnect attempts are exhausted, or the pipeline stopped. From then on, media is dropped and counted until `stop_streaming`. A stream is active (it connects or delivers) in every state but `failed`.

Errors:
- `invalid_state`: a stream is already open, or `stop_streaming` was called without one.
- `unavailable`: the server could not be reached, or the connection or publish handshake failed; `details.target` holds the redacted target.
- `unsupported`: no streaming on this platform (Linux, Windows and macOS have it).

`stop_streaming` returns a report, also for a failed stream: `{"streaming", "pipeline", "warnings"}` (`pipeline` as for recording). Only if the streamer's thread cannot be stopped cleanly (an internal failure) is it an error; its `details` are then `{"pipeline", "warnings"}`, while the stream's own statistics are lost. `streaming` contains:

- **Target and state:** `target` (redacted), `container` (`flv`), and `state` at the moment the stop was requested: `connected`, `reconnecting` or `failed`.
- **Connection history:** `connects`, `disconnects`, `reconnect_attempts`, `connected_ms`, and `last_error`, which never contains the key.
- **Tracks:** `video` and `audio_tracks` (one entry, the mix) with totals over all connections, with the fields of a recording's tracks; their `first_pts_ms` and `end_ms` refer to the last connection.
- **Offline drops:** `frames_dropped_offline` and `blocks_dropped_offline` count media the streamer discarded while not connected. Audio received before a connection's first video frame counts here too. Both numbers are included in the tracks' `frames_dropped` and `blocks_dropped`, which also count producer-side drops (copy pool or queue full).
- **Undelivered media:** `frames_undelivered` and `samples_undelivered` count media that was encoded but never reached the server because the connection broke (in the encoder or in the failed write; audio in whole encoder frames). Both are included in `frames_encoded` and `samples_encoded`.

#### Monitor

The monitor plays a bus on an audio device, e.g. headphones, while the pipeline runs and the monitor is on (ADR 0017). It is not an encoder output: it cannot be recorded, it has no report of its own, and its failures never touch the recording or the stream.

- **On and off:** `set_output_profile {"output": "monitor", "enabled": true}`. It is off by default. While the pipeline runs, switching applies now (`applied_now: ["enabled"]`), ramped over one block. Off, a device that played in this run stays open and plays silence, so switching back on is instant; it closes when the pipeline stops. Switched on while idle, the monitor starts with the next pipeline (an output or the preview).
- **Its bus:** `bus_id` or `bus_name`, by default the program bus; usually a separate mix (an aux bus with `assign_db: null`) with pre-fader sends, so the monitor level does not follow the faders. On a device that runs (on, or off and playing silence), a new bus applies now (`applied_now`), faded over one block, like an output's mix; otherwise it waits for the monitor to be switched on (`next_start`).
- **Its device:** **`set_monitor_device {device}`**, a name from **`list_audio_outputs`**, or `null` or left out for the system default; only while the pipeline is idle (`invalid_state` otherwise), and `sensitive`, since it chooses where audio is played. Linux plays through PipeWire, Windows through WASAPI and macOS through CoreAudio (both through cpal, `docs/platforms.md`). A device that goes away during the run (headphones unplugged) ends the monitor; it is never moved to another device, such as the speakers, where the microphones would pick it up. On the system default, the monitor stays on the device that was the default when it started.
- **A device that does not open** leaves the monitor silent, with a warning in the start's `warnings` (or `set_output_profile`'s), in `get_mixer` while the run lasts, and in `get_health` (`state: "ended"`, `reason: "could not start: …"`). Switching it on again tries again. A device that ends during the run ends the monitor the same way (`get_health`, and a warning in the stop report); the outputs keep running.
- **Feedback:** when a channel captures what the monitor device plays (an input whose `loopback_of` names the device: a PipeWire sink's monitor, or an output's `<output>.monitor` on Windows and macOS), switching the monitor on warns, every time, and so do the run's audio warnings while it is on: if the monitor's bus has that channel, it feeds back. The system default output is not compared, as its name is not known to the engine. If the inputs cannot be listed for the check, that is a warning too.
- **Clocks:** the device runs on its own clock. The mixer writes each block into a lock-free ring that the device reads on its own thread without locks, waiting or allocation, and resamples it by up to ±2000 ppm to hold the ring at the device's largest recent pull plus one block, as for live inputs. The device plays silence when the ring runs short (counted as `underrun_frames`, after the start-up's `warmup_frames`); the mixer then tops the ring up with silence (`padded_frames`), also when the device's pull grows. Far above the target, a block is left out (`trimmed_frames`).
- **The stop report** of the output that stops the pipeline has `pipeline.monitor` with the final counters (`latency_ms` is `null` there), and a warning when the monitor ran short or over, or ended.

### Scenes, mixer and preview

The engine keeps a collection of scenes on a 1920×1080 canvas and renders the **active** one. Changes to the active scene, or switching scenes, apply live while preview or outputs run. A live change that fails (e.g. the picker of a new screen source is cancelled) is undone and returns the error. Scenes live in memory; `save_project` and `load_project` keep them in a file (see Projects).

At start the collection has one scene, `Test pattern`: full-canvas color bars plus a moving gray `overlay`, the M3 reference scene. Every scene command returns the scene list: `{"canvas_width", "canvas_height", "active_scene_name", "active_scene_id", "scenes": [{"id", "name", "sources": [{"id", "name", "kind", ..., "x", "y", "width", "height", "opacity"}]}]}`. `active_scene_name` is the active scene's name, `active_scene_id` its id.

- **Ids (ADR 0014):** every scene and source has a stable id that the engine assigns: `scene_` or `src_` and 12 random characters (`a-z`, `0-9`). Clients treat ids as opaque strings and take them from `list_scenes`.
  - An id never changes while its scene or source exists. Ids are random, so a removed source's id does not come back, and source ids are unique across all scenes.
  - The built-in scene and its sources have fixed ids: `scene_test_pattern`, `src_color_bars` and `src_overlay`.
  - `render.sources[].id` (in `get_health` and the stop reports) and `captures[].id` are these source ids.
- **Naming a scene or source:** every request reference uses `<entity>_id` or `<entity>_name`.
  - A scene: `scene_id` or `scene_name`. A source: `source_id`, or its `scene_id`/`scene_name` and `source_name`.
  - Channels use `channel_id`/`channel_name`, buses use `bus_id`/`bus_name`, including an output profile's mix and each recording track.
  - The id is looked up first. Given both, they must name the same object, otherwise `invalid_params`. A source's scene, when given with its id, must also agree.
  - An unknown reference is `not_found`; `details` names the supplied field: `scene_id`, `scene_name`, `source_id`, `source_name`, `channel_id`, `channel_name`, `bus_id` or `bus_name`. For an entry of `set_output_profile`'s `tracks`, `details` also has `track_index` (from 0), since the mix takes `bus_id` and `bus_name` too.
  - A new object's own name stays `name` (`create_scene`, `add_source`, `add_channel`, `add_bus`); listed objects keep their own `id` and `name`.
  - `capture_target_id` names the monitor or window a screen source captures, from `list_capture_targets`.
- **Scene commands:**
  - `create_scene {name}` creates an empty scene (black); the scene list holds its id.
  - `set_active_scene {scene_id | scene_name}` switches the rendered scene.
  - `delete_scene {scene_id | scene_name, confirm}`: deletion is irreversible, so it is `sensitive`, `confirm` must be `true` (`invalid_params` otherwise), and the active scene cannot be deleted (`invalid_state`).
- **Access:** `add_source`, `set_active_scene` and `restart_source` are `sensitive` because they can start a screen capture. `update_source` and `remove_source` (undone by adding the source again) are `control`.
- **Names:** 1–64 characters, no control characters, no surrounding whitespace, unique within their collection.
- **Limits:** at most 16 scenes and 16 sources per scene (`invalid_state`). An unknown scene or source is `not_found`.
- **`add_source`:** params `scene_id` or `scene_name`, `name`, `kind`, optional rectangle and `opacity`. The scene list holds the new source's id.
  - Kinds:
    - `color_bars`: stretched to the rectangle.
    - `solid` with `color` `#rrggbb` or `#rrggbbaa`: stretched.
    - `screen` with optional `capture_target_id`, `target_kinds` and `cursor_mode` as for the screen probe: a live capture, fitted into the rectangle with its aspect ratio kept. The scene list shows `capture_target_id` when one is set.
  - Rectangle: `x`, `y`, `width` and `height` in canvas pixels, all four or none (none means the whole canvas).
  - `opacity` 0..=1.
  - Sources are drawn in order, so later ones are on top.
  - A screen source starts its capture when it becomes part of the rendered scene: the named source directly, otherwise through the system picker.
  - A capture keeps running while its source exists in any scene, so switching scenes back and forth does not ask again. Removing the source from every scene ends the capture.
  - While a recording or stream is open, a change that would open the picker for a new capture is refused (`invalid_state`): the picker waits for a human and would block the engine. Add picker sources while only the preview runs. Named sources (`capture_target_id`) start without waiting and are allowed.
  - Capture reports are in the output reports' `pipeline.scene_captures` (`{"id", "name", "report", "error"}`, with the source's id and name): the most recent 64, with `scene_captures_omitted` counting older ones.
- **`update_source`:** the source (`source_id`, or `scene_name` or `scene_id` and `source_name`), and any of `x`, `y`, `width`, `height`, `opacity`.
- **`remove_source`:** the source (`source_id`, or `scene_name` or `scene_id` and `source_name`).
- **`restart_source`:** the source (`source_id`, or `scene_name` or `scene_id` and `source_name`), optional `capture_target_id`. Starts a screen source's running capture again and ends the old one, e.g. after `get_health` shows it `ended` (ADR 0013). Outputs keep running. Until the new capture delivers its first frame, a rendered source is not drawn (counted in `missing_ticks`).
  - **Which sources:** screen sources with a running capture (in `get_health.captures`): those of the active scene and those kept for other scenes. Other kinds are `invalid_params`. A source without a capture, no pipeline, or a pipeline that stopped is `invalid_state`.
  - **`capture_target_id`:** capture this source from `list_capture_targets` from now on; the scene keeps it. A closed window's id never comes back, so restarting a window source needs the id of a window that exists. A display's id returns when the display reconnects, so the same id works then.
  - **Pickers:** a picker source asks the user again, so it is refused while a recording or stream is open (`invalid_state`). Pass a `capture_target_id`, or restart it while only the preview runs. On Linux, every screen source is a picker source.
  - **Errors change nothing:** the new capture starts before the old one ends. If it fails, the old capture keeps running, the scene keeps its old `capture_target_id`, and the error is returned (e.g. `not_found` for a gone source).
  - Both captures' reports are in the output reports' `pipeline.scene_captures`.
  - Live audio inputs cannot be restarted: the mixer's inputs are fixed while it runs.
  - It is destructive: it ends a running capture, and a picker choice is gone.
- **`set_capture_target`:** the source (`source_id`, or `scene_name` or `scene_id` and `source_name`), and `capture_target_id` from `list_capture_targets`. Chooses what a screen source captures from now on, e.g. after `load_project`, which does not store it.
  - Only while the source has no capture; with one (running or ended), it is `invalid_state` (switch it with `restart_source` and a `capture_target_id`). Other kinds are `invalid_params`.
  - It is `sensitive`, like `add_source`: it decides what is captured. If the source is in the rendered scene, its capture starts now, and a failure (e.g. `not_found` for a gone source) changes nothing.

Mixer (ADR 0017):
- **Channels:** the pipeline mixes a list of channels, each one input's strip: what it plays (a capture device or a test tone), a fader, mute and balance, and one send to every bus. At most 8 channels. Channels have stable ids (`ch_…`, ADR 0014), and commands name one by `channel_id`, `channel_name` or both (which must then agree; the id is looked up first). Names are unique among the channels.
- **Buses:** the program bus, `bus_program` named `Program`, is the main mix and every output's default (see Output profiles); it cannot be removed. Up to 8 aux buses (`bus_…`) add submixes and separate mixes: each has a fader and mute, and `assign_db`, its level into the program bus, or `null` for a separate mix (e.g. for monitoring, or for an output of its own). Every bus is hard-limited to [−1, 1] where it leaves the graph. Bus names are unique, `Program` included. Two tiers only: an aux bus feeds the program bus or nothing, so no loop can be built.
- **Sends:** each channel has one send per bus, in bus order (program first): `{"bus", "enabled", "level_db", "tap"}`. `tap` is `post_fader` (after fader and balance) or `pre_fader` (after mute, before the fader: the channel fader does not move it). Mute comes before both taps, so a muted channel is silent in every bus. A new channel sends to the program bus post-fader at 0 dB and nowhere else; a new bus starts with every channel's send to it off.
- **`get_mixer`:** returns `{"running", "channels": [{"id", "name", "input", "gain_db", "mute", "balance", "sends", "peak_dbfs"}], "buses": [{"id", "name", "kind", "gain_db", "mute", "assign_db", "peak_dbfs"}], "outputs": [{"output", "bus_id", "tracks", "running"}], "warnings"}`; `outputs` has the recording, the stream and the monitor; an output's `bus_id` is the one it takes now, or would take if it started now (its profile's), and the monitor is `running` while it plays its bus; buses are listed with the program bus first (`kind: "program"`, then `"aux"`). `input` is `{"kind": "device", "device": name or null for the default input}` or `{"kind": "tone", "frequency_hz", "level_dbfs"}`. Peaks are the highest level of the last 300 ms: a channel's after its fader, a bus's before the output limit, so it can exceed 0 dBFS when the mix overloads (`null` for silence, or when not running). Reading them changes nothing, so several clients can poll. `warnings` lists routing that is allowed but rarely meant: a channel that reaches the program bus more than once, directly and through each assigned aux bus it sends to (+6 dB for two equal paths). It is about the routing, so mutes and levels, which change live, do not count.
- **`add_channel {name, input, gain_db?, mute?, balance?}`:** adds a channel at the end, with a fresh id. A tone's `level_dbfs` defaults to −12; gain is −96 to +24 dB, balance −1 (left only) to 1 (right only), centre unity on both sides. It is `sensitive`, because a device channel captures a microphone. A duplicate name is `invalid_params`, a ninth channel `invalid_state`.
- **`add_bus {name, gain_db?, mute?, assign_db?}`** (`assign_db` absent or `null`: a separate mix) and **`remove_bus {bus_id | bus_name}`** (which removes every send to it; the program bus is `invalid_params`).
- **Structure needs an idle pipeline:** `add_channel`, `remove_channel {channel_id | channel_name}`, `set_channel_input {channel_id | channel_name, input}` (`sensitive`, as it chooses what is captured), `add_bus` and `remove_bus` change the mixer's structure, so they are `invalid_state` while a preview or output runs.
- **Live:** **`update_channel {channel_id | channel_name, gain_db?, mute?, balance?}`**, **`update_bus {bus_id | bus_name, gain_db?, mute?, assign_db?}`** (`assign_db: null` makes an aux bus a separate mix; on the program bus it is `invalid_params`) and **`set_send {channel_id | channel_name, bus_id | bus_name, enabled?, level_db?, tap?}`** apply while running, ramped over one block so nothing clicks; a tap change fades out on the old tap, then in on the new one. They are kept in the configuration, which is what every run mixes.

Preview:
- **What it does:** `set_preview {enabled}` keeps the pipeline running for a preview. Frames (480×270, 30 FPS) go to an in-process mailbox that embedding UIs read through `Engine::preview_frames`. Over this protocol no frames are sent; the command only controls the pipeline.
- **Access:** `sensitive`, because it renders (and can capture) private content.
- **Lifecycle:** the pipeline stops when the preview and all outputs are off.
- **Turning the preview off:** reports `frames_published` and `frames_dropped` (no free buffer because the viewer held frames). If that stopped the pipeline, `pipeline` (as in output reports, with `stopped: true`) and `warnings` carry its report; otherwise `pipeline` is `null`. `fps` is `{"num", "den"}`.

### Projects

A project file holds the scenes with their ids, the active scene and the audio channels and program bus (ADR 0015, ADR 0017). Files live in the engine's **project directory**, set at start with `--project-dir` (`streamforge-engine`, `streamforge-mcp`; a directory that does not exist stops them at start).
- Without it, the project commands are `invalid_state`, and the MCP server does not offer them.
- Commands name a file by a plain `.json` name (see File names); anything else is `invalid_params`.
- Only regular files are read or replaced. A link is followed when reading, but saving does not replace it with a file; a directory or device is `invalid_params`.

- **`list_projects`:** the files a load can name: regular files (or links to one) in the project directory whose names end in `.json` and pass the file name rule above, sorted by name, each with its size in `bytes` and `modified_unix_ms` (when it was last written, `null` where the system does not say). At most 256 are listed; `omitted` counts the rest. The files are not read; `validate_project` checks one. `read_only`.
- **`save_project {file_name, overwrite}`:** writes the current scenes and audio, also while the pipeline runs (the configured audio).
  - An existing file is `invalid_params` unless `overwrite` is `true`. A replaced file keeps its permissions (Windows refuses to replace a read-only file).
  - Writing is atomic: a temporary file next to the target, flushed to disk and renamed over it. A save that fails or is cut off leaves the previous file whole.
  - Result: `{"file_name", "path", "replaced", "scenes", "bytes"}`.
- **`load_project {file_name}`:** replaces the scenes, the active scene and the audio with the file's, all or nothing.
  - Only while no preview or output runs (`invalid_state`). Unsaved scenes are lost, so it is destructive.
  - Screen sources load without a capture target; a warning says so. Choose one with `set_capture_target`. Where a system picker exists (Linux), it can ask instead when the scene is rendered.
  - An invalid file loads nothing: `invalid_params`, with every problem in `details.problems` (`[{"path", "message"}]`, at most 100 plus one that counts the rest).
  - Result: the scene list, plus `file_name`, `mixer` (as `get_mixer`) and `warnings`.
- **`validate_project {file_name}`:** the same checks without loading. Result `{"file_name", "valid", "schema_version", "migrates", "problems"}`; `migrates` is true for a valid file of an older schema version. An invalid file is a result, not an error. `read_only`.
- **Errors:** a missing file (or, for a save, a missing directory) is `not_found`, an unreadable one `permission_denied`, a file over 1 MiB `invalid_params`.
- **Not stored:** stream keys, file paths, and what screen sources capture (`capture_target_id`: machine-specific, and window titles can be private).

The file (`schema_version` 3):

```json
{"schema_version": 3,
 "canvas": {"width": 1920, "height": 1080},
 "active_scene": "scene_k3j9x2m4q8w1",
 "scenes": [{"id": "scene_k3j9x2m4q8w1", "name": "Show", "sources": [
   {"id": "src_a1b2c3d4e5f6", "name": "red", "kind": "solid", "color": "#ff000080",
    "x": 10.0, "y": 20.0, "width": 300.0, "height": 200.0, "opacity": 0.5}]}],
 "audio": {"channels": [{"id": "ch_4k2m9x0q1w7e", "name": "Mic",
                         "input": {"kind": "device", "device": null},
                         "gain_db": 0.0, "mute": false, "balance": 0.0,
                         "sends": [{"bus": "bus_program", "enabled": true, "level_db": 0.0,
                                    "tap": "post_fader"}]}],
           "buses": [],
           "program": {"gain_db": 0.0, "mute": false}}}
```

- **Sources** are their `list_scenes` objects without `capture_target_id`, checked by the `add_source` rules; a hand-written source may leave out the rectangle (whole canvas) and `opacity` (1).
- **Channels** are their `get_mixer` objects without `peak_dbfs`, every field given (a device channel's `device` may be left out or `null` for the default input), checked by the `add_channel` rules. Their **`sends`** are checked by the `set_send` rules, each naming an existing bus once; a send that is left out is the default (on to program, off to an aux bus), in any order.
- **`buses`** (aux buses, in mixer order; may be left out for none) are their `get_mixer` objects without `kind` and `peak_dbfs`, checked by the `add_bus` rules (`assign_db` may be left out for a separate mix). **`program`** is checked by the `update_bus` rules.
- **`outputs`** holds the output profiles as `get_output_profiles` shows them, every field given (but the recording's `tracks` may be left out for none, and the monitor's `device` for the default); a profile that is left out (or `outputs` as a whole) is the default. Each is checked by the `set_output_profile` rules, the monitor's `device` by the `set_monitor_device` rules; its `mix` and each of its `tracks` (by id) must name a bus of the file, and a bus is a track once and not the mix as well. A loaded profile with the monitor on plays it from the next pipeline start.
- **Checked across the file:** scene and source ids (form, and unique; source ids across all scenes), unique scene names, unique source names within a scene, at most 16 scenes of 16 sources, `active_scene` naming a scene, and the canvas matching the engine's. The audio is checked by the graph's own rules, the ones the engine keeps: channel and bus ids (form, unique, an aux bus not `bus_program`), unique channel names, unique bus names (`Program` is taken), at most 8 channels and 8 aux buses, and every send naming an existing bus once. An entry that is wrong on its own still counts for these, so every problem is reported at once.
- **Version 1** (whose `audio` was the parameters of the former `set_audio_sources`: `inputs`, `tones`, `master_gain_db`) is migrated on load, with version 1's rules kept as they were. Inputs, then tones, become channels named `Input 1`, `Tone 1`, … at centre balance, with ids from their position (`ch_input_1`, `ch_tone_1`, …), so an unsaved version 1 file gives the same ids every time; the master gain becomes the program bus fader. `load_project` warns that it migrated, and saving writes version 3.
- **Versions 1 and 2** also migrate screen sources' `source_types` to `target_kinds` and `cursor` to `cursor_mode` before source decoding. Version 3 refuses those old fields. Capture targets are never stored: an old file carrying `source_id` is refused at that path, and version 3 refuses `capture_target_id`. Sources must not carry `scene_id` or `scene_name`; their scene is the containing one. `load_project` warns on migration, `validate_project.migrates` reports it, and saving writes version 3.
- **Strict:** unknown fields and duplicate keys are problems, and a `schema_version` other than 1, 2 and 3 is refused (a newer one with a hint to update).
- **Left out or `null`:** an optional single value may be left out or `null` for its default: a source's rectangle and `opacity`, a device (the system default), `assign_db` (a separate mix), `outputs` and each of its profiles. A list that may be left out (`buses`, a channel's `sends`, the recording's `tracks`) is left out for none; `null` is a problem there.
- `streamforge project validate FILE` runs the same check from the CLI, prints the result as JSON and exits 1 if the file is not valid.

## Example session

```console
$ printf '%s\n' '{"id":1,"command":"get_status"}' '{"id":2,"command":"nope"}' | cargo run -q -p streamforge-engine
{"v":1,"id":1,"ok":true,"result":{"application":"StreamForge","capture_available":true,"pipeline_running":false,"preview_active":false,"recording_state":null,"streaming_state":null,"version":"0.1.0"}}
{"v":1,"id":2,"ok":false,"error":{"code":"unknown_command","message":"unknown command `nope`","details":{"available":["get_status","get_health","list_commands","get_capture_capabilities","list_audio_inputs","list_audio_outputs","list_capture_targets","probe_screen_capture","probe_audio_capture","probe_composite","probe_audio_mix","start_recording","stop_recording","start_streaming","stop_streaming","list_scenes","create_scene","delete_scene","set_active_scene","add_source","update_source","remove_source","restart_source","set_capture_target","get_mixer","add_channel","remove_channel","update_channel","set_channel_input","add_bus","remove_bus","update_bus","set_send","get_output_profiles","set_output_profile","set_monitor_device","set_preview","list_projects","save_project","load_project","validate_project"],"command":"nope"}}}
```

Key order inside `result` and `details` is not part of the contract. `capture_available` depends on the machine; this output is from a Linux desktop session.

## Versioning policy

- **Before the first release** (0.1.0), nobody depended on the protocol, and its only clients (desktop, CLI, MCP server) lived in this repository, so v1 changed incompatibly, every client in the repository with it. Those changes, kept as history:
  - 2026-10-27, ADR 0018 step 2c: report shapes. A recording's `audio` and `extra_audio` became `audio_tracks` (the mix first), a stream's `audio` became `audio_tracks` with one entry. The stop reports' `pipeline_stopped`, `render`, `mix`, `audio_captures`, `scene_captures`, `scene_captures_omitted` and `monitor` moved into `pipeline` (with `pipeline_stopped` as `stopped`), as in `set_preview`; so did the pipeline fields of a failed stop's `details`. Errors inside results (`captures[].error`, `audio_captures[].error`, `scene_captures[].error`, `screen_capture_error`) are `{"code", "message", "details"}` with command error codes instead of `{"kind", …}` with capture error kinds, and `snapshot.error` is that object instead of a string. `scene_captures[].source` → `name`. Units and words: `sample_rate` (start reports) and `rate` (audio tracks, capture `format`, `mix`) → `sample_rate_hz`, audio tracks' `bitrate` → `bitrate_bps`, `mix.master` → `program`, `mix.inputs[].muted` → `mute`, `probe_audio_mix`'s `master_gain_db` → `program_gain_db` (CLI `--master-gain-db` → `--program-gain-db` for `mix`, `record` and `stream`). Nominal frame rates are `{"num", "den"}`: the start reports' and `set_preview`'s `fps`, the tracks' `video.fps` and `render.target_fps`. Every result's field names and JSON types are pinned in `crates/command-api/tests/wire_shapes.txt`.
  - 2026-10-27, ADR 0018 step 2b: capture-target `source_id` → `capture_target_id`, `source_types` and capabilities' `source_kinds` → `target_kinds`, `cursor` → `cursor_mode`, capability `source_list` → `target_list`; `list_screen_sources` → `list_capture_targets` with result `sources` → `targets`; `set_screen_source` → `set_capture_target`. Request references now use `scene_id`/`scene_name` (formerly `name` or `scene`), `source_id`/`source_name` (formerly `id`/`name`), `channel_id`/`channel_name` and `bus_id`/`bus_name` (formerly `id`/`name`, or `channel`/`bus` in `set_send`); output mix selectors `mix_bus_id`/`mix_bus` → `bus_id`/`bus_name`. `not_found` details use those supplied selector names (plus `track_index` for a recording track). Scene-list `active`/`active_id` → `active_scene_name`/`active_scene_id`; `get_mixer.outputs[].bus` → `bus_id`. Recording request `tracks` now takes explicit bus references, with both-given agreement and no string heuristic; profile results and files still store bus ids. `probe_composite.snapshot_path` → `snapshot_file_name`. CLI `record --file` → `--file-name`, `stream --key-env` → `--stream-key-env`, `stream --bitrate-kbps` → `--video-bitrate-kbps`, `--source-id` → `--capture-target-id`, `--source` → `--target-kind`, `--cursor` → `--cursor-mode`, `compose --snapshot` → `--snapshot-file-name`, `capture sources` → `capture targets`. Project files are schema 3 for the screen field renames; versions 1 and 2 migrate on load.
  - 2026-10-26, ADR 0018 step 2a: `get_status` lost `media_engine_ready`, `recording_active`, `recording_failed` and `streaming_active` and gained `recording_state` (`null`, `running` or `failed`). `start_recording` and `start_streaming` lost the screen layer (`screen`, `source_id`, `source_types`, `cursor`, now unknown fields), the stop reports and partial-error details lost `screen_capture` and `screen_capture_error`, renders no longer have a `screen_layer` source, and `get_health.captures[].id` is never `null`. In `get_health`, `recording.state` is `running` or `failed` instead of `null`, and `connects` and `reconnect_attempts` are only in `streaming`. `set_preview`, `set_monitor_device` and `set_screen_source` are destructive. File names follow one rule on every system (File names): `\`, `:`, `<`, `>`, `"`, `|`, `?`, `*`, control characters and a device name before the first dot are refused; a project file saved before under such a name is no longer listed or loaded, and has to be renamed. A repeated key in a request is refused (`invalid_request`, or `invalid_params` inside `params`), and `v` is checked before unknown fields. `invalid_params` lists unknown parameters in `details.unexpected` for every command. The CLI's `record --screen` and `stream --screen` add a screen source to the active scene instead.
  - 2026-10-22, ADR 0014: scene and source `id` changed from a per-run number to a stable string, and `render.sources[].id` changed with it (in `probe_composite`: `"bars"`, `"screen"` and `"overlay"`). `add_source` no longer requires `scene` (`scene_id` works too); `update_source`, `remove_source` and `restart_source` also take `id`. `scene_captures` entries gained `id`.
  - 2026-10-25, M12 step 3: `set_output_profile` refuses a running recording's track as its new mix (`invalid_state`, `details.bus`); before, the file could carry the same bus twice until the recording stopped.
  - 2026-10-24, ADR 0017 step 5a: the monitor. `get_output_profiles` gained `monitor` (and a project file's `outputs` may hold it), `set_output_profile` takes `output: "monitor"` and `enabled` and its result gained `warnings`, `get_mixer`'s `outputs` gained the monitor's entry, `get_health` gained `monitor`, the stop reports gained `monitor`, `list_audio_inputs` entries gained `loopback_of`; `list_audio_outputs` and `set_monitor_device` are new.
  - 2026-10-23, ADR 0017 step 3b: `start_recording` and `start_streaming` lost `inputs`, `tones` and `master_gain_db` (now unknown fields, `invalid_params`); every run mixes the configured channels. The CLI keeps `record`'s and `stream`'s `--input`, `--tone`, `--no-tone` and `--master-gain-db`, which now set up the engine's channels (`Input 1`, `Tone 1`, …) and the program bus fader through the mixer commands before the start; a device not found there leaves its channel silent, with a warning, instead of failing the start.
  - 2026-10-23, ADR 0017 step 4: the recording profile gained `tracks`, the recording report `extra_audio`, every audio track's stats a `name`, and `get_mixer`'s `outputs` `tracks`.
  - 2026-10-23, ADR 0017 step 3a: `get_output_profiles` and `set_output_profile` are new; `start_recording`'s and `start_streaming`'s `video_encoder` and `video_bitrate_kbps` default to the profile's; `get_mixer` gained `outputs`, the mix stats `aux_buses`, and the project file `outputs`.
  - 2026-10-23, ADR 0017 step 2c: a configured channel's missing device no longer fails the pipeline's start (it was `not_found`); its channel is silent and reported. `start_recording` and `start_streaming` gained `warnings`, and mix inputs can be `kind: "silent"`.
  - 2026-10-23, ADR 0017 step 2b: `add_bus`, `remove_bus` and `set_send` are new, `update_bus` gained `assign_db`; `get_mixer` channels gained `sends`, buses `assign_db`, and the result `warnings`; the project file may hold `buses` and channel `sends` (files without them read as before).
  - 2026-10-23, ADR 0017 step 2a: `set_mixer_strip`, `set_master_gain` and `set_audio_sources` are replaced by `add_channel`, `remove_channel`, `update_channel`, `set_channel_input` and `update_bus`; `get_mixer` returns `channels` and `buses` with ids instead of `strips` by index and the master fields; audio `captures` in `get_health` carry the channel's `id` and name; the project file is schema version 2 (version 1 is migrated); `validate_project` gained `migrates`.
  - 2026-10-23, ADR 0017 step 1: the master meter (`mix.master`, `master_peak_dbfs`) measures the program bus before the output limit, so it can exceed 0 dBFS when the mix overloads; `clipped_samples` still counts what the limit changed.
- **From 0.1.0 on**, v1 is stable, and the rules below hold.
- Adding a command, adding optional fields to a result, or adding an error code is backward compatible within v1. Clients must ignore unknown result fields and treat unknown error codes as generic failures.
- Removing or renaming a field, command or error code, changing a field's type, or accepting new request envelope fields requires v2.
- Update the contract tests in `crates/control-protocol` and the pinned result shapes (`crates/command-api/tests/wire_shapes.txt`, see `docs/testing.md`) together with this document.
