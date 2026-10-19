# ADR 0002: Local JSON-lines control protocol over stdio

Status: Accepted (M1)

## Context

M1 requires a headless engine process and a local protocol with request IDs. The transport must stay replaceable, and the wire format must not become an accidental copy of internal Rust types. CLI, UI, JSON-RPC and MCP adapters must share the same command handlers.

## Decision

- **Transport:** stdin/stdout of a child process. It needs no new dependencies, works the same on Linux, Windows and macOS, and grants no access beyond the parent process. Unix sockets and named pipes differ per OS, and a localhost TCP port would need authentication.
- **Framing:** one JSON object per line, with a bounded line length.
- **Envelope:** a small custom v1 envelope (`v`, `id`, `command`, `params` → `ok`, `result` / `error`) instead of JSON-RPC 2.0. JSON-RPC stays a planned adapter on top of the same handlers. The custom envelope keeps the M1 surface small and lets us pick explicit, stable error codes.
- **Layering:**
  - `command-api` owns typed commands, the registry, results, errors and the `CommandHandler` trait.
  - `engine` implements the handler.
  - `control-protocol` maps bytes to and from those types and contains no engine logic.
  - `streamforge-engine` only wires stdio to the engine.
- **Wire independence:**
  - Commands are decoded through the registry by name, not with serde enum tagging.
  - Wire error codes are a separate enum, mapped explicitly from `command_api::ErrorCode`.
  - Contract tests freeze the envelope and the code names.

## Consequences

- Only the process that spawned the engine can control it. A shared, long-running engine that several clients reach (UI plus agent) needs a socket or pipe transport. Add it as another adapter over `control_protocol::handle_line` / `serve`.
- Requests are handled one at a time on one thread. Once commands can block (device start, encoder setup), the engine needs its own controller thread and a bounded command queue. The protocol already requires clients to correlate by `id`, so this change stays compatible.
- Result payloads are serialized from `command-api` types with serde. Contract tests catch accidental shape changes, but a field rename in those types is a protocol change and must follow `docs/protocol.md` versioning.
