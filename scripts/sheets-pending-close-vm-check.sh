#!/bin/sh
# Run in the disposable headless Sheets desktop fixture, with Proof.csv (one
# cell containing 1) open in the frontmost document jail. The named fixture
# owns its own Scrinium, input socket and workbook directory.
set -eu
cd /var/tmp/sheets-desktop
test -S inject.sock
test "$(cat workbooks/Proof.csv)" = 1
test ! -e workbooks/Proof.osheet
picker=$(pgrep -U sheetsfiles -x scrinium)
test "$(procstat -b "$picker" | awk 'NR==2 {print $NF}')" = "$PWD/scrinium"
doc_pid() {
    ps -axo jid,pid,comm | awk '$1 > 0 && $3 ~ /atrium-sheets$/ {print $1, $2}' | sort -nr | awk 'NR==1 {print $2}'
}
doc=$(doc_pid)
test -n "$doc"
trap 'if [ -n "$picker" ]; then kill -CONT "$picker" 2>/dev/null || :; fi' EXIT
input() { printf '%s\n' "$@" | nc -N -U inject.sock >/dev/null; }
click() { input "MOVE $1 $2" 'BTN 1 1' 'BTN 1 0'; }
key() { input "KEY $1 1 ${2:-0}" "KEY $1 0 ${2:-0}"; }
edit() { click "$1" 112; key "$2"; key 0x28; sleep .3; }
close_window() { key 0x1a 1; }
save() { click 128 24; sleep .4; }
contents() { tr -d '\r' < workbooks/Proof.csv; }
wait_for() {
    for i in $(jot 60); do "$@" && return 0; sleep .2; done
    echo "Timed out waiting for $*" >&2; return 1
}
has_csv() { test "$(contents)" = "$1"; }
exited() { ! kill -0 "$doc" 2>/dev/null; }
# Close the document twice while the save is queued, then make a late edit.
edit 62 0x1f
kill -STOP "$picker"
save
close_window; close_window
edit 127 0x20
kill -0 "$doc"
has_csv 1
kill -CONT "$picker"
wait_for has_csv 2
sleep .5
kill -0 "$doc"
save
wait_for has_csv '2,3'
kill -0 "$doc"
echo 'Late edit survived acknowledgement and saved on the next Save.'

# Lose the service after requesting close, before confirmation.
edit 254 0x22
kill -STOP "$picker"
save; close_window; sleep .5
kill -0 "$doc"
kill -KILL "$picker"
picker=''
sleep 2
kill -0 "$doc"
has_csv '2,3'
cp screen.png close-without-confirmation.png
# Restore the service, recover the open document under a fresh grant, and
# check another edit is persisted through that grant.
env LIMEN_SOCKET=/atrium/sockets/limen/limend.sock FRESCO_SOCKET=/atrium/sockets/fresco/fresco.sock \
    SCRINIUM_PLACES="Workbooks=$PWD/workbooks" daemon -f -u sheetsfiles -o "$PWD/scrinium-restarted.log" "$PWD/scrinium" --no-window
sleep 1
parent_window=$(awk '/inject BTN 1 0 -> window/ {w=$NF} END {print w}' frescod.log)
click 211 24
picker_visible() {
    input 'MOVE 824 457'
    window=$(awk '/inject MOVE 824 457 -> window/ {w=$NF} END {print w}' frescod.log)
    test -n "$window" && test "$window" != 0 && test "$window" != "$parent_window"
}
wait_for picker_visible
click 824 457
wait_for test -s workbooks/Proof.osheet
sleep .5
hash=$(sha256 -q workbooks/Proof.osheet)
edit 318 0x23; save
changed() { test "$(sha256 -q workbooks/Proof.osheet)" != "$hash"; }
wait_for changed
kill -0 "$doc"
has_csv '2,3'
echo 'Lost acknowledgement kept the window open for Save As and a subsequent Save.'

# Matching acknowledgement closes the recovered document only after Save.
hash=$(sha256 -q workbooks/Proof.osheet)
picker=$(pgrep -U sheetsfiles -x scrinium)
edit 382 0x24
kill -STOP "$picker"
save; close_window; sleep .5
kill -0 "$doc"
test "$(sha256 -q workbooks/Proof.osheet)" = "$hash"
kill -CONT "$picker"
wait_for exited
changed
echo 'Matching acknowledgement closed the document and persisted the revision.'
