#!/bin/sh
# scratch-worktree.sh — make a throwaway git worktree of this repo (for a
# baseline build, an A/B arm, a bisect step) WITHOUT a full private build tree.
#
#   scripts/scratch-worktree.sh <dest> [<rev>]      (rev defaults to HEAD)
#   scripts/scratch-worktree.sh --remove <dest>
#
# Why: every crate here has its own target/, so a plain `git worktree add`
# rebuilds every dependency from scratch, 15-33 GB per worktree. On
# 2026-11-06 idle scratch worktrees held ~117 GB of the Mac's disk.
#
# How: we write <dest>/../.cargo/config.toml setting Cargo's build-dir to one
# shared folder (ATRIUM_SCRATCH_BUILD_DIR, default ~/.cache/atrium-scratch-build).
# Cargo reads config from every ancestor directory, so it applies to builds in
# the worktree without touching any tracked file. Intermediate artifacts
# (deps, fingerprints, incremental) go to the shared folder; each crate's
# target/ keeps only final binaries, so two arms never overwrite each other's
# output. A dependency compiled once is reused by every scratch worktree. A
# workspace crate from a different checkout path gets its own hash, so arms
# never share each other's own code.
#
# Cost: Cargo locks the build-dir, so two scratch builds running at the same
# time may wait for each other ("Blocking waiting for file lock"). The main
# checkout is NOT affected; it keeps its own target/ dirs.
#
# Remove scratch worktrees with --remove (git worktree remove, which never
# follows symlinks), not rm -r.
set -eu

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SHARED="${ATRIUM_SCRATCH_BUILD_DIR:-$HOME/.cache/atrium-scratch-build}"

if [ "${1:-}" = "--remove" ]; then
    [ $# -eq 2 ] || { echo "usage: $0 --remove <dest>" >&2; exit 2; }
    git -C "$REPO" worktree remove --force "$2"
    git -C "$REPO" worktree prune
    exit 0
fi

[ $# -ge 1 ] && [ $# -le 2 ] || { echo "usage: $0 <dest> [<rev>] | --remove <dest>" >&2; exit 2; }
DEST="$1"
REV="${2:-HEAD}"

git -C "$REPO" worktree add --detach "$DEST" "$REV"

PARENT="$(cd "$(dirname "$DEST")" && pwd)"
case "$PARENT" in
    "$REPO"|"$REPO"/*)
        echo "error: $DEST is inside the main checkout; put scratch worktrees elsewhere" >&2
        git -C "$REPO" worktree remove --force "$DEST"
        exit 1 ;;
esac
mkdir -p "$PARENT/.cargo" "$SHARED"
CFG="$PARENT/.cargo/config.toml"
if [ -f "$CFG" ] && ! grep -q '^build-dir' "$CFG"; then
    echo "error: $CFG exists without a build-dir; add  [build] build-dir = \"$SHARED\"  by hand" >&2
    exit 1
fi
[ -f "$CFG" ] || printf '# written by %s\n[build]\nbuild-dir = "%s"\n' "scripts/scratch-worktree.sh" "$SHARED" > "$CFG"

echo "worktree: $DEST ($(git -C "$DEST" rev-parse --short HEAD))"
echo "shared build-dir: $SHARED (via $CFG)"
