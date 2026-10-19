# Linux capture backend (M2 spike)

`crates/capture-linux` implements the platform-neutral `capture_api::CaptureBackend` on Linux:

- **Screen:** the XDG desktop portal (`org.freedesktop.portal.ScreenCast`) shows the compositor's source picker. The selected monitor or window arrives as a PipeWire node, consumed through a portal-provided PipeWire file descriptor.
- **Audio:** PipeWire source nodes are found through the registry and captured as interleaved F32 at the device's native rate and channel count.

This is a spike. Probes measure and report what arrives, but no frames or samples flow into a media graph yet.

## Requirements

### Build

| Dependency | Why | Arch | Debian/Ubuntu |
|------------|-----|------|---------------|
| `libpipewire-0.3` headers, ≥ 0.3.44 | `pipewire` crate (FFI); `target.object` needs 0.3.44 | `pipewire` | `libpipewire-0.3-dev` |
| `clang` / `libclang` | `bindgen` generates the PipeWire bindings | `clang` | `libclang-dev` |
| `pkg-config` | locates libpipewire | `pkgconf` | `pkg-config` |

On other operating systems, `capture-linux` compiles to an empty crate. The binaries then fall back to `capture_api::UnsupportedCapture`.

### Runtime

- A running PipeWire daemon and a session manager (WirePlumber) in the user session.
- `xdg-desktop-portal` plus a backend that implements ScreenCast for the compositor.
- A D-Bus session bus. The portal is reached over D-Bus. The PipeWire socket is found through `$XDG_RUNTIME_DIR`.

## Hyprland

- **Portal backend:** install `xdg-desktop-portal-hyprland` (xdph). It implements ScreenCast and Screenshot, and ships `hyprland-share-picker`, the dialog that pops up for every screen probe.
  - Do not also run `xdg-desktop-portal-wlr` or `xdg-desktop-portal-gnome` for ScreenCast; they compete for the interface.
  - `xdg-desktop-portal-gtk` alongside xdph is fine; it provides the other portals, such as the file chooser.
- **Portal selection:** with `XDG_CURRENT_DESKTOP=Hyprland`, `xdg-desktop-portal` reads `/usr/share/xdg-desktop-portal/hyprland-portals.conf`. On Arch that file ships with the `hyprland` package and contains `default=hyprland;gtk`. Override it in `~/.config/xdg-desktop-portal/hyprland-portals.conf` only if another portal wins.
- **Activation environment:** the portal services must see the Wayland session. When Hyprland is not started through a systemd/uwsm session, add this to `hyprland.conf`:

  ```ini
  exec-once = dbus-update-activation-environment --systemd WAYLAND_DISPLAY XDG_CURRENT_DESKTOP
  ```

  Then restart the portals with `systemctl --user restart xdg-desktop-portal-hyprland xdg-desktop-portal`, or log in again.
- **Damage-driven frames:** Hyprland only sends a frame when the captured content changes. A static screen yields few or no frames, and the negotiated frame rate is `0/1` (variable). Move something on screen during a probe.
- **Buffers:** StreamForge offers no DMA-BUF modifiers, so xdph sends shared-memory (`memfd`) buffers in a packed RGB format (BGRx observed).
- **Picker ignores requested types:** `hyprland-share-picker` shows its screen, window and region tabs whatever `source_types` asks for. The report's `source.kind` shows what was actually selected, and a warning is added when it is not one of the requested types.
- **Cursor modes:** this xdph version offers `hidden` and `embedded`, but not `metadata`. Asking for `metadata` fails with `unsupported`.

### Measured on the development machine (2026-10-19)

| Component | Version |
|-----------|---------|
| Hyprland | 0.56.2 |
| xdg-desktop-portal | 1.22.1 |
| xdg-desktop-portal-hyprland | 1.4.1 |
| PipeWire / WirePlumber | 1.6.9 / 0.5.18 |

`get_capture_capabilities` reported:
- portal version 6
- source kinds `monitor`, `window`, `virtual`
- cursor modes `hidden`, `embedded`
- PipeWire server 1.6.9

Screen probe of a 2560×1440 monitor, 200 ms:
- format `BGRx`, `memfd` buffers, frame rate `0/1`
- 15 frames, measured 77 FPS, frame interval 10.5 / 13.0 / 20.4 ms (min / mean / max)
- PTS present (time base 1/1e9)
- 0 dropped, empty or corrupted buffers

Audio probe of `alsa_input.pci-<address>.analog-stereo`, 2 s:
- F32LE, 48 kHz, 2 channels
- 95 232 frames received against 95 278 expected; the deficit of 46 frames (about 1 ms) is arrival jitter, below one 1024-frame buffer
- digital silence, because no microphone was connected

## Manual acceptance test

Run each command from the workspace root.

1. Capabilities: `cargo run -p streamforge-cli -- capture caps`
   - Check that `screen.available` and `audio.available` are `true`.
   - If either is false, `unavailable_reason` says why.
2. Screen, monitor: `cargo run -p streamforge-cli -- capture screen --source monitor --duration-ms 5000`
   - The picker appears. Choose a monitor and move a window around for 5 s.
   - The report should show the source size, the negotiated `format`, `stats.frames > 0`, `measured_fps`, and `first_pts`/`last_pts` with time base 1/1000000000. Under load, `dropped_frames` and `buffer_starvation` may be non-zero.
3. Screen, window: repeat step 2 with `--source window` and pick an entry in the picker's **window** tab. Check that `source.kind` is `window` and the size matches the window.
4. Cancel: run step 2 again and close the picker without choosing. The command must exit with code 1 and print `cancelled`.
5. Audio inputs: `cargo run -p streamforge-cli -- capture inputs`
6. Audio probe: `cargo run -p streamforge-cli -- capture audio --input <name> --duration-ms 3000`
   - The report should show the format, `frames ≈ expected_frames` (within one buffer), and levels.
   - `peak_dbfs: null` means digital silence.
7. Unsupported capability: `cargo run -p streamforge-cli -- capture screen --cursor metadata` must fail with `unsupported`, and `details.available` lists the offered modes.

The same operations are available over the JSON-lines protocol (`docs/protocol.md`).

Automated tests that need a live session are ignored by default:

```bash
cargo test -p capture-linux --test live -- --ignored --test-threads=1
```

## Troubleshooting

| Symptom | Cause / fix |
|---------|-------------|
| `unavailable: ... D-Bus error` | No session bus, or the portal is not installed or activatable. See Requirements and Hyprland above. |
| `unavailable: no portal implements org.freedesktop.portal.ScreenCast` | No portal backend with ScreenCast for this desktop. Install xdph and check the portals config. |
| `timeout: the screen cast portal dialog did not respond within 120 s` | Nobody picked a source, or the picker could not start (`hyprland-share-picker` missing). |
| `timeout: the screen stream did not start` | The portal created the stream, but no format was negotiated. Report the portal versions. |
| `not_found: no default audio input is available` | WirePlumber has no default source. Typical cause: the configured default, e.g. a removed virtual mic, no longer exists. Pass `--input`, or set a default with `wpctl set-default <id>`. |
| `cannot connect to PipeWire (Creation failed)` | The PipeWire daemon is not running, or `$XDG_RUNTIME_DIR` is unset. |
| Warning "digital silence" | The device is muted, unplugged or has no active port. |

## Limitations

- Probes block the calling thread: through the portal dialog (at most 120 s), then for `duration_ms`. In the headless engine, other requests wait until the probe finishes. Long-running capture needs a dedicated capture thread (M3).
- One stream per probe. No restore tokens, so the picker appears every time. No DMA-BUF import. No cursor metadata.
- Drops are counted from gaps in the source sequence number in `spa_meta_header`. Frames the compositor never rendered, e.g. on a static screen, are not drops.
- Audio `frame_deficit` is estimated from arrival times and is accurate to about one buffer.
- The audio default-source hint relies on WirePlumber's error text ("no target node"). Other failures are reported as `backend_error` with the original message.
- An audio input selected by name is checked against the registry before connecting. A device removed in between may still make PipeWire fall back to the default source.
- If the portal call times out, the half-open portal session is not closed explicitly. The portal cleans it up when the D-Bus connection closes.
- Microphone access is not consent-gated on the desktop (outside Flatpak). `probe_audio_capture` is classified `sensitive`; enforcement arrives with M8.
