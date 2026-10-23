# StreamForge design system: "Control Room"

The visual direction and the reusable rules for the desktop (roadmap M12). Tokens and components live in code (`crates/streamforge-desktop/src/ui/`). This document says why they are what they are, and views follow it instead of styling themselves.

## Direction

StreamForge should feel like the control desk of a broadcast room, built with the care of high-end creative software. It is an instrument for running a live production. It is not a dashboard about one.

- **The picture is the brightest thing on screen.** Everything else is a quiet, neutral frame, so the program monitor and live state carry the hierarchy. Surfaces are pure neutral greys, with no blue or warm tint, because a tinted frame shifts how the eye reads colour in the video.
- **Broadcast conventions carry meaning.** Red is the tally: on air, recording, the program. Green means healthy or connected, amber means needs attention. These colours are kept for state; they are never decoration.
- **One accent, for the operator's own focus:** selection, keyboard focus and the primary action of a dialog. It is a cool teal ("scope teal", after the traces of a waveform monitor). Nothing else uses it.
- **Engineered typography.** IBM Plex Sans for the interface and IBM Plex Mono for every number that is read against time: timecodes, dB, counters, rates. Plex's technical character fits a production tool and sets it apart from the system-font and Inter look of generic apps.
- **Hard edges where the content is, soft only where the hand is.** The monitor and meters are square, as on hardware. Controls have a 3 px radius, enough to read as touchable. There are no gradients, glows or glass, and the only shadow is the one under popups.
- **Dense and calm.** 24 px control rows and 8 px panel padding. The grouping comes from position and alignment, not from boxes inside boxes.

### How it differs from the current egui look

| Today | Control Room |
|---|---|
| Default font; sizes are the only hierarchy | Plex Sans with roles (title, label, body, metadata); Plex Mono for numbers |
| egui blue for selection, saturated pure RGB for status | One teal accent for focus and selection; a tuned semantic palette for state, always with a label or icon |
| Boxes (`group`) and separators everywhere | Docks with a 1 px divider; rows aligned to a grid; no nested borders |
| Text and glyphs as indicators (`●`, `▶`) | Painted icons and indicators on a 16 px grid with a 1.5 px stroke |
| Live state as small text | Transport block with tally, labels and running timers; a tally frame on the monitor |
| Configuration fields in the main view | Set once in output settings; the main view shows state and the start/stop actions |

## Tokens

All values are logical pixels at scale 1.

### Colour

| Token | Value | Use |
|---|---|---|
| `bg.app` | `#101112` | window background, behind docks |
| `bg.panel` | `#16171A` | docks and panels |
| `bg.raised` | `#1D1F22` | rows on hover, cards, strips, buttons |
| `bg.inset` | `#0B0C0D` | monitor well, meter troughs, text fields |
| `line.subtle` | `#25272B` | dividers between docks, row separators |
| `line.strong` | `#33363B` | control outlines, focusable edges |
| `text.primary` | `#E7E8EA` | content, values |
| `text.secondary` | `#A2A6AC` | labels, secondary values |
| `text.tertiary` | `#6C7077` | section labels, metadata, hints |
| `text.disabled` | `#464A50` | disabled text and icons |
| `accent` | `#2CB5C2` | focus ring, selection bar, primary action |
| `accent.weak` | `#2CB5C2` at 14 % | selected row background |
| `tally.program` | `#E2393B` | on air, recording, program |
| `state.ok` | `#3FBF72` | connected, healthy, signal present |
| `state.warn` | `#E6A23C` | degraded, reconnecting, attention |
| `state.error` | `#F2683D` | failed, ended; always with the error icon |
| `meter.low` / `meter.mid` / `meter.high` | `#3FBF72` / `#E6C43C` / `#E2393B` | level zones (below −18, −18 to −6, above −6 dBFS) |

`tally.program` and `state.error` are close in hue but never appear in the same role. The tally is a frame or a filled badge with a label ("REC", "LIVE"); an error is an inline line of text with the error icon.

### Typography

| Role | Face | Size | Use |
|---|---|---|---|
| `title` | Plex Sans SemiBold | 14 | dialog titles, the project name |
| `section` | Plex Sans Medium, upper case, +0.6 tracking | 11 | dock and section labels |
| `body` | Plex Sans Regular | 13 | rows, buttons, inputs |
| `label` | Plex Sans Medium | 12 | control labels, badges |
| `meta` | Plex Sans Regular | 11 | secondary facts, hints |
| `numeric` | Plex Mono Medium | 12 | dB, counters, rates |
| `timecode` | Plex Mono Medium | 16 | running timers in the transport |

### Spacing, sizing, shape

- **Spacing scale:** 2, 4, 6, 8, 12, 16, 24. Panels have 8 px padding, rows 6 px horizontal padding, and controls in a row have 6 px gaps.
- **Control heights:** 24 px (dense, default) and 32 px (transport buttons). Rows are 26 px.
- **Radius:** 0 for the monitor and meters, 3 for controls, 4 for popups and cards.
- **Stroke:** 1 px for dividers and outlines, 2 px for focus rings, selection bars and tally frames.
- **Icons:** a 16 px box, 1.5 px stroke, round caps, drawn in `text.secondary` and in `text.primary` when hovered or active.

### Motion

- 120 ms ease-out for hover and selection, 160 ms for panels opening.
- No blinking. A state waiting for the network ("connecting", "reconnecting") pulses its indicator slowly (1.2 s). Recording and live are steady.
- Meters move as fast as the signal, with a slow fall of 24 dB/s, and a peak hold that drops after 1.5 s.

## Layout: the production workspace

```
┌ Top bar ───────────────────────────────────────────────────────────────────────────┐
│ ▣ StreamForge   project ▾ │      ● REC 00:12:04     ◉ LIVE 00:05:51      │ health ▸ │
├ Scenes ─────┬ Program ─────────────────────────────────────────────┬ Inspector ──────┤
│ ● Interview │ ┌──────────────────────────────────────────────────┐ │ lower third     │
│   BRB       │ │                                                  │ │ solid · src_…   │
│   Starting  │ │          program monitor, 16:9, tally frame      │ │ x  80   y  860  │
│ + scene     │ │                                                  │ │ w 900   h  120  │
├ Sources ────┤ └──────────────────────────────────────────────────┘ │ opacity ──●── 1 │
│ ▤ lower 3rd │ 1920×1080 · 60 fps · render 2.1 ms                   │                 │
│ ▤ bars      │                                                      │                 │
│ ▤ backdrop  │                                                      │                 │
├ Mixer ──────┴──────────────────────────────────────────┬ Outputs ──┴─────────────────┤
│  tone 1 kHz   mic        MASTER                       │ ● Record   recording.mkv     │
│  ▮▮▮▮▯ -20    ▮▮▯▯▯ -41  ▮▮▮▮▯ -18                    │ ◉ Stream   rtmp://…/live     │
└────────────────────────────────────────────────────────┴──────────────────────────────┘
```

- **Program monitor** in the centre, as large as the space allows, at 16:9. A 2 px tally frame and a "REC" or "LIVE" badge appear while an output runs; with only the preview on, it says "PREVIEW". Under it is one line of technical facts in `meta` and `numeric`.
- **Scenes** at top left: the program scene has a tally dot and the label "PGM". The selection, the scene being edited, has the accent bar. These are two separate marks, so editing a scene that is not on air is visible.
- **Sources** of the selected scene, top layer first, with a type icon. Selecting a source opens it in the **inspector** on the right: position, size, opacity, and the capture state for screen sources. The inspector is the only place where properties are edited.
- **Transport** in the top bar: Record and Live, each with an icon, a label, a state colour, a running timer once started, and a confirmation for stopping. Health is summarised there as one indicator (ok, warn or error) that opens the details.
- **Mixer** at the bottom, as a console in zones: channels, then buses, then the program bus set apart on the right. A channel strip has a meter (scale, peak hold, clip), a fader, mute and the name, and later its sends and processing. Bus and output strips share the strip's anatomy. Routing has its own view, a matrix: rows are channels, columns are buses, and each cell is a send with its level and pre/post mark. Outputs sit beside it, each with the bus it takes as its mix (ADR 0017: outputs take buses, never channels). A matrix stays readable at 5 to 30 elements, where a node graph turns into a tangle. The console is built in M12 on the routing model of M11 (`docs/roadmap.md`), so it shows what the engine does and is built once.
- **Outputs** at the bottom right: per output, its target, its state and its numbers, plus a settings action for the target (file name, URL, key). The fields themselves no longer sit in the main view.
- Docks can be resized; the monitor takes what is left. At 1280×720 the inspector folds into a drawer.

## Components

| Component | Variants and states |
|---|---|
| Button | primary (accent), secondary (raised with an outline), ghost (text only), danger (needs confirmation); each with hover, pressed, focus, disabled and busy |
| Transport button | record, live; idle, starting, running (with a timer), failed |
| Toggle | an on/off switch for binary settings such as the preview and mute |
| Text field | inset background, outline, focus ring, error line with icon under it |
| Section header | `section` label, optional count, actions on the right |
| List row | icon, name, trailing meta; hover, selected (accent bar), program (tally dot plus "PGM") |
| Badge | REC, LIVE, PREVIEW, PGM; filled (tally) or outlined (neutral) |
| Meter | vertical or horizontal; zones, peak hold, clip latch, dB scale |
| Fader | vertical, −60 to +12 dB, unity marker, drag commits once |
| Status line | an icon plus text for ok, warn, error and info; never colour alone |
| Empty state | an icon, one line saying what is missing, the action that fixes it |

States are defined once:
- Hover lifts the background to `bg.raised`.
- Pressed darkens it by 4 %.
- Focus draws a 2 px `accent` ring, 1 px outside the control.
- Disabled uses `text.disabled` and no outline.
- Busy replaces the icon with a small spinner and keeps the label.
- Error adds the error icon and a message, and never only a red border.

## Keyboard

| Key | Action |
|---|---|
| Space | Start or stop the preview |
| Ctrl+R | Start recording; stopping asks |
| Ctrl+L | Go live; stopping asks |
| Ctrl+1 … Ctrl+9 | Put scene 1 to 9 on program |
| ↑ / ↓ | Move in the focused list |
| Delete | Remove the selected source; asks first |
| Esc | Cancel a confirmation, close a drawer |

Tab moves focus through docks in reading order, and the focus ring is always visible.
