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
# The repo config shares Cargo intermediates via CARGO_HOME/atrium-build.
# For historical revisions, an ancestor config supplies the same setting.
# Each crate keeps its own final binaries in target/ for staging and tests.
#
# Cargo locks the shared build directory, so concurrent builds may wait.
# Keep the parent config scoped to the worktrees it serves.
#
# Remove scratch worktrees with --remove (git worktree remove, which never
# follows symlinks), not rm -r.
set -eu

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SHARED='{cargo-cache-home}/atrium-build'

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
# The config lands in $PARENT/.cargo, and Cargo applies it to every project
# below $PARENT. A parent that is an ancestor of the main checkout or of $HOME
# (e.g. ~/src) would route the main checkout and every unrelated project into
# the shared build-dir; that happened on 2026-11-15 via ~/src/.cargo. Refuse.
for ANC in "$REPO" "$HOME" "$HOME/src"; do
    case "$ANC/" in
        "${PARENT%/}"/*)
            echo "error: $DEST's parent $PARENT is $ANC or one of its ancestors;" >&2
            echo "       $PARENT/.cargo/config.toml would apply to everything under it." >&2
            echo "       Put the worktree one level deeper, e.g. ~/src/bsd-scratch/<name>" >&2
            git -C "$REPO" worktree remove --force "$DEST"
            exit 1 ;;
    esac
done
mkdir -p "$PARENT/.cargo"
CFG="$PARENT/.cargo/config.toml"
if [ -f "$CFG" ] && ! grep -Fxq "build-dir = \"$SHARED\"" "$CFG"; then
    echo "error: set build-dir = \"$SHARED\" under [build] in $CFG first" >&2
    git -C "$REPO" worktree remove "$DEST"
    exit 1
fi
[ -f "$CFG" ] || printf '# written by %s\n[build]\nbuild-dir = "%s"\n' "scripts/scratch-worktree.sh" "$SHARED" > "$CFG"

echo "worktree: $DEST ($(git -C "$DEST" rev-parse --short HEAD))"
echo "shared build-dir: $SHARED (via $CFG)"
