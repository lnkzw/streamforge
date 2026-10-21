# Suggested Engineering Roadmap

This roadmap is a prioritization proposal, not a fixed schedule. Adapt it to the current repository and the project's actual maturity.

## Phase 0 — Repository Assessment

### Goals
Understand the current implementation before changing architecture.

### Work
- Inventory modules, dependencies, entry points, and platform support.
- Map current project/configuration models.
- Map capture, processing, synchronization, rendering, audio, and output paths.
- Identify thread ownership and resource lifecycle.
- Run tests and establish performance baselines.
- Document the top five architectural risks with evidence.

### Exit criteria
- Architecture map exists.
- Current test/build baseline is known.
- Critical unknowns and platform assumptions are documented.
- A prioritized, incremental plan is agreed.

## Phase 1 — Core Contracts and Observability

### Goals
Make the system understandable and testable.

### Work
- Define source, output, project, capability, and runtime-state contracts.
- Add structured errors and lifecycle events.
- Add basic health metrics.
- Define cancellation, shutdown, and resource ownership.
- Add schema validation and safe project loading.

### Exit criteria
- Invalid configurations fail with actionable errors.
- Source/output lifecycle is testable without depending on the UI.
- Diagnostics distinguish common failure categories.
- Existing behavior remains covered by regression tests.

## Phase 2 — Reliable Media Paths

### Goals
Establish correct capture-to-output behavior.

### Work
- Validate the media clock and timestamp model.
- Test audio continuity and A/V synchronization.
- Bound queues and define backpressure policies.
- Add source/output recovery where safe.
- Measure render, encode, and audio callback costs.
- Add recording finalization and interrupted-output tests.

### Exit criteria
- Repeatable synthetic-source integration tests pass.
- Device/output interruption behavior is documented and tested.
- Performance bottlenecks can be localized with available metrics.
- No known unbounded queue or indefinite shutdown path remains in the tested critical path.

## Phase 3 — Production and Routing Model

### Goals
Support more expressive workflows without UI-specific hacks.

### Work
- Separate composition, production state, and output profiles.
- Add explicit audio buses/sends if they are in project scope.
- Model independent stream and recording outputs.
- Define deterministic automation transitions.
- Add project schema migrations as the model evolves.

### Exit criteria
- Production state can change without relying on ad hoc UI mutations.
- Routing and output assignments are inspectable and testable.
- A failed output does not unnecessarily stop independent outputs.
- Project migration reports unsupported features clearly.

## Phase 4 — UI and Workflow Quality

### Goals
Make the underlying capabilities understandable.

### Work
- Improve source/output health indicators.
- Add preflight validation.
- Provide a visual audio routing editor.
- Add workspace layouts and context-sensitive diagnostics.
- Expose preview-quality controls independently of output quality.
- Improve accessibility and keyboard operation.

### Exit criteria
- Common workflows can be completed without editing configuration files manually.
- Error messages identify likely cause and a useful next action.
- UI responsiveness is tested under slow-device and output-failure conditions.

## Phase 5 — Extension and Automation Ecosystem

### Goals
Make integrations stable and diagnosable.

### Work
- Publish a versioned extension SDK.
- Add compatibility metadata and dependency reporting.
- Define trust boundaries and permissions.
- Evaluate process isolation for suitable plugin categories.
- Stabilize CLI/API contracts and document authentication.
- Build compatibility/import tools if migration is in scope.

### Exit criteria
- An external integration can be built against documented contracts.
- Incompatible plugins fail clearly rather than silently.
- Extension failure and shutdown behavior are tested.
- API compatibility policy is documented.

## Release Gates

Before a public release, define measurable targets for:

- Maximum supported source/output workload on reference hardware.
- Audio underruns and video deadline misses under controlled load.
- A/V synchronization tolerance.
- Recovery behavior for device and network failures.
- Long-duration stability.
- CPU, GPU, memory, and latency budgets.
- Project migration coverage.
- Supported platform/backend matrix.

Set numeric thresholds only after defining reference hardware, workloads, measurement methods, and acceptable quality. Avoid arbitrary performance promises.
