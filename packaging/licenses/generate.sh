#!/usr/bin/env bash
# Writes packaging/licenses/rust-crates.txt: the licences of the Rust
# crates the programs are built from, which every package ships as
# LICENSE-RUST-CRATES.txt. Run it after Cargo.lock changes and commit
# the result. With --check it changes nothing and fails if the
# committed file is out of date; CI runs that.
#
# The file holds, for each crate, the licence files it ships (and so its
# copyright lines), as cargo-about 0.9.2 finds them with about.toml; the
# notices in notices/ for crates that ship none; and the copyright
# notices of the protocol descriptions (XML) the Wayland crates generate
# code from. It fails on a crate under a licence about.toml does not
# accept, on a crate whose licence needs a copyright notice it does not
# ship and notices/ does not provide, and on a notice for a crate no
# longer in Cargo.lock.
#
# Needs cargo-about 0.9.2 (cargo install cargo-about --version 0.9.2
# --locked), jq and the crates' sources (cargo fetch --locked). It runs
# offline, so the result depends on Cargo.lock and this directory only.
set -euo pipefail

cd "$(dirname "$0")/../.."
dir=packaging/licenses
want=0.9.2
export LC_ALL=C

have=$(cargo about --version | awk '{ print $2 }')
if [ "$have" != "$want" ]; then
  echo "needs cargo-about $want, found ${have:-none}" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
json=$work/about.json
: > "$work/protocol-crates"

if ! cargo about generate --workspace --frozen --fail -c "$dir/about.toml" --format json \
  -o "$json" 2> "$work/about.log"; then
  cat "$work/about.log" >&2
  exit 1
fi
# A clarification whose file is missing or changed is only a warning, and
# the crate's files are then left out.
if grep -E 'WARN|ERROR' "$work/about.log" >&2; then
  echo "cargo-about warned (above): check about.toml" >&2
  exit 1
fi

# "name version" of every crate, and of those whose licence needs its
# copyright notice but which ship no file with one: cargo-about then
# gives the licence's standard text, without the notice.
jq -r '.crates[].package | "\(.name) \(.version)"' "$json" | sort -u > "$work/crates"
jq -r '.licenses[]
  | select(.source_path == null and (.id | IN("MIT", "BSD-2-Clause", "BSD-3-Clause", "ISC")))
  | .used_by[].crate | "\(.name) \(.version)"' "$json" | sort -u > "$work/unnoticed"

# A notice starts with "Crates: name version, ..." and "Source: ...".
notices=("$dir"/notices/*.txt)
for notice in "${notices[@]}"; do
  if ! sed -n 1p "$notice" | grep -q '^Crates: ' || ! sed -n 2p "$notice" | grep -q '^Source: '; then
    echo "$notice must start with a Crates: and a Source: line" >&2
    exit 1
  fi
  sed -n '1s/^Crates: //p' "$notice" | tr ',' '\n' | sed 's/^ *//; s/ *$//'
done | sort -u > "$work/noticed"
stale=$(comm -23 "$work/noticed" "$work/crates")
if [ -n "$stale" ]; then
  printf 'notices/ names crates that are not in Cargo.lock:\n%s\n' "$stale" >&2
  exit 1
fi
missing=$(comm -23 "$work/unnoticed" "$work/noticed")
if [ -n "$missing" ]; then
  printf 'these crates ship no copyright notice; add theirs (from their repository) to %s/notices:\n%s\n' \
    "$dir" "$missing" >&2
  exit 1
fi

# The copyright blocks of the XML files in the crates' sources, each once.
jq -r '.crates[].package | select(.source != null) | "\(.name) \(.version)\t\(.manifest_path)"' "$json" |
  sort | while IFS=$'\t' read -r crate manifest; do
    files=$(grep -rl --include='*.xml' '<copyright>' "$(dirname "$manifest")" | sort || true)
    [ -n "$files" ] || continue
    echo "$crate" >> "$work/protocol-crates"
    printf '%s\n' "$files" | while IFS= read -r file; do cat "$file"; done
  done | awk '
    /<copyright>/ { inside = 1; block = ""; sub(/.*<copyright>/, "") }
    inside {
      line = $0
      last = sub(/<\/copyright>.*/, "", line)
      gsub(/^[ \t]+|[ \t]+$/, "", line)
      gsub(/&lt;/, "<", line); gsub(/&gt;/, ">", line); gsub(/&quot;/, "\"", line)
      gsub(/&apos;/, "'\''", line); gsub(/&amp;/, "\\&", line)
      block = block line "\n"
      if (last) {
        inside = 0
        sub(/^\n+/, "", block); sub(/\n+$/, "", block)
        if (block != "" && !(block in seen)) { seen[block] = 1; print ""; print block }
      }
    }' > "$work/protocol-notices"

rule=$(printf '%.0s-' {1..80})
{
  cat <<'EOF'
Licences of the Rust crates in StreamForge
==========================================

The StreamForge programs are built from the Rust crates listed here,
StreamForge's own among them. The list covers the Linux, Windows and
macOS packages, so one package may not contain every crate. A crate
that offers a choice of licences is listed under the one StreamForge
uses for it. INSTALL.md says under which licence the programs as a
whole are distributed.

Some crates, among them unicode-segmentation, unicode-width and
unicode-general-category, contain tables generated from the Unicode
Character Database. That data is licensed under the Unicode License v3
(Unicode-3.0, below): Copyright © 1991-2025 Unicode, Inc.

After the licences come the notices of crates that ship none with
their licence, and the copyright notices of the Wayland protocol
descriptions the Wayland crates generate code from.

Generated from Cargo.lock by packaging/licenses/generate.sh.

Overview: licence and number of crates
--------------------------------------

EOF
  jq -r --arg rule "$rule" '
    (.overview[] | "\(.name) (\(.id)): \(.count)"),
    (.licenses[] | "", $rule, "\(.name) (\(.id))", "", "Used by:",
      (.used_by[].crate | "- \(.name) \(.version)"), "", .text)' "$json"
  printf '\n%s\nNotices of crates that ship none\n' "$rule"
  for notice in "${notices[@]}"; do
    printf '\n%s\n\nUsed by:\n' "$rule"
    sed -n '1s/^Crates: //p' "$notice" | tr ',' '\n' | sed 's/^ */- /'
    printf '\n'
    sed '1d' "$notice"
  done
  printf '\n%s\nCopyright notices of the Wayland protocol descriptions\n\nIn:\n' "$rule"
  sed 's/^/- /' "$work/protocol-crates"
  cat "$work/protocol-notices"
} | sed 's/\r$//; s/[[:space:]]*$//' > "$work/out"

if grep -n -e '\.cargo/registry' -e "$HOME" "$work/out" >&2; then
  echo "the licences name a local path" >&2
  exit 1
fi

if [ "${1-}" = --check ]; then
  if ! diff -u "$dir/rust-crates.txt" "$work/out"; then
    echo "$dir/rust-crates.txt is out of date: run $0 and commit the result" >&2
    exit 1
  fi
  echo "$dir/rust-crates.txt is up to date"
else
  cp "$work/out" "$dir/rust-crates.txt"
  echo "wrote $dir/rust-crates.txt"
fi
