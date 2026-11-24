#!/bin/sh
# Inside the private 1280x720 Sheets desktop fixture with one Scrinium place.
# Seed that place with "native_memory make dense dense.osheet 1". Stage the
# native_memory example under /var/tmp/memory-scale. Give the evidence folder
# room for three dense files, on another volume if the root is small.
set -eu
cd /var/tmp/sheets-desktop
book=/var/tmp/sheets-format-input/dense.osheet
lab=/var/tmp/memory-scale
out=${1:?usage: sheets-dense-recovery-vm-check.sh NEW_EVIDENCE_DIR}
mkdir "$out"
input() { printf '%s\n' "$@" | nc -N -U inject.sock >/dev/null; }
click() { input "MOVE $1 $2" 'BTN 1 1' 'BTN 1 0'; }
key() { input "KEY $1 1 ${2:-0}" "KEY $1 0 ${2:-0}"; }
wait_for() {
    limit=$1
    shift
    stop=$(($(date +%s) + limit))
    while ! "$@"; do
        if [ "$(date +%s)" -ge "$stop" ]; then
            echo "Timed out waiting for $*"
            return 1
        fi
        sleep .25
    done
}
win_at() {
    input "MOVE $1 $2"
    awk -v x="$1" -v y="$2" '$2=="inject" && $3=="MOVE" && $4==x && $5==y {w=$NF} END{print w}' select-fresco.log
}
new_window() { window=$(win_at "$1" "$2"); test "$window" -gt 0 && test "$window" != "$3"; }
same_window() { test "$(win_at "$1" "$2")" = "$3"; }
docs() { jls -n jid name | awk '/org_atrium_sheets__document_/ {sub(/^jid=/,"",$1);print $1}'; }
one_doc() { test "$(docs | wc -l | tr -d ' ')" -eq 1; }
none() { test -z "$(docs)"; }
pick() {
    prior=$1
    click 48 24
    wait_for 10 new_window 520 183 "$prior"
    click 520 183
    click 824 458
    wait_for 120 new_window 594 24 "$prior"
    wait_for 8 one_doc
    document=$window
}
published() { test "$(stat -f %i "$book")" != "$before"; }
close_front() { click 543 242; key 0x1a 1; }

launcher=$(win_at 48 24)
test "$launcher" -gt 0
pick "$launcher"
first=$(docs)
root=$(jls -j "$first" path)/atrium/recovery
dense_checkpoint() {
    for current in "$root"/*/current; do
        test -f "$current" || continue
        dir=$(dirname "$current")
        test "$(cat "$dir/name")" = dense.osheet || continue
        generation=$(cat "$current")
        base="$dir/base.$generation.osheet"
        journal="$dir/journal.$generation"
        test "$(stat -f %z "$base")" = 1065968177 && return 0
    done
    return 1
}
wait_for 180 dense_checkpoint
df -k "$root"
cp selectscreen.png "$out/open.png"
click 60 111; key 0x25; key 0x28
wait_for 20 test -s "$journal"
sleep 3
echo "durable generation $generation base=$(stat -f %z "$base") journal=$(stat -f %z "$journal")"
cp selectscreen.png "$out/unsaved.png"
pid=$(ps -axo jid,pid,comm | awk -v j="$first" '$1==j && $3~/atrium-sheets$/ {print $2}')
test "$(ps -p "$pid" -o jid= | tr -d ' ')" = "$first"
kill -KILL "$pid"
wait_for 60 none
# Keep the original separate so the recovery Save As suggestion is unambiguous.
mv "$book" "$out/original.osheet"
wait_for 30 same_window 48 24 "$launcher"
click 213 24
wait_for 60 one_doc
recovered=$(docs)
test "$recovered" != "$first"
wait_for 90 new_window 205 24 "$launcher"
sleep 8
cp selectscreen.png "$out/recovered.png"
echo "recovered document jail $recovered after crash of $first"
docwin=$(win_at 205 24)
click 205 24
wait_for 60 new_window 824 457 "$docwin"
click 824 457
full_copy() { test -f "$book" && test "$(stat -f %z "$book")" = 1065968177; }
wait_for 120 full_copy
wait_for 60 same_window 125 24 "$docwin"
cp "$book" "$out/recovered-copy.osheet"
before=$(stat -f %i "$book")
click 60 111; key 0x26; key 0x28; sleep 1; click 125 24
wait_for 120 published
sleep 2
close_front
wait_for 60 none
"$lab/native_memory" check dense "$out/original.osheet" 1
"$lab/native_memory" check dense "$out/recovered-copy.osheet" 8
"$lab/native_memory" check dense "$book" 9
wait_for 30 same_window 48 24 "$launcher"
pick "$launcher"
reopened=$(docs)
test "$reopened" != "$recovered"
root=$(jls -j "$reopened" path)/atrium/recovery
wait_for 180 dense_checkpoint
cp selectscreen.png "$out/reopened.png"
# The fresh jail has written its own base from the selected file.
cp "$base" "$out/reopened.osheet"
close_front
wait_for 60 none
"$lab/native_memory" check dense "$out/reopened.osheet" 9
echo "PASS dense crash recovery, Save As, ordinary Save and picker reopen in jail $reopened"
