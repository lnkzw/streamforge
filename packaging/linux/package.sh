#!/usr/bin/env bash
# Build the StreamForge Linux binaries in release mode, check them on this
# system and pack them for one distribution family:
#
#   packaging/linux/package.sh <distro>      e.g. ubuntu24.04, archlinux
#
# Run from the repository root. Writes dist/<name>.tar.gz and
# dist/<name>.tar.gz.sha256. The binaries link FFmpeg and PipeWire
# dynamically, so a package runs where those libraries have the versions
# it was built against: the distribution it was built on.
set -euo pipefail

distro=${1:?usage: packaging/linux/package.sh <distro>}
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
test -n "$version"
name="streamforge-$version-x86_64-linux-$distro"
out="dist/$name"
bins=(streamforge streamforge-engine streamforge-desktop streamforge-mcp)

cargo build --release --locked \
  -p streamforge-cli -p streamforge-engine -p streamforge-desktop -p streamforge-mcp

rm -rf "$out" "dist/$name.tar.gz" "dist/$name.tar.gz.sha256"
mkdir -p "$out/bin"
for bin in "${bins[@]}"; do
  install -m 0755 "target/release/$bin" "$out/bin/"
done
cp LICENSE-MIT LICENSE-APACHE README.md packaging/linux/INSTALL.md "$out/"
# The desktop binary embeds IBM Plex, whose licence must travel with it.
cp crates/streamforge-desktop/assets/fonts/OFL.txt "$out/LICENSE-OFL-IBM-Plex.txt"

# Every library the binaries link must be found here.
missing=$(ldd "$out"/bin/* | grep 'not found' || true)
if [ -n "$missing" ]; then
  echo "unresolved libraries:" >&2
  echo "$missing" >&2
  exit 1
fi

# What this package was built from and against, for bug reports.
{
  echo "StreamForge $version for $distro (x86_64)"
  echo "commit: $(git rev-parse HEAD 2>/dev/null || echo unknown)"
  echo "rustc: $(rustc --version)"
  if [ -r /etc/os-release ]; then
    echo "built on: $(. /etc/os-release && echo "$PRETTY_NAME")"
  fi
  echo "linked system libraries:"
  ldd "$out"/bin/* | grep -oE '(libav[a-z]+|libsw[a-z]+|libpipewire)[^ ]*\.so\.[0-9]+' | sort -u | sed 's/^/  /'
} > "$out/BUILD-INFO.txt"
cat "$out/BUILD-INFO.txt"

# Smoke test on this system: each command-line binary starts and answers.
# (The desktop needs a display; the library check above covers it.)
"$out/bin/streamforge" --version
"$out/bin/streamforge" status --json
response=$(printf '%s\n' '{"id":1,"command":"get_status"}' | "$out/bin/streamforge-engine")
echo "$response"
grep -q '"ok":true' <<< "$response"
"$out/bin/streamforge-engine" --version
"$out/bin/streamforge-mcp" --version

tar -C dist -czf "dist/$name.tar.gz" "$name"
(cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
cat "dist/$name.tar.gz.sha256"
