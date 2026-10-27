#!/usr/bin/env bash
# Print the release notes for VERSION: its section of CHANGELOG.md,
# without the heading, with relative links pointing at the tagged files on
# GitHub (the release page has no files beside it). Fails when the
# changelog has no such section (or an empty one), so a dry run catches a
# release without its entry.
#
#   packaging/release-notes.sh 0.1.0 > notes.md
set -euo pipefail

version=${1:?usage: release-notes.sh VERSION}
changelog="$(cd "$(dirname "$0")/.." && pwd)/CHANGELOG.md"
base="https://github.com/${GITHUB_REPOSITORY:-lnkzw/streamforge}/blob/v$version"

# From "## [VERSION]" to the next "## [" heading; leading blank lines go.
notes=$(awk -v heading="## [$version]" '
  index($0, heading) == 1 { on = 1; next }
  /^## \[/ { on = 0 }
  on && (started || NF) { started = 1; print }
' "$changelog")

if [ -z "${notes//[[:space:]]/}" ]; then
  echo "release-notes.sh: CHANGELOG.md has no notes for $version" >&2
  exit 1
fi
# A link target without a scheme (no ":") and not an anchor is relative.
printf '%s\n' "$notes" | sed -E "s#\]\(([^):/\#][^):]*)\)#](${base}/\1)#g"
