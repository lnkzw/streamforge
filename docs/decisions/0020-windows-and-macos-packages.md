# ADR 0020: Windows and macOS packages

- **Status:** Accepted (plan reviewed with the project owner on 2026-10-29)
- **Date:** 2026-10-29

## Context

Versions 0.1.0 and 0.2.0 ship Linux packages only. Windows and macOS build from source and pass the test suite in CI (ADR 0009), but nobody has run StreamForge on a real Windows PC or Mac yet. The README promises packages for them after tests on real machines. Fourteen manual tests wait in the hardware backlog: audio devices, the monitor, desktop audio, hardware encoders, the desktop UI, quit signals, layers, images and cameras.

What the project owner has (2026-10-29): a MacBook Pro with an M4 (a work machine), a MacBook Pro with an Intel CPU from 2016, and a Windows PC. Rust builds on all three. The 2016 MacBook Pro runs macOS 12 at most, below the macOS 14 that StreamForge needs (ScreenCaptureKit's picker, process taps for desktop audio), so the M4 is the only Mac that can test.

What is missing for a package:

- **Windows:** the desktop is a console program, so starting it from the Start menu also opens a console window. Its executable has no icon or version information. The binaries need the FFmpeg DLLs next to them. Rust's MSVC binaries link the Visual C++ runtime dynamically, which not every PC has.
- **macOS:** there is no app bundle, so the desktop has no Dock icon or name, and macOS has no usage descriptions to show when it asks for the camera or the microphone. The binaries link FFmpeg from Homebrew, which lives outside any bundle and whose libraries require the macOS version they were built on. An app opened from Finder runs in `/`, so the desktop's default folders (the working directory) would not be writable.
- **macOS run loop:** the CLI, the engine process and the MCP server have no main run loop. Whether AVFoundation notices an unplugged camera, or lists a newly plugged one, without it is unverified. ScreenCaptureKit's picker needs the main thread with a running run loop.
- **macOS picker:** screen sources come only from `list_capture_targets`, which needs the screen-recording permission. macOS 15 asks again every month for apps that capture without the system picker (`SCContentSharingPicker`), Apple's intended path.
- **FFmpeg licence:** a package that carries FFmpeg distributes it. With libx264, FFmpeg is GPL (ADR 0005), so the package must come with the corresponding source.

## Decision

### Common to every package

- **Version 0.3.0** releases Linux, Windows and macOS together. The Linux packages stay as they are.
- **Unsigned:** no paid code signing in M15. Windows SmartScreen warns about an unknown publisher; macOS Gatekeeper blocks a downloaded app until it is allowed in System Settings → Privacy & Security ("Open Anyway"). The user guide shows both. Because a macOS app is signed ad hoc, macOS treats every new version as a new app and asks for its permissions again.
- **Contents:** the four binaries, the FFmpeg libraries they link, `README.md`, `CHANGELOG.md`, an `INSTALL.md` for the system, `docs/`, the licences (ours, IBM Plex's, FFmpeg's and x264's) and a `BUILD-INFO.txt` naming the commit, the toolchain and the FFmpeg build.
- **Smoke test** in the release workflow on the system the package is for, as on Linux: each binary answers `--version`, `streamforge status --json` and the engine's `get_status` work, the packaged CLI records two seconds of the test pattern with libx264 (`--video-encoder software`), and the installer installs into a fresh folder and uninstalls without leftovers.
- **Draft first:** a tag creates a draft release with all packages; the project owner installs the Windows and macOS packages from it before it is published.

### FFmpeg in the packages

- **Our own build** on Windows and macOS: FFmpeg 8.1 and x264 from pinned source archives (checksums in the repository), built in the release workflow and cached by the build script's hash. A minimal configuration: the H.264 encoders the engine tries (libx264, and NVENC, Quick Sync and AMF on Windows, VideoToolbox on macOS), the AAC encoder, the Matroska and FLV muxers, RTMP and RTMPS, and swscale; no programs, no other codecs. Shared libraries, so the four binaries share one copy.
- **TLS for RTMPS:** the system's TLS where FFmpeg 8.1 supports it (Schannel on Windows; SecureTransport on macOS if still offered), otherwise a TLS library under GPL-compatible terms; the step that builds it names the choice.
- **Source:** each release carries every source archive the build used as assets (FFmpeg, x264, the hardware encoders' headers and libraries, a TLS library if any), and `BUILD-INFO.txt` holds the configure line. The StreamForge source is the repository at the tag.
- **Why not the prebuilt Windows build CI uses** (BtbN's `ffmpeg-n8.1-latest-win64-gpl-shared`): it is rebuilt in place under a moving `latest` release, so the exact build and its sources (dozens of libraries) cannot be pinned or offered later, and it is far larger than needed. CI keeps it to test the code against a full FFmpeg; the release smoke test covers the build we ship.
- **Licence:** the packaged binaries fall under the GPL (version 2 or later with x264 and system TLS; version 3 if the TLS library needs it), as ADR 0005 says for any GPL FFmpeg build. The sources remain MIT OR Apache-2.0.

### Windows package

- **Archive:** `streamforge-0.3.0-x86_64-windows.zip` with the binaries and FFmpeg DLLs in `bin\` and the documents beside it.
- **No Visual C++ runtime needed:** the binaries link the C runtime statically (`+crt-static`); the smoke test checks that they import only system DLLs and the bundled FFmpeg.
- **Installer `install.ps1`:** per user, without administrator rights, into `%LOCALAPPDATA%\Programs\StreamForge` (or `-Prefix`); a Start-menu shortcut to the desktop that starts in the user's home folder; `-Uninstall` removes what it installed. It prints how to add `bin\` to `PATH`, as `install.sh` does, and changes nothing else. Running a downloaded script needs `powershell -ExecutionPolicy Bypass -File install.ps1`; `INSTALL.md` says so.
- **Desktop window program:** `streamforge-desktop.exe` uses the Windows subsystem, so no console window opens. Started from a console, it attaches to it, so `--version`, `--help` and startup errors still print there. Ctrl+C in that console no longer applies to it; closing the window does (backlog item 10 changes accordingly).
- **Resources:** an icon (the app icon as `.ico`, rendered from its SVG and committed) and version information embedded through a build script, with the new build dependency `winresource`, used only when building for Windows.

### macOS package

- **App bundle** `StreamForge.app`, for Apple silicon (arm64) only, in `streamforge-0.3.0-arm64-macos.zip` with the documents beside it.
  - `Contents/MacOS/`: the desktop (the bundle's executable) and the CLI, the engine and the MCP server, which run from there or through a symbolic link (`INSTALL.md`).
  - `Contents/Frameworks/`: the FFmpeg libraries, found through `@rpath`.
  - `Contents/Resources/`: the icon (`.icns`, rendered from the SVG and committed).
  - `Info.plist`: bundle id `io.github.lnkzw.StreamForge` (the app id), version, `LSMinimumSystemVersion` 14.0, and the usage descriptions macOS shows when it asks for the camera, the microphone and system audio (`NSCameraUsageDescription`, `NSMicrophoneUsageDescription`, `NSAudioCaptureUsageDescription`).
- **macOS 14 or newer:** every binary and library is built for 14.0 (`MACOSX_DEPLOYMENT_TARGET`); the smoke test checks each one's minimum version and runs the package on a macOS 14 runner as well as on the one that built it.
- **Signed ad hoc**, inside out (libraries, then binaries, then the bundle), without the hardened runtime, which would need entitlements and only matters for notarization.
- **Permissions:** started from Finder or the Dock, the app asks for itself and System Settings lists "StreamForge". The CLI, engine and MCP server ask for the app that started them (the terminal, or an MCP client), as today. An MCP client without a camera usage description can make macOS end the process when it asks; the user guide says so, and a fix waits for the hardware test to show it.
- **Intel Macs** build from source; CI keeps testing on `macos-15-intel`. An Intel package would be tested on no real machine.

### Default folders

The desktop's `--output-dir` and `--project-dir` default to the working directory. From the Start-menu shortcut that is the home folder, as from Linux's application menu. A macOS app opened from Finder starts in `/`: there, and only when the working directory is the root, the desktop uses the home folder instead. Settings that remember other folders are not part of M15.

### macOS: a main run loop for every program

- The desktop has one through eframe. The CLI, the engine process and the MCP server gain one: on macOS their work moves to a thread of its own and the main thread runs the main run loop until that thread ends, then the program exits with its result. A function in `capture-platform` (the crate every binary already composes with) does this; on Linux and Windows it calls the work directly.
- Nothing else changes: the protocol, the quit signals and the outputs behave as before. This is its own step, so that a regression shows apart from the picker.

### macOS: the system picker

- **Capability:** on macOS 14 or newer, `get_capture_capabilities` reports `picker: true` next to `target_list: true` in every program that runs a main run loop (the desktop, and after the run-loop step all of them).
- **Use:** a screen source, probe or recording without `capture_target_id` opens `SCContentSharingPicker` on the main thread, limited by `target_kinds` (a display, a window, or either). The rules are the Linux picker's (ADR 0010): the engine waits up to 120 s for a human, a cancel is `cancelled`, a picked source is not saved in a project (it has no capture target to save), and adding one while an output runs is refused.
- **Desktop:** the add-source dialog on macOS offers "Choose in the system dialog" above the list.
- **Protocol:** additive. A request that failed before (`target_list` needed a capture target) now opens the picker.
- **Permission:** Apple offers the picker as the way to capture without the blanket screen-recording permission. Whether capture through it works without that permission, and whether macOS 15's monthly prompt then stays away, is checked on the M4; the docs state what the test shows.

### CI

- `ci.yml` adds `macos-15` (Apple silicon) to the test matrix, beside `macos-15-intel`. Tests that need the runner's screen-recording permission adapt if that image does not grant it.
- `release.yml` adds a Windows job (`windows-latest`), a macOS job (`macos-15`, arm64) and a macOS 14 smoke job (`macos-14`) that runs the macOS package it did not build; the draft release waits for all of them.

## Alternatives considered

- **An installer (MSI through WiX, Inno Setup) on Windows:** familiar, but a second toolchain for what a zip and a short script do; a per-user script also needs no administrator rights. It can come with winget later.
- **A disk image on macOS:** the familiar drag-to-Applications window, but a zip holds the same bundle and matches the Windows archive.
- **Homebrew's FFmpeg in the bundle:** quick to set up, but about forty libraries, each requiring the macOS version of the runner that built it, so no macOS 14.
- **Static FFmpeg:** no library paths to fix, but four copies of FFmpeg, one per binary.
- **A universal (arm64 + x86_64) app:** one download for every Mac, but it doubles the FFmpeg build and the Intel half would be tested on no real machine.
- **Notarization (Apple Developer Program) and a Windows signing certificate:** no Gatekeeper or SmartScreen warning, but yearly costs and secrets in CI; the project owner chose unsigned packages for now.
- **Windows' `GraphicsCapturePicker` in M15:** it needs an owner window and a message loop, and the target list already works there without any permission. Later.
- **A picker only in the desktop, without a run loop in the other programs:** smaller, but the CLI, the engine and the MCP server would keep the unverified camera hot-plug and no picker.

## Validation (planned)

- **Release workflow, per package:** the smoke test above. Windows: imported DLLs checked with `dumpbin /dependents`, the Start-menu shortcut created and removed. macOS: `codesign --verify --deep --strict`, every Mach-O's minimum version at most 14.0, no library linked from outside the bundle or the system (`otool -L`), the CLI run through a symbolic link, the package run on `macos-14`.
- **Unit tests:** the default-folder rule; the run-loop function returns the work's result and exit code, on every OS; the picker's mapping from `target_kinds` to the picker's modes; the capability per macOS version.
- **CI on macOS:** the capability, the picker's error paths, and all existing tests with the run loop in place.
- **Hardware test round** on the Windows PC and the M4, from the draft packages: the backlog items 1–14, installing and starting each package (warnings, Start menu, Dock, permissions under the app's name), the macOS picker with and without the screen-recording permission, a camera unplugged and plugged in while the engine process runs, and the hardware encoders (NVENC, Quick Sync or AMF; VideoToolbox). Findings are fixed in their own pull requests before the release.

## Implementation steps (one PR each)

1. This ADR and the roadmap.
2. **Windows package:** the FFmpeg build, the static C runtime, the window program and its resources, `install.ps1`, the release job with its smoke test, `INSTALL.md`.
3. **macOS package:** the FFmpeg build, the app bundle, the default-folder rule, ad-hoc signing, the release jobs (arm64 and the macOS 14 smoke), the arm64 CI job, `INSTALL.md`.
4. **macOS run loop and picker:**
   - **4a:** the main run loop for the CLI, the engine process and the MCP server, without behaviour change.
   - **4b:** `SCContentSharingPicker` in `capture-macos`, the capability, the desktop's dialog.
5. **Hardware test round** with the project owner, with fixes in their own pull requests.
6. **Release 0.3.0:** changelog, README, user guide per system, the draft packages checked on the Windows PC and the M4, then published.

## Decisions taken with the project owner

On 2026-10-29:

1. M15 is Windows and macOS, ending in release 0.3.0 for all three systems.
2. The macOS package is for Apple silicon only; the 2016 MacBook Pro is below macOS 14.
3. No test kit: the project owner runs the backlog's `cargo test` items on the machines themselves.
4. FFmpeg for macOS is built in the release workflow, not taken from Homebrew. (The same build for Windows replaces the prebuilt one in this ADR, for the reasons above.)
5. No paid code signing; the packages are signed ad hoc (macOS) or not at all (Windows).

Proposed with the plan and not objected to: no Windows picker in M15; the MCP server's macOS permissions documented first; restarting the monitor after a macOS rate change, and the desktop-audio downmix beyond two channels, only if the hardware test shows the need; `macos-15` in CI; everything additive to protocol v1.

## Not in M15

Windows' system picker; a downmix of desktop audio beyond two channels; restarting the monitor on the same device after a rate change (unless the hardware test needs it); code signing and notarization; an installer, winget or Homebrew cask; an Intel or universal macOS package; settings that remember folders; zero-copy capture into the encoder.

## Notes from the implementation

Step 2 (Windows package, 2026-10-30):

- **FFmpeg build:** cross-compiled with mingw-w64 on Ubuntu, not with MSYS2 on Windows. mingw's FFmpeg build writes import libraries the MSVC linker takes, the Linux runner is faster, and the script runs on any Linux machine (in an `ubuntu:24.04` container). The win32 thread model and static libgcc and libstdc++ keep the mingw runtime out of the DLLs. The TLS choice is Schannel.
- **Licence:** the Decision's "version 2 or later" holds for FFmpeg and x264, but the programs also contain Rust crates under the Apache License 2.0 alone (cpal, winit, rmcp and others), which only version 3 of the GPL can be combined with. The Windows package therefore distributes its programs under the GPL version 3 and carries its text; FFmpeg's own licence files stay beside it.
- **Source:** beside the FFmpeg archives attached to the release and the repository at the tag, the Rust crates the programs contain are the versions in `Cargo.lock` from crates.io; the package's `INSTALL.md` gives these directions.
- **x264** comes from snapshot.debian.org: the Debian pool drops an upstream tarball once no suite has that version.
- **Cache:** the FFmpeg build is cached by its script and sources, which serves reruns of a pull request; a tag run builds it again from the same sources, since caches made for pull requests are not shared with tags.

Step 3 (macOS package, 2026-10-30):

- **FFmpeg build:** native on `macos-15` (Apple silicon) with Xcode's clang, from the same pinned archives (FFmpeg and x264 only), for macOS 14.2 (below), with `--install-name-dir=@rpath`; four libraries of about 2.5 MB in under two minutes. The TLS choice is Secure Transport: Apple has deprecated it since macOS 10.15 in favour of Network.framework, for which FFmpeg 8.1 has no backend, but macOS 26 still has it. Should it go, OpenSSL 3 (Apache License 2.0, compatible with the GPL version 3 the programs are distributed under) would replace it.
- **Licence:** the same as on Windows; the programs as a whole are distributed under the GPL version 3.
- **No installer script:** the app bundle is the installation, moved to Applications by the user; the command-line programs run through symbolic links (`INSTALL.md`). Instead of installing and uninstalling, the smoke test copies the app into a folder with a space and runs the programs through symbolic links: dyld resolves `@executable_path` through them. The licences and `BUILD-INFO.txt` are also in `Contents/Resources`, since users keep the bundle, not the folder around it.
- **Run paths:** the binaries keep only `/usr/lib/swift` (from `.cargo/config.toml`; the Swift runtime is linked by absolute path anyway) and gain `@executable_path/../Frameworks`; the package fails on any other run path or linked library outside the bundle and the system.
- **Icon:** `macos.icns` is rendered from the app icon's SVG at the size of Apple's icon grid (824 of 1024 pixels), with Pillow.
- **Default folders:** the rule (the home folder when the working directory is the root) applies on every system, which keeps it one tested function; on Windows the home folder comes from `userenv.dll`, which the package's import check now allows.
- **CI:** on `macos-15`, every test passed on the first run, VideoToolbox opens, and the screen-recording permission is granted. The first `macos-14` job was never picked up ("not acquired by Runner ... after multiple attempts", arm64 capacity); a rerun of the job is the remedy, also before a draft release.
- **Minimum macOS 14.2, not 14.0:** cpal 0.18's Core Audio host, which every program uses, links Core Audio's process-tap functions (new in macOS 14.2) directly rather than weakly, so no StreamForge program, packaged or built from source, starts on 14.0 or 14.1. The package therefore sets 14.2 as its deployment target and `LSMinimumSystemVersion`, and its check allows no minimum newer than 14.2; this replaces 14.0 in the Decision and in the Validation above. Every macOS 14 updates to 14.2 for free. The `macos-14` runner has a later 14.x, so the exact minimum is not run anywhere.
- **Hardened runtime:** besides notarization, it adds runtime protections; it is left out because it needs entitlements for the camera, the microphone and audio input that have to be worked out and tested on a real Mac.

Licences of the Rust crates (2026-10-30, after step 3):

- **Every package** (Linux, Windows and macOS; the macOS app also in `Contents/Resources`) carries `LICENSE-RUST-CRATES.txt`: the licence files of the Rust crates the programs are built from, as each crate ships them, since MIT, BSD, ISC and the fonts' licences require their notices in binary copies; for the few crates that ship none, the notice from their repository, and the notices some crates keep in their source files for code or documents taken from others (`packaging/licenses/notices`; source files are not searched for them, so a new dependency's headers are checked by hand); and the copyright notices of the Wayland protocol descriptions the Wayland crates generate code from. The 0.1.0 and 0.2.0 Linux packages lacked it. It is generated from `Cargo.lock` with cargo-about 0.9.2 (`packaging/licenses/generate.py`), which reads each crate's own files and fails on a licence outside its list (all compatible with version 3 of the GPL), and is committed, so a dependency change shows in its diff and packaging needs no extra tool. CI fails when it is out of date. One file covers the three systems; a package may not contain every crate it lists. The fonts egui embeds (Hack, the emoji icon font) are under licences their crate's licence field does not name, so `about.toml` adds their files, pinned by checksum; cargo-about only warns when such a file changes, so the script fails on anything it reports.

Step 4a (macOS main run loop, 2026-10-30):

- **`capture_platform::run_with_main_run_loop(work)`**, with the macOS part in `capture-macos` (`run_loop.rs`): on the main thread, it runs `work` on a scoped thread named `work` with the main thread's 8 MiB stack and runs `CFRunLoopRun` until that thread has ended; a panic is resumed on the main thread, so the exit status (101) stays. Called off the main thread it runs the work there; on Linux and Windows it runs the work directly. Only safe objc2 calls, no `unsafe`.
- **Stopping:** a supervisor thread, started first, starts the work thread and joins it, so the main run loop still runs while the work thread's thread-local destructors run (they may wait for the main queue). Then, also when the work thread cannot start or the work panics, it stops the main run loop through the main dispatch queue, which runs only inside the loop, so a stop sent before the loop runs is not lost; that callback sets the flag the loop checks, on the main thread, so no stop stays queued after the call returns, and the loop runs again after any other stop. On the main thread, the main queue keeps the run loop's default mode from being empty, so `CFRunLoopRun` does not return at once.
- **Programs:** the CLI, the engine process and the MCP server call it from `main` with what `main` did before; clap's parsing and its exits (`--help`, errors) now happen on the work thread, which `exit` ends the process from as before; the quit-signal handlers and the second signal's exit stay on their own thread. The bench and soak tools are left as they are.
- **Test:** `capture-platform/tests/main_run_loop.rs` runs without the test harness, on the main thread, with a 60 s watchdog: the result comes back twice in a row, each time after a closure sent to the main queue ran on the main thread (macOS), and 100 times from work that ends at once; a panic reaches the caller; a thread-local destructor that waits for the main queue ends (macOS); off the main thread the work runs in place; and in child processes, `exit` from the work keeps its status and buffered output, and a panic exits with 101. The programs' own tests on the macOS runners cover the protocol, the first quit signal and their exit statuses; the second signal's exit has no test on any system.

Step 4b (macOS system picker, 2026-10-30):

- **`capture-macos/src/picker.rs`**, through `screencapturekit`'s `SCContentSharingPicker` (its Swift bridge presents it, observes it and makes the process a regular app while it shows): `picker` is `true` where the picker exists (macOS 14 or newer), off the main thread (a pick waits, and the main thread would wait for its own queue), and while a main run loop runs: the one `run_with_main_run_loop` runs (a flag set from before the work starts until it returns, so a pick at startup is not refused) or AppKit's (the main run loop has a current mode). The test harness has neither, so it is offered there no more than before.
- **Use:** `start` captures the named source as before, or, without `capture_target_id`, the filter the user picks. The configuration and the call go to the main queue, so they run on the main thread inside the loop. The modes follow `target_kinds`: a display, a window or either; changing the picked content later is off. The size comes from the filter as for a named source (pixels), else from the picker's result; the kind from what was picked.
- **Rules (ADR 0010):** the wait ends after 120 s with `timeout`, and the picker is set inactive, which should close it; a late answer is dropped, and a picker not yet sent to the main queue is not sent. Not the bridge's `deactivate()`: it also resets the bridge's count of sessions that keep the process a regular app, and the next pick's cleanup of the timed-out session would then take the new session's place, so the process could leave the regular state while the new picker shows. A timed-out session therefore keeps the process a regular app until the next pick replaces it. A cancel is `cancelled`; a picker that cannot be shown is `unavailable`, with the bridge's generic message (it drops the system's error). The bridge has one picker session per process, so one pick runs at a time; another during it is `unavailable`. The engine's rules need no change: a picked source is not saved and is refused while an output runs, on every system with a picker.
- **Desktop:** the screen source dialog names the choice "Choose in the system dialog" on Linux and macOS; Windows keeps its old label.
- **Limits:** closing the desktop while a pick waits waits for the pick to end (up to 120 s), as with the portal on Linux: the engine thread is joined.
- **Not verified:** whether the picker shows and its choice captures, in the desktop and in the CLI, the engine process and the MCP server, which are not AppKit apps; whether that needs the screen-recording permission; whether setting it inactive closes it; and macOS 15's monthly prompt. CI only tries to show it, where nobody picks, and checks that every wait ends; it cannot see whether the picker appeared. The hardware round (step 5) tries these on the M4.
