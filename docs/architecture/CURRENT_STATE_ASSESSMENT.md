# Current State Assessment

Assessment of the StreamForge repository against `ARCHITECTURE_VISION.md`, `FUNCTIONAL_REQUIREMENTS.md`, `ENGINEERING_GUIDELINES.md` and `ROADMAP.md` in this directory. This is Phase 0 of `ROADMAP.md`. No production code or project configuration was changed for it.

- **Date:** 2026-10-21
- **Commit:** `851abfa` (`main`, after M9c, PR #12).
- **Progress since:** steps 0–7 of the plan (§9) are implemented (ADR 0012–0015, roadmap M10). The findings below describe the state at `851abfa`.
- **Status labels:**
  - **Satisfied:** the principle holds and evidence backs it.
  - **Partial:** some of it holds; the gap is named.
  - **Violated:** current behaviour contradicts the principle.
  - **Not started:** no implementation yet. This is often a deliberate scope decision, not a defect.

## 1. Summary

StreamForge has a well-separated, headless media core with bounded real-time paths, explicit time bases, independent outputs and capability-reporting platform adapters. It runs on three OSes: native screen capture, audio capture, recording, streaming and hardware encoding, all verified in CI except where the runners lack hardware.

Measured against the vision, the largest gaps are not in the media core. They sit around it:

1. **Live observability.** Detailed statistics exist, but only in the report produced when an output stops. While running, a client sees a handful of booleans and mixer peaks.
2. **Persistent, versioned production data.** Scenes and audio setup live in memory and are lost on restart; there is no project file, schema or migration.
3. **Runtime state of sources.** A capture that ends or stalls while an output runs (a window closed, a device removed) becomes visible only in the final report, and it cannot be recovered without stopping the pipeline.

The production model (production states, automation), the bus/send audio graph, per-output compositions and mixes, and the extension layer are **not started**. These are aspirational requirements; AGENTS.md lists the extension layer as a deliberate non-goal for the early milestones. None of them requires reworking the existing core: each can be added on the current boundaries.

There is no case for a rewrite. Four structural decisions should be preserved; the evidence for each is below:

- the shared pipeline with per-output resources (ADR 0006)
- the CPU reference compositor (ADR 0003)
- FFmpeg through `ffmpeg-next` (ADR 0005)
- one typed command surface for every client (ADR 0002, ADR 0008)

## 2. Baseline

| Check | Result |
|-------|--------|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean (Linux host; Windows/macOS targets of the capture crates also cross-checked during M9) |
| `cargo test --workspace --no-fail-fast` | **282 passed, 0 failed, 4 ignored**, in 35 test binaries (Linux, AMD RX 6800 with VAAPI). The 4 ignored tests are the `capture-linux/tests/live.rs` session tests, which need a desktop session and, for screen capture, a human in the picker. |
| CI (`.github/workflows/ci.yml`) | Every PR and every push to `main` runs fmt, clippy, build and tests on `ubuntu-latest`, `windows-latest` and `macos-15-intel`. The last complete run (39590252337, M9c) was green on all three. |
| Toolchain | Rust 1.99.1 stable, edition 2024 |

Not covered by the baseline:
- performance benchmarks (none exist; the ADRs contain one-off manual measurements)
- soak or long-duration tests (none exist)
- live microphone capture on Windows and macOS, and hardware encoders other than VAAPI (no hardware on the CI runners)

## 3. Architecture map

### 3.1 Workspace

19 crates, about 26 600 lines of Rust (including tests). The dependency graph is acyclic, as AGENTS.md requires, and was taken from `cargo metadata`.

```
media-core ◄── capture-api ◄── capture-{linux,cpal,windows,macos} ◄── capture-platform
    ▲   ▲           ▲
    │   └── video-compositor, audio-engine
    │                ▲
encoder-api ◄── encoder-ffmpeg        command-api (← audio-engine, video-compositor, capture-api, encoder-api)
                     ▲                      ▲
                  engine  ───────────────────┘  (← capture-api, media-core, video-compositor, audio-engine, encoder-ffmpeg)
                     ▲
   streamforge-engine (stdio) · streamforge-cli · streamforge-desktop · streamforge-mcp
```

### 3.2 Logical components (vision §5) mapped to the code

| Vision component | Implementation | Evidence |
|------------------|----------------|----------|
| Frontend / Production Studio | `streamforge-desktop` (egui). A client of the engine thread; widget code holds no business logic. | `crates/streamforge-desktop/src/worker.rs` (submit/poll, never blocks), `model.rs` (actions become commands), ADR 0007 |
| Control plane: commands, status, API/CLI | `command-api` (typed `Command`, registry, access classes, parameter schemas), `control-protocol` (JSON lines v1), `engine` (`CommandHandler`), CLI, MCP | `crates/command-api/src/registry.rs`, `crates/control-protocol/src/lib.rs:24`, `crates/engine/src/lib.rs:120`, ADR 0002, ADR 0008 |
| Control plane: project schema, validation, production state, automation, events | **Not present.** Commands are validated; there is no project file, no production-state model and no event stream. | `crates/command-api/src/scenes.rs:71` (`SceneSpec` is `Serialize` only) |
| Media engine: video graph | `video-compositor`: a flat layer stack per scene (`Layer {source, transform, fit, opacity, visible}`), a fixed-rate render loop, `CpuRenderer`. Not a general node graph. | `crates/video-compositor/src/scene.rs`, `runner.rs`, ADR 0003 |
| Media engine: audio graph | `audio-engine`: one mixer with strips (gain, mute), master gain and limiter, tones and live inputs with drift compensation. No buses or sends. | `crates/audio-engine/src/mixer.rs:94`, `live.rs`, `runner.rs`, ADR 0004 |
| Media engine: media clock | `MediaTimestamp` (rational time base, checked `rescale`), producers stamping nominal monotonic times, per-output epochs | `crates/media-core/src/lib.rs:20,49`, `crates/engine/src/pipeline.rs` (`since`), `docs/media-model.md` |
| Media engine: resource manager | Bounded pools, mailboxes and rings; explicit drop order; one pipeline owned by `Engine` | `crates/media-core/src/video.rs` (`FramePool`, `FrameMailbox`), `crates/media-core/src/audio.rs` (`AudioRing`), `crates/engine/src/pipeline.rs:65` ("Field order is drop order") |
| Media engine: output scheduler | `Fanout` in the pipeline: one copy pool (8 frames), one bounded queue (256) and one epoch per output, plus a preview mailbox | `crates/engine/src/pipeline.rs:35,526,620,675`, `crates/encoder-ffmpeg/src/lib.rs:38,106` |
| Media engine: health monitor | **Partial.** Rich statistics in the stop reports; while running only `get_status` (booleans, stream state) and `get_mixer` (peak levels) | `crates/command-api/src/lib.rs:149`, `crates/video-compositor/src/runner.rs:198` (stats only at `stop`), `crates/audio-engine/src/runner.rs:183` |
| Platform adapters | `CaptureBackend` trait with capability reporting; one backend per OS selected in one place; screen and audio combined through `SplitCapture` | `crates/capture-api/src/backend.rs:44,161`, `crates/capture-platform/src/lib.rs:10`, `docs/platforms.md` |
| Extension layer | **Not started** (AGENTS.md non-goal for the early milestones) | AGENTS.md "Non-goals" |
| Output layer | `encoder-ffmpeg`: `Recorder` (Matroska) and `Streamer` (FLV/RTMP with reconnects); hardware encoders with a software fallback | `crates/encoder-ffmpeg/src/lib.rs`, `src/ffmpeg/`, ADR 0005, 0006, 0011 |

### 3.3 Threads and ownership (as built)

| Thread | Owner | Work | Blocking behaviour |
|--------|-------|------|--------------------|
| Engine thread | `EngineThread` (or the stdio loop) | Executes commands one at a time, owns `Engine` | Blocks for the duration of a command: probes up to 60 s plus the picker, `start_streaming` until connected (5 s I/O timeout; name resolution is unbounded, ADR 0006) |
| Compositor | `Pipeline` → `Compositor` | Fixed-rate render, per-output frame copies, preview downscale | Try-locks scene updates; locks the fan-out list once per frame. Skipped ticks are counted. |
| Mixer | `Pipeline` → `MixerThread` | 10 ms blocks; live inputs pulled from `AudioRing` | Documented as allocation-free after start-up (`runner.rs` docs); not enforced by a test |
| Capture threads/callbacks | Each `VideoStream`/`AudioStream` | PipeWire loops, cpal real-time callback (`try_push`, never waits), WGC/SCK callbacks into `FrameSink` | Bounded waits on system services (`capture-linux/src/lib.rs:32`, `capture-macos` 5 s) |
| Recorder / streamer | `RecordingOutput` / `StreamingOutput` | Conversion, encoding, muxing, network | Own threads; queues are bounded; stop grace 2 s (`encoder-ffmpeg/src/lib.rs:41`) |

## 4. Principles: assessment

| # | Principle | Status | Evidence | Gap |
|---|-----------|--------|----------|-----|
| 3.1 | Reliability before feature breadth | **Satisfied (core) / Partial (runtime diagnostics)** | Bounded queues and pools everywhere on the media path: `pipeline.rs:35`, `encoder-ffmpeg/src/lib.rs:38`, `AudioRing` (capacity fixed at construction). Drops are counted: `OutputInput::count_dropped`, `skipped_ticks`, `stale_ticks`. Explicit stream failure states (`StreamState`). Bounded stop: `stop_is_bounded_while_a_reconnect_hangs` (`encoder-ffmpeg/tests/stream.rs:316`). | Failure states of sources are not exposed while running (§6.6) |
| 3.2 | Separation of concerns | **Satisfied, with one coupling** | Layers map to crates (§3.2). `media-core` depends on nothing internal. UI, CLI and MCP depend on `command-api` and `engine`, not on media internals. | `command-api` depends on `audio-engine` and `video-compositor` for report types (`RenderStats`, `MixStats`) and limits (`MAX_INPUTS`, gain range; `command-api/src/registry.rs:16`). A command vocabulary that links the renderer is not wrong, but the boundary is softer than `docs/architecture.md` says ("UI and transport adapters depend on the command API, not on media internals"). |
| 3.3 | Headless, automatable core | **Satisfied (control) / Partial (inspection)** | Headless engine process (`streamforge-engine/src/main.rs`), CLI, MCP server; the desktop is one client among them (ADR 0007). The acceptance tests drive the engine process: `streamforge-engine/tests/*.rs`. | No "load/validate project"; no event subscription; metrics only at stop (FR 9 "subscribe to structured events", "export diagnostics") |
| 3.4 | Explicit data models | **Partial / largely not started** | Stable source ids (`SourceSpec.id`, never reused: `engine/src/scenes.rs` "ids are never reused"). Commands validate their parameters with structured errors and JSON schemas (`params_schema`). | No persisted model at all: `SceneSpec` has no `Deserialize` and nothing writes it; no schema version, migration, atomic save or distinction between configuration and runtime state. Scenes are referenced by **name**, not id (`set_active_scene {name}`). |
| 3.5 | One time model | **Satisfied (synthetic) / Partial (live capture)** | `MediaTimestamp` with rational time bases and checked rescaling (`media-core/src/lib.rs:49`, test `rescales_with_rounding_and_overflow_checks`). Nominal producer times, per-output epochs, a common end on stop (`DRAIN_TIMEOUT`, `pipeline.rs:50`), drift compensation (ADR 0004). Documented in `docs/media-model.md`. A/V drift is checked on decoded files (`streamforge-engine/tests/recording.rs`). | Capture latency is not compensated (`docs/media-model.md` "Capture latency: not compensated yet"). Screen frames are stamped at composite time; capture PTS (WGC/SCK/PipeWire) are recorded in statistics but not used. No test measures real capture-to-output A/V offset. |
| 3.6 | Observability as a product feature | **Partial** | Per-stage statistics: render time histogram, skipped/late frames, per-source stale/missing ticks (`runner.rs:94`), mixer underruns, drift and clipping, encoder drops, offline drops and undelivered media for streams, capture statistics and warnings. Every error carries a structured `ErrorCode` and details. | Statistics surface **only when an output or probe stops** (`Compositor::stop`, `MixerThread::stop`, `Recorder::stop`). No live per-output counters, no frame age, no stage queue depths, no CPU/GPU/memory usage, no event log, no exportable diagnostic snapshot (FR 6). The desktop shows status booleans, mixer peaks and a log of command results (`streamforge-desktop/src/app.rs:14`). |
| 3.7 | Cross-platform by contract | **Satisfied** | The `CaptureBackend` trait reports capabilities with reasons (`capture-api/src/types.rs`, `picker`/`source_list`, `unavailable_reason`). One selection point (`capture-platform/src/lib.rs:10`). Per-OS native tests in CI (`capture-windows/tests/native.rs`, `capture-macos/tests/native.rs`, `capture-cpal/tests/native.rs`). `docs/platforms.md` holds the capability matrix and the verification table. | Encoder availability is discovered only when an output starts (ADR 0011); `get_capture_capabilities` does not cover outputs. Device hot-plug, suspend/resume and graphics-backend failures are untested (FR 11). |
| 3.8 | Extensibility without uncontrolled failure | **Not started (deliberate)** | AGENTS.md: "Do not add a plugin system … before the first end-to-end recording works." | That precondition now holds; whether an extension layer is in scope is an open product decision (§10). |
| 3.9 | Incremental evolution | **Satisfied** | 11 ADRs and milestone PRs M0–M9 with self-review and CI; each ADR names its consequences and limits. | — |

### 4.1 Production model (vision §4)

| Concept | Status | Evidence |
|---------|--------|----------|
| Composition | **Partial** | Scenes with layered sources (color bars, solid, screen), rectangle, opacity, z-order, aspect-preserving fit (`command-api/src/scenes.rs`, `video-compositor/src/scene.rs`). No transitions, crop, masks, effects, nested or shared components. |
| Production state | **Not started** | The only state is the active scene (`SceneList.active`). No production states, triggers or automation. |
| Output profile | **Partial** | Per output: encoder preference and rate control (ADR 0011), stream bit rate and reconnect policy. Fixed for all outputs: canvas 1920×1080@60 (`engine/src/composite.rs:27`), one program mix, one composition. At most one recording and one stream (`pipeline.rs:37`). |

### 4.2 Audio (vision §6)

**Partial.** The mixer is solid for one program mix:
- per-strip gain and mute with click-free ramps
- master gain, hard limiting with counted clipping
- meters
- drift-compensating live inputs with counted warm-up, underrun and trim (`audio-engine/src/live.rs`)

Not started:
- buses, sends, pan, pre/post-fader semantics
- multiple mixes and per-output track mapping, monitoring routes
- channel processing (EQ, gate, compressor)
- feedback detection, device-loss recovery policy

A removed device shows up as underruns while running and as a capture error in the final report; nothing reconnects it.

### 4.3 Video (vision §7)

**Partial.**

In place:
- Preview quality and cadence are independent of the outputs (quarter size, 30 FPS; `pipeline.rs` `PREVIEW_FACTOR`/`PREVIEW_EVERY`).
- Stale frames reuse the last frame and are counted (`stale_ticks`); missing sources are counted (`missing_ticks`).
- No UI, disk or network work runs on the render thread.

Costs paid on the CPU:
- The compositor copies each frame into each output's pool (8 MB per frame at 1080p; `VideoTap`, `pipeline.rs:620`).
- Each output converts BGRA to YUV itself (`encoder-ffmpeg/src/ffmpeg/mod.rs`, one swscale per output).
- Capture copies GPU → staging → pool on Windows and macOS (ADR 0010).

These copies were a measured, documented trade-off (ADR 0006: 0 late frames with two outputs on the development machine). No profile exists for weaker machines; the 3-core macOS CI runner skips about a third of the ticks while x264 encodes on it (ADR 0009).

Not started: render cost per source or effect, and frame age.

### 4.4 Independent outputs (vision §8)

**Satisfied** for failure isolation between outputs. Each output has its own pool, queue, epoch, encoder and thread, and the claim is tested: `recording_continues_when_the_stream_fails` (`engine/src/lib.rs:752`, `streamforge-engine/tests/streaming.rs`).

The shared dependency, the pipeline, is documented: a compositor or mixer failure fails every output. Recovery then requires stopping all outputs (`engine/src/outputs.rs:106`).

**Partial** for configuration independence: every output gets the same composition, canvas and mix (§4.1).

### 4.5 Project format, security, compatibility (vision §9–11)

| Topic | Status | Evidence |
|-------|--------|----------|
| Project format | **Not started** | No persistence (§4, 3.4) |
| Secrets | **Satisfied** | `StreamKey` redacts itself (`encoder-api/src/stream.rs:18`, ADR 0006). The CLI reads the key from an environment variable, never from argv. The MCP server takes the key from `--stream-key-env` (`streamforge-mcp/src/main.rs:34`), and agents cannot change URL or key (`agents_stream_only_to_the_configured_target_and_never_see_the_key`). FFmpeg logging is turned off once streaming is used. OS secure storage is not used; it becomes relevant when a project file exists. |
| Control endpoints | **Satisfied** | stdio only; there is no network listener. Access classes with an explicit `--allow` level (`command-api/src/registry.rs:32,440`, ADR 0008). Deleting a scene, the one irreversible command, needs `confirm: true`. Files are confined to `--output-dir`. |
| Untrusted inputs | **Satisfied (current scope)** | Request size limit (`control-protocol/src/lib.rs:24`), `deny_unknown_fields` on every parameter struct, source-id validation (ADR 0010) |
| Compatibility / migration | **Not started (no scope yet)** | — |

## 5. Functional requirements: coverage

| FR section | Coverage | Notes |
|------------|----------|-------|
| 1 Project and composition | Low | Scenes and sources exist; no load, save, validation or migration |
| 2 Production state and automation | None | — |
| 3 Audio | Medium | Discovery, metering, gain, mute, drift handling; no buses or sends, multiple mixes, DSP, or device-removal recovery |
| 4 Video and sources | Medium | Display/window capture on 3 OSes; color and solid sources; no camera, media file, image, text or browser sources. Source state shows only in reports. |
| 5 Streaming and recording | High | Concurrent outputs, independent failure, reconnects, explicit finalization (`sigterm_finalizes_an_open_recording`, `streamforge-mcp/tests/mcp.rs:372`), hardware encoders. Not covered: master recording at a different resolution than the stream, source recording, replay buffer, live throughput metrics. |
| 6 Performance and diagnostics | Low–medium | Good stop-time statistics; no live metrics, resource usage or diagnostic snapshot |
| 7 Recovery and reliability | Medium | Stream recovery; bounded queues and stops; UI never blocks. Missing: source recovery, a timeout or cancel for long engine commands, a distinction between warning and fatal in live status. |
| 8 UI | Low–medium | Scenes, sources, preview, mixer, outputs, log. No health near controls, preflight, workspace layouts or routing editor. |
| 9 API / headless | Medium–high | Versioned protocol (v1), capability and status queries, start/stop, access control. Missing: project validation, event subscription, metrics export. |
| 10 Plugin platform | None (non-goal) | — |
| 11 Cross-platform | High | Capability discovery, permission reporting, native per-OS tests. Device change and suspend/resume tests are missing. |

## 6. Risks by area

Each risk names its evidence and a classification: **D** = immediate defect, **L** = long-term improvement, **A** = aspirational requirement.

### 6.1 Real-time processing

1. **(L) The compositor thread pays O(outputs × frame size) per tick.** Each output gets a full-frame copy on the render thread (`pipeline.rs:620`), and each encoder converts colour on its own (`ffmpeg/mod.rs`). Measured fine on the development machine (ADR 0006); unmeasured on weak hardware or at higher resolutions. Mitigation needs a profile first (Engineering Guidelines §3), e.g. a frame shared by reference count between outputs, or conversion before the fan-out.
2. **(L) The engine thread is single-file.** A long command blocks every later one, including `stop_recording` (`engine/src/thread.rs:23`, a queue of 32). The worst cases are bounded or refused:
   - probes are refused while outputs are open (`outputs.rs:68`)
   - picker sources are refused while outputs run (`pipeline.rs:191`)
   - `start_streaming` blocks up to the 5 s I/O timeout (`outputs.rs:29`) plus unbounded name resolution (ADR 0006)

   MCP cancellation does not abort a running command (ADR 0008).
3. **(L) Real-time safety is documented, not enforced.** `AudioRing::try_push` keeps the cpal callback lock-free (`media-core/src/audio.rs:96`), and the mixer claims "does not allocate after start-up". No test guards these claims, e.g. a counting allocator in a test build.

### 6.2 Audio/video synchronization

1. **(L) Live capture latency is not compensated** (`docs/media-model.md`). Live audio is stamped when mixed (about one jitter-buffer target after capture); screen frames are stamped when composited. The systematic offset is within common lip-sync tolerance (ADR 0005) but is not measured with real devices.
2. **(D, minor) Test tolerances are now runner-specific.** On the slow macOS runner, several A/V checks allow counted skips and drops (`streamforge-engine/tests/recording.rs`, `streaming.rs`; `engine/src/lib.rs` recording test). They are one-sided and accounted, so no sync error goes unnoticed. But no test asserts that sync holds under load: overload is tolerated, not characterized.

### 6.3 Resource ownership

**Satisfied.**
- Frames have one owner and return to their pool when dropped (`media-core/src/video.rs`).
- Drop order is deliberate and commented: `Engine` drops `pipeline` first so producers stop before outputs finalize (`engine/src/lib.rs`), `Session` drops its output before the interrupt state, and the VAAPI and pixel-buffer FFI modules document their invariants.
- Tests: `shutdown_cancels_queued_commands_and_outlives_handles` (`engine/src/thread.rs:235`); the VAAPI wrapper tests in `encoder-ffmpeg/src/ffmpeg/vaapi.rs`.

### 6.4 Lifecycle management

- **(Satisfied)** Start returns only after the subsystem confirms: the file header is written; the stream is connected. Stops are explicit and bounded. Engine drop, stdin close and SIGTERM (MCP) finalize open recordings.
- **(D) `get_status.media_engine_ready` is always `false`.** It is a leftover placeholder from M0 (`engine/src/lib.rs:70`, comment "No media graph or output exists yet (M3+)"), and a test pins the wrong value (`engine/src/lib.rs:490`). The CLI prints it as "Media engine ready: false", and `docs/protocol.md` shows it in the example session. Clients are told the engine is not ready while it records.
- **(D, minor) Stale component documentation**, found in:
  - `crates/rtmp-test-server/src/lib.rs:2` ("Linux only")
  - `crates/capture-cpal/src/lib.rs:5` and `backend.rs:54` ("a later M9 step"; for `CpalCapture` alone this is still literally true, since screen capture now comes from `capture-windows`/`capture-macos` through `SplitCapture`)
  - `crates/capture-api/src/backend.rs:100` ("planned for M9")
  - `crates/command-api/src/lib.rs:1` and `crates/engine/src/lib.rs:3` ("future MCP/UI adapters")
  - `docs/architecture.md:12,15` ("future JSON-RPC and MCP adapters"; a generic "media graph: nodes, connections" that does not exist as such)

### 6.5 Platform abstraction

1. **(Satisfied)** Capture and encoders sit behind traits and `cfg` gates with one selection point each (`capture-platform/src/lib.rs:10`, `encoder-ffmpeg/src/ffmpeg/encoders.rs:35`).
2. **(L) `CaptureBackend` combines screen and audio in one trait.** Each half returns `unsupported` from backends that only do the other, and `SplitCapture` stitches them together. This works, but adding a source type (camera, application audio) grows every backend. *Planned 2026-10-27:* split into three traits when cameras arrive (ADR 0019, M14 step 4a).
3. **(L) No hardware-dependent CI evidence.** NVENC, Quick Sync, AMF and VideoToolbox are untested on real hardware, as are window capture on Windows/macOS beyond the error paths and live microphones on Windows/macOS (`docs/platforms.md` verification table).
4. **(L) macOS builds depend on fragile tooling.** Xcode 26 and an rpath in `.cargo/config.toml` are required, and a custom `RUSTFLAGS` overrides the rpath (ADR 0010).

### 6.6 Error recovery

1. **(D, against FR 4 and FR 7) Sources that end or stall are invisible while running.**
   - A closed window (`capture-windows/src/wgc.rs:335`, `capture-macos/src/sck.rs:335`), an ended PipeWire stream, or a removed audio device is recorded in the capture's report.
   - That report reaches the client only when the output stops (`PipelineReport.scene_captures`).
   - Meanwhile the compositor keeps the last frame, and the mixer pads silence.
   - ADR 0010 documents this as a known limitation.
2. **(L) Sources cannot be recovered while running.** Nothing restarts a source. Removing and re-adding it starts a new capture, but only where that does not need the picker during an output (`pipeline.rs:191`).
3. **(Satisfied)** Streams reconnect with backoff and a stability window (`encoder-ffmpeg/src/ffmpeg/stream.rs:28`). Hardware encoders fall back to software, including when the first frame is rejected (`ffmpeg/mod.rs:285`). The recording survives a failing stream.
4. **(L) Pipeline failure takes down all outputs** (`outputs.rs:106`). This is acceptable as a documented shared dependency. A restart path that keeps outputs open is a long-term option, not a defect.

## 7. Classification of findings

### 7.1 Immediate defects

Small, low-risk fixes:

| ID | Finding | Evidence | Fix size |
|----|---------|----------|----------|
| D1 | `media_engine_ready` is always false | `engine/src/lib.rs:70,490`, `docs/protocol.md` example | Small: give it a real meaning (pipeline runnable, or running) or remove it with a protocol note |
| D2 | Sources that end or stall are not reported while running | §6.6.1 | Medium: see improvement 2 |
| D3 | Stale statements in module docs and `docs/architecture.md` | §6.4 | Small, documentation only |

### 7.2 Long-term architectural improvements

Justified by evidence, not urgent:

- Live health snapshot and event stream (§6, 3.6).
- Persisted, versioned project model with validation and migration (3.4).
- Profiling-driven reduction of per-output copies and conversions (§6.1.1).
- A cancellable or asynchronous command path for long operations (§6.1.2).
- Allocation and real-time guards in tests (§6.1.3).
- Capture latency compensation and a real-device A/V offset measurement (§6.2.1).
- Encoder capability discovery before start, for preflight checks (3.7).
- Splitting `CaptureBackend` into screen and audio traits once a third source type arrives (§6.5.2).
- Soak and stress tests, device-removal tests (Engineering Guidelines §5).

### 7.3 Aspirational requirements

Not started, and not implied by defects:

- Production states and automation.
- Audio buses, sends, multiple mixes, DSP and monitoring.
- Per-output compositions, canvases and mixes; master recording at a different quality; source recording; replay buffer; virtual camera.
- Camera, media-file, image, text and browser sources; transitions and effects.
- Plugin SDK and process isolation.
- Import and migration from other tools.
- OS secure storage for credentials, which becomes relevant with a project file.

## 8. The three highest-priority improvements

Selection criteria: value measurable with a test or a number, additive to the existing contracts, low regression risk, and a prerequisite for later phases (`ROADMAP.md` Phase 1–2).

### 8.1 Live runtime health snapshot

**Problem.** Statistics exist but surface only at stop (§3.6). Users, the desktop UI and agents cannot tell render overload from encoder backpressure, network trouble or a dead source while it happens (vision §3.6, FR 6).

**Proposal.**
- A read-only `get_health` command, plus `get_status` fixed for D1, returning per-component live counters:
  - **render:** frames, skipped and late ticks, render-time p95 so far, per-source stale/missing ticks and seconds since the last new frame
  - **mixer:** per-input fill, underruns and drift
  - **outputs:** frames encoded, dropped and offline so far; queue depth; encoder name; stream state; connects; bytes sent if available
- Cumulative counters let clients compute rates.
- Implementation: the counters already exist inside the threads. Most become atomics, or cheap snapshots behind a try-lock, mirroring `MixerControl::levels` (`audio-engine/src/runner.rs:183`). No real-time path waits on a reader.

**Value.**
- Diagnosis while running.
- The basis for UI health indicators (FR 8) and preflight.
- Lets tests assert on behaviour during a run instead of only on the final report.

**Risk.** Low:
- additive, read-only
- the existing reports stay as they are
- the producer cost is a few relaxed atomic stores per frame or block

### 8.2 Source liveness and recovery

**Problem.** D2: a capture that ends, errors or stalls is invisible until stop, and cannot be recovered without stopping (§6.6).

**Proposal.**
- `VideoStream`/`AudioStream` gain a non-blocking state query: `Running`, `Stalled { since }`, or `Ended { reason }`.
  - The backends already know the state: WGC `on_closed`, the SCK delegate, PipeWire stream state, cpal error callback.
  - `FrameSink::end` already records it for Windows and macOS.
- The health snapshot (8.1) shows it per source; the desktop flags it next to the source.
- A `restart_source {scene, name}` command re-opens a named source without stopping outputs. Picker sources stay refused while outputs run, as today.

**Value.** Meets FR 4 "expose source state … failure reason" and FR 7 "recover individual sources when safe", with a test per backend.

**Risk.** Low to medium. The trait addition gets a default implementation, so other backends and test fakes keep compiling. Restart reuses the existing scene-apply path (`Pipeline::apply_scene`).

### 8.3 Versioned project file for scenes and audio setup

**Problem.** Every scene and audio setting is lost when the engine exits (3.4, FR 1, vision §9). That is the first thing a real user hits.

**Proposal.**
- `save_project {file_name}`, `load_project {file_name}` and `validate_project {file_name}` (also `streamforge project validate FILE` in the CLI).
- Files are JSON with `schema_version: 1` and contain scenes (stable source ids, explicit references), the active scene and the audio setup.
- Files live in the engine's output or project directory and are written atomically (temporary file, then rename).
- Stream keys and screen-source ids are never saved: ids are machine-specific and sensitive (ADR 0010), so screen sources store their kind and cursor and are re-chosen on load.
- Validation errors name the JSON path.
- Load is allowed only while the pipeline is idle.

**Value.**
- Projects survive restarts.
- Gives the data model of Phase 3 a versioned home.
- Testable round-trips.

**Risk.** Low. It is additive and touches no media path.

**Decision needed (§10).** The project location, and whether the name-based scene references in commands should move to ids at the same time.

## 9. Incremental plan

Each step is one PR. Each keeps the existing behaviour and its tests, adds tests for the new behaviour, and updates `docs/protocol.md` plus an ADR where the decision is substantial.

| Step | Content | Depends on | Acceptance criteria | Tests |
|------|---------|------------|---------------------|-------|
| **0** | D1 and D3: give `media_engine_ready` a meaning (true while a pipeline runs; also add `pipeline_running`) or deprecate it; fix stale docs | — | Status reflects a running pipeline; no "future MCP/UI" or "planned for M9" text remains where those exist | Update `engine/src/lib.rs:490`; stdio contract test for the field |
| **1** | Live counters in compositor, mixer and outputs (atomics or try-lock snapshots), exposed from `Compositor`, `MixerThread`, `Recorder`, `Streamer` | — | A reader never blocks a producer; the counters match the final report's numbers within one frame or block | Unit tests per crate. Counter equals report for a synthetic recording. A test reads counters while the compositor runs and checks that render timing is unaffected (no skipped ticks added at 30 FPS) |
| **2** | `get_health` command (read-only access) in `command-api` and `engine`; the CLI prints it; MCP picks it up from the registry | 1 | During a 2 s recording: `frames_encoded` grows and matches the stop report within 1 frame. With a failing ingest: stream state `reconnecting`, then `failed`, while the recording's counters keep growing | Engine unit test; extend `streamforge-engine/tests/streaming.rs` (`recording_continues_when_the_stream_fails`) with health assertions; MCP tool-list test |
| **3** | Desktop: health strip (outputs, render, per-source state) | 2 | UI shows dropped frames, stream state and stale sources within 1 s, from health only (no new engine logic) | Model test in `streamforge-desktop/tests/acceptance.rs` |
| **4** | Stream state query on `VideoStream`/`AudioStream` (default: `Running`); implemented in WGC, SCK, PipeWire and cpal; surfaced in `get_health` | 1, 2 | A closed window or a stopped fake stream shows `ended` with its reason within 1 s while an output runs | Fake-backend tests in `engine`; Windows native test closes a window it opened (CI); macOS: stop a stream through the delegate path where possible |
| **5** | `restart_source` command | 4 | An ended named source restarts without stopping the recording; the recording's file stays continuous (no gap beyond the restart time, counted) | Engine test with a fake backend whose stream ends and restarts; acceptance check of the recording timeline |
| **6** | Project file v1: `save_project`, `load_project`, `validate_project`, atomic write, schema version, validation with JSON paths; no secrets or screen ids saved | — (parallel to 1–5) | Round-trip save/load is lossless for scenes and audio setup. Invalid files give actionable errors. A crash during save leaves the previous file intact. A load while the pipeline runs is `invalid_state`. | Unit tests: round-trip, every validation error, temp-file/rename behaviour, unknown future `schema_version` refused. CLI `project validate` test. |
| **7** | Measurement harness: a `perf` example or ignored test recording N seconds with 1 and 2 outputs and reporting render p95, skipped ticks, encoder drops and CPU time | 1 | Numbers are reproducible within ±10 % on the same machine; recorded in `docs/testing.md` as a baseline for later copy and conversion work | Run manually and in CI as an informational step, without thresholds (ROADMAP "avoid arbitrary performance promises") |

Steps 0, 1 and 6 can start in parallel. Step 7 should precede any work on frame copies or conversions (§6.1.1).

## 10. Decisions to preserve, and open questions

### 10.1 Preserve

Each item names its evidence and the condition that would justify revisiting it.

- **Shared pipeline with per-output resources** (ADR 0006): failure isolation is tested; the coupling to the shared pipeline is documented. Revisit only with per-output compositions (Phase 3).
- **CPU reference compositor** (ADR 0003): deterministic, pixel-exact tests; measured fast enough at 1080p60 on the development machine. Revisit with a profile showing render cost dominates, or with effects that need a GPU.
- **FFmpeg via `ffmpeg-next`** (ADR 0005), **hardware encoders with fallback** (ADR 0011): verified on three OSes, plus VAAPI on hardware.
- **One typed command surface** for CLI, UI, protocol and MCP (ADR 0002, 0008): every new capability above (health, restart, project) should be a command first.
- **stdio-only control and access classes** (ADR 0008): satisfies vision §11 without network exposure.

### 10.2 Open questions for the project owner

1. Is an **extension layer** in scope now that the first end-to-end recording works (AGENTS.md non-goal)? The answer changes whether `CaptureBackend` and the source model should become a public SDK.
2. **Project file location and identity:** a user-chosen path, or a projects directory next to `--output-dir`? Should commands move from scene names to scene ids when projects exist? *Decided 2026-10-22:* a project directory set at start; opaque prefixed string ids, with commands taking an id or a name (ADR 0014).
3. **Output profiles:** is a master recording at a different resolution or bit rate than the stream a near-term goal? It decides whether per-output canvases (and the copy and conversion work in §6.1.1) move from aspirational to planned.
4. **Audio routing depth:** are buses and sends (vision §6) in scope, or is "one program mix plus per-output track selection" enough for the next phase? *Decided 2026-10-23:* buses, sends, per-output mixes, multitrack recording and a monitor output come before the first release (roadmap M11), so the protocol is frozen after the audio model; effects follow later in prepared slots.
