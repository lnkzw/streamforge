# Installing StreamForge on macOS

This package is for Macs with Apple silicon (M1 or newer) and macOS 14 Sonoma or newer. It brings everything it needs: the app with the command-line programs and the FFmpeg libraries they use (built for this package, see `BUILD-INFO.txt`). Hardware encoding uses VideoToolbox when it works, and otherwise x264 on the CPU. On a Mac with an Intel processor, build StreamForge from source (`docs/development.md`).

## Install

1. Check the download, in Terminal in the folder you downloaded it to:

   ```sh
   shasum -a 256 -c streamforge-*-arm64-macos.zip.sha256
   ```

   It must print `OK`.

2. Unpack it (double-click the zip in Finder, or `ditto -x -k streamforge-*-arm64-macos.zip .`) and move `StreamForge.app` from the unpacked folder to `Applications` (or to `Applications` in your home folder).

3. Open StreamForge from Applications or Launchpad. The first time, macOS refuses (see below).

To use the command-line programs, link them into a folder on your `PATH`:

```sh
mkdir -p ~/.local/bin
for bin in streamforge streamforge-engine streamforge-mcp; do
  ln -sf /Applications/StreamForge.app/Contents/MacOS/$bin ~/.local/bin/$bin
done
streamforge status
```

If `streamforge` is not found, add `~/.local/bin` to your `PATH` (for zsh: `echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zprofile`, then open a new Terminal window).

`StreamForge.app/Contents/MacOS/` holds the programs:

- `streamforge-desktop`: the desktop app, which the app bundle starts
- `streamforge`: command line (system checks, test recordings and streams, `project validate`)
- `streamforge-engine`: headless engine for scripts (JSON lines on stdin and stdout)
- `streamforge-mcp`: MCP server for AI agents

The programs also run straight from the unpacked folder, without moving the app: `./StreamForge.app/Contents/MacOS/streamforge status`.

Opened from Finder, Launchpad or the Dock, the desktop app saves recordings and projects in your home folder. To use other folders, start it from Terminal: `open -a StreamForge --args --output-dir ~/Movies --project-dir ~/Documents/shows` (both folders must exist).

Start with the user guide, `docs/user-guide.md`.

## Opening an app from an unknown developer

The package is signed ad hoc, not with an Apple Developer ID, and not notarized, so macOS does not know who made it. The first time you open StreamForge, macOS says it cannot verify that the app is free of malware and offers only **Done** and **Move to Trash**. Choose **Done**, then:

1. Open System Settings → Privacy & Security.
2. Under Security, next to "StreamForge was blocked", choose **Open Anyway** and confirm with your password.
3. Open StreamForge again and choose **Open**.

macOS remembers this for this copy of the app. Alternatively, remove the download mark from the app before opening it, which skips the check: `xattr -dr com.apple.quarantine /Applications/StreamForge.app`. Do the same if Terminal refuses to run the command-line programs.

## Permissions

macOS asks before an app uses the camera, the microphone, the screen or the sound of other apps:

- **The desktop app** asks for itself: System Settings → Privacy & Security lists it as *StreamForge* under Camera, Microphone, and Screen & System Audio Recording. After allowing screen recording, quit and reopen StreamForge.
- **The command-line programs** ask for the app that started them: Terminal (or iTerm, …) when you run them there, the MCP client for `streamforge-mcp`. An MCP client that does not say why it needs the camera or the microphone can be ended by macOS when it asks; allow the camera and microphone for that client first, or use the desktop app for cameras.
- **Screen capture** lists screens and windows, which needs the screen-recording permission. macOS 15 asks again about once a month whether the app may keep recording the screen.
- **Desktop audio** (an output's monitor, `<output>.monitor`) needs macOS 14.6 or newer and the permission to record system audio; without it, the input stays silent and the capture report says so.
- **After an update**, macOS treats the new version as a new app, because an ad hoc signature differs with every build, and asks again. An old *StreamForge* entry in the lists may then stay off; remove it with **−**, or reset all of StreamForge's permissions: `tccutil reset All io.github.lnkzw.StreamForge`.

## Uninstall

Move `StreamForge.app` to the Trash, and remove the links if you made them:

```sh
rm ~/.local/bin/streamforge ~/.local/bin/streamforge-engine ~/.local/bin/streamforge-mcp
tccutil reset All io.github.lnkzw.StreamForge
```

The last line removes StreamForge's permissions. Projects and recordings stay where you saved them.

## License

StreamForge is licensed under either of the MIT license (`LICENSE-MIT`) or the Apache License 2.0 (`LICENSE-APACHE`), at your option. The desktop app embeds the IBM Plex fonts, licensed under the SIL Open Font License 1.1 (`LICENSE-OFL-IBM-Plex.txt`).

The package also contains FFmpeg, built with x264 and therefore under the GNU General Public License, version 2 or later (`licenses/FFmpeg`, `licenses/x264`). The programs in this package link that FFmpeg and also contain Rust libraries under the Apache License 2.0, which is compatible with version 3 of the GPL only; so the programs as a whole are distributed under the GNU General Public License, version 3 (`licenses/FFmpeg/COPYING.GPLv3`). The app keeps the same licences and `BUILD-INFO.txt` in `StreamForge.app/Contents/Resources`.

The corresponding source code:

- FFmpeg and x264: the archives attached to the same release as this package; `BUILD-INFO.txt` lists them with their checksums and FFmpeg's configuration, and `packaging/macos/build-ffmpeg.sh` builds them.
- StreamForge: the repository at https://github.com/lnkzw/streamforge, at the commit `BUILD-INFO.txt` names (for a release, the tag of its version), with its build and packaging scripts.
- The Rust libraries: the exact versions in that commit's `Cargo.lock`, from https://crates.io; `cargo vendor` in a checkout of it downloads all of them.
