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
app_id=io.github.lnkzw.StreamForge

cargo build --release --locked \
  -p streamforge-cli -p streamforge-engine -p streamforge-desktop -p streamforge-mcp

rm -rf "$out" "dist/$name.tar.gz" "dist/$name.tar.gz.sha256"
mkdir -p "$out/bin"
for bin in "${bins[@]}"; do
  install -m 0755 "target/release/$bin" "$out/bin/"
done
cp LICENSE-MIT LICENSE-APACHE README.md CHANGELOG.md packaging/linux/INSTALL.md packaging/linux/install.sh "$out/"
# The documentation the README and the user guide link to, at the same
# relative paths as in the repository.
cp -R docs "$out/"
# The desktop binary embeds IBM Plex, whose licence must travel with it.
cp crates/streamforge-desktop/assets/fonts/OFL.txt "$out/LICENSE-OFL-IBM-Plex.txt"
# The launcher and its icon, named after the app id the desktop window
# reports (crates/streamforge-desktop/src/identity.rs).
install -D -m 0644 "packaging/linux/$app_id.desktop" "$out/share/applications/$app_id.desktop"
install -D -m 0644 "crates/streamforge-desktop/assets/icon/$app_id.svg" \
  "$out/share/icons/hicolor/scalable/apps/$app_id.svg"
if command -v desktop-file-validate > /dev/null; then
  desktop-file-validate "$out/share/applications/$app_id.desktop"
else
  echo "desktop-file-validate not installed; launcher not validated"
fi

# Every relative link in the user's entry points must resolve inside the
# package (they are read there, not on GitHub). The developer documents
# may point into the source tree.
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

# Smoke test on this system: each binary starts and answers. (The
# desktop's window needs a display, `--version` does not; the library
# check above covers the rest.)
"$out/bin/streamforge-desktop" --version
"$out/bin/streamforge" --version
"$out/bin/streamforge" status --json
response=$(printf '%s\n' '{"id":1,"command":"get_status"}' | "$out/bin/streamforge-engine")
echo "$response"
grep -q '"ok":true' <<< "$response"
"$out/bin/streamforge-engine" --version
"$out/bin/streamforge-mcp" --version

# The installer: into a fresh prefix, the launcher pointing at the
# installed binary, and out again without leftovers.
prefix=$(mktemp -d)
"$out/install.sh" --prefix "$prefix/with space"
"$prefix/with space/bin/streamforge" --version
grep -qxF "Exec=\"$prefix/with space/bin/streamforge-desktop\"" \
  "$prefix/with space/share/applications/$app_id.desktop"
test -f "$prefix/with space/share/icons/hicolor/scalable/apps/$app_id.svg"
test -f "$prefix/with space/share/doc/streamforge/docs/user-guide.md"
test -f "$prefix/with space/share/doc/streamforge/CHANGELOG.md"
if command -v desktop-file-validate > /dev/null; then
  desktop-file-validate "$prefix/with space/share/applications/$app_id.desktop"
fi
"$out/install.sh" --uninstall --prefix "$prefix/with space"
# Without --prefix: the binaries in ~/.local/bin, the shared files where
# XDG_DATA_HOME says.
HOME="$prefix/home" XDG_DATA_HOME="$prefix/data" "$out/install.sh"
test -x "$prefix/home/.local/bin/streamforge-desktop"
grep -qxF "Exec=\"$prefix/home/.local/bin/streamforge-desktop\"" \
  "$prefix/data/applications/$app_id.desktop"
HOME="$prefix/home" XDG_DATA_HOME="$prefix/data" "$out/install.sh" --uninstall
leftovers=$(find "$prefix" -type f)
if [ -n "$leftovers" ]; then
  echo "install.sh --uninstall left files behind:" >&2
  echo "$leftovers" >&2
  exit 1
fi
# A path the launcher's Exec line cannot hold is refused.
if "$out/install.sh" --prefix "$prefix/a=b" 2> /dev/null; then
  echo "install.sh accepted a prefix with =" >&2
  exit 1
fi
rm -rf "$prefix"

tar -C dist -czf "dist/$name.tar.gz" "$name"
(cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
cat "dist/$name.tar.gz.sha256"
