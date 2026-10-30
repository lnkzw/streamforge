#!/usr/bin/env bash
# Build the FFmpeg libraries the macOS package ships (ADR 0020):
#
#   packaging/macos/build-ffmpeg.sh <out-dir>
#
# FFmpeg with libx264, VideoToolbox and Secure Transport, from the pinned
# archives in packaging/ffmpeg/sources.txt, for Apple silicon (arm64) and
# macOS 14.2 or newer (as package.sh); the release workflow runs it on
# macos-15. Only what
# the engine uses is enabled: the H.264 encoders it tries, AAC, the
# Matroska and FLV muxers, the file and RTMP(S) protocols and swscale.
# Shared libraries whose install names start with @rpath, so the package
# puts them into the app bundle's Contents/Frameworks and the binaries,
# linked through FFMPEG_DIR, find them there.
#
# Writes into <out-dir>:
#   lib/            the libraries (with the symbolic links the linker uses)
#   include/        the headers, for ffmpeg-sys-next's bindings
#   licenses/       the licences of FFmpeg and of x264
#   sources/        every source archive the build used
#   BUILD-INFO.txt  the sources, the toolchain and FFmpeg's configure line
#
# Needs the Xcode command line tools (clang, otool), pkg-config, curl and
# xz support in tar, on an arm64 Mac. Archives already in <out-dir>/sources
# are reused when their checksums match.
set -euo pipefail

out=${1:?usage: packaging/macos/build-ffmpeg.sh <out-dir>}
repo=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)

if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
  echo "build-ffmpeg.sh: builds on an arm64 Mac only, not on $(uname -sm)" >&2
  exit 1
fi

ffmpeg="ffmpeg-8.1.3"
x264="x264-0.165.3223+git20250910.0480cb0"
archives=(
  "$ffmpeg.tar.xz"
  "x264-0.165.3223-git20250910-0480cb0.tar.xz"
)

# The minimum macOS of everything built here. package.sh checks it in each
# library's and binary's build version (what the linker records), which
# does not show an object file compiled for a newer one; the flags below
# and MACOSX_DEPLOYMENT_TARGET are what keep those at the minimum.
minimum=14.2
export MACOSX_DEPLOYMENT_TARGET=$minimum
flags="-arch arm64 -mmacosx-version-min=$minimum"
jobs=$(sysctl -n hw.ncpu)

rm -rf "${out:?}/lib" "${out:?}/include" "${out:?}/licenses" "${out:?}/BUILD-INFO.txt"
"$repo/packaging/ffmpeg/fetch-sources.sh" "$out/sources" "${archives[@]}"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
deps="$work/deps"
for archive in "${archives[@]}"; do
  tar -C "$work" -xf "$out/sources/$archive"
done

# x264: a static library, built into libavcodec. 8-bit only, as the
# engine encodes yuv420p or nv12.
x264_configure=(
  --enable-static --disable-cli --bit-depth=8
  --disable-lavf --disable-swscale --disable-ffms --disable-gpac --disable-lsmash
)
(
  cd "$work/$x264"
  ./configure --prefix="$deps" "${x264_configure[@]}" --extra-cflags="$flags" --extra-ldflags="$flags"
  make -j"$jobs"
  make install-lib-static
)

# No autodetection: nothing from Homebrew or elsewhere on the build
# machine gets in; the Apple frameworks VideoToolbox needs (Core
# Foundation, Core Media, Core Video) are checked for by configure anyway.
# The TLS is the system's, Secure Transport (with tls_verify it checks the
# chain against the system's trusted certificates and the host name).
# shellcheck disable=SC2054 # the commas belong to the option values
configure=(
  --arch=arm64 --cc=clang
  --pkg-config=pkg-config --pkg-config-flags=--static
  --extra-cflags="$flags -I$deps/include" --extra-ldflags="$flags -L$deps/lib"
  --enable-shared --disable-static --enable-gpl --install-name-dir=@rpath
  --disable-autodetect --enable-videotoolbox --enable-securetransport
  --disable-programs --disable-doc --disable-avdevice --disable-avfilter --disable-swresample
  --disable-everything
  --enable-libx264
  --enable-encoder=libx264,h264_videotoolbox,aac
  --enable-muxer=matroska,flv
  --enable-protocol=file,tcp,tls,rtmp,rtmps
)
(
  cd "$work/$ffmpeg"
  if ! PKG_CONFIG_LIBDIR="$deps/lib/pkgconfig" ./configure --prefix="$work/ffmpeg" "${configure[@]}"; then
    # Why a check failed is only in the log, which goes with the work
    # directory.
    tail -n 60 ffbuild/config.log >&2
    exit 1
  fi
  make -j"$jobs"
  make install
)

# Everything the engine uses on macOS must be in the build (the encoders
# it tries, the muxers and protocols for recording and streaming), and the
# TLS must be Secure Transport.
for macro in CONFIG_LIBX264_ENCODER CONFIG_H264_VIDEOTOOLBOX_ENCODER CONFIG_AAC_ENCODER \
  CONFIG_MATROSKA_MUXER CONFIG_FLV_MUXER CONFIG_FILE_PROTOCOL CONFIG_TCP_PROTOCOL \
  CONFIG_TLS_PROTOCOL CONFIG_RTMP_PROTOCOL CONFIG_RTMPS_PROTOCOL; do
  if ! grep -q "^#define $macro 1\$" "$work/$ffmpeg/config_components.h"; then
    echo "build-ffmpeg.sh: $macro is not enabled" >&2
    exit 1
  fi
done
for macro in CONFIG_SWSCALE CONFIG_SECURETRANSPORT; do
  if ! grep -q "^#define $macro 1\$" "$work/$ffmpeg/config.h"; then
    echo "build-ffmpeg.sh: $macro is not enabled" >&2
    exit 1
  fi
done

cp -R "$work/ffmpeg/lib" "$work/ffmpeg/include" "$out/"
rm -rf "$out/lib/pkgconfig"

# The libraries name themselves and each other through @rpath and link
# only the system's libraries and frameworks otherwise (nothing from the
# build machine).
bad=0
for lib in "$out/lib/"lib*.dylib; do
  [ -L "$lib" ] && continue
  # Captured first, so a failing otool stops the script; the first line
  # is the file name, the second the library's own install name.
  deps_of=$(otool -L "$lib" | tail -n +2 | sed -E 's/^[[:space:]]+//; s/ \(compatibility.*$//')
  if ! grep -q '^/usr/lib/libSystem\.B\.dylib$' <<< "$deps_of"; then
    echo "$(basename "$lib"): no libraries read from otool" >&2
    bad=1
  fi
  while IFS= read -r dep; do
    case $dep in
      /usr/lib/* | /System/Library/*) ;;
      @rpath/lib*.dylib)
        if [ ! -f "$out/lib/${dep#@rpath/}" ]; then
          echo "$(basename "$lib") links $dep, which is not part of this build" >&2
          bad=1
        fi
        ;;
      *)
        echo "$(basename "$lib") links $dep, which is neither bundled nor part of macOS" >&2
        bad=1
        ;;
    esac
  done <<< "$deps_of"
done
if [ "$bad" -ne 0 ]; then
  exit 1
fi

mkdir -p "$out/licenses/FFmpeg" "$out/licenses/x264"
# GPLv3 too: the programs that link this build also contain Rust crates
# under Apache-2.0 alone, which only version 3 of the GPL can combine with.
cp "$work/$ffmpeg/LICENSE.md" "$work/$ffmpeg/COPYING.GPLv2" "$work/$ffmpeg/COPYING.GPLv3" \
  "$work/$ffmpeg/COPYING.LGPLv2.1" "$out/licenses/FFmpeg/"
cp "$work/$x264/COPYING" "$out/licenses/x264/"

{
  echo "FFmpeg ${ffmpeg#ffmpeg-} for macOS (arm64, $minimum or newer), shared, GPL version 2 or later"
  echo "sources (attached to the release):"
  for archive in "${archives[@]}"; do
    echo "  $archive  sha256 $(shasum -a 256 "$out/sources/$archive" | cut -d' ' -f1)"
  done
  echo "built on: macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion))"
  echo "compiler: $(clang --version | head -1); SDK macOS $(xcrun --sdk macosx --show-sdk-version)"
  echo "x264 configure: ${x264_configure[*]} --extra-cflags=\"$flags\""
  # The build directory is a temporary one; it says nothing.
  echo "FFmpeg configure: ${configure[*]//$deps/<deps>}"
  echo "build script: packaging/macos/build-ffmpeg.sh in the StreamForge repository"
} > "$out/BUILD-INFO.txt"
cat "$out/BUILD-INFO.txt"
ls -l "$out/lib"
