# Testing Strategy

## Every change

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Test layers

- **Unit tests**: timestamp conversion, validation, scene transforms, audio math, queue policies.
- **Contract tests**: serialization and versioning of command/request/response types.
- **Integration tests**: headless command process, capture lifecycle, recording output.
- **Media tests**: inspect generated files with `ffprobe`; validate stream metadata, duration, timestamps, and A/V drift.
- **Performance tests**: FPS, frame latency, queue depth, dropped frames, audio underruns, CPU/GPU utilization.
- **Platform tests**: native capture tests on Linux/Wayland, Windows, and macOS.

## Continuous integration

`.github/workflows/ci.yml` runs on every push to `main` and on every pull request:

- `rustfmt` check on Linux.
- On `ubuntu-latest`, `windows-latest` and `macos-latest` (no fail-fast):
  - `clippy -D warnings`
  - build and tests with `--locked`
  - the M0 acceptance command (`streamforge-cli status --json`)
  - a stdio smoke test of `streamforge-engine`

The workflow uses the current Rust stable from `rustup` and logs the exact toolchain version. Its only third-party code is `actions/checkout`, pinned to a commit SHA. Jobs have timeouts and read-only repository permissions.

A newer stable toolchain can bring new clippy lints. Before pushing, you can run `cargo +<newer> clippy --workspace --all-targets -- -D warnings` locally.

## Where tests live

- `crates/command-api`: unit tests for registry consistency, parameter decoding and result serialization.
- `crates/control-protocol/src/*`: unit tests for bounded line reading and envelope encoding.
- `crates/control-protocol/tests/contract.rs`: wire-contract tests against a fake `CommandHandler`. These cover every error code, ID echoing, ordering and oversized lines.
- `crates/engine`: unit tests showing that every registered command is handled.
- `crates/streamforge-engine/tests/stdio_protocol.rs`: process integration tests. They spawn the real binary and talk JSON lines over pipes, with a 10 s timeout on every wait, and kill the child on failure.

## Reliability requirements

- Tests must have timeouts for process and device operations.
- Tests must clean up temporary media files.
- Device-dependent tests should be marked and report a clear skip reason if no device/session is available.
- Never use only screenshots as proof of media correctness.
- Record benchmark hardware, OS, driver, build mode, resolution, frame rate, and encoder settings.
