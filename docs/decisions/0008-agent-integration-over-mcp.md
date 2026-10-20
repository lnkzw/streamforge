# ADR 0008: Agent integration over MCP with user-chosen access levels

Status: Accepted (M8)

## Context

M8 asks for:
- safe read-only status and diagnostics first
- MCP tools over the same command API
- explicit authorization for sensitive actions and credentials
- no stream keys in logs or default tool responses

Acceptance: an agent can inspect status and create a test scene, and destructive and credential-related operations are permission-gated.

Each command already carries an access class (`read_only`, `control`, `sensitive`); until now nothing enforced it.

## Decision

### MCP server

- **Server:** `streamforge-mcp` is a stdio MCP server built on the official Rust SDK, `rmcp` (pinned to 3.5.1, since 3.x minors still change APIs). Writing our own JSON-RPC layer would avoid tokio, but would also have to track protocol versions and negotiation; the SDK does that.
- **Engine threading:** the engine is not `Send` (capture backends), so it runs on an `EngineThread`, which the desktop UI also uses.
  - Commands queue in a bounded channel. A full queue is `unavailable` ("busy") at once, so at most the queue's worth of tool calls wait on blocking threads.
  - Shutting down (the client closes stdin, or SIGINT/SIGTERM) cancels queued commands (`cancelled`). The running command finishes, then the engine is dropped, which finalizes open recordings and streams (tested with SIGTERM).
  - A panic inside the engine is reported with its message, and later calls are refused.
- **Tools are commands:** every tool is a registry command, called on the same path as every adapter: `Command::decode`, then the access check, then `CommandHandler::handle`.
- **Schemas:** parameter schemas are generated with `schemars` from the decoder's own parameter structs (`params_schema`), with subschemas inlined and descriptions from doc comments, so they cannot drift from validation.
- **Annotations:** taken from the registry, so every command carries them and no adapter keeps name lists. Each `CommandDescriptor` declares `destructive` (overwrites, removes or ends something: scene and source edits, gains, audio sources, stopping outputs) and `open_world` (only `start_streaming`); `list_commands` reports both.
- **Results and errors:**
  - Results are structured content (the command output as JSON).
  - Engine and policy errors are tool results with `isError` and `{"code", "message", "details"}`; codes are mapped explicitly.
  - Unknown tools are protocol errors.

### Access policy

- **`AccessPolicy` (command-api):** allows commands up to a level. The person who starts the adapter chooses it: `streamforge-mcp --allow read-only|control|sensitive`, default **read-only**.
- **Tool listing:** tools above the level are not listed, and calling one anyway is `permission_denied`.
- **Per-call level (`Command::required_access`):**
  - `add_source` needs `control` for color bars and solid colors, and `sensitive` for screen sources. So with `--allow control` an agent can create a test scene, while capture stays gated.
  - The harmless kinds are listed explicitly, so a future capturing kind defaults to `sensitive`.
  - `CommandDescriptor::least_access` gives the lowest level any call can need, which decides whether a tool is listed.
  - Calls are decoded before the policy check, so a denial names the level that particular call needs.
  - Everything else uses its descriptor's class: `delete_scene` (irreversible), capture, recording, streaming, preview and audio-source changes are `sensitive`.
- **JSON-lines server:** `Gated<H>` applies the same policy to any `CommandHandler`. `streamforge-engine --allow` uses it; the default stays `sensitive`, because whoever controls its stdin is a local, trusted caller.
- **Approval prompts:** clients' own per-call approval (e.g. a tool-permission prompt) comes on top; the server does not rely on it.
- **`remove_source` stays `control`:** it is a scene edit that adding the source back undoes, like `update_source`. It is marked destructive so clients ask before running it. `delete_scene` loses a whole scene, so it is `sensitive` and needs `confirm`.

### Credentials

- **No URLs or keys from agents:** agents never handle stream keys or choose where a key goes. `start_streaming` is offered only when the server was started with `--allow sensitive --stream-url URL [--stream-key-env VAR]`.
  - Its tool schema has neither `url` nor `stream_key`, and passing either is `permission_denied`.
  - `command_api::decode_streaming_to(target, params)` decodes the agent's other parameters (bit rate, reconnects, sources) for a fixed target, so no adapter re-implements that.
  - Accepting a URL from the agent would let it send the configured key to a server of its choosing.
- **Startup checks:** the key comes from an environment variable named on the command line, never from argv.
  - An unset or empty variable is refused, so the server does not stream without a key.
  - The target is validated at startup.
  - Errors contain neither the key nor the variable's name, in case the key was passed where the name belongs.
- **Redaction:** responses show the target redacted (`…/<stream key>`). stdout carries only the protocol; diagnostics go to stderr and never include parameters. FFmpeg's own logs are disabled once streaming is used (ADR 0006).

## Testing

- **Unit tests:**
  - every command has an object schema without `$ref`
  - enum values and required fields in the schemas
  - per-call access of `add_source`
  - policies and `Gated`
  - `EngineThread` from several threads, and a stopped engine
- **Acceptance (`crates/streamforge-mcp/tests/mcp.rs`):** the official rmcp client drives the real binary over stdio:
  - read-only by default: status works; `create_scene` is not listed and is denied when called
  - with `--allow control`: an agent creates a test scene with a color-bars source; a screen source and `delete_scene` are denied; invalid parameters come back as `invalid_params`
  - with `--allow sensitive` but no target: no `start_streaming`, and an attempt with a URL is denied
  - startup refuses a target without `--allow sensitive` or with an unset key, without echoing the key
  - with a configured target (Linux): the redirect attempt is denied; streaming to an in-process RTMP ingest works; and the key appears nowhere in tool lists, results or server info
- **JSON-lines:** `--allow read-only` denies `create_scene` with `required: control`.
- **Shutdown:**
  - SIGTERM during a recording, through raw JSON-RPC against the binary: the server exits and the file is finalized.
  - Engine thread: queued commands are cancelled at shutdown, and a handle that outlives the thread does not block it.

## Consequences

- An agent's reach is set by the person who starts the server, not by the agent. Read-only is the safe default.
- Long commands (probes up to 15 min, `start_streaming` connecting, screen pickers) occupy the engine thread; other tool calls wait, or get "busy" once the queue is full. MCP cancellation does not abort a running engine command, and shutdown waits for it.
- Error codes are mapped explicitly in each adapter (as `control-protocol` does), even though they currently match `ErrorCode::as_str`, so internal renames cannot change a wire format.
- `set_preview` over MCP only controls the pipeline; no frames are sent to agents.
- One streaming target per server; several targets or per-call user confirmation (MCP elicitation) are future work.
