#!/bin/sh
# Run in the private Sheets desktop fixture. Pass tessera or ufs for the
# filesystem behind the Scrinium place. Stage check_recovered_copy in /tmp.
set -eu
case "${1:-}" in tessera|ufs) filesystem=$1 ;; *) exit 2 ;; esac
cd /var/tmp/sheets-desktop
books=/var/tmp/sheets-select-input
results=$(mktemp -d /var/tmp/sheets-versions-results.XXXXXX)
echo "Sheets versions evidence: $results"
test "$(ls -A "$books" | sed '/^\.snap$/d')" = Proof.csv

input() { printf '%s\n' "$@" | nc -N -U inject.sock >/dev/null; }
click() { input "MOVE $1 $2" 'BTN 1 1' 'BTN 1 0'; }
key() { input "KEY $1 1 ${2:-0}" "KEY $1 0 ${2:-0}"; }
wait_for() {
    for i in $(jot 200); do "$@" && return 0; sleep .1; done
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
edited() { test -f "$books/Untitled.osheet" && /tmp/check-recovered-copy "$books/Untitled.osheet" "$1" plain 2>/dev/null; }
edit() { click 60 112; key "$1"; key 0x28; }
revision_loaded() {
    base_dir=$(jls -j "$1" path)/atrium/recovery
    generation=$(find "$base_dir" -name current -type f)
    test -n "$generation" || return 1
    /tmp/check-recovered-copy "$(dirname "$generation")/base.$(cat "$generation").osheet" "$2" plain 2>/dev/null
}
browse() {
    click 594 24
    wait_for other_window 656 130 "$document"
    click 656 130
    wait_for other_window 300 125 "$document"
}

launcher=$(win_at 48 24)
click 125 24
wait_for count 1
current=$(docs)
wait_for other_window 594 24 "$launcher"
document=$window
edit 0x1e # A1=1
click 205 24
wait_for other_window 824 458 "$document"
click 824 458
wait_for edited 1
wait_for same_window 824 458 "$document"
edit 0x1f # A1=2
click 125 24
wait_for edited 2
sleep .5
edit 0x20 # A1=3
click 125 24
wait_for edited 3
live_hash=$(sha256 -q "$books/Untitled.osheet")

if test "$filesystem" = tessera; then
    # Closing and reopening Sheets must not lose the current session's history.
    click 60 112
    key 0x1a 1
    wait_for count 0
    click 48 24
    wait_for other_window 520 213 "$launcher"
    click 520 213
    sleep .25
    click 824 458
    wait_for count 1
    current=$(docs)
    wait_for revision_loaded "$current" 3
    wait_for other_window 594 24 "$launcher"
    document=$window

    # The newest entry should hold 2 and the second entry 1. Each
    # selection opens a fresh jail without closing the current workbook.
    for version in 2 1; do
        browse
        sleep .5
        cp selectscreen.png "$results/list-$version.png"
        if test "$version" = 2; then row=138; else row=174; fi
        click 360 "$row"
        wait_for count 2
        historical=$(docs | grep -vx "$current")
        wait_for revision_loaded "$historical" "$version"
        wait_for other_window 125 24 "$document"
        old_window=$window
        cp selectscreen.png "$results/open-$version.png"
        # Save on the historical window asks for a new file and does
        # not update the live workbook.
        click 125 24
        wait_for other_window 824 458 "$old_window"
        cp selectscreen.png "$results/save-copy-$version.png"
        click 750 458
        wait_for same_window 824 458 "$old_window"
        click 60 112
        key 0x1a 1
        wait_for count 1
        test "$(docs)" = "$current"
        wait_for same_window 594 24 "$document"
        test "$(sha256 -q "$books/Untitled.osheet")" = "$live_hash"
    done
else
    browse
    empty_history() { test "$(sha256 -q selectscreen.png)" = 1ee000a93d0c4d4dd5bc36f36eca1f487b515b556b944e722a4ba2f83e204832; }
    wait_for empty_history
    cp selectscreen.png "$results/ufs-empty.png"
    click 360 138
    count 1
    click 824 458
    wait_for same_window 594 24 "$document"
    edit 0x21 # The ordinary Save grant is still usable on UFS.
    click 125 24
    wait_for edited 4
fi
click 60 112
key 0x1a 1
wait_for count 0
mv "$books/Untitled.osheet" "$results/"
echo "Sheets Browse Versions passed on $filesystem: $results"
