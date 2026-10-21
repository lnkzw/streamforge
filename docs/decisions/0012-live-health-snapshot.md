# ADR 0012: Live health snapshot (`get_health`)

- **Status:** Accepted
- **Date:** 2026-10-21

## Context

The architecture assessment (`docs/architecture/CURRENT_STATE_ASSESSMENT.md`, §3.6, §8.1) found that StreamForge counts nearly everything that can go wrong, but reports it only when an output or probe stops:
- render time and skipped/late ticks; stale or missing source frames
- mixer underruns and drift
- encoder and queue drops; offline media of a stream; reconnects

While running, clients saw a few booleans (`get_status`) and mixer peaks (`get_mixer`). Nobody could tell render overload from encoder backpressure, network trouble or a stalled capture while it happened (vision §3.6, FR 6).

`get_status.media_engine_ready` was a placeholder from M0 that always read `false`, and a test pinned it.

## Decision

- **One read-only command, `get_health`,** returns:
  - `pipeline_running`
  - `render` and `mix` (`None` without a pipeline)
  - `recording` and `streaming`, each with target, elapsed time, `running`, stream state, video encoder, and counters (`frames_encoded`, `frames_dropped`, `samples_encoded`, `blocks_dropped`, `queue_depth`, `connects`, `reconnect_attempts`).

  It is `read_only`, like `get_status`; targets are paths or redacted stream targets.
- **Compositor and mixer:** every 250 ms (`LIVE_PUBLISH_MS`, `LIVE_PUBLISH_BLOCKS`) the render or mix thread refreshes a shared snapshot (`LiveRenderStats`, `LiveMixStats`).
  - It updates the snapshot in place, and only after a successful `try_lock`. A refresh that finds a reader holding the snapshot is retried on the next frame or block; the thread never waits. Tests hold the lock and count frames and blocks that continue meanwhile.
  - The compositor reuses the snapshot's allocations (`String::clone_from`). It allocates only after a scene update (more sources, or longer names); scene updates already allocate on the render thread when the source list is rebuilt.
  - The mixer's input list is created with its control, so publishing never allocates.
  - New per-source field `last_frame_age_ms`: the time since a live source last delivered a frame, or since it was added if it never did. A stalled or ended capture becomes visible while outputs run. Its explicit `ended` state with a reason is the next step (assessment §9, step 4).
- **Outputs:** the output thread publishes its totals (atomics in `Shared`) after each item. The totals cover every connection of a stream and media dropped while offline. Producer-side drops are added when the totals are read.
  - `queue_depth` is sent minus received items. It can dip below zero for a moment (a producer counts after sending), so readers clamp it to zero.
  - `Recorder::live` and `Streamer::live` read the atomics without involving the output thread.
- **`get_status`:**
  - `media_engine_ready` is now `true`: compositor, mixer and encoders are in every build. It stays for protocol compatibility.
  - The new field `pipeline_running` carries the runtime meaning.
- **Clients:**
  - The CLI's `record` and `stream` print one progress line per second from `get_health` to stderr.
  - The MCP server exposes the command from the registry.
  - The desktop UI polls it every 250 ms while a pipeline may run and shows a health strip (M10 step 3): a stopped pipeline, output counters, render and mix trouble (including audio lost to overruns), ended captures, and the reading's age when the engine is busy.

## Alternatives considered

1. **Event stream (push).** It is the better long-term model for "state changed" events. But it needs a subscription mechanism in the protocol and MCP, plus queue and lifetime rules for slow subscribers. A pull snapshot is the smaller, safer first step, and events can be added on top later.
2. **Atomics for every counter, including per-source and per-input ones.** This is lock-free throughout, but those lists change with scene edits and need names. A try-locked snapshot keeps the producer non-blocking with much less code; only the output counters (fixed and numeric) are atomics.
3. **Reuse the stop-report types** (`RenderStats`, `MixStats`) for the snapshot. They carry growing histories (`windows_ms`) and per-ring statistics, which would allocate when copied on the media threads. The live types are small subsets with the same field meanings.

## Trade-offs

- `render` and `mix` can be about 250 ms old, plus one frame or block when a refresh had to be retried. Output counters are current to the last item.
- The CLI ends a `record` or `stream` run early once every open output has stopped running; a recording keeps going when only the stream failed.
- The snapshot costs the media threads a few relaxed atomic stores per item and one in-place copy every 250 ms. The measurement harness (assessment step 7, `docs/testing.md`) cannot switch the snapshot off, so that cost is not separated. But polling `get_health` every 100 ms, which makes every refresh contend with a reader, changed neither CPU time nor render time beyond their run-to-run spread on the development machine.

## Validation

- **Unit tests:**
  - compositor: counters grow; a stalled source's age grows; frames continue while a reader holds the snapshot
  - mixer: a silent live input warms up with no fill; blocks continue while the snapshot is held
  - command-api: JSON shape of the health report
  - CLI: progress line format
- **Integration tests:**
  - Recorder live counters equal the stop report's video count and stay within one AAC frame for audio.
  - Streamer: connects, reconnect attempts and drops while failed.
  - Engine: health during a recording whose stream fails. The stream reads `failed`, and the recording's counters keep growing.
  - The engine process over stdio during a preview.
  - MCP read-only tool list.

## Revisit when

- Clients need notifications instead of polling: add an event stream.
- Profiling shows the snapshot refresh on a media thread.
- Per-source or per-output health needs history (rates, trends) beyond totals.
