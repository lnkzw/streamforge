# ADR 0013: Capture stream state

- **Status:** Accepted
- **Date:** 2026-10-21

## Context

A capture can end while outputs run. A captured window can close, the portal session can end, a microphone can be unplugged, or a permission can be revoked. Every backend already noticed this, but only reported it when the capture stopped:
- WGC `on_closed`
- the SCK delegate's `did_stop_with_error`
- the end of the PipeWire stream
- cpal's error callback

While running, `get_health` (ADR 0012) showed such a source only indirectly, through a growing `last_frame_age_ms`. A quiet source looks the same: WGC and SCK deliver frames only when the content changes. Clients could not tell "nothing moved" from "this source is gone" (assessment §9, step 4).

## Decision

- **`capture-api`:** a new `CaptureState`, `Running` or `Ended { reason }`.
  - `VideoStream::state` and `AudioStream::state` return it. They never block. The default is `Running`, so a backend that cannot tell stays truthful about what it knows.
  - "Stalled" is not a capture state: some systems deliver frames only when the content changes. Clients judge stalls from `last_frame_age_ms` and the source kind.
- **Backends:** the end reason is set once, from the thread that notices the end, and read without waiting.

  | Backend | Ends when | Reason |
  |---------|-----------|--------|
  | WGC (Windows) | the item is closed (`on_closed`), or the capture thread ended | "the captured window was closed", or the thread's end |
  | SCK (macOS) | the delegate reports `did_stop_with_error` | the system's error |
  | PipeWire (Linux) | the capture thread ends before a stop: a stream error, or the stream went unconnected after streaming | the error, or "the PipeWire stream ended" |
  | cpal (Windows, macOS audio) | the error callback reports `DeviceNotAvailable`, `StreamInvalidated`, `HostUnavailable` or `PermissionDenied` | cpal's message |

  - WGC and SCK use the end reason the shared `FrameSink` already kept (`FrameSink::state`). PipeWire and cpal keep it in a `OnceLock`.
  - cpal errors that do not end a stream stay warnings in the stop report, as before: xruns, reroutes and a busy device.
  - The cpal error callback runs off the data path and already allocated for its warnings. Setting the reason allocates once per stream.
- **`get_health`** gains `captures`: one entry per running capture.
  - The entries cover the scenes' screen sources, the legacy screen layer and live audio inputs.
  - Each entry has `kind` (`screen` or `audio`), `source_id` (scene sources only), `name`, `state`, and `reason` when the capture ended.
  - Ended captures stay in the list until they are stopped. Their outputs keep running, and the compositor keeps showing the source's last frame, as before.
- **CLI:** progress lines name ended captures with their reason.

## Alternatives considered

1. **A `stalled` state, decided by the backend.** A timeout per source kind would misreport static windows and idle displays as failures. The frame age already carries the information, without a guess.
2. **End the outputs, or remove the source, when a capture ends.** A recording that keeps the last frame is more useful than a recording that stops because a window closed. What to do next (restart, remove, switch scenes) is the client's decision, made through commands.
3. **Push an event when a capture ends.** As in ADR 0012: polling first, events later.

## Validation

- **Unit tests:**
  - `FrameSink`: state before and after `end`, first reason kept
  - cpal: which errors end a stream
  - command-api: JSON shape of `captures`
  - CLI: progress line with an ended capture
- **Engine test (fake backend):** a recording runs, the fake screen stream ends, and `get_health` shows `ended` with the reason while the recording keeps running. After the stop, `captures` is empty.
- **Native tests on CI:**
  - Windows: a PowerShell form with a unique title is captured, its process is killed, and the stream must report `ended` within 10 s.
  - Windows and macOS: the monitor and display stream tests check `running` while frames arrive.
- **Not tested on a real system:**
  - SCK ending through the delegate (macOS)
  - PipeWire streams ending (Linux: the portal picker needs a person)
  - cpal device removal
  - These paths are the ones the stop reports already used. What changed is that their reason is also kept for `state`.

## Consequences

- Clients can show which source is gone and why, while outputs run. Restarting a source is the next step (assessment §9, step 5).
- The `ended` state is as good as each system's notification. A capture that hangs without an error still shows as `running` with a growing frame age.
