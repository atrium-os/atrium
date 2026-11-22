#!/bin/sh
# Run in the isolated 1280x720 Sheets desktop fixture.
# Scrinium must use BOOKS as its home root. Its private bookmark state
# starts empty and persists through the guest restart between the phases.
set -eu
phase=${1:?issue, verify, or copy}
books=${2:?fixture home root}
backing=${3:-"$books"}
cd /var/tmp/sheets-desktop
results=$(mktemp -d /var/tmp/sheets-recent-results.XXXXXX)
echo "Sheets Recent Workbooks evidence: $results"

input() { printf '%s\n' "$@" | nc -N -U inject.sock >/dev/null; }
click() { input "MOVE $1 $2" 'BTN 1 1' 'BTN 1 0'; }
key() { input "KEY $1 1 ${2:-0}" "KEY $1 0 ${2:-0}"; }
wait_for() {
    for i in $(jot 160); do "$@" && return 0; sleep .15; done
    echo "Timed out waiting for $*" >&2
    return 1
}
docs() { jls -n jid name | awk '/org_atrium_sheets__document_/ {sub(/^jid=/,"",$1);print $1}'; }
count() { test "$(docs | wc -w)" -eq "$1"; }
win_at() {
    input "MOVE $1 $2"
    awk -v x="$1" -v y="$2" '$2=="inject" && $3=="MOVE" && $4==x && $5==y {w=$NF} END{print w}' select-fresco.log
}
other_window() { window=$(win_at "$1" "$2"); test "$window" -gt 0 && test "$window" != "$3"; }
same_window() { test "$(win_at "$1" "$2")" = "$3"; }
native_cell() { test -f "$1" && /tmp/check-recovered-copy "$1" "$2" plain 2>/dev/null; }
cell() { test "$(head -n1 "$1" | tr -d '\r')" = "$2"; }
loaded() {
    root=$(jls -j "$(docs)" path)/atrium/recovery
    generation=$(find "$root" -name current -type f)
    test -n "$generation" || return 1
    native_cell "$(dirname "$generation")/base.$(cat "$generation").osheet" "$1"
}
close_document() {
    click 60 112
    key 0x1a 1
    wait_for count 0
}
open_picker_row() {
    click 48 24
    wait_for other_window 520 "$1" "$launcher"
    click 520 "$1"
    click 824 458
    wait_for count 1
}
refusals() { grep -Fc 'notice=“Replaced.csv” is no longer available.' select-files.log || :; }
refused() { test "$(refusals)" -gt "$1"; }
open_recent() {
    click 351 24
    wait_for other_window 500 111 "$launcher"
}

wait_for count 0
launcher=$(win_at 48 24)
test "$launcher" -gt 0
case "$phase" in
    issue)
        cell "$books/Documents/Moving.csv" 41
        cell "$books/Documents/Replaced.csv" 92
        open_picker_row 183
        wait_for loaded 41
        close_document
        open_picker_row 213
        wait_for loaded 92
        close_document

        original=$(stat -f %i "$books/Documents/Moving.csv")
        replaced=$(stat -f %i "$books/Documents/Replaced.csv")
        checksum=$(sha256 -q "$books/Documents/Replaced.csv")
        mv "$backing/Documents/Moving.csv" "$backing/Downloads/Moved.csv"
        test "$original" = "$(stat -f %i "$books/Downloads/Moved.csv")"
        printf '99\r\n' > "$books/Documents/Moving.csv"
        cp -p "$books/Documents/Replaced.csv" "$books/Documents/.replacement"
        mv -f "$books/Documents/.replacement" "$books/Documents/Replaced.csv"
        test "$replaced" != "$(stat -f %i "$books/Documents/Replaced.csv")"
        test "$checksum" = "$(sha256 -q "$books/Documents/Replaced.csv")"
        sync
        echo 'PASS issued both bookmarks, moved the first, and replaced the second with identical bytes'
        ;;
    verify)
        open_recent
        cp selectscreen.png "$results/recent.png"
        before=$(refusals)
        click 500 111
        wait_for refused "$before"
        count 0
        cp selectscreen.png "$results/refused.png"
        click 500 146
        wait_for count 1
        wait_for loaded 41
        cp selectscreen.png "$results/moved.png"
        original=$(stat -f %i "$books/Downloads/Moved.csv")
        click 60 112
        key 0x24 # A1=7
        key 0x28
        click 125 24
        wait_for cell "$books/Downloads/Moved.csv" 7
        test "$original" != "$(stat -f %i "$books/Downloads/Moved.csv")"
        cell "$books/Documents/Moving.csv" 99
        cell "$books/Documents/Replaced.csv" 92
        close_document

        open_recent
        click 500 111
        wait_for count 1
        wait_for loaded 7
        cp selectscreen.png "$results/reopened-save.png"
        close_document
        echo 'PASS moved original reopened after restart, replacements stayed untouched, and ordinary Save refreshed the bookmark'
        ;;
    copy)
        open_recent
        click 500 111
        wait_for count 1
        wait_for loaded 7
        document=$(win_at 205 24)
        click 205 24
        wait_for other_window 824 458 "$document"
        click 824 458
        copy="$books/Documents/Moved.osheet"
        wait_for native_cell "$copy" 7
        wait_for same_window 824 458 "$document"
        close_document

        open_recent
        click 500 111
        wait_for count 1
        wait_for loaded 7
        wait_for grep -Fq '“Moved.osheet” opened in its document jail' select-port.log
        cp selectscreen.png "$results/reopened-copy.png"
        close_document
        cell "$books/Downloads/Moved.csv" 7
        echo 'PASS Save As registered a separate workbook and Recent Workbooks reopened it'
        ;;
    *) exit 2 ;;
esac
