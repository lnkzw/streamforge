#!/usr/bin/env bash
# Download source archives listed in packaging/ffmpeg/sources.txt and
# check them against their pinned SHA-256:
#
#   packaging/ffmpeg/fetch-sources.sh <dir> <file name>...
#
# An archive already in <dir> with the right checksum is kept; a wrong
# one is downloaded again. Fails on an unknown name or a checksum that
# does not match after the download.
set -euo pipefail

dir=${1:?usage: packaging/ffmpeg/fetch-sources.sh <dir> <file name>...}
shift
list="$(cd "$(dirname "$0")" && pwd)/sources.txt"
mkdir -p "$dir"

sha256() {
  if command -v sha256sum > /dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

for name in "$@"; do
  entry=$(awk -v name="$name" '$1 == name { print $2, $3 }' "$list")
  if [ -z "$entry" ]; then
    echo "fetch-sources.sh: $name is not in $list" >&2
    exit 2
  fi
  read -r expected url <<< "$entry"
  file="$dir/$name"
  if [ -f "$file" ] && [ "$(sha256 "$file")" = "$expected" ]; then
    echo "$name: present"
    continue
  fi
  curl -fsSL --retry 3 -o "$file.part" "$url"
  actual=$(sha256 "$file.part")
  if [ "$actual" != "$expected" ]; then
    echo "fetch-sources.sh: $name has SHA-256 $actual, expected $expected" >&2
    rm -f "$file.part"
    exit 1
  fi
  mv "$file.part" "$file"
  echo "$name: downloaded"
done
