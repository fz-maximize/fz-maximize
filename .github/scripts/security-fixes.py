#!/usr/bin/env python3
"""Lists the security advisories that affect the crates in one Cargo.lock but not another.

    security-fixes.py RELEASED_LOCK CURRENT_LOCK

Prints a JSON list (possibly empty) of the advisories fixed by going from RELEASED_LOCK to
CURRENT_LOCK: [{"id", "summary", "crate", "version", "url"}]. Used by
.github/workflows/security-release.yml.

Advisories come from OSV (https://osv.dev), which includes the RustSec and GitHub advisory
databases for crates.io. Only crate names and versions are sent. Withdrawn advisories and
informational RustSec notices (unmaintained, unsound, notice) don't count.
"""

import json
import sys
import tomllib
import urllib.request

OSV = "https://api.osv.dev/v1"


def crates(lock_path):
    with open(lock_path, "rb") as f:
        lock = tomllib.load(f)
    return sorted(
        {
            (p["name"], p["version"])
            for p in lock.get("package", [])
            if p.get("source", "").startswith("registry+")
        }
    )


def request(url, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.load(r)


def advisories(lock_path):
    """{advisory id: (crate, version)} for every advisory affecting the lock file's crates."""
    found = {}
    pkgs = crates(lock_path)
    for i in range(0, len(pkgs), 500):
        chunk = pkgs[i : i + 500]
        queries = [
            {"package": {"ecosystem": "crates.io", "name": n}, "version": v} for n, v in chunk
        ]
        results = request(f"{OSV}/querybatch", {"queries": queries})["results"]
        for (name, version), result in zip(chunk, results):
            for vuln in result.get("vulns", []):
                found.setdefault(vuln["id"], (name, version))
    return found


def main():
    released_lock, current_lock = sys.argv[1:3]
    released = advisories(released_lock)
    current = advisories(current_lock)

    fixed = []
    seen = set()  # the same issue often has both a RUSTSEC and a GHSA id
    # RUSTSEC ids first, so they're the ones listed.
    for vid in sorted(set(released) - set(current), key=lambda i: (not i.startswith("RUSTSEC"), i)):
        if vid in seen:
            continue
        detail = request(f"{OSV}/vulns/{vid}")
        aliases = set(detail.get("aliases", [])) | {vid}
        seen |= aliases
        if aliases & set(current):
            continue  # still affected under another id
        if detail.get("withdrawn"):
            continue
        if any(
            (a.get("database_specific") or {}).get("informational")
            for a in detail.get("affected", [])
        ):
            continue
        crate, version = released[vid]
        fixed.append(
            {
                "id": vid,
                "summary": detail.get("summary") or detail.get("details", "").split("\n")[0],
                "crate": crate,
                "version": version,
                "url": f"https://osv.dev/vulnerability/{vid}",
            }
        )

    json.dump(fixed, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main()
