#!/bin/sh
set -eu
# Run on FreeBSD with cross-built artifacts on local disk. The stage holds
# frescod-i0, editor-parent and the signed renderer/ bundle. Keep logs by run.
[ "$#" = 2 ] || { echo "usage: $0 STAGE SIGNED_EDITOR_BUNDLE" >&2; exit 2; }
stage=$(realpath "$1")
bundle=$(realpath "$2")
trial() (
    # The launcher creates traversable runtime directories for the dropped uid.
    # mktemp below keeps the sockets and logs private.
    umask 022
    d=$(mktemp -d /tmp/i0-display.XXXXXX)
    echo "I0 raster logs: $d"
    cd "$d"
    export FRESCOD_SOCK="$d/app.sock" FRESCOD_ACTIVATION_SOCK="$d/act.sock" FRESCOD_WM_SOCK="$d/wm.sock" FRESCOD_CHROME_SOCK="$d/chrome.sock"
    export FRESCOD_INPUT_SOCK="$d/input.sock" FRESCOD_HEADLESS_PNG="$d/screen" FRESCOD_SPLIT=1 FRESCOD_SPLIT_PIXELS=1 FRESCOD_RENDERER=compute FRESCOD_SLOT_UIDS=0
    export FRESCOD_RENDERER_LAUNCHER="/usr/local/bin/portcullis exec --user _fresco-render --require-memory-limit --instance i0r{client} $stage/renderer"
    "$stage/frescod-i0" >frescod.log 2>&1 &
    pid=$!
    trap 'kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true' EXIT
    for n in $(jot 80); do test -S "$d/act.sock" && break; kill -0 "$pid"; sleep .25; done
    test -S "$d/act.sock"
    timeout 100 "$stage/editor-parent" "$d" "$bundle" >"run-$run.log" 2>&1 || { cat "run-$run.log"; tail -35 frescod.log; exit 1; }
    cat "run-$run.log"
)
for run in 1 2 3; do
    trial
done
