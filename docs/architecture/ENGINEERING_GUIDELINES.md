# Engineering Guidelines for Coding Agents

## 1. First Rule: Understand Before Changing

Before implementing a feature or architectural refactor:

1. Inspect the repository structure, README, build configuration, and existing design documentation.
2. Identify the actual language, framework, media libraries, supported platforms, and runtime model.
3. Run the existing test suite and record the baseline.
4. Map the relevant code to the architecture in `ARCHITECTURE_VISION.md`.
5. Identify concrete gaps with file paths, call paths, tests, or reproducible behavior.
6. Propose the smallest change that addresses the gap.
7. Implement and validate that change before proceeding to the next one.

Do not assume the current codebase matches the target architecture. Do not replace a working subsystem merely because a different design looks cleaner.

## 2. Required Design Questions

For any substantial change, answer the relevant questions below.

### Ownership and lifecycle
- Who owns the resource?
- Which component creates, starts, stops, and destroys it?
- What happens on partial initialization failure?
- Is shutdown idempotent?
- Can a device or source disappear during operation?
- Can callbacks race with destruction?

### Concurrency and real-time behavior
- Which thread executes this code?
- Can it block?
- Can it allocate memory on a real-time audio path?
- Can it perform disk or network I/O on a render or audio callback?
- What are the queue limits and backpressure policy?
- How are cancellation and shutdown coordinated?

### Timing and synchronization
- Which clock and timestamp domain is used?
- How are timestamps converted?
- What happens when source clocks drift?
- How are late, stale, duplicated, or missing frames handled?
- How is A/V sync validated?

### Error handling
- Which failures are recoverable?
- Which errors reach the user?
- What context is recorded?
- Can the subsystem recover without restarting the application?
- Does failure in this component affect unrelated outputs?

### Data and compatibility
- Does this change affect project files, API contracts, plugin interfaces, or persisted settings?
- Is a migration needed?
- Are unsupported configurations reported without data loss?
- Are secrets excluded from logs and exported diagnostics?

### Performance
- What is the expected cost per frame, sample, source, or output?
- Is the cost measured or merely assumed?
- Does the change add copies, synchronization, allocations, or extra rendering?
- What happens at maximum supported workload?

## 3. Real-Time Engineering Rules

- Never perform blocking network or disk I/O on a real-time audio callback.
- Avoid unbounded queues and unbounded memory growth.
- Make ownership of frames, buffers, textures, and device handles explicit.
- Use bounded work and clear backpressure policies.
- Avoid holding broad locks across GPU operations, callbacks, or external calls.
- Ensure cancellation and shutdown cannot wait indefinitely on an untrusted extension.
- Use appropriate lock-free techniques only when justified; lock-free code is not automatically faster or safer.
- Preserve timestamp semantics through conversion, filtering, mixing, and output.
- Do not optimize without a reproducible profile or test unless fixing an obvious correctness issue.

## 4. Observability Rules

Prefer structured, correlated events over unstructured log strings alone.

Each significant source, pipeline, and output should have a stable runtime identity. Diagnostics should include, where relevant:

- Component and operation.
- Timestamp and duration.
- Failure category and native error details.
- Recovery action and result.
- Queue or buffer state.
- Related source/output identifiers.

Never log stream keys, passwords, authentication tokens, or sensitive media metadata by default.

## 5. Testing Strategy

Use multiple layers of testing.

### Unit tests
Test project validation, schema migration, routing rules, state transitions, timestamp calculations, and error mapping.

### Component tests
Test source lifecycle, device loss, output initialization, graph changes, and plugin failures with controlled dependencies.

### Integration tests
Test capture-to-output paths, multiple outputs, A/V synchronization, recording finalization, reconnection, and project reload.

### Stress and soak tests
Run long-duration sessions with repeated scene/state changes, source interruptions, audio-device removal, network interruptions, and constrained resources.

### Regression tests
Use deterministic synthetic sources where possible. Compare expected frame timing, audio continuity, output state, and diagnostic events. Use tolerances appropriate to the hardware and codec rather than assuming byte-identical encoded output.

### Platform tests
Test each supported OS and relevant backend separately. A successful build is not proof of correct capture or audio behavior.

## 6. Definition of Done

A meaningful change is complete only when applicable items below are satisfied:

- Behavior and scope are documented.
- Relevant tests pass.
- Failure cases are considered.
- Resource ownership and shutdown are correct.
- New settings and project fields are validated and migrated if needed.
- Metrics or diagnostic events exist for new failure modes.
- Performance impact is measured when relevant.
- No unrelated working behavior was removed.
- Limitations and unsupported configurations are explicit.
- The summary identifies changed files, tests run, results, and remaining risks.

## 7. Architecture Decision Records

For major decisions, create a short record containing:

- Context and problem.
- Decision.
- Alternatives considered.
- Trade-offs.
- Consequences.
- Validation plan.
- Conditions that would justify revisiting the decision.

Examples include language choice, media framework, GPU abstraction, audio engine design, plugin isolation, IPC, project schema, and UI framework.

## 8. How to Handle Conflicts with This Vision

If the existing implementation conflicts with these guidelines:

1. Describe the conflict and its concrete impact.
2. Determine whether it is a genuine defect, a deliberate trade-off, or a future aspiration.
3. Estimate the migration cost and regression risk.
4. Suggest an incremental path with compatibility where practical.
5. Ask for a decision only when the trade-off materially affects project direction or cannot be resolved from repository context.

Do not treat this document as permission to perform a broad rewrite without evidence.
