#!/usr/bin/env python3
"""Reads or adds entries in CHANGELOG.md.

    changelog.py notes 0.4.2     prints that version's entry (its "- " lines); fails if
                                 there's none
    changelog.py add 0.4.3       adds an entry for that version, read from stdin, above
                                 the newest one

Used by release.yml (the notes at the top of each GitHub release, which the app's update
window shows) and security-release.yml (the entry for a security release).
"""

import re
import sys

PATH = "CHANGELOG.md"


def read():
    with open(PATH, encoding="utf-8", newline="") as f:
        return f.read().replace("\r\n", "\n")


def heading(line, version):
    return line.startswith("## ") and line[3:].split()[:1] == [version]


def notes(version):
    lines = read().split("\n")
    start = next((i for i, l in enumerate(lines) if heading(l, version)), None)
    if start is None:
        sys.exit(f"CHANGELOG.md has no entry for {version}")
    body = []
    for line in lines[start + 1 :]:
        if line.startswith("## "):
            break
        body.append(line)
    text = "\n".join(body).strip()
    if not text:
        sys.exit(f"CHANGELOG.md's entry for {version} is empty")
    print(text)


def add(version, entry):
    if not re.fullmatch(r"\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?", version):
        sys.exit(f"not a version: {version}")
    entry = entry.strip()
    if not entry:
        sys.exit("empty entry")
    text = read()
    lines = text.split("\n")
    if any(heading(l, version) for l in lines):
        print(f"CHANGELOG.md already has an entry for {version}")
        return
    first = next((i for i, l in enumerate(lines) if l.startswith("## ")), len(lines))
    new = lines[:first] + [f"## {version}", "", entry, ""] + lines[first:]
    with open(PATH, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(new))


def main():
    args = sys.argv[1:]
    if len(args) == 2 and args[0] == "notes":
        notes(args[1])
    elif len(args) == 2 and args[0] == "add":
        add(args[1], sys.stdin.read())
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
