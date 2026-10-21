# Architecture Vision

## 1. Product Vision

Build a reliable, extensible, cross-platform media production application for live streaming, local recording, virtual camera output, and future production workflows.

The application should retain the flexibility expected from established production software while making complex workflows easier to understand, automate, diagnose, and maintain.

The goal is not to reproduce another application's interface or internal architecture. The goal is to build a coherent production system whose media engine, project model, control plane, platform integration, and user interface have explicit responsibilities.

## 2. Problems to Solve

The following are design risks and common pain points in complex streaming applications. They are hypotheses to validate against this repository and real user workflows, not claims that every user experiences every problem.

- Configuration is distributed across scenes, sources, filters, audio settings, encoders, outputs, and global preferences.
- Users often need external tools or plugins to complete advanced routing and automation workflows.
- Audio routing may be less expressive than a dedicated digital audio workstation or broadcast mixer.
- Source, plugin, driver, capture, GPU, encoder, and network failures can be difficult to distinguish.
- A plugin or browser-based source can affect the reliability of the entire application.
- Preview rendering, compositing, capture, filtering, and encoding compete for finite CPU, GPU, memory, and bandwidth resources.
- Platform-specific capture and audio behavior can diverge across operating systems and desktop environments.
- Recording, streaming, monitoring, and other outputs may have different quality, routing, and reliability requirements.
- Project migration and plugin compatibility create significant adoption costs for an established ecosystem.

## 3. Core Architectural Principles

### 3.1 Reliability Before Feature Breadth

The system must provide predictable lifecycle management, bounded queues where appropriate, clear failure states, actionable diagnostics, and recovery mechanisms.

Do not add features at the expense of uncontrolled resource usage, silent failures, or untestable interactions between subsystems.

### 3.2 Explicit Separation of Concerns

Keep the following conceptual layers separate:

- **Media engine:** real-time audio/video processing, timing, synchronization, compositing, mixing, and output pipelines.
- **Control plane:** project state, configuration, lifecycle operations, commands, events, automation, and status reporting.
- **Frontend:** editing, preview, monitoring, and user interaction.
- **Platform adapters:** operating-system capture, audio devices, window/display integration, graphics APIs, and platform-specific capabilities.
- **Extension layer:** plugins and integrations with documented APIs and explicit compatibility rules.
- **Output layer:** streaming, recording, virtual camera, network outputs, and other destinations.

These are logical boundaries, not a mandate to create a distributed system. Prefer a modular process initially unless process isolation provides a concrete reliability, security, or operational benefit.

### 3.3 Headless and Automatable Core

The media engine and production state must not depend on the UI being open.

Where practical, support a headless mode and a stable local control API or CLI for:

- Loading and validating a project.
- Inspecting capabilities and runtime status.
- Starting and stopping outputs.
- Triggering production-state changes.
- Reading metrics and diagnostic events.
- Running automated integration tests.

The UI should be a client of the control plane, not the sole owner of production logic.

### 3.4 Explicit Data Models

Treat the production as a versioned, machine-readable model rather than a loose collection of UI preferences.

Use stable identifiers, explicit references, schema validation, migrations, and a clear distinction between persistent configuration and transient runtime state.

### 3.5 One Time Model for All Media

Define and document timestamp, clock, buffering, and synchronization semantics across capture, audio mixing, video rendering, recording, and streaming.

Do not rely on implicit assumptions about frame rate, sample rate, device clocks, or wall-clock time.

### 3.6 Observability Is a Product Feature

Expose enough structured telemetry to determine where a problem originates.

Users and developers should be able to distinguish capture failures, source failures, render overload, encoding overload, audio underruns, output backpressure, network degradation, and plugin faults.

Prefer actionable diagnostics over opaque logs.

### 3.7 Cross-Platform by Contract

Platform-specific behavior must be isolated behind explicit interfaces that report capabilities, limitations, and errors.

Do not pretend that every platform supports identical capture or audio features. Expose differences clearly and test each supported backend.

### 3.8 Extensibility Without Uncontrolled Failure

Provide a stable extension SDK with versioned interfaces, documented lifecycles, ownership rules, threading constraints, and resource-management expectations.

Evaluate process isolation for untrusted or crash-prone extensions. Do not assume all plugins should run out of process: real-time media paths may require low-overhead native execution.

### 3.9 Incremental Evolution

Inspect the existing implementation before proposing large changes. Prefer small, reversible, measurable improvements. Do not rewrite functioning subsystems merely to make the code conform stylistically to this document.

## 4. Production Model: Separate Three Concepts

### 4.1 Composition

Describes what is rendered:

- Sources and reusable components.
- Layouts, transforms, masks, and visual effects.
- Layering and ordering.
- Transitions and composition-specific behavior.

### 4.2 Production State

Describes what the production is doing:

- Intro, live, intermission, outro, recording-only, and other states.
- Rules and triggers for changing state.
- Automation actions and event-driven behavior.
- State-specific visibility, routing, and output policies.

Production state should not be hard-coded into the visual scene tree.

### 4.3 Output Profile

Describes where and how content is delivered:

- Video resolution, frame rate, pixel format, and encoder settings.
- Audio mix, channel layout, sample rate, and track mapping.
- Destination, protocol, bitrate, container, and output-specific options.
- Recovery policy and output health.

A production may use shared components while sending different compositions, mixes, or quality profiles to different outputs.

## 5. Target Logical Architecture

### Frontend / Production Studio
Editing UI, preview, mixer, monitoring, layout management, accessibility, and user interaction.

### Control Plane
Project schema, validation, lifecycle commands, production state, automation, events, status, and API/CLI access.

### Real-Time Media Engine
- Video graph: capture frames, transforms, compositing, effects, and render scheduling.
- Audio graph: capture, channel processing, buses, sends, mixing, and output routing.
- Media clock: timestamps, clock reconciliation, buffering, and A/V synchronization.
- Resource manager: GPU/CPU resources, queues, memory ownership, and lifecycle management.
- Output scheduler: independent output pipelines and backpressure policies.
- Health monitor: metrics, errors, recovery, and diagnostic snapshots.

### Platform Adapters
Native capture and audio backends, device discovery, window/display integration, graphics API integration, and platform capability reporting.

### Extension Layer
Plugin SDK, compatibility metadata, permissions/capabilities, lifecycle contracts, and optional process isolation.

### Output Layer
Streaming, local recording, source recording, replay buffer, virtual camera, and future output types.

This diagram describes logical responsibilities. It does not require each box to be a separate process or library.

## 6. Audio Architecture

Model audio as a directed signal graph.

- **Inputs:** microphones, application audio, system audio, files, network streams, and virtual devices.
- **Channels:** gain, mute, pan, metering, and channel-level processing.
- **Buses:** groups such as Game, Voice, Music, Alerts, Program, and Monitor.
- **Sends:** explicit connections between channels, buses, and outputs.
- **Outputs:** stream mix, recording mix, isolated tracks, headphone monitor, and virtual devices.

Requirements:

- Support multiple independent mixes and track assignments.
- Define pre-fader/post-fader send semantics explicitly.
- Make routing visible and inspectable.
- Keep monitoring routes distinct from program outputs.
- Document latency and clock-domain behavior.
- Detect missing devices and recover according to a defined policy.
- Avoid accidental feedback loops or make them explicitly detectable.
- Provide predictable persistence and migration for routing graphs.

Do not confuse separate recording tracks with a full bus-based routing model.

## 7. Video and Rendering Architecture

Use a graph-based representation for capture, transforms, compositing, effects, and output preparation.

- Make data ownership and synchronization explicit.
- Avoid unnecessary CPU/GPU transfers and frame copies where feasible.
- Reuse compatible intermediate results when correctness permits.
- Allow preview quality and preview cadence to be reduced independently from output quality.
- Track frame age, queue depth, dropped frames, and render time by stage.
- Make the cost of sources and effects observable.
- Define behavior for stale frames, source stalls, and device loss.
- Avoid blocking the real-time path on UI work, disk I/O, or network operations.

Any optimization must preserve correctness and be backed by profiling or reproducible tests.

## 8. Independent Outputs

Treat every output as a managed pipeline with its own configuration, state, metrics, and failure policy.

Examples:

- Live stream.
- High-quality master recording.
- Isolated source recordings.
- Multitrack audio recording.
- Replay buffer.
- Virtual camera.
- Future network or remote-production outputs.

A failure in one output should not stop unrelated outputs unless a shared dependency makes this unavoidable. Shared resources and failure coupling must be documented.

## 9. Project Format and State Management

Use a versioned project schema with:

- Stable IDs and explicit references.
- Schema validation and useful validation errors.
- Migration functions between schema versions.
- Clear separation of project configuration, global preferences, secrets, and runtime state.
- Safe/atomic saves where supported.
- Recovery from interrupted writes.
- Deterministic serialization where practical.
- Import diagnostics for unsupported or partially migrated features.

Never store credentials or stream keys in plaintext project files by default. Use the operating system's secure storage facilities where available, and document platform limitations.

## 10. Compatibility and Migration

Compatibility is a product concern, not an afterthought.

If migrating from established tools is in scope:

- Prioritize scenes, media sources, transforms, common filters, audio settings, and standard output profiles.
- Produce a migration report listing imported, converted, unsupported, and failed items.
- Preserve original files and never silently discard unsupported configuration.
- Treat third-party plugin compatibility as a separate, explicitly scoped problem.
- Avoid promising full plugin compatibility unless it is demonstrated and tested.

## 11. Security and Trust Boundaries

Treat third-party plugins, browser content, scripts, network inputs, and project files as potential trust boundaries.

- Validate imported configuration.
- Apply resource limits where practical.
- Avoid executing project content implicitly.
- Define permissions for extensions and external integrations.
- Protect stream credentials and local control APIs.
- Bind control endpoints to loopback by default unless remote access is explicitly configured and secured.
- Document what can execute code inside the main process.
