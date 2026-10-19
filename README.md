# StreamForge

An experimental, cross-platform, agent-friendly streaming application. The project aims to build its own real-time media graph and application workflow rather than wrapping OBS/libobs.

## Current status

This repository is an **architecture and workflow scaffold**, not a working streaming engine yet. M0 (workspace) and M1 (command boundary) are in place: typed commands, a headless engine process, and a local JSON-lines control protocol. Capture, audio, compositing, encoding, and streaming are not implemented.

## Prerequisites

- Rust stable toolchain (`rustup`, `cargo`, `rustfmt`, `clippy`)
- Git
- Platform-specific multimedia development dependencies will be documented when their implementation is introduced.

Install Rust using the official instructions: <https://www.rust-lang.org/tools/install>

## Start locally

```bash
cargo build --workspace
cargo test --workspace
cargo run -p streamforge-cli -- --help
cargo run -p streamforge-cli -- status

# Headless engine: one JSON request per line on stdin, one response per line on stdout
printf '%s\n' '{"id":1,"command":"get_status"}' | cargo run -q -p streamforge-engine
```

The protocol is specified in `docs/protocol.md`.

The first version deliberately has no capture or streaming functionality. Do not expect it to replace OBS yet.

## Repository layout

- `crates/media-core`: platform-independent media types and time model
- `crates/command-api`: typed commands, registry, structured results/errors, `CommandHandler` trait
- `crates/engine`: engine controller implementing the command handlers
- `crates/control-protocol`: JSON-lines control protocol v1 (transport-agnostic)
- `crates/streamforge-engine`: headless engine process serving the protocol on stdio
- `crates/streamforge-cli`: CLI entry point (calls the same command handlers)
- `.github/workflows/ci.yml`: CI (fmt, clippy, tests on Linux/Windows/macOS)
- `docs/`: architecture, media model, control protocol, roadmap, testing strategy, decisions (ADRs)
- `AGENTS.md`: instructions for coding agents

## Development rules

Read `AGENTS.md` before making changes. Keep platform-specific APIs behind traits in `media-core` or dedicated backend crates. Do not add a dependency or implement a subsystem before documenting why it is needed.

## Design principles

1. Cross-platform APIs first; platform-specific backends behind explicit interfaces.
2. No OBS/libobs dependency in the media engine.
3. Audio and video have explicit timestamps and bounded queues.
4. The engine must be controllable headlessly through the same command API used by the UI and MCP server.
5. Every milestone must have an executable acceptance test.
