# Control Protocol (JSON lines, v1)

Local control channel for the headless engine (`streamforge-engine`). Implemented by `crates/control-protocol`; the engine process serves it on stdin/stdout. The format is versioned and intentionally small. It is a wire contract, not a serialization of internal Rust types. Contract tests in `crates/control-protocol` pin it down.

## Transport and framing

- The client starts `streamforge-engine` and talks to it over its stdin/stdout pipes.
- Each message is one UTF-8 JSON object on its own line, terminated by `\n`. The engine also accepts `\r\n`, and a final line without a terminator.
- Responses on stdout are always single-line JSON. The engine writes nothing else to stdout; diagnostics go to stderr.
- Lines containing only whitespace are ignored and get no response.
- A request line may be at most **65 536 bytes** (`MAX_REQUEST_BYTES`), not counting the `\n`. A longer line is discarded without being buffered and answered with `request_too_large`.
- The engine exits with status 0 when stdin closes. An I/O error on the pipes makes it exit with status 1 and print a message to stderr.

## Request

```json
{"v": 1, "id": "req-1", "command": "get_status", "params": {}}
```

| Field     | Required | Type                      | Meaning |
|-----------|----------|---------------------------|---------|
| `id`      | yes      | string, or integer in i64 range | Client-chosen correlation ID, echoed unchanged in the response. |
| `command` | yes      | string                    | Command name from the registry (`list_commands`). |
| `params`  | no       | object or `null`          | Command parameters. Absent, `null` and `{}` mean the same thing. |
| `v`       | no       | integer                   | Protocol version. Defaults to `1`. |

Any other top-level field is rejected with `invalid_request`, so a typo such as `parms` fails loudly. New fields require a new protocol version.

## Response

Success:

```json
{"v":1,"id":"req-1","ok":true,"result":{ ... }}
```

Failure:

```json
{"v":1,"id":"req-1","ok":false,"error":{"code":"unknown_command","message":"unknown command `foo`","details":{ ... }}}
```

- `v` is always the version of the protocol the engine speaks.
- `id` echoes the request ID. It is `null` only when the ID could not be determined: invalid JSON or UTF-8, a non-object request, a missing or malformed `id`, or an oversized line.
- Use `ok` to tell success from failure. Exactly one of `result` and `error` is present.
- `error.code` is stable and meant for matching. `error.message` is human-readable and may change. `error.details` is optional, structured context.

## Ordering

Requests are handled one at a time, and responses come back in request order. Clients may pipeline several requests, but should still correlate responses by `id`, because a later transport may answer out of order.

## Error codes

| Code                  | Layer    | Meaning |
|-----------------------|----------|---------|
| `parse_error`         | protocol | The line is not valid UTF-8 or not valid JSON. |
| `invalid_request`     | protocol | The JSON is valid but the envelope is wrong: not an object, `id` missing or of the wrong type, `command` missing or not a string, an unknown field, or `v` not an integer. |
| `request_too_large`   | protocol | The line exceeds `MAX_REQUEST_BYTES`. `details.max_bytes` gives the limit. |
| `unsupported_version` | protocol | `v` is not supported. `details.requested` and `details.supported` give the versions. |
| `unknown_command`     | command  | No such command. `details.available` lists the valid names. |
| `invalid_params`      | command  | `params` has the wrong shape or contains unexpected fields. `details.unexpected` lists unexpected names. |
| `invalid_state`       | command  | The command is not allowed in the current engine state. Reserved; no M1 command returns it. |
| `internal`            | command  | The engine failed while executing the command or encoding its result. |

A bad request never terminates the engine. After any error the engine keeps reading the next line.

## Commands (v1)

| Command         | Params | Access      | Result |
|-----------------|--------|-------------|--------|
| `get_status`    | none   | `read_only` | `{"application", "version", "media_engine_ready", "capture_available", "streaming_active", "recording_active"}` |
| `list_commands` | none   | `read_only` | `{"commands": [{"name", "summary", "access"}]}` |

`access` is the permission class: `read_only`, `control` or `sensitive`. Adapters must gate `control` and `sensitive` commands behind explicit authorization. That enforcement arrives with M8; every M1 command is `read_only`.

## Example session

```console
$ printf '%s\n' '{"id":1,"command":"get_status"}' '{"id":2,"command":"nope"}' | cargo run -q -p streamforge-engine
{"v":1,"id":1,"ok":true,"result":{"application":"StreamForge","capture_available":false,"media_engine_ready":false,"recording_active":false,"streaming_active":false,"version":"0.1.0"}}
{"v":1,"id":2,"ok":false,"error":{"code":"unknown_command","message":"unknown command `nope`","details":{"available":["get_status","list_commands"],"command":"nope"}}}
```

Key order inside `result` and `details` is not part of the contract.

## Versioning policy

- Adding a command, adding optional fields to a result, or adding an error code is backward compatible within v1. Clients must ignore unknown result fields and treat unknown error codes as generic failures.
- Removing or renaming a field, command or error code, changing a field's type, or accepting new request envelope fields requires v2.
- Update the contract tests in `crates/control-protocol` together with this document.
