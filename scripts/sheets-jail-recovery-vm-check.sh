#!/bin/sh
# Run inside the private 1280x720 Sheets desktop fixture with its launcher
# frontmost and one Scrinium place containing Proof.csv (A1 is Original).
set -eu
cd /var/tmp/sheets-desktop
log=${FRESCO_LOG:-persist-fresco.log}
source=${WORKBOOK_DIR:-/var/tmp/sheets-teardown-input}/Proof.csv
test "$(cat "$source")" = Original
input() { printf '%s\n' "$@" | nc -N -U inject.sock >/dev/null; }
click() { input "MOVE $1 $2" 'BTN 1 1' 'BTN 1 0'; }
key() { input "KEY $1 1 ${2:-0}" "KEY $1 0 ${2:-0}"; }
frame() { awk '$1=="desktop-open" && $2=="frame"{f=$5} END{print f}' "$log"; }
wait_for() {
    for i in $(jot 100); do "$@" && return 0; sleep .1; done
    echo "Timed out waiting for $*" >&2; return 1
}
shows() { test "$(frame)" = "$1"; }
jids() { jls -n jid name | awk '/org_atrium_sheets__document_/ {sub(/^jid=/,"",$1);print $1}'; }
one() { test "$(jids | wc -w)" -eq 1; }
two() { test "$(jids | wc -w)" -eq 2; }
none() { test -z "$(jids)"; }
journal() { find "$1/atrium/recovery" -name journal.1 -type f; }
has_edit() { strings -n 5 "$1" | grep -Fxq zebra; }
window_at() {
    input "MOVE $1 $2"
    awk -v x="$1" -v y="$2" '$2=="inject" && $3=="MOVE" && $4==x && $5==y {w=$NF} END{print w}' "$log"
}
wait_for none
launcher=$(window_at 48 24)
test "$launcher" -gt 0
at_launcher() { test "$(window_at 48 24)" = "$launcher"; }
at_picker() { win=$(window_at 488 183); test "$win" -gt 0 && test "$win" != "$launcher"; }
click 48 24
wait_for at_picker
click 488 183
sleep .5
key 0x28
wait_for one
first=$(jids)
root=$(jls -j "$first" path)
wait_for shows 63d21a3ad8d4dda7
j=$(journal "$root")
test -f "$j" && test ! -s "$j"
click 62 112
for k in 0x1d 0x08 0x05 0x15 0x04 0x28; do key "$k"; done
wait_for has_edit "$j"
wait_for shows 1915e3f75244f78d
click 515 24
wait_for two
sibling=$(jids | grep -vx "$first")
other=$(jls -j "$sibling" path)
sibling_ready() { test -n "$(journal "$other")"; }
wait_for sibling_ready
own=$(journal "$other")
user=$(stat -f %Su "$own")
# cat runs as the sibling app UID inside its own root. Check a readable
# journal first so a missing runtime or a bad UID cannot pass the denial.
install -m 555 /bin/cat "$other/recovery-test-cat"
test "$(jexec -u "$user" "$sibling" /recovery-test-cat "${own#"$other"}" | wc -c)" -eq 0
test "$(jexec -u "$user" "$sibling" /recovery-test-cat "$(dirname "${own#"$other"}")/name")" = Untitled
if jexec -u "$user" "$sibling" /recovery-test-cat "${j#"$root"}" 2>/dev/null; then exit 1; fi
pid=$(ps -axo jid,pid,comm | awk -v jid="$first" '$1==jid && $3~/atrium-sheets$/ {print $2}')
test "$(ps -p "$pid" -o jid= | tr -d ' ')" = "$first"
kill -KILL "$pid"
wait_for one
test "$(jids)" = "$sibling"
test "$(cat "$source")" = Original
key 0x1a 1
wait_for none
wait_for at_launcher
click 213 24
wait_for one
restored=$(jids)
test "$restored" != "$first"
wait_for shows 9bf0430e59cd84bd
j=$(journal "$(jls -j "$restored" path)")
has_edit "$j"
key 0x1a 1
sleep .5
click 820 70
wait_for none
wait_for at_launcher
test "$(cat "$source")" = Original
no_pending() { test -z "$(find /home/alice/.atrium/apps -maxdepth 2 -name '*.pending')"; }
wait_for no_pending
echo "Recovered A1 in jail $restored after jail $first exited; sibling $sibling stayed isolated. Discard cleared recovery."
