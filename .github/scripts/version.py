#!/usr/bin/env python3
"""Reads or sets fz-maximize's version in Cargo.toml and Cargo.lock.

    version.py              prints the version, e.g. 0.4.0
    version.py next-patch   prints the next patch version, e.g. 0.4.1
    version.py set 0.4.1    sets it in Cargo.toml and Cargo.lock (what `cargo build` would
                            write after a manual bump, without needing a Rust toolchain)
"""

import re
import sys
import tomllib

NAME = "fz-maximize"


def current():
    with open("Cargo.toml", "rb") as f:
        return tomllib.load(f)["package"]["version"]


def replace_once(path, pattern, repl):
    with open(path, encoding="utf-8", newline="") as f:
        text = f.read()
    new, n = re.subn(pattern, repl, text, count=1, flags=re.MULTILINE)
    if n != 1:
        sys.exit(f"{path}: version line not found")
    with open(path, "w", encoding="utf-8", newline="") as f:
        f.write(new)


def set_version(version):
    if not re.fullmatch(r"\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?", version):
        sys.exit(f"not a version: {version}")
    # The first `version =` after [package] in Cargo.toml.
    replace_once("Cargo.toml", r'(^\[package\][^\[]*?^version = )"[^"]*"', rf'\g<1>"{version}"')
    # The workspace package's entry in Cargo.lock.
    replace_once(
        "Cargo.lock", rf'(^name = "{re.escape(NAME)}"\r?\nversion = )"[^"]*"', rf'\g<1>"{version}"'
    )
    if current() != version:
        sys.exit("version was not updated")


def main():
    args = sys.argv[1:]
    if not args:
        print(current())
    elif args == ["next-patch"]:
        major, minor, patch = re.match(r"(\d+)\.(\d+)\.(\d+)", current()).groups()
        print(f"{major}.{minor}.{int(patch) + 1}")
    elif len(args) == 2 and args[0] == "set":
        set_version(args[1])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
