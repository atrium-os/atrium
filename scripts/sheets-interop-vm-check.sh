#!/bin/sh
# Run in the private 1280x720 Sheets desktop fixture. Stage the two books
# from the sheets_interop_fixture example in the only Scrinium place, and
# stage the arm64 FreeBSD build of that example at /tmp/sheets-interop-fixture.
# Set SHEETS_INTEROP_CASE=tables for the sheets_table_chart_fixture inputs
# and stage its binary at /tmp/sheets-table-chart-fixture instead.
# Use SHEETS_INTEROP_CASE=layout and /tmp/sheets-layout-fixture for layout.
# Use SHEETS_INTEROP_CASE=annotations and /tmp/sheets-annotations-fixture for annotations.
# Use SHEETS_INTEROP_CASE=legacy and /tmp/sheets-legacy-fixture for legacy.xls
# and macro.xlsm. Stage the same SimpleMacro.xlsm source at /tmp/sheets-vba-source.xlsm.
# Use SHEETS_INTEROP_CASE=print and /tmp/sheets-print-fixture for print layout.
# The print profile also needs Prelum and the print sheet with no printers
# configured. Run sheets-interop-pdf-check.py on the exported PDFs on the host.
# Use SHEETS_INTEROP_CASE=formulas and /tmp/sheets-formula-fixture for formulas.
set -eu
cd /var/tmp/sheets-desktop
books=${1:-/var/tmp/sheets-format-input}
profile=${SHEETS_INTEROP_CASE:-pivots}
case "$profile" in pivots|tables|layout|annotations|legacy|print|formulas) ;; *) exit 2 ;; esac
results=$(mktemp -d /var/tmp/sheets-interop-results.XXXXXX)
echo "Sheets interoperability evidence: $results"
if test "$profile" = legacy; then
    test "$(ls "$books")" = "$(printf 'legacy.xls\nmacro.xlsm')"
else
    test "$(ls "$books")" = "$(printf 'sales.ods\nsales.xlsx')"
fi

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
    case "$profile" in
        tables) /tmp/sheets-table-chart-fixture check "$1" "$2" "$format" ;;
        layout) /tmp/sheets-layout-fixture check "$1" "$2" ;;
        annotations) /tmp/sheets-annotations-fixture check "$1" "$2" "$format" ;;
        pivots) /tmp/sheets-interop-fixture check "$1" "$2" ;;
        print) /tmp/sheets-print-fixture check "$1" "$2" ;;
        formulas) /tmp/sheets-formula-fixture check "$1" "$2" "$format" ;;
        legacy) /tmp/sheets-legacy-fixture check "$1" "$2" /tmp/sheets-vba-source.xlsm ;;
    esac
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
    if test "${4:-foreign}" = native; then
        test "$(worker_count)" -eq "$before"
    else
        wait_for worker_started "$before"
    fi
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
data_from_bottom() {
    click 594 24
    wait_for other_window 645 18 "$document"
    input 'MOVE 646 18'
    sleep .4
    # Start on Compare with… and wrap up through enabled items.
    for i in $(jot "$1"); do key 0x52; sleep .1; done
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

    data_from_bottom 6 # Refresh All Pivot Tables in the pivot fixture.
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

drag() {
    input "MOVE $1 $2" 'BTN 1 1'
    sleep .25
    input "MOVE $3 $4"
    sleep .25
    input 'BTN 1 0'
}

layout() {
    click 322 205 # D4, beside the hidden row and column.
    key 0x25 # 8
    key 0x28
    save "$book" edited

    drag 277 89 309 89 # Widen column B by 32 pixels.
    save "$book" wider
    drag 20 216 20 240 # Increase row 4 by 24 pixels.
    save "$book" resized

    click 370 24
    save "$book" wider
    click 370 24
    save "$book" edited
    click 445 24
    save "$book" wider
    click 445 24
    save "$book" resized
    cp selectscreen.png "$results/$format-resized.png"
    final=resized
}

scroll_layout() {
    click 350 205
    # Move to the right of the first viewport, then scroll the body down.
    for i in $(jot 25); do key 0x4f; sleep .04; done
    input 'MOVE 605 400' 'SCROLL 440'
    sleep .6
    cp selectscreen.png "$results/$format-scrolled.png"
}

append_x() {
    click 302 681
    key 0x4d # End
    key 0x1b # x
    key 0x28
}

annotations() {
    click 63 112
    data_from_bottom 22 # Edit Comment…
    append_x
    save "$book" note

    click 126 112
    data_from_bottom 22 # Edit Link…
    click 302 681
    key 0x04 1 # Select all
    key 0x09 # f
    key 0x1e # 1
    key 0x28
    save "$book" link

    click 191 112
    data_from_bottom 6 # Data Validation…
    sleep .35
    click 1014 295 # Highest
    key 0x04 1
    key 0x25 # 8
    click 1054 663
    save "$book" rule

    click 254 112
    if test "$format" = xlsx; then
        data_from_bottom 23 # Open Comment Thread…
        append_x
    else
        data_from_bottom 22 # New Comment Thread…
        click 302 681
        key 0x14 # q
        key 0x28
        sleep .45
        click 254 112
        data_from_bottom 23 # Open Comment Thread…
        sleep .35
        click 945 681 # Reply
        click 302 681
        key 0x15 # r
        key 0x28
    fi
    save "$book" thread
    sleep .4
    cp selectscreen.png "$results/$format-annotations-saved.png"
    final=thread
}

check_reopened_validation() {
    click 191 112
    key 0x26 # 9 is refused by the reopened rule.
    key 0x28
    sleep .5
    cp selectscreen.png "$results/$format-validation-refused.png"
    click 191 112
    key 0x25 # 8 is accepted.
    key 0x28
    save "$book" accepted
}

print() {
    click 400 172 # E4, whose edit changes the printed formula in F4.
    key 0x25
    key 0x28
    save "$book" edited
    final=edited
}

pdf_finished() {
    test -s "$1" && tail -c 16 "$1" | grep -q '^%%EOF'
}

pdf_picker_open() {
    other_window 824 458 "$print_window" && test "$window" != "$document"
}

print_pdf() {
    pdf="$books/sales – Report.pdf"
    test ! -e "$pdf"
    click 280 24
    wait_for other_window 829 578 "$document"
    print_window=$window
    sleep 1
    cp selectscreen.png "$results/$format-print.png"
    click 850 551 # Save as PDF is the only destination.
    wait_for pdf_picker_open
    click 824 458 # Confirm the suggested name in the system picker.
    wait_for pdf_finished "$pdf"
    wait_for same_window 824 458 "$document"
    mv "$pdf" "$results/$format.pdf"
}

formulas() {
    click 320 133 # E2. Enter =SUM(D2#), including in the ODS workbook.
    key 0x2e # =
    key 0x16 # s
    key 0x18 # u
    key 0x10 # m
    key 0x26 2 # (
    key 0x07 # d
    key 0x1f # 2
    key 0x20 2 # #
    key 0x27 2 # )
    key 0x28
    click 126 133 # B2. Expand the spill to three rows and produce #DIV/0!.
    key 0x20
    key 0x28
    save "$book" edited
    sleep .8 # The on-disk checkpoint can precede the Save acknowledgment.
    cp selectscreen.png "$results/$format-formulas-saved.png"
    final=edited
}

formulas_reopened() {
    click 126 133 # Resize again to distinguish dynamic and fixed spill references.
    key 0x21 # 4
    key 0x28
    save "$book" resized
    cp selectscreen.png "$results/$format-formulas-resized.png"
    close_book
    open_book "$row" "$book" resized
    sleep .4
    cp selectscreen.png "$results/$format-formulas-reopened.png"
}

legacy() {
    checksum=$(sha256 -q "$books/legacy.xls")
    checkpoint "$books/macro.xlsm" macro-initial
    open_book 213 macro.xlsm macro-initial
    sleep .45
    cp selectscreen.png "$results/xlsm-initial.png"
    click 63 132
    key 0x25 # 8
    key 0x28
    save macro.xlsm macro-edited
    cp selectscreen.png "$results/xlsm-saved.png"
    close_book
    open_book 213 macro.xlsm macro-edited
    sleep .45
    cp selectscreen.png "$results/xlsm-reopened.png"
    close_book

    checkpoint "$books/legacy.xls" xls-initial
    open_book 183 legacy.xls xls-initial
    sleep .45
    cp selectscreen.png "$results/xls-initial.png"
    click 63 112
    key 0x25
    key 0x28
    sleep .4

    # Ordinary Save offers a copy for this read-only format.
    click 125 24
    wait_for other_window 824 458 "$document"
    sleep .4
    cp selectscreen.png "$results/xls-save-offers-copy.png"
    click 752 458 # Cancel, then exercise the explicit Save As button.
    wait_for same_window 824 458 "$document"
    click 205 24
    wait_for other_window 824 458 "$document"
    click 824 458
    wait_for test -s "$books/legacy.osheet"
    wait_for checkpoint "$books/legacy.osheet" native-copy
    wait_for same_window 824 458 "$document"
    sleep .4
    test "$(sha256 -q "$books/legacy.xls")" = "$checksum"
    close_book

    open_book 183 legacy.osheet native-copy native
    sleep .45
    cp selectscreen.png "$results/xls-native-reopened.png"
    close_book
    echo "PASS XLSM VBA bytes and code names, inert notice, ordinary Save and reopen"
    echo "PASS XLS import and fidelity notice, Save As and native reopen, original bytes unchanged"
}

launcher=$(win_at 48 24)
test "$launcher" -gt 0
if test "$profile" = legacy; then legacy; exit; fi
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
    case "$profile" in
        layout) scroll_layout ;;
        annotations) check_reopened_validation ;;
        print) print_pdf ;;
        formulas) formulas_reopened ;;
    esac
    close_book
    echo "PASS $format $profile desktop save and converter reopen"
done
