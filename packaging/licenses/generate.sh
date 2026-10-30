#!/usr/bin/env bash
# Writes packaging/licenses/rust-crates.txt: the licences of the Rust
# crates the programs are built from, with each crate's own licence
# files (and so its copyright lines), which every package ships as
# LICENSE-RUST-CRATES.txt. Run it after Cargo.lock changes and commit
# the result. With --check it changes nothing and fails if the
# committed file is out of date; CI runs that.
#
# Needs cargo-about 0.9.2 (cargo install cargo-about --version 0.9.2
# --locked) and the crates' sources (cargo fetch --locked). It runs
# offline, so the result depends on Cargo.lock and this directory only.
set -euo pipefail

cd "$(dirname "$0")/../.."
dir=packaging/licenses
want=0.9.2

have=$(cargo about --version | awk '{ print $2 }')
if [ "$have" != "$want" ]; then
  echo "needs cargo-about $want, found ${have:-none}" >&2
  exit 1
fi

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
# Line endings and trailing blanks as the crates' files have them would
# make the file differ between checkouts.
cargo about generate --workspace --frozen --fail -c "$dir/about.toml" "$dir/about.hbs" |
  sed 's/\r$//; s/[[:space:]]*$//' > "$tmp"
if grep -n -e '\.cargo/registry' -e "$HOME" "$tmp" >&2; then
  echo "the licences name a local path" >&2
  exit 1
fi

if [ "${1-}" = --check ]; then
  if ! diff -u "$dir/rust-crates.txt" "$tmp"; then
    echo "$dir/rust-crates.txt is out of date: run $0 and commit the result" >&2
    exit 1
  fi
  echo "$dir/rust-crates.txt is up to date"
else
  cp "$tmp" "$dir/rust-crates.txt"
  echo "wrote $dir/rust-crates.txt"
fi
