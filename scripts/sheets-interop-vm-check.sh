#!/bin/sh
# Run in the private 1280x720 Sheets desktop fixture. Stage the two books
# from the sheets_interop_fixture example in the only Scrinium place, and
# stage the arm64 FreeBSD build of that example at /tmp/sheets-interop-fixture.
# Set SHEETS_INTEROP_CASE=tables for the sheets_table_chart_fixture inputs
# and stage its binary at /tmp/sheets-table-chart-fixture instead.
set -eu
cd /var/tmp/sheets-desktop
books=${1:-/var/tmp/sheets-format-input}
profile=${SHEETS_INTEROP_CASE:-pivots}
case "$profile" in pivots|tables) ;; *) exit 2 ;; esac
results=$(mktemp -d /var/tmp/sheets-interop-results.XXXXXX)
echo "Sheets interoperability evidence: $results"
test "$(ls "$books")" = "$(printf 'sales.ods\nsales.xlsx')"

input() { printf '%s\n' "$@" | nc -N -U inject.sock >/dev/null; }
click() { input "MOVE $1 $2" 'BTN 1 1' 'BTN 1 0'; }
key() { input "KEY $1 1 ${2:-0}" "KEY $1 0 ${2:-0}"; }
wait_for() {
    for i in $(jot 180); do "$@" && return 0; sleep .1; done
    echo "Timed out waiting for $*" >&2
    return 1
}
win_at() {
    input "MOVE $1 $2"
    awk -v x="$1" -v y="$2" '$2=="inject" && $3=="MOVE" && $4==x && $5==y {w=$NF} END{print w}' select-fresco.log
}
other_window() { window=$(win_at "$1" "$2"); test "$window" -gt 0 && test "$window" != "$3"; }
same_window() { test "$(win_at "$1" "$2")" = "$3"; }
docs() { jls -n jid name | awk '/org_atrium_sheets__document_/ {sub(/^jid=/,"",$1);print $1}'; }
closed() { test -z "$(docs)"; }
checkpoint() {
    if test "$profile" = tables; then
        /tmp/sheets-table-chart-fixture check "$1" "$2" "$format"
    else
        /tmp/sheets-interop-fixture check "$1" "$2"
    fi
} 2>"$results/checkpoint.log"
loaded() {
    jail=$(docs)
    test -n "$jail" || return 1
    root=$(jls -j "$jail" path)/atrium/recovery
    directory=$(dirname "$(find "$root" -name name -type f -exec grep -slx "$1" {} +)")
    test "$directory" != . && test -s "$directory/current" || return 1
    checkpoint "$directory/base.$(cat "$directory/current").osheet" "$2"
}
worker_count() { grep -Fc 'worker org.atrium.sheets/convert for alice runs as ' select-port.log || :; }
worker_started() { test "$(worker_count)" -gt "$1"; }
open_book() {
    before=$(worker_count)
    click 48 24
    wait_for other_window 520 "$1" "$launcher"
    click 520 "$1"
    click 824 458
    wait_for loaded "$2" "$3"
    wait_for worker_started "$before"
    wait_for other_window 594 24 "$launcher"
    document=$window
}
save() {
    sleep .45
    click 125 24
    wait_for checkpoint "$books/$1" "$2"
    sleep .4
}
close_book() {
    click 543 242
    key 0x1a 1
    wait_for closed
}
refresh_all() {
    click 594 24
    wait_for other_window 645 18 "$document"
    input 'MOVE 646 18'
    sleep .4
    # Start on Compare with… and wrap up from the bottom of the menu.
    # In this single-sheet fixture, Refresh All is sixth from the last
    # enabled item. The following checkpoint verifies the command.
    for i in $(jot 6); do key 0x52; sleep .1; done
    sleep .3
    key 0x28
    wait_for same_window 645 18 "$document"
}

pivots() {
    click 126 133 # B2 feeds the spill, LET, LAMBDA, and the pivot.
    key 0x25 # 8
    key 0x28 # Enter
    save "$book" edited
    cp selectscreen.png "$results/$format-before-refresh.png"

    refresh_all
    save "$book" refreshed
    cp selectscreen.png "$results/$format-refreshed.png"

    click 370 24 # Undo only the pivot refresh.
    save "$book" edited
    click 445 24 # Redo it.
    save "$book" refreshed
    final=refreshed
}

tables() {
    click 383 133 # F2 feeds both named formulas, the table sum, and the chart.
    key 0x25 # 8
    key 0x28
    save "$book" edited
    cp selectscreen.png "$results/$format-edited.png"

    click 83 112 # Open the Region filter and add South to the included values.
    sleep .45
    click 820 240
    sleep .2
    click 1054 663
    save "$book" filtered
    cp selectscreen.png "$results/$format-filtered.png"

    click 370 24
    save "$book" restored
    click 445 24
    save "$book" filtered
    final=filtered
}

launcher=$(win_at 48 24)
test "$launcher" -gt 0
for format in ods xlsx; do
    case "$format" in ods) row=183 ;; xlsx) row=213 ;; esac
    book=sales.$format
    checkpoint "$books/$book" initial
    open_book "$row" "$book" initial
    sleep .4
    cp selectscreen.png "$results/$format-initial.png"

    "$profile"
    close_book

    open_book "$row" "$book" "$final"
    sleep .4
    cp selectscreen.png "$results/$format-reopened.png"
    close_book
    echo "PASS $format $profile edit, undo, redo, and converter reopen"
done
