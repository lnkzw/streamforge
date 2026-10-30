#!/usr/bin/env bash
# Smoke-test an unpacked macOS package on this Mac (ADR 0020):
#
#   packaging/macos/smoke.sh <package-dir>
#
# <package-dir> holds StreamForge.app. package.sh runs this on the Mac
# that built the package, the release workflow again on macOS 14. Needs
# jq (part of macOS 15; the runners have it).
set -euo pipefail

dir=$(cd "${1:?usage: packaging/macos/smoke.sh <package-dir>}" && pwd)
app="$dir/StreamForge.app"
# The full version, as --version prints it (the bundle's leaves out a
# pre-release suffix).
version=$(sed -n 's/^StreamForge \([^ ]*\) for macOS.*/\1/p' "$dir/BUILD-INFO.txt")
sw_vers

codesign --verify --deep --strict --verbose=2 "$app"

# Each program starts and answers from the bundle, so with its libraries.
macos="$app/Contents/MacOS"
desktop_version=$("$macos/streamforge-desktop" --version)
if [ "$desktop_version" != "streamforge-desktop $version" ]; then
  echo "streamforge-desktop --version printed '$desktop_version'" >&2
  exit 1
fi
"$macos/streamforge" --version
"$macos/streamforge" status --json
response=$(printf '%s\n' '{"id":1,"command":"get_status"}' | "$macos/streamforge-engine")
echo "$response"
grep -qF '"id":1,"ok":true' <<< "$response"
"$macos/streamforge-engine" --version
"$macos/streamforge-mcp" --version

# Installed as INSTALL.md says: the app copied into another folder (here
# one with a space), the command-line programs run through symbolic links,
# which must still find the libraries in the bundle.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/Apps with space" "$tmp/bin"
ditto "$app" "$tmp/Apps with space/StreamForge.app"
codesign --verify --deep --strict "$tmp/Apps with space/StreamForge.app"
for bin in streamforge streamforge-engine streamforge-mcp; do
  ln -s "$tmp/Apps with space/StreamForge.app/Contents/MacOS/$bin" "$tmp/bin/$bin"
  "$tmp/bin/$bin" --version
done

# The packaged FFmpeg records two seconds of the test pattern with x264
# and the test tone with AAC.
report=$("$tmp/bin/streamforge" --output-dir "$tmp" record --file-name smoke.mkv \
  --duration-ms 2000 --video-encoder software)
echo "$report"
# At least a quarter of the frames and half the samples: a slow runner may
# drop some (counted).
if ! jq -e '.recording
    | (.duration_ms | type == "number" and . >= 1900)
      and .video.encoder == "libx264"
      and (.video.frames_encoded | type == "number" and . >= 30)
      and (.audio_tracks | length == 1)
      and .audio_tracks[0].encoder == "aac"
      and (.audio_tracks[0].samples_encoded | type == "number" and . >= 48000)' <<< "$report" > /dev/null; then
  echo "the recording is not two seconds of libx264 video (30 frames or more) and AAC audio" >&2
  exit 1
fi
bytes=$(stat -f %z "$tmp/smoke.mkv")
if [ "$bytes" -lt 10000 ]; then
  echo "the recording has only $bytes bytes" >&2
  exit 1
fi
echo "smoke test passed"
