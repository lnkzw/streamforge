#!/usr/bin/env bash
# Install StreamForge from this unpacked package, or remove it again:
#
#   ./install.sh [--prefix DIR]              default prefix: ~/.local
#   ./install.sh --uninstall [--prefix DIR]
#
# Installs the binaries into DIR/bin, the launcher and its icon into
# DIR/share (so the desktop app shows up in the application menu), and the
# documentation and licences into DIR/share/doc/streamforge. Without
# --prefix, the shared files go to $XDG_DATA_HOME if it is set. Nothing
# else is touched; no root rights are needed for a prefix you own.
set -euo pipefail

app_id=io.github.lnkzw.StreamForge
bins=(streamforge streamforge-engine streamforge-desktop streamforge-mcp)
here=$(cd "$(dirname "$0")" && pwd)

prefix="$HOME/.local"
data=${XDG_DATA_HOME:-$HOME/.local/share}
uninstall=false
while [ $# -gt 0 ]; do
  case $1 in
    --prefix)
      prefix=${2:?--prefix needs a directory}
      data=
      shift 2
      ;;
    --uninstall)
      uninstall=true
      shift
      ;;
    -h | --help)
      sed -n '2,11s/^# \{0,1\}//p' "$0"
      exit 0
      ;;
    *)
      echo "install.sh: unknown argument $1 (see --help)" >&2
      exit 2
      ;;
  esac
done

# The launcher's Exec line names the binary by its absolute path, since a
# graphical session's PATH need not include DIR/bin. Characters the
# desktop entry format would need escaped, and `=` (which it does not
# allow in a program path), are refused rather than escaped.
case $prefix in
  /*) ;;
  *)
    echo "install.sh: --prefix must be an absolute path" >&2
    exit 2
    ;;
esac
if [[ $prefix =~ [\"\`\$\\%=[:cntrl:]] ]]; then
  echo "install.sh: --prefix must not contain \" \` \$ \\ % = or control characters" >&2
  exit 2
fi
prefix=${prefix%/}
data=${data:-$prefix/share}
data=${data%/}

launcher="$data/applications/$app_id.desktop"
icon="$data/icons/hicolor/scalable/apps/$app_id.svg"
doc="$data/doc/streamforge"

if $uninstall; then
  for bin in "${bins[@]}"; do
    rm -fv "$prefix/bin/$bin"
  done
  rm -fv "$launcher" "$icon"
  rm -rf "$doc"
  echo "StreamForge removed from $prefix. Projects and recordings stay where you saved them."
  exit 0
fi

install -d "$prefix/bin" "$(dirname "$launcher")" "$(dirname "$icon")"
for bin in "${bins[@]}"; do
  install -m 0755 "$here/bin/$bin" "$prefix/bin/"
done
install -m 0644 "$here/share/icons/hicolor/scalable/apps/$app_id.svg" "$icon"
while IFS= read -r line; do
  if [ "$line" = "Exec=streamforge-desktop" ]; then
    line="Exec=\"$prefix/bin/streamforge-desktop\""
  fi
  printf '%s\n' "$line"
done < "$here/share/applications/$app_id.desktop" > "$launcher.tmp"
chmod 0644 "$launcher.tmp"
mv "$launcher.tmp" "$launcher"

rm -rf "$doc"
install -d "$doc"
cp -R "$here/README.md" "$here/INSTALL.md" "$here/BUILD-INFO.txt" "$here"/LICENSE-* "$here/docs" "$doc/"

echo "StreamForge installed into $prefix:"
echo "  binaries   $prefix/bin"
echo "  launcher   $launcher"
echo "  documents  $doc"
case ":$PATH:" in
  *":$prefix/bin:"*) ;;
  *) echo "Note: $prefix/bin is not on your PATH; add it to use the commands by name." ;;
esac
# Where application menus look for launchers (the XDG base directories).
case ":${XDG_DATA_HOME:-$HOME/.local/share}:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}:" in
  *":$data:"* | *":$data/:"*) ;;
  *) echo "Note: application menus do not look in $data; add it to XDG_DATA_DIRS for the launcher to appear." ;;
esac
