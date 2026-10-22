# ADR 0015: Project file

- **Status:** Accepted
- **Date:** 2026-10-22

## Context

Scenes and the audio setup lived only in memory and were lost when the engine exited (assessment §8.3, step 6). That is the first thing a real user hits. The ids of ADR 0014 make references in a file possible.

The user decided where files live: a project directory set when the engine starts, and commands that name only a file in it. Agents call the same commands over MCP, so a command that took any path could read or write anywhere on the machine.

## Decision

- **Location:** `--project-dir DIR` for `streamforge-engine` and `streamforge-mcp` (`Engine::with_project_dir`).
  - Commands take a plain `.json` file name, checked like recording names: no directories, no `..`, no absolute paths.
  - Without the flag, the project commands are `invalid_state`. There is no default directory, so an engine never writes project files unless whoever starts it chose a place.
- **Commands** (`command-api`):

  | Command | Access | What it does |
  |---|---|---|
  | `save_project {file_name, overwrite}` | `sensitive`, destructive | Writes the scenes, the active scene and the audio setup; also while outputs run |
  | `load_project {file_name}` | `sensitive`, destructive | Replaces all three, only while the pipeline is idle; all or nothing |
  | `validate_project {file_name}` | `read_only` | The same checks, without loading; problems are a result |
  | `set_screen_source {source, source_id}` | `sensitive` | Chooses what a screen source captures while it has no running capture |

  - Saving is `sensitive` because it can replace a file, loading because it brings in audio inputs and screen sources (as `set_audio_sources` and `add_source` do) and discards unsaved scenes.
  - `set_screen_source` exists because a loaded screen source has no target, and Windows and macOS have no picker yet. `update_source` is `control`, and choosing what is captured is `sensitive`, so it is a command of its own.
- **Format, schema version 1** (crate `project-file`): `schema_version`, `canvas`, `active_scene`, `scenes` and `audio`, pretty-printed JSON.
  - A source is its `list_scenes` object without `source_id`, and the `add_source` decoder checks it. The audio is `set_audio_sources` parameters, checked by that decoder. The file therefore has no rules of its own for colors, rectangles, kinds, gains or tones; a change to a command's rules applies to files too.
  - The crate checks what no single command sees: id form and uniqueness (source ids across scenes), unique names, limits, `active_scene`, and the canvas.
  - Every problem has a JSON path. Structure errors (a wrong type, an unknown field, a missing field) get theirs from `serde_path_to_error`, a new dependency for that alone; the checks above collect every problem, not only the first.
  - Strict: unknown fields are problems, and only `schema_version` 1 is read. A newer file says to update StreamForge. A migration goes where the version is read, when version 2 exists.
- **Not stored:** stream keys, file paths, and what a screen source captures. That is machine-specific, and window titles can be private (ADR 0010); a hand-written `source_id` is refused. Audio input names are stored, since without them the audio setup would be lost.
- **Writing:** a temporary file next to the target, `sync_all`, then a rename over the target. On Unix the directory is synced too.
  - A failure or crash before the rename leaves the previous file whole. Windows cannot sync the directory; after a power failure right after a save, the old file may still be there, but never half a file.
  - Without `overwrite`, an existing file is refused. The check happens before writing, so a file another program creates in that moment would be replaced.
- **Reading:** at most 1 MiB, far more than 16 × 16 sources need.
- **CLI:** `streamforge project validate FILE` runs `validate_project` with the file's directory as the project directory, prints the result as JSON and exits 1 if the file is not valid.

## Alternatives considered

1. **Any path, from a human only (desktop, CLI).** Needed for projects next to assets or in a git repository. It is a new security boundary, though, and the same commands serve agents. It can come later for the desktop and CLI, where a person chooses the file.
2. **The output directory.** No new flag, but recordings and projects would share one place, and every engine with an output directory could write project files.
3. **A file format with its own types.** It would repeat every rule of `add_source` and `set_audio_sources` and drift from them. Decoding the command parameters keeps one set of rules; the cost is that a source problem points at the source object, and its message names the field.
4. **Lenient reading (ignore unknown fields).** Friendlier to files from newer versions, but a typo such as `opactiy` would load silently with the default. The version number already marks incompatible files.
5. **Store screen targets as hints.** Displays could be found again; windows could not, and titles would leak into a file someone shares. Choosing again after loading is one command.
6. **`renameat2(RENAME_NOREPLACE)` or hard links for no-overwrite saves.** They close the small race above, but not on every system and file system. One engine writing one directory does not need it.

## Validation

- **`project-file` unit tests:**
  - round trip: a project with a solid, color bars, a screen source, an input, a tone and gains loads back equal; the screen target is not written, and everything else returns
  - the file reads like command results (`list_scenes` objects, `set_audio_sources` fields); a hand-written source may omit its rectangle and opacity
  - refusals with their paths: invalid JSON, not an object, no `schema_version`, versions 0 and 2, an unknown field, a wrong type, a missing field, another canvas, 0 or 18 scenes, bad and duplicate scene ids and names, an unknown `active_scene`, source problems (opacity, kind, color, width, unknown field, duplicate name, `scene`, `source_id`), bad, missing and duplicate source ids, 17 sources, audio gains, frequencies and input names
  - all problems at once
  - store: create, refuse to overwrite, replace, no temporary file left; a failure after the temporary file is complete keeps the old file; missing and oversized files
- **command-api:** decoding of the four commands, file names refused (`..`, absolute, directories, other extensions), access classes.
- **engine:**
  - save, then load into a fresh engine: the same scene list (ids included, screen target cleared), the same mixer, a warning about the screen source, and `set_screen_source` by id afterwards
  - no project directory, existing file without `overwrite`, `overwrite`, missing file, save while the preview runs, load refused while it runs
  - an invalid file changes nothing; `details.problems` has both problems with their paths; `validate_project` reports them
  - `set_screen_source`: refused for color bars, sets the target, refused while the capture runs
- **stdio:** save in one engine process and load in another with `--project-dir`; a path is refused; without the flag the commands are `invalid_state`.
- **CLI:** `project validate` on a valid file (exit 0), an invalid one (exit 1, the path), a newer version, a missing file.
- The example file in `docs/protocol.md` passes `streamforge project validate`.

## Consequences

- Scenes and the audio setup survive a restart, and an agent can save and restore a production, within the directory the operator chose.
- Changing a command's parameter rules changes what a project file accepts. The project tests catch a change that breaks saved files.
- The desktop has no project UI yet; it would need `--project-dir` and a way to pick a file. A `list_projects` command would help it and agents.
