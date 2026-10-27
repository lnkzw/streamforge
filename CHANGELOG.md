# Changelog

All notable changes to StreamForge are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/). The control protocol has its own version, `v` ([docs/protocol.md](docs/protocol.md#versioning-policy)).

## [Unreleased]

### Added

- Layers (ADR 0019): `move_source` changes a source's place in its scene's drawing order, and `visible` (in `add_source`, `update_source` and `list_scenes`) hides a source without stopping its capture. In the desktop app, drag a source row or press Ctrl+Up and Ctrl+Down to reorder, and use the eye to hide a layer.

### Changed

- Project files are schema version 4 (sources have `visible`). Versions 1 to 3 migrate on load; 0.1.0 cannot open a file saved by this version.

## [0.1.0] - 2026-10-27

The first release: a streaming and recording application with its own real-time media engine, for Linux. Packages for Ubuntu 24.04 and Arch Linux on x86_64; see `INSTALL.md` in each.

### Added

- **Desktop app** (`streamforge-desktop`): scenes and sources with a live preview, an audio console and routing matrix, output settings, projects and keyboard control. Closing the window with unsaved changes, or while recording or live, asks first.
- **Scenes** of solid colours, colour bars and screen or window capture (XDG desktop portal and PipeWire on Linux), composed on one 1920×1080 canvas at 60 fps.
- **Audio** from microphones, desktop audio and test tones: channels with gain, mute and balance, aux buses, sends before or after the fader, meters, and a headphone monitor on a chosen output device.
- **Recording** to Matroska with H.264 video and AAC audio, with further audio tracks from aux buses if you want them. A file is never overwritten; the desktop names each take by its start time unless you type a name.
- **Streaming** over RTMP or RTMPS, at the same time as a recording, with reconnects. Each output fails on its own.
- **Hardware video encoding** with VAAPI or NVENC where the encoder opens, and libx264 otherwise.
- **Projects**: scenes, the audio setup and output settings in a JSON file (schema 3).
- **Command line** (`streamforge`): `record`, `stream`, `capture`, `project`, `mix`, `compose` and `status`, with JSON reports.
- **Headless engine** (`streamforge-engine`): the control protocol v1 as JSON lines on stdin and stdout, for scripts.
- **MCP server** (`streamforge-mcp`) for AI agents, with access levels (read-only, control, sensitive) chosen by whoever starts it. Stream keys come from the environment and never reach the agent.
- **Clean shutdown:** Ctrl+C or SIGTERM stops every program and finalizes open recordings and streams; a second one quits at once.
- **Packages** with an installer (`install.sh`, into `~/.local` by default), an application-menu entry and an icon.
- **Documentation:** a user guide, the protocol reference, platform notes, architecture and decision records.

### Compatibility

- From this release on, protocol v1 changes only in backward-compatible ways (new commands, new optional result fields, new error codes); anything else needs v2.
- Windows and macOS build from source and pass the test suite in CI, but have not been tested by hand on real machines, and there are no packages for them yet.

### Known limitations

One canvas (1920×1080 at 60 fps) shared by every output; H.264 and Matroska only; no cameras, images, text or media sources; no audio effects; scene changes are cuts. The full list is in the [user guide](docs/user-guide.md#known-limitations).
