# Instructions for Coding Agents

## Mission

Build StreamForge: a cross-platform streaming application with its own media graph and processing pipeline. Use FilmCraft as an architectural inspiration for agent-friendly workflows, not as a source of code to copy blindly.

## Non-goals for the initial milestones

- Do not implement every OBS feature.
- Do not add a plugin system, scripting runtime, distributed services, or complex scene editor before the first end-to-end recording works.
- Do not use libobs as the engine.
- Do not claim cross-platform support based only on compilation on one OS.

## Required workflow

1. Read `docs/architecture.md`, `docs/media-model.md`, `docs/roadmap.md`, and `docs/testing.md`.
2. Before a substantial change, state the intended design and acceptance criteria.
3. Implement the smallest vertical slice that satisfies one milestone.
4. Add unit tests for pure logic and integration tests for runtime behavior where possible.
5. Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`.
6. Report commands run, results, limitations, and the next recommended step. Never claim a test passed if it was not run.

## Engineering rules

- Rust stable; use the edition specified in the workspace manifest.
- Prefer small crates with one clear responsibility.
- Keep the dependency graph acyclic. `media-core` must not depend on UI, CLI, platform capture, or encoding crates.
- Keep blocking work and media processing off the UI thread.
- No unbounded queues in real-time paths.
- Do not hold locks while waiting on I/O, device callbacks, or encoder output.
- Avoid allocation in real-time audio callbacks. Document any unavoidable allocation.
- Use explicit time bases and checked conversions for timestamps.
- Do not silently drop media. Any frame/sample drop policy must be explicit and observable through metrics.
- Convert errors at subsystem boundaries; do not discard error details.
- Platform-specific code must be isolated behind traits and `cfg` gates.
- Add a dependency only when there is a concrete use case; explain why in the PR/change summary.
- Avoid unsafe Rust. If required for a platform API, isolate it in a small module, document invariants, and test safe wrappers.
- Never commit secrets, stream keys, access tokens, or local machine paths.

## Agent-control interface

All user-facing operations should eventually be represented by typed commands in `command-api`. CLI, desktop UI, JSON-RPC, and MCP adapters must call the same command handlers rather than implementing business logic independently.

Commands that start/stop recording or streaming must return structured outcomes and explicit errors. Destructive actions and credential access must have explicit safeguards.

## Completion checklist

- [ ] Code formatted
- [ ] Clippy clean with warnings denied
- [ ] Tests pass
- [ ] Architecture docs updated if behavior or boundaries changed
- [ ] Cross-platform impact considered
- [ ] Known limitations and test results reported
