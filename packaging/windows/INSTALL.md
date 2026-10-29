# Installing StreamForge on Windows

This package is for 64-bit Windows 10 (version 1903 or newer) and Windows 11 on x86_64. It brings everything it needs: the programs, the FFmpeg libraries they use (built for this package, see `BUILD-INFO.txt`) and no other runtime. Hardware encoding uses the graphics driver's encoder (NVIDIA NVENC, Intel Quick Sync or AMD AMF) when it works, and otherwise x264 on the CPU.

## Install

1. Check the download and unpack it, in PowerShell in the folder you downloaded it to:

   ```powershell
   (Get-FileHash streamforge-*-x86_64-windows.zip).Hash.ToLower()
   Get-Content streamforge-*-x86_64-windows.zip.sha256
   Expand-Archive streamforge-*-x86_64-windows.zip -DestinationPath .
   cd streamforge-*-x86_64-windows
   ```

   The two hashes must be the same.

2. Install for your user (no administrator rights needed):

   ```powershell
   powershell -ExecutionPolicy Bypass -File install.ps1
   ```

   Windows runs downloaded scripts only when allowed; `-ExecutionPolicy Bypass` allows this one, this time, and changes no setting.

`install.ps1` installs into `%LOCALAPPDATA%\Programs\StreamForge`:

- `bin\`: the programs and the FFmpeg libraries
  - `streamforge.exe`: command line (system checks, test recordings and streams, `project validate`)
  - `streamforge-desktop.exe`: desktop app
  - `streamforge-engine.exe`: headless engine for scripts (JSON lines on stdin and stdout)
  - `streamforge-mcp.exe`: MCP server for AI agents
- this file, the README, the documentation (`docs\`), the licences (`licenses\` and `LICENSE-*`) and `BUILD-INFO.txt`
- a Start menu entry *StreamForge*, which starts the desktop app in your home folder (recordings and projects go there unless you choose another folder)

The installer prints how to add `bin\` to your `PATH`, so you can type `streamforge status` in a new console; it does not change `PATH` itself. `-Prefix DIR` installs into another folder, and `-Uninstall` (with the same `-Prefix`) removes what it installed and the Start menu entry. Installing a new version over an old one replaces it. Projects and recordings stay where you saved them.

The programs also run straight from the unpacked folder, without installing: `.\bin\streamforge.exe status`.

Start with the user guide, `docs\user-guide.md`.

## Warnings from Windows

The package is not signed, so Windows does not know its publisher:

- **SmartScreen** may show "Windows protected your PC" when you first start a program. Choose **More info**, then **Run anyway**.
- Some antivirus programs check unknown programs before they start them the first time, which can take a few seconds.

## The desktop app and the console

`streamforge-desktop.exe` is a window program: started from the Start menu, it opens no console window. Started from a console, `--version`, `--help` and startup errors print in that console, after the prompt has returned (the console does not wait for window programs). Once its window is open, the app no longer uses the console: Ctrl+C there does not reach it, and closing the window ends it (asking first while it records or streams).

## License

StreamForge is licensed under either of the MIT license (`LICENSE-MIT`) or the Apache License 2.0 (`LICENSE-APACHE`), at your option. The desktop app embeds the IBM Plex fonts, licensed under the SIL Open Font License 1.1 (`LICENSE-OFL-IBM-Plex.txt`).

The package also contains FFmpeg, built with x264 and therefore under the GNU General Public License, version 2 or later (`licenses\FFmpeg`, `licenses\x264`), together with libvpl, the NVENC headers and the AMF headers under the MIT license (`licenses\`). The binaries in this package, which link that FFmpeg, are distributed under the GPL as a whole. The exact sources of FFmpeg and of everything built into it are attached to the same release as this package, `BUILD-INFO.txt` lists them with their checksums and FFmpeg's configuration, and `packaging/windows/build-ffmpeg.sh` in the StreamForge repository builds them. The StreamForge source code is at https://github.com/lnkzw/streamforge.
