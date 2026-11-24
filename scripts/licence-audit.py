#!/usr/bin/env python3
"""Check every third-party crate in every Cargo.lock against the licence policy.

docs/LICENSING-POLICY.md: permissive only. There is no top-level workspace, so
each crate has its own lockfile; this walks all of them (skipping target/,
vendor/ and third_party/), collects the distinct (crate, version) pairs from
crates.io, and asks crates.io for each version's SPDX licence expression.
Answers are cached in ~/.cache/atrium/licences.json (a version's licence never
changes), so a rerun only asks about new versions.

A pair passes when at least one OR-alternative of its expression consists only
of allowed licences. Exit status: 0 all pass, 1 a crate fails or has no
licence, 2 crates.io could not be asked (with --offline, an uncached pair).

    python3 scripts/licence-audit.py            # whole tree
    python3 scripts/licence-audit.py mail arca  # only these directories
    python3 scripts/licence-audit.py --offline  # cache only, no network
"""
import json
import os
import re
import sys
import time
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CACHE = os.path.join(os.environ.get("XDG_CACHE_HOME", os.path.expanduser("~/.cache")), "atrium", "licences.json")
SKIP_DIRS = {".git", "target", "vendor", "third_party", "node_modules"}

# The policy's allowlist (LICENSING-POLICY.md, AGENTS.md "Hard rules").
# MPL-2.0 is case by case and so is NOT here: a crate that needs it is
# reported, and accepted by adding it to EXCEPTIONS with the reason.
ALLOWED = {
    "MIT", "MIT-0", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause",
    "BSD-3-Clause", "0BSD", "ISC", "Zlib", "CC0-1.0", "Unlicense", "BSL-1.0",
    "Unicode-3.0", "Unicode-DFS-2016",
    # bzip2's own licence: BSD-style, permissive (libbz2-rs-sys).
    "bzip2-1.0.6",
    # The Independent JPEG Group licence: permissive with attribution
    # (jpeg-encoder, which ports IJG tables).
    "IJG",
    # University of Illinois/NCSA: BSD-style, permissive (libfuzzer-sys; fuzz
    # targets only, nothing that ships).
    "NCSA",
}

# crate name -> why it is accepted although its expression does not pass.
EXCEPTIONS = {}


def lockfiles(dirs):
    for top in dirs:
        for dp, dn, fn in os.walk(top):
            dn[:] = [d for d in dn if d not in SKIP_DIRS]
            if "Cargo.lock" in fn:
                yield os.path.join(dp, "Cargo.lock")


def registry_pairs(path):
    """(name, version) of every crates.io package in one lockfile."""
    pairs, name, version, in_pkg = set(), None, None, False
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line.startswith("["):
                in_pkg = line == "[[package]]"
                name = version = None
            elif not in_pkg:
                continue
            elif line.startswith("name = "):
                name = line.split('"')[1]
            elif line.startswith("version = "):
                version = line.split('"')[1]
            elif line.startswith('source = "registry+') and name and version:
                pairs.add((name, version))
    return pairs


def passes(expr):
    """Whether an SPDX expression can be satisfied with allowed licences:
    `A OR B` needs one side, `A AND B` both, `A WITH X` is taken as written
    (only exceptions listed in ALLOWED pass). The legacy `A/B` means OR."""
    toks = re.findall(r"\(|\)|[^\s()]+", expr.replace("/", " OR "))
    pos = 0

    def peek():
        return toks[pos] if pos < len(toks) else None

    def atom():
        nonlocal pos
        if peek() == "(":
            pos += 1
            v = any_of()
            if peek() != ")":
                raise ValueError(expr)
            pos += 1
            return v
        lic = peek()
        if lic is None or lic in ("AND", "OR", "WITH", ")"):
            raise ValueError(expr)
        pos += 1
        if peek() == "WITH":
            lic = f"{lic} WITH {toks[pos + 1]}"
            pos += 2
        return lic in ALLOWED

    def all_of():
        nonlocal pos
        v = atom()
        while peek() == "AND":
            pos += 1
            v = atom() and v
        return v

    def any_of():
        nonlocal pos
        v = all_of()
        while peek() == "OR":
            pos += 1
            v = all_of() or v
        return v

    try:
        v = any_of()
        return v and pos == len(toks)
    except (ValueError, IndexError):
        return False


def ask(name, version):
    req = urllib.request.Request(
        f"https://crates.io/api/v1/crates/{name}/{version}",
        headers={"User-Agent": "licence-audit-script (generic)"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)["version"].get("license")


def main(argv):
    offline = "--offline" in argv
    dirs = [os.path.join(ROOT, a) for a in argv if not a.startswith("--")] or [ROOT]
    locks = sorted(set(lockfiles(dirs)))
    where = {}
    for lock in locks:
        for pair in registry_pairs(lock):
            where.setdefault(pair, set()).add(os.path.relpath(os.path.dirname(lock), ROOT).split(os.sep)[0])
    try:
        with open(CACHE, encoding="utf-8") as f:
            cache = json.load(f)
    except (OSError, ValueError):
        cache = {}
    unasked = []
    for name, version in sorted(where):
        key = f"{name} {version}"
        if key in cache:
            continue
        if offline:
            unasked.append(key)
            continue
        try:
            cache[key] = ask(name, version)
        except Exception as e:  # network, HTTP, JSON: report, do not guess
            print(f"licence-audit: could not ask crates.io about {key}: {e}", file=sys.stderr)
            unasked.append(key)
        time.sleep(1)  # crates.io asks crawlers for one request a second
    os.makedirs(os.path.dirname(CACHE), exist_ok=True)
    with open(CACHE + ".tmp", "w", encoding="utf-8") as f:
        json.dump(cache, f, indent=0, sort_keys=True)
    os.replace(CACHE + ".tmp", CACHE)

    failed = []
    for (name, version), users in sorted(where.items()):
        expr = cache.get(f"{name} {version}")
        if f"{name} {version}" in unasked:
            continue
        if not expr:
            failed.append((name, version, "(no licence on crates.io)", users))
        elif not passes(expr) and name not in EXCEPTIONS:
            failed.append((name, version, expr, users))
    print(f"licence-audit: {len(locks)} lockfiles, {len(where)} crate versions, "
          f"{len(failed)} failing, {len(unasked)} not checked")
    for name, version, expr, users in failed:
        print(f"  FAIL {name} {version}: {expr}  (used by {', '.join(sorted(users))})")
    for key in unasked:
        print(f"  UNCHECKED {key}")
    if failed:
        return 1
    return 2 if unasked else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
