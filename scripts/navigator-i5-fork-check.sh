#!/bin/sh
# FreeBSD guest gate. Pass the signed, installed offline probe bundle.
set -eu
[ "$#" = 1 ] || { echo "usage: $0 INSTALLED_FORK_CHECK_BUNDLE" >&2; exit 2; }
bundle=$(realpath "$1")
mount -p | awk '$2=="/" && $3=="tessera" {ok=1} END {exit !ok}'
out=$(mktemp)
trap 'rm -f "$out"' EXIT
if ! timeout 105 portcullis exec --user _navpage --require-memory-limit --instance "i5fork$$" "$bundle" </dev/null >"$out" 2>&1; then
    cat "$out" >&2
    exit 1
fi
cat "$out"
awk '
/^fork-check fonts=[01] trial=[012] pid=[0-9]+ fork_us=[0-9]+ first_scene_us=[0-9]+ editor_us=[0-9]+ status=0$/ {
    if (seen[$2,$3]++ || pids[$4]++) bad=1
    completed++
}
/^viewport_scroll_top=24$/ {editors++}
/^fork-check event-loop threaded=[01] extra_fds=[0-9]+$/ {loop++}
/^fork-check agent threaded=[01] extra_fds=[0-9]+$/ {agent++}
END {exit (bad || completed!=6 || editors!=6 || loop!=1 || agent!=1)}
' "$out"
