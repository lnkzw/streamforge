#!/usr/bin/env bash
# Build the FFmpeg libraries the Windows package ships (ADR 0020):
#
#   packaging/windows/build-ffmpeg.sh <out-dir>
#
# FFmpeg with libx264, NVENC, Quick Sync (libvpl), AMF and Schannel, from
# the pinned archives in packaging/ffmpeg/sources.txt, cross-compiled with
# mingw-w64 on Linux; the release workflow runs it on Ubuntu 24.04. Only
# what the engine uses is enabled: the H.264 encoders it tries, AAC, the
# Matroska and FLV muxers, the file and RTMP(S) protocols and swscale.
# Shared libraries with import libraries for the MSVC linker, so the Rust
# binaries link them through FFMPEG_DIR as they do BtbN's build in CI.
#
# Writes into <out-dir>:
#   bin/            the DLLs, which go next to the binaries
#   lib/            their import libraries (.lib)
#   include/        the headers, for ffmpeg-sys-next's bindings
#   licenses/       the licences of FFmpeg and of what is built into it
#   sources/        every source archive the build used
#   BUILD-INFO.txt  the sources, the toolchain and FFmpeg's configure line
#
# Needs mingw-w64 with the win32 thread model (Ubuntu's gcc-mingw-w64
# provides x86_64-w64-mingw32-gcc-win32), nasm, cmake, ninja, make,
# pkg-config, curl and xz. Archives already in <out-dir>/sources are
# reused when their checksums match.
set -euo pipefail

out=${1:?usage: packaging/windows/build-ffmpeg.sh <out-dir>}
repo=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)

ffmpeg="ffmpeg-8.1.3"
x264="x264-0.165.3223+git20250910.0480cb0"
nvheaders="nv-codec-headers-12.1.14.1"
amf="amf-headers-v1.5.3"
vpl="libvpl-2.17.0"
archives=(
  "$ffmpeg.tar.xz"
  "x264-0.165.3223-git20250910-0480cb0.tar.xz"
  "$nvheaders.tar.gz"
  "AMF-headers-v1.5.3.tar.gz"
  "$vpl.tar.gz"
)

host=x86_64-w64-mingw32
# The win32 thread model: FFmpeg and x264 use Windows threads anyway, and
# libvpl's C++ threads then need no libwinpthread beside the DLLs.
cc=$host-gcc-win32
cxx=$host-g++-win32
jobs=$(nproc)

rm -rf "${out:?}/bin" "${out:?}/lib" "${out:?}/include" "${out:?}/licenses" "${out:?}/BUILD-INFO.txt"
"$repo/packaging/ffmpeg/fetch-sources.sh" "$out/sources" "${archives[@]}"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
deps="$work/deps"
for archive in "${archives[@]}"; do
  tar -C "$work" -xf "$out/sources/$archive"
done

# x264: a static library, built into libavcodec. 8-bit only, as the
# engine encodes yuv420p or nv12.
(
  cd "$work/$x264"
  CC="$cc" ./configure --prefix="$deps" --host="$host" --cross-prefix="$host-" \
    --enable-static --disable-cli --bit-depth=8 \
    --disable-lavf --disable-swscale --disable-ffms --disable-gpac --disable-lsmash
  make -j"$jobs"
  make install-lib-static
)

# The encoder headers: NVENC and AMF are loaded from the driver at run
# time, so only their headers are needed.
make -C "$work/$nvheaders" PREFIX="$deps" install
mkdir -p "$deps/include"
cp -R "$work/$amf/AMF" "$deps/include/"

# libvpl: the dispatcher that finds Intel's Quick Sync runtime, static,
# built into libavcodec.
cmake -S "$work/$vpl" -B "$work/vpl-build" -G Ninja \
  -DCMAKE_SYSTEM_NAME=Windows -DCMAKE_SYSTEM_PROCESSOR=AMD64 \
  -DCMAKE_C_COMPILER="$cc" -DCMAKE_CXX_COMPILER="$cxx" \
  -DCMAKE_RC_COMPILER="$host-windres" \
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$deps" \
  -DBUILD_SHARED_LIBS=OFF -DBUILD_TESTS=OFF
cmake --build "$work/vpl-build" -j "$jobs"
cmake --install "$work/vpl-build"
# libvpl is C++, which its pkg-config file leaves to the linker driver;
# FFmpeg links with gcc, so the C++ library is named here, the static
# archive (`-lstdc++` would pick libstdc++-6.dll's import library).
sed -i 's/^Libs\.private:.*/& -l:libstdc++.a/' "$deps/lib/pkgconfig/vpl.pc"

# shellcheck disable=SC2054 # the commas belong to the option values
configure=(
  --target-os=mingw32 --arch=x86_64 --cross-prefix="$host-"
  --cc="$cc" --cxx="$cxx"
  --pkg-config=pkg-config --pkg-config-flags=--static
  --extra-cflags="-I$deps/include" --extra-ldflags="-L$deps/lib -static-libgcc -static-libstdc++"
  --enable-shared --disable-static --enable-gpl
  --disable-autodetect --enable-w32threads --enable-d3d11va --enable-dxva2
  --disable-programs --disable-doc --disable-avdevice --disable-avfilter --disable-swresample
  --disable-everything
  --enable-libx264 --enable-ffnvcodec --enable-nvenc --enable-libvpl --enable-amf --enable-schannel
  --enable-encoder=libx264,h264_nvenc,h264_qsv,h264_amf,aac
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

# Everything the engine uses on Windows must be in the build: the encoders
# it tries, the muxers and protocols for recording and streaming, swscale
# and Schannel.
for macro in CONFIG_LIBX264_ENCODER CONFIG_H264_NVENC_ENCODER CONFIG_H264_QSV_ENCODER \
  CONFIG_H264_AMF_ENCODER CONFIG_AAC_ENCODER CONFIG_MATROSKA_MUXER CONFIG_FLV_MUXER \
  CONFIG_FILE_PROTOCOL CONFIG_TCP_PROTOCOL CONFIG_TLS_PROTOCOL CONFIG_RTMP_PROTOCOL \
  CONFIG_RTMPS_PROTOCOL; do
  if ! grep -q "^#define $macro 1\$" "$work/$ffmpeg/config_components.h"; then
    echo "build-ffmpeg.sh: $macro is not enabled" >&2
    exit 1
  fi
done
for macro in CONFIG_SWSCALE CONFIG_SCHANNEL; do
  if ! grep -q "^#define $macro 1\$" "$work/$ffmpeg/config.h"; then
    echo "build-ffmpeg.sh: $macro is not enabled" >&2
    exit 1
  fi
done

mkdir -p "$out/bin" "$out/lib"
cp "$work/ffmpeg/bin/"*.dll "$out/bin/"
# mingw's FFmpeg installs the import libraries beside the DLLs.
cp "$work/ffmpeg/bin/"*.lib "$out/lib/"
cp -R "$work/ffmpeg/include" "$out/"

# The DLLs may import each other and Windows' own libraries, nothing
# else (no mingw runtime DLLs, nothing from the build machine).
system_dlls=" advapi32.dll bcrypt.dll crypt32.dll gdi32.dll kernel32.dll msvcrt.dll ncrypt.dll ole32.dll oleaut32.dll secur32.dll shell32.dll shlwapi.dll user32.dll ws2_32.dll "
bad=0
for dll in "$out/bin/"*.dll; do
  # Captured first, so a failing objdump stops the script; every DLL
  # imports at least kernel32.
  imports=$("$host-objdump" -p "$dll" | sed -n 's/^\s*DLL Name: //p')
  if ! grep -qix 'kernel32.dll' <<< "$imports"; then
    echo "$(basename "$dll"): no imports read from objdump" >&2
    bad=1
  fi
  while IFS= read -r import; do
    lower=$(tr '[:upper:]' '[:lower:]' <<< "$import")
    if [ -e "$out/bin/$import" ] || [[ $system_dlls == *" $lower "* ]] || [[ $lower == api-ms-win-core-* ]]; then
      continue
    fi
    echo "$(basename "$dll") imports $import, which is neither bundled nor part of Windows" >&2
    bad=1
  done <<< "$imports"
done
if [ "$bad" -ne 0 ]; then
  exit 1
fi

mkdir -p "$out/licenses/FFmpeg" "$out/licenses/x264" "$out/licenses/libvpl" \
  "$out/licenses/nv-codec-headers" "$out/licenses/AMF"
# GPLv3 too: the programs that link this build also contain Rust crates
# under Apache-2.0 alone, which only version 3 of the GPL can combine with.
cp "$work/$ffmpeg/LICENSE.md" "$work/$ffmpeg/COPYING.GPLv2" "$work/$ffmpeg/COPYING.GPLv3" \
  "$work/$ffmpeg/COPYING.LGPLv2.1" "$out/licenses/FFmpeg/"
cp "$work/$x264/COPYING" "$out/licenses/x264/"
cp "$work/$vpl/LICENSE" "$out/licenses/libvpl/"
# The NVENC and AMF headers carry their licences in their comments.
{
  sed -n '1,/^ \*\//p' "$work/$nvheaders/include/ffnvcodec/nvEncodeAPI.h"
  echo
  sed -n '1,/^ \*\//p' "$work/$nvheaders/include/ffnvcodec/dynlink_loader.h"
} > "$out/licenses/nv-codec-headers/NOTICE"
sed -n '1,/^#ifndef/p' "$work/$amf/AMF/core/Factory.h" | sed '$d' > "$out/licenses/AMF/NOTICE"

{
  echo "FFmpeg ${ffmpeg#ffmpeg-} for Windows (x86_64), shared, GPL version 2 or later"
  echo "sources (attached to the release):"
  for archive in "${archives[@]}"; do
    echo "  $archive  sha256 $(sha256sum "$out/sources/$archive" | cut -d' ' -f1)"
  done
  echo "built on: $(. /etc/os-release && echo "$PRETTY_NAME")"
  echo "compilers: $("$cc" --version | head -1); $(nasm --version); $(cmake --version | head -1)"
  echo "x264 configure: --host=$host --enable-static --disable-cli --bit-depth=8 --disable-lavf --disable-swscale --disable-ffms --disable-gpac --disable-lsmash"
  echo "libvpl: CMake, BUILD_SHARED_LIBS=OFF"
  # The build directory is a temporary one; it says nothing.
  echo "FFmpeg configure: ${configure[*]//$deps/<deps>}"
  echo "build script: packaging/windows/build-ffmpeg.sh in the StreamForge repository"
} > "$out/BUILD-INFO.txt"
cat "$out/BUILD-INFO.txt"
ls -l "$out/bin"
