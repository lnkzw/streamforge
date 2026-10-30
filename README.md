# StreamForge

StreamForge is a streaming and recording application with its own real-time media engine. It composes scenes, mixes audio with buses and sends, and records and streams the result. A desktop app, a command line, a JSON-lines protocol and an MCP server for AI agents all drive the same engine through the same commands. It does not use OBS or libobs.

![The StreamForge desktop app while recording](docs/images/desktop.png)

## Status

Version 0.2.0 adds cameras, images and layers to the first release, for **Linux** (packages for Ubuntu 24.04 and Arch Linux). Windows and macOS build from source and pass the test suite in CI; packages for them follow after tests on real machines.

What works:

- **Scenes** of solid colours, colour bars, screen or window capture (XDG portal and PipeWire on Linux), PNG and JPEG images and cameras, composed at 1920×1080 and 60 fps, with a live preview. Layers can be reordered and hidden, also on air.
- **Audio** from microphones, desktop audio and test tones: channels with gain, mute and balance, aux buses, sends before or after the fader, and a headphone monitor.
- **Recording** to Matroska (H.264 and AAC), with further audio tracks if you want them.
- **Streaming** over RTMP or RTMPS at the same time as recording, with reconnects. The two fail independently.
- **Hardware encoding** with VAAPI or NVENC when available, with a fallback to libx264.
- **Projects** that save scenes, audio setup and output settings to a JSON file.
- **Control** from the desktop app, the `streamforge` command line, scripts (`streamforge-engine`), and AI agents over MCP with access levels. Agents never see stream keys.

The [known limitations](docs/user-guide.md#known-limitations) list what is missing, such as audio effects, transitions and text sources.

## Install

Download the package for your distribution from the [releases page](https://github.com/lnkzw/streamforge/releases), then:

```sh
sha256sum -c streamforge-*.tar.gz.sha256
tar -xzf streamforge-*.tar.gz
cd streamforge-*/
./install.sh          # into ~/.local, with an entry in the application menu
streamforge status
```

It needs FFmpeg, PipeWire and xdg-desktop-portal from your distribution. The [user guide](docs/user-guide.md#install) has the details and how to build from source.

## Quick start

- **Desktop:** start *StreamForge* from the application menu, or `streamforge-desktop --output-dir ~/Videos`. Add sources and audio channels, then press **REC** or **LIVE**.
- **Command line:** `streamforge record --file-name test.mkv` records ten seconds of a test scene; `streamforge capture caps` shows what your system can capture.
- **AI agents:** `claude mcp add streamforge -- ~/.local/bin/streamforge-mcp --allow control` lets Claude Code edit scenes and the mixer.

## Documentation

- [User guide](docs/user-guide.md): install, desktop app, command line, scripting, MCP setup, troubleshooting, known limitations
- [Control protocol](docs/protocol.md): the JSON-lines commands, results and errors
- [Platforms](docs/platforms.md): what each operating system supports
- [Development](docs/development.md): building from source, tests, repository layout
- [Changelog](CHANGELOG.md): what changed in each version
- [Architecture](docs/architecture.md), [roadmap](docs/roadmap.md) and [decisions](docs/decisions)

## Licence

StreamForge is licensed under either of the [MIT licence](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option. Unless you state otherwise, any contribution you intentionally submit for inclusion is licensed the same way, without additional terms. The desktop app embeds the IBM Plex fonts under the SIL Open Font License 1.1 (`crates/streamforge-desktop/assets/fonts`). The binaries also contain the Rust crates in `Cargo.lock`, under their own licences; the packages list them with their licence texts in `LICENSE-RUST-CRATES.txt` ([`packaging/licenses`](packaging/licenses/rust-crates.txt)).

The programs link the system's FFmpeg libraries, so a binary is also subject to the licence of the FFmpeg build it links. With libx264, as in the Ubuntu and Arch Linux packages, that is the GPL. Anyone distributing StreamForge binaries must comply with it ([ADR 0005](docs/decisions/0005-recording-with-ffmpeg.md)).
