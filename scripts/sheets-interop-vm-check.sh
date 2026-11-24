#!/bin/sh
# Run in the private 1280x720 Sheets desktop fixture. Stage the two books
# from the sheets_interop_fixture example in the only Scrinium place, and
# stage the arm64 FreeBSD build of that example at /tmp/sheets-interop-fixture.
# Use SHEETS_INTEROP_CASE=pivot_layout and /tmp/sheets-pivot-layout-fixture for pivot sorting and layout.
# Use SHEETS_INTEROP_CASE=pivot_sources and /tmp/sheets-pivot-sources-fixture for derived pivot fields.
# Use SHEETS_INTEROP_CASE=pivot_cache and /tmp/sheets-pivot-cache-fixture for Excel cache recovery.
# Use SHEETS_INTEROP_CASE=pivot_scope and /tmp/sheets-pivot-scope-fixture for Excel value sort scopes.
# Use SHEETS_INTEROP_CASE=pivot_order and /tmp/sheets-pivot-order-fixture for pivot item ordering.
# Use SHEETS_INTEROP_CASE=pivot_display and /tmp/sheets-pivot-display-fixture for pivot calculations.
# Set SHEETS_INTEROP_CASE=tables for the sheets_table_chart_fixture inputs
# and stage its binary at /tmp/sheets-table-chart-fixture instead.
# Use SHEETS_INTEROP_CASE=layout and /tmp/sheets-layout-fixture for layout.
# Use SHEETS_INTEROP_CASE=annotations and /tmp/sheets-annotations-fixture for annotations.
# Use SHEETS_INTEROP_CASE=legacy and /tmp/sheets-legacy-fixture for legacy.xls
# and macro.xlsm. Stage the same SimpleMacro.xlsm source at /tmp/sheets-vba-source.xlsm.
# Use SHEETS_INTEROP_CASE=print and /tmp/sheets-print-fixture for print layout.
# Use comments_print with the same binary and its make-comments-print inputs.
# Use print_fidelity with its make-fidelity inputs, pinned fonts in the Sheets
# jail, and the PDF checker's fidelity mode.
# The print profile also needs Prelum and the print sheet with no printers
# configured. Run sheets-interop-pdf-check.py on the exported PDFs on the host.
# Use SHEETS_INTEROP_CASE=formulas and /tmp/sheets-formula-fixture for formulas.
# Use SHEETS_INTEROP_CASE=responsiveness and /tmp/recalc-responsiveness for the
# large native workbook from recalc_responsiveness make. Run measure separately.
# Use SHEETS_INTEROP_CASE=delimited and /tmp/sheets-delimited-fixture for CSV and TSV.
# Use SHEETS_INTEROP_CASE=text_filter and /tmp/sheets-text-filter-fixture for text filters.
# Use SHEETS_INTEROP_CASE=rank_filter and /tmp/sheets-rank-filter-fixture for rank filters.
# Use SHEETS_INTEROP_CASE=sort_filter and /tmp/sheets-sort-filter-fixture for sorting.
# Use SHEETS_INTEROP_CASE=validation and /tmp/sheets-validation-fixture with Tabula.
# Use validation_legacy with the same binary and its make-legacy inputs.
# Approve the Sheets manifest that requests clipboard access for the fixture user.
# Use SHEETS_INTEROP_CASE=condfmt and /tmp/sheets-condfmt-fixture for conditional formatting.
# Use SHEETS_INTEROP_CASE=native_copy and /tmp/sheets-native-copy-fixture for Save As.
# Use SHEETS_INTEROP_CASE=table_filters and /tmp/sheets-table-filter-fixture for table-local filters.
# Use SHEETS_INTEROP_CASE=advanced_filters and /tmp/sheets-advanced-filter-fixture for nested ODS and advanced Excel filters.
# Use SHEETS_INTEROP_CASE=appearance and /tmp/sheets-appearance-fixture with /tmp/sheets-vba-source.xlsm.
# Set SHEETS_INTEROP_NOTES=1 in appearance and delimited to capture the Import Notes dialog for closeout.
# Use SHEETS_INTEROP_CASE=appearance_filters and /tmp/sheets-appearance-filter-fixture for Excel color and icon filters.
# Use SHEETS_INTEROP_CASE=date_filters and /tmp/sheets-date-filter-fixture for Excel date filters.
# Use SHEETS_INTEROP_CASE=dates and /tmp/sheets-date-fixture with the same VBA source as legacy.
# Use SHEETS_INTEROP_CASE=protection and /tmp/sheets-protection-fixture with that VBA source.
set -eu
cd /var/tmp/sheets-desktop
books=${1:-/var/tmp/sheets-format-input}
profile=${SHEETS_INTEROP_CASE:-pivots}
case "$profile" in pivots|pivot_display|pivot_order|pivot_scope|pivot_cache|pivot_sources|pivot_layout|tables|layout|annotations|legacy|print|comments_print|print_fidelity|formulas|responsiveness|delimited|sort_filter|rank_filter|text_filter|validation|validation_legacy|condfmt|native_copy|dates|date_filters|appearance|appearance_filters|advanced_filters|table_filters|protection) ;; *) exit 2 ;; esac
results=$(mktemp -d /var/tmp/sheets-interop-results.XXXXXX)
echo "Sheets interoperability evidence: $results"
case "$profile" in
    responsiveness) test "$(ls "$books")" = responsive.osheet ;;
    pivot_cache) test "$(ls "$books")" = "$(printf 'missing.xlsx\nunusable.xlsx')" ;;
    pivot_scope) test "$(ls "$books")" = "$(printf 'grand.xlsx\nmember.xlsx')" ;;
    comments_print) test "$(ls "$books")" = "$(printf 'legacy.xls\nsales.ods')" ;;
    validation_legacy) test "$(ls "$books")" = "$(printf 'extension.xlsx\nvalidation.xls')" ;;
    protection) test "$(ls "$books")" = "$(printf 'macro.xlsm\nopen.ods\nplain.xlsx')" ;;
    table_filters) test "$(ls "$books")" = "$(printf 'tables.ods\ntables.xlsm\ntables.xlsx')" ;;
    advanced_filters) test "$(ls "$books")" = "$(printf 'sales.ods\nsales.xlsx')" ;;
    appearance) test "$(ls "$books")" = "$(printf 'legacy.xls\nmacro.xlsm\nsales.ods\nsales.xlsx')" ;;
    appearance_filters) test "$(ls "$books")" = "$(printf '01-base.xlsx\n02-extended.xlsx')" ;;
    date_filters) test "$(ls "$books")" = "$(printf 'dates-1900.xlsx\ndates-1904.xlsx')" ;;
    dates) test "$(ls "$books")" = "$(printf 'dates.xlsx\nlegacy.xls\nmacro.xlsm')" ;;
    native_copy) test "$(ls "$books")" = "$(printf 'orders.ods\nsales.xlsx')" ;;
    legacy) test "$(ls "$books")" = "$(printf 'legacy.xls\nmacro.xlsm')" ;;
    delimited) test "$(ls "$books")" = "$(printf '01-utf8.csv\n02-utf8.tsv\n03-ansi.csv\n04-ansi.tsv')" ;;
    *) test "$(ls "$books")" = "$(printf 'sales.ods\nsales.xlsx')" ;;
esac

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
        pivot_display) /tmp/sheets-pivot-display-fixture check "$1" "$2" "$format" ;;
        pivot_order) /tmp/sheets-pivot-order-fixture check "$1" "$2" "$format" ;;
        pivot_scope) /tmp/sheets-pivot-scope-fixture check "$1" "$2" "$scope" ;;
        pivot_layout) /tmp/sheets-pivot-layout-fixture check "$1" "$2" "$format" ;;
        pivot_sources) /tmp/sheets-pivot-sources-fixture check "$1" "$2" "$format" ;;
        pivot_cache) /tmp/sheets-pivot-cache-fixture check "$1" "$2" "$format" ;;
        print) /tmp/sheets-print-fixture check "$1" "$2" ;;
        print_fidelity) /tmp/sheets-print-fixture check "$1" "$2-fidelity" ;;
        comments_print) /tmp/sheets-print-fixture check "$1" "$2-$variant" ;;
        formulas) /tmp/sheets-formula-fixture check "$1" "$2" "$format" ;;
        responsiveness) /tmp/recalc-responsiveness check "$1" "$2" ;;
        sort_filter) /tmp/sheets-sort-filter-fixture check "$1" "$2" ;;
        text_filter) /tmp/sheets-text-filter-fixture check "$1" "$2" "$format" ;;
        rank_filter) /tmp/sheets-rank-filter-fixture check "$1" "$2" "$format" ;;
        validation) /tmp/sheets-validation-fixture check "$1" "$2" ;;
        validation_legacy) /tmp/sheets-validation-fixture check "$1" "$2${list_stage:-}-$variant" ;;
        condfmt) /tmp/sheets-condfmt-fixture check "$1" "$2" ;;
        native_copy) /tmp/sheets-native-copy-fixture check "$1" "$format" "$2" ;;
        protection) /tmp/sheets-protection-fixture check "$1" "$2" "$format" /tmp/sheets-vba-source.xlsm ;;
        table_filters) /tmp/sheets-table-filter-fixture check "$1" "$2" "$format" ;;
        advanced_filters) /tmp/sheets-advanced-filter-fixture check "$1" "$2" "$format" ;;
        appearance) SHEETS_APPEARANCE_VBA=/tmp/sheets-vba-source.xlsm /tmp/sheets-appearance-fixture check "$1" "$2" "$format" ;;
        appearance_filters) /tmp/sheets-appearance-filter-fixture check "$1" "$2" "$kind" ;;
        date_filters) /tmp/sheets-date-filter-fixture check "$1" "$2" "$format" ;;
        dates) /tmp/sheets-date-fixture check "$1" "$2" "$format" /tmp/sheets-vba-source.xlsm ;;
        delimited) /tmp/sheets-delimited-fixture check "$1" "$2" "$format" "$encoding" ;;
        legacy) /tmp/sheets-legacy-fixture check "$1" "$2" /tmp/sheets-vba-source.xlsm ;;
    esac
} 2>"$results/checkpoint.log"
recovery_dir() {
    jail=$(docs)
    test -n "$jail" || return 1
    root=$(jls -j "$jail" path)/atrium/recovery
    name_file=$(find "$root" -name name -type f -exec grep -slx "$1" {} +)
    test -n "$name_file" || return 1
    dirname "$name_file"
}
loaded() {
    directory=$(recovery_dir "$1") || return 1
    test -s "$directory/current" || return 1
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
    if test "$profile" = delimited || test "${4:-foreign}" = native; then
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

pivot_display() {
    click "${1:-256}" 133 # Outside the source. Make an ordinary Save after import.
    key 0x24 # 7
    key 0x28
    save "$book" saved
    cp "$books/$book" "$results/pre-refresh.$format"
    final=saved
}

pivot_display_reopened() {
    click 126 133 # B2, the first source value.
    key 0x25 # 8
    key 0x28
    save "$book" edited # The source changed, the pivot still has cached values.
    cp selectscreen.png "$results/$format-before-refresh.png"

    data_from_bottom 6 # Refresh All Pivot Tables.
    save "$book" refreshed
    sleep .45
    cp selectscreen.png "$results/$format-refreshed.png"
    cp "$books/$book" "$results/refreshed.$format"
}

pivot_scope() {
    row=183
    for scope in grand member; do
        book=$scope.xlsx
        checkpoint "$books/$book" initial
        open_book "$row" "$book" initial
        click 319 133 # E2, outside the source.
        key 0x24 # 7
        key 0x28
        save "$book" saved
        cp "$books/$book" "$results/$scope-saved.xlsx"
        close_book

        open_book "$row" "$book" saved
        sleep .4
        cp selectscreen.png "$results/$scope-reopened.png"
        click 191 133 # C2, the first source sale.
        key 0x21 # 4
        key 0x27 # 0
        key 0x28
        save "$book" edited
        cp selectscreen.png "$results/$scope-before-refresh.png"

        data_from_bottom 6 # Refresh All Pivot Tables.
        save "$book" refreshed
        cp "$books/$book" "$results/$scope-refreshed.xlsx"
        close_book

        open_book "$row" "$book" refreshed
        sleep .4
        cp selectscreen.png "$results/$scope-refreshed-reopened.png"
        close_book
        echo "PASS $scope XLSX scoped pivot on both axes through Save, refresh, and reopen"
        row=$((row + 30))
    done
}

pivot_cache() {
    row=183
    for format in missing unusable; do
        book=$format.xlsx
        checkpoint "$books/$book" initial
        open_book "$row" "$book" initial
        sleep .4
        cp selectscreen.png "$results/$format-initial.png"
        pivot_display
        # The shared gate writes evidence using the fixture name as its suffix.
        mv "$results/pre-refresh.$format" "$results/$format-saved.xlsx"
        close_book

        open_book "$row" "$book" saved
        sleep .4
        cp selectscreen.png "$results/$format-reopened.png"
        pivot_display_reopened
        mv "$results/refreshed.$format" "$results/$format-refreshed.xlsx"
        close_book

        open_book "$row" "$book" refreshed
        sleep .4
        cp selectscreen.png "$results/$format-refreshed-reopened.png"
        close_book
        echo "PASS $format Excel pivot cache through Save, source edit, Refresh, and reopen"
        row=$((row + 30))
    done
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
    sheet_options=0
    if test "$format" = xlsx; then
        sheet_options=2 # Move Sheet Right and Delete Sheet are enabled with the Rates tab.
    fi
    click 63 112
    data_from_bottom "$((22 + sheet_options))" # Edit Comment…
    append_x
    save "$book" note

    click 126 112
    data_from_bottom "$((22 + sheet_options))" # Edit Link…
    click 302 681
    key 0x04 1 # Select all
    key 0x09 # f
    key 0x1e # 1
    key 0x28
    save "$book" link

    click 191 112
    data_from_bottom "$((6 + sheet_options))" # Data Validation…
    sleep .35
    click 1014 295 # Highest
    key 0x04 1
    key 0x25 # 8
    click 1054 663
    save "$book" rule

    click 254 112
    if test "$format" = xlsx; then
        data_from_bottom "$((23 + sheet_options))" # Open Comment Thread…
        append_x
    else
        data_from_bottom 22 # New Comment Thread…
        click 302 681
        key 0x14 # q
        key 0x28
        sleep .45
        click 254 112
        data_from_bottom "$((23 + sheet_options))" # Open Comment Thread…
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
    if test "$format" = xlsx; then
        click 510 112 # H1 has an Excel extension list sourced from Rates.
        key 0x26
        key 0x28
        sleep .5
        cp selectscreen.png "$results/xlsx-extended-list-refused.png"
        click 510 112
        key 0x25
        key 0x28
        save "$book" extended
        cp "$books/$book" "$results/saved.xlsx"
    fi
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

print_fidelity() { print; }

print_pdf() {
    suffix=${1:-}
    pdf="$books/${book%.*} – Report.pdf"
    test ! -e "$pdf"
    if test "$suffix" = -comments; then
        data_from_bottom 1 # Print with Comments… is the final Data command.
    else
        click 280 24
    fi
    wait_for other_window 829 578 "$document"
    print_window=$window
    sleep 1
    cp selectscreen.png "$results/$format$suffix-print.png"
    click 850 551 # Save as PDF is the only destination.
    wait_for pdf_picker_open
    click 824 458 # Confirm the suggested name in the system picker.
    wait_for pdf_finished "$pdf"
    wait_for same_window 824 458 "$document"
    mv "$pdf" "$results/$format$suffix.pdf"
}

appearance() {
    for format in xlsx xlsm ods xls; do
        case "$format" in
            xlsx) book=sales.xlsx; row=273 ;;
            xlsm) book=macro.xlsm; row=213 ;;
            ods) book=sales.ods; row=243 ;;
            xls) book=legacy.xls; row=183; checksum=$(sha256 -q "$books/$book") ;;
        esac
        checkpoint "$books/$book" initial
        open_book "$row" "$book" initial
        sleep 1.6
        cp selectscreen.png "$results/$format-appearance-initial.png"
        if test "$format" = xlsm && test "${SHEETS_INTEROP_NOTES:-0}" = 1; then
            click 921 706 # Import Notes beside the persistent macro label.
            sleep .4
            cp selectscreen.png "$results/xlsm-import-note-1.png"
            click 628 406 # Next in the centered two-line dialog.
            sleep .4
            cp selectscreen.png "$results/xlsm-import-note-2.png"
            key 0x29 # Escape closes the dialog.
        fi
        click 62 112
        key 0x25
        key 0x28
        if test "$format" = xls; then
            sleep .45
            click 205 24
            wait_for other_window 824 458 "$document"
            click 824 458
            book=legacy.osheet
            wait_for checkpoint "$books/$book" edited
            wait_for same_window 824 458 "$document"
            close_book
            test "$(sha256 -q "$books/legacy.xls")" = "$checksum"
            open_book "$row" "$book" edited native
        else
            save "$book" edited
            close_book
            open_book "$row" "$book" edited
        fi
        sleep 1.6
        cp selectscreen.png "$results/$format-appearance-reopened.png"
        cp "$books/$book" "$results/reopened.${book##*.}"
        close_book
        echo "PASS $format cell appearance after desktop Save and picker reopen"
    done
}

comments_print() {
    format=ods
    variant=threads
    book=sales.ods
    checkpoint "$books/$book" initial
    open_book 213 "$book" initial
    print
    close_book
    open_book 213 "$book" edited
    click 326 172 # Commented D4, with the restored conversation.
    sleep .45
    cp selectscreen.png "$results/ods-thread-reopened.png"
    print_pdf
    cp "$books/$book" "$results/thread-reopened.ods"
    close_book

    format=xls
    variant=legacy
    book=legacy.xls
    checksum=$(sha256 -q "$books/$book")
    checkpoint "$books/$book" initial
    open_book 183 "$book" initial
    click 400 172
    key 0x25 # E4 -> 8, printed formula in F4 -> 168.
    key 0x28
    sleep .45
    click 205 24
    wait_for other_window 824 458 "$document"
    click 824 458
    book=legacy.osheet
    wait_for checkpoint "$books/$book" edited
    wait_for same_window 824 458 "$document"
    close_book
    open_book 183 "$book" edited native
    click 326 172
    sleep .45
    cp selectscreen.png "$results/xls-note-reopened.png"
    print_pdf
    cp "$books/$book" "$results/legacy-reopened.osheet"
    close_book
    test "$(sha256 -q "$books/legacy.xls")" = "$checksum"
    echo "PASS ODS thread retained after ordinary Save and picker reopen"
    echo "PASS XLS rich note, two link forms and print settings retained through native Save As and picker reopen"
    echo "PASS both workbooks exported PDF through installed Prelum and Scrinium"
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

responsiveness() {
    book=responsive.osheet
    checkpoint "$books/$book" 1
    open_book 183 "$book" 1 native
    cp selectscreen.png "$results/responsiveness-initial.png"

    # Both edits go through the in-cell editor. Scroll between them and use
    # Ctrl+Home to put the same input cell back in view.
    initial_inode=$(stat -f '%i' "$books/$book")
    click 60 111
    key 0x1f                             # 2
    key 0x28                             # Enter
    input 'MOVE 350 300' 'SCROLL 360'
    sleep .12
    key 0x4a 1                           # Ctrl+Home
    key 0x24                             # 7
    key 0x28
    click 128 24                         # ordinary Save through Scrinium

    file_replaced() { test "$(stat -f '%i' "$books/$book")" != "$initial_inode"; }
    wait_for file_replaced
    checkpoint "$books/$book" 7
    sleep .4
    cp selectscreen.png "$results/responsiveness-saved.png"
    close_book
    open_book 183 "$book" 7 native
    sleep .4
    cp selectscreen.png "$results/responsiveness-reopened.png"
    close_book
    echo "PASS responsiveness desktop Save and picker reopen kept the latest edit and all formula results"
}

condfmt_screen() {
    /tmp/sheets-condfmt-fixture screen selectscreen.png "$1" 2>"$results/screen.log"
}
condfmt_capture() {
    wait_for condfmt_screen "$1"
    cp selectscreen.png "$results/$format-$1.png"
    /tmp/sheets-condfmt-fixture screen "$results/$format-$1.png" "$1"
}
condfmt() {
    condfmt_capture initial

    click 318 133 # E2 is the shared threshold. D3 now satisfies both rules.
    key 0x23 # 6
    key 0x28
    condfmt_capture threshold

    click 62 133 # A2 drives the formula in B2. D2 falls back to the second rule.
    key 0x1f # 2
    key 0x28
    condfmt_capture edited
    save "$book" edited
    cp "$books/$book" "$results/saved.${book##*.}"
    final=edited
}
condfmt_reopened() {
    /tmp/sheets-condfmt-fixture screen "$results/$format-reopened.png" edited

    click 318 133 # D3 must fall back again as soon as the restored rule sees 8.
    key 0x25 # 8
    key 0x28
    condfmt_capture reopened-edit
    save "$book" reopened-edit
}

validation() {
    click 126 133
    key 0x26 # 9 is outside the rule.
    key 0x28
    sleep .45
    cp selectscreen.png "$results/$format-typing-rejected.png"
    click 126 133
    key 0x25 # 8
    key 0x28
    save "$book" typed

    drag 318 133 318 153 # E2:E3 is 7, 9.
    key 0x06 1 # Copy
    click 126 133
    key 0x19 1 # Paste
    sleep .6
    cp selectscreen.png "$results/$format-paste-rejected.png"
    click 370 24 # Undo the accepted typing, skipping the rejected paste.
    save "$book" initial

    drag 382 133 382 153 # F2:F3 is 6, 8.
    key 0x06 1
    click 126 133
    key 0x19 1
    save "$book" pasted
    click 370 24 # Undo both pasted cells together.
    save "$book" initial
    click 445 24
    save "$book" pasted

    drag 191 133 254 153 # Ctrl+D copies C2:D2 (6, 9) into C3:D3.
    key 0x07 1
    sleep .6
    cp selectscreen.png "$results/$format-fill-rejected.png"
    click 254 133
    key 0x25 # Change the unvalidated source D2 to 8.
    key 0x28
    save "$book" ready

    drag 191 133 254 153
    key 0x07 1
    save "$book" filled
    click 370 24 # Undo the whole fill.
    save "$book" ready
    click 445 24
    save "$book" filled
    cp "$books/$book" "$results/saved.${book##*.}"
    final=filled
}

validation_list_type() {
    click 510 133 # H2 allows "2,0" as one item. The fallback's bare 2 must fail.
    key 0x1f # 2
    key 0x28
    sleep .45
    cp selectscreen.png "$results/$format-list-typing-rejected.png"
    click 510 153
    key 0x25 # 8
    key 0x28
    sleep .45
}

validation_list_bulk() {
    drag 318 133 318 153 # 7, 9 are rejected as a list paste.
    key 0x06 1
    click 510 133
    key 0x19 1
    sleep .6
    cp selectscreen.png "$results/$format-list-paste-rejected.png"
    save "$book" filled

    click 510 112 # Seed unvalidated H1 with 2.
    key 0x1f
    key 0x28
    drag 510 112 510 133
    key 0x07 1
    sleep .6
    cp selectscreen.png "$results/$format-list-fill-rejected.png"
    save "$book" filled

    drag 382 133 382 153 # F2:F3 is 6, 8. Both are allowed.
    key 0x06 1
    click 510 133
    key 0x19 1
    list_stage=+paste
    save "$book" filled

    drag 510 133 510 153
    key 0x07 1
    list_stage=+fill
    save "$book" filled
}

validation_legacy() {
    for format in xlsx xls; do
        list_stage=
        if test "$format" = xls; then
            book=validation.xls
            variant=legacy
            row=213
            checksum=$(sha256 -q "$books/$book")
        else
            book=extension.xlsx
            variant=extended
            row=183
        fi
        checkpoint "$books/$book" initial
        open_book "$row" "$book" initial
        validation_list_type
        list_stage=+list
        if test "$format" = xls; then
            click 205 24 # Save As the foreign workbook as a native copy.
            wait_for other_window 824 458 "$document"
            click 824 458
            book=validation.osheet
            wait_for test -s "$books/$book"
            wait_for checkpoint "$books/$book" initial
            wait_for same_window 824 458 "$document"
        else
            save "$book" initial
        fi
        validation
        validation_list_bulk
        close_book

        if test "$format" = xls; then
            test "$(sha256 -q "$books/validation.xls")" = "$checksum"
            open_book 213 "$book" filled native
        else
            open_book "$row" "$book" filled
        fi
        validation_reopened
        click 510 133
        key 0x1f # The exact list still rejects 2 after picker reopen.
        key 0x28
        sleep 1.6
        cp selectscreen.png "$results/$format-list-reopened-rejection.png"
        click 126 133
        key 0x24 # 7 is a valid whole number. Save this fresh edit.
        key 0x28
        save "$book" reopened
        cp "$books/$book" "$results/reopened.${book##*.}"
        close_book
        echo "PASS $format numeric and exact-list typing, clipboard and fill, ordinary Save and picker reopen"
    done
    echo "PASS XLS native Save As retained its validation and the original XLS bytes"
}

validation_reopened() {
    click 126 133
    key 0x26
    key 0x28
    sleep 1.6
    cp selectscreen.png "$results/$format-reopened-rejection.png"
}

sort_filter() {
    click 191 133 # Sort the secondary key, Rank, first. Sort is stable.
    data_from_bottom 17 # Sort Z to A
    save "$book" secondary

    click 62 133 # Region becomes the primary key.
    data_from_bottom 18 # Sort A to Z
    save "$book" sorted
    cp selectscreen.png "$results/$format-sorted.png"

    click 62 133 # A2 contains North in the sorted list.
    data_from_bottom 15 # Show Only North
    save "$book" region

    click 126 133 # B2 contains Open. Show All is now enabled in the menu.
    data_from_bottom 16 # Show Only Open
    save "$book" filtered
    sleep .8
    cp selectscreen.png "$results/$format-filtered.png"
    cp "$books/$book" "$results/saved.$format"
    final=filtered
}

rank_edit_and_save() {
    click 126 133 # First visible score in B.
    case "$format" in ods) key 0x26 ;; xlsx) key 0x1e ;; esac # 9 or 1
    key 0x28
    data_from_bottom 19 # Reapply Filter with a filter on the current column.
    save "$book" "$1"
    cp selectscreen.png "$results/$format-$1.png"
    cp "$books/$book" "$results/$1.$format"
}

rank_filter() {
    rank_edit_and_save edited
    final=edited
}

text_filter() {
    click 257 133 # D2, outside the filter.
    key 0x25 # 8
    key 0x28
    save "$book" saved
    final=saved
}

text_filter_reopened() {
    click 126 133 # B2, still the first visible row below the header.
    key 0x05 # b
    key 0x04 # a
    key 0x07 # d
    key 0x28
    save "$book" edited
    cp selectscreen.png "$results/$format-before-reapply.png"
    cp "$books/$book" "$results/edited.$format"

    click 126 133
    data_from_bottom 19 # Reapply Filter with a filter on the current column.
    save "$book" reapplied
    cp selectscreen.png "$results/$format-reapplied.png"
    cp "$books/$book" "$results/reapplied.$format"
    close_book
    open_book "$row" "$book" reapplied
    sleep .4
    cp selectscreen.png "$results/$format-reapplied-reopened.png"
}

delimited() {
    row=183
    for book in 01-utf8.csv 02-utf8.tsv 03-ansi.csv 04-ansi.tsv; do
        format=${book##*.}
        case "$book" in *-utf8.*) encoding=utf8 ;; *) encoding=ansi ;; esac
        checkpoint "$books/$book" initial
        open_book "$row" "$book" initial
        sleep 1.6
        cp selectscreen.png "$results/$book-initial.png"
        if test "$encoding" = ansi && test "${SHEETS_INTEROP_NOTES:-0}" = 1; then
            click 1034 706
            sleep .4
            cp selectscreen.png "$results/$book-import-note.png"
            key 0x29
        fi

        click 191 133 # C2. Enter =B2*3.
        key 0x2e
        key 0x05
        key 0x1f
        key 0x25 2
        key 0x20
        key 0x28
        save "$book" saved
        sleep .8 # Wait for the Save acknowledgment after checking the bytes.
        cp selectscreen.png "$results/$book-saved.png"
        cp "$books/$book" "$results/$book"
        close_book

        open_book "$row" "$book" saved
        sleep 1.6
        cp selectscreen.png "$results/$book-reopened.png"
        close_book
        echo "PASS $format $encoding quoted records, values-only Save and reopen"
        row=$((row + 30))
    done
}



protection() {
    for format in xlsm ods xlsx; do
        case "$format" in
            xlsm) book=macro.xlsm; row=183 ;;
            ods) book=open.ods; row=213 ;;
            xlsx) book=plain.xlsx; row=243 ;;
        esac
        checkpoint "$books/$book" initial
        open_book "$row" "$book" initial
        sleep .4
        cp selectscreen.png "$results/$format-fidelity.png"

        click 62 133 # A2 is locked by the default style.
        key 0x26 # 9
        key 0x28
        sleep .4
        cp selectscreen.png "$results/$format-typing-blocked.png"
        click 126 133 # B2 is unlocked.
        key 0x25 # 8
        key 0x28
        click 126 153 # B3 is blank but explicitly unlocked.
        key 0x23 # 6
        key 0x28

        drag 126 133 191 133 # Fill Right would replace the locked C2 formula.
        key 0x15 1
        sleep .4
        cp selectscreen.png "$results/$format-fill-blocked.png"
        save "$book" edited
        close_book

        open_book "$row" "$book" edited
        click 191 133 # C2 should show its result with an empty formula bar.
        sleep .4
        cp selectscreen.png "$results/$format-reopened.png"
        click 62 133
        key 0x4c # Delete the contents of locked A2.
        sleep .4
        cp selectscreen.png "$results/$format-delete-blocked.png"
        click 126 133
        key 0x24 # 7
        key 0x28
        save "$book" reopened-edit
        cp "$books/$book" "$results/saved.$format"
        close_book
        echo "PASS $format locked and unlocked edits, ordinary Save and picker reopen"
    done
}

type_epoch_date() {
    click "${2:-62}" "$1"
    for digit in 0x1e 0x26 0x27 0x21 0x2d 0x27 0x1e 0x2d 0x27 0x26; do key "$digit"; done # 1904-01-09
    key 0x28
}
table_filters() {
    row=183
    for format in ods xlsm xlsx; do
        book=tables.$format
        checkpoint "$books/$book" initial
        open_book "$row" "$book" initial
        click 319 133 # E2, outside the filters.
        key 0x25 # 8
        key 0x28
        save "$book" saved
        close_book

        open_book "$row" "$book" saved
        sleep .4
        cp selectscreen.png "$results/$format-reopened.png"
        click 126 133 # West B2. Change Open to no.
        key 0x11
        key 0x12
        key 0x28
        save "$book" edited
        click 126 133
        data_from_bottom 19 # Reapply West.
        save "$book" reapplied
        cp selectscreen.png "$results/$format-reapplied.png"

        click 126 112 # West's header. The column's rule can be cleared here.
        data_from_bottom 12 # Clear Filter in Column B.
        save "$book" cleared
        click 370 24 # Undo just the West clear.
        save "$book" reapplied
        cp "$books/$book" "$results/reapplied.$format"
        close_book

        open_book "$row" "$book" reapplied
        sleep .4
        cp selectscreen.png "$results/$format-final.png"
        close_book
        echo "PASS $format independent table filters, worksheet filter, reapply, clear, undo and reopen"
        row=$((row + 30))
    done
}

advanced_filters() {
    row=183
    for format in ods xlsx; do
        book=sales.$format
        checkpoint "$books/$book" initial
        open_book "$row" "$book" initial
        click 319 133 # E2, outside the filter.
        key 0x25 # 8
        key 0x28
        save "$book" saved
        close_book

        open_book "$row" "$book" saved
        if test "$format" = xlsx; then
            type_epoch_date 133 126 # B2, edit the date seen by the calendar group.
        else
            click 126 133
            key 0x1e
            key 0x28
        fi
        click 191 153
        key 0x1e
        key 0x28
        click 257 173
        key 0x1e
        key 0x28
        save "$book" edited
        cp "$books/$book" "$results/$format-edited.$format"
        cp selectscreen.png "$results/$format-before-reapply.png"

        click 126 133
        if test "$format" = ods; then
            data_from_bottom 18 # No separate per-column rule to clear.
        else
            data_from_bottom 19
        fi
        save "$book" reapplied
        cp "$books/$book" "$results/$format-reapplied.$format"
        close_book

        open_book "$row" "$book" reapplied
        sleep .4
        cp selectscreen.png "$results/$format-reapplied-reopened.png"
        close_book
        echo "PASS $format advanced filters through Save, edit, reapply, and reopen"
        row=$((row + 30))
    done
}

appearance_filters() {
    row=183
    for kind in base extended; do
        book=01-base.xlsx
        test "$kind" = base || book=02-extended.xlsx
        checkpoint "$books/$book" initial
        open_book "$row" "$book" initial
        click 319 133 # E2, outside the filter.
        key 0x25 # 8
        key 0x28
        save "$book" saved
        close_book

        open_book "$row" "$book" saved
        sleep .4
        cp selectscreen.png "$results/$kind-reopened.png"
        click 126 133 # B2 decides the fill color.
        key 0x1e # 1
        key 0x28
        click 191 153 # C3 decides the font color.
        key 0x1e
        key 0x28
        click 257 173 # D4 decides the displayed icon.
        key 0x1e
        key 0x28
        save "$book" edited
        cp selectscreen.png "$results/$kind-before-reapply.png"
        cp "$books/$book" "$results/$kind-edited.xlsx"

        click 126 133
        data_from_bottom 19 # Reapply all three predicates.
        save "$book" reapplied
        cp "$books/$book" "$results/$kind-reapplied.xlsx"
        close_book

        open_book "$row" "$book" reapplied
        sleep .4
        cp selectscreen.png "$results/$kind-reapplied-reopened.png"
        close_book
        echo "PASS XLSX $kind icon set, cell color and font color through Save, edit, reapply and reopen"
        row=$((row + 30))
    done
}

date_filters() {
    row=183
    for format in 1900 1904; do
        book=dates-$format.xlsx
        checkpoint "$books/$book" initial
        open_book "$row" "$book" initial
        click 257 133 # D2, outside the filter.
        key 0x25 # 8
        key 0x28
        save "$book" saved
        close_book

        open_book "$row" "$book" saved
        sleep .4
        cp selectscreen.png "$results/$format-reopened.png"
        type_epoch_date 133 126 # B2, calendar selection.
        type_epoch_date 153 191 # C3, dynamic Today.
        save "$book" edited
        cp "$books/$book" "$results/$format-edited.xlsx"
        cp selectscreen.png "$results/$format-before-reapply.png"

        click 126 133
        data_from_bottom 19 # Reapply both rules on the sheet.
        save "$book" reapplied
        cp "$books/$book" "$results/$format-reapplied.xlsx"
        close_book

        open_book "$row" "$book" reapplied
        sleep .4
        cp selectscreen.png "$results/$format-reapplied-reopened.png"
        close_book
        echo "PASS XLSX $format calendar and dynamic date filters through Save, edit, reapply, and reopen"
        row=$((row + 30))
    done
}

dates() {
    for format in xlsx xlsm; do
        case "$format" in xlsx) book=dates.xlsx; row=183 ;; xlsm) book=macro.xlsm; row=243 ;; esac
        checkpoint "$books/$book" initial
        open_book "$row" "$book" initial
        sleep .4
        cp selectscreen.png "$results/$format-initial.png"
        type_epoch_date 133
        save "$book" edited
        cp "$books/$book" "$results/saved.$format"
        close_book
        open_book "$row" "$book" edited
        sleep .4
        cp selectscreen.png "$results/$format-reopened.png"
        close_book
        echo "PASS $format date arithmetic, ordinary Save and picker reopen"
    done

    format=xls
    checksum=$(sha256 -q "$books/legacy.xls")
    checkpoint "$books/legacy.xls" initial
    open_book 213 legacy.xls initial
    sleep .4
    cp selectscreen.png "$results/xls-initial.png"
    type_epoch_date 112
    click 205 24
    wait_for other_window 824 458 "$document"
    click 824 458
    wait_for checkpoint "$books/legacy.osheet" edited
    wait_for same_window 824 458 "$document"
    wait_for recovery_dir legacy.osheet >/dev/null
    close_book

    open_book 213 legacy.osheet edited native
    sleep .4
    cp selectscreen.png "$results/xls-native-reopened.png"
    cp "$books/legacy.osheet" "$results/reopened.osheet"
    cp "$books/legacy.osheet" "$results/saved.osheet"
    close_book
    test "$(sha256 -q "$books/legacy.xls")" = "$checksum"
    echo "PASS XLS 1904 date arithmetic and native Save As, original bytes unchanged"
}

copy_edit_units() {
    click 126 133 # B2 feeds the formula and the conditional format.
    key "$1"
    key 0x28
}
copy_originals_intact() {
    cmp "$books/orders.ods" "$results/original.ods"
    cmp "$books/sales.xlsx" "$results/original.xlsx"
}
native_copy() {
    cp "$books/orders.ods" "$results/original.ods"
    cp "$books/sales.xlsx" "$results/original.xlsx"
    for format in ods xlsx; do
        case "$format" in
            ods) stem=orders; row=183; first_key=0x23; edit_key=0x25 ;; # 6, 8
            xlsx) stem=sales; row=243; first_key=0x24; edit_key=0x22 ;; # 7, 5
        esac
        source=$stem.$format
        copy=$stem.osheet
        checkpoint "$books/$source" initial
        open_book "$row" "$source" initial
        copy_edit_units "$first_key"

        click 205 24
        wait_for other_window 824 458 "$document"
        sleep .4
        cp selectscreen.png "$results/$format-save-as.png"
        click 824 458
        wait_for checkpoint "$books/$copy" copy
        wait_for same_window 824 458 "$document"
        # The published name confirms Sheets has received the new save grant.
        wait_for recovery_dir "$copy" >/dev/null
        cp "$books/$copy" "$results/$format-published.osheet"
        copy_originals_intact

        copy_edit_units "$edit_key"
        save "$copy" edited
        cp "$books/$copy" "$results/$format-saved.osheet"
        copy_originals_intact
        close_book
    done

    # With both copies present, reopen each by its own name and its own edited value.
    for format in ods xlsx; do
        case "$format" in ods) copy=orders.osheet; row=213 ;; xlsx) copy=sales.osheet; row=243 ;; esac
        open_book "$row" "$copy" edited native
        sleep .4
        cp selectscreen.png "$results/$format-native-reopened.png"
        cmp "$books/$copy" "$results/$format-saved.osheet"
        close_book
        echo "PASS $format native Save As, ordinary Save and picker reopen"
    done
    copy_originals_intact
    echo "PASS both foreign originals are byte-identical"
}

legacy() {
    checksum=$(sha256 -q "$books/legacy.xls")
    checkpoint "$books/macro.xlsm" macro-initial
    open_book 213 macro.xlsm macro-initial
    sleep 1.6
    cp selectscreen.png "$results/xlsm-initial.png"
    click 63 132
    key 0x25 # 8
    key 0x28
    save macro.xlsm macro-edited
    cp selectscreen.png "$results/xlsm-saved.png"
    close_book
    open_book 213 macro.xlsm macro-edited
    sleep 1.6
    cp selectscreen.png "$results/xlsm-reopened.png"
    cp "$books/macro.xlsm" "$results/reopened.xlsm"
    close_book

    checkpoint "$books/legacy.xls" xls-initial
    open_book 183 legacy.xls xls-initial
    sleep 1.6
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
    sleep 1.6
    cp selectscreen.png "$results/xls-native-reopened.png"
    cp "$books/legacy.osheet" "$results/reopened.osheet"
    close_book
    echo "PASS XLSM VBA bytes and code names, inert notice, ordinary Save and reopen"
    echo "PASS XLS import and fidelity notice, Save As and native reopen, original bytes unchanged"
}

launcher=$(win_at 48 24)
test "$launcher" -gt 0
case "$profile" in
    pivot_cache) pivot_cache; exit ;;
    date_filters) date_filters; exit ;;
    advanced_filters) advanced_filters; exit ;;
    appearance_filters) appearance_filters; exit ;;
    table_filters) table_filters; exit ;;
    pivot_scope) pivot_scope; exit ;;
    legacy) legacy; exit ;;
    comments_print) comments_print; exit ;;
    appearance) appearance; exit ;;
    validation_legacy) validation_legacy; exit ;;
    delimited) delimited; exit ;;
    native_copy) native_copy; exit ;;
    dates) dates; exit ;;
    protection) protection; exit ;;
    responsiveness) responsiveness; exit ;;
esac
for format in ods xlsx; do
    case "$format" in ods) row=183 ;; xlsx) row=213 ;; esac
    book=sales.$format
    checkpoint "$books/$book" initial
    open_book "$row" "$book" initial
    sleep .4
    cp selectscreen.png "$results/$format-initial.png"

    case "$profile" in
        pivot_order|pivot_sources) pivot_display ;;
        pivot_layout) pivot_display 319 ;;
        *) "$profile" ;;
    esac
    close_book

    open_book "$row" "$book" "$final"
    sleep .4
    cp selectscreen.png "$results/$format-reopened.png"
    case "$profile" in
        pivot_display|pivot_order|pivot_sources|pivot_layout) pivot_display_reopened ;;
        layout) scroll_layout ;;
        annotations) check_reopened_validation ;;
        print) print_pdf ;;
        print_fidelity) print_pdf; print_pdf -comments ;;
        formulas) formulas_reopened ;;
        validation) validation_reopened ;;
        condfmt) condfmt_reopened ;;
        rank_filter) rank_edit_and_save edited_again ;;
        text_filter) text_filter_reopened ;;
    esac
    close_book
    if test "$profile" = pivot_sources || test "$profile" = pivot_layout; then
        open_book "$row" "$book" refreshed
        sleep .4
        cp selectscreen.png "$results/$format-refreshed-reopened.png"
        close_book
    fi
    echo "PASS $format $profile desktop save and converter reopen"
done
