# ADR 0014: Stable scene and source ids

- **Status:** Accepted
- **Date:** 2026-10-22

## Context

Scenes had only a name. Sources had a number from a counter, assigned again on every run, and commands found both by name. A project file (assessment §8.3, step 6) has to store references between objects: the active scene now, later transitions, hotkeys or which output shows which scene. A reference by name breaks when something is renamed; a per-run number breaks across runs.

The user decided:
- the file stores ids and references them;
- ids are opaque strings with a type prefix;
- commands take an id or a name, and the id comes first.

## Decision

- **Format:** `scene_` or `src_`, then 12 characters from `a-z` and `0-9` (about 62 random bits from the operating system's generator, `getrandom`).
  - Random ids do not collide when scenes are copied between projects, and a removed source's id does not come back.
  - Clients treat ids as opaque. The prefix says what kind of object an id names, so a scene id cannot pass for a source id.
  - Parameters accept the prefix, then `a-z`, `0-9` or `_`, at most 64 bytes, so a project file (step 6) can bring ids of the same shape back.
- **Uniqueness:** source ids are unique across all scenes, not only within one. A source can therefore be named by its id alone, and captures stay keyed by it while their source is in a scene that is not rendered.
- **Built-in scene:** `Test pattern` and its sources have fixed ids (`scene_test_pattern`, `src_color_bars`, `src_overlay`). The moving overlay is recognized by its id, and a fixed id survives saving and loading. Generated ids never look like these. A project file can set any id (ADR 0015), so the overlay moves only if it is also a solid source in the built-in rectangle.
- **Naming in commands:**
  - A scene by `scene_id`, or by its name (`name` in the scene commands, `scene` in the source commands).
  - A source by `id` alone, or by its scene and `name`.
  - The id is looked up first. If a name is given as well, it must name the same object; if it does not, the command fails with `invalid_params` rather than quietly following the id. An unknown id is `not_found`, with the id in `details`.
  - `source_id` stays what a screen source captures (`list_screen_sources`); the new parameters are named so they do not collide with it.
- **Compositor:** `SourceId` holds a shared string (`Arc<str>`) instead of a `u32`, so `render.sources[].id` in `get_health` and the stop reports is the scene source's id with no translation table. Cloning one counts a reference and does not allocate; the render thread copies ids into its live counters each publish, as before. The reference scene of `probe_composite` uses `bars`, `screen` and `overlay`, and the legacy screen layer is `screen_layer`.
- **Protocol:** `id` changes type from number to string in `list_scenes`, `get_health` and the reports. The versioning policy would require v2 for that. Nobody uses the protocol yet, though, and its clients live in this repository, so the user decided the policy applies from the first release on. Until then, breaking changes are listed in `docs/protocol.md`, and every client changes with them. The desktop now names sources by id, and sends a scene's id with its name when it activates or deletes it, so a scene replaced meanwhile is not deleted.

## Alternatives considered

1. **Names only.** They are readable and natural for agents, but references in a file break on rename, and a rename command would have to rewrite every reference.
2. **Ids only in commands.** Robust, but every human or agent call would have to read `list_scenes` first, and errors would only show ids. Taking both costs one lookup rule.
3. **Numbers that are saved and restored.** A counter in the file could be stable too. But numbers from two projects collide when scenes are copied between them, and a scene number can be mistaken for a source number.
4. **UUIDs.** Unique as well, but 36 characters are long in a UI and in an agent's context, and they would need another crate. 62 random bits are enough for collections of at most 16 × 16 sources.
5. **Keep the compositor's `u32` and translate in the engine.** The engine would need a handle table and its own copies of the render statistics types, just to translate ids in `get_health` and the reports. A string id in the compositor gives one id everywhere.
6. **Resolve an id/name mismatch by the id.** "Id first" could mean ignoring the name. A mismatch usually means the client acts on a stale list, and following the id could change the wrong object. So it is an error.

## Validation

- **command-api:**
  - decoding by id, by name and by both, for every scene and source command
  - refusals: neither given, a source name without its scene, a wrong prefix, upper case, an empty or too long id, a non-string id
  - the `add_source` schema no longer requires `scene`
- **engine:**
  - id generation: prefix and length, and a taken id drawn again
  - the collection: fixed built-in ids; new scenes and sources get prefixed ids; lookup by id alone, by name and by both; mismatches are `invalid_params`; an unknown id is `not_found` with `{"id": ...}`; adding a removed source's name again gives a new id
  - `get_health`: `captures[].id` and `render.sources[].id` equal the source's id from `list_scenes`
  - `restart_source` by id alone restarts a kept capture of another scene
- **MCP:** an agent moves a source by its id from `list_scenes`.
- **Desktop:** the model joins health to sources by string id. The acceptance test, against the real engine, changes a source's opacity and removes a source by id, and a deletion whose id and name no longer agree is refused. Restart by id is not covered there (it needs a screen capture).
- **Wire:** `probe_composite` over stdio reports the source ids `bars` and `overlay` as JSON strings.
- **Found on the way:** removing a source from a scene that is not rendered, or deleting that scene, left its kept capture running until the pipeline stopped. Such edits now end it at once, and the engine test checks `get_health.captures` right after them.

## Consequences

- A project file (step 6) can store references that survive renames, and scenes copied between projects keep their ids without collisions.
- Clients that keep a source across calls should keep its id; names remain fine for one-off calls and for agents.
- `id` values in old logs or scripts are numbers and no longer match. There are no users yet, so nothing outside the repository breaks.
