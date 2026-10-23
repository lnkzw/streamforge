# Desktop UI audit (before M11)

The state of `streamforge-desktop` at `56ddcdd`, from the code (`crates/streamforge-desktop/src/app.rs`) and from headless renders of the running app at 1280×720, 1400×900 and 1920×1080. The renders come from `tests/render.rs`, which drives the real model and engine: idle, and live with the preview on and a recording running.

## Summary

The desktop is a working control surface, not a production tool. Every function is reachable, but the screen has no hierarchy: the live state, the editing of scenes, configuration and diagnostics all have the same visual weight. Nearly everything is egui's default dark theme (font, colors, widgets). Two rendering defects and a layout that does not scale make it look unfinished.

## Defects

1. **The REC indicator shows a missing glyph.** `● REC` (`app.rs:146`) renders as an empty box: egui's default font has no U+25CF. Status that relies on a glyph the font lacks fails silently.
2. **The level meter breaks at the floor.** `meter_bar` (`app.rs:584`) uses a `ProgressBar` with text. At −60 dB the fill shrinks to a green circle, and the "−60 dB" label is drawn over it. The label is white on saturated green at every level, so contrast is poor.
3. **The monitor does not scale.** The preview is capped at 960 px wide (`app.rs:269`). At 1920×1080, half the central area stays empty while the monitor is the most important element.

## Hierarchy and layout

- **No production priority.** Recording and streaming, the most consequential actions, are small default buttons in a bottom row (`outputs_panel`, `app.rs:486`), next to text fields for a file name, an RTMP URL and a stream key. "Stop recording" looks like "Remove".
- **Live state is a line of small text.** The top bar says "● REC" (broken, see above) or "not recording"/"not streaming" in faint gray. There is no elapsed time, no clear on-air state, and no tally on the monitor. During a recording the monitor looks the same as during a plain preview.
- **Configuration mixed with operation.** The output file name, URL and key fields stay in the main workspace and in view during a live show, though they are set once.
- **Diagnostics as raw text.** The health strip prints "REC: 109 frames, 0 dropped, queue 0 | render: 0 late, 0 skipped, p95 3.1 ms". It is precise but not glanceable. The activity log takes a fixed band and shows full temporary paths.
- **Editing and program are not told apart.** Selecting a scene other than the active one shows that scene's sources under the monitor, while the monitor shows the active scene. The only cues are a blue highlight (selected) and a "▶" (active), with no label for either.
- **The preview toggle is a checkbox** labelled "Preview" next to the application name.

## Interaction

- **Destructive actions without weight or confirmation.** "Remove" on every source row (`app.rs:317`) deletes at once and looks like every other button. Deleting a scene asks first (good), but the confirmation appears as loose text under the list.
- **Sources cannot be edited after adding.** Only opacity is adjustable (a slider with three decimals, `1.000`). Position and size are only set in the "Add source" form, though `update_source` supports them. There is no selection, no inspector and no reordering.
- **The add-source form is a collapsing section** at the bottom of the central area, with x/y/w/h fields shown once "Whole canvas" is unchecked.
- **Scene creation is always on screen:** a text field and a "Create" button that is disabled until something is typed.
- **The mixer is a list of boxed groups.** Each strip has a horizontal meter, a horizontal gain slider and a checkbox. There is no dB scale, peak hold, clip indicator, or vertical fader layout. "idle — levels show while previewing or live" wraps across two lines.
- **Audio sources cannot be configured** in the desktop: the mixer shows the default test tone, and inputs can only be set through the CLI or protocol.
- **No keyboard shortcuts** for any live action, and few tooltips.
- **No project handling:** scenes are lost on exit (planned for M11).

## Visual language

- Default egui font (Ubuntu Light at small sizes) with no typographic roles. Headings, labels, values and metadata differ only by size and grayness, and numbers do not line up.
- Default widgets and colors: blue selection, gray buttons, saturated `Color32::RED`, `GREEN`, `YELLOW` and `ORANGE` for status (`app.rs:146`, `:395`, `:399`, `:586`) with no shared meaning.
- Spacing comes from egui's defaults; group borders in the mixer, separators elsewhere, no grid.
- No icons; text and glyphs (`▶`, `●`) stand in for them.
- Colour carries critical state on its own, e.g. capture state green or red, and stream state green or orange.

## What works and stays

- The model/widgets split: `Model` turns actions into commands and holds no widgets (ADR 0007). A redesign touches `app.rs` and new UI modules, not the model's contract.
- Live polling cadences (100 ms status and meters, 250 ms health) and repaint at 30 fps only while something is live.
- Confirmations for deleting scenes and restarting working captures, ids sent with names, and drag-commit for sliders (one command per drag).
