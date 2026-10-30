#!/usr/bin/env bash
# Build the StreamForge macOS binaries in release mode, put them into
# StreamForge.app with the FFmpeg they link, sign it ad hoc, check it on
# this Mac and pack it (ADR 0020):
#
#   packaging/macos/package.sh <ffmpeg-dir>
#
# Run from the repository root on an arm64 Mac with Rust and Xcode 26 or
# newer (for screencapturekit's Swift bridge). <ffmpeg-dir> is the output
# of packaging/macos/build-ffmpeg.sh: the binaries link its libraries,
# which go into the bundle with its licences and build information.
# Writes dist/<name>.zip and dist/<name>.zip.sha256 (in the format
# sha256sum -c reads).
set -euo pipefail

ffmpeg=$(cd "${1:?usage: packaging/macos/package.sh <ffmpeg-dir>}" && pwd)
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
test -n "$version"
name="streamforge-$version-arm64-macos"
out="dist/$name"
app="$out/StreamForge.app"
contents="$app/Contents"
bins=(streamforge streamforge-engine streamforge-desktop streamforge-mcp)
app_id=io.github.lnkzw.StreamForge
# The oldest macOS the package runs on; every binary and library must
# require no newer one. 14.2, not 14.0: cpal's Core Audio host links the
# process-tap functions of macOS 14.2 directly, so no StreamForge program
# starts on 14.0 or 14.1.
minimum=14.2

if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
  echo "package.sh: packs on an arm64 Mac only, not on $(uname -sm)" >&2
  exit 1
fi

MACOSX_DEPLOYMENT_TARGET=$minimum FFMPEG_DIR="$ffmpeg" cargo build --release --locked \
  -p streamforge-cli -p streamforge-engine -p streamforge-desktop -p streamforge-mcp

rm -rf "$out" "dist/$name.zip" "dist/$name.zip.sha256"
mkdir -p "$contents/MacOS" "$contents/Frameworks" "$contents/Resources"
# The desktop is the bundle's executable; the other programs run from
# beside it or through a symbolic link (INSTALL.md).
for bin in "${bins[@]}"; do
  install -m 0755 "target/release/$bin" "$contents/MacOS/"
done
# FFmpeg's libraries under the names the binaries link them by (their
# install names, @rpath/<name>), without the linker's symbolic links.
for lib in "$ffmpeg/lib/"lib*.dylib; do
  [ -L "$lib" ] && continue
  id=$(otool -D "$lib" | tail -n +2)
  if [[ $id != @rpath/lib*.dylib ]]; then
    echo "package.sh: $(basename "$lib") has the install name '$id', not @rpath/<name>" >&2
    exit 1
  fi
  install -m 0644 "$lib" "$contents/Frameworks/${id#@rpath/}"
done
# macOS compares bundle versions as up to three integers, so a pre-release
# suffix (0.3.0-rc.1) is left out there; BUILD-INFO.txt has the full one.
bundle_version=${version%%-*}
if ! [[ $bundle_version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "package.sh: cannot make a bundle version from $version" >&2
  exit 1
fi
sed -e "s/@VERSION@/$bundle_version/g" -e "s/@MINIMUM_SYSTEM_VERSION@/$minimum/g" \
  packaging/macos/Info.plist > "$contents/Info.plist"
if grep -q @ "$contents/Info.plist"; then
  echo "package.sh: a placeholder is left in Info.plist" >&2
  exit 1
fi
plutil -lint "$contents/Info.plist"
install -m 0644 crates/streamforge-desktop/assets/icon/macos.icns "$contents/Resources/StreamForge.icns"

cp LICENSE-MIT LICENSE-APACHE README.md CHANGELOG.md packaging/macos/INSTALL.md "$out/"
# The documentation the README and the user guide link to, at the same
# relative paths as in the repository.
cp -R docs "$out/"
# The desktop binary embeds IBM Plex, whose licence must travel with it,
# as must those of the Rust crates in the binaries; FFmpeg's build brings
# its own and those of what is built into it.
cp crates/streamforge-desktop/assets/fonts/OFL.txt "$out/LICENSE-OFL-IBM-Plex.txt"
cp packaging/licenses/rust-crates.txt "$out/LICENSE-RUST-CRATES.txt"
cp -R "$ffmpeg/licenses" "$out/"

# Every relative link in the user's entry points must resolve inside the
# package (they are read there, not on GitHub).
broken=0
for doc in "$out/README.md" "$out/CHANGELOG.md" "$out/INSTALL.md" "$out/docs/user-guide.md"; do
  dir=$(dirname "$doc")
  while IFS= read -r target; do
    target=${target%%#*}
    if [ -n "$target" ] && [ ! -e "$dir/$target" ]; then
      echo "broken link in ${doc#"$out"/}: $target" >&2
      broken=1
    fi
  done < <(grep -oE '\]\([^)[:space:]]+\)' "$doc" | sed -E 's/^\]\(//; s/\)$//' | grep -vE '^[a-z]+:' || true)
done
if [ "$broken" -ne 0 ]; then
  exit 1
fi

# The run paths a Mach-O file searches for @rpath/ libraries, one per line.
rpaths() {
  otool -l "$1" | awk '/cmd LC_RPATH/ { getline; getline; sub(/^ *path /, ""); sub(/ \(offset [0-9]+\)$/, ""); print }'
}

# The binaries find FFmpeg in the bundle. Besides that, they need only the
# system's Swift runtime (/usr/lib/swift, from .cargo/config.toml); any
# other run path would point into the build machine, so it goes.
for bin in "${bins[@]}"; do
  file="$contents/MacOS/$bin"
  while IFS= read -r rpath; do
    if [ "$rpath" != /usr/lib/swift ]; then
      install_name_tool -delete_rpath "$rpath" "$file"
    fi
  done < <(rpaths "$file")
  install_name_tool -add_rpath @executable_path/../Frameworks "$file"
done

# Every Mach-O file is for arm64 and the minimum macOS, links only the
# system's libraries and the bundled FFmpeg (or the system's Swift runtime
# through /usr/lib/swift), and searches nothing outside the bundle and the
# system.
bad=0
for file in "$contents/MacOS/"* "$contents/Frameworks/"*; do
  short=${file#"$contents"/}
  archs=$(lipo -archs "$file")
  if [ "$archs" != arm64 ]; then
    echo "$short is for '$archs', not arm64" >&2
    bad=1
  fi
  minos=$(otool -l "$file" | awk '/cmd LC_BUILD_VERSION/ { found = 1 } found && $1 == "minos" { print $2; exit }')
  # Compared as major, minor and patch; an unreadable version fails.
  if ! awk -v v="$minos" -v m="$minimum" 'BEGIN {
      if (v !~ /^[0-9]+(\.[0-9]+)?(\.[0-9]+)?$/) exit 1
      split(v, a, "."); split(m, b, ".")
      for (i = 1; i <= 3; i++) { if (a[i] + 0 < b[i] + 0) exit 0; if (a[i] + 0 > b[i] + 0) exit 1 }
      exit 0
    }'; then
    echo "$short requires macOS '$minos', newer than $minimum" >&2
    bad=1
  fi
  # Captured first, so a failing otool stops the script; every one links
  # libSystem.
  deps_of=$(otool -L "$file" | tail -n +2 | sed -E 's/^[[:space:]]+//; s/ \(compatibility.*$//')
  if ! grep -q '^/usr/lib/libSystem\.B\.dylib$' <<< "$deps_of"; then
    echo "$short: no libraries read from otool" >&2
    bad=1
  fi
  echo "$short (arm64, macOS $minos): $(tr '\n' ' ' <<< "$deps_of")"
  while IFS= read -r dep; do
    case $dep in
      /usr/lib/* | /System/Library/*) ;;
      @rpath/libswift*.dylib) ;;
      @rpath/lib*.dylib)
        if [ ! -f "$contents/Frameworks/${dep#@rpath/}" ]; then
          echo "$short links $dep, which is not in Contents/Frameworks" >&2
          bad=1
        fi
        ;;
      *)
        echo "$short links $dep, which is neither bundled nor part of macOS" >&2
        bad=1
        ;;
    esac
  done <<< "$deps_of"
  expected=
  if [[ $short == MacOS/* ]]; then
    expected=$'/usr/lib/swift\n@executable_path/../Frameworks'
  fi
  found=$(rpaths "$file")
  if [ "$found" != "$expected" ]; then
    echo "$short searches $(tr '\n' ' ' <<< "$found"), expected $(tr '\n' ' ' <<< "$expected")" >&2
    bad=1
  fi
done
if [ "$bad" -ne 0 ]; then
  exit 1
fi

# What this package was built from and with, for bug reports.
{
  echo "StreamForge $version for macOS (arm64, $minimum or newer)"
  echo "commit: $(git rev-parse HEAD)"
  echo "rustc: $(rustc --version)"
  echo "built on: macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)), $(xcodebuild -version | head -1)"
  echo "signature: ad hoc (not notarized)"
  echo "licence: GPL version 3 for the programs as a whole (INSTALL.md)"
  echo
  cat "$ffmpeg/BUILD-INFO.txt"
} > "$out/BUILD-INFO.txt"
cat "$out/BUILD-INFO.txt"
# The installed app keeps its licences and build information: users copy
# the bundle, not the folder around it.
cp -R "$out/licenses" "$out/LICENSE-MIT" "$out/LICENSE-APACHE" "$out/LICENSE-OFL-IBM-Plex.txt" \
  "$out/LICENSE-RUST-CRATES.txt" "$out/BUILD-INFO.txt" "$contents/Resources/"

# Signed ad hoc from the inside out: the libraries, the other programs,
# then the bundle with its executable. No hardened runtime: its runtime
# protections need entitlements (camera, microphone, audio input) to be
# worked out and tested, and without notarization nothing requires it yet
# (ADR 0020). Extended attributes would break the strict check.
xattr -cr "$app"
for lib in "$contents/Frameworks/"*.dylib; do
  codesign --force --sign - --timestamp=none "$lib"
done
for bin in "${bins[@]}"; do
  if [ "$bin" != streamforge-desktop ]; then
    codesign --force --sign - --timestamp=none --identifier "$app_id.$bin" "$contents/MacOS/$bin"
  fi
done
codesign --force --sign - --timestamp=none "$app"
codesign --verify --deep --strict --verbose=2 "$app"
signature=$(codesign --display --verbose=2 "$app" 2>&1)
echo "$signature"
grep -Fqx "Identifier=$app_id" <<< "$signature"
grep -Fqx 'Signature=adhoc' <<< "$signature"

packaging/macos/smoke.sh "$out"

# ditto keeps the bundle as it is; no resource forks or extended
# attributes of this Mac go into the archive.
(cd dist && ditto -c -k --norsrc --noextattr --noacl --keepParent "$name" "$name.zip")
(cd dist && shasum -a 256 "$name.zip" > "$name.zip.sha256")
cat "dist/$name.zip.sha256"
