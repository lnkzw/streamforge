# Installing StreamForge on Linux

This package was built for one distribution family; its name says which (`ubuntu24.04` or `archlinux`), and `BUILD-INFO.txt` lists the library versions it links. StreamForge uses the system's FFmpeg and PipeWire libraries, so the package runs where those have the same versions: the distribution it was built for. On another one, build from source (README).

## Runtime dependencies

- **FFmpeg** libraries (H.264 and AAC encoding) and **PipeWire** (screen and audio capture).
- For screen capture: **xdg-desktop-portal** with the backend of your desktop (`-gnome`, `-kde`, `-wlr`, `-hyprland`, …).
- For the desktop app: a Wayland or X11 session.

Ubuntu 24.04:

```sh
sudo apt install ffmpeg pipewire xdg-desktop-portal
```

Arch Linux:

```sh
sudo pacman -S --needed ffmpeg pipewire xdg-desktop-portal
```

## Install

```sh
sha256sum -c streamforge-*.tar.gz.sha256
tar -xzf streamforge-*.tar.gz
cd streamforge-*/
./bin/streamforge status
```

The binaries run from anywhere; copy `bin/*` into a directory on your `PATH` (e.g. `~/.local/bin`) to use them by name.

- `streamforge`: command line (recording, streaming, capture checks, `project validate`)
- `streamforge-desktop`: desktop app
- `streamforge-engine`: headless engine for scripts (JSON lines on stdin and stdout)
- `streamforge-mcp`: MCP server for AI agents

## License

StreamForge is licensed under either of the MIT license (`LICENSE-MIT`) or the Apache License 2.0 (`LICENSE-APACHE`), at your option. The binaries link the system FFmpeg, so they are also subject to the license of that FFmpeg build. With libx264, as both distributions build it, that is the GPL. The StreamForge source code is at https://github.com/lnkzw/streamforge.
