# ADR 0001: Rust-first engine and cross-platform UI

Status: Proposed

## Context

The project should be agent-friendly, cross-platform, and architecturally independent of OBS/libobs. It is inspired by the modular and automation-friendly workflow of FilmCraft.

## Decision

Use Rust for the engine and application core. Evaluate egui + wgpu for the initial UI/rendering direction. Keep capture, audio, encoding, and output backends behind explicit interfaces.

Use established OS APIs and codec libraries where appropriate. Owning the media pipeline does not require implementing codecs or kernel protocols from scratch.

## Consequences

- Strong fit for a Rust-first workspace and agent workflow.
- Platform integrations may require FFI or dedicated crates.
- egui is fast to prototype, but its suitability for a polished, complex desktop UI must be validated early.
- wgpu is a graphics abstraction, not a complete video compositor or capture system.
- Revisit this ADR after a minimal UI and compositor prototype.
