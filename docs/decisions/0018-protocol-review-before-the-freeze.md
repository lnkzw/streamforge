# ADR 0018: Protocol review before the freeze

- **Status:** Accepted (reviewed with the project owner on 2026-10-26)
- **Date:** 2026-10-26

## Context

From release 0.1.0 on, the versioning policy in `docs/protocol.md` applies: removing or renaming a field, command or error code, or changing a field's type, needs protocol version 2. Until then, v1 may still change, because its only clients (desktop, CLI, MCP server) live in this repository and change with the engine.

The protocol grew over thirteen milestones. Some of it is legacy from before scenes and the audio graph existed, some names collide, and some shapes differ for the same thing. Whatever ships in 0.1.0 has to stay. So before the freeze (roadmap M13), the whole surface was reviewed: every command, parameter, result and error code, the envelope, the MCP tools (generated from the commands), the CLI flags and the project file. Two independent inventories were merged: one by Claude, one by Codex (read-only, gpt-6.1-sol).

The review only asks which **breaking** changes to make now. Additive changes (a new command, a new optional field, a new error code) stay possible after the release and are not part of it.

## Decision

All changes below are made before 0.1.0, in three steps (2a, 2b, 2c), each listed in the pre-release change list of `docs/protocol.md`.

### Step 2a: legacy, state and strictness

1. **`media_engine_ready` is removed** from `get_status`. It has been `true` in every build since M5 and says nothing about whether a device or an encoder will open.
2. **The output screen layer is removed.** `start_recording` and `start_streaming` lose `screen`, `source_id`, `source_types` and `cursor`; the reports lose `screen_capture` and `screen_capture_error`, and renders no longer have a `screen_layer` source. The layer predates scenes (M3/M5): it was a second way to compose, which depended on which output started the pipeline first and could not be restarted like a scene's screen source. Scene screen sources do the same. The CLI's `record --screen` and `stream --screen` now add a screen source to the active scene before the start. `probe_composite` keeps its optional screen layer: its fixed reference scene is a diagnostic, not a composition.
3. **Each output has one state.** `get_status` reports `recording_state` (`null`, `running` or `failed`) next to `streaming_state` (`null`, `connecting`, `connected`, `reconnecting` or `failed`). `recording_active`, `recording_failed` and `streaming_active` are removed: they were derived from these states and differed between the two outputs. In `get_health`, `recording.state` is the recording's state instead of always `null`, and the recording's entry no longer carries the stream-only counters (`connects`, `reconnect_attempts`).
4. **Destructive flags:** `set_preview` (turning it off can stop the pipeline and its captures), `set_monitor_device` (replaces the chosen device) and `set_screen_source` (replaces what a source captures) are marked destructive, by the registry's own definition.
5. **File names are checked the same on every system.** A plain file name may not contain `/`, `\`, `:` or control characters, and its part before the first dot may not be a Windows device name (`CON.x.json` too). Before, Linux accepted `a\b.mkv`, which names a directory on Windows.
6. **Envelope:** a duplicate key is refused, as in project files: `invalid_request` at the top level, `invalid_params` inside `params`; before, the last value won silently. `v` is checked before unknown fields, so a future version's request with a future field gets `unsupported_version` with the supported versions instead of `invalid_request`.
7. **Code and documentation agree:** `invalid_state` is no longer documented as reserved; the encoder and bit rate defaults are the profiles'; the reconnect delay's cap is 30 s or the initial delay, whichever is larger; probes are refused while the preview runs, not only outputs; nullability is documented per field in project files; stop reports' guarantees are stated as implemented. `probe_audio_mix`'s range error names its real maximum (900 000 ms, not 60 000), and `invalid_params` lists unknown parameters in `details.unexpected` for every command, as documented.

### Step 2b: names

9. **Capture targets get their own name.** What a screen source captures (a monitor or a window from the system) was `source_id`, next to the scene source's own `id`. It becomes `capture_target_id`; `list_screen_sources` becomes `list_capture_targets` (result `targets`), `set_screen_source` becomes `set_capture_target`, the picker filter `source_types` becomes `target_kinds` (and the capabilities' `source_kinds` too), and `cursor` becomes `cursor_mode`.
10. **One rule for references in requests:** an object is named by `<entity>_id` or `<entity>_name`: `scene_id`/`scene_name`, `source_id`/`source_name`, `channel_id`/`channel_name`, `bus_id`/`bus_name` (also for `set_output_profile`'s mix). The id is looked up first, and given both, they must agree, as today. Results name references explicitly: the scene list's `active` becomes `active_scene_name` (next to `active_scene_id`), and `get_mixer`'s `outputs[].bus` becomes `bus_id`. An object's own fields stay `id` and `name`, and the profiles keep `mix` and `tracks` (always bus ids), so the project file keeps schema version 2.
11. **Recording tracks are named explicitly:** `tracks: [{"bus_id": …} | {"bus_name": …}]`. Before, a string that looked like a bus id was taken as one, so a bus named like an id could not be chosen by name.
15. **CLI flags follow the protocol:** `--file-name`, `--target-kind`, `--capture-target-id`, `--video-bitrate-kbps`, `--stream-key-env` (as in the MCP server); `probe_composite`'s `snapshot_path` becomes `snapshot_file_name`, since it takes a file name, not a path.

### Step 2c: report shapes, then the freeze guard

12. **One shape per thing in reports:** a recording's audio is `audio_tracks: [...]`, the mix first, instead of `audio` plus `extra_audio` (a stream has one entry). The pipeline's fields are nested as `pipeline: {...}` in every stop report and in partial-error details, as the preview's report already does, and its `pipeline_stopped` becomes `stopped`. Errors embedded in reports (capture, snapshot) use the command error's shape `{code, message, details}` instead of backend kinds or plain strings.
13. **Units and words:** `sample_rate_hz` and audio `bitrate_bps` (as `_kbps`, `_ms`, `_db` elsewhere), `mute` instead of `muted`, `program` instead of `master` (also `program_gain_db` in `probe_audio_mix`), and a scene capture's source is `name`.
14. **Nominal frame rates are fractions** `{num, den}`, as in capture reports, so 59.94 needs no type change later. Measured rates stay numbers.
8. **Freeze guard:** reports serialize types from `audio-engine`, `encoder-api` and `video-compositor`, so renaming an internal field would change v1 without anyone noticing; the contract tests pin only the envelope and `get_status`. Wire-shape tests pin the field names of every result and report, last, so they hold the final shapes.

### Kept

- JSON lines, ids as strings or integers, `v` optional with default 1.
- "stream" as the output's name (profiles) next to "streaming" for the activity (commands, status, health).
- `probe_audio_mix` keeps its own inputs and tones: it tests devices, apart from the configured channels; only the names of point 13 change.

## Consequences

- Every in-repo client changes in the same step as the protocol; the MCP tools follow from the registry. Agents and scripts written against the pre-release protocol break, which is acceptable before the first release (`docs/protocol.md`).
- The project file keeps schema version 2.
- After step 2c, the soak test (M13) runs against the final protocol, and the user documentation uses the final names.
- From 0.1.0 on, each of these shapes is v1 and changes only additively.
