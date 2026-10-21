# Functional Requirements

This document defines desired behavior. Requirements should be implemented in stages according to `ROADMAP.md`; the existence of a requirement does not imply that it belongs in the first milestone.

## 1. Project and Composition Management

- Create, load, validate, save, and migrate projects.
- Represent sources and reusable components with stable IDs.
- Support composition hierarchies without making production state dependent on the visual tree.
- Provide clear reference and dependency diagnostics.
- Detect missing media, devices, plugins, and unsupported features.
- Support reusable layouts and components where practical.
- Provide explicit handling for duplicated, shared, and nested sources.

## 2. Production State and Automation

- Represent production states independently of compositions.
- Support explicit commands and events for state changes.
- Define deterministic ordering and failure behavior for automation actions.
- Make automation observable and debuggable.
- Prevent accidental trigger loops or report them clearly.
- Allow manual override and recovery from invalid states.

## 3. Audio

- Discover and configure available audio inputs and outputs.
- Provide per-channel metering, gain, mute, and monitoring status.
- Support buses, sends, multiple mixes, and independent output track mapping.
- Support common channel processing such as EQ, gate, compressor, limiter, and gain where feasible.
- Define channel layouts and sample-rate conversion behavior.
- Make routing and feedback paths inspectable.
- Detect device removal, clock drift, and underruns.
- Preserve independent recording and stream mixes.

## 4. Video and Sources

- Support camera, display/window capture, media files, images, text, browser-based content, and extensible source types as platform capabilities permit.
- Provide transforms, crop, layering, effects, and transitions.
- Expose source state, frame age, dimensions, and failure reason.
- Support explicit source lifecycle operations.
- Handle missing devices and source stalls without uncontrolled failure.
- Keep preview performance configurable independently of output quality.

## 5. Streaming and Recording

- Manage each output independently.
- Configure encoder, resolution, frame rate, bitrate, container, destination, audio mix, and tracks.
- Support streaming and local recording concurrently where hardware and resource limits allow.
- Support a high-quality master recording independently from a bandwidth-limited stream.
- Support isolated source recording and replay-buffer workflows when implemented.
- Preserve local recording when a network output fails, where the dependency graph allows.
- Report output state, errors, reconnection attempts, throughput, and dropped frames.
- Make shutdown and file finalization behavior explicit.

## 6. Performance and Diagnostics

Expose structured metrics at minimum for:

- Capture frame rate and frame age.
- Render time and missed frame deadlines.
- Queue depth and dropped/duplicated frames.
- Encoder utilization, latency, and errors.
- Audio callback timing, underruns, and buffer health.
- CPU, GPU, memory, and relevant resource usage.
- Network throughput, output backpressure, and reconnect state.
- Source, plugin, and output lifecycle events.

Provide a diagnostic snapshot that can be exported without exposing credentials or other secrets.

## 7. Recovery and Reliability

- Recover individual sources or outputs when safe.
- Bound queues and memory growth where applicable.
- Define timeouts and cancellation behavior for blocking operations.
- Ensure shutdown releases devices and resources predictably.
- Preserve useful diagnostics after failures.
- Avoid UI freezes when media operations are slow or fail.
- Clearly distinguish recoverable warnings from fatal errors.
- Do not report an operation as successful until the relevant subsystem confirms success.

## 8. UI and Usability

- Support customizable panels and saved workspace layouts.
- Provide a clear simple workflow without removing access to advanced controls.
- Show source/output health near the relevant controls.
- Offer context-sensitive diagnostics and actionable error messages.
- Make audio routing visual and discoverable.
- Provide preflight checks before starting a production.
- Support keyboard navigation and accessible control labels.
- Avoid hiding important state in unrelated settings dialogs.

## 9. API and Headless Operation

Where supported by the current architecture:

- Validate a project from the command line.
- Query runtime capabilities and status.
- Start and stop recording/streaming outputs.
- Trigger production-state transitions.
- Subscribe to structured events.
- Export diagnostics and metrics.
- Authenticate or restrict control endpoints appropriately.
- Version the API and document compatibility guarantees.

## 10. Plugin Platform

- Define a versioned SDK and explicit capability declarations.
- Document lifecycle, threading, memory ownership, error propagation, and shutdown.
- Detect missing and incompatible plugins.
- Show plugin identity, version, source, and compatibility status.
- Support safe disable/recovery paths for failing extensions.
- Evaluate sandboxing or out-of-process execution for suitable extension classes.
- Do not allow optional plugins to silently become hard dependencies of core behavior.

## 11. Cross-Platform Behavior

- Use capability discovery rather than assumptions based only on OS names.
- Report unsupported capture modes and required permissions.
- Test device changes, suspend/resume, window changes, and graphics backend failures.
- Maintain consistent user-facing concepts while acknowledging platform limitations.
- Keep platform-specific code behind testable adapters.
