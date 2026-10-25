# ADR 0013: Capture stream state and source restart

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
  - `VideoStream::state` and `AudioStream::state` return it. They never block, and they have no default: a new backend or a wrapper stream that forgot them would report every capture as running. A backend that cannot tell returns `Running` explicitly.
  - `CaptureEnd` keeps the first reason, set by whichever thread notices the end, and is read without a lock. `FrameSink` (WGC, SCK) and the PipeWire streams use it.
  - "Stalled" is not a capture state: some systems deliver frames only when the content changes. Clients judge stalls from `last_frame_age_ms` and the source kind.
- **Backends:**

  | Backend | Ends when | Reason |
  |---------|-----------|--------|
  | WGC (Windows) | the item is closed (`on_closed`), or the capture thread ended | "the captured window was closed" or "the captured display went away", by item kind; for the thread, a pointer to the capture report, which gets its error at the stop |
  | SCK (macOS) | the delegate reports `did_stop_with_error`, or a frame reports `Stopped` | the system's error, or "ScreenCaptureKit stopped the capture" |
  | PipeWire (Linux) | the capture thread returns, or panics, before a stop: a stream error, or the stream went unconnected after streaming | the error, "the PipeWire stream ended", or the panic |
  | cpal (Windows, macOS audio) | an error other than a glitch was reported and no data followed for 500 ms | the last error's message |

  - **cpal: what follows an error decides, not its kind.** Hosts differ: in cpal 0.18, WASAPI's capture loop ends on any error, but keeps capturing after `StreamInvalidated` (a new default device), and other hosts pause on some errors. A stream that reported an error and delivered no data since, for 500 ms (many device periods), has ended. If data comes back, it reads `running` again.
    - Glitches are the exception: cpal's `Xrun` (an underrun or overrun) and `RealtimeDenied` never end a stream, on every host, so they do not count. macOS reports a processor overload as `Xrun`, which on a loaded machine can come before the first pull; counted, it failed a playback's start once the first pull took longer than 500 ms (a CI flake on macOS).
    - The data callback stores a timestamp (no lock, no allocation).
    - The error callback, off the data path, stores the message in a reused buffer.
    - Errors stay warnings in the stop report, as before.
  - **PipeWire, named audio inputs:** the stream sets `node.dont-reconnect`. Without it, the session manager moves a stream whose input went away to another source: on a machine with a default source, a different microphone would be recorded under the input's name, and on the development machine (no default) the stream went silent and stayed `running`. With it, WirePlumber ends the stream with "target not found". The default input (no name) still follows the default source.
- **`get_health`** gains `captures`: one entry per running capture.
  - The entries cover the scenes' screen sources, the legacy screen layer and live audio inputs.
  - Each entry has `kind` (`screen` or `audio`), `id` (the scene source's id, as in `list_scenes` and `render.sources`; scene sources only), `name` (for audio, the mixer's input name), `state`, and `reason` when the capture ended.
  - Ended captures stay in the list until they are stopped. Their outputs keep running, and the compositor keeps showing the source's last frame, as before.
- **CLI:** progress lines name ended captures with their reason.

### Restarting a source (`restart_source`)

- **Command:** `restart_source {scene, name, source_id?}`. It is `sensitive` like `add_source`, because it starts a screen capture, and destructive (ADR 0008's rule): it ends the running capture, and a picker choice is lost. It returns the scene list.
- **Scope:** screen sources with a running capture, while the pipeline runs.
  - Those are the active scene's sources and those kept for other scenes (captures keep running while their source is in any scene).
  - Other kinds are `invalid_params`. A source without a capture, no pipeline, or a pipeline that stopped (where a new capture would never be rendered) is `invalid_state`.
  - Audio inputs and the legacy screen layer cannot be restarted: the mixer's inputs are fixed while it runs. They need a new pipeline.
- **`source_id`:** restart on another source from `list_screen_sources`, kept by the scene. This is what makes restarting useful for windows:
  - Window ids name a handle (Windows: process and handle; macOS: window number), so a closed window's id never comes back.
  - A display's id does come back when it reconnects, so the same id works then.
  - On Linux every screen source is a picker source (no named sources yet), so a restart there asks the user and works only while no output is open.
- **How:**
  - The old capture leaves the list of running captures.
  - A source of the rendered scene goes through the path of a scene change: it starts the capture and gives the compositor the new mailbox. The compositor treats it as a new source and retires the old one's statistics.
  - A kept source of another scene starts alone; nothing renders it until its scene is active.
  - On success, the old capture is stopped, and its outcome joins `scene_captures`.
  - On error, the old capture goes back into the list unchanged and the scene keeps its old `source_id`. The new capture starts before the old one ends, so a failed start leaves the old one (possibly ended) in place. A new capture that started before a later error is stopped and reported.
- **Pickers:** a picker source would ask the user again. While a recording or stream is open this is refused (`invalid_state`), as for adding one. A named source restarts at any time.
- **Outputs keep running.** The compositor renders every tick whatever its sources do. Until the new capture's first frame, the source is not drawn, and the compositor counts `missing_ticks`. Output drops, if any, are counted as always. Whether a restart costs output frames has not been measured: the measurement harness (assessment step 7) runs without a capture backend.

## Alternatives considered

1. **A `stalled` state, decided by the backend.** A timeout per source kind would misreport static windows and idle displays as failures. The frame age already carries the information, without a guess.
2. **End the outputs, or remove the source, when a capture ends.** A recording that keeps the last frame is more useful than a recording that stops because a window closed. What to do next (restart, remove, switch scenes) is the client's decision, made through commands.
3. **Push an event when a capture ends.** As in ADR 0012: polling first, events later.
4. **For cpal, a list of error kinds that end a stream.** The first version did this, and the review of the cpal 0.18 WASAPI source showed it wrong both ways: errors outside the list stop the capture loop, and `StreamInvalidated` does not. Watching the data callbacks works the same on every host.
5. **Restart automatically when a capture ends.** A closed window usually stays closed, and a picker source would ask the user at an unexpected moment. Restarting is the client's decision.
6. **Restart in place, keeping the compositor's source state.** It would keep showing the last frame until the new one, but the compositor matches live sources by their mailbox. A second way to swap inputs would add a path that scene changes do not test.

## Validation

- **Unit tests:**
  - `FrameSink`: state before and after `end`, first reason kept
  - cpal: the end rule (no error, data after the error, within and after the grace period, never any data), and the state naming the last error and returning to `running` when data comes back; on every OS
  - command-api: JSON shape of `captures`
  - CLI: progress line with an ended capture
- **Engine tests (fake backend):**
  - A recording runs, the fake screen stream ends, and `get_health` shows `ended` with the reason while the recording keeps running. After the stop, `captures` is empty.
  - `restart_source` on the ended named source starts a new capture during the recording. Health shows it `running`, frames keep being encoded, the compositor consumes the new capture's frame, and the stop report has both captures. The test also covers the refusals: no pipeline, a source without a capture, a non-screen source, an unknown source.
  - Restarting on a gone `source_id` fails with `not_found` and changes nothing (old id, old capture); on another id, the scene keeps the new one.
  - A kept capture of a scene that is not rendered restarts.
  - A picker source is refused while recording, and its running capture stays. With only the preview left, it restarts.
- **Native tests on CI:**
  - Windows: a PowerShell form with a unique title is captured, its process is killed, and the stream must report `ended` within 10 s.
  - Windows and macOS: the monitor and display stream tests check `running` while frames arrive.
- **Development machine (Linux, PipeWire 1.x, WirePlumber):** a recording with a named input from a virtual source (`support.null-audio-sink`), whose node was destroyed after 3 s. With `node.dont-reconnect`, the CLI showed the input `ended: ... target not found` from then on, and the recording finished with all 420 frames. Without it, the stream stopped delivering and stayed `running`.
- **Not tested on a real system:**
  - SCK ending through the delegate (macOS)
  - PipeWire screen streams ending (the portal picker needs a person)
  - cpal device removal (no microphone on the CI runners)

## Consequences

- Clients can show which source is gone and why while outputs run, and bring a screen source back without stopping them.
- The `ended` state is as good as each system's notification. A capture that hangs without an error still shows as `running` with a growing frame age.
