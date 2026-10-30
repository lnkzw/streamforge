#!/usr/bin/env python3
"""Writes packaging/licenses/rust-crates.txt: the licences of the Rust
crates the programs are built from, which every package ships as
LICENSE-RUST-CRATES.txt. Run it after Cargo.lock changes and commit the
result. With --check it changes nothing and fails if the committed file
is out of date; CI runs that.

The file holds, for each crate, the licence files it ships (and so its
copyright lines), as cargo-about 0.9.2 finds them with about.toml; the
notices in notices/, for crates that ship none and for notices that
crates keep in their source files; and the copyright notices of the
protocol descriptions (XML) the Wayland crates generate code from.

It fails on anything cargo-about reports (a crate under a licence
about.toml does not accept, a clarified file that changed), on a crate
whose licence needs a copyright notice that it does not ship and
notices/ does not provide, on a malformed notice, on a notice for a
crate no longer in Cargo.lock, and on an XML file it cannot read.

Needs cargo-about 0.9.2 (cargo install cargo-about --version 0.9.2
--locked) and the crates' sources (cargo fetch --locked). It runs
offline, so the result depends on Cargo.lock and this directory only.
"""

import difflib
import json
import os
import re
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ElementTree
from pathlib import Path

CARGO_ABOUT = "0.9.2"
ROOT = Path(__file__).resolve().parents[2]
HERE = ROOT / "packaging" / "licenses"
OUTPUT = HERE / "rust-crates.txt"
RULE = "-" * 80

# Licences whose terms require the copyright notice in binary copies.
NOTICE_LICENCES = {"MIT", "BSD-2-Clause", "BSD-3-Clause", "ISC"}
# The standard texts' fields for the notice, left unfilled.
PLACEHOLDER = re.compile(
    r"[<\[](year|yyyy|copyright holders?|owner|organization|fullname|name of copyright owner)[>\]]",
    re.IGNORECASE,
)
# A line that starts a copyright notice (not one of the terms' own lines,
# such as "copyright notice and this permission notice").
NOTICE_LINE = re.compile(
    r"^\W*(copyright\b(?!\s+(notice|holder|owner|statement))|\(c\)|©)", re.IGNORECASE | re.MULTILINE
)

HEADER = """\
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

After the licences come further notices: of crates that ship none with
their licence, and those crates keep in their source files for code or
documents they took from others; then the copyright notices of the
Wayland protocol descriptions the Wayland crates generate code from.

Generated from Cargo.lock by packaging/licenses/generate.py.

Overview: licence and number of crates
--------------------------------------
"""


class Failure(Exception):
    pass


def crate_id(crate):
    return f"{crate['name']} {crate['version']}"


def cargo_about():
    version = subprocess.run(
        ["cargo", "about", "--version"], capture_output=True, text=True, check=False
    ).stdout.split()
    if version[1:2] != [CARGO_ABOUT]:
        raise Failure(f"needs cargo-about {CARGO_ABOUT}, found {' '.join(version) or 'none'}")
    with tempfile.TemporaryDirectory() as work:
        output = Path(work) / "about.json"
        run = subprocess.run(
            ["cargo", "about", "generate", "--workspace", "--frozen", "--fail",
             "-c", str(HERE / "about.toml"), "--format", "json", "-o", str(output)],
            cwd=ROOT, capture_output=True, text=True, check=False,
        )
        # Some of its warnings (a clarified file that changed, a crate
        # configured that is not in the graph) leave licence files out, so
        # anything it reports fails.
        if run.returncode != 0 or run.stderr.strip():
            raise Failure(f"cargo-about reported (check about.toml):\n{run.stderr.strip()}")
        return json.loads(output.read_text(encoding="utf-8"))


def read_notices(crates):
    """The notices in notices/: (file name, crates, source, text)."""
    notices = []
    for path in sorted((HERE / "notices").glob("*.txt")):
        lines = path.read_text(encoding="utf-8").splitlines()
        if len(lines) < 4 or not lines[0].startswith("Crates: ") or not lines[1].startswith("Source: ") or lines[2]:
            raise Failure(f"{path.name} must start with a Crates: line, a Source: line and an empty line")
        named = [name.strip() for name in lines[0].removeprefix("Crates: ").split(",")]
        source = lines[1].removeprefix("Source: ").strip()
        text = "\n".join(lines[3:]).strip()
        if not all(re.fullmatch(r"\S+ \S+", name) for name in named) or not source or not text:
            raise Failure(f"{path.name} needs crates as 'name version', a source and the notice")
        stale = sorted(set(named) - crates)
        if stale:
            raise Failure(f"{path.name} names crates that are not in Cargo.lock: {', '.join(stale)}")
        notices.append((path.name, named, source, text))
    return notices


def check_notices_cover(licences, notices):
    """Every crate whose licence needs its copyright notice, but whose text
    has none, has a notice: cargo-about gives the standard text when a
    crate ships no file, and a crate's file may be the template or the
    terms alone."""
    noticed = {name for _, named, _, _ in notices for name in named}
    lacking = set()
    for licence in licences:
        text = licence["text"]
        if licence["id"] in NOTICE_LICENCES and (
            licence.get("source_path") is None
            or PLACEHOLDER.search(text)
            or not NOTICE_LINE.search(text)
        ):
            lacking.update(crate_id(user["crate"]) for user in licence["used_by"])
    missing = sorted(lacking - noticed)
    if missing:
        raise Failure(
            "these crates ship no copyright notice; add theirs (from their repository, at the "
            "commit in .cargo_vcs_info.json) to packaging/licenses/notices:\n" + "\n".join(missing)
        )


def xml_files(crate):
    """The crate's XML files, failing on what the walk cannot read."""
    root = Path(crate["manifest_path"]).parent

    def failed(err):
        raise Failure(f"{crate_id(crate)}: cannot read {err.filename}: {err.strerror}")

    for directory, subdirectories, files in os.walk(root, onerror=failed):
        subdirectories.sort()
        for name in sorted(files):
            if name.lower().endswith(".xml"):
                yield root, Path(directory) / name


def protocol_notices(crates):
    """The crates with XML files that hold copyright elements, and each
    element's text once, in the order of crates and files. Only plain,
    UTF-8 files without a document type are read; anything else that
    mentions a copyright fails, so that no notice is lost unseen."""
    named, blocks = [], {}
    for crate in sorted(crates, key=lambda crate: (crate["name"], crate["version"])):
        found = False
        for root, path in xml_files(crate):
            where = f"{crate_id(crate)}: {path.relative_to(root)}"
            if path.is_symlink() or not path.is_file():
                raise Failure(f"{where} is not a regular file")
            data = path.read_bytes()
            if data.startswith((b"\xff\xfe", b"\xfe\xff")) or b"\0" in data:
                raise Failure(f"{where} is not UTF-8")
            if b"copyright" not in data.lower():
                continue
            if b"<!DOCTYPE" in data or b"<!ENTITY" in data:
                raise Failure(f"{where} has a document type, which this script does not read")
            try:
                tree = ElementTree.fromstring(data)
            except (ElementTree.ParseError, ValueError, LookupError) as err:
                raise Failure(f"{where} cannot be read: {err}")
            for element in tree.iter():
                # By local name, with or without a namespace.
                if not isinstance(element.tag, str) or element.tag.rsplit("}", 1)[-1] != "copyright":
                    continue
                lines = [line.strip() for line in "".join(element.itertext()).splitlines()]
                text = "\n".join(lines).strip()
                if text:
                    blocks.setdefault(text, None)
                    found = True
        if found:
            named.append(crate_id(crate))
    return named, list(blocks)


def render(about, notices, protocol_crates, protocol_blocks):
    out = [HEADER]
    out += [f"{entry['name']} ({entry['id']}): {entry['count']}" for entry in about["overview"]]
    for licence in about["licenses"]:
        out += ["", RULE, f"{licence['name']} ({licence['id']})", "", "Used by:"]
        out += [f"- {crate_id(user['crate'])}" for user in licence["used_by"]]
        out += ["", licence["text"]]
    out += ["", RULE, "Further notices"]
    for _, named, source, text in notices:
        out += ["", RULE, "", "Used by:"] + [f"- {name}" for name in named]
        out += ["", f"Source: {source}", "", text]
    out += ["", RULE, "Copyright notices of the Wayland protocol descriptions", "", "In:"]
    out += [f"- {name}" for name in protocol_crates]
    for block in protocol_blocks:
        out += ["", block]
    # Line endings and trailing blanks as the crates' files have them
    # would make the file differ between checkouts.
    lines = "\n".join(out).replace("\r\n", "\n").replace("\r", "\n").split("\n")
    return "\n".join(line.rstrip() for line in lines).rstrip("\n") + "\n"


def main():
    check = sys.argv[1:] == ["--check"]
    if sys.argv[1:] and not check:
        raise Failure("usage: generate.py [--check]")
    about = cargo_about()
    packages = [crate["package"] for crate in about["crates"]]
    notices = read_notices({crate_id(package) for package in packages})
    check_notices_cover(about["licenses"], notices)
    registry = [package for package in packages if package["source"] is not None]
    text = render(about, notices, *protocol_notices(registry))
    home = str(Path.home())
    if ".cargo/registry" in text or home in text:
        raise Failure("the licences name a local path")

    name = OUTPUT.relative_to(ROOT)
    if check:
        current = OUTPUT.read_text(encoding="utf-8") if OUTPUT.exists() else ""
        if current != text:
            diff = difflib.unified_diff(
                current.splitlines(), text.splitlines(), str(name), "generated", lineterm=""
            )
            print("\n".join(list(diff)[:200]))
            raise Failure(f"{name} is out of date: run packaging/licenses/generate.py and commit the result")
        print(f"{name} is up to date")
    else:
        OUTPUT.write_text(text, encoding="utf-8")
        print(f"wrote {name}")


if __name__ == "__main__":
    try:
        main()
    except Failure as failure:
        print(f"generate.py: {failure}", file=sys.stderr)
        sys.exit(1)
