# Installing StreamForge on Linux

This package was built for one distribution family; its name says which (`ubuntu24.04` or `archlinux`), and `BUILD-INFO.txt` lists the library versions it links. StreamForge uses the system's FFmpeg and PipeWire libraries, so the package runs where those have the same versions: the distribution it was built for. On another one, build from source (`docs/development.md`).

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
./install.sh
streamforge status
```

`install.sh` installs for your user, into `~/.local`:

- `bin/`: the programs
  - `streamforge`: command line (system checks, test recordings and streams, `project validate`)
  - `streamforge-desktop`: desktop app
  - `streamforge-engine`: headless engine for scripts (JSON lines on stdin and stdout)
  - `streamforge-mcp`: MCP server for AI agents
- `share/applications/io.github.lnkzw.StreamForge.desktop` and its icon: *StreamForge* in the application menu
- `share/doc/streamforge/`: this file, the README, the documentation (`docs/`) and the licences

If `streamforge` is not found afterwards, add `~/.local/bin` to your `PATH`. `./install.sh --prefix DIR` installs into another directory (for example `/usr/local`, as root), and `./install.sh --uninstall` with the same `--prefix` removes everything it installed. Projects and recordings stay where you saved them.

The programs also run straight from this folder, without installing: `./bin/streamforge status`.

Start with the user guide, `docs/user-guide.md`.

## License

StreamForge is licensed under either of the MIT license (`LICENSE-MIT`) or the Apache License 2.0 (`LICENSE-APACHE`), at your option. The desktop app embeds the IBM Plex fonts, licensed under the SIL Open Font License 1.1 (`LICENSE-OFL-IBM-Plex.txt`). The binaries link the system FFmpeg, so they are also subject to the license of that FFmpeg build. With libx264, as both distributions build it, that is the GPL. The StreamForge source code is at https://github.com/lnkzw/streamforge.
