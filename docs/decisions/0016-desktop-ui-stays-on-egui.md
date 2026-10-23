# ADR 0016: The desktop UI stays on egui, with its own design system

- **Status:** Accepted (the project owner approved the direction, colours and typography on 2026-10-23)
- **Date:** 2026-10-23

## Context

The desktop is to be redesigned into a professional production studio with its own visual identity (roadmap M12; the direction is `docs/ui/design-system.md`). The project owner's brief asks whether the toolkit can deliver that before the redesign starts: retain, extend or replace egui (ADR 0007), with evidence.

The audit (`docs/ui/audit.md`) finds the problems in composition and styling: no hierarchy, default theme and font, status that relies on glyphs and colour alone, a layout that does not scale. It finds none that egui caused.

## What the design needs, and what egui offers

| Need | egui 0.36 | Cost |
|---|---|---|
| Central tokens: colours, spacing, radii, strokes, control sizes | `Style` and `Visuals` cover widget states (inactive, hovered, active, open), selection, spacing, interaction sizes; everything is set once per frame from code | low |
| Own typography | Any TTF through `FontDefinitions`, named font families, sizes per `TextStyle`, `extra_letter_spacing` per text | low; fonts are bundled |
| Tabular numbers for timers and meters | No OpenType features, so no `tnum` | use a monospace face for numbers |
| Icons with one stroke weight | No icon set; the `Painter` draws paths, circles and rectangles | ~25 small painted icons, no font or crate |
| Meters, faders, tally frames, transport buttons | Custom `Widget`s on the painter, with `Sense` for interaction | the main work, as in any toolkit |
| Hover, focus, pressed, disabled, selected | From `Response`, per widget | low |
| Keyboard operation and focus | Focus moves with Tab, focus rings come from `Visuals`, shortcuts come from `InputState` | low |
| Accessibility | AccessKit is built in (eframe default) | none |
| Motion | `animate_bool` / `animate_value_with_time` | low |
| Responsive layout | Resizable side and bottom panels, `available_size`, explicit sizing | needs care: one pass per frame, so sizes are computed, not negotiated |
| Docking and detachable panels | Not built in | not needed for the first release; fixed docks with resizable splits |
| Live preview frames | A texture updated from the engine's mailbox, no copy through IPC | exists |

## Alternatives

1. **iced (Elm-style, retained).** Good theming and text shaping (cosmic-text). Accessibility and custom widgets are less mature, and it means rewriting every view and the update model. Visual quality would still come from custom widgets.
2. **Slint (declarative markup).** Strong styling and tooling. Its licences (GPL-3.0, a royalty-free licence with attribution, or commercial) sit awkwardly with MIT OR Apache-2.0, and the UI moves to a second language.
3. **A web UI (Tauri).** The most flexible styling and layout (CSS). It adds a webview on each OS, a JavaScript toolchain, and an IPC path for preview frames (about 15 MB/s at 480×270, 30 fps) that the in-process mailbox avoids today. It is a second front-end stack to maintain.
4. **GTK 4 / libadwaita.** Native on Linux, but it pulls towards the GNOME look instead of an own identity, and is weaker on Windows and macOS.

## Decision

- Retain egui and extend it with an own design-system layer in `streamforge-desktop` (`ui::theme`, `ui::icons`, `ui::widgets`). Views use tokens and components only, never raw colours or ad-hoc spacing.
- Bundle the fonts (IBM Plex Sans and Mono, SIL OFL 1.1). Draw icons and indicators with the painter, so nothing depends on glyph coverage.
- Use numbers in the monospace face where they must line up: timers, dB, counters.
- Revisit the toolkit only for a concrete need that egui cannot meet at reasonable cost, such as complex-script text or detachable windows.

## Consequences

- No new UI stack. The engine, the model and the command API stay as they are.
- Custom widgets (meters, faders, transport buttons, tally) are written and tested in Rust, and rendered headless in tests (`tests/render.rs`).
- About 1 MB of fonts in the desktop binary.
- Two egui limits stay: no OpenType features, and no automatic font fallback for missing glyphs. Both are handled above.
